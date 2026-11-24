//! Cancellation of a shared Tier-2 flight.
//!
//! Several `aft_inspect` calls join one flight per category. The flight must
//! not stop because one of them was withdrawn: it stops when every joined
//! caller has been cancelled (each caller's token also reports an abandoned
//! or deleted root), and a stop for cancellation is not a builder failure.

use std::cell::RefCell;
use std::sync::Arc;

use super::{builder_failure_terminal, callgraph_unavailable_payload, InspectManager, Waiter};
use crate::executor::JobCancellation;
use crate::inspect::job::{JobKey, JobOutcome};

type CancelCheck = Arc<dyn Fn() -> bool + Send + Sync>;

thread_local! {
    /// Set on the thread running a shared flight: true once every joined
    /// caller has been cancelled.
    static FLIGHT_CALLERS_CANCELLED: RefCell<Option<CancelCheck>> = const { RefCell::new(None) };
}

impl Waiter {
    /// A waiter for the calling thread, carrying that caller's cancellation.
    pub(super) fn new(tx: super::WaiterTx) -> Self {
        Self {
            tx,
            cancellation: crate::executor::current_job_cancellation(),
        }
    }
}

impl InspectManager {
    fn flight_callers_cancelled(&self, key: &JobKey) -> bool {
        let tokens = self.in_flight.lock().ok().and_then(|in_flight| {
            in_flight.get(key).map(|registration| {
                registration
                    .waiters
                    .iter()
                    .map(|waiter| waiter.cancellation.clone())
                    .collect::<Vec<_>>()
            })
        });
        // A caller without a token can never be cancelled, so it keeps the
        // flight running; so does a flight nobody waits for.
        tokens.is_some_and(|tokens| {
            !tokens.is_empty()
                && tokens.iter().all(|token| {
                    token
                        .as_ref()
                        .is_some_and(JobCancellation::cancel_requested_before_commit)
                })
        })
    }
}

/// Mark this thread as running the shared flight for `key` until the guard
/// drops.
pub(super) fn enter(manager: &Arc<InspectManager>, key: JobKey) -> FlightGuard {
    let manager = Arc::clone(manager);
    let check: CancelCheck = Arc::new(move || manager.flight_callers_cancelled(&key));
    FlightGuard(FLIGHT_CALLERS_CANCELLED.with(|slot| slot.replace(Some(check))))
}

pub(super) struct FlightGuard(Option<CancelCheck>);

impl Drop for FlightGuard {
    fn drop(&mut self) {
        FLIGHT_CALLERS_CANCELLED.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

/// The cancellation check for work on this thread, usable from another
/// thread. Outside a shared flight it is the installed job token's check.
/// Inside one, it is "every joined caller was cancelled", or the flight's own
/// token (which carries the root, not any caller's request) being cancelled.
pub(super) fn current() -> impl Fn() -> bool + Send + Sync + 'static {
    let callers_cancelled = FLIGHT_CALLERS_CANCELLED.with(|slot| slot.borrow().clone());
    let token = crate::executor::current_job_cancellation();
    move || {
        if callers_cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled())
        {
            // Let the flight's own checkpoints see it too.
            if let Some(token) = &token {
                token.request_cancel();
            }
            return true;
        }
        token
            .as_ref()
            .is_some_and(JobCancellation::cancel_requested_before_commit)
    }
}

/// [`current`], checked once.
pub(super) fn cancelled() -> bool {
    current()()
}

pub(super) enum BuilderAttemptTerminal {
    Succeeded,
    CacheWriterLeaseWait,
    /// The published call graph view's database was locked.
    ViewBusyWait,
    Failed(String),
    Inconclusive,
}

/// Messages of a Tier-2 run stopped because its callers were cancelled or its
/// root was abandoned. Such a stop says nothing about whether the build works.
fn is_cancellation(message: &str) -> bool {
    message.starts_with("tier2 pass cancelled")
        || message == "explicit inspect Tier-2 cold-build admission was cancelled"
}

pub(super) fn builder_attempt_terminal(outcome: &JobOutcome) -> BuilderAttemptTerminal {
    match outcome {
        JobOutcome::Fresh { payload }
            if payload["status"] == "pending" && payload["code"] == "callgraph_building" =>
        {
            BuilderAttemptTerminal::ViewBusyWait
        }
        JobOutcome::Fresh { payload } if payload["status"] == "pending" => {
            BuilderAttemptTerminal::Inconclusive
        }
        JobOutcome::Fresh { payload } if callgraph_unavailable_payload(payload) => {
            BuilderAttemptTerminal::Failed("callgraph_unavailable".to_string())
        }
        JobOutcome::Fresh { .. } => BuilderAttemptTerminal::Succeeded,
        JobOutcome::Failed { message } if message.contains("writer_lease_timeout:") => {
            BuilderAttemptTerminal::CacheWriterLeaseWait
        }
        JobOutcome::Failed { message } if is_cancellation(message) => {
            BuilderAttemptTerminal::Inconclusive
        }
        JobOutcome::Failed { message } => {
            BuilderAttemptTerminal::Failed(builder_failure_terminal(message))
        }
        JobOutcome::Stale { .. } | JobOutcome::Pending { .. } => {
            BuilderAttemptTerminal::Inconclusive
        }
    }
}
