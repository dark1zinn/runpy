//! # Runpy — Rust-Python Worker Manager
//!
//! A lightweight, async-first framework for spawning and managing Python worker
//! processes from Rust applications. Communication happens over Unix sockets using
//! a simple HTTP-like JSON protocol.
//!
//! ## Features
//!
//! - **Worker Management**: Spawn, monitor, and terminate Python workers
//! - **HTTP-like Protocol**: Clean JSON messages with methods, headers, and body
//! - **Watchdog Service**: Automatic health monitoring and dead worker cleanup
//! - **Structured Logging**: Environment-aware logging via [`Scribbler`]
//! - **Bidirectional Communication**: Send commands and receive responses
//!
//! ## Quick Start
//!
//! ```ignore
//! use runpy::{Manager, Message, Method};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Create manager with paths to Python venv and scripts
//!     let mut manager = Manager::new("./venv", "./scripts");
//!
//!     // Register a global message handler
//!     manager.on_message(|envelope| {
//!         println!("Received: {:?}", envelope.message);
//!     });
//!
//!     // Spawn a worker
//!     let mut worker = manager.worker("my_script");
//!     worker.env("API_KEY", "secret");
//!     worker.spawn().await.unwrap();
//!
//!     // Send a message
//!     worker.send_message(Message::execute(
//!         serde_json::json!({ "task": "process" })
//!     )).await.unwrap();
//! }
//! ```
//!
//! ## Environment Variables
//!
//! The [`Scribbler`] logger respects these environment variables:
//!
//! | Variable      | Values                                         | Description                     |
//! |---------------|------------------------------------------------|---------------------------------|
//! | `ENVIRONMENT` | `development`, `dev`                           | Enables maximum log verbosity   |
//! | `LOG`         | `0`-`5`, `off`, `error`, `warning`, `info`, `debug`, `verbose` | Sets log level |
//! | `NO_COLOR`    | (any value)                                    | Disables ANSI color output      |
//!
//! ## Protocol
//!
//! Messages follow an HTTP-like structure:
//!
//! ```json
//! {
//!   "method": "EXECUTE",
//!   "headers": {
//!     "X-Worker-Id": "my_script_29032026_abc1",
//!     "Content-Type": "application/json"
//!   },
//!   "body": { "task": "process", "data": [1, 2, 3] }
//! }
//! ```
//!
//! Available methods: `GET`, `POST`, `PUT`, `DELETE`, `EXECUTE`, `RETRY`,
//! `TERMINATE`, `META`, `READY`, `STATUS`, `LOG`, `DONE`, `ERROR`, `ACTION`.

mod integrity;
mod protocol;
mod manager;
pub mod scribbler;
mod watchdog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::integrity::IntegrityChecker;
use crate::manager::WorkerHandle;
use crate::protocol::{Envelope, MessageHandler};
use crate::watchdog::WatchdogService;

// ── Public re-exports ──────────────────────────────────────────────────
pub use protocol::{
    headers, ControlPlane, Envelope as MessageEnvelope, Headers, Mailer, Message, MessageSender,
    Method,
};
pub use manager::{Worker, WorkerIdentity};
pub use watchdog::{WatchdogService as Watchdog, WorkerReport, ProcessState};
pub use scribbler::{scribbler, Scribbler, LogLevel};

// ── Manager ────────────────────────────────────────────────────────────

/// Top-level orchestrator. Create one per application to manage all Python
/// worker processes.
///
/// ```ignore
/// let mut manager = Manager::new("path/to/.venv", "path/to/scripts");
/// manager.on_message(|env| { /* global handler */ });
///
/// let mut worker = manager.worker("my_script");
/// worker.env("KEY", "VALUE");
/// worker.on_message(|env| { /* per-worker handler */ });
/// worker.spawn().await.unwrap();
/// ```
pub struct Manager {
    integrity: IntegrityChecker,
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    socket_dir: PathBuf,
    global_handler: Option<MessageHandler>,

