//! Correlated execution requests: admission, one-at-a-time dispatch per
//! worker, deadlines, cancellation, and result delivery.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;
use tokio::sync::{Notify, OwnedSemaphorePermit, mpsc, oneshot};
use tokio::time::{Duration, Instant, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::lifecycle::{LifecycleReceiver, LifecycleState};
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
/// The request was cancelled before it was sent; it never runs.
const CANCELLED: u8 = 3;

/// Capability a worker advertises when it honours `cancel`.
const CANCEL_CAPABILITY: &str = "cancel";

/// Manager-unique identifier carried as `x_req` on `execute`, `done`,
/// `error`, `cancel`, and `cancelled` envelopes.
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

/// What [`Canceller::cancel`] achieved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cancellation {
    /// The request was cancelled: it never ran, or the worker stopped it.
    /// Its result is [`RunpyError::Cancelled`].
    Cancelled,
    /// The request was no longer running (it finished, failed, or was
    /// abandoned); its result reports how it ended.
    AlreadyFinished,
}

/// Terminal answer the session observed for the active request.
pub(crate) enum Completion {
    /// `done`; the envelope is shared with any inbound observers.
    Done(Arc<Envelope>),
    /// `error` carrying the same request ID.
    Failed(RemoteError),
    /// `cancelled` with status `cancelled`: the worker stopped the request.
    Cancelled,
}

/// The request currently executing on a worker.
pub(crate) struct ActiveRequest {
    pub id: RequestId,
    pub complete: oneshot::Sender<Completion>,
    /// Waiting [`Canceller`]; dropped unanswered if the request finishes
    /// another way, which the canceller reports as already finished.
    pub cancel: Option<oneshot::Sender<()>>,
}

/// Slot the session consults to match results to the active request.
pub(crate) type ActiveSlot = Arc<Mutex<Option<ActiveRequest>>>;

/// Where a dispatched request runs, recorded for cancellation.
struct Route {
    sender: MessageSender,
    active: ActiveSlot,
    cancellable: bool,
}

/// State shared by a request's caller handle, its cancellers, and the
/// dispatcher.
struct Tracker {
    state: AtomicU8,
    /// Wakes [`Request::result`] when a queued request is cancelled.
    cancelled: Notify,
    route: Mutex<Option<Route>>,
}

impl Tracker {
    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    fn transition(&self, from: u8, to: u8) -> bool {
        self.state
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// A request admitted to a worker and waiting for dispatch.
pub(crate) struct PendingRequest {
    id: RequestId,
    data: Data,
    deadline: Instant,
    tracker: Arc<Tracker>,
    reply: oneshot::Sender<Result<Data, RunpyError>>,
    /// Shared with the caller's [`Request`]: the Manager-wide pending count is
    /// released only when both the dispatcher and the caller are done.
    _permit: Arc<OwnedSemaphorePermit>,
}

impl PendingRequest {
    /// End-to-end deadline of this request.
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether the caller gave up on or cancelled the request while queued.
    pub(crate) fn is_abandoned(&self) -> bool {
        matches!(self.tracker.state(), ABANDONED | CANCELLED)
    }

    /// Fail a request that never reached a worker. No-op if the caller
    /// already abandoned or cancelled it; its handle reports that itself.
    pub(crate) fn fail_unsent(self, error: RunpyError) {
        if self.tracker.transition(QUEUED, ABANDONED) {
            let _ = self.reply.send(Err(error));
        }
    }

    /// Split a pooled request into the copy handed to one worker's
    /// dispatcher and a [`Relay`] that forwards its outcome to the caller.
    ///
    /// The pool awaits the returned receiver, so it learns when the worker
    /// finished even after the caller stopped waiting.
    pub(crate) fn relay(self) -> (Self, oneshot::Receiver<Result<Data, RunpyError>>, Relay) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let relay = Relay {
            id: self.id.clone(),
            tracker: self.tracker.clone(),
            reply: self.reply,
        };
        let inner = Self {
            id: self.id,
            data: self.data,
            deadline: self.deadline,
            tracker: self.tracker,
            reply: reply_tx,
            _permit: self._permit,
        };
        (inner, reply_rx, relay)
    }
}

/// Caller-facing end of a pooled request, see [`PendingRequest::relay`].
pub(crate) struct Relay {
    id: RequestId,
    tracker: Arc<Tracker>,
    reply: oneshot::Sender<Result<Data, RunpyError>>,
}

impl Relay {
    /// Forward the worker's outcome. A dropped dispatcher reply means the
    /// worker stopped: the outcome is unknown only if the request was sent.
    pub(crate) fn finish(
        self,
        outcome: Result<Result<Data, RunpyError>, oneshot::error::RecvError>,
        worker_id: &str,
    ) {
        let outcome = outcome.unwrap_or_else(|_| {
            Err(match self.tracker.state() {
                DISPATCHED => RunpyError::OutcomeUnknown {
                    request_id: self.id.clone(),
                    worker_id: worker_id.to_string(),
                },
                CANCELLED => RunpyError::Cancelled {
                    request_id: self.id.clone(),
                },
                _ => RunpyError::WorkerExited {
                    worker_id: worker_id.to_string(),
                },
            })
        });
        let _ = self.reply.send(outcome);
    }
}

/// Handle to one submitted request.
///
/// Dropping it discards the result but does not cancel execution; the worker
/// stays occupied until the request finishes. Use [`Request::canceller`] to
/// cancel it explicitly; a deadline alone never cancels a running request.
#[must_use = "dropping a request discards its result, not the execution"]
pub struct Request {
    id: RequestId,
    worker_id: String,
    deadline: Instant,
    tracker: Arc<Tracker>,
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
        let tracker = Arc::new(Tracker {
            state: AtomicU8::new(QUEUED),
            cancelled: Notify::new(),
            route: Mutex::new(None),
        });
        let permit = Arc::new(permit);
        let (reply_tx, reply_rx) = oneshot::channel();
        (
            Self {
                id: id.clone(),
                worker_id,
                deadline,
                tracker: tracker.clone(),
                reply: reply_rx,
                _permit: permit.clone(),
            },
            PendingRequest {
                id,
                data,
                deadline,
                tracker,
                reply: reply_tx,
                _permit: permit,
            },
        )
    }

