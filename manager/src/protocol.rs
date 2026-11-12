use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Weak};
use std::thread;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Duration, Instant, interval, timeout, timeout_at};

use crate::config::{ProtocolLimits, QueueLimits};
use crate::error::{RunpyError, TimeoutStage};
use crate::lifecycle::{
    ExitReason, LifecycleReceiver, LifecycleSender, LifecycleState, WorkerExit, lifecycle_channel,
    parse_ready,
};
use crate::manager::WorkerIdentity;
use crate::request::{
    ActiveSlot, Completion, PendingRequest, RemoteError, Request, RequestId, run_dispatcher,
};
use crate::scribbler::Scribbler;
use crate::watchdog::WatchdogService;

/// Application-owned envelope metadata.
///
/// Every key beginning with `x_` is reserved by Runpy.
pub type Meta = Map<String, Value>;
/// Application-owned envelope data.
pub type Data = Map<String, Value>;

const X_WORKER_ID: &str = "x_wid";
const X_SOCKET_PATH: &str = "x_spath";
const X_OPERATION: &str = "x_op";
const X_REQUEST: &str = "x_req";
const OPERATIONS: &[&str] = &[
    "ready",
    "execute",
    "cancel",
    "terminate",
    "done",
    "error",
    "cancelled",
    "log",
];
const MANAGER_OPERATIONS: &[&str] = &["execute", "cancel", "terminate"];
const WORKER_OPERATIONS: &[&str] = &["ready", "done", "error", "cancelled", "log"];

