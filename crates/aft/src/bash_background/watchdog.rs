use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{tick, Receiver};

use super::registry::{BgTask, BgTaskRegistry, WatchdogPassCause};
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
pub(super) const FINISHED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
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

/// Registries subscribe to one process timer and wake multiplexer. Weak entries
/// cannot keep an unloaded project (and all its finished history) alive forever.
pub(crate) fn start(registry: BgTaskRegistry) {
    if scheduler()
        .send(std::sync::Arc::downgrade(&registry.inner))
        .is_err()
    {
        crate::slog_warn!("bash watchdog scheduler stopped");
    }
}

fn start_cleanup_with(
    cleaning: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    work: impl FnOnce() + Send + 'static,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) {
    struct Reset(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    let reset = Reset(cleaning.clone());
    if let Err(error) = spawn(Box::new(move || {
        let _reset = reset;
        work();
    })) {
        crate::slog_warn!("failed to start bash cleanup: {error}");
    }
}

struct Subscription {
    registry: std::sync::Weak<super::registry::RegistryInner>,
    wake_rx: Receiver<()>,
    awaiting_reader: HashMap<String, Instant>,
}

type TimeoutKill = (BgTaskRegistry, std::sync::Arc<BgTask>, Option<String>);

fn timeout_kills() -> Option<crossbeam_channel::Sender<TimeoutKill>> {
    static KILLS: std::sync::OnceLock<
        std::sync::Mutex<Option<crossbeam_channel::Sender<TimeoutKill>>>,
    > = std::sync::OnceLock::new();
    let mut queue = KILLS
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(queue) = queue.as_ref() {
        return Some(queue.clone());
    }
    let started = {
        let (tx, rx) = crossbeam_channel::bounded::<TimeoutKill>(64);
        let mut workers = 0;
        for index in 0..4 {
            let rx = rx.clone();
            match thread::Builder::new()
                .name(format!("aft-bash-timeout-{index}"))
                .spawn(move || {
                    for (registry, task, memory_reason) in rx {
                        struct Reset(std::sync::Arc<BgTask>);
                        impl Drop for Reset {
                            fn drop(&mut self) {
                                self.0.timeout_kill_queued.store(false, Ordering::SeqCst);
                            }
                        }
                        let _reset = Reset(task.clone());
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if !task.is_terminal() {
                                if let Some(reason) = memory_reason {
                                    registry.kill_for_memory(&task, reason);
                                } else {
                                    let _ =
                                        registry.kill_for_timeout(&task.task_id, &task.session_id);
                                }
                            }
                        }));
                    }
                }) {
                Ok(_) => workers += 1,
                Err(error) => crate::slog_warn!("failed to start bash timeout worker: {error}"),
            }
        }
        (workers > 0).then_some(tx)
    };
    *queue = started.clone();
    started
}

fn schedule_timeout_kill(registry: &BgTaskRegistry, task: &std::sync::Arc<BgTask>) {
    schedule_limit_kill(registry, task, None);
}

pub(super) fn schedule_memory_kill(
    registry: &BgTaskRegistry,
    task: &std::sync::Arc<BgTask>,
    reason: String,
) {
    schedule_limit_kill(registry, task, Some(reason));
}

fn schedule_limit_kill(
    registry: &BgTaskRegistry,
    task: &std::sync::Arc<BgTask>,
    memory_reason: Option<String>,
) {
    if task.timeout_kill_queued.swap(true, Ordering::SeqCst) {
        return;
    }
    if !timeout_kills().is_some_and(|queue| {
        queue
            .try_send((registry.clone(), task.clone(), memory_reason))
            .is_ok()
    }) {
        task.timeout_kill_queued.store(false, Ordering::SeqCst);
    }
}

