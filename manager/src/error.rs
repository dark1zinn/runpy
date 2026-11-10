//! Typed errors returned by Runpy operations.

use std::fmt;
use std::path::PathBuf;

use crate::protocol::EnvelopeError;
use crate::request::{RemoteError, RequestId};

/// Why a worker script name was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidScriptReason {
    /// The name contains a path separator or is not a plain file stem.
    NotSingleComponent,
    /// No `<script>.py` exists at the resolved path.
    NotFound(PathBuf),
}

/// Operation stage at which a deadline expired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TimeoutStage {
    /// The worker did not report `ready` within the startup timeout.
    ///
    /// Its process group was stopped.
    Startup,
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
    /// A request was sent but no result arrived before its deadline.
    ///
    /// It may still be running; the worker stays occupied until it finishes.
    Response,
}

impl fmt::Display for TimeoutStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Startup => "waiting for the worker to become ready",
            Self::Admission => "waiting for queue capacity",
            Self::Queued => "waiting in the outbound queue",
            Self::Write => "writing to the socket",
            Self::Response => "waiting for the worker's result",
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
    /// A worker script name cannot be launched.
    InvalidScript {
        /// Script name as given, without `.py`.
        script: String,
        /// Why the script was rejected.
        reason: InvalidScriptReason,
    },
    /// The configured scripts directory cannot be used.
    ScriptsDirectory {
        /// Configured scripts directory.
        path: PathBuf,
        /// Underlying filesystem error.
        source: std::io::Error,
    },
    /// The configured `uv` executable is missing or failed its version check.
    UvUnavailable {
        /// Configured `uv` path or command name.
        path: PathBuf,
        /// What went wrong.
        detail: String,
    },
    /// An envelope violated reserved metadata or operation direction rules.
    InvalidEnvelope(EnvelopeError),
    /// An encoded envelope would exceed the configured frame size.
    FrameTooLarge {
        /// Encoded body bytes produced before serialization stopped.
        size: u64,
        /// Configured maximum body size.
        limit: usize,
    },
    /// A nonblocking operation found no queue capacity.
    QueueFull,
    /// A deadline expired at the given stage.
    Timeout(TimeoutStage),
    /// The owning Manager has been dropped or is shutting down.
    ManagerClosed,
    /// The worker pool was closed before the request reached a worker.
    PoolClosed,
    /// The worker is not registered or its connection has closed.
    WorkerExited {
        /// Trusted worker identity.
        worker_id: String,
    },
    /// A socket, pipe, or process operation failed.
    Io(std::io::Error),
    /// An envelope could not be serialized.
    Encode(serde_json::Error),
    /// The worker does not support the requested feature.
    Unsupported(String),
    /// The worker reported an error for a request.
    Execution {
        /// The failed request.
        request_id: RequestId,
        /// What the worker reported.
        error: RemoteError,
    },
    /// The worker stopped while running a request, so whether it completed
    /// (and any side effects it had) is unknown. Runpy never replays it.
    OutcomeUnknown {
        /// The interrupted request.
        request_id: RequestId,
        /// Worker that was running it.
        worker_id: String,
    },
    /// The request was cancelled: it never ran, or the worker confirmed it
    /// stopped.
    Cancelled {
        /// The cancelled request.
        request_id: RequestId,
    },
}

impl fmt::Display for RunpyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid configuration: {message}"),
            Self::InvalidScript { script, reason } => match reason {
                InvalidScriptReason::NotSingleComponent => write!(
                    f,
                    "worker script '{script}' must be a single path component"
                ),
                InvalidScriptReason::NotFound(path) => write!(
                    f,
                    "worker script '{script}' does not exist at '{}'",
                    path.display()
                ),
            },
            Self::ScriptsDirectory { path, source } => write!(
                f,
                "scripts directory '{}' is unusable: {source}",
                path.display()
            ),
            Self::UvUnavailable { path, detail } => {
                write!(f, "uv at '{}' is unavailable: {detail}", path.display())
            }
            Self::InvalidEnvelope(error) => write!(f, "invalid envelope: {error}"),
            Self::FrameTooLarge { size, limit } => write!(
                f,
                "envelope of at least {size} bytes exceeds the {limit}-byte frame limit"
            ),
            Self::QueueFull => f.write_str("outbound queue is full"),
            Self::Timeout(stage) => write!(f, "timed out while {stage}"),
            Self::ManagerClosed => f.write_str("worker manager is no longer available"),
            Self::PoolClosed => f.write_str("worker pool is closed"),
            Self::WorkerExited { worker_id } => {
                write!(f, "worker '{worker_id}' is not running")
            }
            Self::Io(error) => write!(f, "I/O failed: {error}"),
            Self::Encode(error) => write!(f, "envelope could not be encoded: {error}"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Execution { request_id, error } => {
                write!(f, "request {request_id} failed: {error}")
            }
            Self::OutcomeUnknown {
                request_id,
                worker_id,
            } => write!(
                f,
                "worker '{worker_id}' stopped while running request {request_id}; \
                 its outcome is unknown"
            ),
            Self::Cancelled { request_id } => write!(f, "request {request_id} was cancelled"),
        }
    }
}

impl std::error::Error for RunpyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidEnvelope(error) => Some(error),
            Self::ScriptsDirectory { source, .. } => Some(source),
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
