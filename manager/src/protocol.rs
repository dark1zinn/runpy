use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use std::sync::Arc;

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

/// The protocol message types exchanged between Rust and Python workers
/// over length-prefixed JSON on Unix sockets.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Message {
    Status { uptime: Option<u64> },
    StatusRes { status: String, uptime: u64 },
    Info { message: String, data: Value },
    Error { message: String, stack_trace: Option<String> },
    Get { key: String },
    Action { action: String, params: Value },
    Done { message: String, data: Value },
    Debug { message: String, data: Value },
    Terminate,
    Ready { message: String },
    Retry,
    Meta { data: Value },
    Execute { payload: Value },
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
