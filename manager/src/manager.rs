use chrono::Local;
use rand::{Rng, distributions::Alphanumeric};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use tokio::net::UnixListener;

use crate::integrity::IntegrityChecker;
use crate::protocol::{ControlPlane, Envelope, MessageHandler};

// ── Worker Identity ────────────────────────────────────────────────────

/// Unique identity assigned to one spawned worker process.
///
/// Names contain the script stem, a minute-resolution timestamp, and a
/// four-character random suffix. Socket filenames prefix that name with
/// `rp_` and append `.sock`.
#[derive(Debug, Clone)]
pub struct WorkerIdentity {
    /// Trusted identifier stamped into protocol messages and output records.
    pub name: String,
    /// Filename used beneath the Manager's socket directory.
    pub sock_file: String,
}

impl WorkerIdentity {
    /// Generate a fresh identity for `script`.
    ///
    /// ```
    /// use runpy::WorkerIdentity;
    ///
    /// let identity = WorkerIdentity::new("scraper");
    /// assert!(identity.name.starts_with("scraper_"));
    /// assert_eq!(identity.sock_file, format!("rp_{}.sock", identity.name));
    /// ```
    pub fn new(script: &str) -> Self {
        let ts = Local::now().format("%d%m%Y-%H%M").to_string();
        let rnd: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(4)
            .map(char::from)
            .collect();
        let name = format!("{}_{}_{}", script, ts, rnd);

        Self {
            sock_file: format!("rp_{}.sock", name),
            name,
        }
    }
}

// ── Worker (user-facing) ───────────────────────────────────────────────

/// Builder and remote handle for one Python worker.
///
/// Configure environment, arguments, and a per-worker handler before
/// [`Worker::spawn`]. A successful spawn stores the generated identity so the
/// same value can call [`Worker::send_message`] and [`Worker::terminate`].
/// The facade uses weak references to Manager-owned services and returns an
/// error after its Manager is dropped.
///
/// ```no_run
/// use runpy::{Envelope, Manager};
///
/// # #[tokio::main]
/// # async fn main() {
/// let manager = Manager::new("worker");
/// let mut worker = manager.worker("my_worker");
/// worker
///     .env("MODE", "fast")
///     .arg("batch", "10")
///     .on_message(|inbound| println!("{:?}", inbound.envelope));
///
/// worker.spawn().await.expect("worker starts");
/// worker.send_message(Envelope::retry()).await.expect("message sent");
/// worker.terminate().await.expect("worker stopped");
/// # }
/// ```
pub struct Worker {
    script: String,
    integrity: Weak<IntegrityChecker>,
    control_plane: Weak<ControlPlane>,
    socket_dir: PathBuf,
    env_vars: HashMap<String, String>,
    extra_args: HashMap<String, String>,
    worker_handler: Option<MessageHandler>,
    worker_id: Option<String>,
}

impl Worker {
    /// Create a Worker facade backed by Manager-owned services.
    pub(crate) fn new(
        script: &str,
        integrity: Weak<IntegrityChecker>,
        control_plane: Weak<ControlPlane>,
        socket_dir: &Path,
    ) -> Self {
        Self {
            script: script.to_string(),
            integrity,
            control_plane,
            socket_dir: socket_dir.to_path_buf(),
            env_vars: HashMap::new(),
            extra_args: HashMap::new(),
            worker_handler: None,
            worker_id: None,
        }
    }

    /// Set or replace an environment variable passed to `uv` and Python.
    ///
    /// Runpy applies `PYTHONUNBUFFERED=1` after builder values, so that one key
    /// cannot be overridden.
    ///
    /// ```no_run
    /// # use runpy::Manager;
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.env("API_BASE", "https://example.com");
    /// ```
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.env_vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Set or replace one extra `--key=value` worker argument.
    ///
    /// Python exposes recognized arguments through `Worker.extra`.
    ///
    /// ```no_run
    /// # use runpy::Manager;
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.arg("mode", "fast");
    /// ```
    pub fn arg(&mut self, key: &str, value: &str) -> &mut Self {
        self.extra_args.insert(key.to_string(), value.to_string());
        self
    }

