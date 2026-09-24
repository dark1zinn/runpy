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
//! ## Service ownership
//!
//! [`Manager`] is the sole composition root. It owns the integrity checker,
//! logger, and one shared control plane. The control plane owns the watchdog,
//! internal mailer, and all running workers. [`Worker`] values are lightweight
//! facades and cannot outlive the Manager-owned services.
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
//!         inbound.reply(Envelope::execute(object(json!({"task": "process"}))));
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
mod scribbler;
mod watchdog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::integrity::IntegrityChecker;
use crate::protocol::ControlPlane;

// ── Public re-exports ──────────────────────────────────────────────────
pub use manager::{Worker, WorkerIdentity};
pub use protocol::{Data, Envelope, EnvelopeError, InboundEnvelope, MessageHandler, Meta};
pub use scribbler::{LogLevel, Scribbler};
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
    logger: Arc<Scribbler>,
    control_plane: Arc<ControlPlane>,
    socket_dir: PathBuf,
}

impl Manager {
    /// Create a manager using `uv` from `PATH`.
    pub fn new(scripts_path: &str) -> Self {
        Self::with_uv_path(scripts_path, "uv")
    }

    /// Create a manager using an explicit `uv` executable path.
    pub fn with_uv_path(scripts_path: &str, uv_path: &str) -> Self {
        let logger = Arc::new(Scribbler::new());
        let integrity = Arc::new(IntegrityChecker::new(scripts_path, uv_path, logger.clone()));
        if let Err(error) = integrity.perform_check() {
            logger.error_with("Manager", &format!("Integrity check failed: {error}"));
        }

        let control_plane = ControlPlane::new(logger.clone());
        control_plane.start_monitoring(5);

        Self {
            integrity,
            logger,
            control_plane,
            socket_dir: PathBuf::from("/tmp/runpy"),
        }
    }

    /// Create a Worker facade for a script name without the `.py` suffix.
    pub fn worker(&self, script: &str) -> Worker {
        Worker::new(
            script,
            Arc::downgrade(&self.integrity),
            Arc::downgrade(&self.control_plane),
            &self.socket_dir,
        )
    }

    /// Return the Manager-owned logger shared by every Runpy service.
    pub fn logger(&self) -> Arc<Scribbler> {
        self.logger.clone()
    }

    /// Return the single ControlPlane-owned watchdog.
    pub fn watchdog(&self) -> &Watchdog {
        self.control_plane.watchdog()
    }

    /// Register the live Manager handler dispatched before worker handlers.
    pub fn on_message<F>(&mut self, handler: F)
    where
        F: Fn(InboundEnvelope) + Send + Sync + 'static,
    {
        self.control_plane.set_global_handler(Arc::new(handler));
    }

    /// Re-run the full integrity check.
    pub fn check_integrity(&self) -> Result<(), String> {
        self.integrity.perform_check()
    }

    /// Broadcast an envelope to all registered workers.
    pub async fn broadcast(&self, envelope: Envelope) -> HashMap<String, Result<(), String>> {
        self.control_plane.broadcast(envelope).await
    }

    /// Gracefully request termination, then force-stop remaining workers.
    pub async fn terminate_all(&mut self) {
        self.control_plane.terminate_all().await;
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.logger
            .info_with("Manager", "Shutting down all workers...");
        self.control_plane.shutdown_now();
        self.logger.success("All workers terminated.");
    }
}
