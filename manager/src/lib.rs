//! # Runpy
//!
//! Runpy is a Rust control plane for `uv`-managed Python worker scripts. It
//! owns process groups, Unix sockets, message routing, health inspection, and
//! attributed stdout/stderr capture. The companion `runpyrs` Python package
//! supplies the worker receive loop.
//!
//! ## Runtime model
//!
//! [`Manager`] is the composition root. It validates `uv` and a scripts
//! directory, creates lightweight [`Worker`] facades, and owns every running
//! worker until explicit termination or Manager drop. Async spawn, messaging,
//! reports, and graceful termination require a Tokio runtime.
//!
//! Each worker is a PEP 723 script launched as:
//!
//! ```text
//! uv run --no-project [--locked] --script <script.py> <socket-path> <worker-id>
//! ```
//!
//! An adjacent `<script.py>.lock` enables `--locked`. Runpy does not create a
//! project virtual environment, run `uv sync`, inject Python dependencies, or
//! mutate lockfiles.
//!
//! ## Envelopes
//!
//! Socket frames contain an 8-byte little-endian length followed by a JSON
//! [`Envelope`] with exactly two object fields:
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
//! Applications own [`Data`] and non-`x_` [`Meta`]. Runpy reserves the entire
//! `x_` namespace and stamps trusted worker/socket values at transport edges.
//! Incoming messages reach the current Manager handler before the worker's
//! per-worker handler. Both callbacks run synchronously in the protocol task
//! and should return promptly.
//!
//! ## Worker process output
//!
//! The control plane drains worker stdout and stderr independently and emits
//! attributed records through [`Scribbler`] and
//! [`Manager::on_worker_output`]. This best-effort channel is separate from
//! structured socket messages. The observer runs on a dedicated dispatcher
//! thread; Runpy catches and logs observer panics and never derives worker
//! lifecycle policy from output text.
//!
//! ## Complete lifecycle
//!
//! ```no_run
//! use runpy::{Data, Envelope, Manager};
//! use serde_json::{json, Value};
//! use std::time::Duration;
//!
//! fn object(value: Value) -> Data {
//!     value.as_object().cloned().expect("JSON object")
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut manager = Manager::new("worker");
//!     manager.check_integrity().expect("valid runtime");
//!     let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
//!     manager.on_message(move |inbound| {
//!         match inbound.envelope.meta().get("x_op").and_then(Value::as_str) {
//!             Some("ready") => inbound.reply(Envelope::execute(object(
//!                 json!({"task": "process"}),
//!             ))),
//!             Some("done" | "error") => {
//!                 println!("{:?}", inbound.envelope.data());
//!                 let _ = finished_tx.send(());
//!             }
//!             _ => {}
//!         }
//!     });
//!
//!     let mut worker = manager.worker("my_worker");
//!     let worker_id = worker.spawn().await.expect("worker starts");
//!     println!("spawned {worker_id}");
//!     tokio::time::timeout(Duration::from_secs(30), finished_rx.recv())
//!         .await
//!         .expect("worker response timed out")
//!         .expect("worker response channel closed");
//!     worker.terminate().await.expect("worker terminates");
//! }
//! ```
//!
//! Dropping [`Manager`] synchronously force-stops remaining process groups and
//! removes their socket files.

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
pub use protocol::{
    Data, Envelope, EnvelopeError, InboundEnvelope, MessageHandler, Meta, WorkerOutput,
    WorkerOutputHandler, WorkerOutputStream,
};
pub use scribbler::{LogLevel, Scribbler};
pub use watchdog::{ProcessState, WatchdogService as Watchdog, WorkerReport};

// ── Manager ────────────────────────────────────────────────────────────

/// Top-level owner of Runpy services and managed worker processes.
///
/// Construction performs an integrity check and logs any failure; use
/// [`Manager::check_integrity`] when the caller needs a returned error. A
/// Manager can be created outside a Tokio runtime, but asynchronous operations
/// and worker registration require one.
///
/// ```no_run
/// use runpy::Manager;
///
/// let manager = Manager::new("worker");
/// let logger = manager.logger();
/// logger.info("manager ready");
/// ```
pub struct Manager {
    integrity: Arc<IntegrityChecker>,
    logger: Arc<Scribbler>,
    control_plane: Arc<ControlPlane>,
    socket_dir: PathBuf,
}

impl Manager {
    /// Create a Manager that resolves `uv` from `PATH`.
    ///
    /// `scripts_path` is the directory containing `<worker-name>.py` files.
    /// Integrity failures are logged and construction still succeeds.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// let manager = Manager::new("worker");
    /// manager.check_integrity().expect("uv and scripts are available");
    /// ```
    pub fn new(scripts_path: &str) -> Self {
        Self::with_uv_path(scripts_path, "uv")
    }

