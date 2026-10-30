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
    static MARKER_READS: std::cell::Cell<Option<MarkerReadCounts>> = const { std::cell::Cell::new(None) };
    static MARKER_SUMMARY: std::cell::RefCell<MarkerReadSummary> =
        std::cell::RefCell::new(MarkerReadSummary::default());
}

#[derive(Clone, Copy)]
pub(super) enum MarkerReadReason {
    FirstPoll,
    NonemptyMarker,
    ChangedProbe,
    PoisonedProbe,
    Unprobed,
}

/// Counts attempts to read and validate the marker, including failures before it opens.
/// Terminal candidates are counted separately: they need cleanup, not a read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MarkerReadCounts {
    pub first_poll: u64,
    pub nonempty_marker: u64,
    pub changed_probe: u64,
    pub poisoned_probe: u64,
    pub unprobed: u64,
    pub terminal_retained_skipped: u64,
}

impl MarkerReadCounts {
    fn add(&mut self, other: Self) {
        self.first_poll += other.first_poll;
        self.nonempty_marker += other.nonempty_marker;
        self.changed_probe += other.changed_probe;
        self.poisoned_probe += other.poisoned_probe;
        self.unprobed += other.unprobed;
        self.terminal_retained_skipped += other.terminal_retained_skipped;
    }
}

pub(super) fn record_marker_read(reason: MarkerReadReason) {
    update_marker_counts(|counts| match reason {
        MarkerReadReason::FirstPoll => counts.first_poll += 1,
        MarkerReadReason::NonemptyMarker => counts.nonempty_marker += 1,
        MarkerReadReason::ChangedProbe => counts.changed_probe += 1,
        MarkerReadReason::PoisonedProbe => counts.poisoned_probe += 1,
        MarkerReadReason::Unprobed => counts.unprobed += 1,
    });
}

pub(super) fn record_terminal_marker_skip() {
    update_marker_counts(|counts| counts.terminal_retained_skipped += 1);
}

fn update_marker_counts(update: impl FnOnce(&mut MarkerReadCounts)) {
    MARKER_READS.with(|cell| {
        if let Some(mut counts) = cell.get() {
            update(&mut counts);
            cell.set(Some(counts));
        }
    });
}

#[derive(Default)]
struct MarkerReadSummary {
    since: Option<Instant>,
    passes: u64,
    counts: MarkerReadCounts,
}

impl MarkerReadSummary {
    fn add_pass(
        &mut self,
        counts: MarkerReadCounts,
        now: Instant,
    ) -> Option<(u64, MarkerReadCounts)> {
        self.passes += 1;
        self.counts.add(counts);
        let since = self.since.get_or_insert(now);
        if now.duration_since(*since) < CLEANUP_INTERVAL {
            return None;
        }
        let summary = (self.passes, self.counts);
        self.since = Some(now);
        self.passes = 0;
        self.counts = MarkerReadCounts::default();
        Some(summary)
    }
}

struct MarkerReadPass {
    previous_cause: Option<WatchdogPassCause>,
    previous_counts: Option<MarkerReadCounts>,
}

impl MarkerReadPass {
    fn begin(cause: WatchdogPassCause) -> Self {
        Self {
            previous_cause: CURRENT_PASS.with(|cell| cell.replace(Some(cause))),
            previous_counts: MARKER_READS
                .with(|cell| cell.replace(Some(MarkerReadCounts::default()))),
        }
    }
}

impl Drop for MarkerReadPass {
    fn drop(&mut self) {
        let counts = MARKER_READS
            .with(|cell| cell.replace(self.previous_counts))
            .unwrap_or_default();
        let cause = CURRENT_PASS.with(|cell| cell.replace(self.previous_cause));
        log::debug!("bash watchdog marker reads pass={cause:?} full_marker_reads={counts:?}");
        // All projects share the scheduler thread, so this is one process-wide
        // summary per minute, not one log line per project per minute.
        MARKER_SUMMARY.with(|summary| {
            if let Some((passes, counts)) = summary.borrow_mut().add_pass(counts, Instant::now()) {
                log::info!(
                    "bash watchdog marker reads passes={passes} full_marker_reads={counts:?}"
                );
            }
        });
    }
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
    let _marker_reads = MarkerReadPass::begin(pass_cause);
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

    #[test]
    fn marker_read_counters_attribute_real_probe_branches() {
        use super::super::persistence::{self, ExitMarker, ExitMarkerProbe, PersistedTask};
        let storage = tempfile::tempdir().unwrap();
        let task = persistence::allocate_task_layout(storage.path(), "session").unwrap();
        let metadata = PersistedTask::starting(
            task.paths.task_id.clone(),
            "session".into(),
            "probe".into(),
            storage.path().into(),
            None,
            None,
            true,
            false,
        );
        persistence::write_task_at(&task, &metadata).unwrap();
        std::fs::write(&task.paths.exit, b"").unwrap();
        let mut probe = ExitMarkerProbe::default();
        let pass = MarkerReadPass::begin(WatchdogPassCause::Tick);
        let mut poll = || persistence::poll_exit_marker(&task.paths, &mut probe).unwrap();
        assert_eq!(poll(), None);
        assert_eq!(poll(), None);
        let replacement = task.paths.io_dir.join("replacement");
        std::fs::write(&replacement, b"").unwrap();
        std::fs::rename(replacement, &task.paths.exit).unwrap();
        assert_eq!(poll(), None);
        std::fs::write(&task.paths.exit, b"kil").unwrap();
        assert_eq!(poll(), None);
        assert_eq!(poll(), None);
        std::fs::write(&task.paths.exit, b"killed\n").unwrap();
        assert_eq!(poll(), Some(ExitMarker::Killed));
        std::fs::remove_file(&task.paths.exit).unwrap();
        std::fs::create_dir(&task.paths.exit).unwrap();
        assert!(persistence::read_exit_marker(&task.paths).is_err());
        let counts = MARKER_READS.with(|cell| cell.get().unwrap());
        assert_eq!(counts.first_poll, 1);
        assert_eq!(counts.nonempty_marker, 3);
        // Unix distinguishes a new empty inode. Windows may preserve the old
        // creation time on replacement, so the probe can legitimately skip it.
        #[cfg(unix)]
        assert_eq!(counts.changed_probe, 1);
        assert_eq!(counts.poisoned_probe, 0);
        assert_eq!(
            counts.unprobed, 1,
            "failed direct reads must be attributed too"
        );
        assert_eq!(counts.terminal_retained_skipped, 0);
        drop(pass);
        assert_eq!(current_pass(), None);
        assert_eq!(MARKER_READS.with(std::cell::Cell::get), None);
    }

    #[test]
    fn marker_read_summary_is_shared_across_passes_and_limited_to_one_per_minute() {
        let now = Instant::now();
        let counts = MarkerReadCounts {
            first_poll: 2,
            nonempty_marker: 3,
            ..Default::default()
        };
        let mut summary = MarkerReadSummary::default();
        assert_eq!(summary.add_pass(counts, now), None);
        assert_eq!(
            summary.add_pass(counts, now + Duration::from_secs(59)),
            None
        );
        let (passes, total) = summary
            .add_pass(counts, now + Duration::from_secs(60))
            .unwrap();
        assert_eq!(passes, 3);
        assert_eq!(total.first_poll, 6);
        assert_eq!(total.nonempty_marker, 9);
        assert_eq!(
            summary.add_pass(counts, now + Duration::from_secs(119)),
            None
        );
        assert_eq!(
            summary
                .add_pass(counts, now + Duration::from_secs(120))
                .unwrap()
                .0,
            2
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
