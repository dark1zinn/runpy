use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

// ══════════════════════════════════════════════════════════════════════════
// HTTP-LIKE MESSAGE PROTOCOL
// ══════════════════════════════════════════════════════════════════════════
//
// Inspired by HTTP, this protocol uses a JSON structure with:
//   - method:  The action type (GET, POST, EXECUTE, TERMINATE, etc.)
//   - path:    A resource path for routing (e.g., "/status", "/execute")
//   - headers: Key-value metadata (worker identification, content info)
//   - body:    Optional payload data
//
// ══════════════════════════════════════════════════════════════════════════

/// HTTP-like methods for the runpy protocol.
/// Includes standard HTTP methods and custom ones for worker management.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Method {
    // ── Standard HTTP-like methods ─────────────────────────────────────
    /// Request information (status, health, etc.)
    Get,
    /// Send data or trigger an action
    Post,
    /// Update existing data/state
    Put,
    /// Remove/clear data
    Delete,

    // ── Custom Runpy methods ───────────────────────────────────────────
    /// Execute the worker's main business logic
    Execute,
    /// Re-execute the last payload
    Retry,
    /// Request graceful termination
    Terminate,
    /// Send/receive metadata about the worker
    Meta,
    /// Signal the worker is ready
    Ready,
    /// Response with status information
    Status,
    /// Generic informational message
    Info,
    /// Debug-level message
    Debug,
    /// Signal successful completion
    Done,
    /// Error response
    Error,
    /// Perform a named action with parameters
    Action,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Method::Get => write!(f, "GET"),
            Method::Post => write!(f, "POST"),
            Method::Put => write!(f, "PUT"),
            Method::Delete => write!(f, "DELETE"),
            Method::Execute => write!(f, "EXECUTE"),
            Method::Retry => write!(f, "RETRY"),
            Method::Terminate => write!(f, "TERMINATE"),
            Method::Meta => write!(f, "META"),
            Method::Ready => write!(f, "READY"),
            Method::Status => write!(f, "STATUS"),
            Method::Info => write!(f, "INFO"),
            Method::Debug => write!(f, "DEBUG"),
            Method::Done => write!(f, "DONE"),
            Method::Error => write!(f, "ERROR"),
            Method::Action => write!(f, "ACTION"),
        }
    }
}

/// Standard header keys used in the protocol.
pub mod headers {
    /// Worker's unique identifier name
    pub const X_WORKER_ID: &str = "X-Worker-Id";
    /// Path to the worker's Unix socket
    pub const X_SOCKET_PATH: &str = "X-Socket-Path";
    /// Content type (typically "application/json")
    pub const CONTENT_TYPE: &str = "Content-Type";
    /// Uptime in seconds
    pub const X_UPTIME: &str = "X-Uptime";
    /// Action name for ACTION method
    pub const X_ACTION: &str = "X-Action";
    /// Optional stack trace for errors
    pub const X_STACK_TRACE: &str = "X-Stack-Trace";
    /// Request key for GET requests
    pub const X_KEY: &str = "X-Key";
}

/// Headers container - a map of string key-value pairs.
pub type Headers = HashMap<String, String>;

/// The unified message structure for all communication between
/// the Rust manager and Python workers.
///
/// Follows an HTTP-like schema:
/// ```json
/// {
///   "method": "EXECUTE",
///   "path": "/task",
///   "headers": {
///     "X-Worker-Id": "my_worker_01012026-1200_Ax4f",
///     "X-Socket-Path": "/tmp/runpy/rp_my_worker.sock",
///     "Content-Type": "application/json"
///   },
///   "body": { "task": "process_data", "input": [...] }
/// }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Message {
    /// The request/response method (GET, POST, EXECUTE, etc.)
    pub method: Method,

    /// Resource path for routing (e.g., "/status", "/execute", "/meta")
    #[serde(default)]
    pub path: String,

    /// Key-value headers for metadata (worker ID, socket path, etc.)
    #[serde(default)]
    pub headers: Headers,

    /// Optional message body containing the payload data
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

impl Message {
    // ── Constructors ───────────────────────────────────────────────────

