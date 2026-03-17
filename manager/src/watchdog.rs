use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use serde::Serialize;

use crate::manager::WorkerHandle;

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
#[derive(Clone)]
pub struct WatchdogService {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
}

impl WatchdogService {
    pub fn new(workers: Arc<RwLock<HashMap<String, WorkerHandle>>>) -> Self {
        Self { workers }
    }

    /// Start a background task that periodically checks every worker.
    /// Dead workers are logged (and could be restarted in the future).
    pub fn start_monitoring(&self, interval_secs: u64) {
        let workers = self.workers.clone();
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                let mut workers = workers.write().await;
                let mut dead_ids: Vec<String> = Vec::new();

                for (id, handle) in workers.iter_mut() {
                    match handle.child.try_wait() {
                        Ok(Some(status)) => {
                            eprintln!(
                                "[Watchdog] Worker '{}' (pid {}) exited with status: {}",
                                handle.identity.name,
                                handle.child.id(),
                                status
                            );
                            dead_ids.push(id.clone());
                        }
                        Ok(None) => {
                            // Still running — healthy as far as OS is concerned
                        }
                        Err(e) => {
                            eprintln!(
                                "[Watchdog] Error checking worker '{}': {}",
                                handle.identity.name, e
                            );
                            dead_ids.push(id.clone());
                        }
                    }
                }

                // Clean up dead workers
                for id in dead_ids {
                    let handle = workers.remove(&id);
                    if let Some(h) = handle {
                        let _ = std::fs::remove_file(&h.sock_path);
                        eprintln!("[Watchdog] Removed dead worker '{}'", h.identity.name);
                    }
                }
            }
        });
    }

    /// Generate a one-shot report for **all** workers.
    pub async fn report(&self) -> Vec<WorkerReport> {
        let workers = self.workers.read().await;
        let mut reports = Vec::new();

        for (_id, handle) in workers.iter() {
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
        let workers = self.workers.read().await;
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
        if ret == 0 {
            Some(String::new())
        } else {
            None
        }
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
