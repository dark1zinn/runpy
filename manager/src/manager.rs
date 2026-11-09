use rand::random;
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use tokio::net::UnixListener;

use crate::error::{InvalidScriptReason, RunpyError};
use crate::integrity::IntegrityChecker;
use crate::protocol::{ControlPlane, Envelope, InboundEnvelope, MessageHandler, SendReceipt};

// ── Worker Identity ────────────────────────────────────────────────────

/// Unique identity assigned to one spawned worker process.
///
/// The name is the script stem followed by a 128-bit random suffix, so it is
/// unique across Managers and restarts. The socket filename is a short,
/// independent random name, so long script names cannot push the socket
/// path past the operating system's limit.
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
    /// assert!(identity.name.starts_with("scraper-"));
    /// assert!(identity.sock_file.ends_with(".sock"));
    /// ```
    pub fn new(script: &str) -> Self {
        Self {
            name: format!("{script}-{:032x}", random::<u128>()),
            sock_file: format!("w{:016x}.sock", random::<u64>()),
        }
    }
}

/// Attempts to find an unused socket name before giving up.
const SOCKET_BIND_ATTEMPTS: usize = 8;

/// Bind a listener at a fresh path in `dir`, never removing an existing file.
///
/// A path that already exists is someone else's: Runpy regenerates the
/// identity instead of unlinking it, up to [`SOCKET_BIND_ATTEMPTS`] times.
pub(crate) fn bind_unique(
    dir: &Path,
    mut identity: impl FnMut() -> WorkerIdentity,
) -> Result<(WorkerIdentity, PathBuf, UnixListener), RunpyError> {
    for _ in 0..SOCKET_BIND_ATTEMPTS {
        let candidate = identity();
        let path = dir.join(&candidate.sock_file);
        match UnixListener::bind(&path) {
            Ok(listener) => return Ok((candidate, path, listener)),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(RunpyError::Io(error)),
        }
    }
    Err(RunpyError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no unused socket name after {SOCKET_BIND_ATTEMPTS} attempts"),
    )))
}

// ── Worker configuration ───────────────────────────────────────────────

/// Standard input given to a worker process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StdinMode {
    /// Connect stdin to the null device (default), so a worker can never
    /// block on or consume the host's input.
    #[default]
    Null,
    /// Inherit the Manager process's stdin.
    Inherit,
}

/// Immutable configuration for launching one worker script.
///
/// Created by [`crate::Manager::worker`]. Setters consume and return the
/// builder; [`WorkerBuilder::spawn`] consumes it and returns a running
/// [`Worker`]. A builder is cheap to clone, so one template can launch many
/// workers.
///
/// ```no_run
/// use runpy::Manager;
///
/// # #[tokio::main]
/// # async fn main() -> Result<(), runpy::RunpyError> {
/// let manager = Manager::builder("worker").build().await?;
/// let worker = manager
///     .worker("my_worker")
///     .env("MODE", "fast")
///     .arg("batch", "10")
///     .on_message(|inbound| println!("{:?}", inbound.envelope))
///     .spawn()
///     .await?;
/// worker.terminate().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct WorkerBuilder {
    script: String,
    integrity: Weak<IntegrityChecker>,
    control_plane: Weak<ControlPlane>,
    socket_dir: PathBuf,
    env_vars: HashMap<String, String>,
    extra_args: HashMap<String, String>,
    cwd: Option<PathBuf>,
    stdin: StdinMode,
    worker_handler: Option<MessageHandler>,
}

