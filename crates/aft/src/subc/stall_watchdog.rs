//! In-process stall watchdog for the subc daemon.
//!
//! A stall is a loop that owes work but has stopped taking turns: the whole
//! daemon goes quiet while the subc daemon waits on route binds that nobody
//! answers. The watchdog runs on its own OS thread and reads only atomics the
//! watched loops publish, so it keeps running when those loops are wedged on a
//! lock or descheduled, and it can capture evidence while the stall is live.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::health::DispatchPathMetrics;
use crate::executor::DispatchLoopLiveness;

/// How long a loop may owe work without progressing before it is a stall.
pub(super) const STALL_THRESHOLD: Duration = Duration::from_secs(15);
/// How often the watchdog samples the markers.
pub(super) const WATCHDOG_TICK: Duration = Duration::from_secs(1);
/// At most one evidence capture per this interval, however many stalls occur.
pub(super) const CAPTURE_COOLDOWN: Duration = Duration::from_secs(10 * 60);
/// Capture files kept in the diagnostics directory; older ones are deleted.
pub(super) const KEEP_CAPTURES: usize = 5;
const CAPTURE_FILE_PREFIX: &str = "stall-";
const CAPTURE_FILE_SUFFIX: &str = ".txt";

/// Stall counters the health report reads. Written only by the watchdog.
#[derive(Default)]
pub(super) struct StallStats {
    stall_count: AtomicU64,
    last_stall_duration_ms: AtomicU64,
    active_stalls: AtomicUsize,
    captures: AtomicU64,
}

impl StallStats {
    pub(super) fn stall_count(&self) -> u64 {
        self.stall_count.load(Ordering::Relaxed)
    }

    /// Duration of the most recent finished stall, or `None` if none ended yet.
    pub(super) fn last_stall_duration_ms(&self) -> Option<u64> {
        match self.last_stall_duration_ms.load(Ordering::Relaxed) {
            0 => None,
            value => Some(value),
        }
    }

    pub(super) fn active_stalls(&self) -> usize {
        self.active_stalls.load(Ordering::Relaxed)
    }

    /// Evidence captures attempted (spawned or written), successful or not.
    pub(super) fn captures(&self) -> u64 {
        self.captures.load(Ordering::Relaxed)
    }
}

/// Where watchdog lines go. Production writes to the daemon log; tests record.
pub(super) type StallLogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// What a capture produced.
pub(super) enum CaptureStarted {
    /// A profiler child is writing the file; the watchdog reaps it later.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Spawned(Child),
    /// The evidence was written synchronously.
    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    Written,
    /// This platform has no capture; only the log line records the stall.
    #[cfg_attr(any(target_os = "macos", target_os = "linux"), allow(dead_code))]
    Unsupported,
}

/// Captures stack evidence of the process while it is stalled. Must not take
/// any lock or need the executor or the async runtime: those are what stall.
pub(super) trait StallCapture: Send + Sync {
    fn capture(&self, pid: u32, path: &Path) -> io::Result<CaptureStarted>;
}

/// macOS: a 3-second `sample` of this process. Linux: every thread's
/// `/proc` stat line and kernel wait channel. Elsewhere: nothing.
pub(super) struct PlatformCapture;

impl StallCapture for PlatformCapture {
    #[cfg(target_os = "macos")]
    fn capture(&self, pid: u32, path: &Path) -> io::Result<CaptureStarted> {
        // `-mayDie` keeps `sample` from failing if the daemon exits mid-sample.
        std::process::Command::new("/usr/bin/sample")
            .arg(pid.to_string())
            .arg("3")
            .arg("-mayDie")
            .arg("-file")
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(CaptureStarted::Spawned)
    }

