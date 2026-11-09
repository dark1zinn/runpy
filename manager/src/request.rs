//! Correlated execution requests: admission, one-at-a-time dispatch per
//! worker, deadlines, and result delivery.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};
use tokio::time::{Duration, Instant, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::protocol::{Data, Envelope, MessageSender};

/// Extra time a [`Request`] waits past its deadline so an outcome the
/// dispatcher already determined (for example "never sent") wins over the
/// caller's own timer.
const REPLY_GRACE: Duration = Duration::from_millis(100);

/// Request is waiting for the worker to become idle.
const QUEUED: u8 = 0;
/// The `execute` frame is being or has been sent.
const DISPATCHED: u8 = 1;
/// The caller gave up while the request was still queued; it never runs.
const ABANDONED: u8 = 2;

/// Manager-unique identifier carried as `x_req` on `execute`, `done`, and
/// `error` envelopes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RequestId(String);

impl RequestId {
    pub(crate) fn new(sequence: u64) -> Self {
        Self(sequence.to_string())
    }

    /// The identifier as sent on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Structured failure a worker reported for one request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteError {
    /// Exception type name, for example `ValueError`.
    pub kind: String,
    /// Human-readable message.
    pub message: String,
}

impl RemoteError {
    pub(crate) fn from_data(data: &Data) -> Self {
        let field = |name: &str| {
            data.get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Self {
            kind: field("kind"),
            message: field("message"),
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

/// Terminal answer the session observed for the active request.
pub(crate) enum Completion {
    /// `done`; the envelope is shared with any inbound observers.
    Done(Arc<Envelope>),
    /// `error` carrying the same request ID.
    Failed(RemoteError),
}

/// The request currently executing on a worker.
pub(crate) struct ActiveRequest {
    pub id: RequestId,
    pub complete: oneshot::Sender<Completion>,
}

/// Slot the session consults to match `done`/`error` to the active request.
pub(crate) type ActiveSlot = Arc<Mutex<Option<ActiveRequest>>>;

/// A request admitted to a worker and waiting for dispatch.
pub(crate) struct PendingRequest {
    id: RequestId,
    data: Data,
    deadline: Instant,
    state: Arc<AtomicU8>,
    reply: oneshot::Sender<Result<Data, RunpyError>>,
    /// Shared with the caller's [`Request`]: the Manager-wide pending count is
    /// released only when both the dispatcher and the caller are done.
    _permit: Arc<OwnedSemaphorePermit>,
}

/// Handle to one submitted request.
///
/// Dropping it discards the result but does not cancel execution; the worker
/// stays occupied until the request finishes.
#[must_use = "dropping a request discards its result, not the execution"]
pub struct Request {
    id: RequestId,
    worker_id: String,
    deadline: Instant,
    state: Arc<AtomicU8>,
    reply: oneshot::Receiver<Result<Data, RunpyError>>,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("id", &self.id)
            .field("worker_id", &self.worker_id)
            .finish_non_exhaustive()
    }
}

impl Request {
    /// Pair a pending request with its caller handle.
    pub(crate) fn new(
        id: RequestId,
        worker_id: String,
        data: Data,
        deadline: Instant,
        permit: OwnedSemaphorePermit,
    ) -> (Self, PendingRequest) {
        let state = Arc::new(AtomicU8::new(QUEUED));
        let permit = Arc::new(permit);
        let (reply_tx, reply_rx) = oneshot::channel();
        (
            Self {
                id: id.clone(),
                worker_id,
                deadline,
                state: state.clone(),
                reply: reply_rx,
                _permit: permit.clone(),
            },
            PendingRequest {
                id,
                data,
                deadline,
                state,
                reply: reply_tx,
                _permit: permit,
            },
        )
    }

    /// Manager-unique ID of this request.
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// Wait for the result, at most until the request's deadline.
    ///
    /// # Errors
    ///
    /// - [`RunpyError::Execution`]: the worker reported an error.
    /// - [`RunpyError::Timeout`] with [`TimeoutStage::Admission`] or
    ///   [`TimeoutStage::Queued`]: the request never reached the worker.
    /// - [`RunpyError::Timeout`] with [`TimeoutStage::Write`] or
    ///   [`TimeoutStage::Response`]: the request may be running; the worker
    ///   stays occupied until it finishes.
    /// - [`RunpyError::WorkerExited`]: the worker stopped before the request
    ///   was sent, so it never ran.
    /// - [`RunpyError::OutcomeUnknown`]: the worker stopped while running it.
    pub async fn result(mut self) -> Result<Data, RunpyError> {
        match timeout_at(self.deadline + REPLY_GRACE, &mut self.reply).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(if self.state.load(Ordering::Acquire) == DISPATCHED {
                RunpyError::OutcomeUnknown {
                    request_id: self.id,
                    worker_id: self.worker_id,
                }
            } else {
                RunpyError::WorkerExited {
                    worker_id: self.worker_id,
                }
            }),
            Err(_) => Err(RunpyError::Timeout(
                match self.state.compare_exchange(
                    QUEUED,
                    ABANDONED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => TimeoutStage::Queued,
                    Err(_) => TimeoutStage::Response,
                },
            )),
        }
    }
}

/// Whether a failed send left the worker untouched (nothing was written).
fn never_written(error: &RunpyError) -> bool {
    matches!(
        error,
        RunpyError::Timeout(TimeoutStage::Admission | TimeoutStage::Queued)
            | RunpyError::WorkerExited { .. }
            | RunpyError::ManagerClosed
            | RunpyError::FrameTooLarge { .. }
            | RunpyError::Encode(_)
    )
}

/// Dispatch a worker's requests one at a time, in submission order.
///
/// A request whose deadline passed while queued is dropped without running.
/// The worker stays occupied by a request until its `done`/`error` arrives,
/// even past the caller's deadline, so it never runs two at once. When the
/// worker stops, this task is aborted and every waiter learns whether its
/// request ran.
pub(crate) async fn run_dispatcher(
    worker_id: String,
    sender: MessageSender,
    mut queue: mpsc::UnboundedReceiver<PendingRequest>,
    active: ActiveSlot,
) {
    while let Some(request) = queue.recv().await {
        let PendingRequest {
            id,
            data,
            deadline,
            state,
            reply,
            _permit,
        } = request;
        let claimed = Instant::now() < deadline
            && state
                .compare_exchange(QUEUED, DISPATCHED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        if !claimed {
            if state
                .compare_exchange(QUEUED, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let _ = reply.send(Err(RunpyError::Timeout(TimeoutStage::Queued)));
            }
            continue;
        }

        let (complete_tx, complete_rx) = oneshot::channel();
        *active.lock() = Some(ActiveRequest {
            id: id.clone(),
            complete: complete_tx,
        });
        if let Err(error) = sender
            .send_internal(Envelope::request(&id, data), deadline)
            .await
        {
            active.lock().take();
            let error = if never_written(&error) {
                error
            } else {
                RunpyError::OutcomeUnknown {
                    request_id: id,
                    worker_id: worker_id.clone(),
                }
            };
            let _ = reply.send(Err(error));
            continue;
        }

        let outcome = match complete_rx.await {
            Ok(Completion::Done(envelope)) => Ok(Arc::try_unwrap(envelope)
                .map_or_else(|shared| shared.data().clone(), Envelope::into_data)),
            Ok(Completion::Failed(error)) => Err(RunpyError::Execution {
                request_id: id,
                error,
            }),
            Err(_) => Err(RunpyError::OutcomeUnknown {
                request_id: id,
                worker_id: worker_id.clone(),
            }),
        };
        // A caller that already timed out has dropped its receiver; the late
        // result is discarded and the worker is free for the next request.
        let _ = reply.send(outcome);
    }
}
