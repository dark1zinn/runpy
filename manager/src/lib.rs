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
//! and should return promptly. A handler panic is caught and logged without
//! ending the session or skipping the other handler.
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
//! async fn main() -> Result<(), runpy::RunpyError> {
//!     let manager = Manager::builder("worker").build().await?;
//!     let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
//!     manager.on_message(move |inbound| {
//!         match inbound.envelope.meta().get("x_op").and_then(Value::as_str) {
//!             Some("ready") => {
//!                 let execute = Envelope::execute(object(json!({"task": "process"})));
//!                 if let Err(error) = inbound.reply(execute) {
//!                     eprintln!("execute not admitted: {error}");
//!                 }
//!             }
//!             Some("done" | "error") => {
//!                 println!("{:?}", inbound.envelope.data());
//!                 let _ = finished_tx.send(());
//!             }
//!             _ => {}
//!         }
//!     });
//!
//!     let worker = manager.worker("my_worker").spawn().await?;
//!     println!("spawned {}", worker.id());
//!     tokio::time::timeout(Duration::from_secs(30), finished_rx.recv())
//!         .await
//!         .expect("worker response timed out")
//!         .expect("worker response channel closed");
//!     worker.terminate().await?;
//!     manager.terminate_all().await;
//!     Ok(())
//! }
//! ```
//!
//! Dropping [`Manager`] synchronously force-stops remaining process groups and
//! removes their socket files.

mod config;
mod error;
mod integrity;
mod manager;
mod protocol;
mod scribbler;
mod watchdog;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{ProtocolLimits, QueueLimits};
use crate::integrity::IntegrityChecker;
use crate::protocol::ControlPlane;

// ── Public re-exports ──────────────────────────────────────────────────
pub use config::{
    DEFAULT_FRAME_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, DEFAULT_OUTBOUND_CAPACITY,
    DEFAULT_QUEUED_BYTES, DEFAULT_SEND_TIMEOUT, DEFAULT_SHUTDOWN_GRACE,
};
pub use error::{InvalidScriptReason, RunpyError, TimeoutStage};
pub use manager::{StdinMode, Worker, WorkerBuilder, WorkerIdentity};
pub use protocol::{
    Data, Envelope, EnvelopeError, InboundEnvelope, MessageHandler, Meta, SendReceipt,
    WorkerOutput, WorkerOutputHandler, WorkerOutputStream,
};
pub use scribbler::{LogLevel, Scribbler};
pub use watchdog::{ProcessState, WatchdogService as Watchdog, WorkerReport};

// ── Manager builder ────────────────────────────────────────────────────

/// Validated configuration for a [`Manager`].
///
/// Every setter consumes and returns the builder. [`ManagerBuilder::build`]
/// validates all values before creating any resource.
///
/// ```no_run
/// use runpy::Manager;
/// use std::time::Duration;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), runpy::RunpyError> {
/// let manager = Manager::builder("worker")
///     .uv_path("/opt/runpy/bin/uv")
///     .max_frame_bytes(1024 * 1024)
///     .send_timeout(Duration::from_secs(2))
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
#[must_use = "a builder does nothing until `build` is awaited"]
pub struct ManagerBuilder {
    scripts: PathBuf,
    uv_path: PathBuf,
    max_frame_bytes: usize,
    frame_timeout: Duration,
    queue: QueueLimits,
}

