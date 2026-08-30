//! Durable process logging and low-cost periodic performance summaries.
//!
//! Rust module processes use one file per PID. That avoids cross-process rename
//! races while preserving a single greppable directory for all AFT activity.

use crate::bash_background::process::is_process_alive;
use crate::executor::Executor;
use crate::run_tool_call::ToolCallPhaseDurations;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Maximum size of an active Rust or plugin log before its single backup rotates in.
const LOG_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Keep one backup generation; retention is hygiene rather than a user setting.
const LOG_GENERATIONS: usize = 1;
/// Check the active file on every write so the cap is not exceeded by a burst.
const ROTATION_CHECK_EVERY: u64 = 1;
const LOG_CHANNEL_CAPACITY: usize = 4096;
/// Do not reap a dead PID's file until it has been quiet for at least one day.
const DEAD_PROCESS_LOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Limit the total regular-file footprint left in the log directory.
const LOG_DIRECTORY_BUDGET_BYTES: u64 = 200 * 1024 * 1024;
/// Maintenance ticks may call the sweep, but actual directory work is hourly.
const LOG_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const DEFAULT_PERF_TICK_INTERVAL: Duration = Duration::from_secs(60);
const PERF_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const SLOW_TOOL_CALL_THRESHOLD: Duration = Duration::from_millis(50);
const TOOL_CALL_SAMPLE_CAPACITY: usize = 256;

/// Initialize the `RUST_LOG`-filtered stderr logger and its additive file sink.
pub fn init() {
    let storage_root = crate::bash_background::storage_dir(None);
    let logs_dir = storage_root.join("logs");
    let file_name = format!("aft-{}.log", std::process::id());
    let file_path = logs_dir.join(file_name);
    let mut startup_sweep = None;

    let file_tx = match prepare_file_sink(&logs_dir, &file_path) {
        Ok((sink, summary)) => {
            startup_sweep = Some(summary);
            let (tx, rx) = mpsc::sync_channel(LOG_CHANNEL_CAPACITY);
            thread::Builder::new()
                .name("aft-log-writer".to_string())
                .spawn(move || run_file_writer(sink, rx))
                .map(|_| {
                    if let Ok(mut control) = FILE_CONTROL.lock() {
                        control.tx = Some(tx.clone());
                        control.storage_root = Some(storage_root.clone());
                    }
                    Some(tx)
                })
                .unwrap_or_else(|error| {
                    write_stderr_once(&format!(
                        "[aft] durable log disabled: cannot start writer thread: {error}\n"
                    ));
                    None
                })
        }
        Err(error) => {
            write_stderr_once(&format!(
                "[aft] durable log disabled for {}: {error}\n",
                file_path.display()
            ));
            None
        }
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(TeeWriter { file_tx })))
        .format(|buf, record| {
            let prefix = if record.target().starts_with("aft::lsp")
                || record.target().starts_with("aft_lsp")
            {
                "[aft-lsp]"
            } else {
                "[aft]"
            };
            // Wall-clock stamp so post-hoc log forensics can correlate
            // lines with external events (health probes, module bounces).
            // Seconds precision is enough; chrono is avoided on purpose —
            // this hand-rolls UTC from the epoch to keep deps flat.
            writeln!(
                buf,
                "{} {} {}",
                format_utc_timestamp(),
                prefix,
                record.args()
            )
        })
        .init();

    if let Some(summary) = startup_sweep {
        log_sweep_summary(summary);
    }
}

/// Render `now` as `YYYY-MM-DDTHH:MM:SSZ` without a date-time dependency.
///
/// Civil-date math uses the days-from-epoch algorithm (Howard Hinnant's
/// `civil_from_days`); u64 seconds keep it valid far past 2100.
fn format_utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_epoch_secs(secs)
}

fn format_epoch_secs(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days: adding 719,468 shifts Unix epoch day 0
    // into the algorithm's era, which begins on 0000-03-01 (putting the leap
    // day last in each year simplifies the month/day arithmetic below).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn prepare_file_sink(
    logs_dir: &Path,
    file_path: &Path,
) -> io::Result<(RotatingFile, SweepSummary)> {
    fs::create_dir_all(logs_dir)?;
    let summary = sweep_logs(
        logs_dir,
        SystemTime::now(),
        DEAD_PROCESS_LOG_MAX_AGE,
        LOG_DIRECTORY_BUDGET_BYTES,
    )?;
    mark_log_sweep_ran();
    let sink = RotatingFile::open(
        file_path.to_path_buf(),
        LOG_FILE_BYTES,
        LOG_GENERATIONS,
        ROTATION_CHECK_EVERY,
    )?;
    Ok((sink, summary))
}

