//! Durable process logging and low-cost periodic performance summaries.
//!
//! Rust module processes use one file per PID. That avoids cross-process rename
//! races while preserving a single greppable directory for all AFT activity.

use crate::executor::{Executor, ExecutorContext};
use crate::process_identity::is_process_alive;
use crate::tool_progress::{ToolCallPhaseDurations, WaitingOn};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

mod stderr_mirror;

static SUBC_LOGGING: AtomicBool = AtomicBool::new(false);
static STDERR_MIRROR: OnceLock<stderr_mirror::Mirror> = OnceLock::new();
static STDERR_MIRROR_HEALTH: LazyLock<Arc<stderr_mirror::MirrorHealth>> =
    LazyLock::new(|| Arc::new(stderr_mirror::MirrorHealth::default()));

/// Maximum size of an active Rust or plugin log before its single backup rotates in.
const LOG_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Keep one backup generation; retention is hygiene rather than a user setting.
const LOG_GENERATIONS: usize = 1;
/// Check the active file on every write so the cap is not exceeded by a burst.
const ROTATION_CHECK_EVERY: u64 = 1;
const LOG_CHANNEL_CAPACITY: usize = 4096;
const LOG_WRITE_RETRY_MIN: Duration = Duration::from_secs(1);
const LOG_WRITE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Do not reap a dead PID's file until it has been quiet for at least one day.
const DEAD_PROCESS_LOG_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Limit the total regular-file footprint left in the log directory.
const LOG_DIRECTORY_BUDGET_BYTES: u64 = 200 * 1024 * 1024;
/// Maintenance ticks may call the sweep, but actual directory work is hourly.
const LOG_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const DEFAULT_PERF_TICK_INTERVAL: Duration = Duration::from_secs(60);
const PERF_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const SLOW_TOOL_CALL_THRESHOLD: Duration = Duration::from_millis(50);
const TOOL_CALL_SAMPLE_CAPACITY: usize = 256;
/// Census lines stay greppable and short; extra fields are dropped past this.
const INDEX_EVENT_MAX_BYTES: usize = 300;
/// Reserve independent space for the message and trace: a huge panic payload
/// must not crowd every stack frame out of the diagnostic.
const PANIC_MESSAGE_BYTES: usize = 4 * 1024;
const PANIC_BACKTRACE_BYTES: usize = 24 * 1024;
const PANIC_TRUNCATION: &str = "\n[panic diagnostic truncated]";

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
            Self::FirstQuery => "first_query",
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::BuildReady | Self::BuildSuperseded | Self::BuildCancelled | Self::BuildFailed
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

#[cfg(any(test, feature = "test-support"))]
static INDEX_EVENT_CAPTURE: LazyLock<Mutex<Option<Vec<String>>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(any(test, feature = "test-support"))]
static INDEX_EVENT_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static LOG_LINE_CAPTURE: RefCell<Option<Arc<Mutex<Vec<String>>>>> = const { RefCell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn capture_log_lines<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let result = capture_log_lines_into(capture.clone(), f);
    let lines = capture.lock().unwrap().clone();
    (result, lines)
}

/// Share a worker thread's captured records with the test while it is running.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn capture_log_lines_into<R>(
    capture: Arc<Mutex<Vec<String>>>,
    f: impl FnOnce() -> R,
) -> R {
    // Keep the test representative of a libtest process where another test
    // may already have installed the global logger.
    let _ = env_logger::builder().is_test(true).try_init();
    let previous = LOG_LINE_CAPTURE.with(|slot| slot.replace(Some(capture)));
    let result = f();
    LOG_LINE_CAPTURE.with(|slot| slot.replace(previous));
    result
}

#[cfg(any(test, feature = "test-support"))]
fn capture_log_line(level: log::Level, line: &str) {
    LOG_LINE_CAPTURE.with(|slot| {
        if let Some(lines) = slot.borrow().as_ref() {
            lines.lock().unwrap().push(format!("{level} {line}"));
        }
    });
}

/// Whether this thread is collecting slog records for a unit test.
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
#[inline]
pub fn capture_active() -> bool {
    LOG_LINE_CAPTURE.with(|slot| slot.borrow().is_some())
}

/// Production builds never capture log lines in memory.
#[doc(hidden)]
#[cfg(not(any(test, feature = "test-support")))]
#[inline]
pub fn capture_active() -> bool {
    false
}

#[doc(hidden)]
#[inline]
pub fn slog_info_enabled(target: &'static str) -> bool {
    log::log_enabled!(target: target, log::Level::Info) || capture_active()
}

#[doc(hidden)]
#[inline]
pub fn slog_warn_enabled(target: &'static str) -> bool {
    log::log_enabled!(target: target, log::Level::Warn) || capture_active()
}

#[doc(hidden)]
#[inline]
pub fn slog_error_enabled(target: &'static str) -> bool {
    log::log_enabled!(target: target, log::Level::Error) || capture_active()
}

#[doc(hidden)]
#[inline]
pub fn slog_debug_enabled(target: &'static str) -> bool {
    log::log_enabled!(target: target, log::Level::Debug) || capture_active()
}

// Exported slog macros expand outside this module, so their entry points must
// be public even though callers should use the macros instead.
#[doc(hidden)]
pub fn emit_slog_info(target: &'static str, message: String) {
    emit_slog(target, log::Level::Info, message);
}

#[doc(hidden)]
pub fn emit_slog_warn(target: &'static str, message: String) {
    emit_slog(target, log::Level::Warn, message);
}

#[doc(hidden)]
pub fn emit_slog_error(target: &'static str, message: String) {
    emit_slog(target, log::Level::Error, message);
}

#[doc(hidden)]
pub fn emit_slog_debug(target: &'static str, message: String) {
    emit_slog(target, log::Level::Debug, message);
}

fn emit_slog(target: &'static str, level: log::Level, message: String) {
    #[cfg(any(test, feature = "test-support"))]
    capture_log_line(level, &message);
    log::log!(target: target, level, "{message}");
}

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

fn push_index_field(line: &mut String, key: &str, value: &str, max_bytes: usize) {
    let sanitized = sanitize_index_value(value);
    let addition = format!(" {key}={sanitized}");
    if line.len() + addition.len() > max_bytes {
        return;
    }
    line.push_str(&addition);
}

/// Keep the last two components when possible, with a stable hash to distinguish worktrees.
fn left_truncate_root(root: &str, max_bytes: usize) -> String {
    if root.len() <= max_bytes {
        return root.to_string();
    }
    let hash = format!("{:x}", Sha256::digest(root.as_bytes()));
    let marker = format!(".../{}", &hash[..8]);
    if max_bytes <= marker.len() {
        return marker[..max_bytes].to_string();
    }
    let root = root.trim_end_matches('/');
    let suffix_start = root.rmatch_indices('/').nth(1).map_or(0, |(i, _)| i + 1);
    let keep = max_bytes.saturating_sub(marker.len() + 1);
    let mut start = suffix_start.max(root.len().saturating_sub(keep));
    while start < root.len() && !root.is_char_boundary(start) {
        start += 1;
    }
    if start == root.len() {
        marker
    } else {
        format!("{marker}/{}", &root[start..])
    }
}