    /// Manager-unique ID of this request.
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// A handle that can cancel this request while [`Request::result`]
    /// waits for it.
    pub fn canceller(&self) -> Canceller {
        Canceller {
            id: self.id.clone(),
            tracker: self.tracker.clone(),
        }
    }

    /// Wait for the result, at most until the request's deadline.
    ///
    /// # Errors
    ///
    /// - [`RunpyError::Execution`]: the worker reported an error.
    /// - [`RunpyError::Cancelled`]: the request was cancelled.
    /// - [`RunpyError::Timeout`] with [`TimeoutStage::Admission`] or
    ///   [`TimeoutStage::Queued`]: the request never reached the worker.
    /// - [`RunpyError::Timeout`] with [`TimeoutStage::Write`] or
    ///   [`TimeoutStage::Response`]: the request may be running; the worker
    ///   stays occupied until it finishes.
    /// - [`RunpyError::WorkerExited`]: the worker stopped before the request
    ///   was sent, so it never ran.
    /// - [`RunpyError::OutcomeUnknown`]: the worker stopped while running it.
    pub async fn result(mut self) -> Result<Data, RunpyError> {
        let cancelled = self.tracker.cancelled.notified();
        tokio::pin!(cancelled);
        cancelled.as_mut().enable();
        if self.tracker.state() == CANCELLED {
            return Err(self.cancelled());
        }
        tokio::select! {
            reply = timeout_at(self.deadline + REPLY_GRACE, &mut self.reply) => match reply {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(match self.tracker.state() {
                    DISPATCHED => RunpyError::OutcomeUnknown {
                        request_id: self.id,
                        worker_id: self.worker_id,
                    },
                    CANCELLED => RunpyError::Cancelled { request_id: self.id },
                    _ => RunpyError::WorkerExited {
                        worker_id: self.worker_id,
                    },
                }),
                Err(_) => Err(if self.tracker.transition(QUEUED, ABANDONED) {
                    RunpyError::Timeout(TimeoutStage::Queued)
                } else if self.tracker.state() == CANCELLED {
                    RunpyError::Cancelled { request_id: self.id }
                } else {
                    RunpyError::Timeout(TimeoutStage::Response)
                }),
            },
            () = cancelled => Err(self.cancelled()),
        }
    }

    fn cancelled(&self) -> RunpyError {
        RunpyError::Cancelled {
            request_id: self.id.clone(),
        }
    }
}

/// Cancels one request; obtained from [`Request::canceller`]. Cloning is
/// cheap.
#[derive(Clone)]
pub struct Canceller {
    id: RequestId,
    tracker: Arc<Tracker>,
}

