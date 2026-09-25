use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crossbeam_channel::tick;

use super::registry::{BgTaskRegistry, WatchdogPassCause};
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const FINISHED_RETENTION: Duration = Duration::from_secs(60 * 60);

pub(crate) fn start(registry: BgTaskRegistry) {
    thread::spawn(move || {
        let ticker = tick(WATCHDOG_INTERVAL);
        let cleanup_ticker = tick(CLEANUP_INTERVAL);
        let wake_rx = registry.inner.wake_rx.clone();
        while !registry.inner.shutdown.load(Ordering::SeqCst) {
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

            if registry.inner.shutdown.load(Ordering::SeqCst) {
                break;
            }

            registry.evaluate_erased_watch_targets();

            let tasks = registry.running_tasks();
            if tasks.is_empty() {
                continue;
            }

            for task in tasks {
                let _ = registry.poll_task(&task);
                registry.scan_task_watch_output(&task);
                if !task.is_running() {
                    if let Ok(mut causes) = registry.inner.completion_pass_cause.lock() {
                        causes.entry(task.task_id.clone()).or_insert(pass_cause);
                    }
                    continue;
                }

                let timeout_expired = task
                    .state
                    .lock()
                    .ok()
                    .and_then(|state| state.metadata.timeout_ms)
                    .map(|timeout_ms| task.started.elapsed() >= Duration::from_millis(timeout_ms))
                    .unwrap_or(false);
                if timeout_expired {
                    let _ = registry.kill_for_timeout(&task.task_id, &task.session_id);
                    continue;
                }

                registry.maybe_emit_long_running_reminder(&task);
                registry.reap_child(&task);
            }
        }
    });
}