fn format_index_event_line(event: &IndexEvent) -> String {
    let kind = sanitize_index_value(event.kind.as_str());
    let plane = sanitize_index_value(event.plane.as_str());
    let build_id = sanitize_index_value(&event.build_id);
    let key = sanitize_index_value(&event.key);
    let mut root = sanitize_index_value(&normalize_index_root(&event.root));
    let prefix = format!("index_event kind={kind} plane={plane} build_id={build_id} root=");
    let key_field = format!(" key={key}");
    // Reserve measurements before descriptive fields, and give the root only the
    // remaining bytes. Keep root/key in their current positions so existing readers
    // can parse the record.
    let extra_budget = INDEX_EVENT_MAX_BYTES
        .saturating_sub(prefix.len() + key_field.len())
        .saturating_sub(root.len().min(".../00000000".len()));
    let mut extra_fields = String::new();
    for numeric in [true, false] {
        for (key, value) in &event.extra {
            if value.parse::<u64>().is_ok() == numeric {
                push_index_field(&mut extra_fields, key, value, extra_budget);
            }
        }
    }
    let root_budget = INDEX_EVENT_MAX_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(key_field.len())
        .saturating_sub(extra_fields.len());
    if root.len() > root_budget {
        root = left_truncate_root(&root, root_budget);
    }
    format!("{prefix}{root}{key_field}{extra_fields}")
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

fn emit_index_event_line(line: String) {
    #[cfg(any(test, feature = "test-support"))]
    if let Ok(mut slot) = INDEX_EVENT_CAPTURE.lock() {
        if let Some(events) = slot.as_mut() {
            events.push(line.clone());
        }
    }
    crate::slog_info!("{}", line);
}

/// Write one greppable `index_event` info line through the house slog path.
pub(crate) fn log_index_event(event: IndexEvent) {
    remember_in_flight(&event);
    emit_index_event_line(format_index_event_line(&event));
}

pub(crate) fn log_watcher_rescan(
    root: &Path,
    reason: RescanReason,
    cost_ms: u64,
    rss_delta_bytes: Option<i64>,
    raw_events_since_last: u64,
) {
    let reason = sanitize_index_value(reason.as_str());
    let rss_delta = rss_delta_bytes
        .map(|delta| delta.to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let prefix = "index_event kind=watcher_rescan plane=watcher root=";
    let suffix = format!(
        " reason={reason} cost_ms={cost_ms} rss_delta_bytes={rss_delta} raw_events_since_last={raw_events_since_last}"
    );
    let mut root = sanitize_index_value(&normalize_index_root(root));
    let root_budget = INDEX_EVENT_MAX_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(suffix.len());
    if root.len() > root_budget {
        root = left_truncate_root(&root, root_budget);
    }
    emit_index_event_line(format!("{prefix}{root}{suffix}"));
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

/// A step of the shared edit pipeline whose time a slow tool call reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditPhase {
    /// Writing the undo snapshot under the backup store's lock.
    Backup,
    /// Atomic writes of the new content (and of a rollback).
    Write,
    /// The project formatter.
    Format,
    /// Tree-sitter syntax checks before and after the write.
    Syntax,
    /// The opt-in full type check.
    Validate,
    /// Line diffs for the reply's counts and the host's patch.
    Diff,
    /// Language-server notification and the optional diagnostics wait.
    Lsp,
}

impl EditPhase {
    const ALL: [EditPhase; 7] = [
        EditPhase::Backup,
        EditPhase::Write,
        EditPhase::Format,
        EditPhase::Syntax,
        EditPhase::Validate,
        EditPhase::Diff,
        EditPhase::Lsp,
    ];

    fn as_str(self) -> &'static str {
        match self {
            EditPhase::Backup => "backup",
            EditPhase::Write => "write",
            EditPhase::Format => "format",
            EditPhase::Syntax => "syntax",
            EditPhase::Validate => "validate",
            EditPhase::Diff => "diff",
            EditPhase::Lsp => "lsp",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Time spent in each [`EditPhase`] by the current tool call's thread.
struct EditPhaseTimes {
    elapsed: [Duration; 7],
    /// Open timers per phase, so a timed helper called from another timed
    /// helper of the same phase is counted once.
    depth: [u8; 7],
    touched: bool,
}

thread_local! {
    static EDIT_PHASES: RefCell<EditPhaseTimes> = const {
        RefCell::new(EditPhaseTimes {
            elapsed: [Duration::ZERO; 7],
            depth: [0; 7],
            touched: false,
        })
    };
}

/// Adds the time until it is dropped to one [`EditPhase`] of this thread's
/// tool call. Only the outermost open timer of a phase counts.
pub(crate) struct EditPhaseTimer {
    phase: EditPhase,
    started: Instant,
}

/// Start timing `phase` on this thread; see [`EditPhaseTimer`].
pub(crate) fn time_edit_phase(phase: EditPhase) -> EditPhaseTimer {
    EDIT_PHASES.with(|slot| {
        let mut times = slot.borrow_mut();
        times.depth[phase.index()] = times.depth[phase.index()].saturating_add(1);
    });
    EditPhaseTimer {
        phase,
        started: Instant::now(),
    }
}

impl Drop for EditPhaseTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        EDIT_PHASES.with(|slot| {
            let mut times = slot.borrow_mut();
            let index = self.phase.index();
            times.depth[index] = times.depth[index].saturating_sub(1);
            if times.depth[index] == 0 {
                times.elapsed[index] += elapsed;
                times.touched = true;
            }
        });
    }
}

/// Clear this thread's edit phase times at the start of a tool call.
pub(crate) fn reset_edit_phase_times() {
    EDIT_PHASES.with(|slot| {
        let mut times = slot.borrow_mut();
        times.elapsed = [Duration::ZERO; 7];
        times.touched = false;
    });
}

/// This thread's edit phase times since the last reset, rendered as
/// `backup:1,write:0,format:0,syntax:12,validate:0,diff:3,lsp:0` (whole
/// milliseconds), or `None` when the call ran no edit step. Clears them.
pub(crate) fn take_edit_phase_times() -> Option<String> {
    EDIT_PHASES.with(|slot| {
        let mut times = slot.borrow_mut();
        if !times.touched {
            return None;
        }
        let rendered = EditPhase::ALL
            .iter()
            .map(|phase| {
                format!(
                    "{}:{}",
                    phase.as_str(),
                    duration_millis_u64(times.elapsed[phase.index()])
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        times.elapsed = [Duration::ZERO; 7];
        times.touched = false;
        Some(rendered)
    })
}

#[cfg(any(test, feature = "test-support"))]
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
    init_with_stderr_mode(false);
}

/// Initialize logging before the supervised module starts its frame runtime.
/// Stderr is a bounded best-effort mirror; the independent file sink is unchanged.
pub fn init_subc() {
    init_with_stderr_mode(true);
}

fn init_with_stderr_mode(subc: bool) {
    SUBC_LOGGING.store(subc, Ordering::Relaxed);
    if subc {
        STDERR_MIRROR.get_or_init(|| {
            stderr_mirror::Mirror::spawn(
                io::stderr(),
                stderr_mirror::QUEUE_CAPACITY,
                Arc::clone(&STDERR_MIRROR_HEALTH),
            )
            .map(|(mirror, _worker)| mirror)
            .unwrap_or_else(|_| stderr_mirror::Mirror::disabled(Arc::clone(&STDERR_MIRROR_HEALTH)))
        });
    }
    let storage_root = crate::storage_path::storage_dir(None);
    let logs_dir = storage_root.join("logs");
    let file_name = format!("aft-{}.log", std::process::id());
    let file_path = logs_dir.join(file_name);
    let (sink, startup_sweep) =
        initial_file_sink(&logs_dir, &file_path, Arc::clone(&LOG_WRITE_HEALTH));
    let (tx, rx) = mpsc::sync_channel(LOG_CHANNEL_CAPACITY);
    let sink = Arc::new(Mutex::new(Some(sink)));
    let worker_sink = Arc::clone(&sink);
    let file_tx = thread::Builder::new()
        .name("aft-log-writer".to_string())
        .spawn(move || {
            run_file_writer(
                worker_sink,
                rx,
                Arc::clone(&LOG_WRITE_HEALTH),
                LOG_WRITE_RETRY_MIN,
            )
        })
        .map(|_| {
            if let Ok(mut control) = FILE_CONTROL.lock() {
                control.tx = Some(tx.clone());
                control.sink = Some(sink);
                control.storage_root = Some(storage_root.clone());
            }
            Some(tx)
        })
        .unwrap_or_else(|error| {
            write_stderr_once(&format!(
                "[aft] WARN durable log disabled: cannot start writer thread: {error}\n"
            ));
            None
        });

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(TeeWriter::new(file_tx))))
        .format(|buf, record| {
            // Wall-clock stamp so post-hoc log forensics can correlate
            // lines with external events (health probes, module bounces).
            // Seconds precision is enough; chrono is avoided on purpose —
            // this hand-rolls UTC from the epoch to keep deps flat.
            write_log_line(
                buf,
                &format_utc_timestamp(),
                record.level(),
                record.target(),
                record.args(),
            )
        })
        .init();

    install_panic_hook(file_path);

    if let Some(summary) = startup_sweep {
        log_sweep_summary(summary);
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn subc_stderr_tee_for_test() -> impl Write + Send {
    TeeWriter::new(None)
}

/// Log before delegating to the previous hook. Do not change RUST_BACKTRACE:
/// stderr retains the default hook's normal short/full/disabled behavior while
/// the durable diagnostic always includes the stack needed after a respawn.
fn install_panic_hook(initial_path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = bounded_panic_text(format_args!("{info}"), PANIC_MESSAGE_BYTES);
        let trace = std::backtrace::Backtrace::force_capture();
        let trace = bounded_panic_text(format_args!("{trace}"), PANIC_BACKTRACE_BYTES);
        let report = crate::log_redact::aft_redact_bytes(
            format!(
                "{} [aft] ERROR panic {message}\nforced backtrace:\n{trace}\n",
                format_utc_timestamp()
            )
            .as_bytes(),
        )
        .into_owned();
        // Never enter env_logger or wait on the file writer from its panic
        // hook: the panicking thread may already hold either logger lock.
        let path = FILE_CONTROL
            .try_lock()
            .ok()
            .and_then(|control| control.storage_root.clone())
            .map(|root| {
                root.join("logs")
                    .join(format!("aft-{}.log", std::process::id()))
            })
            .unwrap_or_else(|| initial_path.clone());
        let persisted = open_private_log_file(&path, false).and_then(|mut file| {
            file.write_all(&report)?;
            // Panic hooks must not enter the logger or its warning-set lock.
            aft_store::sync::sync_all_in_panic(&file, &path)
        });
        if persisted.is_err() {
            let _ = io::stderr().write_all(&report);
        }
        previous(info);
    }));
}

/// The process log follows storage-root reconfiguration, not a caller's project.
pub(crate) fn daemon_log_path() -> PathBuf {
    let root = FILE_CONTROL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .storage_root
        .clone()
        .unwrap_or_else(|| crate::storage_path::storage_dir(None));
    root.join("logs")
        .join(format!("aft-{}.log", std::process::id()))
}

/// Best-effort watchdog evidence, independent of the logger and writer locks.
/// Reopen for each line so normal log rotation does not strand later evidence
/// on a renamed file. The caller must write its guaranteed stderr copy first.
pub(crate) fn write_watchdog_line(path: &Path, message: &str) -> io::Result<()> {
    let mut line = Vec::new();
    write_log_line(
        &mut line,
        &format_utc_timestamp(),
        log::Level::Warn,
        "aft::subc::stall_watchdog",
        &format_args!("{message}"),
    )?;
    open_private_log_file(path, false)?.write_all(&crate::log_redact::aft_redact_bytes(&line))
}

/// The standalone gh shim runs before logger initialization. Preserve its rare
/// authority diagnostics in the process log without adding agent-visible stderr.
pub(crate) fn write_preinit_warning(message: &str) -> io::Result<()> {
    write_preinit_warning_to(&daemon_log_path(), message)
}

fn write_preinit_warning_to(path: &Path, message: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_log_dir(parent)?;
    }
    let mut line = Vec::new();
    write_log_line(
        &mut line,
        &format_utc_timestamp(),
        log::Level::Warn,
        "aft::gh_shim_speech",
        &format_args!("{message}"),
    )?;
    let mut file = open_private_log_file(path, false)?;
    file.write_all(&crate::log_redact::aft_redact_bytes(&line))?;
    aft_store::sync::sync_all(&file, path)
}

