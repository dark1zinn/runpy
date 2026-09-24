use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Weak};
use std::thread;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval, timeout};

use crate::manager::WorkerIdentity;
use crate::scribbler::Scribbler;
use crate::watchdog::WatchdogService;

pub type Meta = Map<String, Value>;
pub type Data = Map<String, Value>;

const X_WORKER_ID: &str = "x_wid";
const X_SOCKET_PATH: &str = "x_spath";
const X_OPERATION: &str = "x_op";
const OPERATIONS: &[&str] = &[
    "ready",
    "execute",
    "retry",
    "terminate",
    "done",
    "error",
    "log",
];
const MANAGER_OPERATIONS: &[&str] = &["execute", "retry", "terminate"];
const WORKER_OPERATIONS: &[&str] = &["ready", "done", "error", "log"];

const OUTPUT_QUEUE_CAPACITY: usize = 256;
const OUTPUT_READ_BUFFER_SIZE: usize = 8 * 1024;
const OUTPUT_RECORD_LIMIT: usize = 16 * 1024;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    ReservedMetadata(String),
    UnknownReservedMetadata(String),
    InvalidReservedMetadata(String),
    InvalidOperation(String),
    WrongDirection {
        operation: String,
        direction: &'static str,
    },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedMetadata(key) => {
                write!(f, "metadata key '{key}' is reserved by Runpy")
            }
            Self::UnknownReservedMetadata(key) => {
                write!(f, "unknown Runpy metadata key '{key}'")
            }
            Self::InvalidReservedMetadata(key) => {
                write!(f, "Runpy metadata '{key}' must be a string")
            }
            Self::InvalidOperation(operation) => {
                write!(f, "unknown Runpy operation '{operation}'")
            }
            Self::WrongDirection {
                operation,
                direction,
            } => write!(
                f,
                "Runpy operation '{operation}' is invalid for {direction} envelopes"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// The complete value exchanged between the Rust manager and Python workers.
///
/// Both `meta` and `data` are always JSON objects. Keys beginning with `x_`
/// are reserved for Runpy and cannot be supplied through [`Envelope::new`].
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Envelope {
    meta: Meta,
    data: Data,
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEnvelope {
            meta: Meta,
            data: Data,
        }

        let wire = WireEnvelope::deserialize(deserializer)?;
        validate_wire_meta(&wire.meta).map_err(serde::de::Error::custom)?;
        Ok(Self {
            meta: wire.meta,
            data: wire.data,
        })
    }
}

impl Envelope {
    /// Build an application-defined envelope.
    pub fn new(meta: Meta, data: Data) -> Result<Self, EnvelopeError> {
        if let Some(key) = meta.keys().find(|key| key.starts_with("x_")) {
            return Err(EnvelopeError::ReservedMetadata(key.clone()));
        }
        Ok(Self { meta, data })
    }

    /// Build a manager request that executes a worker payload.
    pub fn execute(data: Data) -> Self {
        Self::with_operation("execute", data)
    }

    /// Build a manager request that repeats the most recent execution.
    pub fn retry() -> Self {
        Self::with_operation("retry", Data::new())
    }

    /// Build a manager request that gracefully terminates a worker.
    pub fn terminate() -> Self {
        Self::with_operation("terminate", Data::new())
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn data(&self) -> &Data {
        &self.data
    }

    fn with_operation(operation: &'static str, data: Data) -> Self {
        let mut meta = Meta::new();
        meta.insert(
            X_OPERATION.to_string(),
            Value::String(operation.to_string()),
        );
        Self { meta, data }
    }

    fn operation(&self) -> Option<&str> {
        self.meta.get(X_OPERATION).and_then(Value::as_str)
    }

    fn stamp_trusted(&mut self, worker_id: &str, socket_path: &str) {
        self.meta.insert(
            X_WORKER_ID.to_string(),
            Value::String(worker_id.to_string()),
        );
        self.meta.insert(
            X_SOCKET_PATH.to_string(),
            Value::String(socket_path.to_string()),
        );
    }

    fn validate_manager_to_worker(&self) -> Result<(), EnvelopeError> {
        validate_direction(self.operation(), MANAGER_OPERATIONS, "manager-to-worker")
    }

    fn validate_worker_to_manager(&self) -> Result<(), EnvelopeError> {
        validate_direction(self.operation(), WORKER_OPERATIONS, "worker-to-manager")
    }
}

fn validate_wire_meta(meta: &Meta) -> Result<(), EnvelopeError> {
    for (key, value) in meta {
        if !key.starts_with("x_") {
            continue;
        }

        match key.as_str() {
            X_WORKER_ID | X_SOCKET_PATH => {
                if !value.is_string() {
                    return Err(EnvelopeError::InvalidReservedMetadata(key.clone()));
                }
            }
            X_OPERATION => {
                let operation = value
                    .as_str()
                    .ok_or_else(|| EnvelopeError::InvalidReservedMetadata(key.clone()))?;
                if !OPERATIONS.contains(&operation) {
                    return Err(EnvelopeError::InvalidOperation(operation.to_string()));
                }
            }
            _ => return Err(EnvelopeError::UnknownReservedMetadata(key.clone())),
        }
    }
    Ok(())
}

/// Accept a missing operation or one allowed for the given message direction.
/// Return `EnvelopeError::WrongDirection` with `direction` for any other operation.
fn validate_direction(
    operation: Option<&str>,
    allowed: &[&str],
    direction: &'static str,
) -> Result<(), EnvelopeError> {
    if let Some(operation) = operation
        && !allowed.contains(&operation)
    {
        return Err(EnvelopeError::WrongDirection {
            operation: operation.to_string(),
            direction,
        });
    }
    Ok(())
}

pub type MessageHandler = Arc<dyn Fn(InboundEnvelope) + Send + Sync>;

/// The operating-system stream that produced a worker output record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutputStream {
    Stdout,
    Stderr,
}

