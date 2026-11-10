//! Fair distribution of Manager-wide worker slots across pools.
//!
//! Pools that need a new worker beyond their reserved minimum ask the
//! scheduler for a process slot. Slots are granted round-robin across pools
//! with waiting work (FIFO within a pool), so a busy pool cannot starve
//! another. When no slot is free, the scheduler asks another pool to retire
//! one idle worker above its minimum. Running requests and reserved minima
//! are never preempted.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore, TryAcquireError, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep, timeout_at};

use crate::error::{RunpyError, TimeoutStage};
use crate::pool::PoolCore;
use crate::protocol::ProcessSlot;

/// How often a waiting grant retries reclaiming idle surplus workers.
const RECLAIM_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct State {
    /// Waiting requesters per pool ID, oldest first.
    waiting: BTreeMap<u64, VecDeque<oneshot::Sender<ProcessSlot>>>,
    /// Pool that received the most recent grant.
    last: Option<u64>,
    /// Registered pools; dead entries are pruned on registration.
    pools: Vec<(u64, Weak<PoolCore>)>,
}

impl State {
    /// Drop requesters that stopped waiting.
    fn prune(&mut self) {
        for queue in self.waiting.values_mut() {
            queue.retain(|sender| !sender.is_closed());
        }
        self.waiting.retain(|_, queue| !queue.is_empty());
    }

    /// The next pool after the last grantee that has a live requester.
    fn next_pool(&self) -> Option<u64> {
        let after = self.last.map_or(0, |last| last + 1);
        self.waiting
            .range(after..)
            .chain(self.waiting.range(..after))
            .map(|(pool, _)| *pool)
            .next()
    }
}

/// Manager-owned scheduler shared by all pools.
pub(crate) struct Scheduler {
    processes: Arc<Semaphore>,
    state: Mutex<State>,
    wake: Notify,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Scheduler {
    pub(crate) fn new(processes: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            processes,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            task: Mutex::new(None),
        })
    }

    /// Track a pool so it can be asked to free idle capacity and stopped on
    /// Manager drop.
    pub(crate) fn register(self: &Arc<Self>, pool: u64, core: Weak<PoolCore>) {
        let mut state = self.state.lock();
        state.pools.retain(|(_, pool)| pool.strong_count() > 0);
        state.pools.push((pool, core));
        drop(state);
        let mut task = self.task.lock();
        if task.is_none() {
            *task = Some(tokio::spawn(grant(self.clone())));
        }
    }

    /// Wait for a process slot on behalf of `pool` until `deadline`.
    ///
    /// Returns `None` when the deadline passed or the Manager shut down; the
    /// caller decides how to report it.
    pub(crate) async fn acquire(&self, pool: u64, deadline: Instant) -> Option<ProcessSlot> {
        let (sender, receiver) = oneshot::channel();
        self.state
            .lock()
            .waiting
            .entry(pool)
            .or_default()
            .push_back(sender);
        self.wake.notify_one();
        timeout_at(deadline, receiver).await.ok()?.ok()
    }

    /// Stop the grant task and every registered pool. Used on Manager drop.
    pub(crate) fn shutdown_now(&self) {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        let pools = std::mem::take(&mut self.state.lock().pools);
        for (_, pool) in pools {
            if let Some(pool) = pool.upgrade() {
                pool.shutdown_now();
            }
        }
        self.state.lock().waiting.clear();
    }

    /// Ask one pool other than `except` to retire an idle worker above its
    /// minimum, freeing a Manager-wide slot. Returns whether one was retired.
    fn reclaim_idle(&self, except: Option<u64>) -> bool {
        let pools: Vec<Arc<PoolCore>> = self
            .state
            .lock()
            .pools
            .iter()
            .filter(|(id, _)| Some(*id) != except)
            .filter_map(|(_, pool)| pool.upgrade())
            .collect();
        pools.iter().any(|pool| pool.retire_surplus_idle())
    }
}

/// Grant slots one at a time, round-robin across pools with live requesters.
///
/// Holding the scheduler is fine: Manager drop aborts this task.
async fn grant(this: Arc<Scheduler>) {
    loop {
        // Register for a wake-up before checking, so no request is missed.
        let woken = this.wake.notified();
        let next = {
            let mut state = this.state.lock();
            state.prune();
            state.next_pool()
        };
        let Some(next) = next else {
            woken.await;
            continue;
        };

        let mut slot = match this.processes.clone().try_acquire_owned() {
            Ok(slot) => slot,
            Err(TryAcquireError::Closed) => return,
            Err(TryAcquireError::NoPermits) => {
                // One acquire future for the whole wait keeps this grant's
                // place in the semaphore queue. Idle surplus elsewhere is
                // retired at most once per grant; until one is found, retry
                // periodically, since other pools may park workers.
                let acquire = this.processes.clone().acquire_owned();
                tokio::pin!(acquire);
                let mut reclaimed = false;
                loop {
                    if !reclaimed {
                        reclaimed = this.reclaim_idle(Some(next));
                    }
                    tokio::select! {
                        slot = &mut acquire => match slot {
                            Ok(slot) => break slot,
                            Err(_) => return,
                        },
                        () = sleep(RECLAIM_INTERVAL), if !reclaimed => {}
                    }
                }
            }
        };

        // Hand the slot to the next live requester in round-robin order; an
        // unclaimed slot returns to the Manager when dropped.
        loop {
            let recipient = {
                let mut state = this.state.lock();
                state.prune();
                let Some(pool) = state.next_pool() else { break };
                let queue = state.waiting.get_mut(&pool).expect("pool has requesters");
                let sender = queue.pop_front().expect("queues are non-empty");
                state.last = Some(pool);
                sender
            };
            match recipient.send(slot) {
                Ok(()) => break,
                Err(returned) => slot = returned,
            }
        }
    }
}

impl Scheduler {
    /// Why [`Scheduler::acquire`] returned `None`.
    pub(crate) fn unavailable(&self) -> RunpyError {
        if self.processes.is_closed() {
            RunpyError::ManagerClosed
        } else {
            RunpyError::Timeout(TimeoutStage::Queued)
        }
    }
}