    #[cfg(target_os = "linux")]
    fn capture(&self, _pid: u32, path: &Path) -> io::Result<CaptureStarted> {
        std::fs::write(path, linux_thread_states()?)?;
        Ok(CaptureStarted::Written)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn capture(&self, _pid: u32, _path: &Path) -> io::Result<CaptureStarted> {
        Ok(CaptureStarted::Unsupported)
    }
}

/// One block per thread: its stat line (state, CPU times) and the kernel
/// function it is waiting in, which names the lock or I/O a blocked thread
/// sits on.
#[cfg(target_os = "linux")]
fn linux_thread_states() -> io::Result<String> {
    let mut tasks = std::fs::read_dir("/proc/self/task")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    tasks.sort();
    let mut out = String::new();
    for task in tasks {
        let read = |name: &str| {
            std::fs::read_to_string(task.join(name))
                .unwrap_or_else(|error| format!("<unreadable: {error}>"))
        };
        out.push_str(&format!(
            "task {}\nstat: {}\nwchan: {}\n\n",
            task.display(),
            read("stat").trim_end(),
            read("wchan").trim_end(),
        ));
    }
    Ok(out)
}

pub(super) struct StallWatchdogConfig {
    pub(super) tick: Duration,
    pub(super) threshold: Duration,
    pub(super) capture_cooldown: Duration,
    pub(super) keep_captures: usize,
    /// `<storage>/diagnostics`.
    pub(super) diagnostics_dir: PathBuf,
    pub(super) pid: u32,
    pub(super) capture: Arc<dyn StallCapture>,
    pub(super) log: StallLogSink,
}

impl StallWatchdogConfig {
    pub(super) fn production(storage_dir: &Path) -> Self {
        Self {
            tick: WATCHDOG_TICK,
            threshold: STALL_THRESHOLD,
            capture_cooldown: CAPTURE_COOLDOWN,
            keep_captures: KEEP_CAPTURES,
            diagnostics_dir: storage_dir.join("diagnostics"),
            pid: std::process::id(),
            capture: Arc::new(PlatformCapture),
            log: Arc::new(|line| log::warn!("{line}")),
        }
    }
}

/// Handle to the watchdog thread. Dropping it stops the thread without
/// waiting: joining could hang teardown if the log sink itself is wedged.
pub(super) struct StallWatchdog {
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl StallWatchdog {
    pub(super) fn spawn(
        markers: Vec<Box<dyn LivenessMarker>>,
        stats: Arc<StallStats>,
        config: StallWatchdogConfig,
    ) -> io::Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("aft-stall-watchdog".to_string())
            .spawn(move || run_watchdog(markers, stats, config, stop_rx))?;
        Ok(Self {
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        })
    }