impl fmt::Debug for Canceller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Canceller")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Canceller {
    /// Cancel the request.
    ///
    /// A request still queued is cancelled locally and never runs. A running
    /// request is cancelled only on a worker that advertised the `cancel`
    /// capability (the async Python SDK): Runpy sends `cancel` and waits,
    /// within the Manager's send timeout, until the worker reports that the
    /// request actually stopped. Cancellation is cooperative, so a worker may
    /// keep running a request that ignores it; the worker then stays occupied
    /// and this returns `Timeout(Response)`. `Cancelled` is never reported
    /// before execution stopped.
    ///
    /// ```no_run
    /// # async fn demo(worker: runpy::Worker, data: runpy::Data) -> Result<(), runpy::RunpyError> {
    /// use std::time::Duration;
    ///
    /// let request = worker.submit(data, Duration::from_secs(60)).await?;
    /// let canceller = request.canceller();
    /// tokio::spawn(async move {
    ///     tokio::time::sleep(Duration::from_secs(1)).await;
    ///     let _ = canceller.cancel().await;
    /// });
    /// match request.result().await {
    ///     Err(runpy::RunpyError::Cancelled { .. }) => println!("stopped"),
    ///     other => println!("{other:?}"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`RunpyError::Unsupported`] when the running worker cannot cancel,
    /// [`RunpyError::Timeout`] when `cancel` could not be written or the
    /// worker did not confirm in time, and send errors such as
    /// [`RunpyError::WorkerExited`].
    pub async fn cancel(&self) -> Result<Cancellation, RunpyError> {
        if self.tracker.transition(QUEUED, CANCELLED) {
            self.tracker.cancelled.notify_one();
            return Ok(Cancellation::Cancelled);
        }
        if self.tracker.state() != DISPATCHED {
            return Ok(Cancellation::AlreadyFinished);
        }

        // Dispatch records the route before it sends `execute`.
        let (sender, confirmed) = {
            let route = self.tracker.route.lock();
            let Some(route) = route.as_ref() else {
                return Ok(Cancellation::AlreadyFinished);
            };
            if !route.cancellable {
                return Err(RunpyError::Unsupported(
                    "the worker running this request did not advertise `cancel`".to_string(),
                ));
            }
            let mut active = route.active.lock();
            let Some(active) = active.as_mut().filter(|active| active.id == self.id) else {
                return Ok(Cancellation::AlreadyFinished);
            };
            let (confirm_tx, confirm_rx) = oneshot::channel();
            active.cancel = Some(confirm_tx);
            (route.sender.clone(), confirm_rx)
        };
        let deadline = Instant::now() + sender.send_timeout();
        sender
            .send_internal(Envelope::cancel(&self.id), deadline)
            .await?;
        match timeout_at(deadline, confirmed).await {
            Ok(Ok(())) => Ok(Cancellation::Cancelled),
            Ok(Err(_)) => Ok(Cancellation::AlreadyFinished),
            Err(_) => Err(RunpyError::Timeout(TimeoutStage::Response)),
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
/// A request whose deadline passed (or that was cancelled) while queued is
/// dropped without running. The worker stays occupied by a request until its
/// `done`, `error`, or `cancelled` arrives, even past the caller's deadline,
/// so it never runs two at once. When the worker stops, this task is aborted
/// and every waiter learns whether its request ran.
pub(crate) async fn run_dispatcher(
    worker_id: String,
    sender: MessageSender,
    mut queue: mpsc::UnboundedReceiver<PendingRequest>,
    active: ActiveSlot,
    lifecycle: LifecycleReceiver,
) {
    while let Some(request) = queue.recv().await {
        let PendingRequest {
            id,
            data,
            deadline,
            tracker,
            reply,
            _permit,
        } = request;
        let cancellable = matches!(
            &*lifecycle.borrow(),
            LifecycleState::Ready { capabilities }
                if capabilities.iter().any(|capability| capability == CANCEL_CAPABILITY)
        );
        // Record the route and the active slot before claiming, so a
        // canceller that observes DISPATCHED always finds both.
        *tracker.route.lock() = Some(Route {
            sender: sender.clone(),
            active: active.clone(),
            cancellable,
        });
        let (complete_tx, complete_rx) = oneshot::channel();
        *active.lock() = Some(ActiveRequest {
            id: id.clone(),
            complete: complete_tx,
            cancel: None,
        });
        let claimed = Instant::now() < deadline && tracker.transition(QUEUED, DISPATCHED);
        if !claimed {
            active.lock().take();
            tracker.route.lock().take();
            if tracker.transition(QUEUED, ABANDONED) {
                let _ = reply.send(Err(RunpyError::Timeout(TimeoutStage::Queued)));
            }
            continue;
        }

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
            Ok(Completion::Cancelled) => Err(RunpyError::Cancelled { request_id: id }),
            Err(_) => Err(RunpyError::OutcomeUnknown {
                request_id: id,
                worker_id: worker_id.clone(),
            }),
        };
        tracker.route.lock().take();
        // A caller that already timed out has dropped its receiver; the late
        // result is discarded and the worker is free for the next request.
        let _ = reply.send(outcome);
    }
}
