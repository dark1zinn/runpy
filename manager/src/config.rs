//! Transport limits shared by the Manager and every managed worker.

use std::time::Duration;

/// Default maximum JSON body per frame: 8 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Default time allowed to finish a frame once its first byte arrives.
pub const DEFAULT_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Environment variable carrying the maximum frame body size to workers.
pub(crate) const ENV_MAX_FRAME_BYTES: &str = "RUNPY_MAX_FRAME_BYTES";
/// Environment variable carrying the partial-frame deadline to workers.
pub(crate) const ENV_FRAME_TIMEOUT_MS: &str = "RUNPY_FRAME_TIMEOUT_MS";

/// Largest supported body so a whole frame fits one byte-budget reservation.
const MAX_FRAME_BYTES_CEILING: usize = u32::MAX as usize - 8;

/// Default bound on admitted outbound frames per worker connection.
pub(crate) const DEFAULT_OUTBOUND_CAPACITY: usize = 64;
/// Default Manager-wide bound on encoded outbound bytes awaiting a write.
pub(crate) const DEFAULT_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Default deadline covering outbound admission and the socket write.
pub(crate) const DEFAULT_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Default time a worker gets to exit after termination starts.
pub(crate) const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Bounds on outbound memory and time shared by every worker route.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueueLimits {
    /// Admitted frames per worker that may wait for the writer.
    pub outbound_capacity: usize,
    /// Encoded bytes, across all workers, admitted but not yet written.
    pub queued_bytes: usize,
    /// Deadline from the start of a send until its write completes.
    pub send_timeout: Duration,
    /// Deadline from the start of termination until the group is killed.
    pub shutdown_grace: Duration,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            outbound_capacity: DEFAULT_OUTBOUND_CAPACITY,
            queued_bytes: DEFAULT_QUEUED_BYTES,
            send_timeout: DEFAULT_SEND_TIMEOUT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }
}

/// Frame size and partial-frame deadline enforced in both directions.
///
/// The Manager rejects an oversized length header before allocating, refuses
/// to serialize an outbound envelope past the limit, and closes a connection
/// whose frame does not complete within `frame_timeout` after its first byte.
/// An idle connection has no deadline. Managed workers receive the same values
/// through `RUNPY_MAX_FRAME_BYTES` and `RUNPY_FRAME_TIMEOUT_MS`.
///
/// ```
/// use runpy::ProtocolLimits;
/// use std::time::Duration;
///
/// let limits = ProtocolLimits::new(1024 * 1024, Duration::from_secs(5)).unwrap();
/// assert_eq!(limits.max_frame_bytes(), 1024 * 1024);
/// assert!(ProtocolLimits::new(0, Duration::from_secs(5)).is_err());
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProtocolLimits {
    max_frame_bytes: usize,
    frame_timeout: Duration,
}

impl ProtocolLimits {
    /// Validate and build limits.
    ///
    /// Returns an error when either value is zero, the frame size exceeds
    /// `u32::MAX - 8` bytes, or the timeout is shorter than one millisecond,
    /// which workers could not represent.
    pub fn new(max_frame_bytes: usize, frame_timeout: Duration) -> Result<Self, String> {
        if max_frame_bytes == 0 {
            return Err("max_frame_bytes must be greater than zero".to_string());
        }
        if max_frame_bytes > MAX_FRAME_BYTES_CEILING {
            return Err(format!(
                "max_frame_bytes must not exceed {MAX_FRAME_BYTES_CEILING}"
            ));
        }
        if frame_timeout.as_millis() == 0 {
            return Err("frame_timeout must be at least one millisecond".to_string());
        }
        Ok(Self {
            max_frame_bytes,
            frame_timeout,
        })
    }

    /// Maximum JSON body size of one frame, excluding the 8-byte header.
    pub fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Time allowed to finish a frame after its first byte arrives.
    pub fn frame_timeout(&self) -> Duration {
        self.frame_timeout
    }

    /// Environment entries that propagate these limits to a managed worker.
    pub(crate) fn worker_env(&self) -> [(&'static str, String); 2] {
        [
            (ENV_MAX_FRAME_BYTES, self.max_frame_bytes.to_string()),
            (
                ENV_FRAME_TIMEOUT_MS,
                self.frame_timeout.as_millis().to_string(),
            ),
        ]
    }
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            frame_timeout: DEFAULT_FRAME_TIMEOUT,
        }
    }
}