    #[cfg(test)]
    pub(super) fn stop_and_join(mut self) {
        drop(self.stop_tx.take());
        if let Some(handle) = self.handle.take() {
            handle.join().expect("stall watchdog thread");
        }
    }
}

impl Drop for StallWatchdog {
    fn drop(&mut self) {
        // Disconnecting the stop channel wakes the thread, which then exits.
        drop(self.stop_tx.take());
        drop(self.handle.take());
    }
}

fn run_watchdog(
    markers: Vec<Box<dyn LivenessMarker>>,
    stats: Arc<StallStats>,
    config: StallWatchdogConfig,
    stop_rx: mpsc::Receiver<()>,
) {
    let mut detector = StallDetector::new(config.threshold, markers.len());
    let mut expected_wake = Instant::now() + config.tick;
    let mut last_capture_at: Option<Instant> = None;
    let mut capture_children: Vec<Child> = Vec::new();
    loop {
        match stop_rx.recv_timeout(config.tick) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = Instant::now();
        // Reap finished profiler children so they do not linger as zombies.
        capture_children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        // If the watchdog itself woke far later than it asked to, the whole
        // process (not one loop) was not running; the log lines carry this so
        // a stall can be told apart from process-wide descheduling.
        let wake_late = now.saturating_duration_since(expected_wake);
        expected_wake = now + config.tick;
        for (index, marker) in markers.iter().enumerate() {
            let pending = marker.has_pending_work();
            let progress_age = marker.progress_age();
            match detector.observe(index, pending, progress_age, now) {
                None => {}
                Some(StallTransition::Began { stalled_for, .. }) => {
                    stats.stall_count.fetch_add(1, Ordering::Relaxed);
                    stats.active_stalls.fetch_add(1, Ordering::Relaxed);
                    // Capture before logging: the log sink takes the logger's
                    // lock, and a wedged logger may be the very stall.
                    let in_cooldown = last_capture_at.is_some_and(|at| {
                        now.saturating_duration_since(at) < config.capture_cooldown
                    });
                    let capture = if in_cooldown {
                        "skipped (cooldown)".to_string()
                    } else {
                        last_capture_at = Some(now);
                        stats.captures.fetch_add(1, Ordering::Relaxed);
                        start_capture(&config, &mut capture_children)
                    };
                    (config.log)(&format!(
                        "stall watchdog: stall detected marker={} stalled_for_ms={} watchdog_wake_late_ms={} capture={capture}",
                        marker.name(),
                        stalled_for.as_millis(),
                        wake_late.as_millis(),
                    ));
                }
                Some(StallTransition::Ended { total, .. }) => {
                    let total_ms = u64::try_from(total.as_millis()).unwrap_or(u64::MAX).max(1);
                    stats
                        .last_stall_duration_ms
                        .store(total_ms, Ordering::Relaxed);
                    stats.active_stalls.fetch_sub(1, Ordering::Relaxed);
                    (config.log)(&format!(
                        "stall watchdog: stall ended marker={} total_ms={total_ms} watchdog_wake_late_ms={}",
                        marker.name(),
                        wake_late.as_millis(),
                    ));
                }
            }
        }
    }
}

/// Starts one capture and describes the outcome for the log line.
fn start_capture(config: &StallWatchdogConfig, children: &mut Vec<Child>) -> String {
    let path = config.diagnostics_dir.join(capture_file_name(
        SystemTime::now(),
        config.pid,
    ));
    if let Err(error) = std::fs::create_dir_all(&config.diagnostics_dir) {
        return format!("failed ({}: {error})", config.diagnostics_dir.display());
    }
    // Make room first so the new file is one of the newest `keep_captures`.
    prune_captures(
        &config.diagnostics_dir,
        config.keep_captures.saturating_sub(1),
    );
    match config.capture.capture(config.pid, &path) {
        Ok(CaptureStarted::Spawned(child)) => {
            children.push(child);
            path.display().to_string()
        }
        Ok(CaptureStarted::Written) => path.display().to_string(),
        Ok(CaptureStarted::Unsupported) => "unsupported on this platform".to_string(),
        Err(error) => format!("failed ({}: {error})", path.display()),
    }
}

/// `stall-<UTC yyyymmddThhmmssZ>-<pid>.txt`. The timestamp leads so names sort
/// oldest to newest.
fn capture_file_name(at: SystemTime, pid: u32) -> String {
    let seconds = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = crate::subc_format::civil_from_days(days);
    let of_day = seconds % 86_400;
    format!(
        "{CAPTURE_FILE_PREFIX}{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z-{pid}{CAPTURE_FILE_SUFFIX}",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60,
    )
}

/// Deletes all but the newest `keep` capture files in `dir`.
fn prune_captures(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut captures = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(CAPTURE_FILE_PREFIX) && name.ends_with(CAPTURE_FILE_SUFFIX)
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    captures.sort();
    let excess = captures.len().saturating_sub(keep);
    for path in captures.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// A progress signal of one loop. Implementations must read atomics only:
/// the watchdog calls them while the loop may be holding, or blocked on, any
/// lock it uses.
pub(super) trait LivenessMarker: Send + Sync {
    /// Stable name used in log lines and health output.
    fn name(&self) -> &'static str;
    /// Time since the loop last made progress.
    fn progress_age(&self) -> Duration;
    /// Whether the loop currently owes work. An idle loop owes nothing, so a
    /// long `progress_age` alone never reads as a stall.
    fn has_pending_work(&self) -> bool;
}

/// The subc module (frame) loop. It shares a current-thread runtime with the
/// frame reader and writer tasks, so when this loop stops taking turns the
/// socket stops being read and nothing is answered.
pub(super) struct FrameLoopMarker(pub(super) Arc<DispatchPathMetrics>);

impl LivenessMarker for FrameLoopMarker {
    fn name(&self) -> &'static str {
        "subc_frame_loop"
    }

    fn progress_age(&self) -> Duration {
        self.0.frame_loop_progress_age()
    }

    fn has_pending_work(&self) -> bool {
        self.0.frame_loop_has_pending_work()
    }
}

/// The executor scheduler loop, which hands every tool call and maintenance
/// job to a worker.
pub(super) struct DispatchLoopMarker(pub(super) Arc<DispatchLoopLiveness>);

impl LivenessMarker for DispatchLoopMarker {
    fn name(&self) -> &'static str {
        "executor_dispatch_loop"
    }