    /// Create a Manager with an explicit `uv` executable.
    ///
    /// Relative multi-component paths are canonicalized when a worker spawns;
    /// a single component such as `uv` is resolved by the operating system.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// let manager = Manager::with_uv_path("worker", "/opt/runpy/bin/uv");
    /// manager.check_integrity().expect("configured runtime is valid");
    /// ```
    pub fn with_uv_path(scripts_path: &str, uv_path: &str) -> Self {
        let logger = Arc::new(Scribbler::new());
        let integrity = Arc::new(IntegrityChecker::new(scripts_path, uv_path, logger.clone()));
        if let Err(error) = integrity.perform_check() {
            logger.error_with("Manager", &format!("Integrity check failed: {error}"));
        }

        let control_plane = ControlPlane::new(logger.clone());
        if tokio::runtime::Handle::try_current().is_ok() {
            control_plane.start_monitoring(5);
        }

        Self {
            integrity,
            logger,
            control_plane,
            socket_dir: PathBuf::from("/tmp/runpy"),
        }
    }

    /// Create a worker facade for a root-level script name without `.py`.
    ///
    /// The facade is a builder before `spawn` and a remote handle afterward.
    /// It cannot outlive the services owned by this Manager.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.env("MODE", "fast").arg("batch", "10");
    /// ```
    pub fn worker(&self, script: &str) -> Worker {
        Worker::new(
            script,
            Arc::downgrade(&self.integrity),
            Arc::downgrade(&self.control_plane),
            &self.socket_dir,
        )
    }

    /// Return the shared Manager-owned logger.
    ///
    /// Every call clones the same [`Arc`], so configured severity and
    /// environment settings are shared by Manager services and callers.
    pub fn logger(&self) -> Arc<Scribbler> {
        self.logger.clone()
    }

    /// Borrow the shared watchdog for one-shot process reports.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let manager = Manager::new("worker");
    /// for report in manager.watchdog().report().await {
    ///     println!("{}: {:?}", report.worker_name, report.state);
    /// }
    /// # }
    /// ```
    pub fn watchdog(&self) -> &Watchdog {
        self.control_plane.watchdog()
    }

    /// Replace the global inbound-envelope handler.
    ///
    /// The current global handler runs before a worker-specific handler for
    /// every valid worker message. It executes synchronously in the protocol
    /// task and should return promptly.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// let mut manager = Manager::new("worker");
    /// manager.on_message(|inbound| {
    ///     println!("{:?}", inbound.envelope);
    /// });
    /// ```
    pub fn on_message<F>(&mut self, handler: F)
    where
        F: Fn(InboundEnvelope) + Send + Sync + 'static,
    {
        self.control_plane.set_global_handler(Arc::new(handler));
    }

    /// Replace the observer for attributed worker stdout/stderr records.
    ///
    /// The observer runs on Runpy's dedicated output-dispatcher thread and
    /// should return promptly. A panic is caught and logged so later records
    /// continue. Output is best effort and never drives automatic termination.
    ///
    /// ```no_run
    /// use runpy::{Manager, WorkerOutputStream};
    ///
    /// let mut manager = Manager::new("worker");
    /// manager.on_worker_output(|output| {
    ///     if output.stream == WorkerOutputStream::Stderr {
    ///         eprintln!("{}: {}", output.worker_id, output.line);
    ///     }
    /// });
    /// ```
    pub fn on_worker_output<F>(&mut self, handler: F)
    where
        F: Fn(WorkerOutput) + Send + Sync + 'static,
    {
        self.control_plane
            .set_worker_output_handler(Arc::new(handler));
    }

    /// Re-run uv validation, socket-directory creation, and script indexing.
    ///
    /// Unlike Manager construction, this method returns validation failures.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// let manager = Manager::new("worker");
    /// manager.check_integrity().expect("runtime is ready");
    /// ```
    pub fn check_integrity(&self) -> Result<(), String> {
        self.integrity.perform_check()
    }

    /// Send one envelope to every currently registered worker.
    ///
    /// The returned map is keyed by trusted worker ID and contains the result
    /// for each bounded outbound route. An empty registry returns an empty map.
    ///
    /// ```no_run
    /// use runpy::{Envelope, Manager};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let manager = Manager::new("worker");
    /// let results = manager.broadcast(Envelope::retry()).await;
    /// for (worker_id, result) in results {
    ///     println!("{worker_id}: {result:?}");
    /// }
    /// # }
    /// ```
    pub async fn broadcast(&self, envelope: Envelope) -> HashMap<String, Result<(), String>> {
        self.control_plane.broadcast(envelope).await
    }

    /// Request graceful termination for all workers, then force cleanup.
    ///
    /// The control plane broadcasts `terminate`, waits two seconds, kills and
    /// reaps remaining process groups, removes sockets, and briefly drains
    /// output readers.
    ///
    /// ```no_run
    /// use runpy::Manager;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let mut manager = Manager::new("worker");
    /// manager.terminate_all().await;
    /// # }
    /// ```
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
