//! Cold same-script request pools.
//!
//! A pool runs independent requests for one [`WorkerBuilder`] template across
//! separate worker processes. It owns no wire protocol of its own: each
//! request becomes an ordinary correlated request on a freshly spawned worker,
//! and the worker is retired as soon as that request reaches a terminal
//! result.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Duration, Instant, sleep_until, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::lifecycle::WorkerExit;
use crate::manager::{Worker, WorkerBuilder};
use crate::protocol::{ControlPlane, Data};
use crate::request::{PendingRequest, Request};

/// Default bound on requests waiting in one pool's queue.
pub const DEFAULT_POOL_QUEUE_CAPACITY: usize = 64;

/// Configuration for a [`WorkerPool`].
///
/// ```
/// use runpy::PoolConfig;
///
/// let config = PoolConfig::default().max_workers(2).queue_capacity(16);
/// # let _ = config;
/// ```
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    max_workers: Option<usize>,
    queue_capacity: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_workers: None,
            queue_capacity: DEFAULT_POOL_QUEUE_CAPACITY,
        }
    }
}

impl PoolConfig {
    /// Most worker processes this pool runs at once; defaults to the
    /// Manager's `max_workers`. Must be positive and at most that limit.
    pub fn max_workers(mut self, workers: usize) -> Self {
        self.max_workers = Some(workers);
        self
    }

    /// Requests that may wait in this pool for a worker; default
    /// [`DEFAULT_POOL_QUEUE_CAPACITY`] (64). Must be positive.
    pub fn queue_capacity(mut self, requests: usize) -> Self {
        self.queue_capacity = requests;
        self
    }
}

/// A request admitted to the pool and waiting for a worker.
struct Job {
    pending: PendingRequest,
    /// Released when the job leaves the queue for a worker slot.
    _queued: OwnedSemaphorePermit,
}

/// State shared by every [`WorkerPool`] handle and the pool's tasks.
pub(crate) struct PoolCore {
    id: String,
    template: WorkerBuilder,
    control_plane: Weak<ControlPlane>,
    jobs: mpsc::UnboundedSender<Job>,
    queue_slots: Arc<Semaphore>,
    closed: AtomicBool,
    closing: watch::Sender<bool>,
    /// Workers currently running a pool request, keyed by worker ID.
    active: Mutex<HashMap<String, Worker>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    /// One task per started request; owns its worker until it is retired.
    runs: Arc<Mutex<JoinSet<Option<WorkerExit>>>>,
}

impl PoolCore {
    /// Stop every pool task immediately. Used when the Manager drops; the
    /// Manager itself kills the worker processes.
    pub(crate) fn shutdown_now(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(task) = self.dispatcher.lock().take() {
            task.abort();
        }
        self.runs.lock().abort_all();
    }
}

/// Handle to a pool of cold workers for one script template.
///
/// Created by [`crate::Manager::pool`]. Requests are started in FIFO order,
/// each on its own new worker process, up to the pool's `max_workers` and the
/// Manager-wide process limit. A worker is retired after its request's
/// terminal result, so no state carries between requests and requests need
/// no affinity. Cloning is cheap; the Manager owns the pool, so dropping
/// every handle neither stops it nor leaks it: the pool keeps serving
/// submitted requests until [`WorkerPool::close`] or Manager drop.
///
/// ```no_run
/// use runpy::{Data, Manager, PoolConfig};
/// use serde_json::json;
/// use std::time::Duration;
///
/// # async fn demo() -> Result<(), runpy::RunpyError> {
/// let manager = Manager::builder("workers").build().await?;
/// let pool = manager.pool(manager.worker("parse"), PoolConfig::default()).await?;
///
/// // Two independent inputs run concurrently on two processes.
/// let first = pool.submit(Data::from_iter([("text".into(), json!("a"))]), Duration::from_secs(30)).await?;
/// let second = pool.submit(Data::from_iter([("text".into(), json!("b"))]), Duration::from_secs(30)).await?;
/// let (first, second) = tokio::join!(first.result(), second.result());
/// println!("{:?} {:?}", first?, second?);
///
/// pool.close().await;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct WorkerPool {
    core: Arc<PoolCore>,
}

impl std::fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerPool")
            .field("id", &self.core.id)
            .finish_non_exhaustive()
    }
}