fn bounded_panic_text(args: std::fmt::Arguments<'_>, max_bytes: usize) -> String {
    use std::fmt::Write;

    let mut text = PanicText {
        text: String::new(),
        budget: max_bytes.saturating_sub(PANIC_TRUNCATION.len()),
        truncated: false,
    };
    let _ = text.write_fmt(args);
    if text.truncated {
        text.text.push_str(PANIC_TRUNCATION);
    }
    text.text
}

struct PanicText {
    text: String,
    budget: usize,
    truncated: bool,
}

impl std::fmt::Write for PanicText {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let remaining = self.budget.saturating_sub(self.text.len());
        let mut end = value.len().min(remaining);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        self.truncated |= end != value.len();
        Ok(())
    }
}

/// Write one log line in the shared format used by both the stderr copy and
/// the durable file copy (`TeeWriter` receives these exact bytes):
/// `<timestamp> [aft] LEVEL <message>`.
///
/// The level sits after the `[aft]`/`[aft-lsp]` tag rather than before it so
/// every existing reader that anchors on `<timestamp> [aft]` (the health
/// sentinel's CLI-line filter, the plugins' stderr relay tag check, ad-hoc
/// greps) keeps matching, while `grep ' WARN '` or `' ERROR '` now works.
fn write_log_line<W: Write + ?Sized>(
    out: &mut W,
    timestamp: &str,
    level: log::Level,
    target: &str,
    message: &std::fmt::Arguments<'_>,
) -> io::Result<()> {
    let prefix = if target.starts_with("aft::lsp") || target.starts_with("aft_lsp") {
        "[aft-lsp]"
    } else {
        "[aft]"
    };
    writeln!(out, "{timestamp} {prefix} {level} {message}")
}

