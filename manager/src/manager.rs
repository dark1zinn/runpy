use chrono::Local;
use rand::{Rng, distributions::Alphanumeric};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::net::UnixListener;

use crate::integrity::IntegrityChecker;
use crate::protocol::{ControlPlane, Envelope, MessageHandler};

// ── Worker Identity ────────────────────────────────────────────────────

/// A unique identity for each spawned worker, composed of the script name,
/// a timestamp, and a short random suffix.
#[derive(Debug, Clone)]
pub struct WorkerIdentity {
    pub name: String,
    pub sock_file: String,
}

impl WorkerIdentity {
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

/// The user-facing worker object returned by `Manager::worker()`.
/// It acts as a **builder** before `spawn()` is called, and as a
/// **remote handle** after spawning (for sending messages, terminating, etc.).
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

    /// Set an environment variable that will be passed to the Python process.
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.env_vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Add an extra `--key=value` argument for the Python process.
    pub fn arg(&mut self, key: &str, value: &str) -> &mut Self {
        self.extra_args.insert(key.to_string(), value.to_string());
        self
    }

    /// Add multiple extra arguments at once.
    pub fn args(&mut self, args: HashMap<String, String>) -> &mut Self {
        self.extra_args.extend(args);
        self
    }

    /// Register a per-worker handler that runs after the Manager handler.
    pub fn on_message<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(crate::protocol::InboundEnvelope) + Send + Sync + 'static,
    {
        self.worker_handler = Some(Arc::new(handler));
        self
    }

    /// Spawn the Python worker and register it with the Manager control plane.
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

    /// Send an [`Envelope`] to the spawned worker.
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

    /// Request graceful termination, then force-stop the worker process group.
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