impl WorkerPool {
    /// Validate `config` against the Manager and start the pool's
    /// dispatcher. No worker process is created.
    pub(crate) fn start(
        id: String,
        template: WorkerBuilder,
        control_plane: &Arc<ControlPlane>,
        config: PoolConfig,
    ) -> Result<(Self, Weak<PoolCore>), RunpyError> {
        let global = control_plane.max_workers();
        let max_workers = config.max_workers.unwrap_or(global);
        if max_workers == 0 || max_workers > global {
            return Err(RunpyError::InvalidConfig(format!(
                "pool max_workers ({max_workers}) must be between 1 and the Manager's max_workers ({global})"
            )));
        }
        if config.queue_capacity == 0 {
            return Err(RunpyError::InvalidConfig(
                "pool queue_capacity must be greater than zero".to_string(),
            ));
        }

        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        let (closing, closing_rx) = watch::channel(false);
        let core = Arc::new(PoolCore {
            id,
            template,
            control_plane: Arc::downgrade(control_plane),
            jobs: jobs_tx,
            queue_slots: Arc::new(Semaphore::new(config.queue_capacity)),
            closed: AtomicBool::new(false),
            closing,
            active: Mutex::new(HashMap::new()),
            dispatcher: Mutex::new(None),
            runs: Arc::new(Mutex::new(JoinSet::new())),
        });
        let task = tokio::spawn(dispatch(
            core.clone(),
            jobs_rx,
            closing_rx,
            Arc::new(Semaphore::new(max_workers)),
        ));
        *core.dispatcher.lock() = Some(task);
        let weak = Arc::downgrade(&core);
        Ok((Self { core }, weak))
    }

    /// Manager-assigned identity of this pool.
    pub fn id(&self) -> &str {
        &self.core.id
    }

    /// Run one request on a fresh worker and wait for its result.
    ///
    /// Equivalent to [`WorkerPool::submit`] followed by [`Request::result`].
    pub async fn execute(&self, data: Data, timeout: Duration) -> Result<Data, RunpyError> {
        self.submit(data, timeout).await?.result().await
    }

    /// Queue one request, waiting within `timeout` for pool queue capacity
    /// and a Manager-wide pending-request slot.
    ///
    /// `timeout` is the end-to-end deadline: queueing, worker startup, and
    /// execution all count against it. A request whose deadline passes
    /// before a worker received it fails with `Timeout(Queued)` and never
    /// runs. One that times out while running keeps its worker (and pool
    /// slot) occupied until the worker answers; it is never replayed.
    ///
    /// # Errors
    ///
    /// [`RunpyError::Timeout`] with [`TimeoutStage::Admission`] when no
    /// capacity freed up in time, [`RunpyError::PoolClosed`], or
    /// [`RunpyError::ManagerClosed`].
    pub async fn submit(&self, data: Data, timeout: Duration) -> Result<Request, RunpyError> {
        let (control_plane, deadline) = self.admissible(timeout)?;
        let queued = timeout_at(deadline, self.core.queue_slots.clone().acquire_owned())
            .await
            .map_err(|_| RunpyError::Timeout(TimeoutStage::Admission))?
            .map_err(|_| RunpyError::PoolClosed)?;
        let budget = control_plane.admit_request(deadline).await?;
        self.enqueue(&control_plane, data, deadline, queued, budget)
    }

    /// Queue one request only if capacity is available right now.
    ///
    /// # Errors
    ///
    /// [`RunpyError::QueueFull`] when the pool queue or the Manager-wide
    /// pending-request budget is full; otherwise as [`WorkerPool::submit`].
    pub fn try_submit(&self, data: Data, timeout: Duration) -> Result<Request, RunpyError> {
        let (control_plane, deadline) = self.admissible(timeout)?;
        let queued =
            self.core
                .queue_slots
                .clone()
                .try_acquire_owned()
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => RunpyError::QueueFull,
                    TryAcquireError::Closed => RunpyError::PoolClosed,
                })?;
        let budget = control_plane.try_admit_request()?;
        self.enqueue(&control_plane, data, deadline, queued, budget)
    }

    /// Stop accepting requests, fail queued ones with
    /// [`RunpyError::PoolClosed`], gracefully terminate the workers still
    /// running requests, and return the exit of every worker this pool
    /// still owned. Requests interrupted this way fail with
    /// [`RunpyError::OutcomeUnknown`]. Closing twice returns no exits.
    pub async fn close(&self) -> Vec<WorkerExit> {
        let core = &self.core;
        core.closed.store(true, Ordering::Release);
        core.queue_slots.close();
        core.closing.send_replace(true);
        let dispatcher = core.dispatcher.lock().take();
        if let Some(dispatcher) = dispatcher {
            let _ = dispatcher.await;
        }
        let active: Vec<Worker> = core.active.lock().values().cloned().collect();
        let mut stopping = JoinSet::new();
        for worker in active {
            stopping.spawn(async move { worker.terminate().await });
        }
        stopping.join_all().await;
        let mut runs = std::mem::take(&mut *core.runs.lock());
        let mut exits = Vec::new();
        while let Some(exit) = runs.join_next().await {
            if let Ok(Some(exit)) = exit {
                exits.push(exit);
            }
        }
        exits
    }

    fn admissible(&self, timeout: Duration) -> Result<(Arc<ControlPlane>, Instant), RunpyError> {
        if timeout.is_zero() {
            return Err(RunpyError::InvalidConfig(
                "request timeout must be greater than zero".to_string(),
            ));
        }
        if self.core.closed.load(Ordering::Acquire) {
            return Err(RunpyError::PoolClosed);
        }
        let control_plane = self
            .core
            .control_plane
            .upgrade()
            .ok_or(RunpyError::ManagerClosed)?;
        Ok((control_plane, Instant::now() + timeout))
    }

    fn enqueue(
        &self,
        control_plane: &ControlPlane,
        data: Data,
        deadline: Instant,
        queued: OwnedSemaphorePermit,
        budget: OwnedSemaphorePermit,
    ) -> Result<Request, RunpyError> {
        let id = control_plane.next_request_id()?;
        let (request, pending) = Request::new(id, self.core.id.clone(), data, deadline, budget);
        self.core
            .jobs
            .send(Job {
                pending,
                _queued: queued,
            })
            .map_err(|_| RunpyError::PoolClosed)?;
        Ok(request)
    }
}

