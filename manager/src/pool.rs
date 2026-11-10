//! Same-script request pools.
//!
//! A pool runs independent requests for one [`WorkerBuilder`] template across
//! separate worker processes. It owns no wire protocol of its own: each
//! request becomes an ordinary correlated request on a worker. Workers are
//! retired after one request unless the pool explicitly enables reuse.
//!
//! Capacity has three layers. The pool slot semaphore bounds this pool's own
//! workers (`max_workers`). Warm minima are backed by Manager-wide slots
//! taken out of general circulation at creation, so neither direct spawns
//! nor other pools can borrow them. Workers above the minimum obtain
//! Manager-wide slots from the fair [`Scheduler`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::{Duration, Instant, sleep_until, timeout, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::lifecycle::WorkerExit;
use crate::manager::{Worker, WorkerBuilder};
use crate::protocol::{ControlPlane, Data, ProcessSlot};
use crate::request::{PendingRequest, Request};
use crate::scheduler::Scheduler;

/// Default bound on requests waiting in one pool's queue.
pub const DEFAULT_POOL_QUEUE_CAPACITY: usize = 64;
/// Default time a reusable pool worker may stay idle before it is retired.
pub const DEFAULT_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for a [`WorkerPool`].
///
/// ```
/// use runpy::PoolConfig;
/// use std::time::Duration;
///
/// // Two warm workers always ready, up to three under load.
/// let config = PoolConfig::default()
///     .min_workers(2)
///     .max_workers(3)
///     .reuse_workers(true)
///     .idle_timeout(Duration::from_secs(60))
///     .queue_capacity(16);
/// # let _ = config;
/// ```
#[derive(Clone, Copy, Debug)]
pub struct PoolConfig {
    min_workers: usize,
    max_workers: Option<usize>,
    queue_capacity: usize,
    reuse_workers: bool,
    idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min_workers: 0,
            max_workers: None,
            queue_capacity: DEFAULT_POOL_QUEUE_CAPACITY,
            reuse_workers: false,
            idle_timeout: DEFAULT_POOL_IDLE_TIMEOUT,
        }
    }
}

impl PoolConfig {
    /// Warm workers kept ready at all times; default 0.
    ///
    /// A positive minimum requires [`PoolConfig::reuse_workers`]. Their
    /// Manager-wide slots are reserved when the pool is created and are
    /// never used by direct spawns or other pools; the sum of every pool's
    /// minimum must fit the Manager's `max_workers`. Pool creation waits until
    /// all minimum workers are ready.
    pub fn min_workers(mut self, workers: usize) -> Self {
        self.min_workers = workers;
        self
    }

    /// Most worker processes this pool runs at once; defaults to the
    /// Manager's `max_workers`. Must be positive, at least `min_workers`,
    /// and at most the Manager's `max_workers`.
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

    /// How long a reusable worker above the minimum may stay idle before it
    /// is retired; default [`DEFAULT_POOL_IDLE_TIMEOUT`] (30 s). Must be
    /// positive. Minimum workers never time out.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }
}

/// A request admitted to the pool and waiting for a worker.
struct Job {
    pending: PendingRequest,
    /// Released when the job leaves the queue for a worker.
    _queued: OwnedSemaphorePermit,
}

/// A started worker together with the pool slot it occupies.
struct Lease {
    worker: Worker,
    slot: OwnedSemaphorePermit,
    /// Whether its process slot comes from the pool's warm reservation.
    reserved: bool,
}

/// How a queued job obtained a worker.
enum Assigned {
    /// A pool slot for a worker that still has to be started.
    Fresh(OwnedSemaphorePermit),
    /// A parked worker.
    Idle(Lease),
}

/// A worker kept between requests.
struct IdleWorker {
    /// Distinguishes this parking from later ones of the same worker.
    token: u64,
    lease: Lease,
    /// Idle-timeout timer, or the exit watcher of a minimum worker.
    timer: AbortHandle,
}

