use rand::random;
use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Weak};
use tokio::net::UnixListener;

use crate::error::{InvalidScriptReason, RunpyError, TimeoutStage};
use crate::integrity::IntegrityChecker;
use crate::lifecycle::{ExitReason, LifecycleReceiver, LifecycleState, WorkerExit, wait_for_exit};
use crate::protocol::{ControlPlane, Data, Envelope, InboundEnvelope, MessageHandler, SendReceipt};
use crate::request::Request;
use std::time::Duration;

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

    /// Whether this builder was created by the Manager owning `control_plane`.
    pub(crate) fn belongs_to(&self, control_plane: &Arc<ControlPlane>) -> bool {
        std::ptr::eq(self.control_plane.as_ptr(), Arc::as_ptr(control_plane))
    }

    /// Launch the configured script and register it with the Manager.
    ///
    /// The script name must be a single path component naming a root-level
    /// `<script>.py`, checked on every spawn so scripts added or removed after
    /// the Manager was built are seen. Runpy creates a unique socket and
    /// launches `uv run --no-project [--locked] --script`. Registration owns
    /// the child process group, protocol session, and output readers;
    /// failures after the process starts kill and reap its group and remove
    /// the socket. Filesystem checks run off the async runtime threads.
    ///
    /// # Errors
    ///
    /// [`RunpyError::ManagerClosed`] after Manager drop or during shutdown,
    /// [`RunpyError::InvalidScript`] for a bad or missing script,
    /// [`RunpyError::ScriptsDirectory`] or [`RunpyError::UvUnavailable`] when
    /// a path cannot be resolved, [`RunpyError::Timeout`] with
    /// [`TimeoutStage::Admission`] when no worker slot frees up within the
    /// startup timeout, and [`RunpyError::Io`] when the socket, process, or
    /// pipes cannot be set up.
    pub async fn spawn(self) -> Result<Worker, RunpyError> {
        let script_name = Path::new(&self.script);
        if self.script.is_empty() || script_name.file_name() != Some(script_name.as_os_str()) {
            return Err(RunpyError::InvalidScript {
                script: self.script,
                reason: InvalidScriptReason::NotSingleComponent,
            });
        }

        let integrity = self.integrity.upgrade().ok_or(RunpyError::ManagerClosed)?;
        let resolved = integrity.resolve_script(&self.script).await?;
        let uv_path = integrity.resolve_uv().await?;
        let control_plane = self
            .control_plane
            .upgrade()
            .ok_or(RunpyError::ManagerClosed)?;

        // Admission and readiness share one startup deadline. The slot is
        // reserved before `uv` starts and travels with the child, so it is
        // released only after that process group is stopped and reaped (or
        // here, if launching fails).
        let deadline = tokio::time::Instant::now() + integrity.startup_timeout;
        let slot = control_plane.reserve_process(deadline).await?;

        // No await between binding the socket and registering the child, so
        // cancelling `spawn` can never leak a socket or an unowned process.
        let (identity, sock_path, listener) =
            bind_unique(&self.socket_dir, || WorkerIdentity::new(&self.script))?;
        let worker_id = identity.name.clone();

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
        if resolved.locked {
            cmd.arg("--locked");
        }
        cmd.arg("--script")
            .arg(&resolved.script_file)
            .arg(&sock_path)
            .arg(&identity.name);
        for (key, value) in &self.extra_args {
            cmd.arg(format!("--{key}={value}"));
        }
        if let Some(root) = resolved.root() {
            cmd.current_dir(root);
            cmd.env("PYTHONPATH", root);
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

        let lifecycle = control_plane
            .register_worker(
                child,
                identity,
                sock_path,
                listener,
                self.worker_handler,
                slot,
            )
            .await?;

        // Until `ready` arrives, abandoning this future must not leave a
        // registered process nobody can reach: the guard hands it to the
        // monitor for removal.
        let mut guard = StartupGuard {
            control_plane: &control_plane,
            worker_id: &worker_id,
            armed: true,
        };
        // Wait for `ready`, but notice at once if the process exits first
        // instead of waiting for the next watchdog tick.
        let mut observer = lifecycle.clone();
        let ready = tokio::select! {
            state = observer.wait_for(|state| !matches!(state, LifecycleState::Starting)) => {
                state.map(|state| state.clone()).ok()
            }
            () = control_plane.wait_for_exit(&worker_id, deadline) => None,
        };
        guard.armed = false;
        drop(guard);
        match ready {
            Some(LifecycleState::Ready { .. }) => {}
            Some(_) => {
                return Err(RunpyError::WorkerExited {
                    worker_id: worker_id.clone(),
                });
            }
            None if tokio::time::Instant::now() < deadline => {
                control_plane
                    .remove_worker(&worker_id, ExitReason::Exited)
                    .await;
                return Err(RunpyError::WorkerExited {
                    worker_id: worker_id.clone(),
                });
            }
            None => {
                control_plane
                    .remove_worker(&worker_id, ExitReason::StartupTimeout)
                    .await;
                return Err(RunpyError::Timeout(TimeoutStage::Startup));
            }
        }

        control_plane
            .logger()
            .success(&format!("Spawned worker: {worker_id}"));
        Ok(Worker {
            id: worker_id,
            control_plane: self.control_plane,
            lifecycle,
        })
    }
}