/// Render `now` as `YYYY-MM-DDTHH:MM:SSZ` without a date-time dependency.
///
/// Civil-date math uses the days-from-epoch algorithm (Howard Hinnant's
/// `civil_from_days`); u64 seconds keep it valid far past 2100.
fn format_utc_timestamp() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = format_epoch_secs(since_epoch.as_secs());
    format!(
        "{}.{:03}Z",
        seconds.trim_end_matches('Z'),
        since_epoch.subsec_millis()
    )
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
    create_private_log_dir(logs_dir)?;
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

fn initial_file_sink(
    logs_dir: &Path,
    file_path: &Path,
    health: Arc<LogWriteHealth>,
) -> (RotatingFile, Option<SweepSummary>) {
    match prepare_file_sink(logs_dir, file_path) {
        Ok((sink, summary)) => (sink, Some(summary)),
        Err(error) => {
            LogWriteRecovery::new(health, LOG_WRITE_RETRY_MIN).failed(&error, 0, Instant::now());
            write_stderr_once(&format!(
                "[aft] WARN durable log will retry opening {}: {error}\n",
                file_path.display()
            ));
            (
                RotatingFile::unopened(
                    file_path.to_path_buf(),
                    LOG_FILE_BYTES,
                    LOG_GENERATIONS,
                    ROTATION_CHECK_EVERY,
                ),
                None,
            )
        }
    }
}

enum LogMessage {
    Write(Vec<u8>),
    Reconfigure(PathBuf),
    /// Write everything queued before this message, then signal the sender.
    Flush(SyncSender<bool>),
}

type SharedFileSink<S = RotatingFile> = Arc<Mutex<Option<S>>>;

/// Only the writer thread performs sink I/O. Tests inject failures at this
/// boundary without consuming disk space or changing the global logger.
trait FileLogSink: Send {
    fn write_batch(&mut self, lines: &[Vec<u8>]) -> io::Result<()>;
    fn reopen(&mut self) -> io::Result<()>;
    fn discard_buffer(&mut self);
    fn reconfigure(&mut self, storage_root: &Path) -> io::Result<SweepSummary>;
}

#[derive(Default)]
struct LogWriteHealth {
    dropped_lines_total: AtomicU64,
    last_write_error: Mutex<Option<String>>,
    failing_since: Mutex<Option<String>>,
}

static LOG_WRITE_HEALTH: LazyLock<Arc<LogWriteHealth>> =
    LazyLock::new(|| Arc::new(LogWriteHealth::default()));

/// Read error metadata, never the file or the sink's I/O lock. The cumulative
/// counter includes queue overflow and unconfirmed/deferred file batches;
/// recovery markers count the latter over each write outage. Partial writes
/// cannot confirm a record, so an errored batch is counted in full.
pub fn durable_log_health() -> serde_json::Value {
    let mut health = LOG_WRITE_HEALTH.snapshot();
    let mut mirror = STDERR_MIRROR_HEALTH.snapshot();
    mirror["best_effort"] = SUBC_LOGGING.load(Ordering::Relaxed).into();
    health["stderr_mirror"] = mirror;
    health
}

impl LogWriteHealth {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "dropped_lines_total": self.dropped_lines_total.load(Ordering::Relaxed),
            "last_write_error": self.last_write_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
            "failing_since": self.failing_since.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
        })
    }

    fn drop_lines(&self, count: u64) {
        self.dropped_lines_total.fetch_add(count, Ordering::Relaxed);
        PERF.file_lines_dropped.fetch_add(count, Ordering::Relaxed);
    }
}

fn log_line_count(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&byte| byte == b'\n').count().max(1) as u64
}

struct LogWriteOutage {
    since: String,
    first_error: String,
    last_error: String,
    dropped_lines: u64,
    retry_at: Instant,
}

struct LogWriteRecovery {
    health: Arc<LogWriteHealth>,
    outage: Option<LogWriteOutage>,
    retry_min: Duration,
    retry_delay: Duration,
}

impl LogWriteRecovery {
    fn new(health: Arc<LogWriteHealth>, retry_min: Duration) -> Self {
        // Initialization can itself fail on a full volume. Retain its path
        // and error, and let the same worker retry instead of disabling it.
        let since = health
            .failing_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let error = health
            .last_write_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let outage = since.zip(error).map(|(since, error)| LogWriteOutage {
            since,
            first_error: error.clone(),
            last_error: error,
            dropped_lines: 0,
            retry_at: Instant::now(),
        });
        Self {
            health,
            outage,
            retry_min,
            retry_delay: retry_min,
        }
    }

    fn failed(&mut self, error: &io::Error, dropped_lines: u64, now: Instant) {
        let error = crate::log_redact::aft_redactor(&error.to_string()).into_owned();
        let outage = self.outage.get_or_insert_with(|| {
            let since = format_utc_timestamp();
            *self
                .health
                .failing_since
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(since.clone());
            LogWriteOutage {
                since,
                first_error: error.clone(),
                last_error: error.clone(),
                dropped_lines: 0,
                retry_at: now,
            }
        });
        outage.last_error = error.clone();
        outage.dropped_lines = outage.dropped_lines.saturating_add(dropped_lines);
        outage.retry_at = now + self.retry_delay;
        self.retry_delay = self.retry_delay.saturating_mul(2).min(LOG_WRITE_RETRY_MAX);
        *self
            .health
            .last_write_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error);
        self.health.drop_lines(dropped_lines);
    }

    fn write_batch(
        &mut self,
        sink: &mut impl FileLogSink,
        lines: &[Vec<u8>],
        now: Instant,
    ) -> bool {
        if lines.is_empty() {
            return true;
        }
        let dropped_lines = lines.iter().map(|line| log_line_count(line)).sum();
        if let Some(outage) = self.outage.as_mut() {
            if now < outage.retry_at {
                // Drain rather than sleep: logging stays bounded even when a
                // full disk remains unavailable for hours.
                outage.dropped_lines = outage.dropped_lines.saturating_add(dropped_lines);
                self.health.drop_lines(dropped_lines);
                return false;
            }
            let recovered_at = format_utc_timestamp();
            let errors = if outage.first_error == outage.last_error {
                outage.first_error.clone()
            } else {
                format!("{}; last error: {}", outage.first_error, outage.last_error)
            };
            let marker = format!(
                "{recovered_at} [aft] WARN log writes failed from {} to {recovered_at} ({errors}); {} lines dropped\n",
                outage.since, outage.dropped_lines
            );
            // Flush the marker separately. No normal line may precede it on
            // the replacement handle, including buffered data from a failure.
            if let Err(error) = sink
                .reopen()
                .and_then(|()| sink.write_batch(&[marker.into_bytes()]))
            {
                sink.discard_buffer();
                self.failed(&error, dropped_lines, now);
                return false;
            }
            self.outage = None;
            self.retry_delay = self.retry_min;
            *self
                .health
                .failing_since
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        if let Err(error) = sink.write_batch(lines) {
            sink.discard_buffer();
            // Some bytes may have reached disk before a write/flush error.
            // Count the whole unconfirmed batch as lost, rather than silently
            // claiming partially written records were durable.
            self.failed(&error, dropped_lines, now);
            return false;
        }
        true
    }
}