/// One newline-free record captured from a managed worker process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutput {
    pub worker_id: String,
    pub stream: WorkerOutputStream,
    pub line: String,
    pub truncated: bool,
    pub dropped_lines_before: u64,
}

pub type WorkerOutputHandler = Arc<dyn Fn(WorkerOutput) + Send + Sync>;

/// A received wire envelope plus a reply route to its originating worker.
#[derive(Clone)]
pub struct InboundEnvelope {
    pub envelope: Envelope,
    worker_id: String,
    mailer: Arc<Mailer>,
}

impl InboundEnvelope {
    /// Send a reply to the worker that produced this envelope.
    pub fn reply(&self, envelope: Envelope) {
        self.mailer.send(self.worker_id.clone(), envelope);
    }

    /// Send a reply and observe whether it reached the worker route.
    pub async fn reply_async(&self, envelope: Envelope) -> Result<(), String> {
        self.mailer.send_async(&self.worker_id, envelope).await
    }
}

/// A channel-based sender for one registered worker.
#[derive(Clone)]
pub(crate) struct MessageSender {
    tx: mpsc::Sender<Envelope>,
}

impl MessageSender {
    async fn send(&self, envelope: Envelope) -> Result<(), String> {
        self.tx
            .send(envelope)
            .await
            .map_err(|error| format!("Failed to send envelope to worker: {error}"))
    }
}

pub(crate) struct WorkerHandle {
    pub child: Child,
    pub identity: WorkerIdentity,
    pub sock_path: PathBuf,
    pub sender: MessageSender,
    pub process_group_id: u32,
}