impl WorkerBuilder {
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
            cwd: None,
            stdin: StdinMode::default(),
            worker_handler: None,
        }
    }

    /// Set or replace an environment variable passed to `uv` and Python.
    ///
    /// Runpy applies its own `PYTHONUNBUFFERED=1` and frame-limit variables
    /// after builder values, so those keys cannot be overridden.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env_vars.insert(key.to_string(), value.to_string());
        self
    }

    /// Set or replace one extra `--key=value` worker argument.
    ///
    /// Python exposes recognized arguments through `Worker.extra`.
    pub fn arg(mut self, key: &str, value: &str) -> Self {
        self.extra_args.insert(key.to_string(), value.to_string());
        self
    }

    /// Merge multiple extra worker arguments.
    ///
    /// Values in `args` replace existing values for matching keys.
    pub fn args(mut self, args: HashMap<String, String>) -> Self {
        self.extra_args.extend(args);
        self
    }

    /// Run the worker in `dir` instead of the scripts directory's parent.
    ///
    /// `PYTHONPATH` still points at the scripts directory's parent unless an
    /// [`env`](Self::env) value overrides it.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Choose the worker's standard input; the default is [`StdinMode::Null`].
    pub fn stdin(mut self, mode: StdinMode) -> Self {
        self.stdin = mode;
        self
    }

    /// Set the per-worker inbound-envelope handler.
    ///
    /// For this worker, the current Manager-global handler runs first and this
    /// handler second. It executes synchronously in the protocol task and
    /// should return promptly; a panic is caught and logged.
    pub fn on_message<F>(mut self, handler: F) -> Self
    where
        F: Fn(InboundEnvelope) + Send + Sync + 'static,
    {
        self.worker_handler = Some(Arc::new(handler));
        self
    }

    /// Launch the configured script and register it with the Manager.
    ///
    /// The script name must be a single path component naming a root-level
    /// `<script>.py`. Runpy creates a unique socket and launches
    /// `uv run --no-project [--locked] --script`. Registration owns the child
    /// process group, protocol session, and output readers; failures after
    /// the process starts kill and reap its group and remove the socket.
    ///
    /// # Errors
    ///
    /// [`RunpyError::ManagerClosed`] after Manager drop or during shutdown,
    /// [`RunpyError::InvalidScript`] for a bad or missing script,
    /// [`RunpyError::ScriptsDirectory`] or [`RunpyError::UvUnavailable`] when
    /// the runtime check fails, and [`RunpyError::Io`] when the socket,
    /// process, or pipes cannot be set up.
    pub async fn spawn(self) -> Result<Worker, RunpyError> {
        let script_name = Path::new(&self.script);
        if self.script.is_empty() || script_name.file_name() != Some(script_name.as_os_str()) {
            return Err(RunpyError::InvalidScript {
                script: self.script,
                reason: InvalidScriptReason::NotSingleComponent,
            });
        }

        let integrity = self.integrity.upgrade().ok_or(RunpyError::ManagerClosed)?;
        let control_plane = self
            .control_plane
            .upgrade()
            .ok_or(RunpyError::ManagerClosed)?;

        integrity.perform_check()?;
        let scripts_dir = integrity.scripts_dir.canonicalize().map_err(|source| {
            RunpyError::ScriptsDirectory {
                path: integrity.scripts_dir.clone(),
                source,
            }
        })?;
        let script_file = scripts_dir.join(format!("{}.py", self.script));
        if !script_file.is_file() {
            return Err(RunpyError::InvalidScript {
                script: self.script,
                reason: InvalidScriptReason::NotFound(script_file),
            });
        }

        let uv_path =
            if integrity.uv_path.is_absolute() || integrity.uv_path.components().count() == 1 {
                integrity.uv_path.clone()
            } else {
                integrity
                    .uv_path
                    .canonicalize()
                    .map_err(|error| RunpyError::UvUnavailable {
                        path: integrity.uv_path.clone(),
                        detail: format!("cannot resolve path: {error}"),
                    })?
            };
        let lock_file = script_file.with_extension("py.lock");

        let (identity, sock_path, listener) =
            bind_unique(&self.socket_dir, || WorkerIdentity::new(&self.script))?;
        let worker_id = identity.name.clone();
        // The Manager verified at build that its socket directory is UTF-8.
        let socket_path = sock_path.to_string_lossy().into_owned();

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
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }
        // Runpy-owned limits are applied after user variables so the worker
        // always enforces exactly what the Manager enforces.
        for (key, value) in control_plane.limits().worker_env() {
            cmd.env(key, value);
        }
        cmd.env("PYTHONUNBUFFERED", "1")
            .stdin(match self.stdin {
                StdinMode::Null => Stdio::null(),
                StdinMode::Inherit => Stdio::inherit(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A separate process group lets every uv/Python descendant be stopped
        // together; piped streams are drained independently by ControlPlane.
        #[cfg(unix)]
        cmd.process_group(0);

        let child = cmd.spawn().map_err(|error| {
            let _ = std::fs::remove_file(&sock_path);
            RunpyError::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "failed to start worker with uv '{}': {error}",
                    integrity.uv_path.display()
                ),
            ))
        })?;

        control_plane
            .register_worker(
                child,
                identity,
                sock_path,
                socket_path,
                listener,
                self.worker_handler,
            )
            .await?;
        control_plane
            .logger()
            .success(&format!("Spawned worker: {worker_id}"));
        Ok(Worker {
            id: worker_id,
            control_plane: self.control_plane,
        })
    }
}