    /// Merge multiple extra worker arguments.
    ///
    /// Values in `args` replace existing values for matching keys.
    ///
    /// ```no_run
    /// use std::collections::HashMap;
    /// # use runpy::Manager;
    ///
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.args(HashMap::from([
    ///     ("mode".to_string(), "fast".to_string()),
    ///     ("batch".to_string(), "10".to_string()),
    /// ]));
    /// ```
    pub fn args(&mut self, args: HashMap<String, String>) -> &mut Self {
        self.extra_args.extend(args);
        self
    }

    /// Replace the per-worker inbound-envelope handler.
    ///
    /// For this worker, the current Manager-global handler runs first and this
    /// handler runs second. The handler captured at spawn executes
    /// synchronously in the protocol task and should return promptly.
    ///
    /// ```no_run
    /// # use runpy::Manager;
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.on_message(|inbound| {
    ///     println!("{:?}", inbound.envelope.data());
    /// });
    /// ```
    pub fn on_message<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(crate::protocol::InboundEnvelope) + Send + Sync + 'static,
    {
        self.worker_handler = Some(Arc::new(handler));
        self
    }

    /// Start the configured script and register it with the Manager.
    ///
    /// This re-runs integrity checks, requires a root-level `<script>.py`,
    /// creates a unique socket, and launches `uv run --no-project [--locked]
    /// --script`. The returned string is the trusted worker ID. Registration
    /// owns the child process group, protocol session, and output readers.
    ///
    /// Errors include a dropped Manager, failed integrity/path validation,
    /// missing script, socket/spawn/pipe registration failure, shutdown in
    /// progress, and attempting to spawn the same facade twice. Post-spawn
    /// registration failures clean up the child process group and socket.
    ///
    /// ```no_run
    /// # use runpy::Manager;
    /// # #[tokio::main]
    /// # async fn main() {
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// let worker_id = worker.spawn().await.expect("worker starts");
    /// println!("{worker_id}");
    /// # }
    /// ```
    pub async fn spawn(&mut self) -> Result<String, String> {
        if self.worker_id.is_some() {
            return Err("Worker has already been spawned".to_string());
        }

        let integrity = self
            .integrity
            .upgrade()
            .ok_or_else(|| "Worker manager is no longer available".to_string())?;
        let control_plane = self
            .control_plane
            .upgrade()
            .ok_or_else(|| "Worker manager is no longer available".to_string())?;

        integrity.perform_check()?;
        let scripts_dir = integrity.scripts_dir.canonicalize().map_err(|error| {
            format!(
                "Failed to resolve scripts directory '{}': {}",
                integrity.scripts_dir.display(),
                error
            )
        })?;
        let script_file = scripts_dir.join(format!("{}.py", self.script));
        if !script_file.is_file() {
            return Err(format!(
                "Worker script does not exist: '{}'",
                script_file.display()
            ));
        }

        let uv_path =
            if integrity.uv_path.is_absolute() || integrity.uv_path.components().count() == 1 {
                integrity.uv_path.clone()
            } else {
                integrity.uv_path.canonicalize().map_err(|error| {
                    format!(
                        "Failed to resolve uv executable '{}': {}",
                        integrity.uv_path.display(),
                        error
                    )
                })?
            };
        let lock_file = script_file.with_extension("py.lock");

        let identity = WorkerIdentity::new(&self.script);
        let worker_id = identity.name.clone();
        let sock_path = self.socket_dir.join(&identity.sock_file);
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).map_err(|error| {
            format!(
                "Failed to bind socket at '{}': {error}",
                sock_path.display()
            )
        })?;
        let socket_path = sock_path.to_str().map(str::to_owned).ok_or_else(|| {
            let _ = std::fs::remove_file(&sock_path);
            format!("Socket path is not valid UTF-8: {}", sock_path.display())
        })?;

        control_plane.logger().debug_with(
            "Worker",
            &format!(
                "Starting '{}' with socket at '{}'",
                identity.name,
                sock_path.display()
            ),
        );

        // Keep std::process::Child as the ownership boundary: Watchdog polls it
        // synchronously while Tokio adapts only the detached output pipes.
        let mut cmd = std::process::Command::new(&uv_path);
        cmd.arg("run").arg("--no-project");
        if lock_file.is_file() {
            cmd.arg("--locked");
        }
        cmd.arg("--script")
            .arg(&script_file)
            .arg(&sock_path)
            .arg(&identity.name);
        for (key, value) in &self.extra_args {
            cmd.arg(format!("--{key}={value}"));
        }
        if let Some(parent) = scripts_dir.parent() {
            cmd.current_dir(parent);
            cmd.env("PYTHONPATH", parent);
        }
        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }
        cmd.env("PYTHONUNBUFFERED", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A separate process group lets every uv/Python descendant be stopped
        // together; piped streams are drained independently by ControlPlane.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn().map_err(|error| {
            let _ = std::fs::remove_file(&sock_path);
            format!(
                "Failed to start worker with uv '{}': {error}",
                integrity.uv_path.display()
            )
        })?;

        control_plane
            .register_worker(
                child,
                identity,
                sock_path,
                socket_path,
                listener,
                self.worker_handler.clone(),
            )
            .await?;
        self.worker_id = Some(worker_id.clone());
        control_plane
            .logger()
            .success(&format!("Spawned worker: {worker_id}"));
        Ok(worker_id)
    }

    /// Send an envelope to this facade's registered worker.
    ///
    /// Calling before a successful spawn, after Manager drop, or after worker
    /// removal returns an error.
    ///
    /// ```no_run
    /// use runpy::{Data, Envelope, Manager};
    /// use serde_json::{json, Value};
    ///
    /// # fn object(value: Value) -> Data {
    /// #     value.as_object().cloned().unwrap()
    /// # }
    /// # #[tokio::main]
    /// # async fn main() {
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.spawn().await.unwrap();
    /// worker
    ///     .send_message(Envelope::execute(object(json!({"task": "parse"}))))
    ///     .await
    ///     .unwrap();
    /// # }
    /// ```
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), String> {
        let worker_id = self
            .worker_id
            .as_deref()
            .ok_or_else(|| "Worker has not been spawned yet".to_string())?;
        let control_plane = self
            .control_plane
            .upgrade()
            .ok_or_else(|| "Worker manager is no longer available".to_string())?;
        control_plane.send(worker_id, envelope).await
    }

    /// Request graceful termination, then force process-group cleanup.
    ///
    /// On a successful send the control plane waits two seconds before
    /// removal. Cleanup still runs when sending fails; the send error is then
    /// returned. Calling before spawn or after Manager drop returns an error.
    ///
    /// ```no_run
    /// # use runpy::Manager;
    /// # #[tokio::main]
    /// # async fn main() {
    /// let manager = Manager::new("worker");
    /// let mut worker = manager.worker("my_worker");
    /// worker.spawn().await.unwrap();
    /// worker.terminate().await.unwrap();
    /// # }
    /// ```
    pub async fn terminate(&self) -> Result<(), String> {
        let worker_id = self
            .worker_id
            .as_deref()
            .ok_or_else(|| "Worker has not been spawned yet".to_string())?;
        let control_plane = self
            .control_plane
            .upgrade()
            .ok_or_else(|| "Worker manager is no longer available".to_string())?;
        control_plane.terminate_worker(worker_id).await
    }
}