    /// Create a new message with the given method and path.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: HashMap::new(),
            body: None,
        }
    }

    /// Create a message with method, path, and body.
    pub fn with_body(method: Method, path: impl Into<String>, body: Value) -> Self {
        Self {
            method,
            path: path.into(),
            headers: HashMap::new(),
            body: Some(body),
        }
    }

    // ── Builder methods ────────────────────────────────────────────────

    /// Add a header to the message.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    /// Set the body of the message.
    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    // ── Header accessors ───────────────────────────────────────────────

    /// Get the worker ID from headers.
    pub fn worker_id(&self) -> Option<&str> {
        self.headers.get(headers::X_WORKER_ID).map(|s| s.as_str())
    }

    /// Get the socket path from headers.
    pub fn socket_path(&self) -> Option<&str> {
        self.headers.get(headers::X_SOCKET_PATH).map(|s| s.as_str())
    }

    /// Get a specific header value.
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers.get(key).map(|s| s.as_str())
    }

    // ── Convenience constructors for common message types ──────────────

    /// Create a READY message.
    pub fn ready(message: impl Into<String>) -> Self {
        Self::with_body(
            Method::Ready,
            "/ready",
            serde_json::json!({ "message": message.into() }),
        )
    }

    /// Create a DONE message with result data.
    pub fn done(message: impl Into<String>, data: Value) -> Self {
        Self::with_body(
            Method::Done,
            "/done",
            serde_json::json!({
                "message": message.into(),
                "data": data
            }),
        )
    }

    /// Create an ERROR message.
    pub fn error(message: impl Into<String>, stack_trace: Option<String>) -> Self {
        let mut msg = Self::with_body(
            Method::Error,
            "/error",
            serde_json::json!({ "message": message.into() }),
        );
        if let Some(trace) = stack_trace {
            msg.headers.insert(headers::X_STACK_TRACE.to_string(), trace);
        }
        msg
    }

    /// Create a DEBUG message.
    pub fn debug(message: impl Into<String>, data: Value) -> Self {
        Self::with_body(
            Method::Debug,
            "/debug",
            serde_json::json!({
                "message": message.into(),
                "data": data
            }),
        )
    }

    /// Create an INFO message.
    pub fn info(message: impl Into<String>, data: Value) -> Self {
        Self::with_body(
            Method::Info,
            "/info",
            serde_json::json!({
                "message": message.into(),
                "data": data
            }),
        )
    }

    /// Create a STATUS request.
    pub fn status_request() -> Self {
        Self::new(Method::Get, "/status")
    }

    /// Create a STATUS response.
    pub fn status_response(status: impl Into<String>, uptime: u64) -> Self {
        Self::with_body(
            Method::Status,
            "/status",
            serde_json::json!({
                "status": status.into(),
                "uptime": uptime
            }),
        )
        .header(headers::X_UPTIME, uptime.to_string())
    }

    /// Create an EXECUTE message.
    pub fn execute(payload: Value) -> Self {
        Self::with_body(Method::Execute, "/execute", payload)
    }

    /// Create a RETRY message.
    pub fn retry() -> Self {
        Self::new(Method::Retry, "/retry")
    }

    /// Create a TERMINATE message.
    pub fn terminate() -> Self {
        Self::new(Method::Terminate, "/terminate")
    }

    /// Create a META message.
    pub fn meta(data: Value) -> Self {
        Self::with_body(Method::Meta, "/meta", data)
    }

    /// Create a GET request for a specific key.
    pub fn get(key: impl Into<String>) -> Self {
        Self::new(Method::Get, "/get").header(headers::X_KEY, key)
    }

    /// Create an ACTION message.
    pub fn action(action: impl Into<String>, params: Value) -> Self {
        Self::with_body(Method::Action, "/action", params).header(headers::X_ACTION, action)
    }
}

/// Callback type for handling messages from workers.
pub type MessageHandler = Arc<dyn Fn(Envelope) + Send + Sync>;

/// A lightweight handle for sending messages back to a specific worker.
#[derive(Clone)]
pub struct Mailer {
    tx: mpsc::Sender<Message>,
    worker_id: String,
}

impl Mailer {
    pub(crate) fn new(tx: mpsc::Sender<Message>, worker_id: String) -> Self {
        Self { tx, worker_id }
    }

    /// Create a test mailer for unit tests (does not actually send messages).
    /// **Warning**: This creates a disconnected channel - messages sent will be dropped.
    #[doc(hidden)]
    pub fn for_testing(worker_id: String) -> Self {
        let (tx, _rx) = mpsc::channel::<Message>(1);
        Self { tx, worker_id }
    }

    /// Send a message back to the worker that sent the original message.
    /// This is a fire-and-forget method that spawns a task to send the message.
    pub fn send(&self, msg: Message) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = tx.send(msg).await {
                eprintln!("Failed to send message: {}", e);
            }
        });
    }

    /// Async version that returns a Result for proper error handling.
    pub async fn send_async(&self, msg: Message) -> Result<(), String> {
        self.tx
            .send(msg)
            .await
            .map_err(|e| format!("Failed to send message to worker {}: {}", self.worker_id, e))
    }
}

/// An envelope wraps a `Message` with metadata about which worker sent it,
/// and provides a way to send responses back to that worker.
#[derive(Clone)]
pub struct Envelope {
    pub worker_id: String,
    pub message: Message,
    pub mailer: Mailer,
}

/// A channel-based sender that lets user code send messages to a connected worker stream.
#[derive(Clone)]
pub struct MessageSender {
    tx: mpsc::Sender<Message>,
}

impl MessageSender {
    /// Send a message to the worker through the control plane's stream.
    pub async fn send(&self, msg: Message) -> Result<(), String> {
        self.tx
            .send(msg)
            .await
            .map_err(|e| format!("Failed to send message to worker: {}", e))
    }
}