// ── Running worker handle ──────────────────────────────────────────────

/// Handle to one running worker process.
///
/// Returned by [`WorkerBuilder::spawn`]. Cloning is cheap; every clone
/// addresses the same process. The Manager owns the process: after the
/// Manager is dropped, operations return [`RunpyError::ManagerClosed`], and
/// after the worker is removed they return [`RunpyError::WorkerExited`].
#[derive(Clone, Debug)]
pub struct Worker {
    id: String,
    control_plane: Weak<ControlPlane>,
}

impl Worker {
    /// Trusted Manager-generated worker identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Send an envelope and wait until it is completely written.
    ///
    /// Waiting for outbound capacity and the socket write share the
    /// Manager's send deadline. Success means the frame reached the socket,
    /// not that the worker processed it.
    ///
    /// ```no_run
    /// use runpy::{Data, Envelope, Manager, Meta};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), runpy::RunpyError> {
    /// let manager = Manager::builder("worker").build().await?;
    /// let worker = manager.worker("my_worker").spawn().await?;
    /// worker
    ///     .send_message(Envelope::new(Meta::new(), Data::new())?)
    ///     .await?;
    /// worker.terminate().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_message(&self, envelope: Envelope) -> Result<(), RunpyError> {
        self.control_plane()?.send(&self.id, envelope).await
    }

    /// Admit an envelope without waiting, or fail with
    /// [`RunpyError::QueueFull`].
    ///
    /// The returned [`SendReceipt`] resolves when the frame is written or
    /// fails. Dropping the receipt does not cancel the send.
    pub fn try_send_message(&self, envelope: Envelope) -> Result<SendReceipt, RunpyError> {
        self.control_plane()?.try_send(&self.id, envelope)
    }

    /// Request graceful termination, then force process-group cleanup.
    ///
    /// New sends to this worker are refused at once. The worker is removed
    /// when its process exits, and at the latest when the Manager's shutdown
    /// grace ends, even if `terminate` could not be written. Cleanup always
    /// runs; the result reports whether `terminate` was written.
    pub async fn terminate(&self) -> Result<(), RunpyError> {
        self.control_plane()?.terminate_worker(&self.id).await
    }

    fn control_plane(&self) -> Result<Arc<ControlPlane>, RunpyError> {
        self.control_plane
            .upgrade()
            .ok_or(RunpyError::ManagerClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(sock_file: &str) -> WorkerIdentity {
        WorkerIdentity {
            name: format!("worker-{sock_file}"),
            sock_file: sock_file.to_string(),
        }
    }

    #[tokio::test]
    async fn existing_paths_are_skipped_not_unlinked() {
        let dir = tempfile::TempDir::new().unwrap();
        let taken = dir.path().join("taken.sock");
        std::fs::write(&taken, "someone else's file").unwrap();

        let mut names = ["taken.sock", "fresh.sock"].into_iter();
        let (bound, path, _listener) =
            bind_unique(dir.path(), || identity(names.next().unwrap())).unwrap();

        assert_eq!(bound.sock_file, "fresh.sock");
        assert_eq!(path, dir.path().join("fresh.sock"));
        assert_eq!(
            std::fs::read_to_string(&taken).unwrap(),
            "someone else's file"
        );
    }

    #[tokio::test]
    async fn binding_gives_up_after_bounded_attempts() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("taken.sock"), "").unwrap();
        let mut attempts = 0;
        let result = bind_unique(dir.path(), || {
            attempts += 1;
            identity("taken.sock")
        });

        assert!(matches!(
            result,
            Err(RunpyError::Io(ref error)) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(attempts, SOCKET_BIND_ATTEMPTS);
    }

    #[test]
    fn identities_are_random_and_socket_names_are_short() {
        let long_script = "s".repeat(200);
        let first = WorkerIdentity::new(&long_script);
        let second = WorkerIdentity::new(&long_script);
        assert_ne!(first.name, second.name);
        assert_ne!(first.sock_file, second.sock_file);
        assert!(first.name.starts_with(&format!("{long_script}-")));
        assert_eq!(first.sock_file.len(), "w0123456789abcdef.sock".len());
    }
}
