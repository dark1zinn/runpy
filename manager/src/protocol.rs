use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::scribbler::scribbler;

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

fn validate_direction(
    operation: Option<&str>,
    allowed: &[&str],
    direction: &'static str,
) -> Result<(), EnvelopeError> {
    if let Some(operation) = operation {
        if !allowed.contains(&operation) {
            return Err(EnvelopeError::WrongDirection {
                operation: operation.to_string(),
                direction,
            });
        }
    }
    Ok(())
}

pub type MessageHandler = Arc<dyn Fn(InboundEnvelope) + Send + Sync>;

/// A received wire envelope plus a responder bound to its worker connection.
#[derive(Clone)]
pub struct InboundEnvelope {
    pub envelope: Envelope,
    pub mailer: Mailer,
}

/// A lightweight responder for the worker that sent an inbound envelope.
#[derive(Clone)]
pub struct Mailer {
    tx: mpsc::Sender<Envelope>,
    worker_id: String,
}

impl Mailer {
    fn new(tx: mpsc::Sender<Envelope>, worker_id: String) -> Self {
        Self { tx, worker_id }
    }

    #[doc(hidden)]
    pub fn for_testing(worker_id: String) -> Self {
        let (tx, _rx) = mpsc::channel::<Envelope>(1);
        Self { tx, worker_id }
    }

    pub fn send(&self, envelope: Envelope) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(error) = tx.send(envelope).await {
                scribbler().error_with("Mailer", &format!("Failed to send envelope: {error}"));
            }
        });
    }

    pub async fn send_async(&self, envelope: Envelope) -> Result<(), String> {
        self.tx.send(envelope).await.map_err(|error| {
            format!(
                "Failed to send envelope to worker {}: {error}",
                self.worker_id
            )
        })
    }
}

/// A channel-based sender for a connected worker stream.
#[derive(Clone)]
pub struct MessageSender {
    tx: mpsc::Sender<Envelope>,
}

impl MessageSender {
    #[doc(hidden)]
    pub fn for_testing(tx: mpsc::Sender<Envelope>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, envelope: Envelope) -> Result<(), String> {
        self.tx
            .send(envelope)
            .await
            .map_err(|error| format!("Failed to send envelope to worker: {error}"))
    }
}

/// Manages the Unix socket connection for one worker.
pub struct ControlPlane {
    listener: UnixListener,
    worker_id: String,
    socket_path: String,
    global_handler: Option<MessageHandler>,
    worker_handler: Option<MessageHandler>,
}

impl ControlPlane {
    pub fn new(
        listener: UnixListener,
        worker_id: String,
        socket_path: String,
        global_handler: Option<MessageHandler>,
        worker_handler: Option<MessageHandler>,
    ) -> Self {
        Self {
            listener,
            worker_id,
            socket_path,
            global_handler,
            worker_handler,
        }
    }

    pub fn start(self) -> MessageSender {
        let (tx, rx) = mpsc::channel::<Envelope>(64);
        let sender = MessageSender { tx };
        tokio::spawn(async move {
            self.run(rx).await;
        });
        sender
    }

    async fn run(self, mut outbound_rx: mpsc::Receiver<Envelope>) {
        loop {
            let (mut stream, _) = match self.listener.accept().await {
                Ok(connection) => connection,
                Err(error) => {
                    scribbler().error_with(
                        "ControlPlane",
                        &format!("{} Accept error: {error}", self.worker_id),
                    );
                    break;
                }
            };

            let (response_tx, mut response_rx) = mpsc::channel::<Envelope>(64);

            loop {
                tokio::select! {
                    received = Self::recv_envelope(&mut stream) => {
                        let Some(mut envelope) = received else {
                            break;
                        };
                        if let Err(error) = envelope.validate_worker_to_manager() {
                            scribbler().error_with("Protocol", &error.to_string());
                            break;
                        }
                        envelope.stamp_trusted(&self.worker_id, &self.socket_path);

                        let inbound = InboundEnvelope {
                            envelope,
                            mailer: Mailer::new(response_tx.clone(), self.worker_id.clone()),
                        };

                        if let Some(handler) = &self.global_handler {
                            handler(inbound.clone());
                        }
                        if let Some(handler) = &self.worker_handler {
                            handler(inbound);
                        }
                    }
                    Some(envelope) = outbound_rx.recv() => {
                        if let Err(error) = self.write_stamped_envelope(&mut stream, envelope).await {
                            scribbler().error_with(
                                "ControlPlane",
                                &format!("{} Send error: {error}", self.worker_id),
                            );
                            break;
                        }
                    }
                    Some(envelope) = response_rx.recv() => {
                        if let Err(error) = self.write_stamped_envelope(&mut stream, envelope).await {
                            scribbler().error_with(
                                "ControlPlane",
                                &format!("{} Response send error: {error}", self.worker_id),
                            );
                            break;
                        }
                    }
                }
            }
        }
    }

    async fn recv_envelope(stream: &mut UnixStream) -> Option<Envelope> {
        let mut size_buf = [0_u8; 8];
        if let Err(error) = stream.read_exact(&mut size_buf).await {
            match error.kind() {
                std::io::ErrorKind::UnexpectedEof => {}
                std::io::ErrorKind::ConnectionReset => {
                    scribbler().debug_with("Socket", "Connection reset by peer");
                }
                std::io::ErrorKind::BrokenPipe => {
                    scribbler().debug_with("Socket", "Broken pipe - peer closed unexpectedly");
                }
                _ => scribbler().error_with(
                    "Socket",
                    &format!("Read error: {error} (kind: {:?})", error.kind()),
                ),
            }
            return None;
        }

        let envelope_size = u64::from_le_bytes(size_buf) as usize;
        let mut envelope_buf = vec![0_u8; envelope_size];
        if let Err(error) = stream.read_exact(&mut envelope_buf).await {
            scribbler().error_with(
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
                scribbler().error_with(
                    "Protocol",
                    &format!("JSON envelope error: {error}\n  Raw: {raw}"),
                );
                None
            }
        }
    }

    async fn write_stamped_envelope(
        &self,
        stream: &mut UnixStream,
        mut envelope: Envelope,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        envelope.validate_manager_to_worker()?;
        envelope.stamp_trusted(&self.worker_id, &self.socket_path);

        let payload = serde_json::to_vec(&envelope)?;
        let size = (payload.len() as u64).to_le_bytes();
        stream.write_all(&size).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;
        Ok(())
    }
}
