use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{tick, Receiver};

use super::registry::{BgTask, BgTaskRegistry, WatchdogPassCause};
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const FINISHED_RETENTION: Duration = Duration::from_secs(60 * 60);
/// How long periodic passes leave an exited PTY task to its reader's wake.
/// The reader normally reaches end-of-file right after the child exits, but a
/// background grandchild can keep the terminal open, so after this the
/// periodic pass finalizes the task itself. Windows tasks and killed tasks are
/// never left to the wake (see `BgTaskRegistry::pty_exit_awaiting_reader`).
const PTY_READER_WAKE_GRACE: Duration = Duration::from_secs(1);

thread_local! {
    /// The cause of the watchdog pass running on this thread, if any.
    static CURRENT_PASS: std::cell::Cell<Option<WatchdogPassCause>> =
        const { std::cell::Cell::new(None) };
}

/// The watchdog pass running on the calling thread, so a terminal transition
/// published inside a pass can record that pass before its completion is
/// visible. `None` off the watchdog thread.
pub(crate) fn current_pass() -> Option<WatchdogPassCause> {
    CURRENT_PASS.with(std::cell::Cell::get)
}

pub(crate) fn start(registry: BgTaskRegistry) {
    thread::spawn(move || {
        let ticker = tick(WATCHDOG_INTERVAL);
        let cleanup_ticker = tick(CLEANUP_INTERVAL);
        let wake_rx = registry.inner.wake_rx.clone();
        // PTY tasks a periodic pass has left to the reader's wake, mapped to
        // when that was first decided.
        let mut awaiting_reader: HashMap<String, Instant> = HashMap::new();
        while !registry.inner.shutdown.load(Ordering::SeqCst) {
            CURRENT_PASS.with(|pass| pass.set(None));
            let pass_cause = crossbeam_channel::select! {
                recv(ticker) -> tick => {
                    if tick.is_err() {
                        break;
                    }
                    WatchdogPassCause::Tick
                }
                recv(cleanup_ticker) -> _ => {
                    registry.cleanup_finished(FINISHED_RETENTION);
                    continue;
                }
                recv(wake_rx) -> _ => WatchdogPassCause::Wake,
            };
            let pass_cause = prefer_pending_wake(pass_cause, &wake_rx);
            CURRENT_PASS.with(|pass| pass.set(Some(pass_cause)));

            if registry.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }

            registry.evaluate_erased_watch_targets();

            let tasks = registry.running_tasks();
            awaiting_reader.retain(|task_id, _| tasks.iter().any(|task| &task.task_id == task_id));
            if tasks.is_empty() {
                continue;
            }

            for task in tasks {
                if leave_to_reader_wake(&registry, &task, pass_cause, &mut awaiting_reader) {
                    continue;
                }
                let _ = registry.poll_task(&task);
                registry.scan_task_watch_output(&task);
                // A kill that has released the state lock to signal and reap
                // the task owns its outcome and publishes the terminal state
                // itself. Keep watching the task until then rather than
                // retiring it as "no longer running" while it is `killing`.
                if task.kill_in_flight() {
                    continue;
                }
                if !task.is_running() {
                    registry.scan_task_watch_output(&task);
                    retire_terminal_task(&registry, &task, pass_cause, &mut awaiting_reader);
                    continue;
                }

                let timeout_expired = task
                    .state
                    .lock()
                    .ok()
                    .map(|state| {
                        state.metadata.remote.is_none()
                            && state.metadata.timeout_ms.is_some_and(|timeout_ms| {
                                task.elapsed_for_metadata(&state.metadata)
                                    >= Duration::from_millis(timeout_ms)
                            })
                    })
                    .unwrap_or(false);
                if timeout_expired {
                    let _ = registry.kill_for_timeout(&task.task_id, &task.session_id);
                    continue;
                }

                registry.maybe_emit_long_running_reminder(&task);
                // The PTY child may have exited since the poll above; the
                // reap below would finalize it, so give it the same chance
                // to complete through its wake.
                if leave_to_reader_wake(&registry, &task, pass_cause, &mut awaiting_reader) {
                    continue;
                }
                registry.reap_child(&task);
                // Record a completion the reap published in this same pass,
                // so the pass that observed it is the one on record.
                if !task.kill_in_flight() && !task.is_running() {
                    registry.scan_task_watch_output(&task);
                    retire_terminal_task(&registry, &task, pass_cause, &mut awaiting_reader);
                }
            }
        }
    });
}

/// Record which pass observed `task` terminal (if publishing it did not
/// already) and stop watching it.
fn retire_terminal_task(
    registry: &BgTaskRegistry,
    task: &BgTask,
    pass_cause: WatchdogPassCause,
    awaiting_reader: &mut HashMap<String, Instant>,
) {
    registry.record_completion_pass_cause(&task.task_id, pass_cause);
    awaiting_reader.remove(&task.task_id);
    registry.retire_watchdog_task(&task.task_id);
}

/// Whether a periodic pass should skip `task` this time: its PTY child has
/// exited but the reader is still draining output, and the reader's wake
/// (which follows within moments) should complete it with all output read.
/// Wake passes never skip, and after [`PTY_READER_WAKE_GRACE`] periodic
/// passes stop skipping, so a reader that never finishes cannot hold the
/// task open.
fn leave_to_reader_wake(
    registry: &BgTaskRegistry,
    task: &BgTask,
    pass_cause: WatchdogPassCause,
    awaiting_reader: &mut HashMap<String, Instant>,
) -> bool {
    if pass_cause != WatchdogPassCause::Tick || !registry.pty_exit_awaiting_reader(task) {
        return false;
    }
    let first_left = *awaiting_reader
        .entry(task.task_id.clone())
        .or_insert_with(Instant::now);
    first_left.elapsed() < PTY_READER_WAKE_GRACE
}

/// A pass the ticker selected also serves a wake that is already pending.
/// Crossbeam's `select!` picks at random among ready channels, so when the
/// watchdog thread runs late (a loaded machine) and both the ticker and a
/// wake are pending, about half of those passes would otherwise be recorded
/// as periodic, and the wake would only be consumed by a second, empty pass.
fn prefer_pending_wake(selected: WatchdogPassCause, wake_rx: &Receiver<()>) -> WatchdogPassCause {
    if selected == WatchdogPassCause::Tick && wake_rx.try_recv().is_ok() {
        WatchdogPassCause::Wake
    } else {
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_pass_with_a_pending_wake_counts_as_a_wake_pass() {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        wake_tx.send(()).unwrap();
        assert_eq!(
            prefer_pending_wake(WatchdogPassCause::Tick, &wake_rx),
            WatchdogPassCause::Wake
        );
        assert!(wake_rx.is_empty(), "the pass consumes the pending wake");
        assert_eq!(
            prefer_pending_wake(WatchdogPassCause::Tick, &wake_rx),
            WatchdogPassCause::Tick,
            "without a pending wake a tick pass stays a tick pass"
        );
    }
}