struct WorkerTasks {
    session: JoinHandle<()>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

struct WorkerOutputDispatcher {
    sender: SyncSender<WorkerOutput>,
    closed: Arc<AtomicBool>,
}

impl WorkerOutputStream {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

impl WorkerOutputDispatcher {
    fn new(
        logger: Arc<Scribbler>,
        handler: Arc<RwLock<Option<WorkerOutputHandler>>>,
    ) -> Result<Self, String> {
        let (sender, receiver) = sync_channel::<WorkerOutput>(OUTPUT_QUEUE_CAPACITY);
        let closed = Arc::new(AtomicBool::new(false));
        let thread_closed = closed.clone();
        thread::Builder::new()
            .name("runpy-worker-output".to_string())
            .spawn(move || {
                while let Ok(output) = receiver.recv() {
                    if thread_closed.load(Ordering::Acquire) {
                        break;
                    }

                    let mut message = format!(
                        "[worker:{}][{}] {}",
                        output.worker_id,
                        output.stream.as_str(),
                        output.line
                    );
                    if output.truncated {
                        message.push_str(" [continued]");
                    }
                    if output.dropped_lines_before > 0 {
                        message.push_str(&format!(
                            " [dropped {} prior record(s)]",
                            output.dropped_lines_before
                        ));
                    }

                    match output.stream {
                        WorkerOutputStream::Stdout => logger.info(&message),
                        WorkerOutputStream::Stderr => logger.warning(&message),
                    }
                    let current_handler = handler.read().clone();
                    if let Some(handler) = current_handler {
                        handler(output);
                    }
                }
            })
            .map_err(|error| format!("Failed to start worker output dispatcher: {error}"))?;

        Ok(Self { sender, closed })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

struct WorkerSession {
    control_plane: Weak<ControlPlane>,
    listener: UnixListener,
    worker_id: String,
    socket_path: String,
    worker_handler: Option<MessageHandler>,
    outbound_rx: mpsc::Receiver<Envelope>,
    mailer: Arc<Mailer>,
    logger: Arc<Scribbler>,
}

fn force_stop_child(child: &mut Child, process_group_id: u32) {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(process_group_id as libc::pid_t), libc::SIGKILL) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                let _ = child.kill();
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let _ = child.wait();
}

pub(crate) fn force_stop_worker(handle: &mut WorkerHandle) {
    force_stop_child(&mut handle.child, handle.process_group_id);
}

fn cleanup_unregistered_worker(child: &mut Child, process_group_id: u32, sock_path: &PathBuf) {
    force_stop_child(child, process_group_id);
    let _ = std::fs::remove_file(sock_path);
}

fn sanitize_output(bytes: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    let mut sanitized = String::with_capacity(decoded.len());
    for character in decoded.chars() {
        if character == '\t' || !character.is_control() {
            sanitized.push(character);
        } else {
            sanitized.extend(character.escape_default());
        }
    }
    sanitized
}

fn enqueue_worker_output(
    sender: &SyncSender<WorkerOutput>,
    worker_id: &str,
    stream: WorkerOutputStream,
    bytes: &[u8],
    truncated: bool,
    dropped_lines: &mut u64,
) {
    let output = WorkerOutput {
        worker_id: worker_id.to_string(),
        stream,
        line: sanitize_output(bytes),
        truncated,
        dropped_lines_before: *dropped_lines,
    };
    match sender.try_send(output) {
        Ok(()) => *dropped_lines = 0,
        Err(TrySendError::Full(_)) => {
            *dropped_lines = dropped_lines.saturating_add(1);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

async fn capture_worker_output<R>(
    reader: R,
    worker_id: String,
    stream: WorkerOutputStream,
    sender: SyncSender<WorkerOutput>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(OUTPUT_READ_BUFFER_SIZE, reader);
    let mut pending = Vec::with_capacity(OUTPUT_RECORD_LIMIT);
    let mut dropped_lines = 0_u64;

    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) => {
                let diagnostic = format!("Failed to read {} pipe: {error}", stream.as_str());
                enqueue_worker_output(
                    &sender,
                    &worker_id,
                    WorkerOutputStream::Stderr,
                    diagnostic.as_bytes(),
                    false,
                    &mut dropped_lines,
                );
                return;
            }
        };

        if available.is_empty() {
            if !pending.is_empty() {
                enqueue_worker_output(
                    &sender,
                    &worker_id,
                    stream,
                    &pending,
                    false,
                    &mut dropped_lines,
                );
            }
            return;
        }

        let mut consumed = 0;
        while consumed < available.len() {
            let remaining = &available[consumed..];
            let capacity = OUTPUT_RECORD_LIMIT - pending.len();
            let newline = remaining.iter().position(|byte| *byte == b'\n');

            if let Some(position) = newline
                && position <= capacity
            {
                pending.extend_from_slice(&remaining[..position]);
                if pending.last() == Some(&b'\r') {
                    pending.pop();
                }
                enqueue_worker_output(
                    &sender,
                    &worker_id,
                    stream,
                    &pending,
                    false,
                    &mut dropped_lines,
                );
                pending.clear();
                consumed += position + 1;
            } else if remaining.len() >= capacity {
                pending.extend_from_slice(&remaining[..capacity]);
                enqueue_worker_output(
                    &sender,
                    &worker_id,
                    stream,
                    &pending,
                    true,
                    &mut dropped_lines,
                );
                pending.clear();
                consumed += capacity;
            } else {
                pending.extend_from_slice(remaining);
                consumed = available.len();
            }
        }
        reader.consume(consumed);
    }
}

/// The single worker router owned by a [`crate::Manager`].
pub(crate) struct ControlPlane {
    closed: AtomicBool,
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    sessions: Mutex<HashMap<String, WorkerTasks>>,
    global_handler: RwLock<Option<MessageHandler>>,
    output_handler: Arc<RwLock<Option<WorkerOutputHandler>>>,
    output_dispatcher: Mutex<Option<WorkerOutputDispatcher>>,
    watchdog: WatchdogService,
    mailer: Arc<Mailer>,
    logger: Arc<Scribbler>,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

impl ControlPlane {
    pub(crate) fn new(logger: Arc<Scribbler>) -> Arc<Self> {
        let workers = Arc::new(RwLock::new(HashMap::new()));
        let watchdog = WatchdogService::new(workers.clone(), logger.clone());
        let mailer = Arc::new(Mailer {
            workers: workers.clone(),
            logger: logger.clone(),
        });

        Arc::new(Self {
            closed: AtomicBool::new(false),
            workers,
            sessions: Mutex::new(HashMap::new()),
            global_handler: RwLock::new(None),
            output_handler: Arc::new(RwLock::new(None)),
            output_dispatcher: Mutex::new(None),
            watchdog,
            mailer,
            logger,
            monitor: Mutex::new(None),
        })
    }