enum LogMessage {
    Write(Vec<u8>),
    Reconfigure(PathBuf),
}

#[derive(Default)]
struct FileControl {
    tx: Option<SyncSender<LogMessage>>,
    storage_root: Option<PathBuf>,
}

static FILE_CONTROL: LazyLock<Mutex<FileControl>> =
    LazyLock::new(|| Mutex::new(FileControl::default()));

struct TeeWriter {
    file_tx: Option<SyncSender<LogMessage>>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write_all(buf)?;
        if let Some(tx) = self.file_tx.as_ref() {
            match tx.try_send(LogMessage::Write(buf.to_vec())) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    PERF.file_lines_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => self.file_tx = None,
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

fn run_file_writer(mut sink: RotatingFile, rx: mpsc::Receiver<LogMessage>) {
    while let Ok(message) = rx.recv() {
        let mut lines = Vec::new();
        let mut reconfigure = None;
        match message {
            LogMessage::Write(line) => {
                lines.push(line);
                while lines.len() < 256 {
                    match rx.try_recv() {
                        Ok(LogMessage::Write(line)) => lines.push(line),
                        Ok(LogMessage::Reconfigure(storage_root)) => {
                            reconfigure = Some(storage_root);
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            LogMessage::Reconfigure(storage_root) => reconfigure = Some(storage_root),
        }
        if !lines.is_empty() {
            if let Err(error) = sink.write_batch(&lines) {
                write_stderr_once(&format!(
                    "[aft] durable log disabled after write failure for {}: {error}\n",
                    sink.path.display()
                ));
                break;
            }
        }
        if let Some(storage_root) = reconfigure {
            let logs_dir = storage_root.join("logs");
            let path = logs_dir.join(format!("aft-{}.log", std::process::id()));
            match prepare_file_sink(&logs_dir, &path) {
                Ok((new_sink, summary)) => {
                    sink = new_sink;
                    log_sweep_summary(summary);
                }
                Err(error) => write_stderr_once(&format!(
                    "[aft] durable log could not switch to {}: {error}\n",
                    path.display()
                )),
            }
        }
    }
}

fn write_stderr_once(message: &str) {
    let _ = io::stderr().write_all(message.as_bytes());
}

struct RotatingFile {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    size: u64,
    threshold: u64,
    generations: usize,
    check_every: u64,
    writes_since_check: u64,
}

impl RotatingFile {
    fn open(
        path: PathBuf,
        threshold: u64,
        generations: usize,
        check_every: u64,
    ) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        let mut sink = Self {
            path,
            writer: Some(BufWriter::new(file)),
            size,
            threshold,
            generations,
            check_every: check_every.max(1),
            writes_since_check: 0,
        };
        if size > threshold {
            sink.rotate()?;
        }
        Ok(sink)
    }

    fn write_batch(&mut self, lines: &[Vec<u8>]) -> io::Result<()> {
        let batch_bytes = lines.iter().map(Vec::len).sum::<usize>() as u64;
        self.writes_since_check = self.writes_since_check.saturating_add(lines.len() as u64);
        if self.writes_since_check >= self.check_every
            && self.size > 0
            && self.size.saturating_add(batch_bytes) > self.threshold
        {
            self.rotate()?;
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("log writer unavailable"))?;
        for line in lines {
            writer.write_all(line)?;
        }
        // The worker batches channel messages before this flush. File I/O never
        // runs on request, watcher, executor, or transport threads.
        writer.flush()?;
        self.size = self.size.saturating_add(batch_bytes);
        if self.writes_since_check >= self.check_every {
            self.writes_since_check = 0;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        if self.generations > 0 {
            let oldest = rotated_path(&self.path, self.generations);
            remove_file_if_present(&oldest)?;
            for generation in (1..self.generations).rev() {
                let from = rotated_path(&self.path, generation);
                let to = rotated_path(&self.path, generation + 1);
                rename_if_present(&from, &to)?;
            }
            rename_if_present(&self.path, &rotated_path(&self.path, 1))?;
        } else {
            remove_file_if_present(&self.path)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.writer = Some(BufWriter::new(file));
        self.size = 0;
        self.writes_since_check = 0;
        Ok(())
    }
}

fn rotated_path(base: &Path, generation: usize) -> PathBuf {
    let mut path = base.as_os_str().to_os_string();
    path.push(format!(".{generation}"));
    PathBuf::from(path)
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_if_present(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SweepSummary {
    removed_files: usize,
    bytes_freed: u64,
}

struct ProcessLogFile {
    path: PathBuf,
    modified: Option<SystemTime>,
    bytes: u64,
    dead: bool,
    old_enough: bool,
    removed: bool,
}

fn log_sweep_summary(summary: SweepSummary) {
    crate::slog_info!(
        "log retention sweep: removed_files={} bytes_freed={}",
        summary.removed_files,
        summary.bytes_freed
    );
}

/// Sweep dead Rust process logs, then enforce the directory budget without ever
/// deleting a live PID's file or the plugin logger's pid-less file.
fn sweep_logs(
    dir: &Path,
    now: SystemTime,
    max_age: Duration,
    budget_bytes: u64,
) -> io::Result<SweepSummary> {
    let mut total_bytes = 0_u64;
    let mut process_logs = Vec::new();
    let mut live_pids = BTreeMap::new();
    let own_pid = std::process::id();

    for entry in fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) | Err(_) => continue,
        };
        let bytes = metadata.len();
        total_bytes = total_bytes.saturating_add(bytes);
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let pid = match name.as_ref() {
            // This shared TypeScript-owned file has no PID. Keep this explicit
            // so a future default branch cannot accidentally make it reaped.
            "aft-plugin.log" => continue,
            _ => process_log_pid(&name),
        };
        let Some(pid) = pid else {
            continue;
        };
        let modified = metadata.modified().ok();
        let old_enough = modified
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        let alive = *live_pids
            .entry(pid)
            .or_insert_with(|| is_process_alive(pid));
        process_logs.push(ProcessLogFile {
            path: entry.path(),
            modified,
            bytes,
            dead: pid != own_pid && !alive,
            old_enough,
            removed: false,
        });
    }

    let mut summary = SweepSummary::default();
    for file in &mut process_logs {
        if file.dead && file.old_enough && remove_sweep_candidate(&file.path) {
            file.removed = true;
            total_bytes = total_bytes.saturating_sub(file.bytes);
            summary.removed_files += 1;
            summary.bytes_freed = summary.bytes_freed.saturating_add(file.bytes);
        }
    }

    // The budget backstop is deliberately separate from the age-gated reap:
    // once liveness says a PID is dead, budget pressure may remove even a fresh
    // dead file so the directory can actually converge under its hard limit.
    // Live files remain ineligible regardless of age or budget pressure.
    process_logs.sort_by_key(|file| file.modified);
    for file in process_logs
        .iter_mut()
        .filter(|file| file.dead && !file.removed)
    {
        if total_bytes <= budget_bytes {
            break;
        }
        if remove_sweep_candidate(&file.path) {
            file.removed = true;
            total_bytes = total_bytes.saturating_sub(file.bytes);
            summary.removed_files += 1;
            summary.bytes_freed = summary.bytes_freed.saturating_add(file.bytes);
        }
    }

    Ok(summary)
}

fn remove_sweep_candidate(path: &Path) -> bool {
    // A sharing violation means another process has the file pinned (notably on
    // Windows). Leave it for a later sweep instead of failing the maintenance pass.
    fs::remove_file(path).is_ok()
}

fn process_log_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("aft-")?;
    let (pid, suffix) = rest.split_once(".log")?;
    if !suffix.is_empty()
        && !(suffix.starts_with('.') && suffix[1..].chars().all(|ch| ch.is_ascii_digit()))
    {
        return None;
    }
    pid.parse().ok()
}

static LAST_LOG_SWEEP: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

fn mark_log_sweep_ran() {
    if let Ok(mut last_run) = LAST_LOG_SWEEP.lock() {
        *last_run = Some(Instant::now());
    }
}

/// Run log maintenance from an existing idle/maintenance tick at most hourly.
pub fn maybe_sweep_logs() {
    let now = Instant::now();
    let should_run = LAST_LOG_SWEEP
        .lock()
        .map(|mut last_run| {
            if last_run.is_some_and(|last| now.duration_since(last) < LOG_SWEEP_INTERVAL) {
                false
            } else {
                *last_run = Some(now);
                true
            }
        })
        .unwrap_or(false);
    if !should_run {
        return;
    }

    let storage_root = FILE_CONTROL
        .lock()
        .ok()
        .and_then(|control| control.storage_root.clone())
        .unwrap_or_else(|| crate::bash_background::storage_dir(None));
    let logs_dir = storage_root.join("logs");
    match sweep_logs(
        &logs_dir,
        SystemTime::now(),
        DEAD_PROCESS_LOG_MAX_AGE,
        LOG_DIRECTORY_BUDGET_BYTES,
    ) {
        Ok(summary) => log_sweep_summary(summary),
        Err(error) => crate::slog_warn!(
            "log retention sweep failed for {}: {}",
            logs_dir.display(),
            error
        ),
    }
}

#[derive(Default)]
struct PerfMetrics {
    watcher_ingested: AtomicU64,
    watcher_paths: AtomicU64,
    watcher_dropped: AtomicU64,
    drain_slices: AtomicU64,
    semantic_collects: AtomicU64,
    semantic_files: AtomicU64,
    semantic_chunks: AtomicU64,
    semantic_ms: AtomicU64,
    callgraph_invalidations: AtomicU64,
    file_lines_dropped: AtomicU64,
    tool_call_count: AtomicU64,
    tool_calls: Mutex<VecDeque<ToolCallPerfSample>>,
    tier2: Mutex<BTreeMap<String, (u64, u64)>>,
    next_sample_ns: AtomicU64,
    reporter: Mutex<PerfReporter>,
}

struct PerfReporter {
    last_report: Instant,
    last_completed_interactive: u64,
    last_completed_maintenance: u64,
    last_tool_call_count: u64,
}

impl Default for PerfReporter {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            last_completed_interactive: 0,
            last_completed_maintenance: 0,
            last_tool_call_count: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct ToolCallPerfSample {
    total_ms: u64,
    queue_ms: u64,
}

#[derive(Clone, Copy, Default)]
struct ToolCallPerfSummary {
    window: usize,
    p50_total_ms: u64,
    max_total_ms: u64,
    p50_queue_ms: u64,
    max_queue_ms: u64,
}

#[derive(Clone, Copy, Default)]
struct ExecutorSample {
    interactive_running: usize,
    maintenance_running: usize,
    interactive_queued: usize,
    maintenance_queued: usize,
    interactive_oldest_ms: Option<u64>,
    maintenance_oldest_ms: Option<u64>,
    interactive_queue_cap: usize,
    interactive_actor_queue_cap: usize,
    maintenance_queue_cap: usize,
    interactive_admission_rejections: u64,
    maintenance_admission_rejections: u64,
    deadline_expiries: u64,
}

static PERF: LazyLock<PerfMetrics> = LazyLock::new(PerfMetrics::default);

/// Move subsequent file log writes to a newly configured storage root.
///
/// Reconfiguration is queued behind existing writes and is a no-op when the
/// root has not changed. Initialization and explicit configure changes call
/// this directly, avoiding storage-root polling on transport drain turns.
pub fn sync_storage_root(storage_root: PathBuf) {
    let Ok(mut control) = FILE_CONTROL.lock() else {
        return;
    };
    if control.storage_root.as_ref() == Some(&storage_root) {
        return;
    }
    let Some(tx) = control.tx.as_ref() else {
        return;
    };
    if tx
        .try_send(LogMessage::Reconfigure(storage_root.clone()))
        .is_ok()
    {
        control.storage_root = Some(storage_root);
    }
}

/// Called by `drain_watcher_events_bounded` for dispatch events actually received.
pub fn note_watcher_events(count: usize) {
    PERF.watcher_ingested
        .fetch_add(count as u64, Ordering::Relaxed);
}

/// Called when a watcher drain slice takes paths from dispatch continuation state.
pub fn note_drain_paths(count: usize) {
    PERF.watcher_paths
        .fetch_add(count as u64, Ordering::Relaxed);
}

/// Called when `drain_watcher_events_bounded` receives a rescan-required overflow signal.
pub fn note_watcher_overflow() {
    PERF.watcher_dropped.fetch_add(1, Ordering::Relaxed);
}

/// Called by the standalone request loop before a request-triggered runtime drain.
pub fn note_drain_slice() {
    PERF.drain_slices.fetch_add(1, Ordering::Relaxed);
}

/// Called after `SemanticIndex::collect_chunks` has collected one real file batch.
pub fn note_semantic_collect(chunks: usize, files: usize, elapsed_ms: u64) {
    PERF.semantic_collects.fetch_add(1, Ordering::Relaxed);
    PERF.semantic_chunks
        .fetch_add(chunks as u64, Ordering::Relaxed);
    PERF.semantic_files
        .fetch_add(files as u64, Ordering::Relaxed);
    PERF.semantic_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
}

/// Called by `Tier2PhaseTimings::log` after a Tier-2 scan performs measurable work.
pub fn note_tier2_scan(category: String, elapsed_ms: u64) {
    if let Ok(mut tier2) = PERF.tier2.lock() {
        let entry = tier2.entry(category).or_default();
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(elapsed_ms);
    }
}

/// Called after watcher-driven callgraph `refresh_files` succeeds for concrete paths.
pub fn note_callgraph_invalidations(files: usize) {
    PERF.callgraph_invalidations
        .fetch_add(files as u64, Ordering::Relaxed);
}

/// Record a completed subc tool call for slow-call diagnostics and the standing
/// perf-tick window. The writer calls this only after `write_all` has handed the
/// complete response frame to the transport.
pub fn note_tool_call_trace(
    name: &str,
    root: &Path,
    channel: u16,
    corr: u64,
    phases: ToolCallPhaseDurations,
) {
    let sample = ToolCallPerfSample {
        total_ms: duration_millis_u64(phases.total),
        queue_ms: duration_millis_u64(phases.queue),
    };
    if let Ok(mut samples) = PERF.tool_calls.lock() {
        if samples.len() == TOOL_CALL_SAMPLE_CAPACITY {
            samples.pop_front();
        }
        samples.push_back(sample);
        PERF.tool_call_count.fetch_add(1, Ordering::Relaxed);
    }

    crate::slog_debug!(
        "tool_call phase name={} channel={} corr={} total_ms={:.3} queue_ms={:.3} translate_ms={:.3} exec_ms={:.3} format_ms={:.3} finalize_ms={:.3} egress_ms={:.3} egress_enqueue_ms={:.3} egress_queue_ms={:.3} egress_prepare_ms={:.3} egress_write_ms={:.3} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} root={}",
        name,
        channel,
        corr,
        duration_millis_f64(phases.total),
        duration_millis_f64(phases.queue),
        duration_millis_f64(phases.translate),
        duration_millis_f64(phases.execute),
        duration_millis_f64(phases.format),
        duration_millis_f64(phases.finalize),
        duration_millis_f64(phases.egress),
        duration_millis_f64(phases.egress_enqueue),
        duration_millis_f64(phases.egress_queue),
        duration_millis_f64(phases.egress_prepare),
        duration_millis_f64(phases.egress_write),
        phases.frame_bytes,
        phases.writer_queue_depth,
        phases.writer_active_at_enqueue,
        phases.writer_queue_was_full,
        phases.writer_reserve_timeouts,
        root.display(),
    );

    if phases.total > SLOW_TOOL_CALL_THRESHOLD {
        crate::slog_warn!(
            "slow tool_call name={} channel={} corr={} total={}ms queue={} translate={} exec={} format={} finalize={} egress={} egress_enqueue={} egress_queue={} egress_prepare={} egress_write={} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} root={}",
            name,
            channel,
            corr,
            duration_millis_u64(phases.total),
            duration_millis_u64(phases.queue),
            duration_millis_u64(phases.translate),
            duration_millis_u64(phases.execute),
            duration_millis_u64(phases.format),
            duration_millis_u64(phases.finalize),
            duration_millis_u64(phases.egress),
            duration_millis_u64(phases.egress_enqueue),
            duration_millis_u64(phases.egress_queue),
            duration_millis_u64(phases.egress_prepare),
            duration_millis_u64(phases.egress_write),
            phases.frame_bytes,
            phases.writer_queue_depth,
            phases.writer_active_at_enqueue,
            phases.writer_queue_was_full,
            phases.writer_reserve_timeouts,
            root.display(),
        );
    }
}

/// Sample executor liveness and emit one busy-only aggregate at the configured cadence.
///
/// The transport may call this every loop turn; an atomic deadline keeps all
/// executor sampling and reporter locking off that path between drain ticks.
pub fn perf_tick(executor: Option<&Executor>) {
    if !perf_sample_due() {
        return;
    }

    let sample = executor.and_then(|executor| {
        executor
            .try_dispatch_liveness_snapshot()
            .map(|snapshot| ExecutorSample {
                interactive_running: snapshot.running.interactive,
                maintenance_running: snapshot.running.maintenance,
                interactive_queued: snapshot.interactive.queued,
                maintenance_queued: snapshot.maintenance.queued,
                interactive_oldest_ms: snapshot.interactive.oldest_age_ms,
                maintenance_oldest_ms: snapshot.maintenance.oldest_age_ms,
                interactive_queue_cap: snapshot.interactive_queue_cap,
                interactive_actor_queue_cap: snapshot.interactive_actor_queue_cap,
                maintenance_queue_cap: snapshot.maintenance_queue_cap,
                interactive_admission_rejections: snapshot.interactive_admission_rejections,
                maintenance_admission_rejections: snapshot.maintenance_admission_rejections,
                deadline_expiries: snapshot.deadline_expiries,
            })
    });

    let completion_counts = executor.map_or((0, 0), Executor::completion_counts);
    let tool_call_count = PERF.tool_call_count.load(Ordering::Relaxed);
    let (completed_interactive, completed_maintenance, new_tool_calls) = {
        let Ok(mut reporter) = PERF.reporter.lock() else {
            return;
        };
        if reporter.last_report.elapsed() < perf_tick_interval() {
            return;
        }
        reporter.last_report = Instant::now();
        let completed = (
            completion_counts
                .0
                .saturating_sub(reporter.last_completed_interactive),
            completion_counts
                .1
                .saturating_sub(reporter.last_completed_maintenance),
            tool_call_count.saturating_sub(reporter.last_tool_call_count),
        );
        reporter.last_completed_interactive = completion_counts.0;
        reporter.last_completed_maintenance = completion_counts.1;
        reporter.last_tool_call_count = tool_call_count;
        completed
    };

    let watcher_ingested = PERF.watcher_ingested.swap(0, Ordering::Relaxed);
    let watcher_paths = PERF.watcher_paths.swap(0, Ordering::Relaxed);
    let watcher_dropped = PERF.watcher_dropped.swap(0, Ordering::Relaxed);
    let drain_slices = PERF.drain_slices.swap(0, Ordering::Relaxed);
    let semantic_collects = PERF.semantic_collects.swap(0, Ordering::Relaxed);
    let semantic_files = PERF.semantic_files.swap(0, Ordering::Relaxed);
    let semantic_chunks = PERF.semantic_chunks.swap(0, Ordering::Relaxed);
    let semantic_ms = PERF.semantic_ms.swap(0, Ordering::Relaxed);
    let callgraph_invalidations = PERF.callgraph_invalidations.swap(0, Ordering::Relaxed);
    let file_lines_dropped = PERF.file_lines_dropped.swap(0, Ordering::Relaxed);
    let tier2 = PERF
        .tier2
        .lock()
        .map(|mut tier2| std::mem::take(&mut *tier2))
        .unwrap_or_default();
    let tool_calls = PERF
        .tool_calls
        .lock()
        .map(|samples| summarize_tool_calls(&samples))
        .unwrap_or_default();

    let executor_busy = sample.is_some_and(|sample| {
        sample.interactive_running > 0
            || sample.maintenance_running > 0
            || sample.interactive_queued > 0
            || sample.maintenance_queued > 0
    });
    let active = watcher_ingested > 0
        || watcher_paths > 0
        || watcher_dropped > 0
        || drain_slices > 0
        || semantic_collects > 0
        || callgraph_invalidations > 0
        || completed_interactive > 0
        || completed_maintenance > 0
        || new_tool_calls > 0
        || file_lines_dropped > 0
        || !tier2.is_empty()
        || executor_busy;
    if !active {
        return;
    }

    let tier2_summary = if tier2.is_empty() {
        "none".to_string()
    } else {
        tier2
            .into_iter()
            .map(|(category, (count, ms))| format!("{category}:{count}/{ms}ms"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let sample = sample.unwrap_or_default();
    crate::slog_info!(
        "perf tick: watcher={{ingested:{},paths:{},dropped:{}}} drains={} tier2=[{}] semantic={{collects:{},files:{},chunks:{},ms:{}}} callgraph_invalidations={} executor_completed={{interactive:{},maintenance:{}}} oldest_queued_ms={{interactive:{},maintenance:{}}} queue_bounds={{interactive:{},interactive_actor:{},maintenance:{}}} admission_rejections={{interactive:{},maintenance:{}}} deadline_expiries={} {} file_log_dropped={}",
        watcher_ingested,
        watcher_paths,
        watcher_dropped,
        drain_slices,
        tier2_summary,
        semantic_collects,
        semantic_files,
        semantic_chunks,
        semantic_ms,
        callgraph_invalidations,
        completed_interactive,
        completed_maintenance,
        format_optional_ms(sample.interactive_oldest_ms),
        format_optional_ms(sample.maintenance_oldest_ms),
        sample.interactive_queue_cap,
        sample.interactive_actor_queue_cap,
        sample.maintenance_queue_cap,
        sample.interactive_admission_rejections,
        sample.maintenance_admission_rejections,
        sample.deadline_expiries,
        format_tool_call_summary(new_tool_calls, tool_calls),
        file_lines_dropped,
    );
}

fn duration_millis_f64(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn duration_millis_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn summarize_tool_calls(samples: &VecDeque<ToolCallPerfSample>) -> ToolCallPerfSummary {
    if samples.is_empty() {
        return ToolCallPerfSummary::default();
    }
    let mut totals = samples
        .iter()
        .map(|sample| sample.total_ms)
        .collect::<Vec<_>>();
    let mut queues = samples
        .iter()
        .map(|sample| sample.queue_ms)
        .collect::<Vec<_>>();
    totals.sort_unstable();
    queues.sort_unstable();
    let median_index = (samples.len() - 1) / 2;
    ToolCallPerfSummary {
        window: samples.len(),
        p50_total_ms: totals[median_index],
        max_total_ms: totals[totals.len() - 1],
        p50_queue_ms: queues[median_index],
        max_queue_ms: queues[queues.len() - 1],
    }
}

fn format_tool_call_summary(new_tool_calls: u64, summary: ToolCallPerfSummary) -> String {
    format!(
        "toolcall={{new:{new_tool_calls},window:{},p50_total_ms:{},max_total_ms:{},p50_queue_ms:{},max_queue_ms:{}}}",
        summary.window,
        summary.p50_total_ms,
        summary.max_total_ms,
        summary.p50_queue_ms,
        summary.max_queue_ms,
    )
}

fn format_optional_ms(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

fn perf_sample_due() -> bool {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    let now_ns = ORIGIN.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let mut deadline = PERF.next_sample_ns.load(Ordering::Relaxed);
    loop {
        if now_ns < deadline {
            return false;
        }
        let next = now_ns.saturating_add(PERF_SAMPLE_INTERVAL.as_nanos() as u64);
        match PERF.next_sample_ns.compare_exchange_weak(
            deadline,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(observed) => deadline = observed,
        }
    }
}

fn perf_tick_interval() -> Duration {
    static INTERVAL: OnceLock<Duration> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("AFT_PERF_TICK_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PERF_TICK_INTERVAL)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use tempfile::TempDir;

    fn line(value: &str) -> Vec<Vec<u8>> {
        vec![format!("{value}\n").into_bytes()]
    }

    #[test]
    fn epoch_timestamp_renders_known_dates() {
        // Epoch start, a modern date, a post-2038 date (u64 range), and the
        // 2100 non-leap century boundary that naive leap logic gets wrong.
        assert_eq!(format_epoch_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(1_704_067_200), "2024-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(format_epoch_secs(4_102_444_800), "2100-01-01T00:00:00Z");
        assert_eq!(format_epoch_secs(4_107_542_399), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn rotation_rolls_once_and_replaces_the_single_backup_generation() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("aft-123.log");
        fs::write(rotated_path(&path, 1), "stale backup\n").unwrap();
        let mut sink = RotatingFile::open(path.clone(), 10, 1, 1).unwrap();
        sink.write_batch(&line("aaaa")).unwrap();
        sink.write_batch(&line("bbbb")).unwrap();
        sink.write_batch(&line("cccc")).unwrap();
        sink.write_batch(&line("dddd")).unwrap();
        sink.write_batch(&line("eeee")).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "eeee\n");
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 1)).unwrap(),
            "cccc\ndddd\n"
        );
        assert!(!rotated_path(&path, 2).exists());
    }

    #[test]
    fn dead_pid_sweep_respects_age_liveness_and_explicit_plugin_exclusion() {
        let temp = TempDir::new().unwrap();
        let dead = temp.path().join("aft-4294967294.log");
        let dead_rotated = temp.path().join("aft-4294967294.log.1");
        let fresh_dead = temp.path().join("aft-4294967293.log");
        let own = temp.path().join(format!("aft-{}.log", std::process::id()));
        let live_rotated = rotated_path(&own, 1);
        let plugin = temp.path().join("aft-plugin.log");
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        for path in [&dead, &dead_rotated, &own, &live_rotated, &plugin] {
            fs::write(path, "log").unwrap();
            set_file_mtime(path, FileTime::from_unix_time(1, 0)).unwrap();
        }
        fs::write(&fresh_dead, "fresh").unwrap();
        set_file_mtime(
            &fresh_dead,
            FileTime::from_unix_time(
                (now - DEAD_PROCESS_LOG_MAX_AGE + Duration::from_secs(1))
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
                0,
            ),
        )
        .unwrap();

        let summary = sweep_logs(temp.path(), now, DEAD_PROCESS_LOG_MAX_AGE, u64::MAX).unwrap();

        assert_eq!(summary.removed_files, 2);
        assert!(!dead.exists());
        assert!(!dead_rotated.exists());
        assert!(fresh_dead.exists());
        assert!(own.exists());
        assert!(live_rotated.exists());
        assert!(plugin.exists());
    }

    #[test]
    fn budget_backstop_deletes_oldest_dead_files_but_not_live_files() {
        let temp = TempDir::new().unwrap();
        let oldest = temp.path().join("aft-4294967294.log");
        let newest = temp.path().join("aft-4294967293.log");
        let live = temp.path().join(format!("aft-{}.log", std::process::id()));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        fs::write(&oldest, "oldest").unwrap();
        fs::write(&newest, "newest").unwrap();
        fs::write(&live, "live-live").unwrap();
        set_file_mtime(&oldest, FileTime::from_unix_time(1, 0)).unwrap();
        set_file_mtime(&newest, FileTime::from_unix_time(2, 0)).unwrap();
        set_file_mtime(&live, FileTime::from_unix_time(1, 0)).unwrap();

        let summary = sweep_logs(
            temp.path(),
            now,
            Duration::from_secs(365 * 24 * 60 * 60),
            15,
        )
        .unwrap();

        assert_eq!(summary.removed_files, 1);
        assert!(!oldest.exists());
        assert!(newest.exists());
        assert!(live.exists());
    }

    #[test]
    fn tool_call_summary_uses_bounded_window_median_and_maxima() {
        let samples = VecDeque::from([
            ToolCallPerfSample {
                total_ms: 9,
                queue_ms: 5,
            },
            ToolCallPerfSample {
                total_ms: 3,
                queue_ms: 1,
            },
            ToolCallPerfSample {
                total_ms: 7,
                queue_ms: 2,
            },
            ToolCallPerfSample {
                total_ms: 5,
                queue_ms: 4,
            },
        ]);

        let summary = summarize_tool_calls(&samples);

        assert_eq!(summary.window, 4);
        assert_eq!(summary.p50_total_ms, 5);
        assert_eq!(summary.max_total_ms, 9);
        assert_eq!(summary.p50_queue_ms, 2);
        assert_eq!(summary.max_queue_ms, 5);
    }

    #[test]
    fn tool_call_tick_labels_interval_count_and_rolling_window() {
        let samples = VecDeque::from([ToolCallPerfSample {
            total_ms: 3_000,
            queue_ms: 2_900,
        }]);
        let summary = summarize_tool_calls(&samples);

        assert_eq!(
            format_tool_call_summary(1, summary),
            "toolcall={new:1,window:1,p50_total_ms:3000,max_total_ms:3000,p50_queue_ms:2900,max_queue_ms:2900}"
        );
        assert_eq!(
            format_tool_call_summary(0, summary),
            "toolcall={new:0,window:1,p50_total_ms:3000,max_total_ms:3000,p50_queue_ms:2900,max_queue_ms:2900}"
        );
    }
}