fn scheduler() -> &'static crossbeam_channel::Sender<std::sync::Weak<super::registry::RegistryInner>>
{
    static SCHEDULER: std::sync::OnceLock<
        crossbeam_channel::Sender<std::sync::Weak<super::registry::RegistryInner>>,
    > = std::sync::OnceLock::new();
    SCHEDULER.get_or_init(|| {
        let (tx, rx) =
            crossbeam_channel::unbounded::<std::sync::Weak<super::registry::RegistryInner>>();
        thread::Builder::new()
            .name("aft-bash-watchdog".into())
            .spawn(move || {
                let ticker = tick(WATCHDOG_INTERVAL);
                let cleanup_ticker = tick(CLEANUP_INTERVAL);
                let mut subscriptions: Vec<Subscription> = Vec::new();
                let cleaning = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                loop {
                    subscriptions.retain(|entry| {
                        entry
                            .registry
                            .upgrade()
                            .is_some_and(|inner| !inner.shutdown.load(Ordering::SeqCst))
                    });
                    let (selected, registration) = {
                        let mut select = crossbeam_channel::Select::new();
                        select.recv(&rx);
                        select.recv(&ticker);
                        select.recv(&cleanup_ticker);
                        for entry in &subscriptions {
                            select.recv(&entry.wake_rx);
                        }
                        let operation = select.select();
                        let index = operation.index();
                        let mut registration = None;
                        match index {
                            0 => {
                                registration = operation.recv(&rx).ok();
                            }
                            1 => {
                                let _ = operation.recv(&ticker);
                            }
                            2 => {
                                let _ = operation.recv(&cleanup_ticker);
                            }
                            _ => {
                                let _ = operation.recv(&subscriptions[index - 3].wake_rx);
                            }
                        }
                        (index, registration)
                    };
                    if let Some(registry) = registration {
                        if let Some(inner) = registry.upgrade() {
                            subscriptions.push(Subscription {
                                registry,
                                wake_rx: inner.wake_rx.clone(),
                                awaiting_reader: HashMap::new(),
                            });
                        }
                    }
                    if selected == 0 {
                        continue;
                    }
                    if selected == 2 {
                        // Filesystem deletion and persisted GC must not delay every
                        // project's exit observation. At most one cleanup runs.
                        if !cleaning.swap(true, Ordering::SeqCst) {
                            let registries: Vec<_> = subscriptions
                                .iter()
                                .map(|entry| entry.registry.clone())
                                .collect();
                            start_cleanup_with(
                                &cleaning,
                                move || {
                                    for inner in registries
                                        .into_iter()
                                        .filter_map(|registry| registry.upgrade())
                                    {
                                        let registry = BgTaskRegistry { inner };
                                        registry.sweep_retained_scratch();
                                        registry.cleanup_finished(FINISHED_RETENTION);
                                        registry.request_recurring_persisted_gc();
                                    }
                                },
                                |work| {
                                    thread::Builder::new()
                                        .name("aft-bash-cleanup".into())
                                        .spawn(work)
                                        .map(|_| ())
                                },
                            );
                        }
                        continue;
                    }
                    for (index, entry) in subscriptions.iter_mut().enumerate() {
                        if selected >= 3 && index != selected - 3 {
                            continue;
                        }
                        let Some(inner) = entry.registry.upgrade() else {
                            continue;
                        };
                        let registry = BgTaskRegistry { inner };
                        if registry.inner.shutdown.load(Ordering::SeqCst) {
                            continue;
                        }
                        let cause = prefer_pending_wake(
                            if selected == 1 {
                                WatchdogPassCause::Tick
                            } else {
                                WatchdogPassCause::Wake
                            },
                            &entry.wake_rx,
                        );
                        // A broken project must not disable exits for every other root.
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_pass(&registry, cause, &mut entry.awaiting_reader)
                        }))
                        .is_err()
                        {
                            log::error!("background watchdog pass panicked");
                        }
                        CURRENT_PASS.with(|pass| pass.set(None));
                    }
                }
            })
            .expect("failed to start bash watchdog");
        tx
    })
}

pub(super) fn run_pass(
    registry: &BgTaskRegistry,
    pass_cause: WatchdogPassCause,
    awaiting_reader: &mut HashMap<String, Instant>,
) {
    CURRENT_PASS.with(|pass| pass.set(Some(pass_cause)));
    registry.evaluate_erased_watch_targets();
    let tasks = registry.running_tasks();
    awaiting_reader.retain(|task_id, _| tasks.iter().any(|task| &task.task_id == task_id));
    for task in tasks {
        if let Some(reason) = registry.memory_limit_exceeded(&task) {
            schedule_memory_kill(registry, &task, reason);
            continue;
        }
        if leave_to_reader_wake(registry, &task, pass_cause, awaiting_reader) {
            continue;
        }
        let _ = registry.poll_task(&task);
        registry.scan_task_watch_output(&task);
        if task.kill_in_flight() {
            continue;
        }
        if !task.is_running() {
            registry.scan_task_watch_output(&task);
            retire_terminal_task(registry, &task, pass_cause, awaiting_reader);
            continue;
        }
        let timeout_expired = task
            .lock_state()
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
            schedule_timeout_kill(registry, &task);
            continue;
        }
        registry.maybe_emit_long_running_reminder(&task);
        if leave_to_reader_wake(registry, &task, pass_cause, awaiting_reader) {
            continue;
        }
        registry.reap_child(&task);
        if !task.kill_in_flight() && !task.is_running() {
            registry.scan_task_watch_output(&task);
            retire_terminal_task(registry, &task, pass_cause, awaiting_reader);
        }
    }
}

/// Record which pass observed `task` terminal (if publishing it did not
/// already) and stop watching it.
fn retire_terminal_task(
    registry: &BgTaskRegistry,
    task: &BgTask,
    pass_cause: WatchdogPassCause,
    awaiting_reader: &mut HashMap<String, Instant>,
) {
    if registry.retry_terminal_persist(task).is_err() {
        return;
    }
    if !registry.cleanup_terminal_task_temp(task).unwrap_or(false) {
        return;
    }
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
#[test]
fn lifecycle_cleanup_spawn_failure_does_not_panic_or_disable_retry() {
    let cleaning = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let result = std::panic::catch_unwind(|| {
        start_cleanup_with(
            &cleaning,
            || panic!("worker must not execute"),
            |_| Err(std::io::Error::other("injected thread admission failure")),
        )
    });
    assert!(result.is_ok(), "cleanup admission killed the scheduler");
    assert!(!cleaning.load(Ordering::SeqCst));
}
