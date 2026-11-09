//! Typed errors returned by Runpy operations.

use std::fmt;

use crate::protocol::EnvelopeError;

/// Operation stage at which a deadline expired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimeoutStage {
    /// Waiting for outbound queue capacity (byte budget or route slot).
    ///
    /// Nothing was queued or written.
    Admission,
    /// Admitted but still queued when the deadline passed.
    ///
    /// The frame was discarded without writing any byte.
    Queued,
    /// The deadline passed while the frame was being written.
    ///
    /// Delivery is unknown, so the connection is closed instead of risking a
    /// corrupted stream.
    Write,
}

impl fmt::Display for TimeoutStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Admission => "waiting for queue capacity",
            Self::Queued => "waiting in the outbound queue",
            Self::Write => "writing to the socket",
        })
    }
}

/// Error returned by Runpy operations.
///
/// Variants carry structured context so callers can branch on the cause
/// rather than on message text.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunpyError {
    /// A configuration value or call sequence is invalid.
    InvalidConfig(String),
    /// An envelope violated reserved metadata or operation direction rules.
    InvalidEnvelope(EnvelopeError),
    /// An encoded envelope would exceed the configured frame size.
    FrameTooLarge {
        /// Encoded body bytes produced before serialization stopped.
        size: u64,
        /// Configured maximum body size.
        limit: usize,
    },
    /// A nonblocking operation found no outbound capacity.
    QueueFull,
    /// A deadline expired at the given stage.
    Timeout(TimeoutStage),
    /// The owning Manager has been dropped or is shutting down.
    ManagerClosed,
    /// The worker is not registered or its connection has closed.
    WorkerExited {
        /// Trusted worker identity.
        worker_id: String,
    },
    /// A socket operation failed.
    Io(std::io::Error),
    /// An envelope could not be serialized.
    Encode(serde_json::Error),
}

impl fmt::Display for RunpyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid configuration: {message}"),
            Self::InvalidEnvelope(error) => write!(f, "invalid envelope: {error}"),
            Self::FrameTooLarge { size, limit } => write!(
                f,
                "envelope of at least {size} bytes exceeds the {limit}-byte frame limit"
            ),
            Self::QueueFull => f.write_str("outbound queue is full"),
            Self::Timeout(stage) => write!(f, "timed out while {stage}"),
            Self::ManagerClosed => f.write_str("worker manager is no longer available"),
            Self::WorkerExited { worker_id } => {
                write!(f, "worker '{worker_id}' is not running")
            }
            Self::Io(error) => write!(f, "socket I/O failed: {error}"),
            Self::Encode(error) => write!(f, "envelope could not be encoded: {error}"),
        }
    }
}

impl std::error::Error for RunpyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidEnvelope(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EnvelopeError> for RunpyError {
    fn from(error: EnvelopeError) -> Self {
        Self::InvalidEnvelope(error)
    }
}