#[derive(Default)]
struct FileControl {
    tx: Option<SyncSender<LogMessage>>,
    storage_root: Option<PathBuf>,
    sink: Option<SharedFileSink>,
}

static FILE_CONTROL: LazyLock<Mutex<FileControl>> =
    LazyLock::new(|| Mutex::new(FileControl::default()));

struct TeeWriter {
    file_tx: Option<SyncSender<LogMessage>>,
    stderr: StderrOutput,
}

impl TeeWriter {
    fn new(file_tx: Option<SyncSender<LogMessage>>) -> Self {
        Self {
            file_tx,
            stderr: StderrOutput::for_process(),
        }
    }
}

enum StderrOutput {
    Standalone,
    Subc(stderr_mirror::Mirror),
}

impl StderrOutput {
    fn for_process() -> Self {
        if SUBC_LOGGING.load(Ordering::Relaxed) {
            Self::Subc(STDERR_MIRROR.get().cloned().unwrap_or_else(|| {
                stderr_mirror::Mirror::disabled(Arc::clone(&STDERR_MIRROR_HEALTH))
            }))
        } else {
            Self::Standalone
        }
    }
}

impl Write for StderrOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Standalone => {
                let _ = io::stderr().write_all(bytes);
            }
            Self::Subc(mirror) => mirror.record(bytes),
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Standalone => {
                let _ = io::stderr().flush();
            }
            Self::Subc(mirror) => mirror.flush(),
        }
        Ok(())
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Mask credentials before either copy. Standalone writes stderr
        // synchronously so the plugin can save it in its log; subc uses the
        // best-effort mirror.
        let line = crate::log_redact::aft_redact_bytes(buf);
        // The durable file copy goes first and does not depend on stderr: in
        // subc mode stderr is a pipe the daemon reads, and once the daemon is
        // shutting down that pipe can be closed. A failed stderr write used to
        // return early here and drop the file copy too, which is how a module
        // exit could leave no line explaining it.
        enqueue_file_line(&mut self.file_tx, &line, &LOG_WRITE_HEALTH);
        let _ = self.stderr.write_all(&line);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.stderr.flush();
        Ok(())
    }
}

fn enqueue_file_line(
    file_tx: &mut Option<SyncSender<LogMessage>>,
    line: &[u8],
    health: &LogWriteHealth,
) {
    if let Some(tx) = file_tx.as_ref() {
        match tx.try_send(LogMessage::Write(line.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => health.drop_lines(log_line_count(line)),
            Err(TrySendError::Disconnected(_)) => {
                health.drop_lines(log_line_count(line));
                *file_tx = None;
            }
        }
    }
}

fn run_file_writer<S: FileLogSink>(
    shared_sink: SharedFileSink<S>,
    rx: mpsc::Receiver<LogMessage>,
    health: Arc<LogWriteHealth>,
    retry_min: Duration,
) {
    let mut recovery = LogWriteRecovery::new(health, retry_min);
    let mut durable = true;
    // Test-only: hold each batch so a test can prove exit paths wait for the
    // writer instead of racing it. Debug builds only; release ignores it.
    #[cfg(debug_assertions)]
    let test_delay = std::env::var("AFT_TEST_LOG_WRITER_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    // Test-only: acknowledge each flush request no sooner than this long after
    // the writer collects it. The batch delay above does not bound that: a
    // flush queued near the end of a delayed batch is collected and
    // acknowledged with it almost at once. Debug builds only.
    #[cfg(debug_assertions)]
    let test_flush_hold = std::env::var("AFT_TEST_LOG_FLUSH_HOLD_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(std::time::Duration::from_millis);
    while let Ok(message) = rx.recv() {
        #[cfg(debug_assertions)]
        if let Some(delay) = test_delay {
            thread::sleep(delay);
        }
        let mut lines = Vec::new();
        let mut reconfigure = None;
        let mut flushed = None;
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
                        Ok(LogMessage::Flush(done)) => {
                            flushed = Some(done);
                            break;
                        }
                        Err(_) => break,
                    }
                }
            }
            LogMessage::Reconfigure(storage_root) => reconfigure = Some(storage_root),
            LogMessage::Flush(done) => flushed = Some(done),
        }
        // Exit can take the sink while the worker is delayed. Serialize terminal
        // writes with rotation and reconfiguration, but not with queue waits.
        let mut slot = shared_sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sink) = slot.as_mut() else { break };
        durable &= recovery.write_batch(sink, &lines, Instant::now());
        // The channel is FIFO, so every line queued before the flush request
        // has been attempted by now. Report losses instead of confirming that
        // a failed batch was durable; the receiver stays alive for recovery.
        if let Some(done) = flushed {
            let confirmed = durable;
            durable = true;
            #[cfg(debug_assertions)]
            if let Some(hold) = test_flush_hold {
                // Release the sink first: the exit path takes it to write its
                // terminal line, and must not wait out this hold.
                drop(slot);
                thread::sleep(hold);
                let _ = done.try_send(confirmed);
                continue;
            }
            let _ = done.try_send(confirmed);
        }
        if let Some(storage_root) = reconfigure {
            let logs_dir = storage_root.join("logs");
            let path = logs_dir.join(format!("aft-{}.log", std::process::id()));
            match sink.reconfigure(&storage_root) {
                Ok(summary) => {
                    if let Some(outage) = recovery.outage.as_mut() {
                        outage.retry_at = Instant::now();
                    }
                    log_sweep_summary(summary);
                }
                Err(error) => write_stderr_once(&format!(
                    "[aft] WARN durable log could not switch to {}: {error}\n",
                    path.display()
                )),
            }
        }
    }
}

fn write_stderr_once(message: &str) {
    write_stderr_diagnostic(message);
}

/// Emit a process diagnostic without making subc callers wait on its stderr pipe.
/// Standalone retains its synchronous stderr behavior.
pub fn write_stderr_diagnostic(message: &str) {
    let line = crate::log_redact::aft_redact_bytes(message.as_bytes());
    let _ = StderrOutput::for_process().write_all(&line);
}

/// Wait, at most `timeout`, until the durable log writer has written every line
/// logged before this call.
///
/// The file sink is a background thread fed by a channel, so a process that
/// exits right after logging loses whatever is still queued: the lines closest
/// to an exit, which are the ones forensics needs. Every exit path calls this
/// first. Returns false when the writer did not confirm in time (or the durable
/// log is disabled), so an exit is never held longer than `timeout`.
pub fn flush_durable_log(timeout: std::time::Duration) -> bool {
    let tx = match FILE_CONTROL.lock() {
        Ok(control) => control.tx.clone(),
        Err(_) => None,
    };
    let Some(tx) = tx else {
        return false;
    };
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    // A blocking send: a full queue is exactly when there is the most to lose.
    // The writer drains continuously, so this waits for space rather than
    // dropping the request.
    if tx.send(LogMessage::Flush(done_tx)).is_err() {
        return false;
    }
    done_rx.recv_timeout(timeout).unwrap_or(false)
}