/// State shared by every [`WorkerPool`] handle and the pool's tasks.
pub(crate) struct PoolCore {
    id: String,
    /// Scheduler key; grants rotate across pools in key order.
    key: u64,
    template: WorkerBuilder,
    control_plane: Weak<ControlPlane>,
    scheduler: Arc<Scheduler>,
    jobs: mpsc::UnboundedSender<Job>,
    queue_slots: Arc<Semaphore>,
    /// This pool's `max_workers`.
    slots: Arc<Semaphore>,
    min_workers: usize,
    /// Process slots reserved for the warm minimum.
    reserve: Arc<Semaphore>,
    /// Whether the reservation was already returned to the Manager.
    released: AtomicBool,
    closed: AtomicBool,
    closing: watch::Sender<bool>,
    /// Workers currently running a pool request, keyed by worker ID.
    active: Mutex<HashMap<String, Worker>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    /// Request runs, idle timers, and replacements; each may own a worker.
    runs: Arc<Mutex<JoinSet<Option<WorkerExit>>>>,
    /// `Some(idle_timeout)` when finished workers are kept for reuse.
    reuse: Option<Duration>,
    /// Parked workers available for the next request.
    idle: Mutex<Vec<IdleWorker>>,
    /// Wakes the dispatcher when a worker is parked.
    parked: Notify,
    next_token: AtomicU64,
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

    /// Retire one idle worker above the minimum so another pool can use its
    /// Manager-wide slot. Returns whether one was found.
    pub(crate) fn retire_surplus_idle(&self) -> bool {
        let entry = {
            let mut idle = self.idle.lock();
            let Some(position) = idle.iter().position(|entry| !entry.lease.reserved) else {
                return false;
            };
            idle.swap_remove(position)
        };
        entry.timer.abort();
        self.spawn_run(retire_lease(entry.lease));
        true
    }

    fn spawn_run(&self, task: impl Future<Output = Option<WorkerExit>> + Send + 'static) {
        let mut runs = self.runs.lock();
        while runs.try_join_next().is_some() {}
        runs.spawn(task);
    }

    /// Keep a worker idle with its pool slot. Workers above the minimum get
    /// an idle timeout; minimum workers get an exit watcher that replaces
    /// them once if they die.
    ///
    /// Hands the lease back when reuse is off or the pool is closed: `close`
    /// drains the idle list under the same lock after setting `closed`, so a
    /// worker can never be parked after it.
    fn park(self: &Arc<Self>, lease: Lease) -> Result<(), Lease> {
        let Some(idle_timeout) = self.reuse else {
            return Err(lease);
        };
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        // Timers look their entry up under this lock, so they cannot run
        // before the entry exists.
        let mut idle = self.idle.lock();
        if self.closed.load(Ordering::Acquire) {
            return Err(lease);
        }
        let timer = {
            let mut runs = self.runs.lock();
            while runs.try_join_next().is_some() {}
            if lease.reserved {
                runs.spawn(watch_minimum(self.clone(), token, lease.worker.clone()))
            } else {
                runs.spawn(expire_idle(self.clone(), token, idle_timeout))
            }
        };
        idle.push(IdleWorker {
            token,
            lease,
            timer,
        });
        drop(idle);
        self.parked.notify_one();
        Ok(())
    }

    /// Take a parked worker that is still ready, cancelling its timer.
    /// Parked workers that exited meanwhile are discarded (minimum workers
    /// are replaced once).
    fn take_idle(self: &Arc<Self>) -> Option<Lease> {
        loop {
            let entry = self.idle.lock().pop()?;
            entry.timer.abort();
            if entry.lease.worker.is_ready() {
                return Some(entry.lease);
            }
            let reserved = entry.lease.reserved;
            drop(entry);
            if reserved {
                self.spawn_run(replace_minimum(self.clone()));
            }
        }
    }

    /// Close the pool after a minimum worker could not be replaced.
    fn fail(&self, error: &RunpyError) {
        if let Some(control_plane) = self.control_plane.upgrade() {
            control_plane.logger().error_with(
                "Pool",
                &format!("{}: minimum worker could not be replaced: {error}", self.id),
            );
        }
        self.closed.store(true, Ordering::Release);
        self.queue_slots.close();
        self.closing.send_replace(true);
    }

