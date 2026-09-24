use chrono::Local;
use rand::{Rng, distributions::Alphanumeric};
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use crate::integrity::IntegrityChecker;
use crate::protocol::{ControlPlane, Envelope, MessageHandler, MessageSender};
use crate::scribbler::scribbler;
use crate::watchdog::WatchdogService;

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

// ── Worker Handle ──────────────────────────────────────────────────────

/// A handle to a running worker process. Stored in the shared `workers` map
/// and exposed to user code for sending messages and querying health.
pub struct WorkerHandle {
    pub child: Child,
    pub identity: WorkerIdentity,
    pub sock_path: PathBuf,
    pub sender: MessageSender,
    pub process_group_id: u32,
}

pub(crate) fn force_stop_worker(handle: &mut WorkerHandle) {
    #[cfg(unix)]
    {
        let result =
            unsafe { libc::kill(-(handle.process_group_id as libc::pid_t), libc::SIGKILL) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                let _ = handle.child.kill();
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = handle.child.kill();
    }

    let _ = handle.child.wait();
}

// ── Worker (user-facing) ───────────────────────────────────────────────

/// The user-facing worker object returned by `Manager::worker()`.
/// It acts as a **builder** before `spawn()` is called, and as a
/// **remote handle** after spawning (for sending messages, terminating, etc.).
pub struct Worker {
    // ── Builder fields (set before spawn) ───────────────────────────
    script: String,
    integrity: Arc<IntegrityChecker>,
    socket_dir: PathBuf,
    env_vars: HashMap<String, String>,
    extra_args: HashMap<String, String>,
    worker_handler: Option<MessageHandler>,
    global_handler: Option<MessageHandler>,

    // ── Handle fields (populated after spawn) ───────────────────────
    worker_id: Option<String>,
    sender: Option<MessageSender>,

    // ── Shared references ───────────────────────────────────────────
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,

    /// Per-worker watchdog view (reads from the same shared map).
    pub dog: WatchdogService,
}

impl Worker {
    pub(crate) fn new(
        script: &str,
        integrity: Arc<IntegrityChecker>,
        socket_dir: &Path,
        global_handler: Option<MessageHandler>,
        workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    ) -> Self {
        Self {
            script: script.to_string(),
            integrity,
            socket_dir: socket_dir.to_path_buf(),
            env_vars: HashMap::new(),
            extra_args: HashMap::new(),
            worker_handler: None,
            global_handler,
            worker_id: None,
            sender: None,
            dog: WatchdogService::new(workers.clone()),
            workers,
        }
    }

    // ── Builder methods ────────────────────────────────────────────

    /// Set an environment variable that will be passed to the Python process.
    pub fn env(&mut self, key: &str, value: &str) -> &mut Self {
        self.env_vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Add an extra argument that will be passed to the Python process.
    /// Arguments are passed as `--key=value` format after the worker name.
    ///
    /// Example:
    /// ```ignore
    /// worker.arg("db", "postgres").arg("mode", "lazy");
    /// // Results in: python script.py <socket> <name> --db=postgres --mode=lazy
    /// ```
    pub fn arg(&mut self, key: &str, value: &str) -> &mut Self {
        self.extra_args.insert(key.to_string(), value.to_string());
        self
    }

    /// Add multiple extra arguments at once from a HashMap.
    pub fn args(&mut self, args: HashMap<String, String>) -> &mut Self {
        self.extra_args.extend(args);
        self
    }

    /// Register a per-worker message handler. This fires **after** the global
    /// handler (if any).
    pub fn on_message<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(crate::protocol::InboundEnvelope) + Send + Sync + 'static,
    {
        self.worker_handler = Some(Arc::new(handler));
        self
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    /// Spawn the Python worker process and start its control-plane listener.
    /// Returns the unique worker ID on success.
    pub async fn spawn(&mut self) -> Result<String, String> {
        self.integrity.perform_check()?;

        let scripts_dir = self.integrity.scripts_dir.canonicalize().map_err(|error| {
            format!(
                "Failed to resolve scripts directory '{}': {}",
                self.integrity.scripts_dir.display(),
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

        let uv_path = if self.integrity.uv_path.is_absolute()
            || self.integrity.uv_path.components().count() == 1
        {
            self.integrity.uv_path.clone()
        } else {
            self.integrity.uv_path.canonicalize().map_err(|error| {
                format!(
                    "Failed to resolve uv executable '{}': {}",
                    self.integrity.uv_path.display(),
                    error
                )
            })?
        };
        let lock_file = script_file.with_extension("py.lock");

        let identity = WorkerIdentity::new(&self.script);
        let sock_path = self.socket_dir.join(&identity.sock_file);

        // Ensure clean socket start
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path)
            .map_err(|e| format!("Failed to bind socket at '{}': {}", sock_path.display(), e))?;

        let socket_path = match sock_path.to_str() {
            Some(path) => path.to_string(),
            None => {
                let _ = std::fs::remove_file(&sock_path);
                return Err(format!(
                    "Socket path is not valid UTF-8: {}",
                    sock_path.display()
                ));
            }
        };

        scribbler().debug_with(
            "Worker",
            &format!(
                "Starting '{}' with socket at '{}'",
                identity.name,
                sock_path.display()
            ),
        );

        // Start the control plane (runs in background, returns a MessageSender)
        let plane = ControlPlane::new(
            listener,
            identity.name.clone(),
            socket_path,
            self.global_handler.clone(),
            self.worker_handler.clone(),
        );
        let sender = plane.start();

        let mut cmd = std::process::Command::new(&uv_path);
        cmd.arg("run").arg("--no-project");
        if lock_file.is_file() {
            cmd.arg("--locked");
        }
        cmd.arg("--script")
            .arg(&script_file)
            .arg(&sock_path)
            .arg(&identity.name);

        // Pass extra arguments as --key=value format
        for (key, value) in &self.extra_args {
            cmd.arg(format!("--{}={}", key, value));
        }

        // Set working directory *and* PYTHONPATH to the parent of the scripts
        // directory so that sibling Python packages (e.g. `bridge`) are
        // importable via `from bridge.worker import ...`.
        // Python sets sys.path[0] to the script's own directory, so we must
        // also inject the parent into PYTHONPATH.
        if let Some(parent) = scripts_dir.parent() {
            cmd.current_dir(parent);
            cmd.env("PYTHONPATH", parent);
        }

        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }

        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn().map_err(|error| {
            let _ = std::fs::remove_file(&sock_path);
            format!(
                "Failed to start worker with uv '{}': {}",
                self.integrity.uv_path.display(),
                error
            )
        })?;
        let process_group_id = child.id();

        let name = identity.name.clone();

        let handle = WorkerHandle {
            child,
            identity,
            sock_path: sock_path.clone(),
            sender: sender.clone(),
            process_group_id,
        };

        // Store in the shared map
        self.workers.write().await.insert(name.clone(), handle);

        // Keep references for post-spawn methods
        self.worker_id = Some(name.clone());
        self.sender = Some(sender);

        scribbler().success(&format!("Spawned worker: {}", name));
        Ok(name)
    }

    /// Send an [`Envelope`] to the running worker.
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), String> {
        match &self.sender {
            Some(sender) => sender.send(envelope).await,
            None => Err("Worker has not been spawned yet".to_string()),
        }
    }

    /// Request graceful termination, then force-kill the process if necessary.
    pub async fn terminate(&self) -> Result<(), String> {
        let send_result = self.send_message(Envelope::terminate()).await;

        if send_result.is_ok() {
            // Give the worker a moment to shut down cleanly
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }

        // Force-kill if still running
        if let Some(wid) = &self.worker_id {
            let mut workers = self.workers.write().await;
            if let Some(mut handle) = workers.remove(wid) {
                force_stop_worker(&mut handle);
                let _ = std::fs::remove_file(&handle.sock_path);
            }
        }

        send_result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn terminate_cleans_up_worker_when_message_delivery_fails() {
        let workers = Arc::new(RwLock::new(HashMap::new()));
        let integrity = Arc::new(IntegrityChecker::new(".", "uv"));
        let mut worker = Worker::new(
            "managed",
            integrity,
            &PathBuf::from("/tmp/runpy"),
            None,
            workers.clone(),
        );
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let sender = MessageSender::for_testing(tx);

        let mut command = std::process::Command::new("sleep");
        command.arg("300").process_group(0);
        let child = command.spawn().unwrap();
        let pid = child.id();
        let worker_id = format!("failed-delivery-{pid}");
        let sock_path = PathBuf::from(format!("/tmp/runpy/rp_{worker_id}.sock"));
        std::fs::create_dir_all("/tmp/runpy").unwrap();
        std::fs::write(&sock_path, []).unwrap();

        worker.worker_id = Some(worker_id.clone());
        worker.sender = Some(sender.clone());
        workers.write().await.insert(
            worker_id.clone(),
            WorkerHandle {
                child,
                identity: WorkerIdentity {
                    name: worker_id.clone(),
                    sock_file: sock_path.file_name().unwrap().to_str().unwrap().to_string(),
                },
                sock_path: sock_path.clone(),
                sender,
                process_group_id: pid,
            },
        );

        let started = Instant::now();
        let result = worker.terminate().await;
        let elapsed = started.elapsed();
        let worker_removed = !workers.read().await.contains_key(&worker_id);
        let process_stopped = unsafe { libc::kill(pid as libc::pid_t, 0) } != 0;
        let socket_removed = !sock_path.exists();

        if let Some(mut handle) = workers.write().await.remove(&worker_id) {
            force_stop_worker(&mut handle);
            let _ = std::fs::remove_file(&handle.sock_path);
        }

        assert_eq!(
            result,
            Err("Failed to send envelope to worker: channel closed".to_string())
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "failed delivery waited for graceful shutdown"
        );
        assert!(worker_removed);
        assert!(process_stopped);
        assert!(socket_removed);
    }
}