    /// Watchdog service — use `manager.dog.report().await` for health reports.
    pub dog: WatchdogService,
}

impl Manager {
    /// Create a new Manager, performing an initial integrity check.
    ///
    /// * `venv_path` — path to the Python virtual environment (must contain `bin/python`).
    /// * `scripts_path` — path to the directory holding `.py` scripts.
    pub fn new(venv_path: &str, scripts_path: &str) -> Self {
        let integrity = IntegrityChecker::new(venv_path, scripts_path);

        // Run initial integrity check (non-fatal — logs errors)
        if let Err(e) = integrity.perform_check() {
            scribbler::scribbler().error_with("Manager", &format!("Integrity check failed: {}", e));
        }

        let socket_dir = PathBuf::from("/tmp/runpy");
        let workers: Arc<RwLock<HashMap<String, WorkerHandle>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let dog = WatchdogService::new(workers.clone());

        // Start background watchdog with a 5-second interval
        dog.start_monitoring(5);

        Self {
            integrity,
            workers,
            socket_dir,
            global_handler: None,
            dog,
        }
    }

    /// Create a new `Worker` builder for the given script name (without `.py`).
    pub fn worker(&self, script: &str) -> Worker {
        Worker::new(
            script,
            &self.integrity.venv_path,
            &self.integrity.scripts_dir,
            &self.socket_dir,
            self.global_handler.clone(),
            self.workers.clone(),
        )
    }

    /// Register a **global** message handler that fires for every message from
    /// every worker, *before* worker-specific handlers.
    pub fn on_message<F>(&mut self, handler: F)
    where
        F: Fn(Envelope) + Send + Sync + 'static,
    {
        self.global_handler = Some(Arc::new(handler));
    }

    /// Re-run the full integrity check (venv, scripts dir, script index).
    pub fn check_integrity(&self) -> Result<(), String> {
        self.integrity.perform_check()
    }

    /// Broadcast a message to all active workers.
    /// Returns a map of worker_id -> Result indicating success or failure for each.
    pub async fn broadcast(&self, msg: Message) -> HashMap<String, Result<(), String>> {
        let workers = self.workers.read().await;
        let mut results = HashMap::new();

        for (worker_id, handle) in workers.iter() {
            let result = handle.sender.send(msg.clone()).await;
            results.insert(worker_id.clone(), result);
        }

        results
    }

    /// Terminate all active workers gracefully.
    /// Sends TERMINATE to all workers, waits briefly, then force-kills any remaining.
    pub async fn terminate_all(&mut self) {
        // First, send TERMINATE to all
        let _ = self.broadcast(Message::terminate()).await;

        // Give workers time to shut down cleanly
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Force-kill any remaining
        let mut workers = self.workers.write().await;
        for (id, mut handle) in workers.drain() {
            let _ = handle.child.kill();
            let _ = std::fs::remove_file(&handle.sock_path);
            scribbler::scribbler().info_with("Manager", &format!("Terminated worker: {} ({})", handle.identity.name, id));
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        scribbler::scribbler().info_with("Manager", "Shutting down all workers...");

        // `try_write()` is non-blocking and safe inside an async runtime
        // (unlike `blocking_write()` which panics on a current-thread runtime).
        match self.workers.try_write() {
            Ok(mut workers) => {
                for (id, mut handle) in workers.drain() {
                    let _ = handle.child.kill();
                    let _ = std::fs::remove_file(&handle.sock_path);
                    scribbler::scribbler().info_with("Manager", &format!("Terminated worker: {} ({})", handle.identity.name, id));
                }
            }
            Err(_) => {
                scribbler::scribbler().warning_with("Manager", "Could not acquire worker lock during shutdown");
            }
        }

        let _ = std::fs::remove_dir_all(&self.socket_dir);
        scribbler::scribbler().success("All workers terminated. Socket directory cleaned.");
    }
}