    /// Return the warm reservation to the Manager once every minimum worker
    /// has been reaped. Runs at most once.
    async fn release_reservation(&self) {
        if self.min_workers == 0 || self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let count = u32::try_from(self.min_workers).expect("min_workers fits the global cap");
        if let Ok(permits) = self.reserve.acquire_many(count).await {
            permits.forget();
        }
        if let Some(control_plane) = self.control_plane.upgrade() {
            control_plane.processes().add_permits(self.min_workers);
            control_plane.release_minima(self.min_workers);
        }
    }
}

/// Handle to a pool of workers for one script template.
///
/// Created by [`crate::Manager::pool`]. Requests are started in FIFO order up
/// to the pool's `max_workers`. By default each request gets its own new
/// worker process, retired after the request's terminal result, so no state
/// carries between requests; with [`PoolConfig::reuse_workers`] a finished
/// worker serves later requests, and [`PoolConfig::min_workers`] keeps some
/// warm. Requests have no affinity either way. Cloning is cheap; the Manager
/// owns the pool, so dropping every handle neither stops it nor leaks it: the
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
    /// Validate `config`, reserve and start the warm minimum, and start the
    /// pool's dispatcher. Without a minimum no worker process is created.
    pub(crate) async fn start(
        key: u64,
        template: WorkerBuilder,
        control_plane: &Arc<ControlPlane>,
        scheduler: Arc<Scheduler>,
        config: PoolConfig,
    ) -> Result<(Self, Weak<PoolCore>), RunpyError> {
        let global = control_plane.max_workers();
        let max_workers = config.max_workers.unwrap_or(global);
        let min_workers = config.min_workers;
        if max_workers == 0 || max_workers > global || min_workers > max_workers {
            return Err(RunpyError::InvalidConfig(format!(
                "pool workers must satisfy min_workers ({min_workers}) <= max_workers \
                 ({max_workers}) <= the Manager's max_workers ({global}), with max_workers > 0"
            )));
        }
        if min_workers > 0 && !config.reuse_workers {
            return Err(RunpyError::InvalidConfig(
                "pool min_workers requires reuse_workers".to_string(),
            ));
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
        let startup_timeout = template.startup_timeout()?;

        // Take the minimum's slots out of general circulation.
        control_plane.reserve_minima(min_workers)?;
        if min_workers > 0 {
            let count = u32::try_from(min_workers).expect("min_workers fits the global cap");
            match timeout(
                startup_timeout,
                control_plane.processes().clone().acquire_many_owned(count),
            )
            .await
            {
                Ok(Ok(permits)) => permits.forget(),
                Ok(Err(_)) => {
                    control_plane.release_minima(min_workers);
                    return Err(RunpyError::ManagerClosed);
                }
                Err(_) => {
                    control_plane.release_minima(min_workers);
                    return Err(RunpyError::Timeout(TimeoutStage::Admission));
                }
            }
        }

        let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
        let (closing, closing_rx) = watch::channel(false);
        let core = Arc::new(PoolCore {
            id: format!("pool-{key}"),
            key,
            template,
            control_plane: Arc::downgrade(control_plane),
            scheduler,
            jobs: jobs_tx,
            queue_slots: Arc::new(Semaphore::new(config.queue_capacity)),
            slots: Arc::new(Semaphore::new(max_workers)),
            min_workers,
            reserve: Arc::new(Semaphore::new(min_workers)),
            released: AtomicBool::new(false),
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
        let pool = Self { core: core.clone() };

        // Start the minimum eagerly; any failure rolls the whole pool back.
        let mut starting = JoinSet::new();
        for _ in 0..min_workers {
            let core = core.clone();
            starting.spawn(async move { start_reserved(&core).await });
        }
        let mut started = Vec::new();
        let mut failure = None;
        while let Some(result) = starting.join_next().await {
            match result {
                Ok(Ok(lease)) => started.push(lease),
                Ok(Err(error)) => failure = failure.or(Some(error)),
                Err(error) => {
                    failure = failure.or(Some(RunpyError::Io(std::io::Error::other(error))))
                }
            }
        }
        if let Some(error) = failure {
            core.closed.store(true, Ordering::Release);
            for lease in started {
                retire_lease(lease).await;
            }
            core.release_reservation().await;
            return Err(error);
        }
        for lease in started {
            if let Err(lease) = core.park(lease) {
                retire_lease(lease).await;
            }
        }

        let task = tokio::spawn(dispatch(core.clone(), jobs_rx, closing_rx));
        *core.dispatcher.lock() = Some(task);
        Ok((pool, Arc::downgrade(&core)))
    }

    /// Manager-assigned identity of this pool.
    pub fn id(&self) -> &str {
        &self.core.id
    }

    /// Run one request on a pool worker and wait for its result.
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
    /// workers, return the warm reservation to the Manager, and return the
    /// exit of every worker this pool still owned. Requests interrupted this
    /// way fail with [`RunpyError::OutcomeUnknown`]. Closing twice returns
    /// no exits.
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
            stopping.spawn(retire_lease(entry.lease));
        }
        let mut exits: Vec<WorkerExit> = stopping.join_all().await.into_iter().flatten().collect();
        let mut runs = std::mem::take(&mut *core.runs.lock());
        while let Some(exit) = runs.join_next().await {
            if let Ok(Some(exit)) = exit {
                exits.push(exit);
            }
        }
        core.release_reservation().await;
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
) {
    'jobs: loop {
        let job = tokio::select! {
            job = jobs.recv() => job,
            _ = closing.wait_for(|closing| *closing) => None,
        };
        let Some(job) = job else { break };
        let assigned = loop {
            if let Some(lease) = core.take_idle() {
                break Assigned::Idle(lease);
            }
            tokio::select! {
                slot = core.slots.clone().acquire_owned() => {
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
            if let Assigned::Idle(lease) = assigned
                && let Err(lease) = core.park(lease)
            {
                core.spawn_run(retire_lease(lease));
            }
            continue;
        }
        core.spawn_run(run(core.clone(), pending, assigned));
    }
    // Late submissions now fail at send; everything still queued is failed.
    jobs.close();
    while let Ok(job) = jobs.try_recv() {
        job.pending.fail_unsent(RunpyError::PoolClosed);
    }
}

/// Start a worker on a slot reserved for the minimum.
async fn start_reserved(core: &Arc<PoolCore>) -> Result<Lease, RunpyError> {
    let slot = core
        .slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| RunpyError::PoolClosed)?;
    let process = core
        .reserve
        .clone()
        .try_acquire_owned()
        .map_err(|_| RunpyError::PoolClosed)?;
    let worker = core.template.clone().spawn_with(Some(process)).await?;
    Ok(Lease {
        worker,
        slot,
        reserved: true,
    })
}

/// Obtain a process slot for a new worker: from the warm reservation when
/// one is free, otherwise fairly from the Manager-wide limit.
async fn process_slot(
    core: &PoolCore,
    deadline: Instant,
) -> Result<(ProcessSlot, bool), RunpyError> {
    if let Ok(slot) = core.reserve.clone().try_acquire_owned() {
        return Ok((slot, true));
    }
    match core.scheduler.acquire(core.key, deadline).await {
        Some(slot) => Ok((slot, false)),
        None => Err(core.scheduler.unavailable()),
    }
}

/// Run one request on its assigned worker, starting a fresh one if needed,
/// and wait for its terminal result even past the caller's deadline.
///
/// Afterwards the worker is parked for reuse when the pool allows it and the
/// request ended normally (`done` or a reported execution error); otherwise
/// it is retired, and the pool slot is released only after its process
/// group was stopped and reaped. A lost minimum worker is replaced once.
async fn run(
    core: Arc<PoolCore>,
    pending: PendingRequest,
    assigned: Assigned,
) -> Option<WorkerExit> {
    let lease = match assigned {
        Assigned::Idle(lease) => lease,
        Assigned::Fresh(slot) => {
            let deadline = pending.deadline();
            let started = timeout_at(deadline, async {
                let (process, reserved) = process_slot(&core, deadline).await?;
                let worker = core.template.clone().spawn_with(Some(process)).await?;
                Ok::<_, RunpyError>((worker, reserved))
            })
            .await;
            match started {
                Ok(Ok((worker, reserved))) => Lease {
                    worker,
                    slot,
                    reserved,
                },
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
        // A still-ready worker is parked (parking refuses once the pool is
        // closed or reuse is off); one that is no longer ready is retired.
        return finish_lease(&core, lease, true).await;
    }

    let worker_id = lease.worker.id().to_string();
    core.active
        .lock()
        .insert(worker_id.clone(), lease.worker.clone());
    let (inner, outcome, relay) = pending.relay();
    match lease.worker.control_plane() {
        Ok(control_plane) => control_plane.enqueue(&worker_id, inner),
        Err(_) => drop(inner),
    }
    let outcome = outcome.await;
    let ended_normally = matches!(outcome, Ok(Ok(_) | Err(RunpyError::Execution { .. })));
    relay.finish(outcome, &worker_id);
    core.active.lock().remove(&worker_id);
    finish_lease(&core, lease, ended_normally).await
}

/// Park a healthy worker or retire it; a retired minimum worker is replaced
/// once while the pool is open (parking only fails once it closed).
async fn finish_lease(core: &Arc<PoolCore>, lease: Lease, healthy: bool) -> Option<WorkerExit> {
    let lease = if healthy && lease.worker.is_ready() {
        match core.park(lease) {
            Ok(()) => return None,
            Err(lease) => lease,
        }
    } else {
        lease
    };
    let reserved = lease.reserved;
    let exit = retire_lease(lease).await;
    if reserved && !core.closed.load(Ordering::Acquire) {
        core.spawn_run(replace_minimum(core.clone()));
    }
    exit
}

/// Restore a lost minimum worker with one startup attempt.
///
/// Waits, until the pool closes, for a free reservation and a pool slot
/// instead of failing a busy pool: the lost worker's reservation frees once
/// it is reaped, and if a request claims it first that worker restores the
/// minimum and this task keeps waiting harmlessly. The reservation semaphore
/// bounds reserved workers, so waiting can never overshoot the minimum. Only
/// a failed start closes the pool, rather than retrying in a loop.
async fn replace_minimum(core: Arc<PoolCore>) -> Option<WorkerExit> {
    let mut closing = core.closing.subscribe();
    let permits = async {
        let process = core.reserve.clone().acquire_owned().await.ok()?;
        let slot = core.slots.clone().acquire_owned().await.ok()?;
        Some((slot, process))
    };
    let (slot, process) = tokio::select! {
        permits = permits => permits?,
        _ = closing.wait_for(|closing| *closing) => return None,
    };
    if core.closed.load(Ordering::Acquire) {
        return None;
    }
    match core.template.clone().spawn_with(Some(process)).await {
        Ok(worker) => {
            let lease = Lease {
                worker,
                slot,
                reserved: true,
            };
            match core.park(lease) {
                Ok(()) => None,
                Err(lease) => retire_lease(lease).await,
            }
        }
        Err(error) => {
            core.fail(&error);
            None
        }
    }
}

/// Retire a parked worker above the minimum whose idle timeout passed,
/// unless new work took it first: a stale timer never stops a busy worker.
async fn expire_idle(
    core: Arc<PoolCore>,
    token: u64,
    idle_timeout: Duration,
) -> Option<WorkerExit> {
    tokio::time::sleep(idle_timeout).await;
    let entry = take_parked(&core, token)?;
    retire_lease(entry.lease).await
}

/// Watch a parked minimum worker and replace it once if it exits while idle.
async fn watch_minimum(core: Arc<PoolCore>, token: u64, worker: Worker) -> Option<WorkerExit> {
    let exit = worker.wait().await.ok();
    if let Some(entry) = take_parked(&core, token) {
        drop(entry);
        if !core.closed.load(Ordering::Acquire) {
            core.spawn_run(replace_minimum(core.clone()));
        }
    }
    exit
}

fn take_parked(core: &PoolCore, token: u64) -> Option<IdleWorker> {
    let mut idle = core.idle.lock();
    let position = idle.iter().position(|entry| entry.token == token)?;
    Some(idle.swap_remove(position))
}

/// Terminate a worker, then release its pool slot. Returns its exit, or
/// `None` if the Manager is gone and nothing retained it.
async fn retire_lease(lease: Lease) -> Option<WorkerExit> {
    let exit = match lease.worker.terminate().await {
        Ok(exit) => Some(exit),
        Err(_) => lease.worker.wait().await.ok(),
    };
    drop(lease.slot);
    exit
}
