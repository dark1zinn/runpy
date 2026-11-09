//! Same-script request pools.
//!
//! A pool runs independent requests for one [`WorkerBuilder`] template across
//! separate worker processes. It owns no wire protocol of its own: each
//! request becomes an ordinary correlated request on a worker. Workers are
//! retired after one request unless the pool explicitly enables reuse.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::{Duration, Instant, sleep_until, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::lifecycle::WorkerExit;
use crate::manager::{Worker, WorkerBuilder};
use crate::protocol::{ControlPlane, Data};
use crate::request::{PendingRequest, Request};

/// Default bound on requests waiting in one pool's queue.
pub const DEFAULT_POOL_QUEUE_CAPACITY: usize = 64;
/// Default time a reusable pool worker may stay idle before it is retired.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    reuse_workers: bool,
    idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_workers: None,
            queue_capacity: DEFAULT_POOL_QUEUE_CAPACITY,
            reuse_workers: false,
            idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
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

    /// Keep a worker after its request instead of retiring it; default off.
    ///
    /// A reused worker keeps all process state from earlier requests:
    /// imports, caches, globals, open files, and leaked memory. Nothing is
    /// reset. Enable it only when every request is independent of that state
    /// and correct on any worker; requests have no affinity. A worker is
    /// reused only after `done` or a reported execution error, never after a
    /// transport or protocol failure, and never while it still runs a request
    /// whose caller timed out.
    pub fn reuse_workers(mut self, reuse: bool) -> Self {
        self.reuse_workers = reuse;
        self
    }

    /// How long a reusable worker may stay idle before it is retired;
    /// default [`DEFAULT_POOL_IDLE_TIMEOUT`] (30 s). Must be positive.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
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
    /// `Some(idle_timeout)` when finished workers are kept for reuse.
    reuse: Option<Duration>,
    /// Parked workers available for the next request.
    idle: Mutex<Vec<IdleWorker>>,
    /// Wakes the dispatcher when a worker is parked.
    parked: Notify,
    next_token: AtomicU64,
}

/// How a queued job obtained a worker.
enum Assigned {
    /// A pool slot for a worker that still has to be started.
    Fresh(OwnedSemaphorePermit),
    /// A parked worker, together with the pool slot it kept while idle.
    Idle(Worker, OwnedSemaphorePermit),
}

/// A worker kept between requests when reuse is enabled.
struct IdleWorker {
    /// Distinguishes this parking from later ones of the same worker.
    token: u64,
    worker: Worker,
    slot: OwnedSemaphorePermit,
    timer: AbortHandle,
}

impl PoolCore {
    /// Stop every pool task immediately. Used when the Manager drops; the
    /// Manager itself kills the worker processes.
    pub(crate) fn shutdown_now(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(task) = self.dispatcher.lock().take() {
            task.abort();
        }
        self.idle.lock().clear();
        self.runs.lock().abort_all();
    }

    /// Keep a worker idle with its pool slot and arm its idle timeout.
    ///
    /// Hands the worker back when the pool is closed: `close` drains the
    /// idle list under the same lock after setting `closed`, so a worker
    /// can never be parked after it.
    fn park(
        self: &Arc<Self>,
        worker: Worker,
        slot: OwnedSemaphorePermit,
    ) -> Result<(), (Worker, OwnedSemaphorePermit)> {
        let Some(idle_timeout) = self.reuse else {
            return Err((worker, slot));
        };
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        // The timer looks its entry up under this lock, so it cannot run
        // before the entry exists.
        let mut idle = self.idle.lock();
        if self.closed.load(Ordering::Acquire) {
            return Err((worker, slot));
        }
        let timer = self
            .runs
            .lock()
            .spawn(expire_idle(self.clone(), token, idle_timeout));
        idle.push(IdleWorker {
            token,
            worker,
            slot,
            timer,
        });
        drop(idle);
        self.parked.notify_one();
        Ok(())
    }

    /// Take a parked worker that is still ready, cancelling its idle timer.
    /// Parked workers that exited meanwhile are discarded; the Manager has
    /// already reaped or is reaping them.
    fn take_idle(&self) -> Option<IdleWorker> {
        let mut idle = self.idle.lock();
        while let Some(entry) = idle.pop() {
            entry.timer.abort();
            if entry.worker.is_ready() {
                return Some(entry);
            }
        }
        None
    }
}