/// Start queued jobs in FIFO order as pool slots free up.
///
/// A job keeps its queue slot until it gets a pool slot, so the queue bound
/// covers every request not yet handed to a worker. A job whose deadline
/// passes while waiting is failed with `Timeout(Queued)` without starting a
/// process.
async fn dispatch(
    core: Arc<PoolCore>,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    mut closing: watch::Receiver<bool>,
    slots: Arc<Semaphore>,
) {
    loop {
        let job = tokio::select! {
            job = jobs.recv() => job,
            _ = closing.wait_for(|closing| *closing) => None,
        };
        let Some(job) = job else { break };
        let slot = tokio::select! {
            slot = slots.clone().acquire_owned() => slot.expect("pool slots are never closed"),
            () = sleep_until(job.pending.deadline()) => {
                job.pending.fail_unsent(RunpyError::Timeout(TimeoutStage::Queued));
                continue;
            }
            _ = closing.wait_for(|closing| *closing) => {
                job.pending.fail_unsent(RunpyError::PoolClosed);
                break;
            }
        };
        let Job { pending, .. } = job;
        if pending.is_abandoned() {
            continue;
        }
        let mut runs = core.runs.lock();
        while runs.try_join_next().is_some() {}
        runs.spawn(run(core.clone(), pending, slot));
    }
    // Late submissions now fail at send; everything still queued is failed.
    jobs.close();
    while let Ok(job) = jobs.try_recv() {
        job.pending.fail_unsent(RunpyError::PoolClosed);
    }
}

/// Start a worker for one request, wait for its terminal result even past
/// the caller's deadline, then retire the worker. The pool slot is released
/// only after the worker's process group was stopped and reaped.
async fn run(
    core: Arc<PoolCore>,
    pending: PendingRequest,
    _slot: OwnedSemaphorePermit,
) -> Option<WorkerExit> {
    let deadline = pending.deadline();
    let worker = match timeout_at(deadline, core.template.clone().spawn()).await {
        Ok(Ok(worker)) => worker,
        // `spawn` already stopped anything it started; the request never ran.
        Ok(Err(error)) => {
            pending.fail_unsent(error);
            return None;
        }
        Err(_) => {
            pending.fail_unsent(RunpyError::Timeout(TimeoutStage::Queued));
            return None;
        }
    };
    if pending.is_abandoned() || core.closed.load(Ordering::Acquire) {
        pending.fail_unsent(RunpyError::PoolClosed);
        return retire(worker).await;
    }

    let worker_id = worker.id().to_string();
    core.active.lock().insert(worker_id.clone(), worker.clone());
    let (inner, outcome, relay) = pending.relay();
    match worker.control_plane() {
        Ok(control_plane) => control_plane.enqueue(&worker_id, inner),
        Err(_) => drop(inner),
    }
    relay.finish(outcome.await, &worker_id);
    core.active.lock().remove(&worker_id);
    retire(worker).await
}

/// Terminate a worker that finished its request and return its exit, or
/// `None` if the Manager is gone and nothing retained it.
async fn retire(worker: Worker) -> Option<WorkerExit> {
    match worker.terminate().await {
        Ok(exit) => Some(exit),
        Err(_) => worker.wait().await.ok(),
    }
}
