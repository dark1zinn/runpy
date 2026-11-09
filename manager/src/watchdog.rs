//! Process monitoring: dead-worker detection and sampled resource reports.

use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::lifecycle::LifecycleState;
use crate::protocol::WorkerHandle;
use crate::scribbler::Scribbler;

/// State of a worker's process group.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ProcessState {
    /// At least one live member of the group is not stopped.
    Running,
    /// Every observed live member is stopped (for example by `SIGSTOP`).
    Stopped,
    /// The worker's `uv` process exited (authoritative, from Runpy's child
    /// handle rather than PID existence).
    Exited,
    /// Not yet sampled, or no member could be observed (permissions or a
    /// sampling race). Metrics are then `None`, never a fabricated zero.
    Unknown,
}

/// One-point-in-time report for a managed worker's process group.
#[derive(Clone, Debug, Serialize)]
pub struct WorkerReport {
    /// Trusted Manager-generated worker identity.
    pub worker_name: String,
    /// Process ID of the `uv` parent; also the process group ID.
    pub pid: u32,
    /// Current group state.
    pub state: ProcessState,
    /// Sum of resident memory of all live group members, in bytes. Pages
    /// shared between members are counted once per member.
    pub memory_bytes: Option<u64>,
    /// Sum of group members' CPU usage over the last sampling interval, in
    /// percent of one core (so it can exceed 100). `None` until two samples
    /// of the same members exist.
    pub cpu_percent: Option<f32>,
    /// Live processes found in the group, including `uv` and Python.
    pub process_count: Option<usize>,
    /// When the metrics were sampled.
    pub sampled_at: Option<SystemTime>,
}

/// Aggregated metrics for one process group.
#[derive(Clone, Debug)]
struct GroupSample {
    memory_bytes: u64,
    cpu_percent: Option<f32>,
    process_count: usize,
    stopped: bool,
    sampled_at: SystemTime,
}

/// Sampling state; one instance, used only on a blocking thread.
struct Sampler {
    system: System,
    /// PIDs per group seen in the previous sample, to know when a CPU delta
    /// covers every member.
    previous: HashMap<u32, HashSet<Pid>>,
}

impl Sampler {
    fn new() -> Self {
        // On Linux sysinfo otherwise keeps `/proc/<pid>/stat` open for every
        // process it has seen, up to half the descriptor limit: hundreds of
        // descriptors per host for a scan that runs once per second. The
        // setting is process-wide and only trades descriptors for reopening
        // those files on each refresh.
        sysinfo::set_open_files_limit(0);
        Self {
            system: System::new(),
            previous: HashMap::new(),
        }
    }

    /// Refresh every process and aggregate the members of `groups`.
    ///
    /// Membership comes from `getpgid`, not sysinfo's user group. Zombies and
    /// threads are skipped; a process that vanished mid-sample is skipped.
    fn sample(&mut self, groups: &HashSet<u32>) -> HashMap<u32, GroupSample> {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let sampled_at = SystemTime::now();
        let mut members: HashMap<u32, HashSet<Pid>> = HashMap::new();
        let mut samples: HashMap<u32, GroupSample> = HashMap::new();
        for (pid, process) in self.system.processes() {
            if process.thread_kind().is_some()
                || matches!(
                    process.status(),
                    ProcessStatus::Zombie | ProcessStatus::Dead
                )
            {
                continue;
            }
            // SAFETY: getpgid only reads the kernel's process table.
            let group = unsafe { libc::getpgid(pid.as_u32() as libc::pid_t) };
            let Ok(group) = u32::try_from(group) else {
                continue;
            };
            if !groups.contains(&group) {
                continue;
            }
            members.entry(group).or_default().insert(*pid);
            let sample = samples.entry(group).or_insert(GroupSample {
                memory_bytes: 0,
                cpu_percent: Some(0.0),
                process_count: 0,
                stopped: true,
                sampled_at,
            });
            sample.memory_bytes += process.memory();
            if let Some(cpu) = sample.cpu_percent.as_mut() {
                *cpu += process.cpu_usage();
            }
            sample.process_count += 1;
            sample.stopped &= is_stopped(*pid, process);
        }
        for (group, sample) in &mut samples {
            let complete = self
                .previous
                .get(group)
                .is_some_and(|previous| members[group].is_subset(previous));
            if !complete {
                sample.cpu_percent = None;
            }
        }
        self.previous = members;
        samples
    }
}

