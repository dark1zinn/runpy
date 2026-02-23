
mod integrity;
use std::path::PathBuf;

use crate::integrity::IntegrityChecker;

mod manager;
pub use manager::{WorkerIdentity, ActiveWorker};

mod protocol;

pub struct Runpy {
    integrity: IntegrityChecker,
    active_workers: Vec<ActiveWorker>,
    socket_path: PathBuf,
}

impl Runpy {
    pub fn new(venv_path: &str, scripts_path: &str) -> Self {
        let integrity = IntegrityChecker::new(venv_path, scripts_path);
        let active_workers = vec![];

        // Initial check
        if let Err(e) = integrity.perform_check() {
            eprintln!("Integrity check failed: {}", e);
        }

        
        Self { integrity, socket_path: PathBuf::from("/tmp/runpy"), active_workers }
    }

    pub async fn spawn_worker(&mut self, script: &str) -> Result<String, String> {
        // 1. Check script existence via Registry
        if !self.integrity.check_script(script) {
            return Err(format!("Script '{}' does not exist or failed integrity check", script));
        }

        let worker = ActiveWorker::new(&self.integrity.venv_path, &self.socket_path, script);
        let identity = worker.identity.name.clone();

        self.active_workers.push(worker);
        println!("Spawned worker: {}", identity);
        Ok(identity)
    }
}

impl Drop for Runpy {
    fn drop(&mut self) {
        println!("Shutting down all workers...");
        for worker in &mut self.active_workers {
            let _ = worker.child.kill();
            println!("Terminated worker: {}", worker.identity.name);
        }
        println!("All workers terminated. Cleaning up sockets...");
        let _ = std::fs::remove_dir_all("/tmp/runpy");
    }
}