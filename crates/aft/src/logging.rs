//! Durable process logging and low-cost periodic performance summaries.
//!
//! Rust module processes use one file per PID. That avoids cross-process rename
//! races while preserving a single greppable directory for all AFT activity.

use crate::bash_background::process::is_process_alive;
use crate::executor::Executor;
use crate::run_tool_call::{ToolCallPhaseDurations, WaitingOn};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
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
/// Census lines stay greppable and short; extra fields are dropped past this.
const INDEX_EVENT_MAX_BYTES: usize = 300;

/// Standing-index plane recorded on every `index_event` line.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum IndexPlane {
    Callgraph,
    Search,
    Semantic,
    Tier2,
}

impl IndexPlane {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Callgraph => "callgraph",
            Self::Search => "search",
            Self::Semantic => "semantic",
            Self::Tier2 => "tier2",
        }
    }
}

/// Lifecycle kind recorded on every `index_event` line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IndexEventKind {
    BuildStarted,
    BuildProgress,
    BuildReady,
    BuildSuperseded,
    #[allow(dead_code)]
    BuildCancelled,
    BuildFailed,
    BuildSuspended,
    BreakerAdmitted,
    BreakerReset,
    ArtifactLoaded,
    FirstQuery,
}

impl IndexEventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BuildStarted => "build_started",
            Self::BuildProgress => "build_progress",
            Self::BuildReady => "build_ready",
            Self::BuildSuperseded => "build_superseded",
            Self::BuildCancelled => "build_cancelled",
            Self::BuildFailed => "build_failed",
            Self::BuildSuspended => "build_suspended",
            Self::BreakerAdmitted => "breaker_admitted",
            Self::BreakerReset => "breaker_reset",
            Self::ArtifactLoaded => "artifact_loaded",
            Self::FirstQuery => "first_query",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::BuildReady
                | Self::BuildSuperseded
                | Self::BuildCancelled
                | Self::BuildFailed
                | Self::BuildSuspended
        )
    }
}

/// One structured standing-index lifecycle record.
#[derive(Clone, Debug)]
pub(crate) struct IndexEvent {
    pub kind: IndexEventKind,
    pub plane: IndexPlane,
    pub build_id: String,
    pub root: PathBuf,
    pub key: String,
    extra: Vec<(&'static str, String)>,
}

impl IndexEvent {
    pub(crate) fn new(
        kind: IndexEventKind,
        plane: IndexPlane,
        build_id: impl Into<String>,
        root: impl AsRef<Path>,
        key: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            plane,
            build_id: build_id.into(),
            root: root.as_ref().to_path_buf(),
            key: key.into(),
            extra: Vec::new(),
        }
    }

    pub(crate) fn from_scope(kind: IndexEventKind, scope: &IndexBuildScope) -> Self {
        Self::new(
            kind,
            scope.plane,
            scope.build_id.clone(),
            &scope.root,
            scope.key.clone(),
        )
    }

    pub(crate) fn field(mut self, key: &'static str, value: impl ToString) -> Self {
        self.extra.push((key, value.to_string()));
        self
    }
}

/// Identity of one in-flight build attempt, carried on a thread-local so nested
/// helpers such as `ensure_cold_build_current` can emit without extra params.
#[derive(Clone, Debug)]
pub(crate) struct IndexBuildScope {
    pub plane: IndexPlane,
    pub build_id: String,
    pub root: PathBuf,
    pub key: String,
    pub started_at: Instant,
}