    pub(crate) fn logger(&self) -> &Scribbler {
        &self.logger
    }

    pub(crate) fn start_monitoring(self: &Arc<Self>, interval_secs: u64) {
        let mut monitor = self.monitor.lock();
        if monitor.is_some() || self.closed.load(Ordering::Acquire) {
            return;
        }

        let control_plane = Arc::downgrade(self);
        *monitor = Some(tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(interval_secs));
            loop {
                tick.tick().await;
                let Some(control_plane) = control_plane.upgrade() else {
                    break;
                };
                for worker_id in control_plane.watchdog.dead_worker_ids().await {
                    control_plane.remove_worker(&worker_id).await;
                }
            }
        }));
    }

    pub(crate) fn set_global_handler(&self, handler: MessageHandler) {
        *self.global_handler.write() = Some(handler);
    }

    pub(crate) fn set_worker_output_handler(&self, handler: WorkerOutputHandler) {
        *self.output_handler.write() = Some(handler);
    }

    pub(crate) fn watchdog(&self) -> &WatchdogService {
        &self.watchdog
    }

    pub(crate) async fn register_worker(
        self: &Arc<Self>,
        mut child: Child,
        identity: WorkerIdentity,
        sock_path: PathBuf,
        socket_path: String,
        listener: UnixListener,
        worker_handler: Option<MessageHandler>,
    ) -> Result<(), String> {
        let worker_id = identity.name.clone();
        let process_group_id = child.id();
        let stdout = match child.stdout.take() {
            Some(stdout) => ChildStdout::from_std(stdout)
                .map_err(|error| format!("Failed to attach worker stdout to Tokio: {error}")),
            None => Err("Worker stdout pipe was not configured".to_string()),
        };
        let stdout = match stdout {
            Ok(stdout) => stdout,
            Err(error) => {
                cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                return Err(error);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => ChildStderr::from_std(stderr)
                .map_err(|error| format!("Failed to attach worker stderr to Tokio: {error}")),
            None => Err("Worker stderr pipe was not configured".to_string()),
        };
        let stderr = match stderr {
            Ok(stderr) => stderr,
            Err(error) => {
                cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                return Err(error);
            }
        };
        let (tx, outbound_rx) = mpsc::channel::<Envelope>(64);
        let sender = MessageSender { tx };

        // Keep task registration and worker insertion together with shutdown.
        let mut sessions = self.sessions.lock();
        let mut workers = self.workers.write();
        if self.closed.load(Ordering::Acquire) {
            drop(workers);
            drop(sessions);
            cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
            return Err("Worker manager is shutting down".to_string());
        }

        let output_sender = {
            let mut dispatcher = self.output_dispatcher.lock();
            if dispatcher.is_none() {
                let new_dispatcher = match WorkerOutputDispatcher::new(
                    self.logger.clone(),
                    self.output_handler.clone(),
                ) {
                    Ok(dispatcher) => dispatcher,
                    Err(error) => {
                        drop(dispatcher);
                        drop(workers);
                        drop(sessions);
                        cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                        return Err(error);
                    }
                };
                *dispatcher = Some(new_dispatcher);
            }
            dispatcher
                .as_ref()
                .expect("dispatcher was initialized")
                .sender
                .clone()
        };

        self.start_monitoring(5);
        workers.insert(
            worker_id.clone(),
            WorkerHandle {
                child,
                identity,
                sock_path,
                sender,
                process_group_id,
            },
        );

        let session = tokio::spawn(Self::run_session(WorkerSession {
            control_plane: Arc::downgrade(self),
            listener,
            worker_id: worker_id.clone(),
            socket_path,
            worker_handler,
            outbound_rx,
            mailer: self.mailer.clone(),
            logger: self.logger.clone(),
        }));
        let stdout = tokio::spawn(capture_worker_output(
            stdout,
            worker_id.clone(),
            WorkerOutputStream::Stdout,
            output_sender.clone(),
        ));
        let stderr = tokio::spawn(capture_worker_output(
            stderr,
            worker_id.clone(),
            WorkerOutputStream::Stderr,
            output_sender,
        ));
        sessions.insert(
            worker_id,
            WorkerTasks {
                session,
                stdout,
                stderr,
            },
        );
        Ok(())
    }

    pub(crate) async fn send(&self, worker_id: &str, envelope: Envelope) -> Result<(), String> {
        let sender = self
            .workers
            .read()
            .get(worker_id)
            .map(|handle| handle.sender.clone())
            .ok_or_else(|| format!("Worker '{worker_id}' is not registered"))?;
        sender.send(envelope).await
    }

    pub(crate) async fn broadcast(
        &self,
        envelope: Envelope,
    ) -> HashMap<String, Result<(), String>> {
        let routes: Vec<_> = self
            .workers
            .read()
            .iter()
            .map(|(worker_id, handle)| (worker_id.clone(), handle.sender.clone()))
            .collect();
        let mut results = HashMap::with_capacity(routes.len());
        for (worker_id, sender) in routes {
            results.insert(worker_id, sender.send(envelope.clone()).await);
        }
        results
    }

    pub(crate) async fn terminate_worker(&self, worker_id: &str) -> Result<(), String> {
        let send_result = self.send(worker_id, Envelope::terminate()).await;
        if send_result.is_ok() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        self.remove_worker(worker_id).await;
        send_result
    }

    pub(crate) async fn terminate_all(&self) {
        let _ = self.broadcast(Envelope::terminate()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        let worker_ids: Vec<_> = self.workers.read().keys().cloned().collect();
        for worker_id in worker_ids {
            self.remove_worker(&worker_id).await;
        }
    }

    async fn remove_worker(&self, worker_id: &str) {
        let tasks = self.sessions.lock().remove(worker_id);
        if let Some(tasks) = &tasks {
            tasks.session.abort();
        }

        let handle = self.workers.write().remove(worker_id);
        if let Some(mut handle) = handle {
            force_stop_worker(&mut handle);
            let _ = std::fs::remove_file(&handle.sock_path);
            self.logger.info_with(
                "ControlPlane",
                &format!("Removed worker '{}'", handle.identity.name),
            );
        }

        if let Some(tasks) = tasks {
            Self::finish_output_task(tasks.stdout).await;
            Self::finish_output_task(tasks.stderr).await;
        }
    }

    async fn finish_output_task(mut task: JoinHandle<()>) {
        if timeout(OUTPUT_DRAIN_TIMEOUT, &mut task).await.is_err() {
            task.abort();
        }
    }

    pub(crate) fn shutdown_now(&self) {
        self.closed.store(true, Ordering::Release);
        if let Some(task) = self.monitor.lock().take() {
            task.abort();
        }
        let mut sessions = self.sessions.lock();
        let mut workers = self.workers.write();
        if let Some(dispatcher) = self.output_dispatcher.lock().take() {
            dispatcher.close();
        }
        for (_, tasks) in sessions.drain() {
            tasks.session.abort();
            tasks.stdout.abort();
            tasks.stderr.abort();
        }
        for (_, mut handle) in workers.drain() {
            force_stop_worker(&mut handle);
            let _ = std::fs::remove_file(&handle.sock_path);
        }
    }

    async fn run_session(session: WorkerSession) {
        let WorkerSession {
            control_plane,
            listener,
            worker_id,
            socket_path,
            worker_handler,
            mut outbound_rx,
            mailer,
            logger,
        } = session;
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    logger.error_with(
                        "ControlPlane",
                        &format!("{worker_id} Accept error: {error}"),
                    );
                    break;
                }
            };

            loop {
                tokio::select! {
                    received = Self::recv_envelope(&logger, &mut stream) => {
                        let Some(mut envelope) = received else {
                            break;
                        };
                        if let Err(error) = envelope.validate_worker_to_manager() {
                            logger.error_with("Protocol", &error.to_string());
                            break;
                        }
                        envelope.stamp_trusted(&worker_id, &socket_path);

                        let inbound = InboundEnvelope {
                            envelope,
                            worker_id: worker_id.clone(),
                            mailer: mailer.clone(),
                        };
                        let Some(control_plane) = control_plane.upgrade() else {
                            return;
                        };
                        let global_handler = control_plane.global_handler.read().clone();
                        drop(control_plane);
                        if let Some(handler) = global_handler {
                            handler(inbound.clone());
                        }
                        if let Some(handler) = &worker_handler {
                            handler(inbound);
                        }
                    }
                    Some(envelope) = outbound_rx.recv() => {
                        if let Err(error) =
                            Self::write_stamped_envelope(
                                &worker_id,
                                &socket_path,
                                &mut stream,
                                envelope,
                            )
                            .await
                        {
                            logger.error_with(
                                "ControlPlane",
                                &format!("{worker_id} Send error: {error}"),
                            );
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn recv_envelope(logger: &Scribbler, stream: &mut UnixStream) -> Option<Envelope> {
        let mut size_buf = [0_u8; 8];
        if let Err(error) = stream.read_exact(&mut size_buf).await {
            match error.kind() {
                std::io::ErrorKind::UnexpectedEof => {}
                std::io::ErrorKind::ConnectionReset => {
                    logger.debug_with("Socket", "Connection reset by peer");
                }
                std::io::ErrorKind::BrokenPipe => {
                    logger.debug_with("Socket", "Broken pipe - peer closed unexpectedly");
                }
                _ => logger.error_with(
                    "Socket",
                    &format!("Read error: {error} (kind: {:?})", error.kind()),
                ),
            }
            return None;
        }

        let envelope_size = u64::from_le_bytes(size_buf) as usize;
        let mut envelope_buf = vec![0_u8; envelope_size];
        if let Err(error) = stream.read_exact(&mut envelope_buf).await {
            logger.error_with(
                "Socket",
                &format!(
                    "Error reading envelope body: {error} (kind: {:?})",
                    error.kind()
                ),
            );
            return None;
        }

        match serde_json::from_slice(&envelope_buf) {
            Ok(envelope) => Some(envelope),
            Err(error) => {
                let raw = String::from_utf8_lossy(&envelope_buf);
                logger.error_with(
                    "Protocol",
                    &format!("JSON envelope error: {error}\n  Raw: {raw}"),
                );
                None
            }
        }
    }

    async fn write_stamped_envelope(
        worker_id: &str,
        socket_path: &str,
        stream: &mut UnixStream,
        mut envelope: Envelope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        envelope.validate_manager_to_worker()?;
        envelope.stamp_trusted(worker_id, socket_path);

        let payload = serde_json::to_vec(&envelope)?;
        let size = (payload.len() as u64).to_le_bytes();
        stream.write_all(&size).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;
        Ok(())
    }
}

struct Mailer {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
    logger: Arc<Scribbler>,
}

impl Mailer {
    fn send(self: &Arc<Self>, worker_id: String, envelope: Envelope) {
        let sender = self
            .workers
            .read()
            .get(&worker_id)
            .map(|handle| handle.sender.clone());
        let Some(sender) = sender else {
            self.logger.error_with(
                "Mailer",
                &format!("Failed to send envelope: Worker '{worker_id}' is not registered"),
            );
            return;
        };

        match sender.tx.try_send(envelope) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => self.logger.error_with(
                "Mailer",
                "Failed to send envelope: worker channel is closed",
            ),
            Err(mpsc::error::TrySendError::Full(envelope)) => {
                let mailer = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = sender.send(envelope).await {
                        mailer
                            .logger
                            .error_with("Mailer", &format!("Failed to send envelope: {error}"));
                    }
                });
            }
        }
    }

    async fn send_async(&self, worker_id: &str, envelope: Envelope) -> Result<(), String> {
        let sender = self
            .workers
            .read()
            .get(worker_id)
            .map(|handle| handle.sender.clone())
            .ok_or_else(|| format!("Worker '{worker_id}' is not registered"))?;
        sender.send(envelope).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc as std_mpsc;
    use tokio::time::timeout;

    fn object(value: Value) -> Data {
        value
            .as_object()
            .cloned()
            .expect("test value must be an object")
    }

    fn test_logger() -> Arc<Scribbler> {
        Arc::new(Scribbler::new())
    }

    async fn register_worker(
        control_plane: &Arc<ControlPlane>,
        directory: &std::path::Path,
        worker_id: &str,
        handler: Option<MessageHandler>,
    ) -> UnixStream {
        let sock_path = directory.join(format!("{worker_id}.sock"));
        let listener = UnixListener::bind(&sock_path).unwrap();
        let socket_path = sock_path.to_string_lossy().into_owned();
        let mut command = Command::new("sleep");
        command
            .arg("300")
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let identity = WorkerIdentity {
            name: worker_id.to_string(),
            sock_file: format!("{worker_id}.sock"),
        };

        control_plane
            .register_worker(
                child,
                identity,
                sock_path.clone(),
                socket_path,
                listener,
                handler,
            )
            .await
            .unwrap();
        UnixStream::connect(sock_path).await.unwrap()
    }

    async fn write_frame(stream: &mut UnixStream, envelope: &Envelope) {
        let payload = serde_json::to_vec(envelope).unwrap();
        stream
            .write_all(&(payload.len() as u64).to_le_bytes())
            .await
            .unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn read_frame(stream: &mut UnixStream) -> Envelope {
        let mut size = [0_u8; 8];
        stream.read_exact(&mut size).await.unwrap();
        let mut payload = vec![0_u8; u64::from_le_bytes(size) as usize];
        stream.read_exact(&mut payload).await.unwrap();
        serde_json::from_slice(&payload).unwrap()
    }

    #[tokio::test]
    async fn output_framer_handles_split_crlf_empty_and_eof_records() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (tx, rx) = sync_channel(8);
        let task = tokio::spawn(capture_worker_output(
            reader,
            "worker".to_string(),
            WorkerOutputStream::Stdout,
            tx,
        ));

        writer.write_all(b"one\r").await.unwrap();
        writer.write_all(b"\n\ntwo").await.unwrap();
        drop(writer);
        task.await.unwrap();

        let records: Vec<_> = rx.try_iter().collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].line, "one");
        assert_eq!(records[1].line, "");
        assert_eq!(records[2].line, "two");
        assert!(records.iter().all(|record| {
            record.worker_id == "worker"
                && record.stream == WorkerOutputStream::Stdout
                && !record.truncated
        }));
    }

    #[tokio::test]
    async fn output_framer_handles_invalid_utf8_and_caps_unterminated_records() {
        let (mut writer, reader) = tokio::io::duplex(OUTPUT_RECORD_LIMIT * 2);
        let (tx, rx) = sync_channel(8);
        let task = tokio::spawn(capture_worker_output(
            reader,
            "worker".to_string(),
            WorkerOutputStream::Stderr,
            tx,
        ));
        let mut output = vec![b'x'; OUTPUT_RECORD_LIMIT + 3];
        output.extend_from_slice(&[b'\n', 0xff, 0x1b, b'A']);

        writer.write_all(&output).await.unwrap();
        drop(writer);
        task.await.unwrap();

        let records: Vec<_> = rx.try_iter().collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].line.len(), OUTPUT_RECORD_LIMIT);
        assert!(records[0].truncated);
        assert_eq!(records[1].line, "xxx");
        assert!(!records[1].truncated);
        assert_eq!(records[2].line, "�\\u{1b}A");
        assert!(!records[2].truncated);
    }