/// Whether `pid` is stopped by job control (for example `SIGSTOP`).
///
/// sysinfo 0.37 on macOS reads a process's BSD status only when it first sees
/// the process and afterwards reports its first thread's run state, which a
/// stop (a task suspension) does not change, so read the BSD status directly.
#[cfg(target_os = "macos")]
fn is_stopped(pid: Pid, _process: &sysinfo::Process) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info` and
    // returns how many it wrote; the struct is only read when complete.
    let written = unsafe {
        libc::proc_pidinfo(
            pid.as_u32() as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    written == size && unsafe { info.assume_init() }.pbi_status == libc::SSTOP
}

/// Whether `pid` is stopped by job control (for example `SIGSTOP`).
#[cfg(not(target_os = "macos"))]
fn is_stopped(_pid: Pid, process: &sysinfo::Process) -> bool {
    process.status() == ProcessStatus::Stop
}

/// Shared process monitor owned by a [`crate::Manager`].
///
/// The control plane's monitor asks it every five seconds for workers whose
/// `uv` process exited, and every second samples each worker's process group
/// on a blocking thread. Reports read the latest sample; they do not
/// implement application retry or alert policy.
pub struct WatchdogService {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    logger: Arc<Scribbler>,
    sampler: Arc<Mutex<Sampler>>,
    /// Latest sample per process group.
    samples: Arc<RwLock<HashMap<u32, GroupSample>>>,
}

impl WatchdogService {
    pub(crate) fn new(
        workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
        logger: Arc<Scribbler>,
    ) -> Self {
        Self {
            workers,
            logger,
            sampler: Arc::new(Mutex::new(Sampler::new())),
            samples: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) async fn dead_worker_ids(&self) -> Vec<String> {
        let mut workers = self.workers.write();
        let mut dead_ids = Vec::new();

        for (id, handle) in workers.iter_mut() {
            match handle.child.try_wait() {
                Ok(Some(status)) => {
                    self.logger.warning_with(
                        "Watchdog",
                        &format!(
                            "Worker '{}' (pid {}) exited with status: {}",
                            handle.identity.name,
                            handle.child.id(),
                            status
                        ),
                    );
                    dead_ids.push(id.clone());
                }
                Ok(None) => {}
                Err(error) => {
                    self.logger.error_with(
                        "Watchdog",
                        &format!("Error checking worker '{}': {error}", handle.identity.name),
                    );
                    dead_ids.push(id.clone());
                }
            }
        }

        dead_ids
    }

    /// Sample every registered worker's process group once.
    ///
    /// The returned future owns what it needs, so callers can release the
    /// control plane while it waits. Sampling runs on Tokio's blocking pool;
    /// the single sampler is locked for the duration, so overlapping calls
    /// queue instead of sampling twice.
    pub(crate) fn sample(&self) -> impl Future<Output = ()> + Send + 'static {
        let groups: HashSet<u32> = self
            .workers
            .read()
            .values()
            .map(|handle| handle.process_group_id)
            .collect();
        let sampler = self.sampler.clone();
        let samples = self.samples.clone();
        async move {
            if groups.is_empty() {
                samples.write().clear();
                return;
            }
            if let Ok(sampled) =
                tokio::task::spawn_blocking(move || sampler.lock().sample(&groups)).await
            {
                *samples.write() = sampled;
            }
        }
    }

    /// Generate a report for every currently registered worker.
    ///
    /// Metrics come from the latest per-second sample, so a worker spawned
    /// moments ago reports `Unknown` until it is first sampled.
    ///
    /// ```no_run
    /// # async fn demo(manager: &runpy::Manager) {
    /// for report in manager.watchdog().report().await {
    ///     println!(
    ///         "{}: {:?}, {:?} bytes, {:?}% CPU",
    ///         report.worker_name, report.state, report.memory_bytes, report.cpu_percent
    ///     );
    /// }
    /// # }
    /// ```
    pub async fn report(&self) -> Vec<WorkerReport> {
        let workers = self.workers.read();
        let samples = self.samples.read();
        workers
            .values()
            .map(|handle| Self::build_report(handle, &samples))
            .collect()
    }

    /// Generate a report for one trusted worker ID, or `None` when the worker
    /// is not registered.
    ///
    /// ```no_run
    /// # async fn demo(manager: &runpy::Manager) {
    /// if let Some(report) = manager.watchdog().report_worker("worker-id").await {
    ///     println!("pid {} uses {:?} bytes", report.pid, report.memory_bytes);
    /// }
    /// # }
    /// ```
    pub async fn report_worker(&self, worker_id: &str) -> Option<WorkerReport> {
        let workers = self.workers.read();
        let handle = workers.get(worker_id)?;
        Some(Self::build_report(handle, &self.samples.read()))
    }

    fn build_report(handle: &WorkerHandle, samples: &HashMap<u32, GroupSample>) -> WorkerReport {
        let exited = matches!(*handle.lifecycle.borrow(), LifecycleState::Exited(_));
        let sample = if exited {
            None
        } else {
            samples.get(&handle.process_group_id)
        };
        let state = match sample {
            _ if exited => ProcessState::Exited,
            Some(sample) if sample.stopped => ProcessState::Stopped,
            Some(_) => ProcessState::Running,
            None => ProcessState::Unknown,
        };
        WorkerReport {
            worker_name: handle.identity.name.clone(),
            pid: handle.child.id(),
            state,
            memory_bytes: sample.map(|sample| sample.memory_bytes),
            cpu_percent: sample.and_then(|sample| sample.cpu_percent),
            process_count: sample.map(|sample| sample.process_count),
            sampled_at: sample.map(|sample| sample.sampled_at),
        }
    }
}
