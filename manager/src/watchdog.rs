use parking_lot::RwLock;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::protocol::WorkerHandle;
use crate::scribbler::Scribbler;

/// The health state of a monitored process.
#[derive(Debug, Clone, Serialize)]
pub enum ProcessState {
    Healthy,
    Frozen,
    Dead,
}

/// A snapshot report for a single worker process.
#[derive(Debug, Clone, Serialize)]
pub struct WorkerReport {
    pub worker_name: String,
    pub pid: u32,
    pub state: ProcessState,
    pub memory_kb: Option<u64>,
    pub cpu_percent: Option<f32>,
}

/// The watchdog service monitors all registered workers.
/// It can run periodic background health checks and produce on-demand reports.
pub struct WatchdogService {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    logger: Arc<Scribbler>,
}

impl WatchdogService {
    pub(crate) fn new(
        workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
        logger: Arc<Scribbler>,
    ) -> Self {
        Self { workers, logger }
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

    /// Generate a one-shot report for **all** workers.
    pub async fn report(&self) -> Vec<WorkerReport> {
        let workers = self.workers.read();
        let mut reports = Vec::new();

        for handle in workers.values() {
            let pid = handle.child.id();
            let state = match Self::read_proc_status(pid) {
                Some(_) => ProcessState::Healthy,
                None => ProcessState::Dead,
            };

            reports.push(WorkerReport {
                worker_name: handle.identity.name.clone(),
                pid,
                state,
                memory_kb: Self::read_proc_mem(pid),
                cpu_percent: None, // Requires sampling delta over time — future work
            });
        }

        reports
    }

    /// Generate a report for a **single** worker by name.
    pub async fn report_worker(&self, worker_id: &str) -> Option<WorkerReport> {
        let workers = self.workers.read();
        let handle = workers.get(worker_id)?;
        let pid = handle.child.id();
        let state = match Self::read_proc_status(pid) {
            Some(_) => ProcessState::Healthy,
            None => ProcessState::Dead,
        };

        Some(WorkerReport {
            worker_name: handle.identity.name.clone(),
            pid,
            state,
            memory_kb: Self::read_proc_mem(pid),
            cpu_percent: None,
        })
    }

    // ── Platform-specific helpers ──────────────────────────────────────

    #[cfg(target_os = "linux")]
    fn read_proc_status(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn read_proc_status(pid: u32) -> Option<String> {
        // Fallback: check if process exists via kill(pid, 0)
        let ret = unsafe { libc::kill(pid as i32, 0) };
        if ret == 0 { Some(String::new()) } else { None }
    }

    #[cfg(target_os = "linux")]
    fn read_proc_mem(pid: u32) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                return parts.get(1)?.parse().ok();
            }
        }
        None
    }

    #[cfg(not(target_os = "linux"))]
    fn read_proc_mem(_pid: u32) -> Option<u64> {
        None
    }
}