impl IndexBuildScope {
    pub(crate) fn new(plane: IndexPlane, root: impl AsRef<Path>, key: impl Into<String>) -> Self {
        Self {
            plane,
            build_id: mint_index_build_id(),
            root: root.as_ref().to_path_buf(),
            key: key.into(),
            started_at: Instant::now(),
        }
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

struct InFlightBuild {
    build_id: String,
}

struct UnclaimedReady {
    build_id: String,
    key: String,
    ready_at: Instant,
}

#[derive(Default)]
struct IndexLifecycle {
    in_flight: Mutex<HashMap<(String, IndexPlane), InFlightBuild>>,
    unclaimed: Mutex<HashMap<(String, IndexPlane), UnclaimedReady>>,
}

#[derive(Default)]
struct IndexBuildStartSignals {
    sequence: u64,
    last_by_root_plane: HashMap<(String, IndexPlane), u64>,
    in_flight_by_root_plane: HashMap<(String, IndexPlane), u64>,
    waiters: HashMap<(String, IndexPlane), Vec<(u64, crossbeam_channel::Sender<()>)>>,
}

struct ToolCallWaitState {
    waiting_on: WaitingOn,
    waiting_on_build_id: Option<String>,
    wait_ms: u64,
    queue_ms: u64,
}

impl Default for ToolCallWaitState {
    fn default() -> Self {
        Self {
            waiting_on: WaitingOn::None,
            waiting_on_build_id: None,
            wait_ms: 0,
            queue_ms: 0,
        }
    }
}

static INDEX_BUILD_COUNTER: AtomicU64 = AtomicU64::new(1);
static INDEX_LIFECYCLE: LazyLock<IndexLifecycle> = LazyLock::new(IndexLifecycle::default);
static INDEX_BUILD_START_SIGNALS: LazyLock<Mutex<IndexBuildStartSignals>> =
    LazyLock::new(|| Mutex::new(IndexBuildStartSignals::default()));

thread_local! {
    static CURRENT_INDEX_BUILD: RefCell<Option<IndexBuildScope>> = const { RefCell::new(None) };
    static TOOL_CALL_WAIT: RefCell<ToolCallWaitState> = const {
        RefCell::new(ToolCallWaitState {
            waiting_on: WaitingOn::None,
            waiting_on_build_id: None,
            wait_ms: 0,
            queue_ms: 0,
        })
    };
}

#[cfg(test)]
static INDEX_EVENT_CAPTURE: LazyLock<Mutex<Option<Vec<String>>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
static INDEX_EVENT_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// Mint a stable per-attempt id (`b-<pid>-<n>`) at `build_started`.
pub(crate) fn mint_index_build_id() -> String {
    let n = INDEX_BUILD_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("b-{}-{n}", std::process::id())
}

/// RAII install of the current index-build attempt on this thread.
pub(crate) struct IndexBuildGuard {
    previous: Option<IndexBuildScope>,
}

impl Drop for IndexBuildGuard {
    fn drop(&mut self) {
        CURRENT_INDEX_BUILD.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Install `scope` as the current index-build attempt until the guard drops.
pub(crate) fn install_index_build(scope: IndexBuildScope) -> IndexBuildGuard {
    let previous = CURRENT_INDEX_BUILD.with(|slot| slot.replace(Some(scope)));
    IndexBuildGuard { previous }
}

/// Run `f` with `scope` as the current index-build attempt on this thread.
#[allow(dead_code)]
pub(crate) fn with_index_build<R>(scope: IndexBuildScope, f: impl FnOnce() -> R) -> R {
    let _guard = install_index_build(scope);
    f()
}

/// Emits `build_failed` if the attempt returns without a terminal event.
pub(crate) struct IndexBuildFailureGuard {
    armed: bool,
}

impl IndexBuildFailureGuard {
    pub(crate) fn new() -> Self {
        Self { armed: true }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for IndexBuildFailureGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(scope) = current_index_build() else {
            return;
        };
        // A prior terminal event already cleared the in-flight slot.
        if in_flight_build_id(scope.plane, &scope.root).as_deref() != Some(scope.build_id.as_str())
        {
            return;
        }
        log_current_index_event(
            IndexEventKind::BuildFailed,
            &[("reason", "error".to_string())],
        );
    }
}

pub(crate) fn current_index_build() -> Option<IndexBuildScope> {
    CURRENT_INDEX_BUILD.with(|slot| slot.borrow().clone())
}

/// Normalize a project root for `index_event` (`\` → `/`).
pub(crate) fn normalize_index_root(root: &Path) -> String {
    root.to_string_lossy().replace('\\', "/")
}

fn sanitize_index_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len().max(1));
    for ch in value.chars() {
        match ch {
            ' ' | '\t' | '\n' | '\r' | '=' => out.push('_'),
            '\\' => out.push('/'),
            c if c.is_ascii_graphic() || c == '/' => out.push(c),
            _ => out.push('_'),
        }
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

fn push_index_field(line: &mut String, key: &str, value: &str) {
    let sanitized = sanitize_index_value(value);
    let addition = format!(" {key}={sanitized}");
    if line.len() + addition.len() > INDEX_EVENT_MAX_BYTES {
        return;
    }
    line.push_str(&addition);
}

/// Keep the distinctive suffix of a long root so the five required fields still fit.
fn left_truncate_root(root: &str, max_bytes: usize) -> String {
    const MARKER: &str = "...";
    if root.len() <= max_bytes {
        return root.to_string();
    }
    let max_bytes = max_bytes.max(MARKER.len());
    let keep = max_bytes.saturating_sub(MARKER.len());
    let mut start = root.len().saturating_sub(keep);
    while start < root.len() && !root.is_char_boundary(start) {
        start += 1;
    }
    format!("{MARKER}{}", &root[start..])
}

fn format_index_event_line(event: &IndexEvent) -> String {
    let kind = sanitize_index_value(event.kind.as_str());
    let plane = sanitize_index_value(event.plane.as_str());
    let build_id = sanitize_index_value(&event.build_id);
    let key = sanitize_index_value(&event.key);
    let mut root = sanitize_index_value(&normalize_index_root(&event.root));
    let prefix = format!("index_event kind={kind} plane={plane} build_id={build_id} root=");
    let key_field = format!(" key={key}");
    let root_budget = INDEX_EVENT_MAX_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(key_field.len());
    if root.len() > root_budget {
        root = left_truncate_root(&root, root_budget);
    }
    let mut line = format!("{prefix}{root}{key_field}");
    for (key, value) in &event.extra {
        push_index_field(&mut line, key, value);
    }
    line
}

fn root_plane_key(root: &Path, plane: IndexPlane) -> (String, IndexPlane) {
    (normalize_index_root(root), plane)
}

fn remember_in_flight(event: &IndexEvent) {
    let key = root_plane_key(&event.root, event.plane);
    if event.kind == IndexEventKind::BuildStarted {
        if let Ok(mut signals) = INDEX_BUILD_START_SIGNALS.lock() {
            signals.sequence = signals.sequence.wrapping_add(1);
            let sequence = signals.sequence;
            signals.last_by_root_plane.insert(key.clone(), sequence);
            signals
                .in_flight_by_root_plane
                .insert(key.clone(), sequence);
            if let Some(waiters) = signals.waiters.remove(&key) {
                for (baseline, sender) in waiters {
                    if sequence > baseline {
                        let _ = sender.send(());
                    }
                }
            }
        }
        if let Ok(mut in_flight) = INDEX_LIFECYCLE.in_flight.lock() {
            in_flight.insert(
                key,
                InFlightBuild {
                    build_id: event.build_id.clone(),
                },
            );
        }
        return;
    }
    if event.kind.is_terminal() {
        if let Ok(mut in_flight) = INDEX_LIFECYCLE.in_flight.lock() {
            in_flight.remove(&key);
        }
        if let Ok(mut signals) = INDEX_BUILD_START_SIGNALS.lock() {
            signals.in_flight_by_root_plane.remove(&key);
        }
        release_index_build_start_waiters(event.plane, &event.root);
    }
    if event.kind == IndexEventKind::BuildReady {
        if let Ok(mut unclaimed) = INDEX_LIFECYCLE.unclaimed.lock() {
            unclaimed.insert(
                key,
                UnclaimedReady {
                    build_id: event.build_id.clone(),
                    key: event.key.clone(),
                    ready_at: Instant::now(),
                },
            );
        }
    }
}

/// Write one greppable `index_event` info line through the house slog path.
pub(crate) fn log_index_event(event: IndexEvent) {
    remember_in_flight(&event);
    let line = format_index_event_line(&event);
    #[cfg(test)]
    if let Ok(mut slot) = INDEX_EVENT_CAPTURE.lock() {
        if let Some(events) = slot.as_mut() {
            events.push(line.clone());
        }
    }
    crate::slog_info!("{}", line);
}

/// Emit an event for the current thread's index-build scope, if any.
pub(crate) fn log_current_index_event(kind: IndexEventKind, extra: &[(&'static str, String)]) {
    let Some(scope) = current_index_build() else {
        return;
    };
    let mut event = IndexEvent::from_scope(kind, &scope);
    for (key, value) in extra {
        event.extra.push((key, value.clone()));
    }
    log_index_event(event);
}

/// Most recent build-start sequence observed for one root and index plane.
pub(crate) fn index_build_start_sequence(plane: IndexPlane, root: &Path) -> u64 {
    INDEX_BUILD_START_SIGNALS
        .lock()
        .ok()
        .and_then(|signals| {
            signals
                .last_by_root_plane
                .get(&root_plane_key(root, plane))
                .copied()
        })
        .unwrap_or(0)
}

/// Send a one-shot signal once the current or next build starts for this root and plane.
pub(crate) fn signal_after_index_build_start(
    plane: IndexPlane,
    root: &Path,
    baseline: u64,
    sender: crossbeam_channel::Sender<()>,
) {
    let Ok(mut signals) = INDEX_BUILD_START_SIGNALS.lock() else {
        let _ = sender.send(());
        return;
    };
    let key = root_plane_key(root, plane);
    if signals.in_flight_by_root_plane.contains_key(&key)
        || signals
            .last_by_root_plane
            .get(&key)
            .is_some_and(|sequence| *sequence > baseline)
    {
        let _ = sender.send(());
    } else {
        signals
            .waiters
            .entry(key)
            .or_default()
            .push((baseline, sender));
    }
}

/// Release start waiters when a build attempt ends before emitting `build_started`.
pub(crate) fn release_index_build_start_waiters(plane: IndexPlane, root: &Path) {
    let waiters = INDEX_BUILD_START_SIGNALS
        .lock()
        .ok()
        .and_then(|mut signals| signals.waiters.remove(&root_plane_key(root, plane)))
        .unwrap_or_default();
    for (_, sender) in waiters {
        let _ = sender.send(());
    }
}

/// In-flight `build_id` for a root/plane, if a cold build has started and not yet terminated.
pub(crate) fn in_flight_build_id(plane: IndexPlane, root: &Path) -> Option<String> {
    INDEX_LIFECYCLE
        .in_flight
        .lock()
        .ok()?
        .get(&root_plane_key(root, plane))
        .map(|build| build.build_id.clone())
}

/// Consume the per-root unclaimed ready slot for `plane` and emit `first_query` once.
pub(crate) fn claim_first_query(
    plane: IndexPlane,
    root: &Path,
    tool: &str,
    queue_ms: u64,
    service_ms: u64,
    status: &str,
) -> bool {
    let slot = {
        let Ok(mut unclaimed) = INDEX_LIFECYCLE.unclaimed.lock() else {
            return false;
        };
        unclaimed.remove(&root_plane_key(root, plane))
    };
    let Some(slot) = slot else {
        return false;
    };
    let ready_to_first_query_ms = slot.ready_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
    log_index_event(
        IndexEvent::new(
            IndexEventKind::FirstQuery,
            plane,
            slot.build_id,
            root,
            slot.key,
        )
        .field("tool", tool)
        .field("queue_ms", queue_ms)
        .field("service_ms", service_ms)
        .field("status", status)
        .field("ready_to_first_query_ms", ready_to_first_query_ms),
    );
    true
}

/// Query-path helper: claim `first_query` on a ready plane, or attribute a Building wait.
pub(crate) fn note_index_query(
    plane: IndexPlane,
    root: &Path,
    tool: &str,
    service_ms: u64,
    status: &str,
) {
    match status {
        "building" | "rebuilding" => {
            let build_id = in_flight_build_id(plane, root);
            note_tool_call_wait(WaitingOn::Build, build_id.as_deref(), 0);
        }
        _ => {
            let queue_ms = TOOL_CALL_WAIT.with(|slot| slot.borrow().queue_ms);
            claim_first_query(plane, root, tool, queue_ms, service_ms, status);
        }
    }
}

/// Reset causal-wait state on the executor worker at job admission.
pub(crate) fn reset_tool_call_wait() {
    TOOL_CALL_WAIT.with(|slot| *slot.borrow_mut() = ToolCallWaitState::default());
}

/// Record what the current tool-call thread waited on.
pub(crate) fn note_tool_call_wait(waiting_on: WaitingOn, build_id: Option<&str>, wait_ms: u64) {
    TOOL_CALL_WAIT.with(|slot| {
        let mut state = slot.borrow_mut();
        state.waiting_on = waiting_on;
        state.waiting_on_build_id = build_id.map(str::to_string);
        state.wait_ms = state.wait_ms.saturating_add(wait_ms);
    });
}

pub(crate) fn note_tool_call_queue_ms(queue_ms: u64) {
    TOOL_CALL_WAIT.with(|slot| slot.borrow_mut().queue_ms = queue_ms);
}

pub(crate) fn take_tool_call_wait() -> (WaitingOn, Option<String>, u64) {
    TOOL_CALL_WAIT.with(|slot| {
        let state = std::mem::take(&mut *slot.borrow_mut());
        (state.waiting_on, state.waiting_on_build_id, state.wait_ms)
    })
}

#[cfg(test)]
pub(crate) fn capture_index_events<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let _serial = INDEX_EVENT_CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(mut slot) = INDEX_EVENT_CAPTURE.lock() {
        *slot = Some(Vec::new());
    }
    let result = f();
    let events = INDEX_EVENT_CAPTURE
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_default();
    (result, events)
}

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

    let waiting_on_build_id = phases.waiting_on_build_id.as_deref().unwrap_or("-");
    crate::slog_debug!(
        "tool_call phase name={} channel={} corr={} total_ms={:.3} queue_ms={:.3} translate_ms={:.3} exec_ms={:.3} format_ms={:.3} finalize_ms={:.3} egress_ms={:.3} egress_enqueue_ms={:.3} egress_queue_ms={:.3} egress_prepare_ms={:.3} egress_write_ms={:.3} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} waiting_on={} waiting_on_build_id={} wait_ms={} root={}",
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
        phases.waiting_on.as_str(),
        waiting_on_build_id,
        phases.wait_ms,
        root.display(),
    );

    if phases.total > SLOW_TOOL_CALL_THRESHOLD {
        crate::slog_warn!(
            "slow tool_call name={} channel={} corr={} total={}ms queue={} translate={} exec={} format={} finalize={} egress={} egress_enqueue={} egress_queue={} egress_prepare={} egress_write={} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} waiting_on={} waiting_on_build_id={} wait_ms={} root={}",
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
            phases.waiting_on.as_str(),
            waiting_on_build_id,
            phases.wait_ms,
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
        "perf tick: watcher={{ingested:{},paths:{},dropped:{}}} drains={} tier2=[{}] semantic={{collects:{},files:{},chunks:{},ms:{}}} callgraph_invalidations={} executor_completed={{interactive:{},maintenance:{}}} oldest_queued_ms={{interactive:{},maintenance:{}}} {} file_log_dropped={}",
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

    fn index_event_matches_grammar(line: &str) -> bool {
        let Some(rest) = line.strip_prefix("index_event ") else {
            return false;
        };
        if rest.is_empty() {
            return false;
        }
        rest.split(' ').all(|token| {
            let Some((key, value)) = token.split_once('=') else {
                return false;
            };
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && !value.is_empty()
                && !value.contains(' ')
                && !value.contains('=')
        })
    }

    fn index_event_fields(line: &str) -> BTreeMap<String, String> {
        let mut fields = BTreeMap::new();
        for token in line.split_whitespace().skip(1) {
            let Some((key, value)) = token.split_once('=') else {
                continue;
            };
            fields.insert(key.to_string(), value.to_string());
        }
        fields
    }

    fn assert_index_event_grammar(lines: &[String]) {
        for line in lines {
            assert!(
                index_event_matches_grammar(line),
                "index_event line failed grammar: {line}"
            );
        }
    }

    fn event_matches(line: &str, plane: &str, root: &str, key: &str) -> bool {
        let fields = index_event_fields(line);
        fields.get("plane").map(String::as_str) == Some(plane)
            && fields.get("root").map(String::as_str) == Some(root)
            && fields.get("key").map(String::as_str) == Some(key)
    }

    fn assert_lifecycle_sequence(
        lines: &[String],
        plane: &str,
        root: &str,
        key: &str,
        require_progress: bool,
    ) {
        let events: Vec<_> = lines
            .iter()
            .filter(|line| event_matches(line, plane, root, key))
            .cloned()
            .collect();
        assert_index_event_grammar(&events);
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|line| index_event_fields(line).get("kind").cloned())
            .filter(|kind| {
                matches!(
                    kind.as_str(),
                    "build_started" | "build_progress" | "build_ready"
                )
            })
            .collect();
        assert!(
            kinds.first().is_some_and(|kind| kind == "build_started"),
            "expected build_started first, got {kinds:?} from {events:?}"
        );
        if require_progress {
            assert!(
                kinds.iter().any(|kind| kind == "build_progress"),
                "expected build_progress in {kinds:?} from {events:?}"
            );
        }
        assert!(
            kinds.last().is_some_and(|kind| kind == "build_ready"),
            "expected build_ready last, got {kinds:?} from {events:?}"
        );
        let build_ids: Vec<_> = events
            .iter()
            .filter_map(|line| index_event_fields(line).get("build_id").cloned())
            .collect();
        assert!(!build_ids.is_empty());
        assert!(
            build_ids.iter().all(|id| id == &build_ids[0]),
            "build_id drifted across events: {build_ids:?}"
        );
        for line in &events {
            let fields = index_event_fields(line);
            assert_eq!(fields.get("root").map(String::as_str), Some(root), "{line}");
            assert_eq!(fields.get("key").map(String::as_str), Some(key), "{line}");
        }
    }

    fn tiny_project(name: &str, file_name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join(name);
        std::fs::create_dir_all(&root).expect("create project");
        std::fs::write(root.join(file_name), contents).expect("write fixture");
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        (temp, root)
    }

    #[test]
    fn index_event_grammar_rejects_spaces_and_equals_in_values() {
        let (result, lines) = capture_index_events(|| {
            log_index_event(
                IndexEvent::new(
                    IndexEventKind::BuildStarted,
                    IndexPlane::Search,
                    "b-1-1",
                    "/tmp/root",
                    "abc123",
                )
                .field("stage", "streaming"),
            );
        });
        let _ = result;
        assert_index_event_grammar(&lines);
        // The capture slot is process-wide: a parallel lib test that builds an
        // index can emit into the same window. Only this test's own event
        // (unique build_id) is the subject here.
        let own: Vec<&String> = lines
            .iter()
            .filter(|line| {
                index_event_fields(line).get("build_id").map(String::as_str) == Some("b-1-1")
            })
            .collect();
        assert_eq!(own.len(), 1, "{lines:?}");
    }

    #[test]
    fn index_event_keeps_required_fields_for_long_root() {
        let root = format!("/{}", "a".repeat(399));
        let (_, lines) = capture_index_events(|| {
            log_index_event(IndexEvent::new(
                IndexEventKind::BuildStarted,
                IndexPlane::Search,
                "b-9-9",
                &root,
                "keepkey",
            ));
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        let fields = index_event_fields(&lines[0]);
        assert_eq!(fields.get("build_id").map(String::as_str), Some("b-9-9"));
        assert_eq!(fields.get("key").map(String::as_str), Some("keepkey"));
        assert!(fields.get("root").is_some(), "{lines:?}");
        assert_index_event_grammar(&lines);
    }

    #[test]
    fn first_query_fires_once_per_build_id() {
        let root = PathBuf::from("/tmp/first-query-root");
        let key = "firstquerykey";
        let build_id = "b-1-99";
        let (_, lines) = capture_index_events(|| {
            log_index_event(IndexEvent::new(
                IndexEventKind::BuildReady,
                IndexPlane::Search,
                build_id,
                &root,
                key,
            ));
            assert!(claim_first_query(
                IndexPlane::Search,
                &root,
                "grep",
                1,
                2,
                "ok"
            ));
            assert!(!claim_first_query(
                IndexPlane::Search,
                &root,
                "grep",
                1,
                2,
                "ok"
            ));
        });
        let first_query: Vec<_> = lines
            .iter()
            .filter(|line| line.contains("kind=first_query"))
            .collect();
        assert_eq!(first_query.len(), 1, "{lines:?}");
        let fields = index_event_fields(first_query[0]);
        assert_eq!(fields.get("build_id").map(String::as_str), Some(build_id));
        assert_eq!(fields.get("tool").map(String::as_str), Some("grep"));
    }

    #[test]
    fn callgraph_cold_build_emits_started_progress_ready() {
        let (_temp, root) = tiny_project("cg", "lib.rs", "pub fn marker() {}\n");
        let key = crate::search_index::artifact_cache_key(&root);
        let callgraph_dir = _temp.path().join("callgraph").join(&key);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let source = root.join("lib.rs");
        let (built, lines) = capture_index_events(|| {
            crate::callgraph_store::CallGraphStore::cold_build_with_lease(
                callgraph_dir,
                root.clone(),
                std::slice::from_ref(&source),
            )
        });
        built.expect("callgraph cold build");
        assert_lifecycle_sequence(
            &lines,
            "callgraph",
            &normalize_index_root(&root),
            &key,
            true,
        );
    }

    #[test]
    fn search_cold_build_emits_started_progress_ready() {
        let (_temp, root) = tiny_project("search", "file.txt", "alpha token\n");
        let key = crate::search_index::artifact_cache_key(&root);
        let (index, lines) = capture_index_events(|| {
            crate::search_index::SearchIndex::build_with_limit(&root, 1_000_000)
        });
        assert!(index.ready);
        assert_lifecycle_sequence(&lines, "search", &normalize_index_root(&root), &key, true);
    }

    #[test]
    fn semantic_cold_build_emits_started_progress_ready() {
        let (_temp, root) = tiny_project("sem", "lib.rs", "pub fn hello() {}\n");
        let key = crate::search_index::artifact_cache_key(&root);
        let files = vec![root.join("lib.rs")];
        let mut embed = |texts: Vec<String>| {
            Ok(texts
                .into_iter()
                .map(|_| vec![0.01_f32; 384])
                .collect::<Vec<_>>())
        };
        let (built, lines) = capture_index_events(|| {
            crate::semantic_index::SemanticIndex::build(&root, &files, &mut embed, 8)
        });
        built.expect("semantic build");
        assert_lifecycle_sequence(&lines, "semantic", &normalize_index_root(&root), &key, true);
    }

    #[test]
    fn tier2_category_emits_started_ready() {
        let (_temp, root) = tiny_project("t2", "mod0.ts", "export function f0() { return 0; }\n");
        let key = crate::search_index::artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let inspect_dir = _temp.path().join("inspect");
        let manager = std::sync::Arc::new(crate::inspect::InspectManager::new());
        let snapshot = crate::inspect::InspectSnapshot::new(
            root.clone(),
            inspect_dir,
            std::sync::Arc::new(crate::config::Config {
                project_root: Some(root.clone()),
                ..crate::config::Config::default()
            }),
            std::sync::Arc::new(std::sync::RwLock::new(crate::parser::SymbolCache::new())),
        );
        let (outcome, lines) = capture_index_events(|| {
            manager.tier2_run_with_reuse_blocking_fresh(
                snapshot,
                crate::inspect::InspectCategory::Complexity,
                crate::inspect::JobScope::for_project(root.clone()),
            )
        });
        assert!(outcome.payload().is_some(), "{outcome:?}");
        assert_lifecycle_sequence(&lines, "tier2", &normalize_index_root(&root), &key, false);
        assert!(
            !lines.iter().any(|line| {
                event_matches(line, "tier2", &normalize_index_root(&root), &key)
                    && line.contains("kind=build_progress")
            }),
            "tier2 must not emit synthetic build_progress: {lines:?}"
        );
    }

    #[test]
    fn superseded_cold_build_keeps_build_id_and_skips_ready() {
        let (_temp, root) = tiny_project("sup", "lib.rs", "pub fn marker() {}\n");
        let key = crate::search_index::artifact_cache_key(&root);
        let callgraph_dir = _temp.path().join("callgraph").join(&key);
        crate::root_cache::configure_artifact_access(&root, &key, false);
        let source = root.join("lib.rs");
        let epoch = crate::root_cache::ArtifactPublishEpoch::default();
        let stale_epoch = epoch.current();
        epoch.next();
        let (failed, lines) = capture_index_events(|| {
            crate::callgraph_store::with_publish_epoch(epoch, stale_epoch, || {
                crate::callgraph_store::CallGraphStore::cold_build_with_lease(
                    callgraph_dir,
                    root.clone(),
                    std::slice::from_ref(&source),
                )
            })
        });
        assert!(matches!(
            failed,
            Err(crate::callgraph_store::CallGraphStoreError::Superseded)
        ));
        let root_s = normalize_index_root(&root);
        let events: Vec<_> = lines
            .iter()
            .filter(|line| event_matches(line, "callgraph", &root_s, &key))
            .cloned()
            .collect();
        assert_index_event_grammar(&events);
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|line| index_event_fields(line).get("kind").cloned())
            .collect();
        assert!(
            kinds.contains(&"build_started".to_string()),
            "{kinds:?} {events:?}"
        );
        assert!(
            kinds.contains(&"build_superseded".to_string()),
            "{kinds:?} {events:?}"
        );
        assert!(
            !kinds.contains(&"build_ready".to_string()),
            "superseded build must not emit build_ready: {kinds:?}"
        );
        let started_id = events
            .iter()
            .find(|line| line.contains("kind=build_started"))
            .and_then(|line| index_event_fields(line).get("build_id").cloned());
        let superseded_id = events
            .iter()
            .find(|line| line.contains("kind=build_superseded"))
            .and_then(|line| index_event_fields(line).get("build_id").cloned());
        assert_eq!(started_id, superseded_id);
        for line in &events {
            let fields = index_event_fields(line);
            assert_eq!(
                fields.get("root").map(String::as_str),
                Some(root_s.as_str())
            );
            assert_eq!(fields.get("key").map(String::as_str), Some(key.as_str()));
        }
    }
}