/// Handle to a pool of workers for one script template.
///
/// Created by [`crate::Manager::pool`]. Requests are started in FIFO order up
/// to the pool's `max_workers` and the Manager-wide process limit. By
/// default each request gets its own new worker process, retired after the
/// request's terminal result, so no state carries between requests; with
/// [`PoolConfig::reuse_workers`] a finished worker serves later requests.
/// Requests have no affinity either way. Cloning is cheap; the Manager owns
/// the pool, so dropping every handle neither stops it nor leaks it: the
/// pool keeps serving submitted requests until [`WorkerPool::close`] or
/// Manager drop.
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
        if config.idle_timeout.is_zero() {
            return Err(RunpyError::InvalidConfig(
                "pool idle_timeout must be greater than zero".to_string(),
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
            reuse: config.reuse_workers.then_some(config.idle_timeout),
            idle: Mutex::new(Vec::new()),
            parked: Notify::new(),
            next_token: AtomicU64::new(0),
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
    /// [`RunpyError::PoolClosed`], gracefully terminate running and idle
    /// workers, and return the exit of every worker this pool still owned.
    /// Requests interrupted this way fail with
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
        // `closed` is set, so no worker can be parked after this drain.
        let parked: Vec<IdleWorker> = core.idle.lock().drain(..).collect();
        let active: Vec<Worker> = core.active.lock().values().cloned().collect();
        let mut stopping = JoinSet::new();
        for worker in active {
            stopping.spawn(async move {
                let _ = worker.terminate().await;
                None
            });
        }
        for entry in parked {
            entry.timer.abort();
            stopping.spawn(async move {
                let exit = retire(entry.worker).await;
                drop(entry.slot);
                exit
            });
        }
        let mut exits: Vec<WorkerExit> = stopping.join_all().await.into_iter().flatten().collect();
        let mut runs = std::mem::take(&mut *core.runs.lock());
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

/// Start queued jobs in FIFO order as workers become available, preferring
/// a parked idle worker over starting a new process.
///
/// A job keeps its queue slot until it gets a worker, so the queue bound
/// covers every request not yet handed to a worker. A job whose deadline
/// passes while waiting is failed with `Timeout(Queued)` without starting a
/// process.
async fn dispatch(
    core: Arc<PoolCore>,
    mut jobs: mpsc::UnboundedReceiver<Job>,
    mut closing: watch::Receiver<bool>,
    slots: Arc<Semaphore>,
) {
    'jobs: loop {
        let job = tokio::select! {
            job = jobs.recv() => job,
            _ = closing.wait_for(|closing| *closing) => None,
        };
        let Some(job) = job else { break };
        let assigned = loop {
            if let Some(entry) = core.take_idle() {
                break Assigned::Idle(entry.worker, entry.slot);
            }
            tokio::select! {
                slot = slots.clone().acquire_owned() => {
                    break Assigned::Fresh(slot.expect("pool slots are never closed"));
                }
                // A finished worker was parked; prefer it over a new process.
                () = core.parked.notified() => {}
                () = sleep_until(job.pending.deadline()) => {
                    job.pending.fail_unsent(RunpyError::Timeout(TimeoutStage::Queued));
                    continue 'jobs;
                }
                _ = closing.wait_for(|closing| *closing) => {
                    job.pending.fail_unsent(RunpyError::PoolClosed);
                    break 'jobs;
                }
            }
        };
        let Job { pending, .. } = job;
        if pending.is_abandoned() {
            continue;
        }
        let mut runs = core.runs.lock();
        while runs.try_join_next().is_some() {}
        runs.spawn(run(core.clone(), pending, assigned));
    }
    // Late submissions now fail at send; everything still queued is failed.
    jobs.close();
    while let Ok(job) = jobs.try_recv() {
        job.pending.fail_unsent(RunpyError::PoolClosed);
    }
}

/// Run one request on its assigned worker, starting a fresh one if needed,
/// and wait for its terminal result even past the caller's deadline.
///
/// Afterwards the worker is parked for reuse when the pool allows it and the
/// request ended normally (`done` or a reported execution error); otherwise
/// it is retired, and the pool slot is released only after its process
/// group was stopped and reaped.
async fn run(
    core: Arc<PoolCore>,
    pending: PendingRequest,
    assigned: Assigned,
) -> Option<WorkerExit> {
    let (worker, slot) = match assigned {
        Assigned::Idle(worker, slot) => (worker, slot),
        Assigned::Fresh(slot) => {
            let deadline = pending.deadline();
            match timeout_at(deadline, core.template.clone().spawn()).await {
                Ok(Ok(worker)) => (worker, slot),
                // `spawn` already stopped anything it started; the request
                // never ran.
                Ok(Err(error)) => {
                    pending.fail_unsent(error);
                    return None;
                }
                Err(_) => {
                    pending.fail_unsent(RunpyError::Timeout(TimeoutStage::Queued));
                    return None;
                }
            }
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
    let outcome = outcome.await;
    let ended_normally = matches!(outcome, Ok(Ok(_) | Err(RunpyError::Execution { .. })));
    relay.finish(outcome, &worker_id);
    core.active.lock().remove(&worker_id);

    let (worker, slot) = if ended_normally && worker.is_ready() {
        match core.park(worker, slot) {
            Ok(()) => return None,
            Err(returned) => returned,
        }
    } else {
        (worker, slot)
    };
    let exit = retire(worker).await;
    drop(slot);
    exit
}

/// Retire a parked worker whose idle timeout passed, unless new work took it
/// first: a stale timer can never stop a busy worker.
async fn expire_idle(
    core: Arc<PoolCore>,
    token: u64,
    idle_timeout: Duration,
) -> Option<WorkerExit> {
    tokio::time::sleep(idle_timeout).await;
    let entry = {
        let mut idle = core.idle.lock();
        let position = idle.iter().position(|entry| entry.token == token)?;
        idle.swap_remove(position)
    };
    let exit = retire(entry.worker).await;
    drop(entry.slot);
    exit
}

/// Terminate a worker and return its exit, or `None` if the Manager is gone
/// and nothing retained it.
async fn retire(worker: Worker) -> Option<WorkerExit> {
    match worker.terminate().await {
        Ok(exit) => Some(exit),
        Err(_) => worker.wait().await.ok(),
    }
}