/// Wait only during process exit for the queued stderr flush fence.
/// File-log durability is a separate fence; ordinary log flushes never wait here.
pub fn flush_stderr_on_exit(timeout: std::time::Duration) -> bool {
    STDERR_MIRROR
        .get()
        .is_none_or(|mirror| mirror.drain_on_exit(timeout))
}

/// Test-only (debug builds): stop the stderr mirror from writing until
/// [`flush_stderr_on_exit`] starts, so the lines logged in between are still
/// queued when the exit fence runs. Without the fence they would be lost.
#[cfg(debug_assertions)]
pub fn hold_stderr_mirror_until_exit_fence() {
    if let Some(mirror) = STDERR_MIRROR.get() {
        mirror.hold_until_exit_fence();
    }
}

/// Persist the last process-exit line without depending on the async writer.
/// Taking its active sink prevents later rotation or reconfiguration from
/// moving the marker. Queued diagnostics may be abandoned once their bounded
/// flush expires, but this line is appended and synced before exit proceeds.
pub fn write_terminal_log_sync(message: &str) -> io::Result<()> {
    let shared = FILE_CONTROL
        .lock()
        .map_err(|_| io::Error::other("log control poisoned"))?
        .sink
        .clone()
        .ok_or_else(|| io::Error::other("durable log disabled"))?;
    let mut slot = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut sink = slot
        .take()
        .ok_or_else(|| io::Error::other("log sink already stopped"))?;
    if let Some(mut writer) = sink.writer.take() {
        writer.flush()?;
    }
    write_terminal_line(&sink, &mut StderrOutput::for_process(), message)
}

fn write_terminal_line(
    sink: &RotatingFile,
    stderr: &mut impl Write,
    message: &str,
) -> io::Result<()> {
    let mut line = Vec::new();
    write_log_line(
        &mut line,
        &format_utc_timestamp(),
        log::Level::Info,
        "aft",
        &format_args!("{message}"),
    )?;
    let line = crate::log_redact::aft_redact_bytes(&line);
    let _ = stderr.write_all(&line);
    let mut file = open_private_log_file(&sink.path, false)?;
    file.write_all(&line)?;
    aft_store::sync::sync_all(&file, &sink.path)?;
    sink.write_counter.credit_logical(line.len() as u64);
    Ok(())
}

struct RotatingFile {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    size: u64,
    threshold: u64,
    generations: usize,
    check_every: u64,
    writes_since_check: u64,
    write_counter: crate::write_counters::Counter,
}

impl RotatingFile {
    fn unopened(path: PathBuf, threshold: u64, generations: usize, check_every: u64) -> Self {
        Self {
            path,
            writer: None,
            size: 0,
            threshold,
            generations,
            check_every: check_every.max(1),
            writes_since_check: 0,
            write_counter: crate::write_counters::process_root_counter(
                crate::write_counters::Domain::Logs,
            ),
        }
    }

    fn open(
        path: PathBuf,
        threshold: u64,
        generations: usize,
        check_every: u64,
    ) -> io::Result<Self> {
        let file = open_private_log_file(&path, false)?;
        #[cfg(unix)]
        {
            // Files from before logs were private keep their old mode across
            // opens and renames, so tighten the live file and its backups.
            tighten_if_owned(&path, LOG_FILE_MODE);
            for generation in 1..=generations {
                tighten_if_owned(&rotated_path(&path, generation), LOG_FILE_MODE);
            }
        }
        let size = file.metadata()?.len();
        let mut sink = Self::unopened(path, threshold, generations, check_every);
        sink.writer = Some(BufWriter::new(file));
        sink.size = size;
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
        self.write_counter.credit_logical(batch_bytes);
        self.size = self.size.saturating_add(batch_bytes);
        if self.writes_since_check >= self.check_every {
            self.writes_since_check = 0;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            if let Err(error) = writer.flush() {
                let _ = writer.into_parts();
                return Err(error);
            }
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
        let file = open_private_log_file(&self.path, true)?;
        self.writer = Some(BufWriter::new(file));
        self.size = 0;
        self.writes_since_check = 0;
        Ok(())
    }
}

impl FileLogSink for RotatingFile {
    fn write_batch(&mut self, lines: &[Vec<u8>]) -> io::Result<()> {
        RotatingFile::write_batch(self, lines)
    }

    fn reopen(&mut self) -> io::Result<()> {
        self.discard_buffer();
        create_private_log_dir(
            self.path
                .parent()
                .ok_or_else(|| io::Error::other("log path has no parent"))?,
        )?;
        let file = open_private_log_file(&self.path, false)?;
        self.size = file.metadata()?.len();
        self.writer = Some(BufWriter::new(file));
        self.writes_since_check = 0;
        Ok(())
    }

    fn discard_buffer(&mut self) {
        if let Some(writer) = self.writer.take() {
            // BufWriter::drop flushes implicitly. A failed batch is already
            // counted as lost; never replay it ahead of the recovery marker.
            let _ = writer.into_parts();
        }
    }

