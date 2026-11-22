use std::time::{Duration, Instant};

use crate::context::AppContext;
use crate::feature_status::IndexEffective;

use super::{extensions::ReadinessWait, search_cancellation_requested, waits, SearchLaneStatus};

const FIRST_TEXT_INDEX_WAIT: Duration = Duration::from_secs(8);

/// Admission uses the same predicate as the wait, so only cold searches leave
/// the executor. A semantic lane that can already answer needs no text wait.
pub fn first_text_index_wait_needed(ctx: &AppContext) -> bool {
    let lanes = SearchLaneStatus::observe_current(ctx);
    lanes.trigram.effective == IndexEffective::Building
        && ctx
            .data_checkout()
            .is_some_and(|checkout| checkout.first_text_building())
        && !lanes.semantic.is_ready()
}

pub(super) fn wait_for_first_text_index(ctx: &AppContext) -> ReadinessWait {
    // Tool-provider admission hands cold searches to a continuation. A direct
    // executor caller must not park a worker that the index builder may need.
    if crate::executor::current_thread_is_executor_worker() {
        return ReadinessWait::Completed;
    }
    let Some(checkout) = ctx.data_checkout() else {
        return ReadinessWait::Completed;
    };
    let deadline = Instant::now() + FIRST_TEXT_INDEX_WAIT;
    let deadline =
        crate::bounded_io::current().map_or(deadline, |budget| deadline.min(budget.deadline));
    let mut generation = checkout.checkout().readiness_generation();
    waits::timed(waits::SearchWait::Readiness, || loop {
        if search_cancellation_requested() {
            #[cfg(test)]
            test_signals::signal_exit(|| test_signals::Exit::Cancelled);
            return ReadinessWait::Cancelled;
        }
        if Instant::now() >= deadline {
            #[cfg(test)]
            test_signals::signal_exit(|| test_signals::Exit::Timeout);
            return ReadinessWait::Completed;
        }
        if !first_text_index_wait_needed(ctx) {
            #[cfg(test)]
            test_signals::signal_exit(|| {
                if checkout.last_failure().is_some() {
                    test_signals::Exit::Failure
                } else {
                    test_signals::Exit::Ready
                }
            });
            return ReadinessWait::Completed;
        }
        // Publication and failure wake the checkout condition variable. The
        // job token wakes a separate condition variable, so poll its state
        // between checkout waits without resampling readiness on each poll.
        let observed = generation;
        while generation == observed && Instant::now() < deadline {
            #[cfg(test)]
            test_signals::signal_started();
            checkout.checkout().wait_for_readiness_change(
                &mut generation,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            );
            if search_cancellation_requested() {
                #[cfg(test)]
                test_signals::signal_exit(|| test_signals::Exit::Cancelled);
                return ReadinessWait::Cancelled;
            }
        }
    })
}

#[cfg(test)]
pub(super) mod test_signals {
    use std::cell::Cell;
    use std::sync::mpsc::Sender;
    use std::time::Instant;

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Exit {
        Failure,
        Cancelled,
        Ready,
        Timeout,
    }

    thread_local! {
        pub(crate) static STARTED: Cell<Option<Sender<()>>> = const { Cell::new(None) };
        pub(crate) static EXITED: Cell<Option<Sender<(Exit, Instant)>>> = const { Cell::new(None) };
    }

    pub(super) fn signal_started() {
        if let Some(started) = STARTED.take() {
            started.send(()).expect("first-text wait test is listening");
        }
    }

    pub(super) fn signal_exit(reason: impl FnOnce() -> Exit) {
        if let Some(exited) = EXITED.take() {
            let now = Instant::now();
            exited
                .send((reason(), now))
                .expect("first-text wait test is listening");
        }
    }
}