const OUTPUT_QUEUE_CAPACITY: usize = 256;
const OUTPUT_READ_BUFFER_SIZE: usize = 8 * 1024;
const OUTPUT_RECORD_LIMIT: usize = 16 * 1024;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
/// Complete envelopes buffered between a session and its socket I/O tasks.
const SESSION_QUEUE_CAPACITY: usize = 64;
/// How often termination checks whether a worker already exited.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How often the watchdog samples process-group resources; well above
/// sysinfo's minimum CPU update interval on every platform.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Abort an owned helper task when its owning scope ends or is cancelled.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Validation failure for reserved metadata or operation direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// Application metadata supplied a key in Runpy's reserved `x_` namespace.
    ReservedMetadata(String),
    /// A decoded wire envelope contained an unrecognized `x_` key.
    UnknownReservedMetadata(String),
    /// A decoded reserved metadata value was not a string.
    InvalidReservedMetadata(String),
    /// `x_op` named an operation outside Runpy's operation set.
    InvalidOperation(String),
    /// A known operation was sent from the wrong protocol side.
    WrongDirection {
        /// Rejected operation name.
        operation: String,
        /// Human-readable expected message direction.
        direction: &'static str,
    },
    /// The envelope nests containers deeper than the protocol allows.
    TooDeep,
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
            Self::TooDeep => write!(
                f,
                "envelope nesting exceeds the maximum depth of {}",
                crate::strict_json::MAX_JSON_DEPTH
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Complete JSON value exchanged between the Rust Manager and Python worker.
///
/// Both fields are JSON objects. [`Envelope::new`] creates application
/// messages and rejects all `x_` metadata; operation constructors add the
/// appropriate Manager-to-worker `x_op`. Runpy stamps trusted `x_wid` and
/// `x_spath` values when writing a frame.
///
/// ```
/// use runpy::{Data, Envelope, Meta};
/// use serde_json::json;
///
/// let mut meta = Meta::new();
/// meta.insert("correlation_id".into(), json!(42));
/// let mut data = Data::new();
/// data.insert("task".into(), json!("parse"));
///
/// let envelope = Envelope::new(meta, data).unwrap();
/// assert_eq!(envelope.meta()["correlation_id"], 42);
/// assert_eq!(envelope.data()["task"], "parse");
/// ```
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
    /// Build an application-defined envelope with no `x_op`.
    ///
    /// Returns [`EnvelopeError::ReservedMetadata`] when any metadata key
    /// begins with `x_`.
    ///
    /// ```
    /// use runpy::{Data, Envelope, Meta};
    /// use serde_json::json;
    ///
    /// let envelope = Envelope::new(
    ///     Meta::from_iter([("correlation_id".into(), json!(7))]),
    ///     Data::from_iter([("value".into(), json!("ok"))]),
    /// )
    /// .unwrap();
    /// assert_eq!(envelope.data()["value"], "ok");
    /// ```
    pub fn new(meta: Meta, data: Data) -> Result<Self, EnvelopeError> {
        if let Some(key) = meta.keys().find(|key| key.starts_with("x_")) {
            return Err(EnvelopeError::ReservedMetadata(key.clone()));
        }
        Ok(Self { meta, data })
    }

    /// Internal `execute` carrying a request ID; never built by callers, who
    /// use [`crate::Worker::submit`] so the one-request-at-a-time invariant
    /// cannot be bypassed.
    pub(crate) fn request(id: &RequestId, data: Data) -> Self {
        let mut envelope = Self::with_operation("execute", data);
        envelope
            .meta
            .insert(X_REQUEST.to_string(), Value::String(id.to_string()));
        envelope
    }

    /// Internal `cancel` for one request; built only by [`crate::Canceller`].
    pub(crate) fn cancel(id: &RequestId) -> Self {
        let mut envelope = Self::with_operation("cancel", Data::new());
        envelope
            .meta
            .insert(X_REQUEST.to_string(), Value::String(id.to_string()));
        envelope
    }

    /// Build a Manager-to-worker graceful `terminate` request.
    ///
    /// ```
    /// use runpy::Envelope;
    ///
    /// let envelope = Envelope::terminate();
    /// assert_eq!(envelope.meta()["x_op"], "terminate");
    /// ```
    pub fn terminate() -> Self {
        Self::with_operation("terminate", Data::new())
    }

    /// Borrow this envelope's metadata object.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// Borrow this envelope's application data object.
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

    fn request_id(&self) -> Option<&str> {
        self.meta.get(X_REQUEST).and_then(Value::as_str)
    }

    pub(crate) fn into_data(self) -> Data {
        self.data
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

    /// Reject envelopes nested deeper than the strict JSON profile allows,
    /// counting the envelope object itself.
    fn validate_depth(&self) -> Result<(), EnvelopeError> {
        let deepest = 1 + crate::strict_json::object_depth(&self.meta)
            .max(crate::strict_json::object_depth(&self.data));
        if deepest > crate::strict_json::MAX_JSON_DEPTH {
            return Err(EnvelopeError::TooDeep);
        }
        Ok(())
    }
}

fn validate_wire_meta(meta: &Meta) -> Result<(), EnvelopeError> {
    for (key, value) in meta {
        if !key.starts_with("x_") {
            continue;
        }

        match key.as_str() {
            X_WORKER_ID | X_SOCKET_PATH | X_REQUEST => {
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
    // Only request-bearing operations may carry a request ID. A worker `error`
    // may omit it (unsolicited), so presence is not required here.
    if meta.contains_key(X_REQUEST)
        && !matches!(
            meta.get(X_OPERATION).and_then(Value::as_str),
            Some("execute" | "done" | "error" | "cancel" | "cancelled")
        )
    {
        return Err(EnvelopeError::InvalidReservedMetadata(
            X_REQUEST.to_string(),
        ));
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

/// Shared synchronous callback type for validated worker envelopes.
///
/// Handlers should return promptly because they run in the protocol task.
/// Runpy catches a handler panic so dispatch continues.
pub type MessageHandler = Arc<dyn Fn(InboundEnvelope) + Send + Sync>;

/// Operating-system stream that produced a [`WorkerOutput`] record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutputStream {
    /// Captured process standard output; logged at info level.
    Stdout,
    /// Captured process standard error; logged at warning level.
    Stderr,
}

/// One newline-free record captured from a managed worker process.
///
/// Records preserve byte order within a stream, but no total order exists
/// between stdout, stderr, and socket envelopes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutput {
    /// Trusted Manager-generated worker identity.
    pub worker_id: String,
    /// Source operating-system stream.
    pub stream: WorkerOutputStream,
    /// Lossy UTF-8 text with terminal control characters escaped.
    pub line: String,
    /// Whether the 16 KiB limit was reached before a newline.
    pub truncated: bool,
    /// Records dropped for this worker/stream before this accepted record.
    pub dropped_lines_before: u64,
}

/// Shared callback type for attributed worker process output.
///
/// It runs on the dedicated output-dispatcher thread and should return
/// promptly. Runpy catches callback panics so the dispatcher can continue.
pub type WorkerOutputHandler = Arc<dyn Fn(WorkerOutput) + Send + Sync>;

/// Validated worker envelope plus a reply route to its source.
///
/// The Manager overwrites `x_wid` and `x_spath` with trusted values before
/// constructing this context.
#[derive(Clone)]
pub struct InboundEnvelope {
    /// Received envelope, including trusted routing metadata.
    ///
    /// Shared between the global and per-worker handler without copying.
    pub envelope: Arc<Envelope>,
    worker_id: String,
    mailer: Arc<Mailer>,
}

impl InboundEnvelope {
    /// Admit a reply to the originating worker without waiting.
    ///
    /// Returns [`RunpyError::QueueFull`] when the worker's route or the
    /// Manager's outbound byte budget is exhausted; nothing is buffered
    /// outside those bounds. The returned [`SendReceipt`] reports whether the
    /// frame was written; dropping it is fine for fire-and-forget replies.
    ///
    /// ```no_run
    /// use runpy::{Data, Envelope, Manager, Meta};
    ///
    /// # fn demo(manager: &Manager) {
    /// manager.on_message(|inbound| {
    ///     let reply = Envelope::new(Meta::new(), Data::new()).unwrap();
    ///     if let Err(error) = inbound.reply(reply) {
    ///         eprintln!("reply not admitted: {error}");
    ///     }
    /// });
    /// # }
    /// ```
    pub fn reply(&self, envelope: Envelope) -> Result<SendReceipt, RunpyError> {
        self.mailer.route(&self.worker_id)?.try_send(envelope)
    }

    /// Send a reply and wait until it is completely written to the socket.
    ///
    /// Success means the frame reached the socket, not that the worker
    /// processed it. Waiting for capacity and the write share the Manager's
    /// send deadline.
    ///
    /// ```no_run
    /// use runpy::{Data, Envelope, Manager, Meta};
    ///
    /// # fn demo(manager: &Manager) {
    /// manager.on_message(|inbound| {
    ///     tokio::spawn(async move {
    ///         let reply = Envelope::new(Meta::new(), Data::new()).unwrap();
    ///         let _ = inbound.reply_async(reply).await;
    ///     });
    /// });
    /// # }
    /// ```
    pub async fn reply_async(&self, envelope: Envelope) -> Result<(), RunpyError> {
        self.mailer.route(&self.worker_id)?.send(envelope).await
    }
}

/// Extra time a receipt waits past its deadline for a writer that is already
/// enforcing the same deadline to report its own outcome.
const RECEIPT_GRACE: Duration = Duration::from_millis(100);

/// Write outcome of an admitted outbound envelope.
///
/// Admission already reserved queue capacity. [`SendReceipt::wait`] resolves
/// once the frame is completely written, expires unwritten, or its connection
/// fails, and never later than shortly after the send deadline. A successful
/// write does not mean the worker processed the envelope.
#[must_use = "dropping a receipt discards the write outcome, not the send"]
pub struct SendReceipt {
    worker_id: String,
    deadline: Instant,
    outcome: oneshot::Receiver<Result<(), RunpyError>>,
}

impl SendReceipt {
    /// Wait for the write outcome.
    ///
    /// Returns [`TimeoutStage::Queued`] when no writer picked the frame up
    /// before the deadline (for example because the worker never connected);
    /// such a frame is later discarded unwritten.
    pub async fn wait(self) -> Result<(), RunpyError> {
        match timeout_at(self.deadline + RECEIPT_GRACE, self.outcome).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(RunpyError::WorkerExited {
                worker_id: self.worker_id,
            }),
            Err(_) => Err(RunpyError::Timeout(TimeoutStage::Queued)),
        }
    }
}

/// One encoded frame admitted to a worker route.
///
/// The byte-budget permit is released when the frame is written or dropped,
/// so queued memory never exceeds the Manager's budget.
pub(crate) struct OutboundFrame {
    frame: Vec<u8>,
    deadline: Instant,
    ack: oneshot::Sender<Result<(), RunpyError>>,
    _budget: OwnedSemaphorePermit,
}

/// Bounded outbound route to one registered worker.
///
/// Envelopes are validated, trust-stamped, and encoded before admission so
/// errors return to the caller and the byte budget reflects real frame size.
#[derive(Clone)]
pub(crate) struct MessageSender {
    tx: mpsc::Sender<OutboundFrame>,
    worker_id: String,
    socket_path: String,
    budget: Arc<Semaphore>,
    limits: ProtocolLimits,
    send_timeout: Duration,
    /// Set once termination starts; new application sends are refused.
    closing: Arc<AtomicBool>,
}

impl MessageSender {
    /// Deadline the Manager allows for one send, also used for cancellation.
    pub(crate) fn send_timeout(&self) -> Duration {
        self.send_timeout
    }

    fn encode(&self, mut envelope: Envelope) -> Result<(Vec<u8>, u32), RunpyError> {
        envelope.validate_manager_to_worker()?;
        envelope.validate_depth()?;
        envelope.stamp_trusted(&self.worker_id, &self.socket_path);
        let frame =
            encode_frame(&envelope, self.limits.max_frame_bytes()).map_err(
                |error| match error {
                    FrameError::TooLarge { size, limit } => {
                        RunpyError::FrameTooLarge { size, limit }
                    }
                    FrameError::Encode(error) => RunpyError::Encode(error),
                    other => RunpyError::Io(std::io::Error::other(other.to_string())),
                },
            )?;
        // ProtocolLimits caps frames below u32::MAX, so this never truncates.
        let cost = frame.len() as u32;
        Ok((frame, cost))
    }

    fn exited(&self) -> RunpyError {
        RunpyError::WorkerExited {
            worker_id: self.worker_id.clone(),
        }
    }

    fn receipt(
        &self,
        frame: Vec<u8>,
        budget: OwnedSemaphorePermit,
        deadline: Instant,
    ) -> (OutboundFrame, SendReceipt) {
        let (ack, outcome) = oneshot::channel();
        (
            OutboundFrame {
                frame,
                deadline,
                ack,
                _budget: budget,
            },
            SendReceipt {
                worker_id: self.worker_id.clone(),
                deadline,
                outcome,
            },
        )
    }

    fn accepting(&self) -> Result<(), RunpyError> {
        if self.closing.load(Ordering::Acquire) {
            Err(self.exited())
        } else {
            Ok(())
        }
    }

    /// Admit before `deadline`, waiting for byte budget and a slot.
    async fn admit_until(
        &self,
        envelope: Envelope,
        deadline: Instant,
    ) -> Result<SendReceipt, RunpyError> {
        let (frame, cost) = self.encode(envelope)?;
        let budget = timeout_at(deadline, self.budget.clone().acquire_many_owned(cost))
            .await
            .map_err(|_| RunpyError::Timeout(TimeoutStage::Admission))?
            .map_err(|_| RunpyError::ManagerClosed)?;
        let slot = timeout_at(deadline, self.tx.reserve())
            .await
            .map_err(|_| RunpyError::Timeout(TimeoutStage::Admission))?
            .map_err(|_| self.exited())?;
        let (entry, receipt) = self.receipt(frame, budget, deadline);
        slot.send(entry);
        Ok(receipt)
    }

    /// Reject internal request operations on the application send paths.
    fn application(envelope: &Envelope) -> Result<(), RunpyError> {
        if matches!(envelope.operation(), Some("execute" | "cancel")) {
            return Err(RunpyError::InvalidConfig(
                "execute and cancel envelopes cannot be sent directly; use Worker::submit, \
                 Worker::execute, or Request::canceller"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Admit within the send deadline, waiting for byte budget and a slot.
    async fn admit(&self, envelope: Envelope) -> Result<SendReceipt, RunpyError> {
        self.accepting()?;
        Self::application(&envelope)?;
        self.admit_until(envelope, Instant::now() + self.send_timeout)
            .await
    }

    /// Admit immediately or fail with [`RunpyError::QueueFull`].
    pub(crate) fn try_send(&self, envelope: Envelope) -> Result<SendReceipt, RunpyError> {
        self.accepting()?;
        Self::application(&envelope)?;
        let deadline = Instant::now() + self.send_timeout;
        let (frame, cost) = self.encode(envelope)?;
        let budget =
            self.budget
                .clone()
                .try_acquire_many_owned(cost)
                .map_err(|error| match error {
                    TryAcquireError::NoPermits => RunpyError::QueueFull,
                    TryAcquireError::Closed => RunpyError::ManagerClosed,
                })?;
        let slot = self.tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(()) => RunpyError::QueueFull,
            mpsc::error::TrySendError::Closed(()) => self.exited(),
        })?;
        let (entry, receipt) = self.receipt(frame, budget, deadline);
        slot.send(entry);
        Ok(receipt)
    }

    /// Admit and wait until the frame is completely written.
    pub(crate) async fn send(&self, envelope: Envelope) -> Result<(), RunpyError> {
        self.admit(envelope).await?.wait().await
    }

    /// Write a Runpy-internal envelope (a request) before `deadline`.
    pub(crate) async fn send_internal(
        &self,
        envelope: Envelope,
        deadline: Instant,
    ) -> Result<(), RunpyError> {
        self.accepting()?;
        self.admit_until(envelope, deadline).await?.wait().await
    }

    /// Stop accepting application sends, then try to deliver `terminate`
    /// before `deadline`. The result only reports whether the request was
    /// written; the caller enforces the deadline by stopping the process.
    async fn request_termination(&self, deadline: Instant) -> Result<(), RunpyError> {
        self.closing.store(true, Ordering::Release);
        self.admit_until(Envelope::terminate(), deadline)
            .await?
            .wait()
            .await
    }
}

/// Reservation of one of the Manager's worker-process slots.
pub(crate) type ProcessSlot = OwnedSemaphorePermit;

/// Process and routing state retained for one registered worker.
///
/// The standard child remains here so Watchdog polling and synchronous
/// process-group cleanup share one ownership boundary.
pub(crate) struct WorkerHandle {
    pub child: Child,
    pub identity: WorkerIdentity,
    pub sock_path: PathBuf,
    pub sender: MessageSender,
    pub process_group_id: u32,
    /// Publishes readiness and the retained final exit to worker handles.
    pub lifecycle: LifecycleSender,
    /// Submission queue of this worker's request dispatcher.
    pub requests: mpsc::UnboundedSender<PendingRequest>,
    /// Released when this handle is dropped, which every removal path does
    /// only after the process group was stopped and reaped.
    pub _slot: ProcessSlot,
}

/// Process-lifetime tasks removed as one unit during cleanup.
///
/// The socket session serves exactly one connection; stdout/stderr readers
/// remain singular for the lifetime of the process.
struct WorkerTasks {
    session: JoinHandle<()>,
    /// Request dispatcher; aborting it tells every waiter whether its
    /// request ran.
    dispatcher: JoinHandle<()>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

/// Bounded bridge from asynchronous pipe readers to synchronous observers.
///
/// Its detached standard thread owns Scribbler writes and user callbacks so
/// neither can block a Tokio runtime worker. Callback panics are contained.
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
    /// Start the dispatcher thread. With `mirror`, every record is also
    /// logged through Scribbler before the user handler runs.
    fn new(
        logger: Arc<Scribbler>,
        handler: Arc<RwLock<Option<WorkerOutputHandler>>>,
        mirror: bool,
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

                    if mirror {
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
                    }
                    let current_handler = handler.read().clone();
                    if let Some(handler) = current_handler
                        && catch_unwind(AssertUnwindSafe(|| handler(output))).is_err()
                    {
                        logger.error_with(
                            "WorkerOutput",
                            "Worker output handler panicked; continuing output dispatch",
                        );
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
    outbound_rx: mpsc::Receiver<OutboundFrame>,
    mailer: Arc<Mailer>,
    logger: Arc<Scribbler>,
    limits: ProtocolLimits,
    ended: mpsc::UnboundedSender<(String, ExitReason)>,
    lifecycle: LifecycleSender,
    active_request: ActiveSlot,
}

/// Kill the complete process group when possible, then reap the stored child.
///
/// Reaping is required on every registration/removal failure path to avoid
/// leaking a zombie `uv` process. Returns the reaped exit status.
fn force_stop_child(child: &mut Child, process_group_id: u32) -> Option<ExitStatus> {
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

    child.wait().ok()
}

pub(crate) fn force_stop_worker(handle: &mut WorkerHandle) -> Option<ExitStatus> {
    force_stop_child(&mut handle.child, handle.process_group_id)
}

fn cleanup_unregistered_worker(child: &mut Child, process_group_id: u32, sock_path: &PathBuf) {
    let _ = force_stop_child(child, process_group_id);
    let _ = std::fs::remove_file(sock_path);
}

/// Decode worker bytes lossily and neutralize terminal control characters.
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

/// Per-stream byte budget: one second of `rate` as burst, refilled
/// continuously. A record larger than the whole budget costs the full budget
/// so it can still pass once the stream has been quiet for a second.
struct OutputBudget {
    rate: f64,
    available: f64,
    refilled: tokio::time::Instant,
}

impl OutputBudget {
    fn new(bytes_per_second: u64) -> Self {
        let rate = bytes_per_second as f64;
        Self {
            rate,
            available: rate,
            refilled: tokio::time::Instant::now(),
        }
    }

    /// Take budget for a record of `bytes`, or report that it must be dropped.
    fn take(&mut self, bytes: usize) -> bool {
        let now = tokio::time::Instant::now();
        let elapsed = now.duration_since(self.refilled).as_secs_f64();
        self.available = (self.available + elapsed * self.rate).min(self.rate);
        self.refilled = now;
        let cost = (bytes.max(1) as f64).min(self.rate);
        if self.available >= cost {
            self.available -= cost;
            true
        } else {
            false
        }
    }
}

/// Submit without waiting. Records over the stream's byte budget, or arriving
/// while the queue is full, are dropped rather than blocking pipe drainage;
/// the next accepted record reports the accumulated loss.
fn enqueue_worker_output(
    sender: &SyncSender<WorkerOutput>,
    worker_id: &str,
    stream: WorkerOutputStream,
    bytes: &[u8],
    truncated: bool,
    dropped_lines: &mut u64,
    budget: Option<&mut OutputBudget>,
) {
    if let Some(budget) = budget
        && !budget.take(bytes.len())
    {
        *dropped_lines = dropped_lines.saturating_add(1);
        return;
    }
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

/// Frame one async byte stream into bounded, newline-free output records.
///
/// Newline is the only delimiter, one preceding carriage return is normalized,
/// EOF emits an unterminated fragment, and 16 KiB segments bound retained
/// memory for newline-free output. `rate` caps the bytes per second this
/// stream delivers; `None` is unlimited.
async fn capture_worker_output<R>(
    reader: R,
    worker_id: String,
    stream: WorkerOutputStream,
    sender: SyncSender<WorkerOutput>,
    rate: Option<u64>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(OUTPUT_READ_BUFFER_SIZE, reader);
    let mut pending = Vec::with_capacity(OUTPUT_RECORD_LIMIT);
    let mut dropped_lines = 0_u64;
    let mut budget = rate.map(OutputBudget::new);

    loop {
        let available = match reader.fill_buf().await {
            Ok(available) => available,
            Err(error) => {
                // Runpy's own diagnostic is not worker output: never budgeted.
                let diagnostic = format!("Failed to read {} pipe: {error}", stream.as_str());
                enqueue_worker_output(
                    &sender,
                    &worker_id,
                    WorkerOutputStream::Stderr,
                    diagnostic.as_bytes(),
                    false,
                    &mut dropped_lines,
                    None,
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
                    budget.as_mut(),
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
                    budget.as_mut(),
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
                    budget.as_mut(),
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

/// Shared worker registry, router, monitoring service, and task owner.
///
/// Manager is the public composition root. This private service centralizes
/// process registration so worker facades cannot outlive cleanup ownership.
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
    limits: ProtocolLimits,
    queue: QueueLimits,
    /// Manager-wide byte budget for admitted, unwritten outbound frames.
    budget: Arc<Semaphore>,
    /// Manager-wide bound on submitted requests not yet consumed.
    request_budget: Arc<Semaphore>,
    /// Manager-wide bound on worker processes. A slot is held from before
    /// `uv` starts until its process group has been killed and reaped.
    processes: Arc<Semaphore>,
    /// Sum of warm minima held by pools; never above `max_workers`.
    reserved_minima: AtomicUsize,
    /// Next request ID; never wraps.
    next_request: AtomicU64,
    /// Sessions report their terminal reason here; the monitor removes them.
    ended_tx: mpsc::UnboundedSender<(String, ExitReason)>,
    ended_rx: Mutex<Option<mpsc::UnboundedReceiver<(String, ExitReason)>>>,
}

impl ControlPlane {
    pub(crate) fn new(
        logger: Arc<Scribbler>,
        limits: ProtocolLimits,
        queue: QueueLimits,
    ) -> Arc<Self> {
        let workers = Arc::new(RwLock::new(HashMap::new()));
        let watchdog = WatchdogService::new(workers.clone(), logger.clone());
        let mailer = Arc::new(Mailer {
            workers: workers.clone(),
        });
        // Each session reports at most once, so this channel holds at most
        // one entry per registered worker.
        let (ended_tx, ended_rx) = mpsc::unbounded_channel();

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
            limits,
            queue,
            budget: Arc::new(Semaphore::new(queue.queued_bytes)),
            request_budget: Arc::new(Semaphore::new(queue.max_pending_requests)),
            processes: Arc::new(Semaphore::new(queue.max_workers)),
            reserved_minima: AtomicUsize::new(0),
            next_request: AtomicU64::new(1),
            ended_tx,
            ended_rx: Mutex::new(Some(ended_rx)),
        })
    }

    pub(crate) fn limits(&self) -> ProtocolLimits {
        self.limits
    }

    /// Manager-wide bound on worker processes.
    pub(crate) fn max_workers(&self) -> usize {
        self.queue.max_workers
    }

    pub(crate) fn logger(&self) -> &Scribbler {
        &self.logger
    }

    pub(crate) fn start_monitoring(self: &Arc<Self>, interval_secs: u64) {
        let mut monitor = self.monitor.lock();
        if monitor.is_some() || self.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(mut ended_rx) = self.ended_rx.lock().take() else {
            return;
        };

        // The monitor owns every asynchronous removal: dead processes found
        // by the watchdog and sessions reporting that their connection ended.
        // It also drives the watchdog's per-second resource sampling.
        let control_plane = Arc::downgrade(self);
        *monitor = Some(tokio::spawn(async move {
            enum Event {
                Check,
                Sample,
                Ended(Option<(String, ExitReason)>),
            }
            let mut tick = interval(Duration::from_secs(interval_secs));
            let mut sample_tick = interval(SAMPLE_INTERVAL);
            sample_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let event = tokio::select! {
                    _ = tick.tick() => Event::Check,
                    _ = sample_tick.tick() => Event::Sample,
                    ended = ended_rx.recv() => Event::Ended(ended),
                };
                let Some(control_plane) = control_plane.upgrade() else {
                    break;
                };
                match event {
                    Event::Ended(Some((worker_id, reason))) => {
                        control_plane.remove_worker(&worker_id, reason).await;
                    }
                    Event::Ended(None) => break,
                    Event::Sample => {
                        // Sampling may wait on a blocking thread; do not keep
                        // the control plane alive across it after Manager drop.
                        let sample = control_plane.watchdog.sample();
                        drop(control_plane);
                        sample.await;
                    }
                    Event::Check => {
                        for worker_id in control_plane.watchdog.dead_worker_ids().await {
                            control_plane
                                .remove_worker(&worker_id, ExitReason::Exited)
                                .await;
                        }
                    }
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

    /// Reserve a worker-process slot before launching `uv`, waiting until
    /// `deadline` for a running worker to be stopped and reaped.
    pub(crate) async fn reserve_process(
        &self,
        deadline: Instant,
    ) -> Result<ProcessSlot, RunpyError> {
        timeout_at(deadline, self.processes.clone().acquire_owned())
            .await
            .map_err(|_| RunpyError::Timeout(TimeoutStage::Admission))?
            .map_err(|_| RunpyError::ManagerClosed)
    }

    /// The Manager-wide worker-process semaphore, for the pool scheduler.
    pub(crate) fn processes(&self) -> &Arc<Semaphore> {
        &self.processes
    }

    /// Account for `count` more warm-minimum workers, failing if the sum of
    /// all pools' minima would exceed `max_workers`.
    pub(crate) fn reserve_minima(&self, count: usize) -> Result<(), RunpyError> {
        let limit = self.queue.max_workers;
        // See `next_request_id` for why this is not `fetch_update`.
        let mut reserved = self.reserved_minima.load(Ordering::Acquire);
        loop {
            let Some(total) = reserved.checked_add(count).filter(|total| *total <= limit) else {
                return Err(RunpyError::InvalidConfig(format!(
                    "pool min_workers ({count}) plus minima already reserved ({reserved}) \
                     exceeds the Manager's max_workers ({limit})"
                )));
            };
            match self.reserved_minima.compare_exchange_weak(
                reserved,
                total,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => reserved = actual,
            }
        }
    }

    /// Return minima accounted by [`ControlPlane::reserve_minima`].
    pub(crate) fn release_minima(&self, count: usize) {
        self.reserved_minima.fetch_sub(count, Ordering::AcqRel);
    }

    pub(crate) async fn register_worker(
        self: &Arc<Self>,
        mut child: Child,
        identity: WorkerIdentity,
        sock_path: PathBuf,
        listener: UnixListener,
        worker_handler: Option<MessageHandler>,
        slot: ProcessSlot,
    ) -> Result<LifecycleReceiver, RunpyError> {
        let worker_id = identity.name.clone();
        // The Manager verified at build that its socket directory is UTF-8.
        let socket_path = sock_path.to_string_lossy().into_owned();
        let process_group_id = child.id();
        let missing = |stream: &str| {
            RunpyError::Io(std::io::Error::other(format!(
                "worker {stream} pipe was not configured"
            )))
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => ChildStdout::from_std(stdout).map_err(RunpyError::Io),
            None => Err(missing("stdout")),
        };
        let stdout = match stdout {
            Ok(stdout) => stdout,
            Err(error) => {
                cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                return Err(error);
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => ChildStderr::from_std(stderr).map_err(RunpyError::Io),
            None => Err(missing("stderr")),
        };
        let stderr = match stderr {
            Ok(stderr) => stderr,
            Err(error) => {
                cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                return Err(error);
            }
        };
        let (tx, outbound_rx) = mpsc::channel::<OutboundFrame>(self.queue.outbound_capacity);
        let sender = MessageSender {
            tx,
            worker_id: worker_id.clone(),
            socket_path: socket_path.clone(),
            budget: self.budget.clone(),
            limits: self.limits,
            send_timeout: self.queue.send_timeout,
            closing: Arc::new(AtomicBool::new(false)),
        };

        // Registration and synchronous shutdown acquire sessions before
        // workers. Preserve that order to avoid deadlocking concurrent cleanup.
        // Insertion of the child and all task handles stays atomic with `closed`.
        let mut sessions = self.sessions.lock();
        let mut workers = self.workers.write();
        if self.closed.load(Ordering::Acquire) {
            drop(workers);
            drop(sessions);
            cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
            return Err(RunpyError::ManagerClosed);
        }
        // An identity already in use belongs to a live worker; never replace
        // it. Only the new, unregistered child is stopped.
        if workers.contains_key(&worker_id) || sessions.contains_key(&worker_id) {
            drop(workers);
            drop(sessions);
            cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
            return Err(RunpyError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("worker identity '{worker_id}' is already registered"),
            )));
        }

        let output_sender = {
            let mut dispatcher = self.output_dispatcher.lock();
            if dispatcher.is_none() {
                let new_dispatcher = match WorkerOutputDispatcher::new(
                    self.logger.clone(),
                    self.output_handler.clone(),
                    self.queue.mirror_worker_output,
                ) {
                    Ok(dispatcher) => dispatcher,
                    Err(error) => {
                        drop(dispatcher);
                        drop(workers);
                        drop(sessions);
                        cleanup_unregistered_worker(&mut child, process_group_id, &sock_path);
                        return Err(RunpyError::Io(std::io::Error::other(error)));
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
        let (lifecycle, observer) = lifecycle_channel();
        let (requests, request_queue) = mpsc::unbounded_channel();
        let active_request = ActiveSlot::default();
        let dispatcher = tokio::spawn(run_dispatcher(
            worker_id.clone(),
            sender.clone(),
            request_queue,
            active_request.clone(),
            observer.clone(),
        ));
        workers.insert(
            worker_id.clone(),
            WorkerHandle {
                child,
                identity,
                sock_path,
                sender,
                process_group_id,
                lifecycle: lifecycle.clone(),
                requests,
                _slot: slot,
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
            limits: self.limits,
            ended: self.ended_tx.clone(),
            lifecycle,
            active_request,
        }));
        let output_rate = self.queue.worker_output_rate;
        let stdout = tokio::spawn(capture_worker_output(
            stdout,
            worker_id.clone(),
            WorkerOutputStream::Stdout,
            output_sender.clone(),
            output_rate,
        ));
        let stderr = tokio::spawn(capture_worker_output(
            stderr,
            worker_id.clone(),
            WorkerOutputStream::Stderr,
            output_sender,
            output_rate,
        ));
        sessions.insert(
            worker_id,
            WorkerTasks {
                session,
                dispatcher,
                stdout,
                stderr,
            },
        );
        Ok(observer)
    }

    /// Admit a request for `worker_id`, waiting for a pending slot until the
    /// end-to-end deadline.
    pub(crate) async fn submit(
        &self,
        worker_id: &str,
        data: Data,
        timeout: Duration,
    ) -> Result<Request, RunpyError> {
        if timeout.is_zero() {
            return Err(RunpyError::InvalidConfig(
                "request timeout must be greater than zero".to_string(),
            ));
        }
        let deadline = Instant::now() + timeout;
        let queue = {
            let workers = self.workers.read();
            let handle = workers
                .get(worker_id)
                .ok_or_else(|| RunpyError::WorkerExited {
                    worker_id: worker_id.to_string(),
                })?;
            let supported = matches!(
                &*handle.lifecycle.borrow(),
                LifecycleState::Ready { capabilities }
                    if capabilities.iter().any(|capability| capability == "requests")
            );
            if !supported {
                return Err(RunpyError::Unsupported(format!(
                    "worker '{worker_id}' did not advertise the `requests` capability"
                )));
            }
            handle.requests.clone()
        };
        let permit = self.admit_request(deadline).await?;
        let id = self.next_request_id()?;
        let (request, pending) = Request::new(id, worker_id.to_string(), data, deadline, permit);
        queue.send(pending).map_err(|_| RunpyError::WorkerExited {
            worker_id: worker_id.to_string(),
        })?;
        Ok(request)
    }

    /// Reserve one Manager-wide pending-request slot within `deadline`.
    pub(crate) async fn admit_request(
        &self,
        deadline: Instant,
    ) -> Result<OwnedSemaphorePermit, RunpyError> {
        timeout_at(deadline, self.request_budget.clone().acquire_owned())
            .await
            .map_err(|_| RunpyError::Timeout(TimeoutStage::Admission))?
            .map_err(|_| RunpyError::ManagerClosed)
    }

    /// Reserve a pending-request slot without waiting.
    pub(crate) fn try_admit_request(&self) -> Result<OwnedSemaphorePermit, RunpyError> {
        self.request_budget
            .clone()
            .try_acquire_owned()
            .map_err(|error| match error {
                TryAcquireError::NoPermits => RunpyError::QueueFull,
                TryAcquireError::Closed => RunpyError::ManagerClosed,
            })
    }

    /// Hand an already-admitted request to a worker's dispatcher. If the
    /// worker is gone the request is dropped, which its waiter observes.
    pub(crate) fn enqueue(&self, worker_id: &str, pending: PendingRequest) {
        let queue = self
            .workers
            .read()
            .get(worker_id)
            .map(|handle| handle.requests.clone());
        if let Some(queue) = queue {
            let _ = queue.send(pending);
        }
    }

    /// Allocate a request ID, failing rather than wrapping when exhausted.
    pub(crate) fn next_request_id(&self) -> Result<RequestId, RunpyError> {
        // A compare-exchange loop rather than `fetch_update`, which newer
        // toolchains deprecate in favour of `try_update`.
        let mut id = self.next_request.load(Ordering::Acquire);
        loop {
            let next = id.checked_add(1).ok_or_else(|| {
                RunpyError::InvalidConfig("request ID space exhausted".to_string())
            })?;
            match self.next_request.compare_exchange_weak(
                id,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(RequestId::new(id)),
                Err(actual) => id = actual,
            }
        }
    }

    pub(crate) async fn send(&self, worker_id: &str, envelope: Envelope) -> Result<(), RunpyError> {
        self.mailer.route(worker_id)?.send(envelope).await
    }

    pub(crate) fn try_send(
        &self,
        worker_id: &str,
        envelope: Envelope,
    ) -> Result<SendReceipt, RunpyError> {
        self.mailer.route(worker_id)?.try_send(envelope)
    }

    /// Send to every registered worker concurrently and collect each result.
    ///
    /// One stuck route delays only its own entry. Concurrency is bounded by
    /// the number of registered workers.
    pub(crate) async fn broadcast(
        &self,
        envelope: Envelope,
    ) -> HashMap<String, Result<(), RunpyError>> {
        let routes: Vec<_> = self
            .workers
            .read()
            .iter()
            .map(|(worker_id, handle)| (worker_id.clone(), handle.sender.clone()))
            .collect();
        let mut sends = JoinSet::new();
        for (worker_id, sender) in routes {
            let envelope = envelope.clone();
            sends.spawn(async move { (worker_id, sender.send(envelope).await) });
        }
        let mut results = HashMap::with_capacity(sends.len());
        while let Some(joined) = sends.join_next().await {
            if let Ok((worker_id, result)) = joined {
                results.insert(worker_id, result);
            }
        }
        results
    }

    /// Request graceful termination, then stop the process group no later
    /// than the shutdown grace after this call started.
    ///
    /// New application sends are refused immediately. The process is killed
    /// at the deadline even when the `terminate` frame is stuck behind a
    /// saturated route or could not be written; an earlier exit ends the
    /// wait early. The result reports only whether `terminate` was written.
    pub(crate) async fn terminate_worker(&self, worker_id: &str) -> Result<(), RunpyError> {
        let deadline = Instant::now() + self.queue.shutdown_grace;
        let sender = self.mailer.route(worker_id)?;
        let request = sender.request_termination(deadline).await;
        self.wait_for_exit(worker_id, deadline).await;
        self.remove_worker(worker_id, ExitReason::Terminated).await;
        request
    }

    /// Terminate every registered worker under one shared grace deadline.
    pub(crate) async fn terminate_all(&self) -> HashMap<String, WorkerExit> {
        let deadline = Instant::now() + self.queue.shutdown_grace;
        let routes: Vec<_> = self
            .workers
            .read()
            .iter()
            .map(|(worker_id, handle)| {
                (
                    worker_id.clone(),
                    handle.sender.clone(),
                    handle.lifecycle.subscribe(),
                )
            })
            .collect();
        let mut requests = JoinSet::new();
        for (_, sender, _) in &routes {
            let sender = sender.clone();
            requests.spawn(async move {
                let _ = sender.request_termination(deadline).await;
            });
        }
        while requests.join_next().await.is_some() {}

        for (worker_id, _, _) in &routes {
            self.wait_for_exit(worker_id, deadline).await;
        }
        let mut exits = HashMap::with_capacity(routes.len());
        for (worker_id, _, lifecycle) in routes {
            self.remove_worker(&worker_id, ExitReason::Terminated).await;
            if let LifecycleState::Exited(exit) = &*lifecycle.borrow() {
                exits.insert(worker_id, exit.clone());
            }
        }
        exits
    }

    /// Stop a worker whose spawn was abandoned before it became ready.
    ///
    /// Called from a synchronous drop guard, so removal is handed to the
    /// monitor rather than awaited here.
    pub(crate) fn abandon(&self, worker_id: &str) {
        if let Some(handle) = self.workers.read().get(worker_id) {
            handle.sender.closing.store(true, Ordering::Release);
        }
        let _ = self
            .ended_tx
            .send((worker_id.to_string(), ExitReason::Terminated));
    }

    /// Wait until the worker's process exits, it is removed, or `deadline`.
    ///
    /// Polls `try_wait` without holding the registry lock across a sleep.
    pub(crate) async fn wait_for_exit(&self, worker_id: &str, deadline: Instant) {
        loop {
            let exited = match self.workers.write().get_mut(worker_id) {
                None => true,
                Some(handle) => !matches!(handle.child.try_wait(), Ok(None)),
            };
            if exited || Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until((Instant::now() + EXIT_POLL_INTERVAL).min(deadline)).await;
        }
    }

    /// Remove a worker once, stopping its process group and releasing its
    /// tasks, socket file, and route.
    ///
    /// Watchdog, session end, termination, and shutdown can race to remove
    /// the same worker; the first caller wins and records the reason, later
    /// callers return `false`. A worker whose termination was requested is
    /// recorded as terminated even if it exited or disconnected first. The
    /// blocking kill-and-reap runs off the async runtime threads.
    pub(crate) async fn remove_worker(&self, worker_id: &str, reason: ExitReason) -> bool {
        let tasks = self.sessions.lock().remove(worker_id);
        if let Some(tasks) = &tasks {
            tasks.session.abort();
            tasks.dispatcher.abort();
        }

        let handle = self.workers.write().remove(worker_id);
        let removed = handle.is_some();
        if let Some(mut handle) = handle {
            let reason = if !matches!(
                reason,
                ExitReason::ManagerShutdown | ExitReason::StartupTimeout
            ) && handle.sender.closing.load(Ordering::Acquire)
            {
                ExitReason::Terminated
            } else {
                reason
            };
            let name = handle.identity.name.clone();
            let lifecycle = handle.lifecycle.clone();
            let stopped = tokio::task::spawn_blocking(move || {
                let status = force_stop_worker(&mut handle);
                let _ = std::fs::remove_file(&handle.sock_path);
                status
            })
            .await;
            let status = stopped.unwrap_or_else(|error| {
                self.logger.error_with(
                    "ControlPlane",
                    &format!("Failed to stop worker '{name}': {error}"),
                );
                None
            });
            lifecycle.send_replace(LifecycleState::Exited(WorkerExit::new(
                name.clone(),
                status,
                reason,
            )));
            self.logger.info_with(
                "ControlPlane",
                &format!("Removed worker '{name}' ({})", reason.as_str()),
            );
        }

        if let Some(tasks) = tasks {
            Self::finish_output_task(tasks.stdout).await;
            Self::finish_output_task(tasks.stderr).await;
        }
        removed
    }

    async fn finish_output_task(mut task: JoinHandle<()>) {
        if timeout(OUTPUT_DRAIN_TIMEOUT, &mut task).await.is_err() {
            task.abort();
        }
    }

    pub(crate) fn shutdown_now(&self) {
        self.closed.store(true, Ordering::Release);
        // Waiting spawns fail with `ManagerClosed` instead of starting.
        self.processes.close();
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
            tasks.dispatcher.abort();
            tasks.stdout.abort();
            tasks.stderr.abort();
        }
        for (_, mut handle) in workers.drain() {
            let status = force_stop_worker(&mut handle);
            let _ = std::fs::remove_file(&handle.sock_path);
            handle
                .lifecycle
                .send_replace(LifecycleState::Exited(WorkerExit::new(
                    handle.identity.name.clone(),
                    status,
                    ExitReason::ManagerShutdown,
                )));
        }
    }

    /// Serve the worker's single connection, then report why it ended.
    ///
    /// There is no reconnect: once the connection closes or violates the
    /// protocol, the monitor removes the worker, which fails pending sends
    /// and stops the process group. The session never removes itself.
    async fn run_session(session: WorkerSession) {
        let worker_id = session.worker_id.clone();
        let ended = session.ended.clone();
        let reason = Self::serve_connection(session).await;
        let _ = ended.send((worker_id, reason));
    }

    async fn serve_connection(session: WorkerSession) -> ExitReason {
        let WorkerSession {
            control_plane,
            listener,
            worker_id,
            socket_path,
            worker_handler,
            outbound_rx,
            mailer,
            logger,
            limits,
            ended: _,
            lifecycle,
            active_request,
        } = session;
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                logger.error_with(
                    "ControlPlane",
                    &format!("{worker_id} Accept error: {error}"),
                );
                return ExitReason::Disconnected;
            }
        };
        drop(listener);

        // A dedicated task owns every receive operation until it completes,
        // so selecting on outbound work can never drop a partially read
        // frame. Only complete envelopes cross this bounded channel.
        let (mut read_half, write_half) = stream.into_split();
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<Envelope>(SESSION_QUEUE_CAPACITY);
        let reader_logger = logger.clone();
        let reader = tokio::spawn(async move {
            loop {
                match Self::recv_envelope(&reader_logger, &mut read_half, &limits).await {
                    Ok(envelope) => {
                        if inbound_tx.send(envelope).await.is_err() {
                            return ExitReason::Disconnected;
                        }
                    }
                    Err(reason) => return reason,
                }
            }
        });
        // Aborting the session (cleanup) drops this guard and the reader.
        let mut reader = AbortOnDrop(reader);

        // The writer owns the write half so a write blocked on a busy peer
        // never stops inbound dispatch. A partially written frame is never
        // retried: either task ending tears down the whole connection.
        let writer = tokio::spawn(Self::run_writer(
            worker_id.clone(),
            write_half,
            outbound_rx,
            logger.clone(),
        ));
        let mut writer = AbortOnDrop(writer);

        let mut ready = false;
        loop {
            tokio::select! {
                received = inbound_rx.recv() => {
                    let Some(mut envelope) = received else {
                        return (&mut reader.0).await.unwrap_or(ExitReason::Disconnected);
                    };
                    if let Err(error) = envelope.validate_worker_to_manager() {
                        logger.error_with("Protocol", &error.to_string());
                        return ExitReason::ProtocolViolation;
                    }
                    // The first envelope must be `ready` and nothing may
                    // repeat it. Readiness is lifecycle state, consumed here
                    // and published to handles, not dispatched to handlers.
                    let is_ready = envelope.operation() == Some("ready");
                    if is_ready == ready {
                        let problem = if ready {
                            "sent `ready` more than once".to_string()
                        } else {
                            format!(
                                "sent {} before `ready`",
                                envelope.operation().unwrap_or("a custom envelope")
                            )
                        };
                        logger.error_with("Protocol", &format!("{worker_id} {problem}"));
                        return ExitReason::ProtocolViolation;
                    }
                    if is_ready {
                        match parse_ready(envelope.data()) {
                            Ok(capabilities) => {
                                ready = true;
                                lifecycle.send_replace(LifecycleState::Ready { capabilities });
                                continue;
                            }
                            Err(error) => {
                                logger.error_with("Protocol", &format!("{worker_id}: {error}"));
                                return ExitReason::ProtocolViolation;
                            }
                        }
                    }
                    envelope.stamp_trusted(&worker_id, &socket_path);
                    let envelope = Arc::new(envelope);

                    // Request results complete their request before any
                    // observer runs. An `error` without `x_req` reports a
                    // custom-handler failure and only reaches observers.
                    let operation = envelope.operation();
                    if operation == Some("cancelled")
                        && let Err(problem) = Self::complete_cancellation(&envelope, &active_request)
                    {
                        logger.error_with("Protocol", &format!("{worker_id} {problem}"));
                        return ExitReason::ProtocolViolation;
                    }
                    if matches!(operation, Some("done" | "error")) {
                        match envelope.request_id() {
                            Some(id) => {
                                let active = active_request.lock().take();
                                let Some(active) = active.filter(|active| active.id.as_str() == id)
                                else {
                                    logger.error_with(
                                        "Protocol",
                                        &format!("{worker_id} answered unknown request '{id}'"),
                                    );
                                    return ExitReason::ProtocolViolation;
                                };
                                let completion = if operation == Some("done") {
                                    Completion::Done(envelope.clone())
                                } else {
                                    Completion::Failed(RemoteError::from_data(envelope.data()))
                                };
                                let _ = active.complete.send(completion);
                            }
                            None if operation == Some("done") => {
                                logger.error_with(
                                    "Protocol",
                                    &format!("{worker_id} sent `done` without a request ID"),
                                );
                                return ExitReason::ProtocolViolation;
                            }
                            None => {}
                        }
                    }

                    let inbound = InboundEnvelope {
                        envelope,
                        worker_id: worker_id.clone(),
                        mailer: mailer.clone(),
                    };
                    let Some(control_plane) = control_plane.upgrade() else {
                        return ExitReason::ManagerShutdown;
                    };
                    let global_handler = control_plane.global_handler.read().clone();
                    drop(control_plane);
                    if let Some(handler) = global_handler {
                        Self::dispatch_inbound(&logger, &worker_id, "global", &handler, inbound.clone());
                    }
                    if let Some(handler) = &worker_handler {
                        Self::dispatch_inbound(&logger, &worker_id, "worker", handler, inbound);
                    }
                }
                _ = &mut writer.0 => return ExitReason::Disconnected,
            }
        }
    }

    /// Apply a worker's `cancelled` answer.
    ///
    /// `cancelled` completes the matching active request and confirms its
    /// canceller. `already_finished` only acknowledges the `cancel`: the
    /// request's own `done`/`error` came first, so it must not match (and
    /// complete) the active request, which may be a newer one.
    fn complete_cancellation(envelope: &Envelope, active: &ActiveSlot) -> Result<(), String> {
        let Some(id) = envelope.request_id() else {
            return Err("sent `cancelled` without a request ID".to_string());
        };
        let mut slot = active.lock();
        let matches_active = slot.as_ref().is_some_and(|active| active.id.as_str() == id);
        match envelope.data().get("status").and_then(Value::as_str) {
            Some("cancelled") if matches_active => {
                let active = slot.take().expect("matched an active request");
                if let Some(confirm) = active.cancel {
                    let _ = confirm.send(());
                }
                let _ = active.complete.send(Completion::Cancelled);
                Ok(())
            }
            Some("cancelled") => Err(format!("cancelled request '{id}' that was not running")),
            Some("already_finished") if matches_active => Err(format!(
                "reported request '{id}' as finished without its result"
            )),
            Some("already_finished") => Ok(()),
            _ => Err("sent `cancelled` without a valid status".to_string()),
        }
    }

    /// Invoke one inbound handler, containing a panic so it cannot end the
    /// session or skip the next handler and later envelopes.
    fn dispatch_inbound(
        logger: &Scribbler,
        worker_id: &str,
        kind: &str,
        handler: &MessageHandler,
        inbound: InboundEnvelope,
    ) {
        if catch_unwind(AssertUnwindSafe(|| handler(inbound))).is_err() {
            logger.error_with(
                "Protocol",
                &format!(
                    "Inbound message handler panicked; continuing dispatch \
                     (handler: {kind}, worker: {worker_id})"
                ),
            );
        }
    }

    /// Write admitted frames in order until the route closes or a write fails.
    ///
    /// A frame whose deadline passed while queued is discarded unwritten. A
    /// deadline that interrupts a write leaves delivery unknown, so the
    /// connection closes rather than continue on a possibly corrupted stream.
    /// Frames still queued when the connection ends are dropped, which fails
    /// their receipts.
    async fn run_writer(
        worker_id: String,
        mut write_half: tokio::net::unix::OwnedWriteHalf,
        mut outbound_rx: mpsc::Receiver<OutboundFrame>,
        logger: Arc<Scribbler>,
    ) {
        while let Some(entry) = outbound_rx.recv().await {
            let OutboundFrame {
                frame,
                deadline,
                ack,
                _budget,
            } = entry;
            if Instant::now() >= deadline {
                let _ = ack.send(Err(RunpyError::Timeout(TimeoutStage::Queued)));
                continue;
            }
            match timeout_at(deadline, write_frame(&mut write_half, &frame)).await {
                Ok(Ok(())) => {
                    let _ = ack.send(Ok(()));
                }
                Ok(Err(error)) => {
                    logger.error_with("ControlPlane", &format!("{worker_id} Send error: {error}"));
                    let _ = ack.send(Err(RunpyError::Io(error)));
                    return;
                }
                Err(_) => {
                    logger.error_with(
                        "ControlPlane",
                        &format!("{worker_id} Send error: write deadline expired mid-frame"),
                    );
                    let _ = ack.send(Err(RunpyError::Timeout(TimeoutStage::Write)));
                    return;
                }
            }
        }
        // Every sender is gone, so the worker is being removed; keep the
        // connection open until cleanup aborts the session.
        std::future::pending::<()>().await;
    }

    /// Read and decode one envelope, or classify why the connection ends.
    ///
    /// A clean close or I/O failure is a disconnect; a malformed, oversized,
    /// stalled, or undecodable frame is a protocol violation.
    async fn recv_envelope<R>(
        logger: &Scribbler,
        stream: &mut R,
        limits: &ProtocolLimits,
    ) -> Result<Envelope, ExitReason>
    where
        R: AsyncRead + Unpin,
    {
        let payload = match read_frame(stream, limits).await {
            Ok(Some(payload)) => payload,
            Ok(None) | Err(FrameError::Truncated) => return Err(ExitReason::Disconnected),
            Err(FrameError::Io(error)) => {
                match error.kind() {
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
                return Err(ExitReason::Disconnected);
            }
            Err(error) => {
                logger.error_with("Protocol", &error.to_string());
                return Err(ExitReason::ProtocolViolation);
            }
        };

        // Strict profile first (depth scan, duplicate keys), then the
        // envelope's own shape and metadata validation.
        crate::strict_json::parse_strict(&payload)
            .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()))
            .map_err(|error| {
                logger.error_with("Protocol", &invalid_body_diagnostic(&error, &payload));
                ExitReason::ProtocolViolation
            })
    }
}

/// Size of the little-endian length header that precedes every frame body.
const FRAME_HEADER_BYTES: usize = 8;
/// Most bytes of an invalid body included in its diagnostic.
const INVALID_BODY_EXCERPT: usize = 1024;

/// Diagnostic for an undecodable body: the error, the body length, and at
/// most [`INVALID_BODY_EXCERPT`] sanitized bytes. The body may be large,
/// binary, contain terminal control sequences, or carry sensitive data, so it
/// is never logged whole.
fn invalid_body_diagnostic(error: &dyn fmt::Display, payload: &[u8]) -> String {
    let excerpt = &payload[..payload.len().min(INVALID_BODY_EXCERPT)];
    format!(
        "JSON envelope error: {error} ({} bytes); first {} bytes: {}",
        payload.len(),
        excerpt.len(),
        sanitize_output(excerpt)
    )
}

/// Why a frame could not be read or encoded.
#[derive(Debug)]
enum FrameError {
    Io(std::io::Error),
    Empty,
    TooLarge { size: u64, limit: usize },
    Truncated,
    TimedOut,
    Encode(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "socket I/O failed: {error}"),
            Self::Empty => write!(f, "frame declares an empty body"),
            Self::TooLarge { size, limit } => {
                write!(
                    f,
                    "frame body of {size} bytes exceeds the {limit}-byte limit"
                )
            }
            Self::Truncated => write!(f, "connection closed in the middle of a frame"),
            Self::TimedOut => write!(f, "frame did not complete before the frame timeout"),
            Self::Encode(error) => write!(f, "envelope could not be encoded: {error}"),
        }
    }
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(error)
        }
    }
}

/// Read one frame body, enforcing size and partial-frame deadline.
///
/// Returns `Ok(None)` for a clean close before any byte. Waiting for the
/// first byte is unbounded; once it arrives the rest of the frame must finish
/// within `frame_timeout`. The declared length is validated before any body
/// allocation, so a hostile header cannot exhaust memory.
async fn read_frame<R>(
    stream: &mut R,
    limits: &ProtocolLimits,
) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    loop {
        match stream.read(&mut header[..1]).await {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        }
    }

    let rest = async {
        stream.read_exact(&mut header[1..]).await?;
        let size = u64::from_le_bytes(header);
        if size == 0 {
            return Err(FrameError::Empty);
        }
        let length = usize::try_from(size)
            .ok()
            .filter(|length| *length <= limits.max_frame_bytes())
            .ok_or(FrameError::TooLarge {
                size,
                limit: limits.max_frame_bytes(),
            })?;
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await?;
        Ok(payload)
    };
    match timeout(limits.frame_timeout(), rest).await {
        Ok(result) => result.map(Some),
        Err(_) => Err(FrameError::TimedOut),
    }
}

/// `io::Write` sink that refuses to grow a frame past its body limit.
struct LimitedFrame {
    buffer: Vec<u8>,
    limit: usize,
}

impl std::io::Write for LimitedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len() - FRAME_HEADER_BYTES + bytes.len() > self.limit {
            return Err(std::io::Error::other("frame limit exceeded"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialize header and body into one buffer without exceeding `limit`.
///
/// Serialization stops at the limit instead of materializing an arbitrarily
/// large body first.
fn encode_frame(envelope: &Envelope, limit: usize) -> Result<Vec<u8>, FrameError> {
    let mut sink = LimitedFrame {
        buffer: vec![0_u8; FRAME_HEADER_BYTES],
        limit,
    };
    if let Err(error) = serde_json::to_writer(&mut sink, envelope) {
        return Err(if error.is_io() {
            FrameError::TooLarge {
                size: (sink.buffer.len() - FRAME_HEADER_BYTES) as u64,
                limit,
            }
        } else {
            FrameError::Encode(error)
        });
    }
    let size = (sink.buffer.len() - FRAME_HEADER_BYTES) as u64;
    sink.buffer[..FRAME_HEADER_BYTES].copy_from_slice(&size.to_le_bytes());
    Ok(sink.buffer)
}

/// Write one pre-encoded frame with a single buffered write.
async fn write_frame<W>(stream: &mut W, frame: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    stream.write_all(frame).await?;
    stream.flush().await
}

/// Resolves trusted worker IDs to their bounded outbound routes.
struct Mailer {
    workers: Arc<RwLock<HashMap<String, WorkerHandle>>>,
}

impl Mailer {
    fn route(&self, worker_id: &str) -> Result<MessageSender, RunpyError> {
        self.workers
            .read()
            .get(worker_id)
            .map(|handle| handle.sender.clone())
            .ok_or_else(|| RunpyError::WorkerExited {
                worker_id: worker_id.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::wait_for_exit;
    use serde_json::json;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc as std_mpsc;
    use tokio::net::UnixStream;
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

    fn test_control_plane(limits: ProtocolLimits) -> Arc<ControlPlane> {
        ControlPlane::new(test_logger(), limits, QueueLimits::default())
    }

    /// Register a `sleep` child and connect to its socket without `ready`.
    async fn register_unready(
        control_plane: &Arc<ControlPlane>,
        directory: &std::path::Path,
        worker_id: &str,
        handler: Option<MessageHandler>,
    ) -> (UnixStream, LifecycleReceiver) {
        let sock_path = directory.join(format!("{worker_id}.sock"));
        let listener = UnixListener::bind(&sock_path).unwrap();
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

        let lifecycle = control_plane
            .register_worker(
                child,
                identity,
                sock_path.clone(),
                listener,
                handler,
                test_slot(),
            )
            .await
            .unwrap();
        (UnixStream::connect(sock_path).await.unwrap(), lifecycle)
    }

    /// A slot from a private semaphore, so plane-level tests are not bound by
    /// the Manager-wide process limit they do not exercise.
    fn test_slot() -> ProcessSlot {
        Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()
    }

    fn ready_envelope(protocol_version: u64) -> Envelope {
        serde_json::from_value(json!({
            "meta": {"x_op": "ready"},
            "data": {"protocol_version": protocol_version, "capabilities": ["requests", "extra"]}
        }))
        .unwrap()
    }

    /// Register a worker that has announced `ready`.
    async fn register_worker(
        control_plane: &Arc<ControlPlane>,
        directory: &std::path::Path,
        worker_id: &str,
        handler: Option<MessageHandler>,
    ) -> UnixStream {
        let (mut stream, mut lifecycle) =
            register_unready(control_plane, directory, worker_id, handler).await;
        write_envelope(&mut stream, &ready_envelope(1)).await;
        timeout(
            Duration::from_secs(2),
            lifecycle.wait_for(|state| matches!(state, LifecycleState::Ready { .. })),
        )
        .await
        .unwrap()
        .unwrap();
        stream
    }

    async fn write_envelope(stream: &mut UnixStream, envelope: &Envelope) {
        let payload = serde_json::to_vec(envelope).unwrap();
        stream
            .write_all(&(payload.len() as u64).to_le_bytes())
            .await
            .unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn read_envelope(stream: &mut UnixStream) -> Envelope {
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
            None,
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

    fn json_values() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::from),
            any::<u64>().prop_map(Value::from),
            any::<f64>()
                .prop_filter("finite", |value| value.is_finite())
                .prop_map(Value::from),
            any::<String>().prop_map(Value::String),
        ];
        leaf.prop_recursive(6, 48, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::btree_map(any::<String>(), inner, 0..4)
                    .prop_map(|fields| Value::Object(fields.into_iter().collect())),
            ]
        })
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 128,
            failure_persistence: None,
            ..proptest::test_runner::Config::default()
        })]

        /// Any strict-profile envelope survives encoding, arbitrary write
        /// segmentation, framing, and strict decoding unchanged.
        #[test]
        fn frames_round_trip_under_arbitrary_segmentation(
            data in proptest::collection::btree_map(proptest::prelude::any::<String>(), json_values(), 0..5),
            cuts in proptest::collection::vec(1_usize..17, 1..40),
        ) {
            let envelope = Envelope::new(Meta::new(), data.into_iter().collect()).unwrap();
            let frame = encode_frame(&envelope, 1 << 20).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let payload = runtime.block_on(async {
                let (mut writer, mut reader) = tokio::io::duplex(64);
                let sender = tokio::spawn(async move {
                    let mut rest = frame.as_slice();
                    for cut in cuts.iter().cycle() {
                        if rest.is_empty() {
                            break;
                        }
                        let (chunk, tail) = rest.split_at((*cut).min(rest.len()));
                        writer.write_all(chunk).await.unwrap();
                        tokio::task::yield_now().await;
                        rest = tail;
                    }
                });
                let payload = read_frame(&mut reader, &ProtocolLimits::default())
                    .await
                    .unwrap()
                    .unwrap();
                sender.await.unwrap();
                payload
            });
            let decoded: Envelope =
                serde_json::from_value(crate::strict_json::parse_strict(&payload).unwrap()).unwrap();
            proptest::prop_assert_eq!(decoded.data(), envelope.data());
        }
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn frame_reader_accepts_exactly_the_shared_frames() {
        let vectors: Value =
            serde_json::from_str(include_str!("../../tests/protocol_vectors.json")).unwrap();
        for frame in vectors["frames"].as_array().unwrap() {
            let bytes = hex(frame["frame_hex"].as_str().unwrap());
            let accepted = matches!(
                read_frame(&mut bytes.as_slice(), &ProtocolLimits::default()).await,
                Ok(Some(_))
            );
            assert_eq!(accepted, frame["valid"], "frame {}", frame["name"]);
        }
    }

    /// Body for a "depth" vector: an envelope nested `depth` levels deep.
    fn deep_body(depth: usize) -> String {
        format!(
            r#"{{"meta":{{}},"data":{{"v":{}{}}}}}"#,
            "[".repeat(depth - 2),
            "]".repeat(depth - 2)
        )
    }

    #[test]
    fn manager_accepts_exactly_the_shared_worker_vectors() {
        let vectors: Value =
            serde_json::from_str(include_str!("../../tests/protocol_vectors.json")).unwrap();
        let mut checked = 0;
        for vector in vectors["vectors"].as_array().unwrap() {
            if vector["direction"] != "worker_to_manager" {
                continue;
            }
            let body: Vec<u8> = if let Some(depth) = vector["depth"].as_u64() {
                deep_body(depth as usize).into_bytes()
            } else if let Some(text) = vector["body_hex"].as_str() {
                hex(text)
            } else {
                vector["body"].as_str().unwrap().as_bytes().to_vec()
            };
            let accepted = crate::strict_json::parse_strict(&body)
                .ok()
                .and_then(|value| serde_json::from_value::<Envelope>(value).ok())
                .is_some_and(|envelope| envelope.validate_worker_to_manager().is_ok());
            assert_eq!(accepted, vector["valid"], "vector {}", vector["name"]);
            checked += 1;
        }
        assert!(checked > 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ambiguous_or_too_deep_inbound_json_is_a_violation() {
        let mut too_deep = String::from(r#"{"meta": {}, "data": {"a": "#);
        too_deep.push_str(&"[".repeat(crate::strict_json::MAX_JSON_DEPTH));
        too_deep.push_str(&"]".repeat(crate::strict_json::MAX_JSON_DEPTH));
        too_deep.push_str("}}");
        for (name, body) in [
            (
                "duplicate",
                r#"{"meta": {"k": 1, "k": 2}, "data": {}}"#.to_string(),
            ),
            ("deep", too_deep),
        ] {
            let directory = tempfile::TempDir::new().unwrap();
            let control_plane = test_control_plane(ProtocolLimits::default());
            let mut stream = register_worker(&control_plane, directory.path(), name, None).await;
            stream
                .write_all(&(body.len() as u64).to_le_bytes())
                .await
                .unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            wait_for_removal(&control_plane, name).await;
            control_plane.shutdown_now();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn too_deep_outbound_envelopes_are_rejected_before_writing() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;
        let mut value = json!(1);
        // Envelope (1) + data (2) + 62 nested arrays reaches the limit.
        for _ in 0..crate::strict_json::MAX_JSON_DEPTH - 2 {
            value = json!([value]);
        }
        let at_limit = Envelope::new(Meta::new(), object(json!({ "v": value.clone() }))).unwrap();
        let too_deep = Envelope::new(Meta::new(), object(json!({ "v": [value] }))).unwrap();

        assert!(matches!(
            control_plane.send("worker", too_deep).await,
            Err(RunpyError::InvalidEnvelope(EnvelopeError::TooDeep))
        ));
        control_plane.send("worker", at_limit).await.unwrap();
        let received = timeout(Duration::from_secs(2), read_envelope(&mut stream))
            .await
            .unwrap();
        assert!(received.data().contains_key("v"));
        control_plane.shutdown_now();
    }

    #[test]
    fn invalid_body_diagnostics_are_bounded_and_escaped() {
        let mut payload = b"{\"meta\": \x1b[31m\xff".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 10_000));
        let error = serde_json::from_slice::<Envelope>(&payload).unwrap_err();
        let diagnostic = invalid_body_diagnostic(&error, &payload);

        assert!(diagnostic.contains(&format!("({} bytes)", payload.len())));
        assert!(!diagnostic.contains('\x1b'), "{diagnostic}");
        assert!(diagnostic.contains("\\u{1b}[31m\u{fffd}"), "{diagnostic}");
        let excerpt_x = diagnostic.matches('x').count();
        assert!(excerpt_x <= INVALID_BODY_EXCERPT, "excerpt was not bounded");
        assert!(diagnostic.len() < 2 * INVALID_BODY_EXCERPT + 200);
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
            None,
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

    fn limits(max_frame_bytes: usize, frame_timeout: Duration) -> ProtocolLimits {
        ProtocolLimits::new(max_frame_bytes, frame_timeout).unwrap()
    }

    fn header(size: u64) -> [u8; 8] {
        size.to_le_bytes()
    }

    #[tokio::test]
    async fn frame_reader_accepts_bodies_up_to_the_limit() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let limits = limits(16, Duration::from_secs(1));
        writer.write_all(&header(16)).await.unwrap();
        writer.write_all(&[b'a'; 16]).await.unwrap();
        writer.write_all(&header(1)).await.unwrap();
        writer.write_all(b"b").await.unwrap();
        drop(writer);

        assert_eq!(
            read_frame(&mut reader, &limits).await.unwrap().unwrap(),
            [b'a'; 16]
        );
        assert_eq!(
            read_frame(&mut reader, &limits).await.unwrap().unwrap(),
            b"b"
        );
        assert!(read_frame(&mut reader, &limits).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_reader_rejects_empty_and_oversized_headers_before_allocating() {
        for (size, expect_too_large) in [(0_u64, false), (17, true), (u64::MAX, true)] {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            // No body follows: an oversized header must fail immediately
            // rather than wait for (or allocate) the declared body.
            writer.write_all(&header(size)).await.unwrap();
            let result = timeout(
                Duration::from_secs(1),
                read_frame(&mut reader, &limits(16, Duration::from_secs(30))),
            )
            .await
            .expect("header validation must not wait for a body");
            match result {
                Err(FrameError::TooLarge {
                    size: rejected,
                    limit,
                }) => {
                    assert!(expect_too_large);
                    assert_eq!((rejected, limit), (size, 16));
                }
                Err(FrameError::Empty) => assert!(!expect_too_large),
                other => panic!("unexpected result for size {size}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn frame_reader_reports_truncation_at_every_boundary() {
        let mut frame = header(4).to_vec();
        frame.extend_from_slice(b"body");
        for cut in 1..frame.len() {
            let (mut writer, mut reader) = tokio::io::duplex(64);
            writer.write_all(&frame[..cut]).await.unwrap();
            drop(writer);
            let result = read_frame(&mut reader, &limits(16, Duration::from_secs(1))).await;
            assert!(
                matches!(result, Err(FrameError::Truncated)),
                "cut at {cut}: {result:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn frame_deadline_starts_at_first_byte_and_covers_header_and_body() {
        let limits = limits(16, Duration::from_secs(5));

        // Stalled header.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&header(4)[..3]).await.unwrap();
        let result = read_frame(&mut reader, &limits).await;
        assert!(matches!(result, Err(FrameError::TimedOut)), "{result:?}");

        // Stalled body.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&header(4)).await.unwrap();
        writer.write_all(b"bo").await.unwrap();
        let result = read_frame(&mut reader, &limits).await;
        assert!(matches!(result, Err(FrameError::TimedOut)), "{result:?}");
        drop(writer);

        // An idle connection far longer than the deadline is still healthy.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let read = tokio::spawn(async move { read_frame(&mut reader, &limits).await });
        tokio::time::sleep(Duration::from_secs(600)).await;
        writer.write_all(&header(2)).await.unwrap();
        writer.write_all(b"ok").await.unwrap();
        assert_eq!(read.await.unwrap().unwrap().unwrap(), b"ok");
    }

    #[test]
    fn frame_encoder_writes_one_buffer_and_stops_at_the_limit() {
        let envelope =
            Envelope::new(Meta::new(), object(json!({ "blob": "x".repeat(64) }))).unwrap();
        let body = serde_json::to_vec(&envelope).unwrap();

        let frame = encode_frame(&envelope, body.len()).unwrap();
        assert_eq!(frame[..8], header(body.len() as u64));
        assert_eq!(frame[8..], body[..]);

        let error = encode_frame(&envelope, body.len() - 1).unwrap_err();
        assert!(matches!(error, FrameError::TooLarge { limit, .. } if limit == body.len() - 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_outbound_envelope_is_rejected_without_closing_the_connection() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(limits(256, Duration::from_secs(5)));
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        let oversized =
            Envelope::new(Meta::new(), object(json!({ "blob": "x".repeat(1024) }))).unwrap();
        let small = Envelope::new(Meta::new(), object(json!({ "ok": true }))).unwrap();
        assert!(matches!(
            control_plane.send("worker", oversized).await,
            Err(RunpyError::FrameTooLarge { limit: 256, .. })
        ));
        control_plane.send("worker", small).await.unwrap();

        let received = timeout(Duration::from_secs(1), read_envelope(&mut stream))
            .await
            .unwrap();
        assert_eq!(received.data()["ok"], true);
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_inbound_header_closes_the_connection_without_dispatch() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(limits(256, Duration::from_secs(5)));
        let (called_tx, called_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |_| {
            called_tx.send(()).unwrap();
        });
        let mut stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;

        stream.write_all(&header(u64::MAX)).await.unwrap();
        let mut byte = [0_u8; 1];
        let read = timeout(Duration::from_secs(1), stream.read(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0, "the Manager must close the connection");
        assert!(called_rx.try_recv().is_err());
        control_plane.shutdown_now();
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
            None,
        );
        enqueue_worker_output(
            &tx,
            "worker",
            WorkerOutputStream::Stdout,
            b"dropped",
            false,
            &mut dropped,
            None,
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
            None,
        );
        let next = rx.recv().unwrap();
        assert_eq!(next.line, "next");
        assert_eq!(next.dropped_lines_before, 1);
        assert_eq!(dropped, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn output_over_the_byte_budget_is_dropped_and_counted() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (tx, rx) = sync_channel(64);
        let task = tokio::spawn(capture_worker_output(
            reader,
            "worker".to_string(),
            WorkerOutputStream::Stderr,
            tx,
            Some(100),
        ));

        // Ten 49-byte records at once: the one-second burst of 100 bytes
        // admits two, and the other eight are dropped without blocking.
        for _ in 0..10 {
            writer.write_all(&[b'a'; 49]).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        }
        // The budget refills over (virtual) time; the next record reports
        // every record lost in between.
        tokio::time::sleep(Duration::from_secs(1)).await;
        writer.write_all(b"late\n").await.unwrap();
        drop(writer);
        task.await.unwrap();

        let records: Vec<_> = rx.try_iter().collect();
        assert_eq!(records.len(), 3, "{records:?}");
        assert!(
            records[..2]
                .iter()
                .all(|record| record.dropped_lines_before == 0)
        );
        assert_eq!(records[2].line, "late");
        assert_eq!(records[2].dropped_lines_before, 8);
    }

    #[test]
    fn output_dispatcher_continues_after_handler_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let (observed_tx, observed_rx) = std_mpsc::channel();
        let handler: WorkerOutputHandler = Arc::new(move |output| {
            if handler_calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                panic!("intentional output handler panic");
            }
            observed_tx.send(output.line).unwrap();
        });
        let dispatcher =
            WorkerOutputDispatcher::new(test_logger(), Arc::new(RwLock::new(Some(handler))), false)
                .unwrap();
        for line in ["first", "second"] {
            dispatcher
                .sender
                .send(WorkerOutput {
                    worker_id: "worker".to_string(),
                    stream: WorkerOutputStream::Stdout,
                    line: line.to_string(),
                    truncated: false,
                    dropped_lines_before: 0,
                })
                .unwrap();
        }

        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "second"
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
        dispatcher.close();
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
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut first = register_worker(&control_plane, directory.path(), "worker-a", None).await;
        let mut second = register_worker(&control_plane, directory.path(), "worker-b", None).await;
        let (observed_tx, observed_rx) = std_mpsc::channel();

        control_plane.set_global_handler(Arc::new(move |inbound| {
            observed_tx
                .send(Envelope::clone(&inbound.envelope))
                .unwrap();
            let source = inbound.envelope.data()["source"].clone();
            let _ = inbound
                .reply(Envelope::new(Meta::new(), object(json!({ "source": source }))).unwrap())
                .unwrap();
        }));

        for (stream, source) in [(&mut first, "first"), (&mut second, "second")] {
            let inbound: Envelope = serde_json::from_value(json!({
                "meta": {
                    "x_op": "log",
                    "x_wid": "spoofed",
                    "x_spath": "/tmp/spoofed.sock"
                },
                "data": {"source": source}
            }))
            .unwrap();
            write_envelope(stream, &inbound).await;
        }

        let first_reply = timeout(Duration::from_secs(1), read_envelope(&mut first))
            .await
            .unwrap();
        let second_reply = timeout(Duration::from_secs(1), read_envelope(&mut second))
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
    async fn handler_panics_do_not_end_the_session_or_skip_handlers() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let global_calls = Arc::new(AtomicUsize::new(0));
        let calls = global_calls.clone();
        control_plane.set_global_handler(Arc::new(move |_| {
            if calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                panic!("intentional global handler panic");
            }
        }));
        let (observed_tx, observed_rx) = std_mpsc::channel();
        let worker_handler: MessageHandler = Arc::new(move |inbound| {
            let sequence = inbound.envelope.data()["sequence"].as_u64().unwrap();
            observed_tx.send(sequence).unwrap();
            if sequence == 2 {
                panic!("intentional worker handler panic");
            }
        });
        let mut stream = register_worker(
            &control_plane,
            directory.path(),
            "worker",
            Some(worker_handler),
        )
        .await;
        let _other = register_worker(&control_plane, directory.path(), "other", None).await;

        for sequence in 1..=3 {
            let envelope: Envelope = serde_json::from_value(json!({
                "meta": {"x_op": "log"},
                "data": {"sequence": sequence}
            }))
            .unwrap();
            write_envelope(&mut stream, &envelope).await;
        }

        // The global panic did not skip the worker handler, and the worker
        // panic did not stop dispatch of the next envelope.
        for expected in 1..=3 {
            assert_eq!(
                observed_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                expected
            );
        }
        assert_eq!(global_calls.load(AtomicOrdering::SeqCst), 3);
        assert!(control_plane.workers.read().contains_key("worker"));
        assert!(control_plane.workers.read().contains_key("other"));
        control_plane.send("worker", numbered(4)).await.unwrap();
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ready_is_consumed_as_lifecycle_state_not_dispatched() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (observed_tx, observed_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |inbound| {
            observed_tx
                .send(inbound.envelope.meta().get("x_op").cloned())
                .unwrap();
        });
        let (mut stream, mut lifecycle) =
            register_unready(&control_plane, directory.path(), "worker", Some(handler)).await;
        assert!(matches!(*lifecycle.borrow(), LifecycleState::Starting));

        write_envelope(&mut stream, &ready_envelope(1)).await;
        let state = timeout(
            Duration::from_secs(2),
            lifecycle.wait_for(|state| !matches!(state, LifecycleState::Starting)),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        match state {
            LifecycleState::Ready { capabilities } => {
                assert_eq!(&*capabilities, ["requests", "extra"])
            }
            other => panic!("unexpected state {other:?}"),
        }

        write_envelope(
            &mut stream,
            &Envelope::new(Meta::new(), Data::new()).unwrap(),
        )
        .await;
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            None,
            "only the custom envelope reaches handlers"
        );
        assert!(observed_rx.try_recv().is_err());
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn readiness_violations_stop_the_worker_with_a_retained_reason() {
        let custom = Envelope::new(Meta::new(), Data::new()).unwrap();
        let cases: [(&str, Vec<Envelope>); 3] = [
            ("custom-first", vec![custom]),
            ("wrong-version", vec![ready_envelope(2)]),
            ("ready-twice", vec![ready_envelope(1), ready_envelope(1)]),
        ];
        for (name, frames) in cases {
            let directory = tempfile::TempDir::new().unwrap();
            let control_plane = test_control_plane(ProtocolLimits::default());
            let (mut stream, lifecycle) =
                register_unready(&control_plane, directory.path(), name, None).await;
            for frame in &frames {
                write_envelope(&mut stream, frame).await;
            }
            let exit = timeout(Duration::from_secs(3), wait_for_exit(lifecycle, name))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(exit.reason, ExitReason::ProtocolViolation, "case {name}");
            assert_eq!(exit.signal, Some(libc::SIGKILL));
            control_plane.shutdown_now();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exits_are_retained_for_late_and_simultaneous_waiters() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (_stream, lifecycle) =
            register_unready(&control_plane, directory.path(), "worker", None).await;
        let waiters: Vec<_> = (0..3)
            .map(|_| tokio::spawn(wait_for_exit(lifecycle.clone(), "worker")))
            .collect();

        control_plane
            .remove_worker("worker", ExitReason::Terminated)
            .await;
        for waiter in waiters {
            let exit = waiter.await.unwrap().unwrap();
            assert_eq!(exit.reason, ExitReason::Terminated);
            assert_eq!(exit.worker_id, "worker");
        }

        // Every sender is gone now; a late waiter still sees the exit.
        control_plane.shutdown_now();
        drop(control_plane);
        let late = wait_for_exit(lifecycle, "worker").await.unwrap();
        assert_eq!(late.reason, ExitReason::Terminated);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_direction_closes_connection_without_dispatch() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (called_tx, called_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |_| {
            called_tx.send(()).unwrap();
        });
        let mut stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;

        let execute: Envelope =
            serde_json::from_value(json!({"meta": {"x_op": "execute", "x_req": "1"}, "data": {}}))
                .unwrap();
        write_envelope(&mut stream, &execute).await;
        let mut byte = [0_u8; 1];
        let result = timeout(Duration::from_secs(1), stream.read_exact(&mut byte))
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(called_rx.try_recv().is_err());

        control_plane.shutdown_now();
    }

    /// Outbound traffic must never discard bytes of a partially received
    /// inbound frame, whichever boundary the fragment stops at.
    #[tokio::test(flavor = "multi_thread")]
    async fn outbound_activity_preserves_partially_received_frames() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (observed_tx, observed_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |inbound| {
            observed_tx.send(inbound.envelope.data().clone()).unwrap();
        });
        let mut stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;

        let mut frames = Vec::new();
        for sequence in 0..2 {
            let envelope: Envelope = serde_json::from_value(json!({
                "meta": {"x_op": "log"},
                "data": {"sequence": sequence, "padding": "p".repeat(512)}
            }))
            .unwrap();
            let payload = serde_json::to_vec(&envelope).unwrap();
            let mut frame = (payload.len() as u64).to_le_bytes().to_vec();
            frame.extend_from_slice(&payload);
            frames.push(frame);
        }

        // Split inside the header, then inside the body, forcing the
        // outbound branch to win while the reader holds partial progress.
        let first = &frames[0];
        let cuts = [0, 4, 8 + 100, first.len()];
        for (index, window) in cuts.windows(2).enumerate() {
            stream
                .write_all(&first[window[0]..window[1]])
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            control_plane
                .send(
                    "worker",
                    Envelope::new(Meta::new(), object(json!({ "outbound": index }))).unwrap(),
                )
                .await
                .unwrap();
            let outbound = timeout(Duration::from_secs(1), read_envelope(&mut stream))
                .await
                .unwrap();
            assert_eq!(outbound.data()["outbound"], index);
        }
        stream.write_all(&frames[1]).await.unwrap();
        stream.flush().await.unwrap();

        for sequence in 0..2 {
            let data = observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(data["sequence"], sequence);
            assert_eq!(data["padding"].as_str().unwrap().len(), 512);
        }

        control_plane.shutdown_now();
    }

    /// A Manager write blocked on a peer that is itself still writing must not
    /// stop inbound dispatch; otherwise both socket directions fill and stall.
    #[tokio::test(flavor = "multi_thread")]
    async fn blocked_outbound_write_keeps_draining_inbound_frames() {
        const INBOUND_FRAMES: usize = 400;
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let received = Arc::new(AtomicUsize::new(0));
        let handler_received = received.clone();
        let handler: MessageHandler = Arc::new(move |_| {
            handler_received.fetch_add(1, AtomicOrdering::SeqCst);
        });
        let stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;
        let (mut read_half, mut write_half) = stream.into_split();

        let inbound: Envelope = serde_json::from_value(json!({
            "meta": {"x_op": "log"},
            "data": {"padding": "p".repeat(4096)}
        }))
        .unwrap();
        let payload = serde_json::to_vec(&inbound).unwrap();
        let mut frame = (payload.len() as u64).to_le_bytes().to_vec();
        frame.extend_from_slice(&payload);
        // The peer writes far more than the socket buffers hold before it
        // starts reading, like a single-threaded Python worker would.
        let peer_writes = tokio::spawn(async move {
            for _ in 0..INBOUND_FRAMES {
                write_half.write_all(&frame).await.unwrap();
            }
            write_half
        });

        let large = Envelope::new(
            Meta::new(),
            object(json!({ "blob": "b".repeat(4 * 1024 * 1024) })),
        )
        .unwrap();
        // Admission only: the write cannot finish until the peer reads.
        let receipt = control_plane.try_send("worker", large).unwrap();

        let _write_half = timeout(Duration::from_secs(10), peer_writes)
            .await
            .expect("peer writes stalled: inbound frames stopped draining")
            .unwrap();
        let mut size = [0_u8; 8];
        timeout(Duration::from_secs(10), read_half.read_exact(&mut size))
            .await
            .unwrap()
            .unwrap();
        let mut body = vec![0_u8; u64::from_le_bytes(size) as usize];
        timeout(Duration::from_secs(10), read_half.read_exact(&mut body))
            .await
            .unwrap()
            .unwrap();
        let outbound: Envelope = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            outbound.data()["blob"].as_str().unwrap().len(),
            4 * 1024 * 1024
        );
        receipt.wait().await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while received.load(AtomicOrdering::SeqCst) < INBOUND_FRAMES {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        control_plane.shutdown_now();
    }

    #[test]
    fn registration_starts_monitoring_after_synchronous_construction() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            QueueLimits::default(),
        );
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

    #[tokio::test(flavor = "multi_thread")]
    async fn colliding_identity_is_rejected_without_touching_the_live_worker() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut live = register_worker(&control_plane, directory.path(), "worker", None).await;
        let live_pid = control_plane.workers.read()["worker"].child.id();

        let sock_path = directory.path().join("other.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let mut command = Command::new("sleep");
        command
            .arg("300")
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        let new_pid = child.id();
        let error = control_plane
            .register_worker(
                child,
                WorkerIdentity {
                    name: "worker".to_string(),
                    sock_file: "other.sock".to_string(),
                },
                sock_path.clone(),
                listener,
                None,
                test_slot(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(error, RunpyError::Io(ref io) if io.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert!(
            !process_alive(new_pid),
            "the unregistered child must be stopped"
        );
        assert!(!sock_path.exists());
        assert!(process_alive(live_pid), "the live worker must be untouched");
        control_plane.send("worker", numbered(1)).await.unwrap();
        assert_eq!(read_envelope(&mut live).await.data()["sequence"], 1);
        control_plane.shutdown_now();
    }

    #[tokio::test]
    async fn registration_after_shutdown_stops_child_and_removes_socket() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            QueueLimits::default(),
        );
        let sock_path = directory.path().join("worker.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
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
                listener,
                None,
                test_slot(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RunpyError::ManagerClosed));
        assert!(!sock_path.exists());
        assert!(control_plane.workers.read().is_empty());
        assert!(control_plane.sessions.lock().is_empty());
        assert!(control_plane.monitor.lock().is_none());
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
    }

    /// Poll `check` until it holds or a few seconds pass.
    async fn eventually(mut check: impl FnMut() -> bool, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while !check() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what} did not happen"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until the monitor has removed `worker_id`.
    async fn wait_for_removal(control_plane: &Arc<ControlPlane>, worker_id: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while control_plane.workers.read().contains_key(worker_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "worker '{worker_id}' was not removed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn process_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disconnect_removes_worker_and_fails_pending_sends() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let stream = register_worker(&control_plane, directory.path(), "worker", None).await;
        let pid = control_plane.workers.read()["worker"].child.id();
        let sock_path = directory.path().join("worker.sock");

        // The peer never reads: the first large frame blocks the writer and
        // the rest stay queued behind it.
        let large = Envelope::new(
            Meta::new(),
            object(json!({ "blob": "b".repeat(1024 * 1024) })),
        )
        .unwrap();
        let receipts: Vec<_> = (0..4)
            .map(|_| control_plane.try_send("worker", large.clone()).unwrap())
            .collect();
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(stream);

        for receipt in receipts {
            let result = timeout(Duration::from_secs(2), receipt.wait())
                .await
                .expect("pending sends must fail once the worker is gone");
            assert!(result.is_err());
        }
        wait_for_removal(&control_plane, "worker").await;
        eventually(|| !sock_path.exists() && !process_alive(pid), "cleanup").await;
        assert!(matches!(
            control_plane.send("worker", numbered(1)).await,
            Err(RunpyError::WorkerExited { .. })
        ));
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_frame_is_a_protocol_violation_that_stops_the_process_group() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;
        let pid = control_plane.workers.read()["worker"].child.id();

        stream.write_all(&header(9)).await.unwrap();
        stream.write_all(b"not json!").await.unwrap();

        wait_for_removal(&control_plane, "worker").await;
        eventually(|| !process_alive(pid), "process group stop").await;
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_removals_clean_up_exactly_once() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let _stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        let (first, second, third) = tokio::join!(
            control_plane.remove_worker("worker", ExitReason::Exited),
            control_plane.remove_worker("worker", ExitReason::Disconnected),
            control_plane.remove_worker("worker", ExitReason::Terminated),
        );
        assert_eq!(
            [first, second, third].iter().filter(|done| **done).count(),
            1
        );
        assert!(control_plane.sessions.lock().is_empty());
        control_plane.shutdown_now();
    }

    fn payload(value: u64) -> Data {
        object(json!({ "value": value }))
    }

    /// Read the next frame and require it to be an `execute`; return its
    /// request ID and data.
    async fn read_request(stream: &mut UnixStream) -> (String, Data) {
        let envelope = timeout(Duration::from_secs(2), read_envelope(stream))
            .await
            .expect("expected an execute frame");
        assert_eq!(envelope.meta()["x_op"], "execute");
        let id = envelope.meta()["x_req"].as_str().unwrap().to_string();
        (id, envelope.data().clone())
    }

    async fn answer(stream: &mut UnixStream, operation: &str, id: Option<&str>, data: Data) {
        let mut meta = json!({ "x_op": operation });
        if let Some(id) = id {
            meta["x_req"] = json!(id);
        }
        let envelope: Envelope =
            serde_json::from_value(json!({ "meta": meta, "data": data })).unwrap();
        write_envelope(stream, &envelope).await;
    }

    /// Assert that no frame arrives within a short window.
    async fn assert_idle(stream: &mut UnixStream) {
        use std::os::fd::AsRawFd;

        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut byte = 0_u8;
        // A non-consuming, nonblocking peek: anything pending is a frame the
        // worker should not have received yet.
        let pending = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(
            pending, -1,
            "the worker received a frame while it should be idle"
        );
        // Only "would block" proves an empty socket; any other error would
        // make this check pass vacuously.
        let error = std::io::Error::last_os_error().raw_os_error();
        assert!(
            error == Some(libc::EAGAIN) || error == Some(libc::EWOULDBLOCK),
            "idle check failed: {error:?}"
        );
    }

    const LONG: Duration = Duration::from_secs(10);

    /// Register a worker whose `ready` advertises `cancel`.
    async fn register_cancellable(
        control_plane: &Arc<ControlPlane>,
        directory: &std::path::Path,
    ) -> UnixStream {
        let (mut stream, mut lifecycle) =
            register_unready(control_plane, directory, "worker", None).await;
        let ready: Envelope = serde_json::from_value(json!({
            "meta": {"x_op": "ready"},
            "data": {"protocol_version": 1, "capabilities": ["requests", "cancel"]}
        }))
        .unwrap();
        write_envelope(&mut stream, &ready).await;
        lifecycle
            .wait_for(|state| matches!(state, LifecycleState::Ready { .. }))
            .await
            .unwrap();
        stream
    }

    async fn expect_cancel(stream: &mut UnixStream, id: &str) {
        let envelope = timeout(Duration::from_secs(2), read_envelope(stream))
            .await
            .expect("expected a cancel frame");
        assert_eq!(envelope.meta()["x_op"], "cancel");
        assert_eq!(envelope.meta()["x_req"], id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_request_is_cancelled_locally_and_never_sent() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_cancellable(&control_plane, directory.path()).await;
        let busy = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let queued = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        let (busy_id, _) = read_request(&mut stream).await;

        assert_eq!(
            queued.canceller().cancel().await.unwrap(),
            crate::request::Cancellation::Cancelled
        );
        assert!(matches!(
            queued.result().await,
            Err(RunpyError::Cancelled { .. })
        ));
        answer(&mut stream, "done", Some(&busy_id), payload(1)).await;
        busy.result().await.unwrap();
        assert_idle(&mut stream).await;
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn running_request_is_cancelled_only_after_the_worker_confirms() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_cancellable(&control_plane, directory.path()).await;
        let request = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let canceller = request.canceller();
        let (id, _) = read_request(&mut stream).await;

        let cancelling = tokio::spawn(async move { canceller.cancel().await });
        expect_cancel(&mut stream, &id).await;
        assert!(
            !cancelling.is_finished(),
            "cancel reported before the worker stopped"
        );
        answer(
            &mut stream,
            "cancelled",
            Some(&id),
            object(json!({"status": "cancelled"})),
        )
        .await;
        assert_eq!(
            cancelling.await.unwrap().unwrap(),
            crate::request::Cancellation::Cancelled
        );
        assert!(matches!(
            request.result().await,
            Err(RunpyError::Cancelled { .. })
        ));

        // The worker is free for the next request.
        let next = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        let (next_id, _) = read_request(&mut stream).await;
        answer(&mut stream, "done", Some(&next_id), payload(2)).await;
        next.result().await.unwrap();
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn late_cancel_reports_already_finished_and_keeps_the_result() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_cancellable(&control_plane, directory.path()).await;
        let request = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let canceller = request.canceller();
        let (id, _) = read_request(&mut stream).await;

        let cancelling = tokio::spawn(async move { canceller.cancel().await });
        expect_cancel(&mut stream, &id).await;
        // The request finished before the worker saw `cancel`.
        answer(&mut stream, "done", Some(&id), payload(5)).await;
        answer(
            &mut stream,
            "cancelled",
            Some(&id),
            object(json!({"status": "already_finished"})),
        )
        .await;
        assert_eq!(
            cancelling.await.unwrap().unwrap(),
            crate::request::Cancellation::AlreadyFinished
        );
        assert_eq!(request.result().await.unwrap()["value"], 5);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(control_plane.workers.read().contains_key("worker"));
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_needs_capability_and_a_confirmation_in_time() {
        let directory = tempfile::TempDir::new().unwrap();
        let queue = QueueLimits {
            send_timeout: Duration::from_millis(200),
            ..QueueLimits::default()
        };
        let control_plane = ControlPlane::new(test_logger(), ProtocolLimits::default(), queue);
        let mut plain = register_worker(&control_plane, directory.path(), "plain", None).await;
        let request = control_plane
            .submit("plain", payload(1), LONG)
            .await
            .unwrap();
        read_request(&mut plain).await;
        assert!(matches!(
            request.canceller().cancel().await,
            Err(RunpyError::Unsupported(_))
        ));

        let mut stream = register_cancellable(&control_plane, directory.path()).await;
        let stubborn = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let (id, _) = read_request(&mut stream).await;
        // The worker never confirms: the request keeps its worker busy.
        assert!(matches!(
            stubborn.canceller().cancel().await,
            Err(RunpyError::Timeout(TimeoutStage::Response))
        ));
        expect_cancel(&mut stream, &id).await;
        answer(&mut stream, "done", Some(&id), payload(1)).await;
        stubborn.result().await.unwrap();
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_for_a_request_that_is_not_running_is_a_violation() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_cancellable(&control_plane, directory.path()).await;
        answer(
            &mut stream,
            "cancelled",
            Some("9"),
            object(json!({"status": "cancelled"})),
        )
        .await;
        wait_for_removal(&control_plane, "worker").await;
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_run_one_at_a_time_and_return_their_own_results() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        let first = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let second = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        assert_ne!(first.id(), second.id());

        let (first_id, data) = read_request(&mut stream).await;
        assert_eq!(first_id, first.id().as_str());
        assert_eq!(data["value"], 1);
        assert_idle(&mut stream).await;
        // Logs between execute and done do not complete anything.
        answer(&mut stream, "log", None, object(json!({"progress": 50}))).await;
        answer(&mut stream, "done", Some(&first_id), payload(10)).await;

        let (second_id, data) = read_request(&mut stream).await;
        assert_eq!(data["value"], 2);
        answer(
            &mut stream,
            "error",
            Some(&second_id),
            object(json!({"kind": "ValueError", "message": "bad"})),
        )
        .await;

        assert_eq!(first.result().await.unwrap()["value"], 10);
        match second.result().await {
            Err(RunpyError::Execution { request_id, error }) => {
                assert_eq!(request_id.as_str(), second_id);
                assert_eq!(
                    error,
                    crate::request::RemoteError {
                        kind: "ValueError".to_string(),
                        message: "bad".to_string(),
                    }
                );
            }
            other => panic!("unexpected result {other:?}"),
        }
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_request_that_expires_never_runs() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        let busy = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let queued = control_plane
            .submit("worker", payload(2), Duration::from_millis(200))
            .await
            .unwrap();
        let (busy_id, _) = read_request(&mut stream).await;

        assert!(matches!(
            queued.result().await,
            Err(RunpyError::Timeout(TimeoutStage::Queued))
        ));
        answer(&mut stream, "done", Some(&busy_id), payload(1)).await;
        busy.result().await.unwrap();
        assert_idle(&mut stream).await;
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn running_request_past_its_deadline_keeps_the_worker_busy() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        let slow = control_plane
            .submit("worker", payload(1), Duration::from_millis(200))
            .await
            .unwrap();
        let (slow_id, _) = read_request(&mut stream).await;
        assert!(matches!(
            slow.result().await,
            Err(RunpyError::Timeout(TimeoutStage::Response))
        ));

        let next = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        assert_idle(&mut stream).await;
        // The late result frees the worker; it is discarded, not delivered.
        answer(&mut stream, "done", Some(&slow_id), payload(99)).await;
        let (next_id, data) = read_request(&mut stream).await;
        assert_eq!(data["value"], 2);
        answer(&mut stream, "done", Some(&next_id), payload(2)).await;
        assert_eq!(next.result().await.unwrap()["value"], 2);
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_request_still_runs_and_occupies_the_worker() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        drop(
            control_plane
                .submit("worker", payload(1), LONG)
                .await
                .unwrap(),
        );
        let (dropped_id, _) = read_request(&mut stream).await;
        let next = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        assert_idle(&mut stream).await;
        answer(&mut stream, "done", Some(&dropped_id), payload(1)).await;
        let (next_id, _) = read_request(&mut stream).await;
        answer(&mut stream, "done", Some(&next_id), payload(2)).await;
        next.result().await.unwrap();
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_or_missing_request_ids_are_protocol_violations() {
        for (name, id) in [("wrong", Some("999")), ("missing", None)] {
            let directory = tempfile::TempDir::new().unwrap();
            let control_plane = test_control_plane(ProtocolLimits::default());
            let mut stream = register_worker(&control_plane, directory.path(), name, None).await;
            let request = control_plane.submit(name, payload(1), LONG).await.unwrap();
            read_request(&mut stream).await;

            answer(&mut stream, "done", id, payload(1)).await;
            assert!(
                matches!(
                    request.result().await,
                    Err(RunpyError::OutcomeUnknown { .. })
                ),
                "case {name}"
            );
            wait_for_removal(&control_plane, name).await;
            control_plane.shutdown_now();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsolicited_error_reaches_observers_without_completing_a_request() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (observed_tx, observed_rx) = std_mpsc::channel();
        let handler: MessageHandler = Arc::new(move |inbound| {
            observed_tx
                .send(Envelope::clone(&inbound.envelope))
                .unwrap();
        });
        let mut stream =
            register_worker(&control_plane, directory.path(), "worker", Some(handler)).await;
        let request = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let (id, _) = read_request(&mut stream).await;

        answer(
            &mut stream,
            "error",
            None,
            object(json!({"kind": "KeyError", "message": "x"})),
        )
        .await;
        let unsolicited = observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(unsolicited.meta()["x_op"], "error");
        assert!(unsolicited.meta().get("x_req").is_none());

        answer(&mut stream, "done", Some(&id), payload(5)).await;
        assert_eq!(request.result().await.unwrap()["value"], 5);
        // Request results still reach observers, after completing the request.
        let done = observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(done.meta()["x_req"], id.as_str());
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn worker_exit_is_never_replayed_and_reports_what_ran() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;
        let running = control_plane
            .submit("worker", payload(1), LONG)
            .await
            .unwrap();
        let queued = control_plane
            .submit("worker", payload(2), LONG)
            .await
            .unwrap();
        read_request(&mut stream).await;

        drop(stream);
        assert!(matches!(
            running.result().await,
            Err(RunpyError::OutcomeUnknown { .. })
        ));
        assert!(matches!(
            queued.result().await,
            Err(RunpyError::WorkerExited { .. })
        ));
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pending_requests_are_bounded_manager_wide() {
        let directory = tempfile::TempDir::new().unwrap();
        let queue = QueueLimits {
            max_pending_requests: 1,
            ..QueueLimits::default()
        };
        let control_plane = ControlPlane::new(test_logger(), ProtocolLimits::default(), queue);
        let _first_stream = register_worker(&control_plane, directory.path(), "first", None).await;
        let _second_stream =
            register_worker(&control_plane, directory.path(), "second", None).await;

        let held = control_plane
            .submit("first", payload(1), LONG)
            .await
            .unwrap();
        assert!(matches!(
            control_plane
                .submit("second", payload(2), Duration::from_millis(200))
                .await,
            Err(RunpyError::Timeout(TimeoutStage::Admission))
        ));
        drop(held);
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_api_rejects_misuse() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = test_control_plane(ProtocolLimits::default());
        let (mut stream, mut lifecycle) =
            register_unready(&control_plane, directory.path(), "plain", None).await;
        let ready: Envelope = serde_json::from_value(json!({
            "meta": {"x_op": "ready"},
            "data": {"protocol_version": 1, "capabilities": []}
        }))
        .unwrap();
        write_envelope(&mut stream, &ready).await;
        lifecycle
            .wait_for(|state| matches!(state, LifecycleState::Ready { .. }))
            .await
            .unwrap();

        assert!(matches!(
            control_plane.submit("plain", payload(1), LONG).await,
            Err(RunpyError::Unsupported(_))
        ));
        let raw: Envelope =
            serde_json::from_value(json!({"meta": {"x_op": "execute", "x_req": "1"}, "data": {}}))
                .unwrap();
        assert!(matches!(
            control_plane.send("plain", raw).await,
            Err(RunpyError::InvalidConfig(_))
        ));

        let _ready = register_worker(&control_plane, directory.path(), "ready", None).await;
        control_plane
            .next_request
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            control_plane.submit("ready", payload(1), LONG).await,
            Err(RunpyError::InvalidConfig(message)) if message.contains("exhausted")
        ));
        control_plane.shutdown_now();
    }

    fn short_limits(send_timeout: Duration, shutdown_grace: Duration) -> QueueLimits {
        QueueLimits {
            outbound_capacity: 1,
            send_timeout,
            shutdown_grace,
            ..QueueLimits::default()
        }
    }

    /// Fill a route whose peer never reads: one large frame blocks the writer
    /// and a second occupies the only queue slot. Their own deadline is far
    /// away so the blocked write does not close the connection meanwhile.
    async fn saturate(control_plane: &Arc<ControlPlane>, worker_id: &str) -> Vec<SendReceipt> {
        let large = Envelope::new(
            Meta::new(),
            object(json!({ "blob": "b".repeat(2 * 1024 * 1024) })),
        )
        .unwrap();
        let sender = control_plane.mailer.route(worker_id).unwrap();
        let far = Instant::now() + Duration::from_secs(60);
        let mut receipts = Vec::new();
        while let Ok(admitted) = timeout(
            Duration::from_millis(200),
            sender.admit_until(large.clone(), far),
        )
        .await
        {
            receipts.push(admitted.unwrap());
        }
        assert!(receipts.len() >= 2, "route never saturated");
        receipts
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saturated_route_cannot_delay_termination_past_the_grace() {
        let directory = tempfile::TempDir::new().unwrap();
        let grace = Duration::from_millis(400);
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            short_limits(Duration::from_secs(30), grace),
        );
        let _stuck = register_worker(&control_plane, directory.path(), "stuck", None).await;
        let _healthy = register_worker(&control_plane, directory.path(), "healthy", None).await;
        let stuck_pid = control_plane.workers.read()["stuck"].child.id();
        let healthy_pid = control_plane.workers.read()["healthy"].child.id();
        let _receipts = saturate(&control_plane, "stuck").await;

        let started = tokio::time::Instant::now();
        control_plane.terminate_all().await;
        assert!(
            started.elapsed() < grace + Duration::from_secs(1),
            "termination took {:?}",
            started.elapsed()
        );
        assert!(!process_alive(stuck_pid));
        assert!(!process_alive(healthy_pid));
        assert!(control_plane.workers.read().is_empty());
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn early_exit_ends_the_termination_wait_and_new_sends_are_refused() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            short_limits(Duration::from_secs(5), Duration::from_secs(10)),
        );
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;
        let pid = control_plane.workers.read()["worker"].child.id();

        let started = tokio::time::Instant::now();
        let terminating = {
            let control_plane = control_plane.clone();
            tokio::spawn(async move { control_plane.terminate_worker("worker").await })
        };
        let request = timeout(Duration::from_secs(1), read_envelope(&mut stream))
            .await
            .unwrap();
        assert_eq!(request.meta()["x_op"], "terminate");
        assert!(matches!(
            control_plane.send("worker", numbered(1)).await,
            Err(RunpyError::WorkerExited { .. })
        ));

        // The worker honours the request by exiting.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        terminating.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!process_alive(pid));
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn broadcast_waits_for_stuck_routes_concurrently() {
        let directory = tempfile::TempDir::new().unwrap();
        let send_timeout = Duration::from_millis(500);
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            short_limits(send_timeout, Duration::from_secs(2)),
        );
        let _first = register_worker(&control_plane, directory.path(), "first", None).await;
        let _second = register_worker(&control_plane, directory.path(), "second", None).await;
        let mut healthy = register_worker(&control_plane, directory.path(), "healthy", None).await;
        let _first_receipts = saturate(&control_plane, "first").await;
        let _second_receipts = saturate(&control_plane, "second").await;

        let started = tokio::time::Instant::now();
        let results = control_plane.broadcast(numbered(9)).await;
        let elapsed = started.elapsed();

        assert!(matches!(results["first"], Err(RunpyError::Timeout(_))));
        assert!(matches!(results["second"], Err(RunpyError::Timeout(_))));
        assert!(results["healthy"].is_ok());
        assert!(
            elapsed < send_timeout * 3 / 2,
            "stuck routes were awaited sequentially: {elapsed:?}"
        );
        let delivered = read_envelope(&mut healthy).await;
        assert_eq!(delivered.data()["sequence"], 9);
        control_plane.shutdown_now();
    }

    fn test_sender(
        capacity: usize,
        budget_bytes: usize,
        send_timeout: Duration,
    ) -> (MessageSender, mpsc::Receiver<OutboundFrame>) {
        let (tx, rx) = mpsc::channel(capacity);
        let sender = MessageSender {
            tx,
            worker_id: "worker".to_string(),
            socket_path: "/tmp/worker.sock".to_string(),
            budget: Arc::new(Semaphore::new(budget_bytes)),
            limits: ProtocolLimits::default(),
            send_timeout,
            closing: Arc::new(AtomicBool::new(false)),
        };
        (sender, rx)
    }

    fn numbered(sequence: u64) -> Envelope {
        Envelope::new(Meta::new(), object(json!({ "sequence": sequence }))).unwrap()
    }

    fn decoded_sequence(entry: &OutboundFrame) -> u64 {
        let envelope: Envelope = serde_json::from_slice(&entry.frame[8..]).unwrap();
        assert_eq!(envelope.meta()["x_wid"], "worker");
        envelope.data()["sequence"].as_u64().unwrap()
    }

    #[tokio::test]
    async fn full_route_rejects_nonblocking_sends_without_buffering() {
        let (sender, mut rx) = test_sender(2, 1 << 20, Duration::from_secs(5));
        let _first = sender.try_send(numbered(1)).unwrap();
        let _second = sender.try_send(numbered(2)).unwrap();
        assert!(matches!(
            sender.try_send(numbered(3)),
            Err(RunpyError::QueueFull)
        ));

        assert_eq!(decoded_sequence(&rx.try_recv().unwrap()), 1);
        assert_eq!(decoded_sequence(&rx.try_recv().unwrap()), 2);
        assert!(
            rx.try_recv().is_err(),
            "the rejected send must not be queued"
        );
    }

    #[tokio::test]
    async fn byte_budget_bounds_queued_frames_and_releases_on_drop() {
        let (probe, _probe_rx) = test_sender(1, 1, Duration::from_secs(5));
        let (_, cost) = probe.encode(numbered(1)).unwrap();
        let (sender, mut rx) = test_sender(64, cost as usize, Duration::from_secs(5));
        let _first = sender.try_send(numbered(1)).unwrap();
        assert!(matches!(
            sender.try_send(numbered(2)),
            Err(RunpyError::QueueFull)
        ));

        drop(rx.try_recv().unwrap());
        let _third = sender.try_send(numbered(3)).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn blocking_send_times_out_at_admission_when_capacity_never_frees() {
        let (sender, _rx) = test_sender(1, 1 << 20, Duration::from_secs(5));
        let _first = sender.try_send(numbered(1)).unwrap();
        let error = sender.send(numbered(2)).await.unwrap_err();
        assert!(matches!(
            error,
            RunpyError::Timeout(TimeoutStage::Admission)
        ));
    }

    #[tokio::test]
    async fn waiting_sends_are_admitted_in_arrival_order() {
        let (sender, mut rx) = test_sender(1, 1 << 20, Duration::from_secs(5));
        let _first = sender.try_send(numbered(1)).unwrap();
        let mut waiting = Vec::new();
        for sequence in 2..=4 {
            let sender = sender.clone();
            waiting.push(tokio::spawn(async move {
                sender.admit(numbered(sequence)).await.unwrap()
            }));
            // Let each sender register as a waiter before the next starts.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }

        let mut order = Vec::new();
        for _ in 1..=4 {
            let entry = timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            order.push(decoded_sequence(&entry));
        }
        assert_eq!(order, [1, 2, 3, 4]);
        for task in waiting {
            let _ = task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn wrong_direction_and_oversized_envelopes_fail_before_admission() {
        let (mut sender, mut rx) = test_sender(4, 1 << 20, Duration::from_secs(5));
        sender.limits = limits(64, Duration::from_secs(1));
        let ready: Envelope =
            serde_json::from_value(json!({"meta": {"x_op": "ready"}, "data": {}})).unwrap();
        assert!(matches!(
            sender.try_send(ready),
            Err(RunpyError::InvalidEnvelope(_))
        ));
        let oversized =
            Envelope::new(Meta::new(), object(json!({ "blob": "x".repeat(128) }))).unwrap();
        assert!(matches!(
            sender.try_send(oversized),
            Err(RunpyError::FrameTooLarge { limit: 64, .. })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_route_reports_worker_exited() {
        let (sender, rx) = test_sender(1, 1 << 20, Duration::from_secs(5));
        drop(rx);
        assert!(matches!(
            sender.send(numbered(1)).await,
            Err(RunpyError::WorkerExited { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_resolves_after_the_frame_is_written() {
        let directory = tempfile::TempDir::new().unwrap();
        let control_plane = ControlPlane::new(
            test_logger(),
            ProtocolLimits::default(),
            QueueLimits::default(),
        );
        let stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        control_plane.send("worker", numbered(7)).await.unwrap();
        // The write already completed, so the whole frame is readable at once.
        timeout(Duration::from_secs(1), stream.readable())
            .await
            .unwrap()
            .unwrap();
        let mut header = [0_u8; 8];
        assert_eq!(stream.try_read(&mut header).unwrap(), 8);
        control_plane.shutdown_now();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interrupted_write_times_out_and_closes_the_connection() {
        let directory = tempfile::TempDir::new().unwrap();
        // Long enough to start writing even on a loaded host; the write
        // itself can never finish because the peer never reads.
        let queue = QueueLimits {
            send_timeout: Duration::from_secs(2),
            ..QueueLimits::default()
        };
        let control_plane = ControlPlane::new(test_logger(), ProtocolLimits::default(), queue);
        let mut stream = register_worker(&control_plane, directory.path(), "worker", None).await;

        // The peer never reads, so a frame far larger than the socket
        // buffers cannot finish before the deadline.
        let large = Envelope::new(
            Meta::new(),
            object(json!({ "blob": "b".repeat(4 * 1024 * 1024) })),
        )
        .unwrap();
        let error = control_plane.send("worker", large).await.unwrap_err();
        assert!(
            matches!(error, RunpyError::Timeout(TimeoutStage::Write)),
            "{error:?}"
        );

        // Draining what was written ends in EOF: the stream is not reused.
        let mut sink = Vec::new();
        timeout(Duration::from_secs(5), stream.read_to_end(&mut sink))
            .await
            .unwrap()
            .unwrap();
        assert!(sink.len() < 4 * 1024 * 1024);
        control_plane.shutdown_now();
    }
}
