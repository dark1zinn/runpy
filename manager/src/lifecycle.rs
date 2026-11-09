//! Worker lifecycle states shared between the control plane and handles.

use std::process::ExitStatus;
use std::sync::Arc;

use tokio::sync::watch;

use crate::error::RunpyError;

/// Wire protocol version this Manager speaks; a worker's `ready` must match.
pub(crate) const PROTOCOL_VERSION: u64 = 1;

/// Why a worker stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExitReason {
    /// The `uv` process exited on its own.
    Exited,
    /// Termination was requested, or a spawn was cancelled before it finished.
    Terminated,
    /// The connection closed or failed.
    Disconnected,
    /// The worker sent an invalid frame, envelope, or `ready`.
    ProtocolViolation,
    /// The worker did not report `ready` within the startup timeout.
    StartupTimeout,
    /// The Manager shut down.
    ManagerShutdown,
}

impl ExitReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "process exited",
            Self::Terminated => "terminated",
            Self::Disconnected => "disconnected",
            Self::ProtocolViolation => "protocol violation",
            Self::StartupTimeout => "startup timeout",
            Self::ManagerShutdown => "manager shutdown",
        }
    }
}

/// Final, retained outcome of one worker process.
///
/// Available from [`crate::Worker::wait`] for as long as any handle exists,
/// even after the worker was removed from the Manager.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerExit {
    /// Trusted worker identity.
    pub worker_id: String,
    /// Exit code of the `uv` process, when it exited normally.
    pub exit_code: Option<i32>,
    /// Signal that ended the `uv` process, when it was killed.
    pub signal: Option<i32>,
    /// Why Runpy considers the worker stopped.
    pub reason: ExitReason,
}

impl WorkerExit {
    pub(crate) fn new(worker_id: String, status: Option<ExitStatus>, reason: ExitReason) -> Self {
        use std::os::unix::process::ExitStatusExt;

        Self {
            worker_id,
            exit_code: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()),
            reason,
        }
    }
}

/// Observable state of one worker, published through a watch channel so a
/// late observer always sees the latest state.
#[derive(Clone, Debug)]
pub(crate) enum LifecycleState {
    /// Process started; `ready` not yet received.
    Starting,
    /// `ready` received with a supported protocol version.
    Ready {
        /// Capabilities the worker advertised.
        capabilities: Arc<[String]>,
    },
    /// The worker stopped; terminal.
    Exited(WorkerExit),
}

pub(crate) type LifecycleSender = Arc<watch::Sender<LifecycleState>>;
pub(crate) type LifecycleReceiver = watch::Receiver<LifecycleState>;

pub(crate) fn lifecycle_channel() -> (LifecycleSender, LifecycleReceiver) {
    let (sender, receiver) = watch::channel(LifecycleState::Starting);
    (Arc::new(sender), receiver)
}

/// Wait until the worker reaches a terminal state and return its exit.
pub(crate) async fn wait_for_exit(
    mut receiver: LifecycleReceiver,
    worker_id: &str,
) -> Result<WorkerExit, RunpyError> {
    let exited = |state: &LifecycleState| match state {
        LifecycleState::Exited(exit) => Some(exit.clone()),
        _ => None,
    };
    if let Ok(state) = receiver
        .wait_for(|state| matches!(state, LifecycleState::Exited(_)))
        .await
        && let Some(exit) = exited(&state)
    {
        return Ok(exit);
    }
    // The sender is gone; the last published state is still retained.
    exited(&receiver.borrow()).ok_or_else(|| RunpyError::WorkerExited {
        worker_id: worker_id.to_string(),
    })
}

/// Validate a `ready` payload and return the advertised capabilities.
///
/// Unknown capability names are kept but ignored by Runpy.
pub(crate) fn parse_ready(
    data: &serde_json::Map<String, serde_json::Value>,
) -> Result<Arc<[String]>, String> {
    match data
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
    {
        Some(PROTOCOL_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "worker speaks protocol version {other}; this Manager requires {PROTOCOL_VERSION}"
            ));
        }
        None => {
            return Err(format!(
                "ready must carry integer protocol_version {PROTOCOL_VERSION}"
            ));
        }
    }
    let capabilities = match data.get("capabilities") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "ready capabilities must be strings".to_string())
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("ready capabilities must be an array".to_string()),
    };
    Ok(capabilities.into())
}