    #[test]
    fn full_output_queue_reports_drops_on_next_accepted_record() {
        let (tx, rx) = sync_channel(1);
        let mut dropped = 0;
        enqueue_worker_output(
            &tx,
            "worker",
            WorkerOutputStream::Stdout,
            b"first",
            false,
            &mut dropped,
        );
        enqueue_worker_output(
            &tx,
            "worker",
            WorkerOutputStream::Stdout,
            b"dropped",
            false,
            &mut dropped,
        );
        assert_eq!(dropped, 1);
        assert_eq!(rx.recv().unwrap().line, "first");

        enqueue_worker_output(
            &tx,
            "worker",
            WorkerOutputStream::Stdout,
            b"next",
            false,
            &mut dropped,
        );
        let next = rx.recv().unwrap();
        assert_eq!(next.line, "next");
        assert_eq!(next.dropped_lines_before, 1);
        assert_eq!(dropped, 0);
    }

    #[tokio::test]
    async fn output_task_drain_is_bounded() {
        let task = tokio::spawn(std::future::pending::<()>());
        let started = tokio::time::Instant::now();
        ControlPlane::finish_output_task(task).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_router_uses_live_handler_and_replies_to_originating_workers() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(test_logger());
        let mut first = register_worker(&control_plane, directory.path(), "worker-a", None).await;
        let mut second = register_worker(&control_plane, directory.path(), "worker-b", None).await;
        let (observed_tx, observed_rx) = std_mpsc::channel();

        control_plane.set_global_handler(Arc::new(move |inbound| {
            observed_tx.send(inbound.envelope.clone()).unwrap();
            let source = inbound.envelope.data()["source"].clone();
            inbound.reply(Envelope::execute(object(json!({ "source": source }))));
        }));

        for (stream, source) in [(&mut first, "first"), (&mut second, "second")] {
            let inbound: Envelope = serde_json::from_value(json!({
                "meta": {
                    "x_op": "ready",
                    "x_wid": "spoofed",
                    "x_spath": "/tmp/spoofed.sock"
                },
                "data": {"source": source}
            }))
            .unwrap();
            write_frame(stream, &inbound).await;
        }

        let first_reply = timeout(Duration::from_secs(1), read_frame(&mut first))
            .await
            .unwrap();
        let second_reply = timeout(Duration::from_secs(1), read_frame(&mut second))
            .await
            .unwrap();
        assert_eq!(first_reply.meta()["x_wid"], "worker-a");
        assert_eq!(first_reply.data()["source"], "first");
        assert_eq!(second_reply.meta()["x_wid"], "worker-b");
        assert_eq!(second_reply.data()["source"], "second");

        let mut observed = [observed_rx.recv().unwrap(), observed_rx.recv().unwrap()];
        observed.sort_by(|left, right| {
            left.meta()["x_wid"]
                .as_str()
                .cmp(&right.meta()["x_wid"].as_str())
        });
        assert_eq!(observed[0].meta()["x_wid"], "worker-a");
        assert_eq!(
            observed[0].meta()["x_spath"],
            directory
                .path()
                .join("worker-a.sock")
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(observed[1].meta()["x_wid"], "worker-b");

        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_direction_closes_connection_without_dispatch() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(test_logger());
        let (called_tx, called_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |_| {
            called_tx.send(()).unwrap();
        });
        let mut stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;

        write_frame(&mut stream, &Envelope::execute(Data::new())).await;
        let mut byte = [0_u8; 1];
        let result = timeout(Duration::from_secs(1), stream.read_exact(&mut byte))
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(called_rx.try_recv().is_err());

        control_plane.shutdown_now();
    }

    #[tokio::test]
    async fn sender_reports_dropped_receiver() {
        let (tx, rx) = mpsc::channel(1);
        let sender = MessageSender { tx };
        drop(rx);

        let error = sender.send(Envelope::terminate()).await.unwrap_err();
        assert!(error.contains("Failed to send envelope"));
    }

    #[test]
    fn registration_starts_monitoring_after_synchronous_construction() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(test_logger());
        assert!(control_plane.monitor.lock().is_none());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let _stream = register_worker(&control_plane, directory.path(), "worker", None).await;
            assert!(control_plane.monitor.lock().is_some());
            control_plane.shutdown_now();
        });
    }

    #[tokio::test]
    async fn registration_after_shutdown_stops_child_and_removes_socket() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(test_logger());
        let sock_path = directory.path().join("worker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let socket_path = sock_path.to_string_lossy().into_owned();
        let mut command = Command::new("sleep");
        command
            .arg("300")
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let pid = child.id();

        control_plane.shutdown_now();
        let error = control_plane
            .register_worker(
                child,
                WorkerIdentity {
                    name: "worker".to_string(),
                    sock_file: "worker.sock".to_string(),
                },
                sock_path.clone(),
                socket_path,
                listener,
                None,
            )
            .await
            .unwrap_err();

        assert!(error.contains("shutting down"));
        assert!(!sock_path.exists());
        assert!(control_plane.workers.read().is_empty());
        assert!(control_plane.sessions.lock().is_empty());
        assert!(control_plane.monitor.lock().is_none());
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
    }

    #[tokio::test]
    async fn mailer_queues_replies_in_call_order_and_waits_when_full() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut command = Command::new("sleep");
        command.arg("300").process_group(0);
        let child = command.spawn().unwrap();
        let workers = Arc::new(RwLock::new(HashMap::from([(
            "worker".to_string(),
            WorkerHandle {
                process_group_id: child.id(),
                child,
                identity: WorkerIdentity {
                    name: "worker".to_string(),
                    sock_file: "worker.sock".to_string(),
                },
                sock_path: PathBuf::from("worker.sock"),
                sender: MessageSender { tx },
            },
        )])));
        let mailer = Arc::new(Mailer {
            workers: workers.clone(),
            logger: test_logger(),
        });
        let first = Envelope::execute(object(json!({ "reply": 1 })));
        let second = Envelope::execute(object(json!({ "reply": 2 })));
        let third = Envelope::execute(object(json!({ "reply": 3 })));

        mailer.send("worker".to_string(), first.clone());
        mailer.send("worker".to_string(), second.clone());
        mailer.send("worker".to_string(), third.clone());

        assert_eq!(rx.try_recv().unwrap(), first);
        assert_eq!(rx.try_recv().unwrap(), second);
        assert_eq!(
            timeout(Duration::from_secs(1), rx.recv()).await.unwrap(),
            Some(third)
        );
        let mut handle = workers.write().remove("worker").unwrap();
        force_stop_worker(&mut handle);
    }
}