    fn progress_age(&self) -> Duration {
        self.0.progress_age()
    }

    fn has_pending_work(&self) -> bool {
        self.0.has_pending_work()
    }
}

/// A change in one marker's stall state, reported once per edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StallTransition {
    /// The marker has owed work without progress for `stalled_for`.
    Began { marker: usize, stalled_for: Duration },
    /// The marker progressed again (or stopped owing work); `total` spans
    /// from its last progress before the stall to its first progress after.
    Ended { marker: usize, total: Duration },
}

#[derive(Default)]
struct MarkerWatch {
    /// When the watchdog first saw the marker owe work in the current
    /// uninterrupted pending run.
    pending_observed_at: Option<Instant>,
    /// Estimated start of the active stall, if one is active.
    stall_started_at: Option<Instant>,
}

/// Pure stall detection over periodic observations. Time is passed in so the
/// state machine is testable without sleeping.
pub(super) struct StallDetector {
    threshold: Duration,
    watches: Vec<MarkerWatch>,
}

impl StallDetector {
    pub(super) fn new(threshold: Duration, marker_count: usize) -> Self {
        Self {
            threshold,
            watches: (0..marker_count).map(|_| MarkerWatch::default()).collect(),
        }
    }

    /// Feeds one observation of marker `index`.
    ///
    /// A marker counts as stalled for the shorter of its progress age and the
    /// time the watchdog has continuously seen it owe work. The second bound
    /// matters when an idle loop is handed work: its progress age is large,
    /// but it has only owed work since just now.
    pub(super) fn observe(
        &mut self,
        index: usize,
        pending: bool,
        progress_age: Duration,
        now: Instant,
    ) -> Option<StallTransition> {
        let watch = &mut self.watches[index];
        if pending {
            watch.pending_observed_at.get_or_insert(now);
        } else {
            watch.pending_observed_at = None;
        }
        let stalled_for = watch
            .pending_observed_at
            .map_or(Duration::ZERO, |observed| {
                progress_age.min(now.saturating_duration_since(observed))
            });

        match watch.stall_started_at {
            None if stalled_for >= self.threshold => {
                watch.stall_started_at = Some(now.checked_sub(stalled_for).unwrap_or(now));
                Some(StallTransition::Began {
                    marker: index,
                    stalled_for,
                })
            }
            Some(started_at) if stalled_for < self.threshold => {
                watch.stall_started_at = None;
                let since_start = now.saturating_duration_since(started_at);
                // Progress resumed `progress_age` ago; if the marker merely
                // stopped owing work without progressing, the stall ends now.
                let ended_at = if progress_age < since_start {
                    now.checked_sub(progress_age).unwrap_or(now)
                } else {
                    now
                };
                Some(StallTransition::Ended {
                    marker: index,
                    total: ended_at.saturating_duration_since(started_at),
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const THRESHOLD: Duration = Duration::from_secs(15);

    /// Thresholds scaled down so the thread tests run in well under a second
    /// per phase while keeping the production ratio of tick to threshold.
    const TEST_TICK: Duration = Duration::from_millis(20);
    const TEST_THRESHOLD: Duration = Duration::from_millis(300);

    #[derive(Clone, Default)]
    struct RecordedLog(Arc<Mutex<Vec<String>>>);

    impl RecordedLog {
        fn sink(&self) -> StallLogSink {
            let lines = Arc::clone(&self.0);
            Arc::new(move |line| lines.lock().unwrap().push(line.to_string()))
        }

        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Records capture requests instead of running a profiler.
    #[derive(Default)]
    struct RecordingCapture(Mutex<Vec<(u32, PathBuf)>>);

    impl StallCapture for RecordingCapture {
        fn capture(&self, pid: u32, path: &Path) -> io::Result<CaptureStarted> {
            self.0.lock().unwrap().push((pid, path.to_path_buf()));
            Ok(CaptureStarted::Written)
        }
    }

    impl RecordingCapture {
        fn calls(&self) -> Vec<(u32, PathBuf)> {
            self.0.lock().unwrap().clone()
        }
    }

    const TEST_PID: u32 = 4242;

    fn test_config(
        log: &RecordedLog,
        capture: &Arc<RecordingCapture>,
        diagnostics_dir: &Path,
    ) -> StallWatchdogConfig {
        StallWatchdogConfig {
            tick: TEST_TICK,
            threshold: TEST_THRESHOLD,
            capture_cooldown: CAPTURE_COOLDOWN,
            keep_captures: KEEP_CAPTURES,
            diagnostics_dir: diagnostics_dir.to_path_buf(),
            pid: TEST_PID,
            capture: Arc::clone(capture) as Arc<dyn StallCapture>,
            log: log.sink(),
        }
    }

    fn daemon_markers(
        metrics: &Arc<DispatchPathMetrics>,
        executor: &crate::executor::Executor,
    ) -> Vec<Box<dyn LivenessMarker>> {
        vec![
            Box::new(FrameLoopMarker(Arc::clone(metrics))),
            Box::new(DispatchLoopMarker(executor.dispatch_loop_liveness())),
        ]
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn held_dispatch_loop_is_captured_once_and_logged_with_its_marker_and_end() {
        let executor = crate::executor::Executor::new();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let log = RecordedLog::default();
        let capture = Arc::new(RecordingCapture::default());
        let storage = tempfile::tempdir().expect("storage dir");
        let diagnostics = storage.path().join("diagnostics");
        let watchdog = StallWatchdog::spawn(
            daemon_markers(&metrics, &executor),
            Arc::clone(&metrics.stall_stats),
            test_config(&log, &capture, &diagnostics),
        )
        .expect("spawn watchdog");

        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let holder = executor.hold_dispatch_loop_for_test(release_rx);
        wait_until("stall detection", || metrics.stall_stats.stall_count() > 0);
        // Stay stalled for several more thresholds: detection must not repeat.
        thread::sleep(TEST_THRESHOLD * 3);
        release_tx.send(()).expect("release hold");
        holder.join().expect("hold thread");
        wait_until("stall end", || metrics.stall_stats.active_stalls() == 0);

        // A second stall inside the cooldown is logged but not captured.
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let holder = executor.hold_dispatch_loop_for_test(release_rx);
        wait_until("second stall", || metrics.stall_stats.stall_count() == 2);
        release_tx.send(()).expect("release second hold");
        holder.join().expect("second hold thread");
        wait_until("second stall end", || {
            metrics.stall_stats.active_stalls() == 0
        });
        watchdog.stop_and_join();

        let calls = capture.calls();
        assert_eq!(calls.len(), 1, "exactly one capture attempted: {calls:?}");
        let (pid, path) = &calls[0];
        assert_eq!(*pid, TEST_PID);
        assert_eq!(path.parent(), Some(diagnostics.as_path()));
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            file_name.starts_with("stall-") && file_name.ends_with(&format!("-{TEST_PID}.txt")),
            "{file_name}"
        );
        assert_eq!(metrics.stall_stats.captures(), 1);

        let lines = log.lines();
        let detected = lines
            .iter()
            .filter(|line| line.contains("stall detected"))
            .collect::<Vec<_>>();
        assert_eq!(detected.len(), 2, "one line per stall: {lines:?}");
        assert!(
            detected[0].contains("marker=executor_dispatch_loop")
                && detected[0].contains("stalled_for_ms=")
                && detected[0].contains(&format!("capture={}", path.display())),
            "{lines:?}"
        );
        assert!(
            detected[1].contains("marker=executor_dispatch_loop")
                && detected[1].contains("capture=skipped (cooldown)"),
            "{lines:?}"
        );
        let ended = lines
            .iter()
            .filter(|line| line.contains("stall ended marker=executor_dispatch_loop total_ms="))
            .count();
        assert_eq!(ended, 2, "{lines:?}");
        assert_eq!(metrics.stall_stats.stall_count(), 2);
        assert!(metrics.stall_stats.last_stall_duration_ms().is_some());
    }

    #[test]
    fn first_stall_duration_covers_the_whole_hold() {
        let executor = crate::executor::Executor::new();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let log = RecordedLog::default();
        let capture = Arc::new(RecordingCapture::default());
        let storage = tempfile::tempdir().expect("storage dir");
        let watchdog = StallWatchdog::spawn(
            daemon_markers(&metrics, &executor),
            Arc::clone(&metrics.stall_stats),
            test_config(&log, &capture, &storage.path().join("diagnostics")),
        )
        .expect("spawn watchdog");

        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let holder = executor.hold_dispatch_loop_for_test(release_rx);
        wait_until("stall detection", || metrics.stall_stats.stall_count() > 0);
        thread::sleep(TEST_THRESHOLD * 3);
        release_tx.send(()).expect("release hold");
        holder.join().expect("hold thread");
        wait_until("stall end", || metrics.stall_stats.active_stalls() == 0);
        watchdog.stop_and_join();

        let last = metrics
            .stall_stats
            .last_stall_duration_ms()
            .expect("finished stall duration");
        assert!(
            last >= u64::try_from((TEST_THRESHOLD * 4).as_millis()).unwrap(),
            "threshold plus three more thresholds of hold: {last}ms"
        );
    }

    #[test]
    fn idle_daemon_past_the_threshold_is_never_a_stall() {
        let executor = crate::executor::Executor::new();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let log = RecordedLog::default();
        let capture = Arc::new(RecordingCapture::default());
        let storage = tempfile::tempdir().expect("storage dir");
        let watchdog = StallWatchdog::spawn(
            daemon_markers(&metrics, &executor),
            Arc::clone(&metrics.stall_stats),
            test_config(&log, &capture, &storage.path().join("diagnostics")),
        )
        .expect("spawn watchdog");

        // Both loops have been idle since creation: the scheduler is parked
        // in recv() and the frame loop has promised no wake. Their progress
        // ages grow far past the threshold while they owe nothing.
        thread::sleep(TEST_THRESHOLD * 4);
        watchdog.stop_and_join();

        assert!(
            executor.dispatch_loop_liveness().progress_age() > TEST_THRESHOLD,
            "the idle loop really went unprogressed past the threshold"
        );
        assert_eq!(log.lines(), Vec::<String>::new());
        assert_eq!(capture.calls(), Vec::new());
        assert_eq!(metrics.stall_stats.stall_count(), 0);
    }

    #[test]
    fn capture_file_names_sort_by_utc_time() {
        // 2026-09-24T12:01:55Z
        let at = UNIX_EPOCH + Duration::from_secs(1_790_251_315);
        assert_eq!(
            capture_file_name(at, 36755),
            "stall-20260924T120155Z-36755.txt"
        );
    }

    #[test]
    fn pruning_keeps_only_the_newest_capture_files() {
        let dir = tempfile::tempdir().expect("diagnostics dir");
        for second in 0..8 {
            std::fs::write(
                dir.path().join(format!("stall-20260924T12000{second}Z-1.txt")),
                "x",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("unrelated.txt"), "keep").unwrap();

        prune_captures(dir.path(), KEEP_CAPTURES - 1);

        let mut left = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(
            left,
            vec![
                "stall-20260924T120004Z-1.txt",
                "stall-20260924T120005Z-1.txt",
                "stall-20260924T120006Z-1.txt",
                "stall-20260924T120007Z-1.txt",
                "unrelated.txt",
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "runs /usr/bin/sample against the test process for about 3 seconds"]
    fn macos_platform_capture_writes_a_sample_file() {
        let dir = tempfile::tempdir().expect("diagnostics dir");
        let path = dir.path().join("stall-test.txt");
        let started = PlatformCapture
            .capture(std::process::id(), &path)
            .expect("spawn sample");
        let CaptureStarted::Spawned(mut child) = started else {
            panic!("macOS capture spawns sample");
        };
        assert!(child.wait().expect("wait sample").success());
        assert!(std::fs::metadata(&path).expect("sample file").len() > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_platform_capture_writes_every_thread_state() {
        let dir = tempfile::tempdir().expect("diagnostics dir");
        let path = dir.path().join("stall-test.txt");
        assert!(matches!(
            PlatformCapture.capture(std::process::id(), &path),
            Ok(CaptureStarted::Written)
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("stat: ") && text.contains("wchan: "), "{text}");
    }

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn idle_marker_with_old_progress_never_stalls() {
        let start = Instant::now();
        let mut detector = StallDetector::new(THRESHOLD, 1);
        for second in 0..120 {
            assert_eq!(
                detector.observe(0, false, secs(second), start + secs(second)),
                None,
                "an idle loop is not a stalled loop"
            );
        }
    }

    #[test]
    fn pending_marker_stalls_once_and_reports_the_end_with_total_duration() {
        let start = Instant::now();
        let mut detector = StallDetector::new(THRESHOLD, 1);
        // Last progress at `start`; the loop owes work from then on.
        let mut began = Vec::new();
        for second in 0..=20 {
            if let Some(transition) = detector.observe(0, true, secs(second), start + secs(second))
            {
                began.push((second, transition));
            }
        }
        assert_eq!(
            began,
            vec![(
                15,
                StallTransition::Began {
                    marker: 0,
                    stalled_for: secs(15)
                }
            )]
        );

        // The loop progresses at start+56s; the watchdog observes it at 57s.
        assert_eq!(
            detector.observe(0, true, secs(1), start + secs(57)),
            Some(StallTransition::Ended {
                marker: 0,
                total: secs(56)
            })
        );
        assert_eq!(detector.observe(0, false, secs(2), start + secs(58)), None);
    }

    #[test]
    fn newly_handed_work_is_measured_from_when_it_was_first_observed() {
        let start = Instant::now();
        let mut detector = StallDetector::new(THRESHOLD, 1);
        // Idle for ten minutes, then work arrives: progress age is huge but the
        // loop has only owed work since the first pending observation.
        assert_eq!(detector.observe(0, true, secs(600), start), None);
        assert_eq!(detector.observe(0, true, secs(614), start + secs(14)), None);
        assert!(matches!(
            detector.observe(0, true, secs(615), start + secs(15)),
            Some(StallTransition::Began { .. })
        ));
    }

    #[test]
    fn markers_are_tracked_independently() {
        let start = Instant::now();
        let mut detector = StallDetector::new(THRESHOLD, 2);
        for second in 0..=15 {
            let now = start + secs(second);
            let frame = detector.observe(0, true, secs(second), now);
            let dispatch = detector.observe(1, false, secs(second), now);
            assert_eq!(dispatch, None);
            if second == 15 {
                assert!(matches!(frame, Some(StallTransition::Began { marker: 0, .. })));
            }
        }
    }
}
