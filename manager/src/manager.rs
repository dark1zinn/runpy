use chrono::Local;
use rand::{distributions::Alphanumeric, Rng};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::RwLock;

use crate::protocol::{ControlPlane, Envelope, Message, MessageHandler, MessageSender};
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
}

// ── Worker (user-facing) ───────────────────────────────────────────────

/// The user-facing worker object returned by `Manager::worker()`.
/// It acts as a **builder** before `spawn()` is called, and as a
/// **remote handle** after spawning (for sending messages, terminating, etc.).
pub struct Worker {
    // ── Builder fields (set before spawn) ───────────────────────────
    script: String,
    venv_path: PathBuf,
    scripts_dir: PathBuf,
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
        venv_path: &PathBuf,
        scripts_dir: &PathBuf,
        socket_dir: &PathBuf,
        global_handler: Option<MessageHandler>,
        workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    ) -> Self {
        Self {
            script: script.to_string(),
            venv_path: venv_path.clone(),
            scripts_dir: scripts_dir.clone(),
            socket_dir: socket_dir.clone(),
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
        F: Fn(Envelope) + Send + Sync + 'static,
    {
        self.worker_handler = Some(Arc::new(handler));
        self
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    /// Spawn the Python worker process and start its control-plane listener.
    /// Returns the unique worker ID on success.
    pub async fn spawn(&mut self) -> Result<String, String> {
        let identity = WorkerIdentity::new(&self.script);
        let sock_path = self.socket_dir.join(&identity.sock_file);

        // Ensure clean socket start
        let _ = std::fs::remove_file(&sock_path);

        let listener = UnixListener::bind(&sock_path)
            .map_err(|e| format!("Failed to bind socket at '{}': {}", sock_path.display(), e))?;

        println!(
            "Starting worker '{}' with socket at '{}'",
            identity.name,
            sock_path.display()
        );

        // Start the control plane (runs in background, returns a MessageSender)
        let plane = ControlPlane::new(
            listener,
            identity.name.clone(),
            self.global_handler.clone(),
            self.worker_handler.clone(),
        );
        let sender = plane.start();

        // Resolve the Python executable
        let py_executable = if cfg!(windows) {
            self.venv_path.join("Scripts/python.exe")
        } else {
            self.venv_path.join("bin/python")
        };

        // Resolve the script path
        let script_file = self.scripts_dir.join(format!("{}.py", self.script));

        let mut cmd = std::process::Command::new(&py_executable);
        cmd.arg(&script_file)
            .arg(&sock_path)
            .arg(&identity.name); // Pass worker name as third argument

        // Pass extra arguments as --key=value format
        for (key, value) in &self.extra_args {
            cmd.arg(format!("--{}={}", key, value));
        }

        // Set working directory *and* PYTHONPATH to the parent of the scripts
        // directory so that sibling Python packages (e.g. `bridge`) are
        // importable via `from bridge.worker import ...`.
        // Python sets sys.path[0] to the script's own directory, so we must
        // also inject the parent into PYTHONPATH.
        if let Some(parent) = self.scripts_dir.parent() {
            cmd.current_dir(parent);
            cmd.env("PYTHONPATH", parent);
        }

        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("Failed to start worker process: {}", e))?;

        let name = identity.name.clone();

        let handle = WorkerHandle {
            child,
            identity,
            sock_path: sock_path.clone(),
            sender: sender.clone(),
        };

        // Store in the shared map
        self.workers.write().await.insert(name.clone(), handle);

        // Keep references for post-spawn methods
        self.worker_id = Some(name.clone());
        self.sender = Some(sender);

        println!("Spawned worker: {}", name);
        Ok(name)
    }

    /// Send a `Message` to the running worker via its control-plane channel.
    pub async fn send_message(&self, msg: Message) -> Result<(), String> {
        match &self.sender {
            Some(sender) => sender.send(msg).await,
            None => Err("Worker has not been spawned yet".to_string()),
        }
    }

    /// Request graceful termination: send a TERMINATE message, then wait briefly
    /// before force-killing the process.
    pub async fn terminate(&self) -> Result<(), String> {
        self.send_message(Message::terminate()).await?;

        // Give the worker a moment to shut down cleanly
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Force-kill if still running
        if let Some(ref wid) = self.worker_id {
            let mut workers = self.workers.write().await;
            if let Some(mut handle) = workers.remove(wid) {
                let _ = handle.child.kill();
                let _ = std::fs::remove_file(&handle.sock_path);
            }
        }

        Ok(())
    }
}