impl ManagerBuilder {
    /// Executable used to launch workers; defaults to `uv` from `PATH`.
    ///
    /// Relative multi-component paths are canonicalized when a worker spawns;
    /// a single component such as `uv` is resolved by the operating system.
    pub fn uv_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.uv_path = path.into();
        self
    }

    /// Maximum JSON body per frame in both directions; default
    /// [`DEFAULT_MAX_FRAME_BYTES`] (8 MiB).
    pub fn max_frame_bytes(mut self, bytes: usize) -> Self {
        self.max_frame_bytes = bytes;
        self
    }

    /// Time allowed to finish a frame after its first byte; default
    /// [`DEFAULT_FRAME_TIMEOUT`] (30 seconds).
    pub fn frame_timeout(mut self, timeout: Duration) -> Self {
        self.frame_timeout = timeout;
        self
    }

    /// Admitted frames per worker waiting for the writer; default
    /// [`DEFAULT_OUTBOUND_CAPACITY`] (64).
    pub fn outbound_capacity(mut self, frames: usize) -> Self {
        self.queue.outbound_capacity = frames;
        self
    }

    /// Manager-wide bound on encoded outbound bytes awaiting a write; default
    /// [`DEFAULT_QUEUED_BYTES`] (16 MiB). Must hold at least one maximum frame.
    pub fn queued_bytes(mut self, bytes: usize) -> Self {
        self.queue.queued_bytes = bytes;
        self
    }

    /// Deadline covering outbound admission and the socket write; default
    /// [`DEFAULT_SEND_TIMEOUT`] (5 seconds).
    pub fn send_timeout(mut self, timeout: Duration) -> Self {
        self.queue.send_timeout = timeout;
        self
    }

    /// Time a worker gets to exit after termination starts; default
    /// [`DEFAULT_SHUTDOWN_GRACE`] (2 seconds).
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.queue.shutdown_grace = grace;
        self
    }

    fn validate(&self) -> Result<ProtocolLimits, RunpyError> {
        let limits = ProtocolLimits::new(self.max_frame_bytes, self.frame_timeout)
            .map_err(RunpyError::InvalidConfig)?;
        let queue = &self.queue;
        if queue.outbound_capacity == 0 {
            return Err(RunpyError::InvalidConfig(
                "outbound_capacity must be greater than zero".to_string(),
            ));
        }
        if queue.queued_bytes < limits.max_frame_bytes() + 8 {
            return Err(RunpyError::InvalidConfig(format!(
                "queued_bytes ({}) must hold at least one maximum frame ({} bytes)",
                queue.queued_bytes,
                limits.max_frame_bytes() + 8
            )));
        }
        for (name, value) in [
            ("send_timeout", queue.send_timeout),
            ("shutdown_grace", queue.shutdown_grace),
        ] {
            if value.is_zero() {
                return Err(RunpyError::InvalidConfig(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        Ok(limits)
    }

    /// Validate the configuration, check `uv` and the scripts directory, and
    /// start the Manager's services.
    ///
    /// # Errors
    ///
    /// [`RunpyError::InvalidConfig`] for an invalid value or when called
    /// outside a Tokio runtime, [`RunpyError::UvUnavailable`] when `uv`
    /// cannot run, and [`RunpyError::ScriptsDirectory`] when the scripts
    /// directory is missing or not a directory.
    pub async fn build(self) -> Result<Manager, RunpyError> {
        let limits = self.validate()?;
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(RunpyError::InvalidConfig(
                "Manager::build must run inside a Tokio runtime".to_string(),
            ));
        }

        let logger = Arc::new(Scribbler::new());
        let integrity = Arc::new(IntegrityChecker::new(
            self.scripts,
            self.uv_path,
            logger.clone(),
        ));
        integrity.perform_check()?;

        let control_plane = ControlPlane::new(logger.clone(), limits, self.queue);
        control_plane.start_monitoring(5);

        Ok(Manager {
            integrity,
            logger,
            control_plane,
            socket_dir: PathBuf::from("/tmp/runpy"),
        })
    }
}

// ── Manager ────────────────────────────────────────────────────────────

/// Top-level owner of Runpy services and managed worker processes.
///
/// Create one with [`Manager::builder`]. The Manager owns every worker it
/// spawns until explicit termination or Manager drop; [`Worker`] handles stop
/// working once it is gone.
///
/// ```no_run
/// use runpy::Manager;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), runpy::RunpyError> {
/// let manager = Manager::builder("worker").build().await?;
/// manager.logger().info("manager ready");
/// # Ok(())
/// # }
/// ```
pub struct Manager {
    integrity: Arc<IntegrityChecker>,
    logger: Arc<Scribbler>,
    control_plane: Arc<ControlPlane>,
    socket_dir: PathBuf,
}

impl Manager {
    /// Start configuring a Manager for the directory containing
    /// `<worker-name>.py` scripts.
    pub fn builder(scripts: impl Into<PathBuf>) -> ManagerBuilder {
        ManagerBuilder {
            scripts: scripts.into(),
            uv_path: PathBuf::from("uv"),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            frame_timeout: DEFAULT_FRAME_TIMEOUT,
            queue: QueueLimits::default(),
        }
    }

    /// Start configuring a worker for a root-level script name without `.py`.
    ///
    /// The returned [`WorkerBuilder`] cannot outlive the services owned by
    /// this Manager.
    pub fn worker(&self, script: &str) -> WorkerBuilder {
        WorkerBuilder::new(
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
    /// # async fn main() -> Result<(), runpy::RunpyError> {
    /// let manager = Manager::builder("worker").build().await?;
    /// for report in manager.watchdog().report().await {
    ///     println!("{}: {:?}", report.worker_name, report.state);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn watchdog(&self) -> &Watchdog {
        self.control_plane.watchdog()
    }

    /// Replace the global inbound-envelope handler.
    ///
    /// The current global handler runs before a worker-specific handler for
    /// every valid worker message. It executes synchronously in the protocol
    /// task and should return promptly. A panic is caught and logged; the
    /// worker handler and later messages still run.
    pub fn on_message<F>(&self, handler: F)
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
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), runpy::RunpyError> {
    /// let manager = Manager::builder("worker").build().await?;
    /// manager.on_worker_output(|output| {
    ///     if output.stream == WorkerOutputStream::Stderr {
    ///         eprintln!("{}: {}", output.worker_id, output.line);
    ///     }
    /// });
    /// # Ok(())
    /// # }
    /// ```
    pub fn on_worker_output<F>(&self, handler: F)
    where
        F: Fn(WorkerOutput) + Send + Sync + 'static,
    {
        self.control_plane
            .set_worker_output_handler(Arc::new(handler));
    }

    /// Re-run the `uv` and scripts-directory checks performed by `build`.
    pub async fn check_integrity(&self) -> Result<(), RunpyError> {
        self.integrity.perform_check()
    }

    /// Send one envelope to every currently registered worker.
    ///
    /// The returned map is keyed by trusted worker ID and contains each
    /// worker's write outcome. An empty registry returns an empty map.
    pub async fn broadcast(&self, envelope: Envelope) -> HashMap<String, Result<(), RunpyError>> {
        self.control_plane.broadcast(envelope).await
    }

    /// Request graceful termination for all workers, then force cleanup.
    ///
    /// New sends are refused at once. `terminate` requests are delivered
    /// concurrently, and every process group is killed and reaped as soon as
    /// it exits or at the latest when the shutdown grace ends, even when a
    /// worker's queue is saturated. Sockets are removed and output readers
    /// drain briefly.
    pub async fn terminate_all(&self) {
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
