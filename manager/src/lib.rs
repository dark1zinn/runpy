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
//!     "x_spath": "/tmp/runpy-Xk3p9Q/w8f3c2a1b0e9d7c6a.sock",
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
//! use runpy::{Data, Manager};
//! use serde_json::json;
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), runpy::RunpyError> {
//!     let manager = Manager::builder("worker").build().await?;
//!
//!     // `spawn` returns once the worker reported `ready`.
//!     let worker = manager.worker("my_worker").spawn().await?;
//!     println!("spawned {}", worker.id());
//!
//!     // Each request is correlated with its own result.
//!     let input = Data::from_iter([("task".into(), json!("process"))]);
//!     let output = worker.execute(input, Duration::from_secs(30)).await?;
//!     println!("{output:?}");
//!
//!     let exit = worker.terminate().await?;
//!     println!("stopped: {:?}", exit.reason);
//!     Ok(())
//! }
//! ```
//!
//! Dropping [`Manager`] synchronously force-stops remaining process groups and
//! removes their socket files.

mod config;
mod error;
mod integrity;
mod lifecycle;
mod manager;
mod protocol;
mod request;
mod scribbler;
mod watchdog;

use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{ProtocolLimits, QueueLimits};
use crate::integrity::IntegrityChecker;
use crate::protocol::ControlPlane;

// ── Public re-exports ──────────────────────────────────────────────────
pub use config::{
    DEFAULT_FRAME_TIMEOUT, DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_PENDING_REQUESTS,
    DEFAULT_MAX_WORKERS, DEFAULT_OUTBOUND_CAPACITY, DEFAULT_QUEUED_BYTES, DEFAULT_SEND_TIMEOUT,
    DEFAULT_SHUTDOWN_GRACE, DEFAULT_STARTUP_TIMEOUT,
};
pub use error::{InvalidScriptReason, RunpyError, TimeoutStage};
pub use lifecycle::{ExitReason, WorkerExit};
pub use manager::{StdinMode, Worker, WorkerBuilder, WorkerIdentity};
pub use protocol::{
    Data, Envelope, EnvelopeError, InboundEnvelope, MessageHandler, Meta, SendReceipt,
    WorkerOutput, WorkerOutputHandler, WorkerOutputStream,
};
pub use request::{RemoteError, Request, RequestId};
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
    socket_parent: Option<PathBuf>,
    max_frame_bytes: usize,
    frame_timeout: Duration,
    startup_timeout: Duration,
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

    /// Directory in which the Manager creates its private socket directory;
    /// defaults to the platform temporary directory.
    ///
    /// Runpy always creates a fresh `runpy-*` child with mode `0700` and
    /// removes it when the Manager drops; it never adopts or deletes the
    /// given directory itself. Keep the path short: Unix socket paths are
    /// limited to about 100 bytes.
    pub fn socket_parent(mut self, dir: impl Into<PathBuf>) -> Self {
        self.socket_parent = Some(dir.into());
        self
    }

    /// Deadline for starting up: the `uv --version` check; default
    /// [`DEFAULT_STARTUP_TIMEOUT`] (30 seconds). A `uv` that does not exit in
    /// time is killed and reported as [`RunpyError::UvUnavailable`].
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
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

    /// Manager-wide bound on submitted requests whose result has not been
    /// consumed or dropped; default [`DEFAULT_MAX_PENDING_REQUESTS`] (64).
    /// Submissions beyond it wait, within their deadline, for a slot.
    pub fn max_pending_requests(mut self, requests: usize) -> Self {
        self.queue.max_pending_requests = requests;
        self
    }

    /// Manager-wide bound on worker processes across all scripts; default
    /// [`DEFAULT_MAX_WORKERS`] (3). A worker counts from before `uv` starts
    /// until its process group has been stopped and reaped, so starting and
    /// stopping workers occupy a slot too. [`WorkerBuilder::spawn`] waits for
    /// a slot within the startup timeout, then fails with
    /// `Timeout(Admission)`. The count is of managed workers; each one may
    /// show several OS processes (`uv` plus Python and its descendants).
    pub fn max_workers(mut self, workers: usize) -> Self {
        self.queue.max_workers = workers;
        self
    }

    fn validate(&self) -> Result<ProtocolLimits, RunpyError> {
        let limits = ProtocolLimits::new(self.max_frame_bytes, self.frame_timeout)
            .map_err(RunpyError::InvalidConfig)?;
        let queue = &self.queue;
        if queue.outbound_capacity == 0 || queue.max_pending_requests == 0 || queue.max_workers == 0
        {
            return Err(RunpyError::InvalidConfig(
                "outbound_capacity, max_pending_requests, and max_workers must be greater than zero"
                    .to_string(),
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
            ("startup_timeout", self.startup_timeout),
        ] {
            if value.is_zero() {
                return Err(RunpyError::InvalidConfig(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        Ok(limits)
    }

    /// Validate the configuration, check `uv` and the scripts directory,
    /// create the private socket directory, and start the Manager's services.
    ///
    /// # Errors
    ///
    /// [`RunpyError::InvalidConfig`] for an invalid value, when called outside
    /// a Tokio runtime, or when the socket directory would be unsafe, non-UTF-8,
    /// or too long; [`RunpyError::UvUnavailable`] when `uv` cannot run;
    /// [`RunpyError::ScriptsDirectory`] when the scripts directory is missing
    /// or not a directory; [`RunpyError::Io`] when the socket directory
    /// cannot be created.
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
            self.startup_timeout,
        ));
        integrity.check().await?;
        let socket_dir = create_socket_dir(self.socket_parent.as_deref())?;

        let control_plane = ControlPlane::new(logger.clone(), limits, self.queue);
        control_plane.start_monitoring(5);

        Ok(Manager {
            integrity,
            logger,
            control_plane,
            socket_dir,
        })
    }
}

/// Longest socket path, excluding the terminating NUL, every supported
/// platform accepts (`sun_path` is 104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 103;

/// Create the Manager-owned socket directory and verify it is private.
///
/// The directory is created atomically with an unpredictable name and mode
/// `0700` (the umask can only remove bits). The check afterwards guards
/// against an unexpected filesystem and rejects a directory not owned by this
/// process's effective user.
fn create_socket_dir(parent: Option<&Path>) -> Result<tempfile::TempDir, RunpyError> {
    let mut builder = tempfile::Builder::new();
    builder
        .prefix("runpy-")
        .permissions(std::fs::Permissions::from_mode(0o700));
    let dir = match parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
    .map_err(RunpyError::Io)?;

    let metadata = std::fs::symlink_metadata(dir.path()).map_err(RunpyError::Io)?;
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.is_dir()
        || metadata.uid() != effective_uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(RunpyError::InvalidConfig(format!(
            "socket directory '{}' is not a private directory owned by this user",
            dir.path().display()
        )));
    }

    let Some(path) = dir.path().to_str() else {
        return Err(RunpyError::InvalidConfig(format!(
            "socket directory '{}' is not valid UTF-8",
            dir.path().display()
        )));
    };
    // `/` plus a socket name such as `w0123456789abcdef.sock`.
    let longest = path.len() + 1 + "w0123456789abcdef.sock".len();
    if longest > MAX_SOCKET_PATH {
        return Err(RunpyError::InvalidConfig(format!(
            "socket directory '{path}' is too long: socket paths would reach {longest} \
             bytes, above the {MAX_SOCKET_PATH}-byte limit; choose a shorter socket_parent"
        )));
    }
    Ok(dir)
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
    /// Removed when the Manager drops, after every worker has been stopped.
    socket_dir: tempfile::TempDir,
}

impl Manager {
    /// Start configuring a Manager for the directory containing
    /// `<worker-name>.py` scripts.
    pub fn builder(scripts: impl Into<PathBuf>) -> ManagerBuilder {
        ManagerBuilder {
            scripts: scripts.into(),
            uv_path: PathBuf::from("uv"),
            socket_parent: None,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            frame_timeout: DEFAULT_FRAME_TIMEOUT,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            queue: QueueLimits::default(),
        }
    }

    /// Private directory holding this Manager's worker sockets.
    pub fn socket_dir(&self) -> &Path {
        self.socket_dir.path()
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
            self.socket_dir.path(),
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
        self.integrity.check().await
    }

    /// Send one envelope to every currently registered worker.
    ///
    /// The returned map is keyed by trusted worker ID and contains each
    /// worker's write outcome. An empty registry returns an empty map.
    pub async fn broadcast(&self, envelope: Envelope) -> HashMap<String, Result<(), RunpyError>> {
        self.control_plane.broadcast(envelope).await
    }

    /// Request graceful termination for all workers, force cleanup, and
    /// return each worker's exit keyed by worker ID.
    ///
    /// New sends are refused at once. `terminate` requests are delivered
    /// concurrently, and every process group is killed and reaped as soon as
    /// it exits or at the latest when the shutdown grace ends, even when a
    /// worker's queue is saturated. Sockets are removed and output readers
    /// drain briefly.
    pub async fn terminate_all(&self) -> HashMap<String, WorkerExit> {
        self.control_plane.terminate_all().await
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