    fn reconfigure(&mut self, storage_root: &Path) -> io::Result<SweepSummary> {
        let logs_dir = storage_root.join("logs");
        let path = logs_dir.join(format!("aft-{}.log", std::process::id()));
        let (sink, summary) = prepare_file_sink(&logs_dir, &path)?;
        self.discard_buffer();
        *self = sink;
        Ok(summary)
    }
}

fn rotated_path(base: &Path, generation: usize) -> PathBuf {
    let mut path = base.as_os_str().to_os_string();
    path.push(format!(".{generation}"));
    PathBuf::from(path)
}

/// Owner read/write only for every log file AFT creates.
#[cfg(unix)]
const LOG_FILE_MODE: u32 = crate::private_storage::FILE_MODE;

/// Create the log directory owner-only, and tighten one left over from before
/// logs were private. Only the storage root and its log directory are tightened.
fn create_private_log_dir(dir: &Path) -> io::Result<()> {
    crate::private_storage::open_dir(dir.parent().unwrap_or(dir), dir)
}

/// Open a log file for appending (or truncate it, for a fresh generation after
/// rotation). A newly created file is owner read/write only on Unix.
fn open_private_log_file(path: &Path, truncate: bool) -> io::Result<File> {
    let mut options = crate::private_storage::options();
    options.create(true);
    if truncate {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(LOG_FILE_MODE);
    }
    options.open(path)
}

/// Drop group/other permission bits from `path` when the current user owns it.
/// Best effort: a path owned by someone else (a shared storage root) or one
/// that cannot be changed is left as it is rather than failing log setup.
#[cfg(unix)]
fn tighten_if_owned(path: &Path, mode: u32) {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    if path
        .parent()
        .into_iter()
        .chain(path.parent().and_then(Path::parent))
        .any(|dir| fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.is_symlink()))
    {
        return;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return;
    };
    let Ok(metadata) = file.metadata() else {
        return;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid || metadata.mode() & 0o777 & !mode == 0 {
        return;
    }
    let _ = file.set_permissions(fs::Permissions::from_mode(mode));
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
    /// Files whose PID number is live but belongs to a process started after
    /// the file's last write. Reported separately so a sweep that removes
    /// nothing can be read from the log as "nothing was dead" versus "the
    /// recycled-PID verdict never fired".
    recycled_pids: usize,
    /// Bytes the directory still holds above its budget after the sweep,
    /// because the only remaining candidates were protected (the newest dead
    /// process log inside its forensic window). Zero when the budget was met.
    over_budget_bytes: u64,
}

struct ProcessLogFile {
    path: PathBuf,
    modified: Option<SystemTime>,
    bytes: u64,
    dead: bool,
    old_enough: bool,
    removed: bool,
}

/// A numbered rotation the plugin logger no longer owns. The TypeScript sink
/// keeps exactly one backup generation (`aft-plugin.log.1`); higher numbers
/// are relics of an earlier multi-generation policy and nothing rotates or
/// removes them, so they sat at 140 MB of a 200 MB budget for months.
fn relic_plugin_rotation(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("aft-plugin") else {
        return false;
    };
    let Some((_, generation)) = rest.rsplit_once(".log.") else {
        return false;
    };
    generation
        .parse::<u32>()
        .is_ok_and(|generation| generation >= 2)
}

fn log_sweep_summary(summary: SweepSummary) {
    crate::slog_info!(
        "log retention sweep: removed_files={} bytes_freed={} recycled_pids={}",
        summary.removed_files,
        summary.bytes_freed,
        summary.recycled_pids
    );
    if summary.over_budget_bytes > 0 {
        crate::slog_warn!(
            "log directory over budget by {} bytes after sweep: the newest dead process log is kept as the record of that process's exit",
            summary.over_budget_bytes
        );
    }
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
    let mut relic_rotations = Vec::new();
    let mut live_pids = BTreeMap::new();
    let mut recycled_count = 0_usize;
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
        if relic_plugin_rotation(&name) {
            relic_rotations.push((entry.path(), bytes));
            continue;
        }
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
        let alive = *live_pids.entry(pid).or_insert_with(|| {
            if !is_process_alive(pid) {
                return false;
            }
            let recycled = pid_started_after_last_write(pid, modified);
            if recycled {
                recycled_count += 1;
            }
            !recycled
        });
        process_logs.push(ProcessLogFile {
            path: entry.path(),
            modified,
            bytes,
            dead: pid != own_pid && !alive,
            old_enough,
            removed: false,
        });
    }

    let mut summary = SweepSummary {
        recycled_pids: recycled_count,
        ..SweepSummary::default()
    };
    for file in &mut process_logs {
        if file.dead && file.old_enough && remove_sweep_candidate(&file.path) {
            file.removed = true;
            total_bytes = total_bytes.saturating_sub(file.bytes);
            summary.removed_files += 1;
            summary.bytes_freed = summary.bytes_freed.saturating_add(file.bytes);
        }
    }

    // Relic rotations have no owner and no reader; they go unconditionally,
    // and before any process log is weighed against the budget.
    for (path, bytes) in &relic_rotations {
        if remove_sweep_candidate(path) {
            total_bytes = total_bytes.saturating_sub(*bytes);
            summary.removed_files += 1;
            summary.bytes_freed = summary.bytes_freed.saturating_add(*bytes);
        }
    }

    // The budget backstop is deliberately separate from the age-gated reap:
    // once liveness says a PID is dead, budget pressure may remove even a fresh
    // dead file so the directory can actually converge under its hard limit.
    // Live files remain ineligible regardless of age or budget pressure.
    //
    // One dead file is exempt: the newest one, while it is younger than the
    // age reap. It is the log of the process that died last — the record of
    // an unexplained exit — and on 2026-09-18 a successor's startup sweep
    // deleted exactly that file (the directory was over budget on relic
    // rotations, and the predecessor's log was the only eligible candidate)
    // while the supervisor's own exit record died with the daemon restart
    // that caused the exit. Losing budget headroom for one 32 MB file is the
    // cheaper failure.
    process_logs.sort_by_key(|file| file.modified);
    let protected = process_logs
        .iter()
        .rposition(|file| file.dead && !file.removed && !file.old_enough);
    for (index, file) in process_logs
        .iter_mut()
        .enumerate()
        .filter(|(_, file)| file.dead && !file.removed)
    {
        if total_bytes <= budget_bytes {
            break;
        }
        if Some(index) == protected {
            continue;
        }
        if remove_sweep_candidate(&file.path) {
            file.removed = true;
            total_bytes = total_bytes.saturating_sub(file.bytes);
            summary.removed_files += 1;
            summary.bytes_freed = summary.bytes_freed.saturating_add(file.bytes);
        }
    }
    summary.over_budget_bytes = total_bytes.saturating_sub(budget_bytes);

    Ok(summary)
}

/// Slack for clock and filesystem timestamp granularity when comparing a
/// process start time against a log file's last write.
const PID_REUSE_TOLERANCE_MS: u64 = 2_000;

/// The PID number outlives the process that wrote the log. After a reboot the
/// kernel hands low numbers to system daemons, so a liveness check by number
/// alone pinned sixteen-day-old files under the age reap and the budget
/// backstop alike. The real owner started before it wrote the file, so a start
/// time later than the file's last write proves the number was recycled.
/// Platforms without a start-time source keep the number-only verdict.
fn pid_started_after_last_write(pid: u32, modified: Option<SystemTime>) -> bool {
    let Some(modified_ms) = modified
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|since_epoch| since_epoch.as_millis() as u64)
    else {
        return false;
    };
    crate::process_identity::process_start_time_ms(pid)
        .is_some_and(|started_ms| started_ms > modified_ms.saturating_add(PID_REUSE_TOLERANCE_MS))
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
/// The sweep walks and deletes files, so it runs on its own thread: callers
/// include the subc frame loop, which must not wait on the filesystem.
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

    let spawned = std::thread::Builder::new()
        .name("aft-log-sweep".to_owned())
        .spawn(sweep_logs_now);
    if let Err(error) = spawned {
        crate::slog_warn!("log retention sweep could not start: {}", error);
    }
}