/// Hands a worker to the monitor for removal if `spawn` is cancelled while
/// waiting for `ready`.
struct StartupGuard<'a> {
    control_plane: &'a ControlPlane,
    worker_id: &'a str,
    armed: bool,
}

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.control_plane.abandon(self.worker_id);
        }
    }
}

// ── Running worker handle ──────────────────────────────────────────────

/// Handle to one running worker process.
///
/// Returned by [`WorkerBuilder::spawn`] once the worker reported `ready`.
/// Cloning is cheap; every clone addresses the same process. The Manager
/// owns the process: after the Manager is dropped, operations return
/// [`RunpyError::ManagerClosed`], and after the worker is removed they return
/// [`RunpyError::WorkerExited`]. [`Worker::wait`] keeps working after both.
#[derive(Clone, Debug)]
pub struct Worker {
    id: String,
    control_plane: Weak<ControlPlane>,
    lifecycle: LifecycleReceiver,
}

impl Worker {
    /// Trusted Manager-generated worker identity.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Capabilities the worker advertised in `ready`; empty once it exits.
    pub fn capabilities(&self) -> Vec<String> {
        match &*self.lifecycle.borrow() {
            LifecycleState::Ready { capabilities } => capabilities.to_vec(),
            _ => Vec::new(),
        }
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

    /// Run one request on this worker and wait for its result.
    ///
    /// Equivalent to [`Worker::submit`] followed by [`Request::result`]; see
    /// those for queueing, deadline, and error semantics.
    ///
    /// ```no_run
    /// use runpy::{Data, Manager};
    /// use serde_json::json;
    /// use std::time::Duration;
    ///
    /// # async fn demo(manager: &Manager) -> Result<(), runpy::RunpyError> {
    /// let worker = manager.worker("parser").spawn().await?;
    /// let input = Data::from_iter([("text".into(), json!("hello"))]);
    /// let output = worker.execute(input, Duration::from_secs(10)).await?;
    /// println!("{output:?}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&self, data: Data, timeout: Duration) -> Result<Data, RunpyError> {
        self.submit(data, timeout).await?.result().await
    }

    /// Queue one request and return a handle to its result.
    ///
    /// A worker runs one request at a time, in submission order. `timeout`
    /// bounds the whole request from this call: waiting for a pending slot,
    /// waiting for the worker, sending, and the result. A request still
    /// queued at its deadline never runs; one already running keeps the
    /// worker busy until it finishes, and its late result is discarded.
    /// Nothing is ever retried or replayed: resubmitting creates a new
    /// request with a new ID and may repeat side effects.
    ///
    /// # Errors
    ///
    /// [`RunpyError::Unsupported`] when the worker did not advertise the
    /// `requests` capability, [`RunpyError::Timeout`] with
    /// [`TimeoutStage::Admission`] when no pending slot freed in time, and
    /// [`RunpyError::WorkerExited`] or [`RunpyError::ManagerClosed`] when the
    /// worker or Manager is gone.
    pub async fn submit(&self, data: Data, timeout: Duration) -> Result<Request, RunpyError> {
        self.control_plane()?.submit(&self.id, data, timeout).await
    }

    /// Wait until the worker stops and return its retained exit.
    ///
    /// Any number of handles may wait; one that starts after the worker
    /// already stopped returns at once. This keeps working after the Manager
    /// is dropped.
    pub async fn wait(&self) -> Result<WorkerExit, RunpyError> {
        wait_for_exit(self.lifecycle.clone(), &self.id).await
    }

    /// Request graceful termination, force process-group cleanup, and return
    /// the worker's exit.
    ///
    /// New sends to this worker are refused at once. The worker is removed
    /// when its process exits, and at the latest when the Manager's shutdown
    /// grace ends, even if `terminate` could not be written. A worker that
    /// already stopped returns its retained exit.
    pub async fn terminate(&self) -> Result<WorkerExit, RunpyError> {
        let control_plane = self.control_plane()?;
        // Whether `terminate` was written does not change the outcome: the
        // group is stopped by the grace deadline either way.
        let _ = control_plane.terminate_worker(&self.id).await;
        self.wait().await
    }

    /// Whether the worker is ready and has not exited.
    pub(crate) fn is_ready(&self) -> bool {
        matches!(&*self.lifecycle.borrow(), LifecycleState::Ready { .. })
    }

    pub(crate) fn control_plane(&self) -> Result<Arc<ControlPlane>, RunpyError> {
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
