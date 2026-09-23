//! # Runpy — Rust-Python Worker Manager
//!
//! Runpy spawns and manages Python worker processes and exchanges bare JSON
//! envelopes over length-prefixed Unix socket connections.
//!
//! ## Envelope
//!
//! Every payload contains exactly two JSON objects:
//!
//! ```json
//! {
//!   "meta": {
//!     "x_wid": "worker-id",
//!     "x_spath": "/tmp/runpy/rp_worker.sock",
//!     "correlation_id": 42
//!   },
//!   "data": { "task": "process" }
//! }
//! ```
//!
//! Applications own the schema and type safety of `data` and non-`x_`
//! metadata. Runpy reserves every `x_` key for worker identity, socket
//! identity, and lifecycle routing.
//!
//! ## uv-managed worker scripts
//!
//! [`Manager`] launches each worker with `uv run --no-project --script`.
//! The worker's [PEP 723](https://packaging.python.org/en/latest/specifications/inline-script-metadata/)
//! metadata declares its Python requirement and complete dependency set:
//!
//! ```python
//! # /// script
//! # requires-python = ">=3.10"
//! # dependencies = [
//! #   "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker",
//! # ]
//! # ///
//! ```
//!
//! `uv` selects or downloads a compatible Python and maintains an isolated,
//! cached environment. Runpy does not create a project `.venv` or run
//! `uv sync`. An adjacent `<script>.py.lock`, created explicitly with
//! `uv lock --script <script>.py`, is optional. When present, Runpy passes
//! `--locked`, so stale locks fail instead of being modified at launch.
//!
//! ## Quick start
//!
//! ```ignore
//! use runpy::{Data, Envelope, Manager};
//! use serde_json::json;
//!
//! fn object(value: serde_json::Value) -> Data {
//!     value.as_object().cloned().expect("JSON object")
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut manager = Manager::new("./scripts");
//!     manager.on_message(|inbound| {
//!         println!("Received: {:?}", inbound.envelope);
//!     });
//!
//!     let mut worker = manager.worker("my_script");
//!     worker.spawn().await.unwrap();
//!     worker
//!         .send_message(Envelope::execute(object(json!({"task": "process"}))))
//!         .await
//!         .unwrap();
//! }
//! ```

mod integrity;
mod manager;
mod protocol;
pub mod scribbler;
mod watchdog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::integrity::IntegrityChecker;
use crate::manager::{WorkerHandle, force_stop_worker};
use crate::watchdog::WatchdogService;

// ── Public re-exports ──────────────────────────────────────────────────
pub use manager::{Worker, WorkerIdentity};
pub use protocol::{
    ControlPlane, Data, Envelope, EnvelopeError, InboundEnvelope, Mailer, MessageHandler,
    MessageSender, Meta,
};
pub use scribbler::{LogLevel, Scribbler, scribbler};
pub use watchdog::{ProcessState, WatchdogService as Watchdog, WorkerReport};

// ── Manager ────────────────────────────────────────────────────────────

/// Top-level orchestrator for uv-managed Python worker scripts.
///
/// `uv` must be available on `PATH` unless [`Manager::with_uv_path`] selects
/// an explicit executable.
///
/// ```ignore
/// let mut manager = Manager::new("path/to/scripts");
/// manager.on_message(|env| { /* global handler */ });
///
/// let mut worker = manager.worker("my_script");
/// worker.env("KEY", "VALUE");
/// worker.on_message(|env| { /* per-worker handler */ });
/// worker.spawn().await.unwrap();
/// ```
pub struct Manager {
    integrity: Arc<IntegrityChecker>,
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    socket_dir: PathBuf,
    global_handler: Option<MessageHandler>,

    /// Watchdog service — use `manager.dog.report().await` for health reports.
    pub dog: WatchdogService,
}

impl Manager {
    /// Create a manager using `uv` from `PATH`.
    ///
    /// `scripts_path` is the directory containing PEP 723 `.py` worker
    /// scripts. Construction performs a non-fatal integrity check; spawning a
    /// worker repeats it and returns any uv or scripts-directory error.
    pub fn new(scripts_path: &str) -> Self {
        Self::with_uv_path(scripts_path, "uv")
    }

    /// Create a manager using an explicit `uv` executable path.
    ///
    /// Use this when uv is packaged outside `PATH`. Runtime behavior is
    /// otherwise identical to [`Manager::new`].
    pub fn with_uv_path(scripts_path: &str, uv_path: &str) -> Self {
        let integrity = Arc::new(IntegrityChecker::new(scripts_path, uv_path));

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
            self.integrity.clone(),
            &self.socket_dir,
            self.global_handler.clone(),
            self.workers.clone(),
        )
    }

    /// Register a **global** message handler that fires for every message from
    /// every worker, *before* worker-specific handlers.
    pub fn on_message<F>(&mut self, handler: F)
    where
        F: Fn(InboundEnvelope) + Send + Sync + 'static,
    {
        self.global_handler = Some(Arc::new(handler));
    }

    /// Re-run the full integrity check (uv, scripts dir, script index).
    pub fn check_integrity(&self) -> Result<(), String> {
        self.integrity.perform_check()
    }

    /// Broadcast an envelope to all active workers.
    /// Returns a map of worker_id -> Result indicating success or failure for each.
    pub async fn broadcast(&self, envelope: Envelope) -> HashMap<String, Result<(), String>> {
        let workers = self.workers.read().await;
        let mut results = HashMap::new();

        for (worker_id, handle) in workers.iter() {
            let result = handle.sender.send(envelope.clone()).await;
            results.insert(worker_id.clone(), result);
        }

        results
    }

    /// Terminate all active workers gracefully.
    /// Sends a reserved termination envelope, waits briefly, then force-kills survivors.
    pub async fn terminate_all(&mut self) {
        let _ = self.broadcast(Envelope::terminate()).await;

        // Give workers time to shut down cleanly
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Force-kill any remaining
        let mut workers = self.workers.write().await;
        for (id, mut handle) in workers.drain() {
            force_stop_worker(&mut handle);
            let _ = std::fs::remove_file(&handle.sock_path);
            scribbler::scribbler().info_with(
                "Manager",
                &format!("Terminated worker: {} ({})", handle.identity.name, id),
            );
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
                    force_stop_worker(&mut handle);
                    let _ = std::fs::remove_file(&handle.sock_path);
                    scribbler::scribbler().info_with(
                        "Manager",
                        &format!("Terminated worker: {} ({})", handle.identity.name, id),
                    );
                }
            }
            Err(_) => {
                scribbler::scribbler()
                    .warning_with("Manager", "Could not acquire worker lock during shutdown");
            }
        }

        scribbler::scribbler().success("All workers terminated.");
    }
}