fn sweep_logs_now() {
    let storage_root = FILE_CONTROL
        .lock()
        .ok()
        .and_then(|control| control.storage_root.clone())
        .unwrap_or_else(|| crate::storage_path::storage_dir(None));
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

/// Who made a tool call, which decides how loudly a slow one is logged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallCaller {
    /// A tool call made on behalf of the model (an agent-facing tool).
    Agent,
    /// A background call the plugins make on their own (completion drains,
    /// task status polls, permission previews). Nobody waits on these
    /// interactively, so a slow one is not worth a warning.
    Plumbing,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static SLOW_TOOL_CALL_CAPTURE: RefCell<Vec<(log::Level, String)>> =
        const { RefCell::new(Vec::new()) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn take_slow_tool_call_logs_for_test() -> Vec<String> {
    SLOW_TOOL_CALL_CAPTURE.with(|captured| {
        captured
            .borrow_mut()
            .drain(..)
            .map(|(_, message)| message)
            .collect()
    })
}

/// Log level for a tool call that exceeded `SLOW_TOOL_CALL_THRESHOLD`.
///
/// Plugin plumbing calls (completion drains, status polls, detach checks) run
/// in the background and made up about half of all slow-call warnings, which
/// buried the slow calls an agent actually waited on. They still log at debug
/// so a slow drain can be investigated with debug logging on.
fn slow_tool_call_level(caller: ToolCallCaller) -> log::Level {
    match caller {
        ToolCallCaller::Agent => log::Level::Warn,
        ToolCallCaller::Plumbing => log::Level::Debug,
    }
}

/// Record a completed subc tool call or provider read for slow-call diagnostics
/// and the standing perf-tick window. The writer calls this only after
/// `write_all` has handed the complete response frame to the transport.
pub fn note_tool_call_trace(
    name: &str,
    session: &str,
    root: &Path,
    channel: u16,
    corr: u64,
    phases: ToolCallPhaseDurations,
    caller: ToolCallCaller,
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
        "tool_call phase name={} session={} channel={} corr={} total_ms={:.3} queue_ms={:.3} setup_ms={:.3} translate_ms={:.3} exec_ms={:.3} format_ms={:.3} finalize_ms={:.3} egress_ms={:.3} egress_enqueue_ms={:.3} egress_queue_ms={:.3} egress_prepare_ms={:.3} egress_write_ms={:.3} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} waiting_on={} waiting_on_build_id={} wait_ms={} root={}",
        name,
        session,
        channel,
        corr,
        duration_millis_f64(phases.total),
        duration_millis_f64(phases.queue),
        duration_millis_f64(phases.setup),
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

    // Waiting tools do not separate their requested wait from execution work.
    // Exclude that whole phase rather than warning about an intentional wait;
    // queueing, translation, formatting and transport delays still warrant a warning.
    let diagnostic_elapsed = if phases.intentional_wait {
        phases.total.saturating_sub(phases.execute)
    } else {
        phases.total
    };
    if diagnostic_elapsed > SLOW_TOOL_CALL_THRESHOLD {
        let level = slow_tool_call_level(caller);
        // The argument shape and edit phase times go before `root=`, which
        // stays last because a root path may contain spaces.
        let mut detail = String::new();
        if let Some(shape) = phases.arg_shape.as_deref() {
            detail.push(' ');
            detail.push_str(shape);
        }
        if let Some(edit_phases) = phases.edit_phases.as_deref() {
            detail.push_str(" edit_phases=");
            detail.push_str(edit_phases);
        }
        let message = format!(
            "slow tool_call name={} session={} channel={} corr={} total={}ms queue={} setup={} translate={} exec={} format={} finalize={} egress={} egress_enqueue={} egress_queue={} egress_prepare={} egress_write={} frame_bytes={} writer_queue_depth={} writer_active={} writer_queue_full={} reserve_timeouts={} waiting_on={} waiting_on_build_id={} wait_ms={}{} root={}",
            name,
            session,
            channel,
            corr,
            duration_millis_u64(phases.total),
            slow_phase_time(phases.queue, None),
            slow_phase_time(phases.setup, phases.cpu.setup),
            slow_phase_time(phases.translate, phases.cpu.translate),
            slow_phase_time(phases.execute, phases.cpu.execute),
            slow_phase_time(phases.format, phases.cpu.format),
            slow_phase_time(phases.finalize, phases.cpu.finalize),
            slow_phase_time(phases.egress, None),
            slow_phase_time(phases.egress_enqueue, None),
            slow_phase_time(phases.egress_queue, None),
            slow_phase_time(phases.egress_prepare, phases.cpu.egress_prepare),
            slow_phase_time(phases.egress_write, None),
            phases.frame_bytes,
            phases.writer_queue_depth,
            phases.writer_active_at_enqueue,
            phases.writer_queue_was_full,
            phases.writer_reserve_timeouts,
            phases.waiting_on.as_str(),
            waiting_on_build_id,
            phases.wait_ms,
            detail,
            root.display(),
        );
        #[cfg(any(test, feature = "test-support"))]
        SLOW_TOOL_CALL_CAPTURE
            .with(|captured| captured.borrow_mut().push((level, message.clone())));
        log::log!(level, "{}{}", crate::log_ctx::session_prefix(), message);
    }
}

fn slow_phase_time(wall: Duration, cpu: Option<Duration>) -> String {
    // Fractional milliseconds keep microsecond translation work visible beside
    // scheduling delay. Unknown CPU is explicit, not a misleading zero.
    match cpu {
        Some(cpu) => format!(
            "{:.3}ms/cpu={:.3}ms",
            duration_millis_f64(wall),
            duration_millis_f64(cpu)
        ),
        None => format!("{:.3}ms/cpu=?", duration_millis_f64(wall)),
    }
}

/// Sample executor liveness and emit one busy-only aggregate at the configured cadence.
///
/// The transport may call this every loop turn; an atomic deadline keeps all
/// executor sampling and reporter locking off that path between drain ticks.
pub fn perf_tick<C: ExecutorContext>(executor: Option<&Executor<C>>) {
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

    let completion_counts = executor.map_or((0, 0), Executor::<C>::completion_counts);
    report_perf_tick(&PERF, sample, completion_counts);
}

fn report_perf_tick(
    perf: &PerfMetrics,
    sample: Option<ExecutorSample>,
    completion_counts: (u64, u64),
) {
    let tool_call_count = perf.tool_call_count.load(Ordering::Relaxed);
    let (completed_interactive, completed_maintenance, new_tool_calls) = {
        let Ok(mut reporter) = perf.reporter.lock() else {
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

    let watcher_ingested = perf.watcher_ingested.swap(0, Ordering::Relaxed);
    let watcher_paths = perf.watcher_paths.swap(0, Ordering::Relaxed);
    let watcher_dropped = perf.watcher_dropped.swap(0, Ordering::Relaxed);
    let drain_slices = perf.drain_slices.swap(0, Ordering::Relaxed);
    let semantic_collects = perf.semantic_collects.swap(0, Ordering::Relaxed);
    let semantic_files = perf.semantic_files.swap(0, Ordering::Relaxed);
    let semantic_chunks = perf.semantic_chunks.swap(0, Ordering::Relaxed);
    let semantic_ms = perf.semantic_ms.swap(0, Ordering::Relaxed);
    let callgraph_invalidations = perf.callgraph_invalidations.swap(0, Ordering::Relaxed);
    let file_lines_dropped = perf.file_lines_dropped.swap(0, Ordering::Relaxed);
    let tier2 = perf
        .tier2
        .lock()
        .map(|mut tier2| std::mem::take(&mut *tier2))
        .unwrap_or_default();
    let tool_calls = perf
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanReason {
    BufferOverflow,
    KernelDropped,
    UserDropped,
    Unknown,
}

impl RescanReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BufferOverflow => "buffer_overflow",
            Self::KernelDropped => "kernel_dropped",
            Self::UserDropped => "user_dropped",
            Self::Unknown => "unknown",
        }
    }
}

impl RescanReason {
    pub(crate) fn from_event_info(info: Option<&str>) -> Self {
        match info {
            Some("rescan: buffer overflow") => Self::BufferOverflow,
            Some("rescan: kernel dropped") => Self::KernelDropped,
            Some("rescan: user dropped") => Self::UserDropped,
            _ => Self::Unknown,
        }
    }
}