/// The control plane manages the Unix socket listener for a single worker.
/// It receives messages from the Python worker, wraps them in an `Envelope`,
/// and dispatches them to the registered handlers. It also supports sending
/// messages back to the worker via a channel-based `MessageSender`.
pub struct ControlPlane {
    listener: UnixListener,
    worker_id: String,
    global_handler: Option<MessageHandler>,
    worker_handler: Option<MessageHandler>,
}

impl ControlPlane {
    pub fn new(
        listener: UnixListener,
        worker_id: String,
        global_handler: Option<MessageHandler>,
        worker_handler: Option<MessageHandler>,
    ) -> Self {
        Self {
            listener,
            worker_id,
            global_handler,
            worker_handler,
        }
    }

    /// Start accepting connections. Returns a `MessageSender` that can be used
    /// to send messages to the connected worker. The control plane runs in the
    /// background via `tokio::spawn`.
    pub fn start(self) -> MessageSender {
        let (tx, rx) = mpsc::channel::<Message>(64);
        let sender = MessageSender { tx };

        tokio::spawn(async move {
            self.run(rx).await;
        });

        sender
    }

    async fn run(self, mut outbound_rx: mpsc::Receiver<Message>) {
        loop {
            let accept_result = self.listener.accept().await;
            let (mut stream, _) = match accept_result {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!(
                        "[ControlPlane {}] Accept error: {}",
                        self.worker_id, e
                    );
                    break;
                }
            };

            let global = self.global_handler.clone();
            let worker = self.worker_handler.clone();
            let wid = self.worker_id.clone();

            // Create a channel for this connection to send messages back
            let (response_tx, mut response_rx) = mpsc::channel::<Message>(64);

            // Handle a single connection (Python workers connect once and keep the stream open)
            loop {
                tokio::select! {
                    // Inbound: read messages from the Python worker
                    recv_result = Self::recv_message(&mut stream) => {
                        match recv_result {
                            Some(msg) => {
                                let mailer = Mailer::new(response_tx.clone(), wid.clone());
                                let envelope = Envelope {
                                    worker_id: wid.clone(),
                                    message: msg,
                                    mailer,
                                };

                                // Global handler fires first
                                if let Some(ref handler) = global {
                                    handler(envelope.clone());
                                }

                                // Then worker-specific handler
                                if let Some(ref handler) = worker {
                                    handler(envelope);
                                }
                            }
                            None => {
                                // Connection closed by the Python side
                                break;
                            }
                        }
                    }

                    // Outbound: messages from the main outbound channel
                    Some(msg) = outbound_rx.recv() => {
                        if let Err(e) = Self::send_message(&mut stream, &msg).await {
                            eprintln!("[ControlPlane {}] Send error: {}", wid, e);
                            break;
                        }
                    }

                    // Responses: messages from the envelope mailer
                    Some(msg) = response_rx.recv() => {
                        if let Err(e) = Self::send_message(&mut stream, &msg).await {
                            eprintln!("[ControlPlane {}] Response send error: {}", wid, e);
                            break;
                        }
                    }
                }
            }

            // If the inner loop breaks the connection is gone; wait for a new one
            // or break if the listener itself failed.
        }
    }

    /// Read a single length-prefixed JSON message from the stream.
    async fn recv_message(stream: &mut tokio::net::UnixStream) -> Option<Message> {
        let mut size_buf = [0u8; 8];
        match stream.read_exact(&mut size_buf).await {
            Ok(_) => {}
            Err(e) => {
                match e.kind() {
                    std::io::ErrorKind::UnexpectedEof => {} // clean close
                    std::io::ErrorKind::ConnectionReset => {
                        eprintln!("Connection reset by peer");
                    }
                    std::io::ErrorKind::BrokenPipe => {
                        eprintln!("Broken pipe - peer closed unexpectedly");
                    }
                    _ => {
                        eprintln!("Socket read error: {} (kind: {:?})", e, e.kind());
                    }
                }
                return None;
            }
        }

        let message_size = u64::from_le_bytes(size_buf) as usize;
        let mut message_buf = vec![0u8; message_size];

        match stream.read_exact(&mut message_buf).await {
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "Error reading message body: {} (kind: {:?})",
                    e,
                    e.kind()
                );
                return None;
            }
        }

        match serde_json::from_slice::<Message>(&message_buf) {
            Ok(msg) => Some(msg),
            Err(e) => {
                let raw = String::from_utf8_lossy(&message_buf);
                eprintln!("JSON parse error: {} \n  Raw: {}", e, raw);
                None
            }
        }
    }

    /// Send a length-prefixed JSON message over the stream.
    pub async fn send_message(
        stream: &mut tokio::net::UnixStream,
        msg: &Message,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = serde_json::to_vec(msg)?;
        let size = (payload.len() as u64).to_le_bytes();
        stream.write_all(&size).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;
        Ok(())
    }
}
