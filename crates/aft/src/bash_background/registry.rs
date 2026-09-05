use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
#[cfg(not(windows))]
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::db::TrackedConnection;
use serde::Serialize;

use crate::bash_permissions::PermissionAsk;
use crate::compress::caps::DropClass;
#[cfg(unix)]
use crate::compress::single_top_level_pipeline;
use crate::compress::CompressionResult;
use crate::context::SharedProgressSender;
use crate::db::compression_events::CompressionAggregateCache;
use crate::harness::Harness;
use crate::protocol::{BashCompletedFrame, BashLongRunningFrame, BashPatternMatchFrame, PushFrame};
use crate::sandbox_spawn::SpawnPlan;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use super::buffer::{combine_streams, BgBuffer, DiskTruncation, StreamKind, TokenCountInput};
use super::output::{
    cap_completion_output, cap_completion_output_with_marker, cap_final_output,
    cap_final_output_with_marker, completion_preview_threshold, json_output_pointer, quote_path,
    retained_json_output_pointer, COMPRESS_INPUT_CAP_BYTES, COMPRESS_INPUT_HEAD_BYTES,
    COMPRESS_INPUT_TAIL_BYTES, FINAL_OUTPUT_CAP_BYTES, RAW_PASSTHROUGH_CAP_BYTES,
    RAW_PASSTHROUGH_HEAD_BYTES, RAW_PASSTHROUGH_TAIL_BYTES, RUNNING_OUTPUT_PREVIEW_BYTES,
    STRUCTURED_OUTPUT_CAP_BYTES,
};
use super::persistence::{
    allocate_task_layout, delete_resolved_task, delete_task_bundle, discover_task_ids,
    open_task_artifact, quarantine_invalid_entry, quarantine_task_layout, read_exit_marker,
    read_task_at, resolve_task_layout, session_tasks_dir, uninitialized_layout_is_recent,
    unix_millis, update_task_at, validate_task_id, write_kill_marker_if_absent, write_task_at,
    BgMode, ExitMarker, PersistedTask, TaskArtifact, TaskIoHandles, TaskPaths,
};
#[cfg(unix)]
use super::process::terminate_pgid;
#[cfg(windows)]
use super::process::terminate_pid;
use super::process::{is_process_alive, is_recorded_process_alive};
use super::pty_process::spawn_pty_for_command;
use super::pty_runtime::PtyRuntime;
use super::watches::{
    PatternMatch, WatchPattern, WatchRegistry, WATCH_TARGET_ERASED_CONTEXT,
    WATCH_TARGET_ERASED_TEXT,
};
use super::{BgTaskInfo, BgTaskStatus};
use crate::db::bash_tasks::BashTaskRow;
use crate::db::bash_watches::BashPatternWatchRow;
/// Default timeout for background bash tasks: 30 minutes.
/// Agents can override per-call via the `timeout` parameter (in ms).
const DEFAULT_BG_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PERSISTED_GC_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
const QUARANTINE_GC_GRACE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

const TOKENIZE_CAP_BYTES_PER_STREAM: usize = 128 * 1024;
pub const ROOT_RECLAIMED_REASON: &str = "root_reclaimed";

#[derive(Debug, Clone, Serialize)]
pub struct BgCompletion {
    pub task_id: String,
    /// Intentionally omitted from serialized completion payloads: push frames
    /// carry `session_id` at the BashCompletedFrame envelope level for routing.
    #[serde(skip_serializing)]
    pub session_id: String,
    pub status: BgTaskStatus,
    pub exit_code: Option<i32>,
    pub command: String,
    /// Small head+tail preview of the cached terminal render at completion time,
    /// cached so push-frame consumers and `bash_drain_completions` callers see
    /// the same preview without racing against later output rotation. Empty
    /// when not captured (e.g., persisted task seen on startup before buffer
    /// reattachment).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_preview: String,
    /// True when the captured tail is shorter than the actual output (because
    /// rotation occurred or the output exceeds the preview cap). Plugins use
    /// this to render a `…` prefix and signal that `bash_status` would return
    /// more.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_truncated: bool,
    /// Token count for raw stdout+stderr before compression. Omitted when any
    /// stream exceeds the 128 KiB tokenization cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_tokens: Option<u32>,
    /// Token count for the compressed output generated from the same capped
    /// raw payload. Omitted when raw tokenization is skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_tokens: Option<u32>,
    /// True when a stream exceeded the tokenization cap and counts are absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tokens_skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Serialize)]
pub struct BgTaskSnapshot {
    #[serde(flatten)]
    pub info: BgTaskInfo,
    pub exit_code: Option<i32>,
    pub child_pid: Option<u32>,
    pub workdir: String,
    pub output_preview: String,
    pub output_truncated: bool,
    pub output_path: Option<String>,
    pub stderr_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty_rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty_cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pty_screen: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scanner_report: Vec<PermissionAsk>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sandbox_native: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub sandbox_unavailable: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BgTaskHealthCounts {
    pub running: usize,
    pub pending_completions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalOutputKind {
    Compressed,
    Raw,
    Structured,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalOutputCache {
    output_preview: String,
    output_truncated: bool,
    kind: TerminalOutputKind,
    output_path: Option<String>,
    stderr_path: Option<String>,
    artifact_access: ArtifactRecoveryAccess,
    recovery: Option<RecoveryContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtifactRecoveryAccess {
    task_id: String,
    readable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryContext {
    dropped_by_class: BTreeMap<DropClass, usize>,
    had_inner_drop: bool,
    offset_hint_eligible: bool,
    offset_start_line: Option<usize>,
    byte_truncated: bool,
    disk_truncated_prefix_bytes: u64,
    output_path: Option<String>,
    stderr_path: Option<String>,
    include_stderr_path: bool,
    artifact_access: ArtifactRecoveryAccess,
}

fn optional_string_bytes(value: Option<&String>) -> u64 {
    value
        .map(|value| crate::memory::usize_to_u64(value.len()))
        .unwrap_or(0)
}

fn terminal_output_cache_estimated_bytes(cache: &TerminalOutputCache) -> u64 {
    let recovery_bytes = cache
        .recovery
        .as_ref()
        .map(|recovery| {
            crate::memory::usize_to_u64(recovery.dropped_by_class.len())
                .saturating_mul(
                    (std::mem::size_of::<DropClass>() + std::mem::size_of::<usize>()) as u64,
                )
                .saturating_add(optional_string_bytes(recovery.output_path.as_ref()))
                .saturating_add(optional_string_bytes(recovery.stderr_path.as_ref()))
                .saturating_add(crate::memory::usize_to_u64(
                    recovery.artifact_access.task_id.len(),
                ))
        })
        .unwrap_or(0);
    (std::mem::size_of::<TerminalOutputCache>() as u64)
        .saturating_add(crate::memory::usize_to_u64(cache.output_preview.len()))
        .saturating_add(optional_string_bytes(cache.output_path.as_ref()))
        .saturating_add(optional_string_bytes(cache.stderr_path.as_ref()))
        .saturating_add(crate::memory::usize_to_u64(
            cache.artifact_access.task_id.len(),
        ))
        .saturating_add(recovery_bytes)
}

fn completion_estimated_bytes(completion: &BgCompletion) -> u64 {
    (std::mem::size_of::<BgCompletion>() as u64)
        .saturating_add(crate::memory::usize_to_u64(completion.task_id.len()))
        .saturating_add(crate::memory::usize_to_u64(completion.session_id.len()))
        .saturating_add(crate::memory::usize_to_u64(completion.command.len()))
        .saturating_add(crate::memory::usize_to_u64(completion.output_preview.len()))
}

impl RecoveryContext {
    fn has_visible_drop(&self) -> bool {
        self.byte_truncated
            || self.disk_truncated_prefix_bytes > 0
            || self.had_inner_drop
            || !self.dropped_by_class.is_empty()
    }
}

#[derive(Clone)]
pub struct BgTaskRegistry {
    pub(crate) inner: Arc<RegistryInner>,
}

pub(crate) struct RegistryInner {
    pub(crate) tasks: Mutex<HashMap<String, Arc<BgTask>>>,
    pub(crate) completions: Mutex<VecDeque<BgCompletion>>,
    pub(crate) progress_sender: SharedProgressSender,
    watchdog_started: AtomicBool,
    pub(crate) shutdown: AtomicBool,
    pub(crate) long_running_reminder_enabled: AtomicBool,
    pub(crate) long_running_reminder_interval_ms: AtomicU64,
    persisted_gc_started: AtomicBool,
    #[cfg(test)]
    persisted_gc_runs: AtomicU64,
    /// Name of the thread the once-per-process persisted GC ran on. Lets the
    /// integration suite pin that the GC stays off the configure/replay thread
    /// (the replay caller is the standalone request loop).
    persisted_gc_thread: Mutex<Option<String>>,
    /// Output compression callback. Set by `AppContext` after construction.
    /// Takes (command, raw_output, exit_code) and returns compressed text. Called from
    /// the watchdog thread when a task reaches a terminal state and from
    /// `bash_status`/`list` snapshot reads. When `None`, output is returned
    /// uncompressed.
    pub(crate) compressor:
        Mutex<Option<Box<dyn Fn(&str, String, Option<i32>) -> CompressionResult + Send + Sync>>>,
    pub(crate) db_pool: RwLock<Option<Arc<Mutex<TrackedConnection>>>>,
    pub(crate) db_harness: RwLock<Option<String>>,
    pub(crate) compression_aggregates: Arc<CompressionAggregateCache>,
    pub(crate) wake_tx: crossbeam_channel::Sender<()>,
    pub(crate) wake_rx: crossbeam_channel::Receiver<()>,
    pub(crate) watch_registry: Mutex<WatchRegistry>,
    wait_detach_sessions: Mutex<HashSet<String>>,
    active_wait_sessions: Mutex<HashMap<String, usize>>,
    wait_registered_tasks: Mutex<HashMap<String, HashSet<String>>>,
}

pub(crate) struct BgTask {
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    delivery_session_id: String,
    pub(crate) paths: TaskPaths,
    artifact_root: PathBuf,
    pub(crate) started: Instant,
    pub(crate) last_reminder_at: Mutex<Option<Instant>>,
    pub(crate) terminal_at: Mutex<Option<Instant>>,
    pub(crate) state: Mutex<BgTaskState>,
}

pub(crate) enum TaskRuntime {
    Piped(Option<Child>),
    Pty(Option<PtyRuntime>),
}

pub(crate) struct BgTaskState {
    pub(crate) metadata: PersistedTask,
    pub(crate) runtime: TaskRuntime,
    /// Pinned task-io directory and original O_EXCL output handles retained for
    /// the process lifetime. Daemon writes never reopen child-writable names.
    pub(crate) io_handles: Option<TaskIoHandles>,
    pub(crate) detached: bool,
    /// True once `reap_child` has observed the direct child handle's exit
    /// via `try_wait()`. Used by the two-pass watchdog to skip the racy
    /// `is_process_alive(child_pid)` probe on the second pass — we already
    /// have authoritative evidence that the child is dead, no need to
    /// re-verify via PID liveness which is unreliable on Windows where
    /// PIDs can be recycled within seconds.
    ///
    /// Remains `false` on replay-restored tasks (those have a `child_pid`
    /// but never observed exit via this process's `try_wait()`), so those
    /// continue to fall through to the `is_process_alive` probe path.
    pub(crate) child_exit_observed: bool,
    pub(crate) buffer: BgBuffer,
    terminal_output_cache: Option<TerminalOutputCache>,
    /// PTY-only: set for timeout kill intent before signaling the child.
    pub(crate) pending_terminal_override: Option<BgTaskStatus>,
}

fn completion_matches_session(completion: &BgCompletion, session_id: Option<&str>) -> bool {
    session_id
        .map(|session_id| completion.session_id == session_id)
        .unwrap_or(true)
}

impl BgTaskRegistry {
    pub fn new(progress_sender: SharedProgressSender) -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        Self {
            inner: Arc::new(RegistryInner {
                tasks: Mutex::new(HashMap::new()),
                completions: Mutex::new(VecDeque::new()),
                progress_sender,
                watchdog_started: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                long_running_reminder_enabled: AtomicBool::new(true),
                long_running_reminder_interval_ms: AtomicU64::new(600_000),
                persisted_gc_started: AtomicBool::new(false),
                #[cfg(test)]
                persisted_gc_runs: AtomicU64::new(0),
                persisted_gc_thread: Mutex::new(None),
                compressor: Mutex::new(None),
                db_pool: RwLock::new(None),
                db_harness: RwLock::new(None),
                compression_aggregates: Arc::new(CompressionAggregateCache::default()),
                wake_tx,
                wake_rx,
                watch_registry: Mutex::new(WatchRegistry::default()),
                wait_detach_sessions: Mutex::new(HashSet::new()),
                active_wait_sessions: Mutex::new(HashMap::new()),
                wait_registered_tasks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Return whether `path` is an exact artifact registered to `session_id`.
    ///
    /// The requested path is canonicalized and compared with exact artifact
    /// names under the task directory identity captured at registration time.
    /// Deliberately do not grant access by `bash-tasks` directory prefix: a
    /// prefix exception would expose unrelated files and could be widened
    /// through symlinks or directory replacement.
    pub fn is_session_owned_artifact_path(&self, session_id: &str, path: &Path) -> bool {
        let Ok(requested) = fs::canonicalize(path) else {
            return false;
        };
        let Ok(tasks) = self.inner.tasks.lock() else {
            return false;
        };

        tasks.values().any(|task| {
            task.session_id == session_id
                && [
                    &task.paths.stdout,
                    &task.paths.stderr,
                    &task.paths.exit,
                    &task.paths.pty,
                ]
                .into_iter()
                .filter_map(|known| known.file_name())
                .any(|name| task.artifact_root.join(name) == requested)
        })
    }

    pub fn read_artifact_path(
        &self,
        session_id: &str,
        path: &Path,
    ) -> Option<Result<Vec<u8>, String>> {
        let requested = fs::canonicalize(path).ok()?;
        let tasks = self.inner.tasks.lock().ok()?;
        let (task, artifact) = tasks.values().find_map(|task| {
            if task.session_id != session_id {
                return None;
            }
            TaskArtifact::ALL.into_iter().find_map(|artifact| {
                let expected = task
                    .paths
                    .artifact_path(artifact)
                    .file_name()
                    .map(|name| task.artifact_root.join(name));
                (expected.as_deref() == Some(requested.as_path()))
                    .then(|| (Arc::clone(task), artifact))
            })
        })?;
        drop(tasks);
        Some(self.read_artifact(&task.task_id, session_id, artifact))
    }

    pub fn read_artifact_range(
        &self,
        task_id: &str,
        session_id: &str,
        artifact: TaskArtifact,
        offset: u64,
    ) -> Result<(Vec<u8>, u64), String> {
        validate_task_id(task_id).map_err(|error| error.to_string())?;
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or_else(|| "task_not_found".to_string())?;
        let mut file = open_task_artifact(&task.paths, artifact)
            .map_err(|error| format!("artifact_refused: {error}"))?;
        let len = file
            .len()
            .map_err(|error| format!("artifact_refused: {error}"))?;
        let start = offset.min(len);
        let bytes = file
            .read_range(start, len.saturating_sub(start))
            .map_err(|error| format!("artifact_refused: {error}"))?;
        Ok((bytes, len))
    }

    pub fn read_artifact(
        &self,
        task_id: &str,
        session_id: &str,
        artifact: TaskArtifact,
    ) -> Result<Vec<u8>, String> {
        validate_task_id(task_id).map_err(|error| error.to_string())?;
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or_else(|| "task_not_found".to_string())?;
        let mut file = open_task_artifact(&task.paths, artifact)
            .map_err(|error| format!("artifact_refused: {error}"))?;
        file.read_all()
            .map_err(|error| format!("artifact_refused: {error}"))
    }

    pub fn set_harness(&self, harness: Harness) {
        if let Ok(mut slot) = self.inner.db_harness.write() {
            *slot = Some(harness.storage_segment());
        }
    }

    pub fn set_db_pool(&self, conn: Arc<Mutex<TrackedConnection>>) {
        if let Ok(mut slot) = self.inner.db_pool.write() {
            *slot = Some(conn);
        }
        self.inner.compression_aggregates.clear();
    }

    pub fn clear_db_pool(&self) {
        if let Ok(mut slot) = self.inner.db_pool.write() {
            *slot = None;
        }
        self.inner.compression_aggregates.clear();
    }

    pub(crate) fn compression_aggregate_cache(&self) -> Arc<CompressionAggregateCache> {
        Arc::clone(&self.inner.compression_aggregates)
    }

    pub fn register_foreground_task(&self, session_id: &str, task_id: &str) {
        if let Ok(mut tasks) = self.inner.wait_registered_tasks.lock() {
            tasks
                .entry(session_id.to_string())
                .or_default()
                .insert(task_id.to_string());
        }
    }

    pub fn begin_wait_mode_session(&self, session_id: &str, task_id: &str) {
        if let Ok(mut active) = self.inner.active_wait_sessions.lock() {
            *active.entry(session_id.to_string()).or_insert(0) += 1;
        }
        self.register_foreground_task(session_id, task_id);
        if let Ok(mut detach) = self.inner.wait_detach_sessions.lock() {
            detach.remove(session_id);
        }
    }

    pub fn unregister_foreground_task(&self, session_id: &str, task_id: &str) {
        if let Ok(mut tasks) = self.inner.wait_registered_tasks.lock() {
            if let Some(session_tasks) = tasks.get_mut(session_id) {
                session_tasks.remove(task_id);
                if session_tasks.is_empty() {
                    tasks.remove(session_id);
                }
            }
        }
    }

    pub fn end_wait_mode_session(&self, session_id: &str, task_id: &str) {
        let no_active_wait = if let Ok(mut active) = self.inner.active_wait_sessions.lock() {
            match active.get_mut(session_id) {
                Some(count) if *count > 1 => *count -= 1,
                Some(_) => {
                    active.remove(session_id);
                }
                None => {}
            }
            !active.contains_key(session_id)
        } else {
            false
        };
        self.unregister_foreground_task(session_id, task_id);
        if no_active_wait {
            if let Ok(mut detach) = self.inner.wait_detach_sessions.lock() {
                detach.remove(session_id);
            }
        }
    }

    /// Kill the foreground bash task(s) still registered for an in-flight
    /// call. Explicit background and PTY tasks are never registered
    /// here, so an abort cannot affect those deliberately detached tasks.
    pub fn abort_inflight(&self, session_id: &str) -> Result<usize, String> {
        let task_ids = self
            .inner
            .wait_registered_tasks
            .lock()
            .map(|mut tasks| tasks.remove(session_id).unwrap_or_default())
            .map_err(|_| "wait registration lock poisoned".to_string())?;
        if let Ok(mut active) = self.inner.active_wait_sessions.lock() {
            active.remove(session_id);
        }
        if let Ok(mut detach) = self.inner.wait_detach_sessions.lock() {
            detach.remove(session_id);
        }

        let mut killed = 0;
        for task_id in task_ids {
            let Some(task) = self.task_for_session(&task_id, session_id) else {
                continue;
            };
            let is_terminal = task
                .state
                .lock()
                .map(|state| state.metadata.status.is_terminal())
                .map_err(|_| "background task lock poisoned".to_string())?;
            if is_terminal {
                continue;
            }
            let snapshot = self.kill_with_status_reason(
                &task_id,
                session_id,
                BgTaskStatus::Killed,
                Some("call_aborted".to_string()),
            )?;
            if snapshot.info.status == BgTaskStatus::Killed
                && snapshot.info.status_reason.as_deref() == Some("call_aborted")
            {
                killed += 1;
            }
        }
        Ok(killed)
    }

    pub fn signal_wait_mode_detach(&self, session_id: &str) -> bool {
        let is_waiting = self
            .inner
            .active_wait_sessions
            .lock()
            .map(|active| active.get(session_id).copied().unwrap_or(0) > 0)
            .unwrap_or(false);
        if !is_waiting {
            return false;
        }
        self.inner
            .wait_detach_sessions
            .lock()
            .map(|mut detach| detach.insert(session_id.to_string()))
            .unwrap_or(false)
    }

    /// Number of sessions currently blocked in a `wait: true` foreground bash.
    /// Diagnostic only (the detach-signal trace log).
    pub fn active_wait_session_count(&self) -> usize {
        self.inner
            .active_wait_sessions
            .lock()
            .map(|active| active.len())
            .unwrap_or(0)
    }

    pub fn take_wait_mode_detach(&self, session_id: &str) -> bool {
        self.inner
            .wait_detach_sessions
            .lock()
            .map(|mut detach| detach.remove(session_id))
            .unwrap_or(false)
    }

    /// Install the output-compression callback. Called by `main.rs` after
    /// `AppContext` is constructed so that snapshot/completion paths can
    /// invoke `compress::compress_with_registry` without holding a context
    /// reference. When called multiple times, the latest installation wins.
    pub fn set_compressor<F>(&self, compressor: F)
    where
        F: Fn(&str, String) -> CompressionResult + Send + Sync + 'static,
    {
        self.set_compressor_with_exit_code(move |command, output, _exit_code| {
            compressor(command, output)
        });
    }

    pub fn set_compressor_with_exit_code<F>(&self, compressor: F)
    where
        F: Fn(&str, String, Option<i32>) -> CompressionResult + Send + Sync + 'static,
    {
        if let Ok(mut slot) = self.inner.compressor.lock() {
            *slot = Some(Box::new(compressor));
        }
    }

    /// Apply the installed compressor (if any) to `output`. Returns `output`
    /// untouched when no compressor is installed.
    pub(crate) fn compress_output(
        &self,
        command: &str,
        output: String,
        exit_code: Option<i32>,
    ) -> CompressionResult {
        let Ok(slot) = self.inner.compressor.lock() else {
            return CompressionResult::new(output);
        };
        match slot.as_ref() {
            Some(compressor) => compressor(command, output, exit_code),
            None => CompressionResult::new(output),
        }
    }

    fn ensure_terminal_output_cache(&self, task: &Arc<BgTask>) -> Option<TerminalOutputCache> {
        let (metadata, buffer) = {
            let state = task.state.lock().ok()?;
            if !state.metadata.status.is_terminal() || state.metadata.mode == BgMode::Pty {
                return None;
            }
            if let Some(cache) = state.terminal_output_cache.clone() {
                return Some(cache);
            }
            (state.metadata.clone(), state.buffer.clone())
        };

        let mut cap_buffer = buffer.clone();
        let disk_truncation = cap_buffer.enforce_terminal_cap();
        let cache =
            self.render_terminal_output(&metadata, &cap_buffer, disk_truncation, Some(&task.paths));
        let mut state = task.state.lock().ok()?;
        if !state.metadata.status.is_terminal() || state.metadata.mode == BgMode::Pty {
            return None;
        }
        if let Some(existing) = state.terminal_output_cache.clone() {
            return Some(existing);
        }
        state.terminal_output_cache = Some(cache.clone());
        Some(cache)
    }

    fn render_terminal_output(
        &self,
        metadata: &PersistedTask,
        buffer: &BgBuffer,
        disk_truncation: DiskTruncation,
        paths: Option<&TaskPaths>,
    ) -> TerminalOutputCache {
        let output_readable = buffer
            .output_path()
            .is_some_and(|path| self.is_session_owned_artifact_path(&metadata.session_id, &path));
        let stderr_readable = buffer
            .stderr_path()
            .map(|path| self.is_session_owned_artifact_path(&metadata.session_id, path))
            .unwrap_or(true);
        let artifact_access = ArtifactRecoveryAccess {
            task_id: metadata.task_id.clone(),
            readable: output_readable && stderr_readable,
        };

        if metadata.mode == BgMode::Pty {
            return TerminalOutputCache {
                output_preview: String::new(),
                output_truncated: false,
                kind: TerminalOutputKind::Raw,
                output_path: buffer.output_path().map(|path| path.display().to_string()),
                stderr_path: buffer.stderr_path().map(|path| path.display().to_string()),
                artifact_access,
                recovery: None,
            };
        }

        let mut rendered = if let Some(structured) = render_structured_output(
            &metadata.command,
            buffer,
            disk_truncation,
            artifact_access.clone(),
        ) {
            structured
        } else if !metadata.compressed {
            render_raw_passthrough(buffer, disk_truncation, artifact_access)
        } else {
            let raw = buffer.read_combined_head_tail(
                COMPRESS_INPUT_CAP_BYTES,
                COMPRESS_INPUT_HEAD_BYTES,
                COMPRESS_INPUT_TAIL_BYTES,
            );
            let compressed = self.compress_output(&metadata.command, raw.text, metadata.exit_code);
            render_compressed_with_recovery(
                buffer,
                compressed,
                raw.truncated,
                disk_truncation,
                artifact_access,
            )
        };
        normalize_piped_display_output(&mut rendered.output_preview);
        append_pipeline_warning(&mut rendered, metadata, paths);
        rendered
    }

    fn snapshot_with_terminal_cache(
        &self,
        task: &Arc<BgTask>,
        preview_bytes: usize,
    ) -> BgTaskSnapshot {
        let mut snapshot = task.snapshot(preview_bytes);
        self.maybe_compress_snapshot(task, &mut snapshot);
        snapshot
    }

    fn post_terminal_transition(&self, task: &Arc<BgTask>, emit_frame: bool) -> Result<(), String> {
        let (metadata, buffer) = {
            let state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            if !state.metadata.status.is_terminal() {
                return Ok(());
            }
            (state.metadata.clone(), state.buffer.clone())
        };

        let cache = self.ensure_terminal_output_cache(task);
        self.enqueue_completion_from_parts(
            &metadata,
            Some(&buffer),
            None,
            emit_frame,
            cache.as_ref(),
        );
        self.retarget_pending_completion(&metadata.task_id, &task.delivery_session_id);
        Ok(())
    }

    fn persist_task(&self, paths: &TaskPaths, metadata: &PersistedTask) -> std::io::Result<()> {
        let task = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
        write_task_at(&task, metadata)?;
        self.dual_write_task(paths, metadata);
        Ok(())
    }

    fn update_task_metadata<F>(
        &self,
        paths: &TaskPaths,
        update: F,
    ) -> std::io::Result<PersistedTask>
    where
        F: FnOnce(&mut PersistedTask),
    {
        let task = resolve_task_layout(&paths.session_dir, &paths.task_id)?;
        let metadata = update_task_at(&task, update)?;
        self.dual_write_task(paths, &metadata);
        Ok(metadata)
    }

    fn dual_write_task(&self, paths: &TaskPaths, metadata: &PersistedTask) {
        let pool = self.inner.db_pool.read().ok().and_then(|slot| slot.clone());
        let Some(pool) = pool else {
            return;
        };
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(harness) = harness else {
            crate::slog_warn!(
                "dual-write bash_task to DB skipped for {}: harness not configured",
                metadata.task_id
            );
            return;
        };
        let row = match metadata.to_bash_task_row(&harness, paths) {
            Ok(row) => row,
            Err(error) => {
                crate::slog_warn!(
                    "dual-write bash_task to DB failed for {}: {}",
                    metadata.task_id,
                    error
                );
                return;
            }
        };
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => {
                crate::slog_warn!(
                    "dual-write bash_task to DB failed for {}: db mutex poisoned",
                    metadata.task_id
                );
                return;
            }
        };
        if let Err(error) = crate::db::bash_tasks::upsert_bash_task(&conn, &row) {
            crate::slog_warn!(
                "dual-write bash_task to DB failed for {}: {}",
                metadata.task_id,
                error
            );
        }
    }

    fn delete_gc_task_from_db(&self, metadata: &PersistedTask) {
        let pool = self.inner.db_pool.read().ok().and_then(|slot| slot.clone());
        let Some(pool) = pool else {
            return;
        };
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(harness) = harness else {
            crate::slog_warn!(
                "GC bash_task DB delete skipped for {}: harness not configured",
                metadata.task_id
            );
            return;
        };
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => {
                crate::slog_warn!(
                    "GC bash_task DB delete failed for {}: db mutex poisoned",
                    metadata.task_id
                );
                return;
            }
        };
        if let Err(error) = crate::db::bash_tasks::delete_delivered_terminal_bash_task(
            &conn,
            &harness,
            &metadata.session_id,
            &metadata.task_id,
            "persisted_gc_delivered_terminal",
        ) {
            crate::slog_warn!(
                "GC bash_task DB delete failed for {}: {}",
                metadata.task_id,
                error
            );
        }
    }

    fn persisted_task_process_is_alive(metadata: &PersistedTask) -> bool {
        let child_pid = metadata.child_pid;
        let group_leader = metadata.pgid.and_then(|pid| u32::try_from(pid).ok());
        child_pid
            .into_iter()
            .chain(group_leader)
            .any(|pid| is_recorded_process_alive(pid, metadata.started_at))
    }

    fn db_has_live_process_for_task(&self, task_id: &str) -> bool {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return false;
        };
        let Ok(conn) = pool.lock() else {
            return false;
        };
        crate::db::bash_tasks::list_bash_tasks_by_id(&conn, &harness, task_id)
            .map(|rows| {
                rows.into_iter().any(|row| {
                    let started_at = u64::try_from(row.started_at).unwrap_or_default();
                    row.pid
                        .and_then(|pid| u32::try_from(pid).ok())
                        .into_iter()
                        .chain(row.pgid.and_then(|pid| u32::try_from(pid).ok()))
                        .any(|pid| is_recorded_process_alive(pid, started_at))
                })
            })
            .unwrap_or(false)
    }

    fn db_harness_and_pool(&self) -> Option<(String, Arc<Mutex<TrackedConnection>>)> {
        let pool = self
            .inner
            .db_pool
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        Some((harness, pool))
    }

    fn redeliver_pending_watches_for_session(&self, session_id: &str) -> usize {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return 0;
        };
        let rows = {
            let Ok(conn) = pool.lock() else {
                return 0;
            };
            match crate::db::bash_watches::list_bash_pattern_watches_for_session(
                &conn, &harness, session_id,
            ) {
                Ok(rows) => rows,
                Err(error) => {
                    crate::slog_warn!(
                        "failed to load pending bash watches for session {session_id}: {error}"
                    );
                    return 0;
                }
            }
        };
        let mut delivered = 0;
        for row in rows.into_iter().filter(|row| row.pending_match) {
            let Some(match_text) = row.match_text else {
                crate::slog_warn!(
                    "pending bash watch {}/{} has no match text",
                    row.task_id,
                    row.watch_id
                );
                continue;
            };
            let context = row.match_context.unwrap_or_else(|| match_text.clone());
            if match_text == WATCH_TARGET_ERASED_TEXT {
                self.emit_bash_watch_erased(session_id, &row.task_id, &row.watch_id);
            } else {
                self.emit_bash_pattern_match(
                    session_id,
                    PatternMatch {
                        watch_id: row.watch_id,
                        task_id: row.task_id,
                        match_text,
                        match_offset: row.match_offset.unwrap_or_default().max(0) as u64,
                        context,
                        once: row.once,
                    },
                );
            }
            delivered += 1;
        }
        delivered
    }

    fn terminal_db_status_for_session(
        &self,
        session_id: &str,
        task_id: &str,
        storage_dir: &Path,
    ) -> Option<BgTaskSnapshot> {
        let (harness, pool) = self.db_harness_and_pool()?;
        let conn = pool.lock().ok()?;
        let row =
            crate::db::bash_tasks::get_bash_task(&conn, &harness, session_id, task_id).ok()??;
        if !task_bundle_is_absent(storage_dir, &row.session_id, &row.task_id) {
            return None;
        }
        let metadata = PersistedTask::from(row.clone());
        metadata
            .status
            .is_terminal()
            .then(|| terminal_db_row_snapshot(row, metadata))
    }

    pub fn has_erased_watch_reference(&self, task_id: &str) -> bool {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return false;
        };
        let Ok(conn) = pool.lock() else {
            return false;
        };
        let watched =
            crate::db::bash_watches::list_bash_pattern_watches_by_task_id(&conn, &harness, task_id)
                .map(|rows| !rows.is_empty())
                .unwrap_or(false);
        if !watched {
            return false;
        }
        crate::db::bash_tasks::list_bash_tasks_by_id(&conn, &harness, task_id)
            .map(|rows| rows.is_empty())
            .unwrap_or(false)
    }

    pub(crate) fn evaluate_erased_watch_targets(&self) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let notifications = {
            let Ok(conn) = pool.lock() else {
                return;
            };
            let rows = match crate::db::bash_watches::list_bash_pattern_watches(&conn, &harness) {
                Ok(rows) => rows,
                Err(error) => {
                    crate::slog_warn!("failed to inspect bash watch targets: {error}");
                    return;
                }
            };
            let mut notifications = Vec::new();
            for mut row in rows {
                let task_exists = match crate::db::bash_tasks::get_bash_task(
                    &conn,
                    &harness,
                    &row.session_id,
                    &row.task_id,
                ) {
                    Ok(task) => task.is_some(),
                    Err(error) => {
                        crate::slog_warn!(
                            "failed to inspect bash watch target {}: {error}",
                            row.task_id
                        );
                        continue;
                    }
                };
                if task_exists {
                    continue;
                }
                let already_tombstoned = !row.scanning
                    && row.pending_match
                    && row.match_text.as_deref() == Some(WATCH_TARGET_ERASED_TEXT);
                if !already_tombstoned {
                    row.scanning = false;
                    row.pending_match = true;
                    row.match_text = Some(WATCH_TARGET_ERASED_TEXT.to_string());
                    row.match_offset = Some(0);
                    row.match_context = Some(WATCH_TARGET_ERASED_CONTEXT.to_string());
                    if let Err(error) =
                        crate::db::bash_watches::upsert_bash_pattern_watch(&conn, &row)
                    {
                        crate::slog_warn!(
                            "failed to terminalize erased bash watch {}/{}: {error}",
                            row.task_id,
                            row.watch_id
                        );
                        continue;
                    }
                }
                notifications.push((row.session_id, row.task_id, row.watch_id));
            }
            notifications
        };

        for (session_id, task_id, watch_id) in notifications {
            let should_emit = self
                .inner
                .watch_registry
                .lock()
                .map(|mut registry| registry.terminalize_erased_task(&task_id, &watch_id))
                .unwrap_or(false);
            if should_emit {
                self.emit_bash_watch_erased(&session_id, &task_id, &watch_id);
            }
        }
    }

    fn persist_watch_registration(
        &self,
        session_id: &str,
        task_id: &str,
        watch_id: &str,
        pattern: &WatchPattern,
        once: bool,
        stdout_offset: u64,
        stderr_offset: u64,
        pty_offset: u64,
    ) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        let row = BashPatternWatchRow {
            harness,
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            watch_id: watch_id.to_string(),
            pattern_kind: pattern.kind_name().to_string(),
            pattern: pattern.pattern_text().to_string(),
            once,
            created_at: unix_millis() as i64,
            stdout_offset: stdout_offset as i64,
            stderr_offset: stderr_offset as i64,
            pty_offset: pty_offset as i64,
            scanning: true,
            pending_match: false,
            match_text: None,
            match_offset: None,
            match_context: None,
        };
        if let Err(error) = crate::db::bash_watches::upsert_bash_pattern_watch(&conn, &row) {
            crate::slog_warn!(
                "persist bash_pattern_watch failed for {task_id}/{watch_id}: {error}"
            );
        }
    }

    fn delete_persisted_watch(&self, session_id: &str, task_id: &str, watch_id: &str) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        if let Err(error) = crate::db::bash_watches::delete_bash_pattern_watch(
            &conn, &harness, session_id, task_id, watch_id,
        ) {
            crate::slog_warn!("delete bash_pattern_watch failed for {task_id}/{watch_id}: {error}");
        }
    }

    fn delete_persisted_watches_for_task(&self, session_id: &str, task_id: &str) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        if let Err(error) = crate::db::bash_watches::delete_bash_pattern_watches_for_task(
            &conn, &harness, session_id, task_id,
        ) {
            crate::slog_warn!("delete bash_pattern_watches for {task_id} failed: {error}");
        }
    }

    fn persist_watch_match(
        &self,
        session_id: &str,
        task_id: &str,
        pattern_match: &PatternMatch,
        stdout_offset: u64,
        stderr_offset: u64,
        pty_offset: u64,
    ) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        let Ok(Some(mut row)) = crate::db::bash_watches::get_bash_pattern_watch(
            &conn,
            &harness,
            session_id,
            task_id,
            &pattern_match.watch_id,
        ) else {
            return;
        };
        row.stdout_offset = stdout_offset as i64;
        row.stderr_offset = stderr_offset as i64;
        row.pty_offset = pty_offset as i64;
        row.pending_match = true;
        row.match_text = Some(pattern_match.match_text.clone());
        row.match_offset = Some(pattern_match.match_offset as i64);
        row.match_context = Some(pattern_match.context.clone());
        if pattern_match.once {
            // Once-watches stop scanning after the first hit but stay durable
            // until ack so a lost push can be re-delivered after restart.
            row.scanning = false;
        }
        if let Err(error) = crate::db::bash_watches::upsert_bash_pattern_watch(&conn, &row) {
            crate::slog_warn!(
                "persist bash_pattern_watch match failed for {}/{}: {error}",
                task_id,
                pattern_match.watch_id
            );
        }
    }

    fn persist_task_watch_cursors(
        &self,
        session_id: &str,
        task_id: &str,
        stdout_offset: u64,
        stderr_offset: u64,
        pty_offset: u64,
    ) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        if let Err(error) = crate::db::bash_watches::update_watch_offsets_for_task(
            &conn,
            &harness,
            session_id,
            task_id,
            stdout_offset as i64,
            stderr_offset as i64,
            pty_offset as i64,
        ) {
            crate::slog_warn!("persist bash_pattern_watch cursors failed for {task_id}: {error}");
        }
    }

    fn watch_stream_cursors(&self, task_id: &str) -> (u64, u64, u64) {
        let Ok(registry) = self.inner.watch_registry.lock() else {
            return (0, 0, 0);
        };
        let stdout = registry
            .file_cursor(&format!("{task_id}:stdout"))
            .unwrap_or(0);
        let stderr = registry
            .file_cursor(&format!("{task_id}:stderr"))
            .unwrap_or(0);
        let pty = registry.file_cursor(&format!("{task_id}:pty")).unwrap_or(0);
        (stdout, stderr, pty)
    }

    /// Ack path for pattern watches: once-watches (and any terminal-task watches)
    /// are dropped after delivery is confirmed; sticky watches clear pending only.
    fn ack_persisted_watches_for_task(&self, session_id: &str, task_id: &str, task_terminal: bool) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        if task_terminal {
            let _ = crate::db::bash_watches::delete_bash_pattern_watches_for_task(
                &conn, &harness, session_id, task_id,
            );
            return;
        }
        let Ok(rows) = crate::db::bash_watches::list_bash_pattern_watches_for_task(
            &conn, &harness, session_id, task_id,
        ) else {
            return;
        };
        for mut row in rows {
            if row.once && (!row.scanning || row.pending_match) {
                let _ = crate::db::bash_watches::delete_bash_pattern_watch(
                    &conn,
                    &harness,
                    session_id,
                    task_id,
                    &row.watch_id,
                );
                continue;
            }
            if row.pending_match {
                row.pending_match = false;
                row.match_text = None;
                row.match_offset = None;
                row.match_context = None;
                let _ = crate::db::bash_watches::upsert_bash_pattern_watch(&conn, &row);
            }
        }
    }

    pub fn record_scanner_report(
        &self,
        task_id: &str,
        session_id: &str,
        scanner_report: Vec<PermissionAsk>,
    ) -> Result<(), String> {
        if scanner_report.is_empty() {
            return Ok(());
        }
        let task = self.task_for_session(task_id, session_id).ok_or_else(|| {
            "background task not found while recording scanner report".to_string()
        })?;
        let metadata = {
            let mut state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            state.metadata.scanner_report = scanner_report;
            state.metadata.clone()
        };
        self.persist_task(&task.paths, &metadata)
            .map_err(|error| format!("failed to persist scanner report: {error}"))
    }

    pub fn configure_long_running_reminders(&self, enabled: bool, interval_ms: u64) {
        self.inner
            .long_running_reminder_enabled
            .store(enabled, Ordering::SeqCst);
        self.inner
            .long_running_reminder_interval_ms
            .store(interval_ms, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
    ) -> Result<String, String> {
        self.spawn_with_shell(
            spawn_plan,
            command,
            super::BashShell::Bash,
            resolve_posix_shell(),
            session_id,
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
        )
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_shell(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        shell: super::BashShell,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
    ) -> Result<String, String> {
        self.start_watchdog();

        let running = self.running_count();
        if running >= max_running {
            #[cfg(unix)]
            if let Some(prepared) = spawn_plan.prepared_task() {
                let _ = delete_resolved_task(&prepared.resolved_task());
            }
            return Err(format!(
                "background bash task limit exceeded: {running} running (max {max_running})"
            ));
        }

        let timeout = timeout.or(Some(DEFAULT_BG_TIMEOUT));
        let timeout_ms = timeout.map(|timeout| timeout.as_millis() as u64);
        let (spawn_plan, task_layout) = if let Some(prepared) = spawn_plan.prepared_task() {
            (spawn_plan.clone(), prepared.resolved_task())
        } else {
            let task = allocate_task_layout(&storage_dir, &session_id)
                .map_err(|error| format!("failed to create background task layout: {error}"))?;
            let root = project_root.as_deref().unwrap_or(&workdir);
            let environment =
                crate::sandbox_spawn::approved_payload_environment(&env, &std::env::temp_dir());
            let prepared = match crate::sandbox_spawn::prepare_task_payload(
                &task,
                command.as_bytes(),
                root,
                &workdir,
                &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                &shell_path,
                &environment,
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = delete_resolved_task(&task);
                    return Err(error);
                }
            };
            let task = prepared.resolved_task();
            (spawn_plan.with_prepared_task(prepared), task)
        };
        let task_id = task_layout.paths.task_id.clone();
        let paths = task_layout.paths.clone();

        if self.task(&task_id).is_some() {
            let _ = delete_resolved_task(&task_layout);
            return Err("background task id collided with a live task".to_string());
        }

        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            session_id.clone(),
            command.to_string(),
            workdir.clone(),
            project_root,
            timeout_ms,
            notify_on_completion,
            compressed,
        );
        // Pipeline-status capture is a Unix-only mechanism: the wrapper needs
        // bash/zsh PIPESTATUS and a dedicated inherited fd, neither of which
        // exists on the Windows spawn path.
        #[cfg(unix)]
        let capture_pipeline_status = {
            let pipeline = single_top_level_pipeline(command);
            let capture = !shell.is_powershell()
                && should_capture_pipeline_status(&spawn_plan, pipeline.is_some(), &shell_path);
            if capture {
                metadata.pipeline_segments = pipeline
                    .as_ref()
                    .map(|pipeline| {
                        pipeline
                            .segments
                            .iter()
                            .map(|segment| segment.label.clone())
                            .collect()
                    })
                    .unwrap_or_default();
            }
            capture
        };
        #[cfg(windows)]
        let capture_pipeline_status = false;
        attach_sandbox_metadata(&mut metadata, &spawn_plan);
        if let Err(error) = write_task_at(&task_layout, &metadata) {
            let _ = delete_resolved_task(&task_layout);
            return Err(format!(
                "failed to persist background task metadata: {error}"
            ));
        }
        self.dual_write_task(&paths, &metadata);

        let mut io_handles =
            TaskIoHandles::create(&task_layout, BgMode::Pipes, capture_pipeline_status)
                .map_err(|error| format!("failed to pre-open task output handles: {error}"))?;
        let child = match spawn_detached_child(
            &spawn_plan,
            command,
            shell,
            &shell_path,
            &paths,
            &workdir,
            &env,
            &mut io_handles,
            capture_pipeline_status,
        ) {
            Ok(child) => child,
            Err(error) => {
                crate::slog_warn!("failed to spawn background bash task {task_id}; deleting partial bundle: {error}");
                let _ = delete_task_bundle(&paths);
                return Err(error);
            }
        };

        let child_pid = child.id();
        metadata.mark_running(child_pid, child_pid as i32);
        self.persist_task(&paths, &metadata)
            .map_err(|e| format!("failed to persist running background task metadata: {e}"))?;

        let task = Arc::new(BgTask {
            task_id: task_id.clone(),
            delivery_session_id: session_id.clone(),
            session_id,
            paths: paths.clone(),
            artifact_root: canonical_artifact_root(&paths),
            started: Instant::now(),
            last_reminder_at: Mutex::new(None),
            terminal_at: Mutex::new(None),
            state: Mutex::new(BgTaskState {
                metadata,
                runtime: TaskRuntime::Piped(Some(child)),
                io_handles: Some(io_handles),
                detached: false,
                child_exit_observed: false,
                buffer: BgBuffer::registered(&paths, BgMode::Pipes),
                terminal_output_cache: None,
                pending_terminal_override: None,
            }),
        });

        self.inner
            .tasks
            .lock()
            .map_err(|_| "background task registry lock poisoned".to_string())?
            .insert(task_id.clone(), task);

        Ok(task_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_pty(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
        rows: u16,
        cols: u16,
    ) -> Result<String, String> {
        self.spawn_pty_with_shell(
            spawn_plan,
            command,
            super::BashShell::Bash,
            super::resolve_shell_path(true, super::BashShell::Bash)
                .expect("POSIX shell must resolve for bash PTY"),
            session_id,
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
            rows,
            cols,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_pty_with_shell(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        shell: super::BashShell,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
        rows: u16,
        cols: u16,
    ) -> Result<String, String> {
        self.start_watchdog();

        let running = self.running_count();
        if running >= max_running {
            #[cfg(unix)]
            if let Some(prepared) = spawn_plan.prepared_task() {
                let _ = delete_resolved_task(&prepared.resolved_task());
            }
            return Err(format!(
                "background bash task limit exceeded: {running} running (max {max_running})"
            ));
        }

        let timeout = timeout.or(Some(DEFAULT_BG_TIMEOUT));
        let timeout_ms = timeout.map(|timeout| timeout.as_millis() as u64);
        #[cfg(unix)]
        let (spawn_plan, task_layout) = if let Some(prepared) = spawn_plan.prepared_task() {
            (spawn_plan.clone(), prepared.resolved_task())
        } else {
            let task = allocate_task_layout(&storage_dir, &session_id)
                .map_err(|error| format!("failed to create PTY task layout: {error}"))?;
            let root = project_root.as_deref().unwrap_or(&workdir);
            let environment =
                crate::sandbox_spawn::approved_payload_environment(&env, &std::env::temp_dir());
            let prepared = match crate::sandbox_spawn::prepare_task_payload(
                &task,
                command.as_bytes(),
                root,
                &workdir,
                &crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty,
                &shell_path,
                &environment,
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = delete_resolved_task(&task);
                    return Err(error);
                }
            };
            let task = prepared.resolved_task();
            (spawn_plan.with_prepared_task(prepared), task)
        };
        #[cfg(windows)]
        let task_layout = allocate_task_layout(&storage_dir, &session_id)
            .map_err(|error| format!("failed to create PTY task layout: {error}"))?;
        let task_id = task_layout.paths.task_id.clone();
        let paths = task_layout.paths.clone();

        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            session_id.clone(),
            command.to_string(),
            workdir.clone(),
            project_root,
            timeout_ms,
            notify_on_completion,
            compressed,
        );
        attach_sandbox_metadata(&mut metadata, &spawn_plan);
        metadata.mode = BgMode::Pty;
        metadata.pty_rows = Some(rows);
        metadata.pty_cols = Some(cols);
        if let Err(error) = write_task_at(&task_layout, &metadata) {
            let _ = delete_resolved_task(&task_layout);
            return Err(format!(
                "failed to persist background task metadata: {error}"
            ));
        }
        self.dual_write_task(&paths, &metadata);
        let mut io_handles = TaskIoHandles::create(&task_layout, BgMode::Pty, false)
            .map_err(|error| format!("failed to pre-open PTY output handles: {error}"))?;

        let runtime = match spawn_pty_for_command(
            &spawn_plan,
            &task_id,
            &session_id,
            command,
            shell,
            &shell_path,
            &paths,
            &workdir,
            &env,
            rows,
            cols,
            self.inner.wake_tx.clone(),
            &mut io_handles,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                crate::slog_warn!(
                    "failed to spawn PTY background bash task {task_id}; deleting partial bundle: {error}"
                );
                let _ = delete_task_bundle(&paths);
                return Err(error);
            }
        };

        if let Some(child_pid) = runtime.child_pid {
            metadata.mark_running(child_pid, child_pid as i32);
        } else {
            metadata.status = BgTaskStatus::Running;
            metadata.pgid = None;
        }
        self.persist_task(&paths, &metadata)
            .map_err(|e| format!("failed to persist running background task metadata: {e}"))?;

        let task = Arc::new(BgTask {
            task_id: task_id.clone(),
            delivery_session_id: session_id.clone(),
            session_id,
            paths: paths.clone(),
            artifact_root: canonical_artifact_root(&paths),
            started: Instant::now(),
            last_reminder_at: Mutex::new(None),
            terminal_at: Mutex::new(None),
            state: Mutex::new(BgTaskState {
                metadata,
                runtime: TaskRuntime::Pty(Some(runtime)),
                io_handles: Some(io_handles),
                detached: false,
                child_exit_observed: false,
                buffer: BgBuffer::registered(&paths, BgMode::Pty),
                terminal_output_cache: None,
                pending_terminal_override: None,
            }),
        });

        self.inner
            .tasks
            .lock()
            .map_err(|_| "background task registry lock poisoned".to_string())?
            .insert(task_id.clone(), task);

        Ok(task_id)
    }

    #[cfg(windows)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
    ) -> Result<String, String> {
        self.spawn_with_shell(
            spawn_plan,
            command,
            super::BashShell::Bash,
            PathBuf::from("cmd.exe"),
            session_id,
            workdir,
            env,
            timeout,
            storage_dir,
            max_running,
            notify_on_completion,
            compressed,
            project_root,
        )
    }

    #[cfg(windows)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_shell(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        shell: super::BashShell,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        timeout: Option<Duration>,
        storage_dir: PathBuf,
        max_running: usize,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
    ) -> Result<String, String> {
        self.start_watchdog();

        let running = self.running_count();
        if running >= max_running {
            #[cfg(unix)]
            if let Some(prepared) = spawn_plan.prepared_task() {
                let _ = delete_resolved_task(&prepared.resolved_task());
            }
            return Err(format!(
                "background bash task limit exceeded: {running} running (max {max_running})"
            ));
        }

        let timeout = timeout.or(Some(DEFAULT_BG_TIMEOUT));
        let timeout_ms = timeout.map(|timeout| timeout.as_millis() as u64);
        let task_layout = allocate_task_layout(&storage_dir, &session_id)
            .map_err(|error| format!("failed to create background task layout: {error}"))?;
        let task_id = task_layout.paths.task_id.clone();
        let paths = task_layout.paths.clone();

        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            session_id.clone(),
            command.to_string(),
            workdir.clone(),
            project_root,
            timeout_ms,
            notify_on_completion,
            compressed,
        );
        attach_sandbox_metadata(&mut metadata, &spawn_plan);
        if let Err(error) = write_task_at(&task_layout, &metadata) {
            let _ = delete_resolved_task(&task_layout);
            return Err(format!(
                "failed to persist background task metadata: {error}"
            ));
        }
        self.dual_write_task(&paths, &metadata);
        let mut io_handles = TaskIoHandles::create(&task_layout, BgMode::Pipes, false)
            .map_err(|error| format!("failed to pre-open task output handles: {error}"))?;

        let child = match spawn_detached_child(
            &spawn_plan,
            command,
            shell,
            &shell_path,
            &paths,
            &workdir,
            &env,
            &mut io_handles,
            false,
        ) {
            Ok(child) => child,
            Err(error) => {
                crate::slog_warn!("failed to spawn background bash task {task_id}; deleting partial bundle: {error}");
                let _ = delete_task_bundle(&paths);
                return Err(error);
            }
        };

        let child_pid = child.id();
        metadata.status = BgTaskStatus::Running;
        metadata.child_pid = Some(child_pid);
        metadata.pgid = None;
        self.persist_task(&paths, &metadata)
            .map_err(|e| format!("failed to persist running background task metadata: {e}"))?;

        let task = Arc::new(BgTask {
            task_id: task_id.clone(),
            delivery_session_id: session_id.clone(),
            session_id,
            paths: paths.clone(),
            artifact_root: canonical_artifact_root(&paths),
            started: Instant::now(),
            last_reminder_at: Mutex::new(None),
            terminal_at: Mutex::new(None),
            state: Mutex::new(BgTaskState {
                metadata,
                runtime: TaskRuntime::Piped(Some(child)),
                io_handles: Some(io_handles),
                detached: false,
                child_exit_observed: false,
                buffer: BgBuffer::registered(&paths, BgMode::Pipes),
                terminal_output_cache: None,
                pending_terminal_override: None,
            }),
        });

        self.inner
            .tasks
            .lock()
            .map_err(|_| "background task registry lock poisoned".to_string())?
            .insert(task_id.clone(), task);

        Ok(task_id)
    }

    pub fn write_pty(
        &self,
        task_id: &str,
        session_id: &str,
        input: &[u8],
    ) -> Result<usize, String> {
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or_else(|| "task_not_found".to_string())?;

        let writer = {
            let state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            if state.metadata.mode != BgMode::Pty {
                return Err("task_not_pty".to_string());
            }
            if state.metadata.status.is_terminal() {
                return Err("task_exited".to_string());
            }
            match &state.runtime {
                TaskRuntime::Pty(Some(runtime)) => Arc::clone(&runtime.writer),
                TaskRuntime::Pty(None) => return Err("task_exited".to_string()),
                TaskRuntime::Piped(_) => return Err("task_not_pty".to_string()),
            }
        };

        let mut writer = writer
            .lock()
            .map_err(|_| "PTY writer lock poisoned".to_string())?;
        writer
            .write_all(input)
            .map_err(|error| format!("failed to write to PTY: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("failed to flush PTY writer: {error}"))?;
        Ok(input.len())
    }

    pub fn replay_session(&self, storage_dir: &Path, session_id: &str) -> Result<(), String> {
        self.replay_session_inner(storage_dir, session_id, None)
    }

    /// Thread name recorded by the last `maybe_gc_persisted` run, if any.
    #[doc(hidden)]
    pub fn persisted_gc_thread(&self) -> Option<String> {
        self.inner
            .persisted_gc_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn replay_session_for_project(
        &self,
        storage_dir: &Path,
        session_id: &str,
        project_root: &Path,
    ) -> Result<(), String> {
        self.replay_session_inner(storage_dir, session_id, Some(project_root))
    }

    fn replay_session_inner(
        &self,
        storage_dir: &Path,
        session_id: &str,
        project_root: Option<&Path>,
    ) -> Result<(), String> {
        self.start_watchdog();
        if !self.inner.persisted_gc_started.swap(true, Ordering::SeqCst) {
            // The persisted GC walks every session under the shared storage root
            // (liveness probes, row deletes, quarantines) and scales with the
            // machine's task history, not this session's. Replay runs before the
            // first request after configure, so keeping the GC inline made that
            // request wait on storage-wide housekeeping: 2.4 s on a warm box,
            // past the plugin's 5 s request timeout under load. Replay itself
            // needs only this project's rows, so the GC runs detached; a row it
            // would have deleted is at worst rehydrated as terminal and reaped
            // by the watchdog.
            let registry = self.clone();
            let storage_dir = storage_dir.to_path_buf();
            let spawned = std::thread::Builder::new()
                .name("aft-bash-task-gc".to_string())
                .spawn(move || {
                    if let Err(error) = registry.maybe_gc_persisted(&storage_dir) {
                        crate::slog_warn!("failed to GC persisted background bash tasks: {error}");
                    }
                });
            if let Err(error) = spawned {
                crate::slog_warn!("failed to spawn persisted background task GC: {error}");
            }
        }

        let canonical_project = project_root.map(canonicalized_path);
        // Replay strategy: DB is the post-v0.27 source of truth. Disk
        // fallback handles pre-v0.27 tasks that haven't been migrated and
        // the cold-start `__default__` namespace (configure runs before any
        // user session exists, so plugin-init triggers a session-less DB
        // lookup that will be empty until a real session writes a task).
        //
        // We deliberately keep the empty-DB / empty-disk path silent — it's
        // the normal startup case and would otherwise fire on every configure
        // (see GitHub user report against v0.27.0). INFO-level logs only when
        // disk actually returned tasks (real migration signal); WARN when the
        // DB lookup itself errored.
        let tasks = match self.replay_session_from_db(session_id, project_root) {
            Some(Ok(tasks)) if !tasks.is_empty() => tasks,
            Some(Ok(_)) => {
                let disk_tasks = self.replay_session_from_disk(storage_dir, session_id)?;
                if !disk_tasks.is_empty() {
                    crate::slog_info!(
                        "bash task replay: 0 in DB for session {}, {} from disk fallback",
                        session_id,
                        disk_tasks.len()
                    );
                }
                disk_tasks
            }
            Some(Err(error)) => {
                crate::slog_warn!(
                    "bash task replay DB lookup failed for session {}; falling back to disk: {}",
                    session_id,
                    error
                );
                self.replay_session_from_disk(storage_dir, session_id)?
            }
            None => {
                // DB pool unconfigured — common in tests + before harness is set.
                self.replay_session_from_disk(storage_dir, session_id)?
            }
        };

        for mut metadata in tasks {
            if project_root.is_none() && metadata.session_id != session_id {
                continue;
            }
            if let Some(canonical_project) = canonical_project.as_deref() {
                let metadata_project = metadata.project_root.as_deref().map(canonicalized_path);
                if metadata_project.as_deref() != Some(canonical_project) {
                    continue;
                }
            }

            if validate_task_id(&metadata.task_id).is_err() {
                crate::slog_warn!(
                    "ignoring persisted background task with invalid id {:?}",
                    metadata.task_id
                );
                continue;
            }
            // Another session in this daemon may bind the same project. Keep the
            // authoritative child handle and pinned artifact handles already in memory;
            // replacing them with a disk-only replay would orphan control of a live task.
            if self.task(&metadata.task_id).is_some() {
                continue;
            }
            let session_dir = session_tasks_dir(storage_dir, &metadata.session_id);
            let resolved = match resolve_task_layout(&session_dir, &metadata.task_id) {
                Ok(task) => task,
                Err(error) => {
                    if Self::persisted_task_process_is_alive(&metadata) {
                        crate::slog_warn!(
                            "refusing to quarantine unresolved live background task {}: {error}",
                            metadata.task_id
                        );
                        continue;
                    }
                    crate::slog_warn!(
                        "quarantining unresolved background task {}: {error}",
                        metadata.task_id
                    );
                    let _ = quarantine_task_layout(
                        storage_dir,
                        &session_dir,
                        &metadata.task_id,
                        "invalid",
                    );
                    continue;
                }
            };
            match read_task_at(&resolved) {
                Ok(disk)
                    if disk.task_id == metadata.task_id
                        && disk.session_id == metadata.session_id => {}
                Ok(_) | Err(_) => {
                    if Self::persisted_task_process_is_alive(&metadata) {
                        crate::slog_warn!(
                            "refusing to quarantine mismatched live background task {}",
                            metadata.task_id
                        );
                        continue;
                    }
                    let _ = quarantine_task_layout(
                        storage_dir,
                        &session_dir,
                        &metadata.task_id,
                        "mismatch",
                    );
                    continue;
                }
            }
            let paths = resolved.paths;
            let replay_task_id = metadata.task_id.clone();
            let delivery_session_id = (metadata.session_id != session_id).then_some(session_id);
            match metadata.status {
                BgTaskStatus::Starting => {
                    let completion_was_delivered = metadata.completion_delivered;
                    metadata.mark_terminal(
                        BgTaskStatus::Failed,
                        None,
                        Some("spawn aborted".to_string()),
                    );
                    metadata.completion_delivered |= completion_was_delivered;
                    let _ = self.persist_task(&paths, &metadata);
                    self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                    self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                }
                BgTaskStatus::Running | BgTaskStatus::Killing => {
                    if metadata.mode == BgMode::Pty {
                        if let Ok(Some(marker)) = read_exit_marker(&paths) {
                            let completion_was_delivered = metadata.completion_delivered;
                            metadata = terminal_metadata_from_marker(metadata, marker, None);
                            metadata.completion_delivered |= completion_was_delivered;
                            let _ = self.persist_task(&paths, &metadata);
                            self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                            self.insert_rehydrated_task(
                                metadata,
                                paths,
                                true,
                                delivery_session_id,
                            )?;
                        } else if metadata.status.is_terminal() {
                            self.insert_rehydrated_task(
                                metadata,
                                paths,
                                true,
                                delivery_session_id,
                            )?;
                        } else {
                            let completion_was_delivered = metadata.completion_delivered;
                            metadata.mark_terminal(
                                BgTaskStatus::Killed,
                                None,
                                Some("pty_lost_on_bridge_restart".to_string()),
                            );
                            metadata.completion_delivered |= completion_was_delivered;
                            let _ = self.persist_task(&paths, &metadata);
                            self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                            self.insert_rehydrated_task(
                                metadata,
                                paths,
                                true,
                                delivery_session_id,
                            )?;
                        }
                    } else if let Ok(Some(marker)) = read_exit_marker(&paths) {
                        let reason = (metadata.status == BgTaskStatus::Killing).then(|| {
                            "recovered from inconsistent killing state on replay".to_string()
                        });
                        if reason.is_some() {
                            crate::slog_warn!("background task {} had killing state with exit marker; preferring marker",
                            metadata.task_id);
                        }
                        let completion_was_delivered = metadata.completion_delivered;
                        metadata = terminal_metadata_from_marker(metadata, marker, reason);
                        metadata.completion_delivered |= completion_was_delivered;
                        let _ = self.persist_task(&paths, &metadata);
                        self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                        self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                    } else if metadata.status == BgTaskStatus::Killing {
                        let _ = write_kill_marker_if_absent(&paths);
                        let completion_was_delivered = metadata.completion_delivered;
                        metadata.mark_terminal(
                            BgTaskStatus::Killed,
                            None,
                            Some("recovered from inconsistent killing state on replay".to_string()),
                        );
                        metadata.completion_delivered |= completion_was_delivered;
                        let _ = self.persist_task(&paths, &metadata);
                        self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                        self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                    } else if Self::persisted_task_process_is_alive(&metadata) {
                        self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                    } else {
                        let completion_was_delivered = metadata.completion_delivered;
                        metadata.mark_terminal(
                            BgTaskStatus::FateUnknown,
                            None,
                            Some(restart_fate_unknown_reason(&metadata, &paths)),
                        );
                        metadata.completion_delivered |= completion_was_delivered;
                        let _ = self.persist_task(&paths, &metadata);
                        self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                        self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                    }
                }
                _ if metadata.status.is_terminal() => {
                    // Borrow `paths` for the completion enqueue BEFORE
                    // `insert_rehydrated_task` consumes it. The completion
                    // helper only reads from `paths` (stdout/stderr/exit) to
                    // reconstruct a tail preview, so it must see the same
                    // paths the rehydrated task will own.
                    self.enqueue_completion_if_needed(&metadata, Some(&paths), false);
                    self.insert_rehydrated_task(metadata, paths, true, delivery_session_id)?;
                }
                _ => {}
            }
            self.retarget_pending_completion(&replay_task_id, session_id);
        }

        Ok(())
    }

    fn replay_session_from_db(
        &self,
        session_id: &str,
        project_root: Option<&Path>,
    ) -> Option<Result<Vec<PersistedTask>, String>> {
        let pool = self
            .inner
            .db_pool
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let rows = if let Some(project_root) = project_root {
            let project_key = crate::path_identity::project_scope_key(project_root);
            crate::db::bash_tasks::list_replayable_bash_tasks_for_project(
                &conn,
                &harness,
                &project_key,
            )
        } else {
            crate::db::bash_tasks::list_bash_tasks_for_session(&conn, &harness, session_id)
        };
        Some(
            rows.map(|rows| rows.into_iter().map(PersistedTask::from).collect())
                .map_err(|error| error.to_string()),
        )
    }

    fn replay_session_from_disk(
        &self,
        storage_dir: &Path,
        session_id: &str,
    ) -> Result<Vec<PersistedTask>, String> {
        let dir = session_tasks_dir(storage_dir, session_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let (task_ids, invalid_entries) = discover_task_ids(&dir)
            .map_err(|error| format!("failed to discover background task layouts: {error}"))?;
        for entry in invalid_entries {
            if let Err(error) = quarantine_invalid_entry(storage_dir, &dir, &entry) {
                crate::slog_warn!(
                    "failed to quarantine invalid background task entry {:?}: {error}",
                    entry
                );
            }
        }

        let mut tasks = Vec::new();
        for task_id in task_ids {
            let task = match resolve_task_layout(&dir, &task_id) {
                Ok(task) => task,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && uninitialized_layout_is_recent(
                            &dir,
                            &task_id,
                            Duration::from_secs(5 * 60),
                        )
                        .unwrap_or(false) =>
                {
                    continue;
                }
                Err(error) => {
                    if self.db_has_live_process_for_task(&task_id) {
                        crate::slog_warn!(
                            "refusing to quarantine unresolved live background task {task_id} during replay: {error}"
                        );
                        continue;
                    }
                    crate::slog_warn!(
                        "quarantining unresolved background task {task_id} during replay: {error}"
                    );
                    let _ = quarantine_task_layout(storage_dir, &dir, &task_id, "invalid");
                    continue;
                }
            };
            match read_task_at(&task) {
                Ok(metadata) if metadata.session_id == session_id => tasks.push(metadata),
                Ok(_) => {
                    crate::slog_warn!(
                        "quarantining background task {task_id} with mismatched session metadata"
                    );
                    let _ = quarantine_task_layout(storage_dir, &dir, &task_id, "mismatch");
                }
                Err(error) => {
                    if self.db_has_live_process_for_task(&task_id) {
                        crate::slog_warn!(
                            "refusing to quarantine unreadable live background task {task_id} during replay: {error}"
                        );
                        continue;
                    }
                    crate::slog_warn!(
                        "quarantining invalid background task metadata {task_id} during replay: {error}"
                    );
                    let _ = quarantine_task_layout(storage_dir, &dir, &task_id, "invalid");
                }
            }
        }
        Ok(tasks)
    }

    pub fn register_watch(
        &self,
        task_id: String,
        pattern: WatchPattern,
        once: bool,
    ) -> Result<String, &'static str> {
        let task = self.task(&task_id).ok_or("task_not_found")?;
        validate_task_id(&task_id).map_err(|_| "invalid_task_id")?;
        let (mode, terminal_at_registration) = task
            .state
            .lock()
            .map(|state| {
                (
                    state.metadata.mode.clone(),
                    state.metadata.status.is_terminal(),
                )
            })
            .map_err(|_| "background_task_lock_poisoned")?;
        let mut stdout = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stdout))
            .transpose()
            .map_err(|_| "artifact_refused")?;
        let mut stderr = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stderr))
            .transpose()
            .map_err(|_| "artifact_refused")?;
        let mut pty = (mode == BgMode::Pty)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Pty))
            .transpose()
            .map_err(|_| "artifact_refused")?;

        let mut terminal_matches = Vec::new();
        let scanned_terminal = terminal_at_registration;
        let watch_id = {
            let mut registry = self
                .inner
                .watch_registry
                .lock()
                .map_err(|_| "watch_registry_poisoned")?;
            let watch_id = registry.register(task_id.clone(), pattern.clone(), once)?;
            match &mode {
                BgMode::Pipes => {
                    let stdout_key = format!("{task_id}:stdout");
                    let stderr_key = format!("{task_id}:stderr");
                    if terminal_at_registration {
                        registry.set_file_cursor(&stdout_key, 0);
                        registry.set_file_cursor(&stderr_key, 0);
                        terminal_matches.extend(registry.scan_file_new_bytes(
                            &stdout_key,
                            &task_id,
                            stdout.as_mut().expect("pipe stdout opened"),
                        ));
                        terminal_matches.extend(registry.scan_file_new_bytes(
                            &stderr_key,
                            &task_id,
                            stderr.as_mut().expect("pipe stderr opened"),
                        ));
                    } else {
                        registry.prime_file_cursor(
                            &stdout_key,
                            stdout.as_ref().expect("pipe stdout opened"),
                        );
                        registry.prime_file_cursor(
                            &stderr_key,
                            stderr.as_ref().expect("pipe stderr opened"),
                        );
                    }
                }
                BgMode::Pty => {
                    let pty_key = format!("{task_id}:pty");
                    if terminal_at_registration {
                        registry.set_file_cursor(&pty_key, 0);
                        terminal_matches.extend(registry.scan_file_new_bytes(
                            &pty_key,
                            &task_id,
                            pty.as_mut().expect("PTY artifact opened"),
                        ));
                    } else {
                        registry.prime_file_cursor(
                            &pty_key,
                            pty.as_ref().expect("PTY artifact opened"),
                        );
                    }
                }
            }
            watch_id
        };

        let (stdout_offset, stderr_offset, pty_offset) = self.watch_stream_cursors(&task_id);
        self.persist_watch_registration(
            &task.session_id,
            &task_id,
            &watch_id,
            &pattern,
            once,
            stdout_offset,
            stderr_offset,
            pty_offset,
        );

        if task.is_terminal() {
            if !scanned_terminal {
                terminal_matches = {
                    let mut registry = self
                        .inner
                        .watch_registry
                        .lock()
                        .map_err(|_| "watch_registry_poisoned")?;
                    match &mode {
                        BgMode::Pipes => {
                            let stdout_key = format!("{task_id}:stdout");
                            let stderr_key = format!("{task_id}:stderr");
                            registry.set_file_cursor(&stdout_key, 0);
                            registry.set_file_cursor(&stderr_key, 0);
                            let mut matches = registry.scan_file_new_bytes(
                                &stdout_key,
                                &task_id,
                                stdout.as_mut().expect("pipe stdout opened"),
                            );
                            matches.extend(registry.scan_file_new_bytes(
                                &stderr_key,
                                &task_id,
                                stderr.as_mut().expect("pipe stderr opened"),
                            ));
                            matches
                        }
                        BgMode::Pty => {
                            let pty_key = format!("{task_id}:pty");
                            registry.set_file_cursor(&pty_key, 0);
                            registry.scan_file_new_bytes(
                                &pty_key,
                                &task_id,
                                pty.as_mut().expect("PTY artifact opened"),
                            )
                        }
                    }
                };
            }

            let (stdout_offset, stderr_offset, pty_offset) = self.watch_stream_cursors(&task_id);
            let (watch_controlled, watch_matched) = self.task_watch_state(&task_id);
            if terminal_matches.is_empty() && (!watch_controlled || watch_matched) {
                if watch_matched {
                    let _ = task.set_completion_delivered(true, self);
                    self.clear_task_watch_state(&task_id);
                    // Immediate terminal delivery is already confirmed locally.
                    self.delete_persisted_watches_for_task(&task.session_id, &task_id);
                }
                return Ok(watch_id);
            }

            let completion = self
                .remove_pending_completion(&task_id)
                .or_else(|| self.completion_snapshot_for_task(&task));
            if terminal_matches.is_empty() {
                if let Some(completion) = completion.as_ref() {
                    self.emit_bash_watch_exit(completion);
                }
            } else {
                for pattern_match in &terminal_matches {
                    self.persist_watch_match(
                        &task.session_id,
                        &task_id,
                        pattern_match,
                        stdout_offset,
                        stderr_offset,
                        pty_offset,
                    );
                    self.emit_bash_pattern_match(&task.delivery_session_id, pattern_match.clone());
                }
            }
            let _ = task.set_completion_delivered(true, self);
            self.clear_task_watch_state(&task_id);
            // Same as live path: terminal registration finishes delivery in-process.
            self.delete_persisted_watches_for_task(&task.session_id, &task_id);
        }

        Ok(watch_id)
    }

    pub fn unregister_watch(&self, task_id: &str, watch_id: &str) {
        let session_id = self.task(task_id).map(|task| task.session_id.clone());
        if let Ok(mut registry) = self.inner.watch_registry.lock() {
            registry.unregister(task_id, watch_id);
        }
        if let Some(session_id) = session_id {
            self.delete_persisted_watch(&session_id, task_id, watch_id);
        }
    }

    pub fn active_watch_count(&self, task_id: &str) -> usize {
        self.inner
            .watch_registry
            .lock()
            .map(|registry| registry.active_count(task_id))
            .unwrap_or(0)
    }

    fn task_watch_state(&self, task_id: &str) -> (bool, bool) {
        self.inner
            .watch_registry
            .lock()
            .map(|registry| {
                (
                    registry.has_controlled_task(task_id),
                    registry.has_matched_task(task_id),
                )
            })
            .unwrap_or((false, false))
    }

    fn task_has_watch_control(&self, task_id: &str) -> bool {
        self.inner
            .watch_registry
            .lock()
            .map(|registry| registry.has_controlled_task(task_id))
            .unwrap_or(false)
    }

    fn clear_task_watch_state(&self, task_id: &str) {
        if let Ok(mut registry) = self.inner.watch_registry.lock() {
            registry.clear_task(task_id);
        }
    }

    pub(crate) fn scan_task_watch_output(&self, task: &Arc<BgTask>) {
        let mode = match task.state.lock() {
            Ok(state) => state.metadata.mode.clone(),
            Err(_) => return,
        };
        let mut stdout = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stdout))
            .transpose()
            .ok()
            .flatten();
        let mut stderr = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stderr))
            .transpose()
            .ok()
            .flatten();
        let mut pty = (mode == BgMode::Pty)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Pty))
            .transpose()
            .ok()
            .flatten();
        let mut matches = Vec::new();
        if let Ok(mut registry) = self.inner.watch_registry.lock() {
            match mode {
                BgMode::Pipes => {
                    let (Some(stdout), Some(stderr)) = (stdout.as_mut(), stderr.as_mut()) else {
                        return;
                    };
                    let stdout_key = format!("{}:stdout", task.task_id);
                    let stderr_key = format!("{}:stderr", task.task_id);
                    matches.extend(registry.scan_file_new_bytes(
                        &stdout_key,
                        &task.task_id,
                        stdout,
                    ));
                    matches.extend(registry.scan_file_new_bytes(
                        &stderr_key,
                        &task.task_id,
                        stderr,
                    ));
                }
                BgMode::Pty => {
                    let Some(pty) = pty.as_mut() else {
                        return;
                    };
                    let pty_key = format!("{}:pty", task.task_id);
                    matches.extend(registry.scan_file_new_bytes(&pty_key, &task.task_id, pty));
                }
            }
        }
        let (stdout_offset, stderr_offset, pty_offset) = self.watch_stream_cursors(&task.task_id);
        if matches.is_empty() {
            // Advance durable cursors even when nothing matched so a restart
            // does not re-scan already-observed bytes.
            if self.task_has_watch_control(&task.task_id) {
                self.persist_task_watch_cursors(
                    &task.session_id,
                    &task.task_id,
                    stdout_offset,
                    stderr_offset,
                    pty_offset,
                );
            }
            return;
        }
        for pattern_match in matches {
            self.persist_watch_match(
                &task.session_id,
                &task.task_id,
                &pattern_match,
                stdout_offset,
                stderr_offset,
                pty_offset,
            );
            self.emit_bash_pattern_match(&task.delivery_session_id, pattern_match);
        }
        self.persist_task_watch_cursors(
            &task.session_id,
            &task.task_id,
            stdout_offset,
            stderr_offset,
            pty_offset,
        );
    }

    pub fn status(
        &self,
        task_id: &str,
        session_id: &str,
        project_root: Option<&Path>,
        storage_dir: Option<&Path>,
        preview_bytes: usize,
    ) -> Option<BgTaskSnapshot> {
        validate_task_id(task_id).ok()?;
        let terminal_db_fallback_allowed = storage_dir
            .is_some_and(|storage_dir| task_bundle_is_absent(storage_dir, session_id, task_id));
        let mut task = self.task_for_session(task_id, session_id);
        if task.is_none() {
            if let Some(storage_dir) = storage_dir {
                let _ = if let Some(project_root) = project_root {
                    self.replay_session_for_project(storage_dir, session_id, project_root)
                } else {
                    self.replay_session(storage_dir, session_id)
                };
                task = self.task_for_session(task_id, session_id);
            }
        }
        let Some(task) = task else {
            if terminal_db_fallback_allowed {
                if let Some(snapshot) = storage_dir.and_then(|storage_dir| {
                    self.terminal_db_status_for_session(session_id, task_id, storage_dir)
                }) {
                    return Some(snapshot);
                }
            }
            return self.status_relaxed(
                task_id,
                session_id,
                project_root?,
                storage_dir?,
                preview_bytes,
                terminal_db_fallback_allowed,
            );
        };
        let _ = self.poll_task(&task);
        Some(self.snapshot_with_terminal_cache(&task, preview_bytes))
    }

    fn status_relaxed_task(
        &self,
        task_id: &str,
        project_root: &Path,
        storage_dir: &Path,
    ) -> Option<Arc<BgTask>> {
        validate_task_id(task_id).ok()?;
        let canonical_project = canonicalized_path(project_root);
        match self.lookup_relaxed_task_from_db(task_id, project_root) {
            Some(Ok(Some(row))) => {
                let metadata = PersistedTask::from(row);
                if let Some(task) = self.task(task_id) {
                    let matches_project = task
                        .state
                        .lock()
                        .map(|state| {
                            state
                                .metadata
                                .project_root
                                .as_deref()
                                .map(canonicalized_path)
                                .as_deref()
                                == Some(canonical_project.as_path())
                        })
                        .unwrap_or(false);
                    return matches_project.then_some(task);
                }
                let resolved = resolve_task_layout(
                    &session_tasks_dir(storage_dir, &metadata.session_id),
                    &metadata.task_id,
                )
                .ok()?;
                let disk = read_task_at(&resolved).ok()?;
                if disk.task_id != metadata.task_id || disk.session_id != metadata.session_id {
                    return None;
                }
                if self
                    .insert_rehydrated_task(metadata, resolved.paths, true, None)
                    .is_err()
                {
                    return None;
                }
                return self.task(task_id);
            }
            Some(Ok(None)) => {
                crate::slog_info!(
                    "bash task relaxed DB miss for {}; falling back to disk",
                    task_id
                );
            }
            Some(Err(error)) => {
                crate::slog_warn!(
                    "bash task relaxed DB lookup failed for {}; falling back to disk: {}",
                    task_id,
                    error
                );
            }
            None => {
                crate::slog_info!(
                    "bash task relaxed DB unavailable for {}; falling back to disk",
                    task_id
                );
            }
        }
        let root = storage_dir.join("bash-tasks");
        let entries = fs::read_dir(&root).ok()?;
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let resolved = match resolve_task_layout(&dir, task_id) {
                Ok(task) => task,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    if self.db_has_live_process_for_task(task_id) {
                        crate::slog_warn!(
                            "refusing to quarantine unresolved live background task {task_id} during relaxed lookup: {error}"
                        );
                        continue;
                    }
                    crate::slog_warn!(
                        "quarantining unresolved background task {task_id} during relaxed lookup: {error}"
                    );
                    let _ = quarantine_task_layout(storage_dir, &dir, task_id, "invalid");
                    continue;
                }
            };
            let metadata = match read_task_at(&resolved) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if self.db_has_live_process_for_task(task_id) {
                        crate::slog_warn!(
                            "refusing to quarantine unreadable live background task {task_id} during relaxed lookup: {error}"
                        );
                        continue;
                    }
                    crate::slog_warn!(
                        "quarantining invalid background task metadata {task_id} during relaxed lookup: {error}"
                    );
                    let _ = quarantine_task_layout(storage_dir, &dir, task_id, "invalid");
                    continue;
                }
            };
            let metadata_project = metadata.project_root.as_deref().map(canonicalized_path);
            if metadata_project.as_deref() != Some(canonical_project.as_path()) {
                continue;
            }
            if let Some(task) = self.task(task_id) {
                let matches_project = task
                    .state
                    .lock()
                    .map(|state| {
                        state
                            .metadata
                            .project_root
                            .as_deref()
                            .map(canonicalized_path)
                            .as_deref()
                            == Some(canonical_project.as_path())
                    })
                    .unwrap_or(false);
                return matches_project.then_some(task);
            }
            if self
                .insert_rehydrated_task(metadata, resolved.paths, true, None)
                .is_err()
            {
                return None;
            }
            return self.task(task_id);
        }
        None
    }

    fn lookup_relaxed_task_from_db(
        &self,
        task_id: &str,
        project_root: &Path,
    ) -> Option<Result<Option<BashTaskRow>, String>> {
        let pool = self
            .inner
            .db_pool
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone())?;
        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => return Some(Err("db mutex poisoned".to_string())),
        };
        let project_key = crate::path_identity::project_scope_key(project_root);
        Some(
            crate::db::bash_tasks::find_bash_task_for_project(
                &conn,
                &harness,
                &project_key,
                task_id,
            )
            .map_err(|error| error.to_string()),
        )
    }

    pub(super) fn status_relaxed(
        &self,
        task_id: &str,
        _session_id: &str,
        project_root: &Path,
        storage_dir: &Path,
        preview_bytes: usize,
        allow_terminal_db_fallback: bool,
    ) -> Option<BgTaskSnapshot> {
        let fallback_row = if allow_terminal_db_fallback {
            self.lookup_relaxed_task_from_db(task_id, project_root)
        } else {
            None
        }
        .and_then(Result::ok)
        .flatten()
        .filter(|row| task_bundle_is_absent(storage_dir, &row.session_id, &row.task_id));
        if let Some(task) = self.status_relaxed_task(task_id, project_root, storage_dir) {
            let _ = self.poll_task(&task);
            return Some(self.snapshot_with_terminal_cache(&task, preview_bytes));
        }
        let row = fallback_row?;
        let metadata = PersistedTask::from(row.clone());
        metadata
            .status
            .is_terminal()
            .then(|| terminal_db_row_snapshot(row, metadata))
    }

    pub fn kill_relaxed(
        &self,
        task_id: &str,
        project_root: &Path,
        storage_dir: &Path,
    ) -> Result<BgTaskSnapshot, String> {
        let task = self
            .status_relaxed_task(task_id, project_root, storage_dir)
            .ok_or_else(|| format!("background task not found: {task_id}"))?;
        self.kill_with_status(task_id, &task.session_id, BgTaskStatus::Killed)
    }

    pub fn maybe_gc_persisted(&self, storage_dir: &Path) -> Result<usize, String> {
        #[cfg(test)]
        self.inner.persisted_gc_runs.fetch_add(1, Ordering::SeqCst);
        *self
            .inner
            .persisted_gc_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
            std::thread::current()
                .name()
                .unwrap_or("<unnamed>")
                .to_string(),
        );

        let mut deleted = 0usize;

        let root = storage_dir.join("bash-tasks");
        if root.exists() {
            let session_dirs = fs::read_dir(&root).map_err(|e| {
                format!(
                    "failed to read background task root {}: {e}",
                    root.display()
                )
            })?;
            for session_entry in session_dirs.flatten() {
                let session_dir = session_entry.path();
                if !session_dir.is_dir() {
                    continue;
                }
                let (task_ids, invalid_entries) = match discover_task_ids(&session_dir) {
                    Ok(discovery) => discovery,
                    Err(error) => {
                        crate::slog_warn!(
                            "failed to discover background task session {}: {error}",
                            session_dir.display()
                        );
                        continue;
                    }
                };
                for entry in invalid_entries {
                    let _ = quarantine_invalid_entry(storage_dir, &session_dir, &entry);
                }
                for task_id in task_ids {
                    let resolved = match resolve_task_layout(&session_dir, &task_id) {
                        Ok(task) => task,
                        // Uncertainty never quarantines: if the age probe itself
                        // fails (the directory vanished or is mid-creation), skip
                        // this pass; a genuinely abandoned layout is still there
                        // for the next one.
                        Err(error)
                            if error.kind() == std::io::ErrorKind::NotFound
                                && uninitialized_layout_is_recent(
                                    &session_dir,
                                    &task_id,
                                    Duration::from_secs(5 * 60),
                                )
                                .unwrap_or(true) =>
                        {
                            continue;
                        }
                        Err(error) => {
                            if self.db_has_live_process_for_task(&task_id) {
                                crate::slog_warn!(
                                    "refusing to quarantine unresolved live background task {task_id} during GC: {error}"
                                );
                                continue;
                            }
                            crate::slog_warn!(
                                "quarantining unresolved background task {task_id}: {error}"
                            );
                            quarantine_task_layout(storage_dir, &session_dir, &task_id, "invalid")
                                .map_err(|error| error.to_string())?;
                            continue;
                        }
                    };
                    if modified_within(&resolved.paths.json, PERSISTED_GC_GRACE) {
                        continue;
                    }
                    let metadata = match read_task_at(&resolved) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            if self.db_has_live_process_for_task(&task_id) {
                                crate::slog_warn!(
                                    "refusing to quarantine unreadable live background task {task_id} during GC: {error}"
                                );
                                continue;
                            }
                            crate::slog_warn!(
                                "quarantining corrupt background task metadata {task_id}: {error}"
                            );
                            quarantine_task_layout(storage_dir, &session_dir, &task_id, "corrupt")
                                .map_err(|error| error.to_string())?;
                            continue;
                        }
                    };
                    if !(metadata.status.is_terminal() && metadata.completion_delivered) {
                        continue;
                    }
                    if Self::persisted_task_process_is_alive(&metadata)
                        || self.db_has_live_process_for_task(&task_id)
                    {
                        crate::slog_warn!(
                            "refusing to delete terminal background task bundle {task_id}: recorded process is still alive"
                        );
                        continue;
                    }
                    match delete_task_bundle(&resolved.paths) {
                        Ok(()) => {
                            self.delete_gc_task_from_db(&metadata);
                            self.evaluate_erased_watch_targets();
                            deleted += 1;
                            log::debug!(
                                "deleted persisted background task bundle {}",
                                metadata.task_id
                            );
                        }
                        Err(error) => {
                            crate::slog_warn!(
                                "failed to delete background task bundle {}: {error}",
                                metadata.task_id
                            );
                        }
                    }
                }
            }
        }
        gc_quarantine(storage_dir);
        Ok(deleted)
    }

    pub fn list(&self, preview_bytes: usize) -> Vec<BgTaskSnapshot> {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map(|tasks| tasks.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        tasks
            .into_iter()
            .map(|task| {
                let _ = self.poll_task(&task);
                self.snapshot_with_terminal_cache(&task, preview_bytes)
            })
            .collect()
    }

    /// Replace terminal pipe snapshots with the task's cached rendered output.
    /// Running tasks stay raw (tail-only) so agents debugging a live process see
    /// exactly what it emitted. PTY tasks are explicitly excluded: their raw
    /// terminal bytes are rendered by the plugin's PTY path, not the line
    /// compressor.
    fn maybe_compress_snapshot(&self, task: &Arc<BgTask>, snapshot: &mut BgTaskSnapshot) {
        if !snapshot.info.status.is_terminal() || snapshot.info.mode == BgMode::Pty {
            return;
        }
        if let Some(cache) = self.ensure_terminal_output_cache(task) {
            snapshot.output_preview = cache.output_preview;
            snapshot.output_truncated = cache.output_truncated;
        }
    }

    pub fn kill(&self, task_id: &str, session_id: &str) -> Result<BgTaskSnapshot, String> {
        self.kill_with_status(task_id, session_id, BgTaskStatus::Killed)
    }

    /// Terminate live tasks whose project root has been confirmed absent by
    /// subc's consecutive directory-absence scans, so tasks cannot keep using
    /// a root that has been verified for reclamation.
    ///
    /// The absence signal intentionally cannot distinguish deletion from
    /// renaming: a task's cwd handle can follow a moved directory even while
    /// the registered path is absent. A renamed root is nevertheless a retired
    /// registry identity, so killing those tasks is accepted; operational
    /// guidance is to rename a project only after its live tasks have ended.
    pub fn kill_running_tasks_for_root(&self, project_root: &Path) -> usize {
        let canonical_root = canonicalized_path(project_root);
        let targets = self
            .inner
            .tasks
            .lock()
            .map(|tasks| {
                tasks
                    .values()
                    .filter_map(|task| {
                        let state = task.state.lock().ok()?;
                        let status = &state.metadata.status;
                        let running = matches!(status, BgTaskStatus::Running)
                            || (state.metadata.mode == BgMode::Pty
                                && matches!(status, BgTaskStatus::Killing));
                        if !running {
                            return None;
                        }
                        let task_root = state
                            .metadata
                            .project_root
                            .as_deref()
                            .unwrap_or(&state.metadata.workdir);
                        (canonicalized_path(task_root) == canonical_root)
                            .then(|| (task.task_id.clone(), task.session_id.clone()))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let mut killed = 0;
        for (task_id, session_id) in targets {
            match self.kill_with_status_reason(
                &task_id,
                &session_id,
                BgTaskStatus::Killed,
                Some(ROOT_RECLAIMED_REASON.to_string()),
            ) {
                Ok(_) => killed += 1,
                Err(error) => crate::slog_warn!(
                    "failed to terminate background task {task_id} for reclaimed root {}: {error}",
                    project_root.display()
                ),
            }
        }
        killed
    }

    pub fn promote(&self, task_id: &str, session_id: &str) -> Result<bool, String> {
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or_else(|| format!("background task not found: {task_id}"))?;
        let terminal_after_promote = {
            let mut state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            let updated = self
                .update_task_metadata(&task.paths, |metadata| {
                    metadata.notify_on_completion = true;
                    metadata.completion_delivered = false;
                })
                .map_err(|e| format!("failed to promote background task: {e}"))?;
            state.metadata = updated;
            state.metadata.status.is_terminal()
        };
        if terminal_after_promote {
            self.post_terminal_transition(&task, true)?;
        }
        Ok(true)
    }

    pub(crate) fn kill_for_timeout(&self, task_id: &str, session_id: &str) -> Result<(), String> {
        self.kill_with_status(task_id, session_id, BgTaskStatus::TimedOut)
            .map(|_| ())
    }

    pub fn cleanup_finished(&self, older_than: Duration) {
        let cutoff = Instant::now().checked_sub(older_than);
        let removable_paths: Vec<(String, TaskPaths)> =
            if let Ok(mut tasks) = self.inner.tasks.lock() {
                let removable = tasks
                    .iter()
                    .filter_map(|(task_id, task)| {
                        let delivered_terminal = task
                            .state
                            .lock()
                            .map(|state| {
                                state.metadata.status.is_terminal()
                                    && state.metadata.completion_delivered
                            })
                            .unwrap_or(false);
                        if !delivered_terminal {
                            return None;
                        }

                        let terminal_at = task.terminal_at.lock().ok().and_then(|at| *at);
                        let expired = match (terminal_at, cutoff) {
                            (Some(terminal_at), Some(cutoff)) => terminal_at <= cutoff,
                            (Some(_), None) => true,
                            (None, _) => false,
                        };
                        expired.then(|| task_id.clone())
                    })
                    .collect::<Vec<_>>();

                removable
                    .into_iter()
                    .filter_map(|task_id| {
                        tasks
                            .remove(&task_id)
                            .map(|task| (task_id, task.paths.clone()))
                    })
                    .collect()
            } else {
                Vec::new()
            };

        for (task_id, paths) in removable_paths {
            match delete_task_bundle(&paths) {
                Ok(()) => log::debug!("deleted persisted background task bundle {task_id}"),
                Err(error) => crate::slog_warn!(
                    "failed to delete persisted background task bundle {task_id}: {error}"
                ),
            }
        }
    }

    pub fn drain_completions(&self) -> Vec<BgCompletion> {
        self.drain_completions_for_session(None)
    }

    pub fn drain_completions_for_session(&self, session_id: Option<&str>) -> Vec<BgCompletion> {
        if let Some(session_id) = session_id {
            self.redeliver_pending_watches_for_session(session_id);
        }
        let completions = match self.inner.completions.lock() {
            Ok(completions) => completions,
            Err(_) => return Vec::new(),
        };

        completions
            .iter()
            .filter(|completion| completion_matches_session(completion, session_id))
            .cloned()
            .collect()
    }

    pub fn has_completions_for_session(&self, session_id: Option<&str>) -> bool {
        match self.inner.completions.lock() {
            Ok(completions) => completions
                .iter()
                .any(|completion| completion_matches_session(completion, session_id)),
            // Bias to safety: if the queue state cannot be inspected cheaply,
            // let callers take the existing drain path rather than risk
            // suppressing a pending completion.
            Err(_) => true,
        }
    }

    pub fn ack_completions_for_session(
        &self,
        session_id: Option<&str>,
        task_ids: &[String],
    ) -> Vec<String> {
        if task_ids.is_empty() {
            return Vec::new();
        }
        let requested_task_ids = task_ids.iter().map(String::as_str).collect::<HashSet<_>>();
        let mut completion_sessions = HashMap::new();
        if let Ok(mut completions) = self.inner.completions.lock() {
            completions.retain(|completion| {
                let session_matches = session_id
                    .map(|session_id| completion.session_id == session_id)
                    .unwrap_or(true);
                if session_matches && requested_task_ids.contains(completion.task_id.as_str()) {
                    completion_sessions
                        .insert(completion.task_id.clone(), completion.session_id.clone());
                    false
                } else {
                    true
                }
            });
        }

        let mut delivered = Vec::new();
        for task_id in task_ids {
            if self.has_erased_watch_reference(task_id) {
                if let Some((harness, pool)) = self.db_harness_and_pool() {
                    if let Ok(conn) = pool.lock() {
                        if let Ok(rows) =
                            crate::db::bash_watches::list_bash_pattern_watches_by_task_id(
                                &conn, &harness, task_id,
                            )
                        {
                            for row in rows {
                                let _ = crate::db::bash_watches::delete_bash_pattern_watch(
                                    &conn,
                                    &harness,
                                    &row.session_id,
                                    task_id,
                                    &row.watch_id,
                                );
                            }
                        }
                    }
                }
                self.clear_task_watch_state(task_id);
                if let Ok(mut registry) = self.inner.watch_registry.lock() {
                    registry.forget_erased_task(task_id);
                }
                delivered.push(task_id.clone());
                continue;
            }
            let task = if let Some(session_id) = session_id {
                self.task_for_session(task_id, session_id)
                    .or_else(|| {
                        self.task(task_id)
                            .filter(|task| task.delivery_session_id == session_id)
                    })
                    .or_else(|| {
                        completion_sessions
                            .contains_key(task_id)
                            .then(|| self.task(task_id))
                            .flatten()
                    })
            } else if let Some(completion_session_id) = completion_sessions.get(task_id) {
                self.task_for_session(task_id, completion_session_id)
                    .or_else(|| self.task(task_id))
            } else {
                self.task(task_id)
            };
            if let Some(task) = task {
                let terminal = task
                    .state
                    .lock()
                    .map(|state| state.metadata.status.is_terminal())
                    .unwrap_or(false);
                // Pattern-watch delivery shares the completion ack lane: once the
                // plugin confirms the agent saw the notification, drop once-watches
                // (and all watches on terminal tasks) so restart cannot re-fire them.
                self.ack_persisted_watches_for_task(&task.session_id, task_id, terminal);
                if terminal {
                    self.clear_task_watch_state(task_id);
                    if task.set_completion_delivered(true, self).is_ok() {
                        delivered.push(task_id.clone());
                    }
                } else {
                    // Mid-run pattern-match ack must not flip completion_delivered —
                    // the task is still running and will need a real completion later.
                    self.sync_memory_watches_after_ack(task_id);
                    delivered.push(task_id.clone());
                }
            } else if let Some(session_id) = session_id {
                // Task may have been cleaned from memory; still clear durable watches.
                self.ack_persisted_watches_for_task(session_id, task_id, true);
                delivered.push(task_id.clone());
            }
        }

        delivered
    }

    fn sync_memory_watches_after_ack(&self, task_id: &str) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let session_id = match self.task(task_id) {
            Some(task) => task.session_id.clone(),
            None => return,
        };
        let Ok(conn) = pool.lock() else {
            return;
        };
        let Ok(rows) = crate::db::bash_watches::list_bash_pattern_watches_for_task(
            &conn,
            &harness,
            &session_id,
            task_id,
        ) else {
            return;
        };
        let remaining: HashSet<String> = rows.into_iter().map(|row| row.watch_id).collect();
        if let Ok(mut registry) = self.inner.watch_registry.lock() {
            registry.retain_watch_ids(task_id, &remaining);
        }
    }

    pub fn pending_completions_for_session(&self, session_id: &str) -> Vec<BgCompletion> {
        self.inner
            .completions
            .lock()
            .map(|completions| {
                completions
                    .iter()
                    .filter(|completion| completion.session_id == session_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn remove_pending_completion(&self, task_id: &str) -> Option<BgCompletion> {
        let mut completions = self.inner.completions.lock().ok()?;
        let idx = completions
            .iter()
            .position(|completion| completion.task_id == task_id)?;
        completions.remove(idx)
    }

    fn retarget_pending_completion(&self, task_id: &str, session_id: &str) {
        if let Ok(mut completions) = self.inner.completions.lock() {
            if let Some(completion) = completions
                .iter_mut()
                .find(|completion| completion.task_id == task_id)
            {
                completion.session_id = session_id.to_string();
            }
        }
    }

    fn completion_snapshot_for_task(&self, task: &Arc<BgTask>) -> Option<BgCompletion> {
        let snapshot = self.snapshot_with_terminal_cache(task, RUNNING_OUTPUT_PREVIEW_BYTES);
        if !snapshot.info.status.is_terminal() {
            return None;
        }
        let (output_preview, output_truncated) = if snapshot.info.mode == BgMode::Pty {
            (String::new(), false)
        } else {
            self.ensure_terminal_output_cache(task)
                .map(|cache| completion_preview_for_cache(&cache, snapshot.exit_code))
                .unwrap_or_else(|| (String::new(), false))
        };
        Some(BgCompletion {
            task_id: snapshot.info.task_id,
            session_id: task.delivery_session_id.clone(),
            status: snapshot.info.status,
            exit_code: snapshot.exit_code,
            command: snapshot.info.command,
            output_preview,
            output_truncated,
            original_tokens: None,
            compressed_tokens: None,
            tokens_skipped: false,
            status_reason: snapshot.info.status_reason,
        })
    }

    pub fn detach(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut tasks) = self.inner.tasks.lock() {
            for task in tasks.values() {
                if let Ok(mut state) = task.state.lock() {
                    match &mut state.runtime {
                        TaskRuntime::Piped(child) => *child = None,
                        TaskRuntime::Pty(runtime) => *runtime = None,
                    }
                    state.detached = true;
                }
            }
            tasks.clear();
        }
    }

    pub fn shutdown(&self) {
        let tasks = self
            .inner
            .tasks
            .lock()
            .map(|tasks| {
                tasks
                    .values()
                    .map(|task| (task.task_id.clone(), task.session_id.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (task_id, session_id) in tasks {
            let _ = self.kill(&task_id, &session_id);
        }
    }

    pub(crate) fn poll_task(&self, task: &Arc<BgTask>) -> Result<(), String> {
        if let Ok(state) = task.state.lock() {
            if let TaskRuntime::Pty(Some(pty)) = &state.runtime {
                // On Windows ConPTY, the reader may not observe EOF while the
                // master handle is still held in `PtyRuntime`. The waiter writes
                // the authoritative exit marker before setting `exit_observed`,
                // so once exit is observed we can finalize from that marker and
                // drop the runtime, which lets the reader finish. Waiting for
                // `reader_done && exit_observed` wedges completed PTY tasks on
                // Windows.
                if !pty.exit_observed.load(Ordering::SeqCst) {
                    return Ok(());
                }
            }
        }
        let marker = match read_exit_marker(&task.paths) {
            Ok(Some(marker)) => marker,
            Ok(None) => return Ok(()),
            Err(error) => return Err(format!("failed to read exit marker: {error}")),
        };
        self.finalize_from_marker(task, marker, None)
    }

    pub(crate) fn reap_child(&self, task: &Arc<BgTask>) {
        let mut needs_completion = false;
        {
            let Ok(mut state) = task.state.lock() else {
                return;
            };
            match &mut state.runtime {
                TaskRuntime::Piped(child_slot) => {
                    if let Some(child) = child_slot.as_mut() {
                        if let Ok(Some(status)) = child.try_wait() {
                            *child_slot = None;
                            state.detached = true;
                            state.child_exit_observed = true;
                            if let Some(handles) = state.io_handles.as_mut() {
                                if handles.artifact_len(TaskArtifact::Exit).unwrap_or(1) == 0 {
                                    let marker = status
                                        .code()
                                        .map(|code| code.to_string())
                                        .unwrap_or_else(|| "1".to_string());
                                    let _ = handles.write(TaskArtifact::Exit, marker.as_bytes());
                                }
                            }
                        }
                    } else if state.detached {
                        let child_known_dead = state.child_exit_observed
                            || state
                                .metadata
                                .child_pid
                                .is_some_and(|pid| !is_process_alive(pid));
                        if child_known_dead {
                            needs_completion =
                                self.fail_without_exit_marker_if_needed(task, &mut state);
                        }
                    }
                }
                TaskRuntime::Pty(Some(pty)) => {
                    if pty.exit_observed.load(Ordering::SeqCst) {
                        drop(state);
                        let _ = self.poll_task(task);
                        return;
                    }
                }
                TaskRuntime::Pty(None) => {}
            }
        }
        if needs_completion {
            let _ = self.post_terminal_transition(task, true);
        }
    }

    fn fail_without_exit_marker_if_needed(
        &self,
        task: &Arc<BgTask>,
        state: &mut BgTaskState,
    ) -> bool {
        if state.metadata.status.is_terminal() {
            return false;
        }
        if matches!(read_exit_marker(&task.paths), Ok(Some(_))) {
            return false;
        }
        let watch_controlled = self.task_has_watch_control(&task.task_id);
        let child_exit_observed = state.child_exit_observed;
        let updated = self.update_task_metadata(&task.paths, |metadata| {
            let (status, reason) = if child_exit_observed {
                (
                    BgTaskStatus::Failed,
                    "process exited without exit marker".to_string(),
                )
            } else {
                (
                    BgTaskStatus::FateUnknown,
                    restart_fate_unknown_reason(metadata, &task.paths),
                )
            };
            metadata.mark_terminal(status, None, Some(reason));
            if watch_controlled {
                metadata.completion_delivered = true;
            }
        });
        if let Ok(metadata) = updated {
            state.pending_terminal_override = None;
            state.metadata = metadata;
            task.mark_terminal_now();
            return true;
        }
        false
    }

    pub(crate) fn running_tasks(&self) -> Vec<Arc<BgTask>> {
        self.inner
            .tasks
            .lock()
            .map(|tasks| {
                tasks
                    .values()
                    .filter(|task| task.is_running())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn insert_rehydrated_task(
        &self,
        metadata: PersistedTask,
        paths: TaskPaths,
        detached: bool,
        delivery_session_id: Option<&str>,
    ) -> Result<(), String> {
        let task_id = metadata.task_id.clone();
        let session_id = metadata.session_id.clone();
        let started = started_instant_from_unix_millis(metadata.started_at);
        let suppress_replayed_running_reminder = metadata.status == BgTaskStatus::Running;
        let mode = metadata.mode.clone();
        let task = Arc::new(BgTask {
            task_id: task_id.clone(),
            delivery_session_id: delivery_session_id.unwrap_or(&session_id).to_string(),
            session_id,
            paths: paths.clone(),
            artifact_root: canonical_artifact_root(&paths),
            started,
            last_reminder_at: Mutex::new(suppress_replayed_running_reminder.then(Instant::now)),
            terminal_at: Mutex::new(metadata.status.is_terminal().then(Instant::now)),
            state: Mutex::new(BgTaskState {
                metadata,
                runtime: if mode == BgMode::Pty {
                    TaskRuntime::Pty(None)
                } else {
                    TaskRuntime::Piped(None)
                },
                io_handles: None,
                detached,
                // Replay path: we never observed the child handle's exit
                // in this process (the previous AFT process did, but its
                // observation didn't survive restart). Leave this false so
                // the second-pass reap falls through to the
                // `is_process_alive(child_pid)` probe rather than declaring
                // failure based on stale evidence.
                child_exit_observed: false,
                buffer: BgBuffer::registered(&paths, mode.clone()),
                terminal_output_cache: None,
                pending_terminal_override: None,
            }),
        });
        self.inner
            .tasks
            .lock()
            .map_err(|_| "background task registry lock poisoned".to_string())?
            .insert(task_id.clone(), Arc::clone(&task));
        // Re-arm durable pattern watches after the task is addressable again so
        // gap matches (bytes written while the bridge was down) are scanned and
        // pending undelivered matches are re-pushed.
        self.rearm_persisted_watches(&task);
        Ok(())
    }

    fn rearm_persisted_watches(&self, task: &Arc<BgTask>) {
        let Some((harness, pool)) = self.db_harness_and_pool() else {
            return;
        };
        let rows = {
            let Ok(conn) = pool.lock() else {
                return;
            };
            match crate::db::bash_watches::list_bash_pattern_watches_for_task(
                &conn,
                &harness,
                &task.session_id,
                &task.task_id,
            ) {
                Ok(rows) if !rows.is_empty() => rows,
                _ => return,
            }
        };

        let mode = match task.state.lock() {
            Ok(state) => state.metadata.mode.clone(),
            Err(_) => return,
        };
        let terminal = task
            .state
            .lock()
            .map(|state| state.metadata.status.is_terminal())
            .unwrap_or(false);
        let completion_delivered = task
            .state
            .lock()
            .map(|state| state.metadata.completion_delivered)
            .unwrap_or(true);

        let mut stdout = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stdout))
            .transpose()
            .ok()
            .flatten();
        let mut stderr = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stderr))
            .transpose()
            .ok()
            .flatten();
        let mut pty = (mode == BgMode::Pty)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Pty))
            .transpose()
            .ok()
            .flatten();

        let mut pending_to_emit = Vec::new();
        let mut gap_matches = Vec::new();
        {
            let Ok(mut registry) = self.inner.watch_registry.lock() else {
                return;
            };
            let stdout_key = format!("{}:stdout", task.task_id);
            let stderr_key = format!("{}:stderr", task.task_id);
            let pty_key = format!("{}:pty", task.task_id);

            // All rows for a task share stream cursors; take them from the first row.
            let first = &rows[0];
            match mode {
                BgMode::Pipes => {
                    registry.set_file_cursor(&stdout_key, first.stdout_offset.max(0) as u64);
                    registry.set_file_cursor(&stderr_key, first.stderr_offset.max(0) as u64);
                }
                BgMode::Pty => {
                    registry.set_file_cursor(&pty_key, first.pty_offset.max(0) as u64);
                }
            }

            for row in &rows {
                let Ok(pattern) = WatchPattern::from_persisted(&row.pattern_kind, &row.pattern)
                else {
                    crate::slog_warn!(
                        "skipping unreadable persisted watch {}/{}",
                        row.task_id,
                        row.watch_id
                    );
                    continue;
                };
                if let Err(error) = registry.restore(
                    row.watch_id.clone(),
                    row.task_id.clone(),
                    pattern,
                    row.once,
                    row.scanning,
                ) {
                    crate::slog_warn!(
                        "failed to restore watch {}/{}: {error}",
                        row.task_id,
                        row.watch_id
                    );
                    continue;
                }
                if row.pending_match {
                    if let (Some(match_text), Some(match_offset), Some(context)) = (
                        row.match_text.clone(),
                        row.match_offset,
                        row.match_context.clone(),
                    ) {
                        pending_to_emit.push(PatternMatch {
                            watch_id: row.watch_id.clone(),
                            task_id: row.task_id.clone(),
                            match_text,
                            match_offset: match_offset.max(0) as u64,
                            context,
                            once: row.once,
                        });
                    }
                }
            }

            // Gap scan: bytes written after the last persisted cursor while the
            // previous process was down. Skip when we already have a pending
            // once-match to re-deliver (avoids double-firing the same hit).
            let should_gap_scan =
                rows.iter().any(|row| row.scanning) && !pending_to_emit.iter().any(|m| m.once);
            if should_gap_scan {
                match mode {
                    BgMode::Pipes => {
                        if let (Some(stdout), Some(stderr)) = (stdout.as_mut(), stderr.as_mut()) {
                            gap_matches.extend(registry.scan_file_new_bytes(
                                &stdout_key,
                                &task.task_id,
                                stdout,
                            ));
                            gap_matches.extend(registry.scan_file_new_bytes(
                                &stderr_key,
                                &task.task_id,
                                stderr,
                            ));
                        }
                    }
                    BgMode::Pty => {
                        if let Some(pty) = pty.as_mut() {
                            gap_matches.extend(registry.scan_file_new_bytes(
                                &pty_key,
                                &task.task_id,
                                pty,
                            ));
                        }
                    }
                }
            }
        }

        let (stdout_offset, stderr_offset, pty_offset) = self.watch_stream_cursors(&task.task_id);
        for pattern_match in &gap_matches {
            self.persist_watch_match(
                &task.session_id,
                &task.task_id,
                pattern_match,
                stdout_offset,
                stderr_offset,
                pty_offset,
            );
        }
        if !gap_matches.is_empty() || rows.iter().any(|row| row.scanning) {
            self.persist_task_watch_cursors(
                &task.session_id,
                &task.task_id,
                stdout_offset,
                stderr_offset,
                pty_offset,
            );
        }

        // Prefer a single delivery: pending re-push first, else fresh gap matches.
        let emitted_pending = !pending_to_emit.is_empty();
        let to_emit = if emitted_pending {
            pending_to_emit
        } else {
            gap_matches
        };
        for pattern_match in to_emit {
            self.emit_bash_pattern_match(&task.delivery_session_id, pattern_match);
        }

        if !terminal {
            return;
        }

        // Terminal + watches: suppress the normal completion queue entry that
        // replay may have enqueued before re-arm, and mirror the live exit path.
        let _ = self.remove_pending_completion(&task.task_id);
        let (watch_controlled, watch_matched) = self.task_watch_state(&task.task_id);
        if !watch_controlled {
            return;
        }
        if watch_matched {
            // Pattern already covered delivery; do not also emit task_exit.
            return;
        }
        if completion_delivered {
            // Already acked before restart — drop durable rows and memory state.
            self.clear_task_watch_state(&task.task_id);
            self.delete_persisted_watches_for_task(&task.session_id, &task.task_id);
            return;
        }
        if let Some(completion) = self.completion_snapshot_for_task(task) {
            self.emit_bash_watch_exit(&completion);
        }
        // Keep durable watches until bash_ack_completions confirms delivery.
        self.clear_task_watch_state(&task.task_id);
    }

    fn kill_with_status(
        &self,
        task_id: &str,
        session_id: &str,
        terminal_status: BgTaskStatus,
    ) -> Result<BgTaskSnapshot, String> {
        self.kill_with_status_reason(task_id, session_id, terminal_status, None)
    }

    fn kill_with_status_reason(
        &self,
        task_id: &str,
        session_id: &str,
        terminal_status: BgTaskStatus,
        reason: Option<String>,
    ) -> Result<BgTaskSnapshot, String> {
        let task = self
            .task_for_session(task_id, session_id)
            .ok_or_else(|| format!("background task not found: {task_id}"))?;
        let mut terminalized = false;

        {
            let mut state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            if state.metadata.status.is_terminal() {
                state.pending_terminal_override = None;
            } else if let Ok(Some(marker)) = read_exit_marker(&task.paths) {
                state.metadata =
                    terminal_metadata_from_marker(state.metadata.clone(), marker, reason.clone());
                if self.task_has_watch_control(&task.task_id) {
                    state.metadata.completion_delivered = true;
                }
                state.pending_terminal_override = None;
                task.mark_terminal_now();
                match &mut state.runtime {
                    // Exit marker already present: the child finished on its
                    // own before this kill observed it. Reap it rather than
                    // dropping the handle so it doesn't become a zombie
                    // (issue #91). The active-kill branch below already
                    // `wait()`s after signaling, so this is the only kill
                    // path that needed the explicit reap.
                    TaskRuntime::Piped(child_slot) => reap_piped_child(child_slot),
                    TaskRuntime::Pty(runtime) => *runtime = None,
                }
                state.detached = true;
                self.persist_task(&task.paths, &state.metadata)
                    .map_err(|e| format!("failed to persist terminal state: {e}"))?;
                terminalized = true;
            } else {
                let was_already_killing = state.metadata.status == BgTaskStatus::Killing;
                if !was_already_killing {
                    state.metadata.status = BgTaskStatus::Killing;
                }
                if reason.is_some() {
                    state.metadata.status_reason = reason.clone();
                }
                if !was_already_killing || reason.is_some() {
                    self.persist_task(&task.paths, &state.metadata)
                        .map_err(|e| format!("failed to persist killing state: {e}"))?;
                }

                #[cfg(unix)]
                let pgid = state.metadata.pgid;
                #[cfg(windows)]
                let child_pid = state.metadata.child_pid;
                if !was_already_killing
                    && state.metadata.mode == BgMode::Pty
                    && terminal_status == BgTaskStatus::TimedOut
                {
                    state.pending_terminal_override = Some(BgTaskStatus::TimedOut);
                }

                #[cfg(windows)]
                let mut pty_forced_terminal_status: Option<BgTaskStatus> = None;

                match &mut state.runtime {
                    TaskRuntime::Piped(child_slot) => {
                        #[cfg(unix)]
                        if let Some(pgid) = pgid {
                            terminate_pgid(pgid, child_slot.as_mut());
                        }
                        #[cfg(windows)]
                        if let Some(child) = child_slot.as_mut() {
                            super::process::terminate_process(child);
                        } else if let Some(pid) = child_pid {
                            terminate_pid(pid);
                        }
                        if let Some(child) = child_slot.as_mut() {
                            let _ = child.wait();
                        }
                        *child_slot = None;
                        state.detached = true;

                        if let Some(handles) = state.io_handles.as_mut() {
                            handles.write(TaskArtifact::Exit, b"killed").map_err(|e| {
                                format!("failed to write retained kill marker: {e}")
                            })?;
                        } else {
                            write_kill_marker_if_absent(&task.paths)
                                .map_err(|e| format!("failed to write kill marker: {e}"))?;
                        }

                        let exit_code = terminal_exit_code_for_status(&terminal_status);
                        state
                            .metadata
                            .mark_terminal(terminal_status, exit_code, reason.clone());
                        if self.task_has_watch_control(&task.task_id) {
                            state.metadata.completion_delivered = true;
                        }
                        state.pending_terminal_override = None;
                        task.mark_terminal_now();
                        self.persist_task(&task.paths, &state.metadata)
                            .map_err(|e| format!("failed to persist killed state: {e}"))?;
                        terminalized = true;
                    }
                    TaskRuntime::Pty(Some(pty)) => {
                        pty.was_killed.store(true, Ordering::SeqCst);
                        if let Err(error) = pty.killer.kill() {
                            crate::slog_warn!(
                                "[pty-kill] {task_id} ChildKiller::kill failed: {error}"
                            );
                        }
                        if let Some(pid) = pty.child_pid {
                            #[cfg(unix)]
                            terminate_pgid(pid as i32, None);
                            #[cfg(windows)]
                            terminate_pid(pid);
                        }
                        drop(pty.master.take());

                        #[cfg(windows)]
                        {
                            let default_status = if terminal_status == BgTaskStatus::TimedOut {
                                BgTaskStatus::TimedOut
                            } else {
                                BgTaskStatus::Killed
                            };
                            pty_forced_terminal_status = Some(
                                state
                                    .pending_terminal_override
                                    .take()
                                    .unwrap_or(default_status),
                            );
                        }
                    }
                    TaskRuntime::Pty(None) => {}
                }

                #[cfg(windows)]
                if let Some(target_status) = pty_forced_terminal_status {
                    if !task.paths.exit.exists() {
                        write_kill_marker_if_absent(&task.paths)
                            .map_err(|e| format!("failed to write kill marker: {e}"))?;
                    }

                    let exit_code = terminal_exit_code_for_status(&target_status);
                    state
                        .metadata
                        .mark_terminal(target_status, exit_code, reason.clone());
                    if self.task_has_watch_control(&task.task_id) {
                        state.metadata.completion_delivered = true;
                    }
                    state.pending_terminal_override = None;
                    task.mark_terminal_now();
                    if let TaskRuntime::Pty(runtime) = &mut state.runtime {
                        *runtime = None;
                    }
                    state.detached = true;
                    self.persist_task(&task.paths, &state.metadata)
                        .map_err(|e| format!("failed to persist killed PTY state: {e}"))?;
                    terminalized = true;
                }
            }
        }

        if terminalized {
            self.post_terminal_transition(&task, true)?;
        }
        Ok(self.snapshot_with_terminal_cache(&task, RUNNING_OUTPUT_PREVIEW_BYTES))
    }

    fn finalize_from_marker(
        &self,
        task: &Arc<BgTask>,
        marker: ExitMarker,
        reason: Option<String>,
    ) -> Result<(), String> {
        let watch_controlled = self.task_has_watch_control(&task.task_id);
        let mut pty_reader_done = None;
        {
            let mut state = task
                .state
                .lock()
                .map_err(|_| "background task lock poisoned".to_string())?;
            if state.metadata.status.is_terminal() {
                state.pending_terminal_override = None;
                return Ok(());
            }

            let pending_override = state.pending_terminal_override.take();
            let is_pty = state.metadata.mode == BgMode::Pty;
            let reason = reason.or_else(|| state.metadata.status_reason.clone());
            let updated = self
                .update_task_metadata(&task.paths, |metadata| {
                    let mut new_metadata = if is_pty && marker == ExitMarker::Killed {
                        let mut metadata = metadata.clone();
                        let target_status = pending_override.unwrap_or(BgTaskStatus::Killed);
                        let exit_code = terminal_exit_code_for_status(&target_status);
                        metadata.mark_terminal(target_status, exit_code, reason.clone());
                        metadata
                    } else {
                        terminal_metadata_from_marker(metadata.clone(), marker, reason.clone())
                    };
                    if watch_controlled {
                        new_metadata.completion_delivered = true;
                    }
                    *metadata = new_metadata;
                })
                .map_err(|e| format!("failed to persist terminal state: {e}"))?;
            state.metadata = updated;
            task.mark_terminal_now();
            match &mut state.runtime {
                // Reap the exited direct child instead of dropping it, so it
                // does not linger as a `<defunct>` zombie (issue #91). The
                // wrapper writes the exit marker as its final act, so the
                // child is already exiting and `wait()` returns immediately.
                TaskRuntime::Piped(child_slot) => reap_piped_child(child_slot),
                TaskRuntime::Pty(runtime) => {
                    pty_reader_done = runtime
                        .as_ref()
                        .map(|runtime| Arc::clone(&runtime.reader_done));
                    *runtime = None;
                }
            }
            state.detached = true;
        }

        if let Some(reader_done) = pty_reader_done {
            let deadline = Instant::now() + Duration::from_millis(200);
            while !reader_done.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        // One final scan runs before terminal notification routing so bytes
        // printed immediately before exit can win over the exit safety net.
        self.scan_task_watch_output(task);

        self.post_terminal_transition(task, true)
    }

    fn enqueue_completion_if_needed(
        &self,
        metadata: &PersistedTask,
        paths: Option<&TaskPaths>,
        emit_frame: bool,
    ) {
        if metadata.status.is_terminal() && !metadata.completion_delivered {
            let cache =
                paths.and_then(|paths| self.render_terminal_output_from_paths(metadata, paths));
            self.enqueue_completion_from_parts(metadata, None, paths, emit_frame, cache.as_ref());
        }
    }

    fn render_terminal_output_from_paths(
        &self,
        metadata: &PersistedTask,
        paths: &TaskPaths,
    ) -> Option<TerminalOutputCache> {
        if metadata.mode == BgMode::Pty {
            return None;
        }
        let mut buffer = BgBuffer::registered(paths, BgMode::Pipes);
        let disk_truncation = buffer.enforce_terminal_cap();
        Some(self.render_terminal_output(metadata, &buffer, disk_truncation, Some(paths)))
    }

    fn enqueue_completion_from_parts(
        &self,
        metadata: &PersistedTask,
        buffer: Option<&BgBuffer>,
        paths: Option<&TaskPaths>,
        emit_frame: bool,
        terminal_render: Option<&TerminalOutputCache>,
    ) {
        // Only the terminal-state guard prevents double-recording here. The
        // `completion_delivered` flag is NOT used to gate compression-event
        // recording, because `mark_terminal` flips `completion_delivered=true`
        // immediately for tasks with `notify_on_completion=false` (foreground
        // bash polled via `bash_status`, which is the common case). Pre-emptive
        // delivery flagging is correct for the push-frame queue (suppresses
        // duplicate user-visible notifications) but would silently skip the
        // database insert below. Compression event recording is idempotent at
        // the DB layer (unique on harness+session+task_id), so re-entry is
        // safe; the dedupe-by-queue check stays for the push frame side.
        if !metadata.status.is_terminal() {
            return;
        }

        let owned_buffer = if buffer.is_none() && metadata.mode != BgMode::Pty {
            paths.map(|paths| BgBuffer::registered(paths, BgMode::Pipes))
        } else {
            None
        };
        let render_buffer = buffer.or(owned_buffer.as_ref());
        let owned_render = if terminal_render.is_none() {
            render_buffer.map(|buffer| {
                let mut capped_buffer = buffer.clone();
                let disk_truncation = capped_buffer.enforce_terminal_cap();
                self.render_terminal_output(metadata, &capped_buffer, disk_truncation, paths)
            })
        } else {
            None
        };
        let render = terminal_render.or(owned_render.as_ref());

        // Completion reminders use the already-rendered terminal output and a
        // smaller, exit-aware head+tail cap. They never invoke the compressor
        // themselves.
        let (mut output_preview, output_truncated) = render
            .map(|cache| completion_preview_for_cache(cache, metadata.exit_code))
            .unwrap_or_else(|| (String::new(), false));
        if metadata.status == BgTaskStatus::FateUnknown {
            if let Some(reason) = metadata.status_reason.as_deref() {
                output_preview = if output_preview.is_empty() {
                    reason.to_string()
                } else {
                    format!("{reason}\n{output_preview}")
                };
            }
        }

        let token_counts = self.completion_token_counts(
            metadata,
            buffer,
            paths,
            render.map(|render| render.output_preview.as_str()),
        );
        let completion = BgCompletion {
            task_id: metadata.task_id.clone(),
            session_id: metadata.session_id.clone(),
            status: metadata.status.clone(),
            exit_code: metadata.exit_code,
            command: metadata.command.clone(),
            output_preview,
            output_truncated,
            original_tokens: token_counts.original_tokens,
            compressed_tokens: token_counts.compressed_tokens,
            tokens_skipped: token_counts.tokens_skipped,
            status_reason: metadata.status_reason.clone(),
        };

        // Record the compression event BEFORE the push-frame dedupe. Event
        // recording has its own idempotency at the DB layer (unique key on
        // harness+session+task_id), so it's safe to attempt for every
        // terminal-state finalize. Critically, this path runs even when
        // `completion_delivered=true` was pre-set by `mark_terminal` for
        // foreground bash (`notify_on_completion=false`) — which is the common
        // case for OpenCode/Pi `bash` tool calls. Previously this code lived
        // after the dedupe guard and never fired for foreground tasks, which
        // meant compression accounting was effectively dead for >99% of
        // real-world bash usage.
        self.record_compression_event_if_applicable(metadata, &token_counts);

        let (watch_controlled, watch_matched) = self.task_watch_state(&metadata.task_id);
        if watch_controlled {
            if emit_frame && !watch_matched {
                self.emit_bash_watch_exit(&completion);
            } else if watch_matched {
                // Pattern match already notified the agent; mark completion
                // delivered so replay does not enqueue a duplicate bash_completed.
                // Durable once-watch rows with pending_match stay until ack/rearm
                // recovery so a lost push can still be re-delivered once.
                if let Some(task) = self.task(&metadata.task_id) {
                    let _ = task.set_completion_delivered(true, self);
                }
            }
            // Memory only — SQLite watch rows survive until ack, GC, or rearm settle.
            self.clear_task_watch_state(&metadata.task_id);
            return;
        }

        // Push-frame queue is gated on `completion_delivered` so foreground
        // bash with `notify_on_completion=false` does not leak a user-visible
        // completion notification. `mark_terminal` pre-sets
        // `completion_delivered=true` for those tasks; honoring it here keeps
        // the suppression invariant the test
        // `no_notify_foreground_poll_completion_does_not_enqueue_completion`
        // asserts. The compression-event recording above intentionally runs
        // before this gate so foreground bash still contributes to the
        // session/project aggregates.
        if metadata.completion_delivered {
            return;
        }

        // Push-frame queue dedupe stays per-task to prevent duplicate
        // user-visible completion notifications.
        let pushed = if let Ok(mut completions) = self.inner.completions.lock() {
            if completions
                .iter()
                .any(|existing| existing.task_id == metadata.task_id)
            {
                false
            } else {
                completions.push_back(completion.clone());
                true
            }
        } else {
            false
        };

        if pushed && emit_frame {
            self.emit_bash_completed(completion);
        }
    }

    fn record_compression_event_if_applicable(
        &self,
        metadata: &PersistedTask,
        token_counts: &CompletionTokenCounts,
    ) {
        if metadata.mode == BgMode::Pty {
            return;
        }

        let (original_tokens, compressed_tokens, original_bytes, compressed_bytes) = match (
            token_counts.original_tokens,
            token_counts.compressed_tokens,
            token_counts.original_bytes,
            token_counts.compressed_bytes,
        ) {
            (
                Some(original_tokens),
                Some(compressed_tokens),
                Some(original_bytes),
                Some(compressed_bytes),
            ) => (
                original_tokens,
                compressed_tokens,
                original_bytes,
                compressed_bytes,
            ),
            _ => {
                crate::slog_warn!(
                    "compression event skipped for {}: token counts unavailable (likely spill file missing or unreadable)",
                    metadata.task_id
                );
                return;
            }
        };

        let pool = self.inner.db_pool.read().ok().and_then(|slot| slot.clone());
        let Some(pool) = pool else {
            crate::slog_warn!(
                "compression event skipped for {}: db_pool not initialized — was configure run?",
                metadata.task_id
            );
            return;
        };
        let harness = self
            .inner
            .db_harness
            .read()
            .ok()
            .and_then(|slot| slot.clone());
        let Some(harness) = harness else {
            crate::slog_warn!(
                "compression event insert skipped for {}: harness not configured",
                metadata.task_id
            );
            return;
        };

        let project_root = metadata
            .project_root
            .as_deref()
            .unwrap_or(&metadata.workdir);
        let project_key = crate::path_identity::project_scope_key(project_root);
        let row = crate::db::compression_events::CompressionEventRow {
            harness: &harness,
            session_id: Some(&metadata.session_id),
            project_key: &project_key,
            tool: "bash",
            task_id: Some(&metadata.task_id),
            command: Some(&metadata.command),
            compressor: if metadata.compressed {
                "registry"
            } else {
                "none"
            },
            original_bytes,
            compressed_bytes,
            original_tokens,
            compressed_tokens,
            created_at: unix_millis() as i64,
        };

        let conn = match pool.lock() {
            Ok(conn) => conn,
            Err(_) => {
                crate::slog_warn!(
                    "compression event insert failed for {}: db mutex poisoned",
                    metadata.task_id
                );
                return;
            }
        };
        match crate::db::compression_events::insert_compression_event(&conn, &row) {
            Ok(Some(row_id)) => {
                // The database mutex remains held while the matching warm entries
                // advance, so status cannot observe the durable row without its
                // in-process aggregate delta.
                self.inner
                    .compression_aggregates
                    .record_successful_insert(&conn, &row, row_id);
                // DEBUG-level: each foreground bash call records one of these,
                // which clutters info-level logs without adding diagnostic value.
                // Aggregate totals are visible via the status RPC / TUI sidebar.
                crate::slog_debug!(
                    "compression event recorded for {} (project={}, session={}, {} → {} tokens)",
                    metadata.task_id,
                    project_key,
                    metadata.session_id,
                    original_tokens,
                    compressed_tokens
                );
            }
            Ok(None) => {
                crate::slog_debug!(
                    "duplicate compression event ignored for {} (project={}, session={})",
                    metadata.task_id,
                    project_key,
                    metadata.session_id
                );
            }
            Err(error) => {
                crate::slog_warn!(
                    "compression event insert failed for {}: {}",
                    metadata.task_id,
                    error
                );
            }
        }
    }

    fn emit_bash_pattern_match(&self, session_id: &str, pattern_match: PatternMatch) {
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::BashPatternMatch(BashPatternMatchFrame::new(
                pattern_match.task_id,
                session_id.to_string(),
                pattern_match.watch_id,
                pattern_match.match_text,
                pattern_match.match_offset,
                pattern_match.context,
                pattern_match.once,
            )));
        }
    }

    fn emit_bash_watch_erased(&self, session_id: &str, task_id: &str, watch_id: &str) {
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        let Some(sender) = progress_sender.as_ref() else {
            return;
        };
        sender(PushFrame::BashPatternMatch(
            BashPatternMatchFrame::watch_target_erased(
                task_id,
                session_id,
                watch_id,
                WATCH_TARGET_ERASED_TEXT,
                WATCH_TARGET_ERASED_CONTEXT,
            ),
        ));
    }

    fn emit_bash_watch_exit(&self, completion: &BgCompletion) {
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        let Some(sender) = progress_sender.as_ref() else {
            return;
        };
        let status = completion_status_text(&completion.status, completion.exit_code);
        let preview = completion.output_preview.trim_end();
        let context = if preview.is_empty() {
            format!("task {} exited ({status})", completion.task_id)
        } else {
            format!(
                "task {} exited ({status})
{preview}",
                completion.task_id
            )
        };
        sender(PushFrame::BashPatternMatch(
            BashPatternMatchFrame::task_exit(
                completion.task_id.clone(),
                completion.session_id.clone(),
                format!("exited ({status})"),
                context,
            ),
        ));
    }

    fn emit_bash_completed(&self, completion: BgCompletion) {
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        let Some(sender) = progress_sender.as_ref() else {
            return;
        };
        // Clone the callback out of the registry mutex before writing to stdout;
        // otherwise a blocked push-frame write could pin the mutex and starve
        // unrelated progress-sender updates.
        // Bg task transitions are discovered by the watchdog thread, so the
        // sender is shared behind a Mutex. It still uses the same stdout writer
        // closure as foreground progress frames, preserving the existing lock/
        // flush behavior in main.rs.
        let mut frame = BashCompletedFrame::new(
            completion.task_id,
            completion.session_id,
            completion.status,
            completion.exit_code,
            completion.command,
            completion.output_preview,
            completion.output_truncated,
            completion.original_tokens,
            completion.compressed_tokens,
            completion.tokens_skipped,
        );
        frame.status_reason = completion.status_reason;
        sender(PushFrame::BashCompleted(frame));
    }

    fn completion_token_counts(
        &self,
        metadata: &PersistedTask,
        buffer: Option<&BgBuffer>,
        paths: Option<&TaskPaths>,
        rendered_output: Option<&str>,
    ) -> CompletionTokenCounts {
        if metadata.mode == BgMode::Pty {
            return CompletionTokenCounts::skipped();
        }

        let raw = match buffer {
            Some(buffer) => buffer.read_for_token_count(TOKENIZE_CAP_BYTES_PER_STREAM),
            None => paths
                .map(|paths| {
                    read_for_token_count_from_disk(metadata, paths, TOKENIZE_CAP_BYTES_PER_STREAM)
                })
                .unwrap_or(TokenCountInput::Skipped),
        };

        let TokenCountInput::Text(raw_output) = raw else {
            return CompletionTokenCounts::skipped();
        };

        let original_tokens = token_count_u32(&raw_output);
        let original_bytes = raw_output.len() as i64;
        let compressed_output = rendered_output.unwrap_or(&raw_output);
        let compressed_tokens = token_count_u32(compressed_output);
        let compressed_bytes = compressed_output.len() as i64;
        CompletionTokenCounts {
            original_tokens: Some(original_tokens),
            compressed_tokens: Some(compressed_tokens),
            original_bytes: Some(original_bytes),
            compressed_bytes: Some(compressed_bytes),
            tokens_skipped: false,
        }
    }

    pub(crate) fn maybe_emit_long_running_reminder(&self, task: &Arc<BgTask>) {
        if !self
            .inner
            .long_running_reminder_enabled
            .load(Ordering::SeqCst)
        {
            return;
        }
        let interval_ms = self
            .inner
            .long_running_reminder_interval_ms
            .load(Ordering::SeqCst);
        if interval_ms == 0 {
            return;
        }
        let interval = Duration::from_millis(interval_ms);
        let now = Instant::now();
        let Ok(mut last_reminder_at) = task.last_reminder_at.lock() else {
            return;
        };
        let since = last_reminder_at.unwrap_or(task.started);
        if now.duration_since(since) < interval {
            return;
        }
        let command = task
            .state
            .lock()
            .map(|state| state.metadata.command.clone())
            .unwrap_or_default();
        *last_reminder_at = Some(now);
        self.emit_bash_long_running(BashLongRunningFrame::new(
            task.task_id.clone(),
            task.session_id.clone(),
            command,
            task.started.elapsed().as_millis() as u64,
        ));
    }

    fn emit_bash_long_running(&self, frame: BashLongRunningFrame) {
        let Ok(progress_sender) = self
            .inner
            .progress_sender
            .lock()
            .map(|sender| sender.clone())
        else {
            return;
        };
        if let Some(sender) = progress_sender.as_ref() {
            sender(PushFrame::BashLongRunning(frame));
        }
    }

    fn task(&self, task_id: &str) -> Option<Arc<BgTask>> {
        validate_task_id(task_id).ok()?;
        self.inner
            .tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(task_id).cloned())
    }

    fn task_for_session(&self, task_id: &str, session_id: &str) -> Option<Arc<BgTask>> {
        self.task(task_id)
            .filter(|task| task.session_id == session_id)
    }

    pub fn try_health_counts(&self) -> Option<BgTaskHealthCounts> {
        let running = self
            .inner
            .tasks
            .try_lock()
            .ok()
            .map(|tasks| tasks.values().filter(|task| task.is_running()).count())?;
        let pending_completions = self.inner.completions.try_lock().ok().map(|q| q.len())?;
        Some(BgTaskHealthCounts {
            running,
            pending_completions,
        })
    }

    /// Count background task PIDs that are still alive without keeping registry
    /// locks across the OS liveness probes used by the lifecycle health rollup.
    pub(crate) fn detached_live_process_count(&self) -> usize {
        let Some(pids) = self.inner.tasks.try_lock().ok().map(|tasks| {
            tasks
                .values()
                .filter_map(|task| {
                    task.state
                        .try_lock()
                        .ok()
                        .and_then(|state| state.metadata.child_pid)
                })
                .collect::<Vec<_>>()
        }) else {
            return 0;
        };
        pids.into_iter()
            .filter(|pid| is_process_alive(*pid))
            .count()
    }

    /// Estimate resident bash output caches without reading disk-backed task
    /// streams. Spill files are deliberately excluded because they do not
    /// occupy the daemon heap.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let tasks = match self.inner.tasks.try_lock() {
            Ok(tasks) => tasks.values().cloned().collect::<Vec<_>>(),
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };
        let mut bytes = 0u64;
        let mut terminal_output_caches = 0usize;
        let mut sessions = HashSet::new();
        for task in &tasks {
            sessions.insert(task.session_id.clone());
            let state = match task.state.try_lock() {
                Ok(state) => state,
                Err(_) => return crate::memory::MemoryEstimate::busy(),
            };
            if let Some(cache) = state.terminal_output_cache.as_ref() {
                terminal_output_caches = terminal_output_caches.saturating_add(1);
                bytes = bytes.saturating_add(terminal_output_cache_estimated_bytes(cache));
            }
        }
        let completion_count = match self.inner.completions.try_lock() {
            Ok(completions) => {
                for completion in completions.iter() {
                    sessions.insert(completion.session_id.clone());
                    bytes = bytes.saturating_add(completion_estimated_bytes(completion));
                }
                completions.len()
            }
            Err(_) => return crate::memory::MemoryEstimate::busy(),
        };

        crate::memory::MemoryEstimate::estimated(bytes)
            .count("tasks", tasks.len())
            .count("sessions", sessions.len())
            .count("terminal_output_caches", terminal_output_caches)
            .count("completion_caches", completion_count)
            .count_u64("output_ring_bytes", 0)
    }

    fn running_count(&self) -> usize {
        self.inner
            .tasks
            .lock()
            .map(|tasks| tasks.values().filter(|task| task.is_running()).count())
            .unwrap_or(0)
    }

    fn start_watchdog(&self) {
        if !self.inner.watchdog_started.swap(true, Ordering::SeqCst) {
            super::watchdog::start(self.clone());
        }
    }

    #[cfg(test)]
    pub fn task_json_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf> {
        self.task_for_session(task_id, session_id)
            .map(|task| task.paths.json.clone())
    }

    #[cfg(test)]
    pub fn task_exit_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf> {
        self.task_for_session(task_id, session_id)
            .map(|task| task.paths.exit.clone())
    }
}

#[cfg(unix)]
fn should_capture_pipeline_status(
    spawn_plan: &SpawnPlan,
    has_pipeline: bool,
    shell: &Path,
) -> bool {
    if spawn_plan.is_native_launcher() {
        // Landlock closes fd 5 and above before exec; sandboxed tasks therefore
        // cannot safely pass CHILD_PIPE_STATUS_FD to the payload wrapper.
        return false;
    }
    has_pipeline && super::process::pipeline_shell_kind(shell).is_some()
}

fn canonical_artifact_root(paths: &TaskPaths) -> PathBuf {
    fs::canonicalize(&paths.io_dir).unwrap_or_else(|_| paths.io_dir.clone())
}

fn restart_fate_unknown_reason(metadata: &PersistedTask, paths: &TaskPaths) -> String {
    let output = match metadata.mode {
        BgMode::Pipes => &paths.stdout,
        BgMode::Pty => &paths.pty,
    };
    format!(
        "task {}: daemon restarted, process fate unknown, last output at {}",
        metadata.task_id,
        output.display()
    )
}

/// Append the pipeline-failure note after compression and output capping. The
/// note is intentionally derived from the same scanner used before spawning;
/// the status file only supplies numeric results for those already-known
/// segments.
fn append_pipeline_warning(
    cache: &mut TerminalOutputCache,
    metadata: &PersistedTask,
    paths: Option<&TaskPaths>,
) {
    if metadata.exit_code != Some(0) {
        return;
    }
    let Some(paths) = paths else {
        return;
    };
    if metadata.pipeline_segments.len() < 2 {
        return;
    }
    let Ok(mut status_file) = open_task_artifact(paths, TaskArtifact::PipelineStatus) else {
        return;
    };
    let Ok(status_bytes) = status_file.read_all() else {
        return;
    };
    let Some(statuses) = String::from_utf8_lossy(&status_bytes)
        .lines()
        .map(|line| line.trim().parse::<i32>().ok())
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    if statuses.len() != metadata.pipeline_segments.len() {
        return;
    }
    let Some((failing_index, failing_code)) = statuses
        .iter()
        .enumerate()
        .take(statuses.len().saturating_sub(1))
        .find(|(_, code)| **code != 0)
        .map(|(index, code)| (index, *code))
    else {
        return;
    };
    let Some(final_segment) = metadata.pipeline_segments.last() else {
        return;
    };
    let failing_segment = &metadata.pipeline_segments[failing_index];
    let footer = format!(
        "note: `{}` (segment {} of {}) exited {}; the pipeline's exit code is `{}`'s.",
        failing_segment,
        failing_index + 1,
        metadata.pipeline_segments.len(),
        failing_code,
        final_segment,
    );
    if cache.output_preview.trim().is_empty() {
        cache.output_preview = footer;
    } else {
        cache.output_preview = format!("{}\n{}", cache.output_preview.trim_end(), footer,);
    }
}

/// Normalize pipes for text rendering without changing their byte-exact artifacts.
/// PTY output bypasses this path because its vt100 renderer owns terminal state.
fn normalize_piped_display_output(text: &mut String) {
    if !text.contains('\r') {
        return;
    }

    let mut rendered = String::with_capacity(text.len());
    let mut line = Vec::new();
    let mut column = 0;
    let mut chars = text.chars().peekable();

    while let Some(character) = chars.next() {
        match character {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                for character in &line {
                    rendered.push(*character);
                }
                rendered.push('\n');
                line.clear();
                column = 0;
            }
            '\r' => column = 0,
            '\n' => {
                for character in &line {
                    rendered.push(*character);
                }
                rendered.push('\n');
                line.clear();
                column = 0;
            }
            character => {
                if column < line.len() {
                    line[column] = character;
                } else {
                    line.resize(column, ' ');
                    line.push(character);
                }
                column += 1;
            }
        }
    }

    for character in &line {
        rendered.push(*character);
    }
    *text = rendered;
}

fn render_compressed_with_recovery(
    buffer: &BgBuffer,
    mut compressed: CompressionResult,
    input_truncated: bool,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> TerminalOutputCache {
    // Preserve a single canonical trailing newline. A bare `.trim_end()` strips
    // the legitimate final newline that `echo` and most commands emit, so
    // agent-facing output diverged from native bash ("hello" vs "hello\n") and
    // broke the no-JSON-envelope contract. Collapse excess trailing blank lines
    // to one, but keep that one when the content had a trailing newline. NOTE:
    // the check must read the ORIGINAL text — strip_plain_truncation_marker_lines
    // rebuilds via `.lines().join("\n")`, which itself drops the trailing newline.
    let had_trailing_newline = compressed.text.ends_with('\n');
    let mut text = strip_plain_truncation_marker_lines(&compressed.text)
        .trim_end()
        .to_string();
    if had_trailing_newline && !text.is_empty() {
        text.push('\n');
    }
    compressed.text = text;

    let output_path = buffer.output_path().map(|path| path.display().to_string());
    let stderr_path = buffer.stderr_path().map(|path| path.display().to_string());
    let include_stderr_path = buffer.stream_len(StreamKind::Stderr) > 0;
    let mut recovery = RecoveryContext {
        dropped_by_class: compressed.dropped_by_class,
        had_inner_drop: compressed.had_inner_drop,
        offset_hint_eligible: compressed.offset_hint_eligible,
        offset_start_line: compressed.offset_start_line,
        byte_truncated: input_truncated,
        disk_truncated_prefix_bytes: disk_truncation.total_prefix_bytes(),
        output_path: output_path.clone(),
        stderr_path: stderr_path.clone(),
        include_stderr_path,
        artifact_access: artifact_access.clone(),
    };

    let (output_preview, output_truncated) =
        render_body_with_recovery_marker(&compressed.text, &mut recovery);
    TerminalOutputCache {
        output_preview,
        output_truncated,
        kind: TerminalOutputKind::Compressed,
        output_path,
        stderr_path,
        artifact_access,
        recovery: Some(recovery),
    }
}

fn render_body_with_recovery_marker(body: &str, recovery: &mut RecoveryContext) -> (String, bool) {
    render_body_with_recovery_marker_at_cap(
        body,
        recovery,
        FINAL_OUTPUT_CAP_BYTES,
        cap_final_output,
        cap_final_output_with_marker,
    )
}

fn render_raw_body_with_recovery_marker(
    body: &str,
    recovery: &mut RecoveryContext,
) -> (String, bool) {
    render_body_with_recovery_marker_at_cap(
        body,
        recovery,
        RAW_PASSTHROUGH_CAP_BYTES,
        |input| {
            super::output::cap_head_tail(
                input,
                RAW_PASSTHROUGH_CAP_BYTES,
                RAW_PASSTHROUGH_HEAD_BYTES,
                RAW_PASSTHROUGH_TAIL_BYTES,
            )
        },
        |input, marker| {
            super::output::cap_head_tail_with_marker(
                input,
                RAW_PASSTHROUGH_CAP_BYTES,
                RAW_PASSTHROUGH_HEAD_BYTES,
                RAW_PASSTHROUGH_TAIL_BYTES,
                marker,
            )
        },
    )
}

fn render_body_with_recovery_marker_at_cap<F, G>(
    body: &str,
    recovery: &mut RecoveryContext,
    cap_bytes: usize,
    cap_plain: F,
    cap_with_marker: G,
) -> (String, bool)
where
    F: Fn(&str) -> super::output::CappedText,
    G: Fn(&str, &str) -> super::output::CappedText,
{
    let needs_marker = recovery.has_visible_drop();
    if body.len() > cap_bytes {
        recovery.byte_truncated = true;
        if let Some(marker) = recovery_marker(recovery) {
            let capped = cap_with_marker(body, &marker);
            return (capped.text, true);
        }
        let capped = cap_plain(body);
        return (capped.text, capped.truncated || needs_marker);
    }

    if !needs_marker {
        return (body.to_string(), false);
    }

    let Some(marker) = recovery_marker(recovery) else {
        return (body.to_string(), true);
    };
    let with_marker = append_recovery_marker(body, &marker);
    if with_marker.len() <= cap_bytes {
        return (with_marker, true);
    }

    recovery.byte_truncated = true;
    let marker = recovery_marker(recovery).unwrap_or(marker);
    let capped = cap_with_marker(body, &marker);
    (capped.text, true)
}

fn append_recovery_marker(body: &str, marker: &str) -> String {
    if body.is_empty() {
        return marker.to_string();
    }
    let mut output = body.trim_end().to_string();
    output.push('\n');
    output.push_str(marker);
    output
}

fn recovery_marker(recovery: &RecoveryContext) -> Option<String> {
    let mut parts = Vec::new();
    for (class, count) in &recovery.dropped_by_class {
        let label = if *count == 1 {
            class.singular()
        } else {
            class.plural()
        };
        parts.push(format!("+{count} more {label}"));
    }
    if recovery.byte_truncated {
        parts.push("truncated output".to_string());
    }
    let disk_truncated_prefix_bytes = recovery.disk_truncated_prefix_bytes;
    if disk_truncated_prefix_bytes > 0 {
        parts.push(format!(
            "truncated {disk_truncated_prefix_bytes} bytes from saved output prefix"
        ));
    } else if recovery.had_inner_drop && parts.is_empty() {
        parts.push("omitted output".to_string());
    }

    if parts.is_empty() {
        return None;
    }

    let hint = recovery_hint(recovery);
    Some(format!("[{}; {hint}]", parts.join(", ")))
}

fn bash_status_recovery_hint(access: &ArtifactRecoveryAccess) -> String {
    let task_id = serde_json::to_string(&access.task_id)
        .unwrap_or_else(|_| format!("\"{}\"", access.task_id));
    format!("use bash_status({{taskId: {task_id}}})")
}

fn recovery_hint(recovery: &RecoveryContext) -> String {
    if !recovery.artifact_access.readable {
        return bash_status_recovery_hint(&recovery.artifact_access);
    }

    // AFT stores stdout/stderr separately and combines them in memory. Class caps,
    // middle truncation, and mixed stdout/stderr renders are not line-offset
    // portable. Only a single-file contiguous-prefix drop may use `tail -n +N`.
    if recovery.offset_hint_eligible
        && !recovery.byte_truncated
        && recovery.dropped_by_class.is_empty()
        && !recovery.include_stderr_path
    {
        if let (Some(path), Some(line)) =
            (recovery.output_path.as_deref(), recovery.offset_start_line)
        {
            return format!("see remaining: tail -n +{line} {}", quote_path(path));
        }
    }

    let mut paths = Vec::new();
    if let Some(path) = recovery.output_path.as_deref() {
        paths.push(path);
    }
    if recovery.include_stderr_path {
        if let Some(path) = recovery.stderr_path.as_deref() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }

    if paths.is_empty() {
        return "full output unavailable".to_string();
    }

    let reads = paths
        .into_iter()
        .map(|path| format!("read {}", quote_path(path)))
        .collect::<Vec<_>>()
        .join(" and ");
    if recovery.disk_truncated_prefix_bytes > 0 {
        format!("retained output: {reads}")
    } else {
        format!("full output: {reads}")
    }
}

fn strip_plain_truncation_marker_lines(input: &str) -> String {
    input
        .lines()
        .filter(|line| !is_plain_truncation_marker(line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_recovery_marker_lines(input: &str) -> String {
    input
        .lines()
        .filter(|line| !is_recovery_marker(line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_plain_truncation_marker(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("...<truncated ") else {
        return false;
    };
    let Some(bytes) = rest.strip_suffix(" bytes>...") else {
        return false;
    };
    !bytes.is_empty() && bytes.chars().all(|ch| ch.is_ascii_digit())
}

fn is_recovery_marker(line: &str) -> bool {
    line.starts_with('[')
        && line.ends_with(']')
        && (line.contains("full output: read ")
            || line.contains("retained output: read ")
            || line.contains("see remaining: tail -n +")
            || line.contains("use bash_status({taskId:")
            || line.contains("full output unavailable"))
}

fn structured_output_pointer(
    total_bytes: u64,
    output_path: &str,
    truncated_prefix_bytes: u64,
    artifact_access: &ArtifactRecoveryAccess,
) -> String {
    if artifact_access.readable {
        return if truncated_prefix_bytes > 0 {
            retained_json_output_pointer(total_bytes, output_path, truncated_prefix_bytes)
        } else {
            json_output_pointer(total_bytes, output_path)
        };
    }

    let kb = total_bytes.div_ceil(1024);
    let hint = bash_status_recovery_hint(artifact_access);
    if truncated_prefix_bytes > 0 {
        format!(
            "[JSON output {kb} KB; truncated {truncated_prefix_bytes} bytes from saved output prefix; retained output: {hint}]"
        )
    } else {
        format!("[JSON output {kb} KB; full output: {hint}]")
    }
}

fn render_structured_output(
    command: &str,
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> Option<TerminalOutputCache> {
    if !is_gh_structured_command(command) {
        return None;
    }

    let output_path = buffer
        .output_path()
        .map(|path| path.display().to_string())?;
    let stdout_bytes = buffer.stream_len(StreamKind::Stdout);
    if stdout_bytes == 0 {
        return None;
    }

    if stdout_bytes > STRUCTURED_OUTPUT_CAP_BYTES as u64 {
        if !stream_starts_like_json(buffer, StreamKind::Stdout) {
            return None;
        }
        let output_preview = structured_output_pointer(
            stdout_bytes,
            &output_path,
            disk_truncation.total_prefix_bytes(),
            &artifact_access,
        );
        return Some(TerminalOutputCache {
            output_preview,
            output_truncated: true,
            kind: TerminalOutputKind::Structured,
            output_path: Some(output_path),
            stderr_path: buffer.stderr_path().map(|path| path.display().to_string()),
            artifact_access,
            recovery: None,
        });
    }

    let stdout = buffer.read_stream_bounded(StreamKind::Stdout, STRUCTURED_OUTPUT_CAP_BYTES);
    if stdout.truncated || !is_structured_body(&stdout.text) {
        return None;
    }

    Some(TerminalOutputCache {
        output_preview: stdout.text,
        output_truncated: false,
        kind: TerminalOutputKind::Structured,
        output_path: Some(output_path),
        stderr_path: buffer.stderr_path().map(|path| path.display().to_string()),
        artifact_access,
        recovery: None,
    })
}

fn render_raw_passthrough(
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> TerminalOutputCache {
    let raw = buffer.read_combined_head_tail(
        RAW_PASSTHROUGH_CAP_BYTES,
        RAW_PASSTHROUGH_HEAD_BYTES,
        RAW_PASSTHROUGH_TAIL_BYTES,
    );
    let output_path = buffer.output_path().map(|path| path.display().to_string());
    let stderr_path = buffer.stderr_path().map(|path| path.display().to_string());
    if !raw.truncated && disk_truncation.total_prefix_bytes() == 0 {
        return TerminalOutputCache {
            output_preview: raw.text,
            output_truncated: false,
            kind: TerminalOutputKind::Raw,
            output_path,
            stderr_path,
            artifact_access,
            recovery: None,
        };
    }

    let include_stderr_path = buffer.stream_len(StreamKind::Stderr) > 0;
    let mut recovery = RecoveryContext {
        dropped_by_class: BTreeMap::new(),
        had_inner_drop: false,
        offset_hint_eligible: false,
        offset_start_line: None,
        byte_truncated: raw.truncated,
        disk_truncated_prefix_bytes: disk_truncation.total_prefix_bytes(),
        output_path: output_path.clone(),
        stderr_path: stderr_path.clone(),
        include_stderr_path,
        artifact_access: artifact_access.clone(),
    };
    let (output_preview, output_truncated) =
        render_raw_body_with_recovery_marker(&raw.text, &mut recovery);
    TerminalOutputCache {
        output_preview,
        output_truncated,
        kind: TerminalOutputKind::Raw,
        output_path,
        stderr_path,
        artifact_access,
        recovery: Some(recovery),
    }
}

fn completion_preview_for_cache(
    cache: &TerminalOutputCache,
    exit_code: Option<i32>,
) -> (String, bool) {
    // Reminder previews are sized by exit status: success gets a short tail,
    // failure keeps head+tail context (see output.rs completion caps).
    let exit_ok = exit_code == Some(0);
    let threshold = completion_preview_threshold(exit_ok);
    if cache.kind == TerminalOutputKind::Structured && cache.output_preview.len() > threshold {
        if let Some(path) = cache.output_path.as_deref() {
            return (
                structured_output_pointer(
                    cache.output_preview.len() as u64,
                    path,
                    0,
                    &cache.artifact_access,
                ),
                true,
            );
        }
        return (cache.output_preview.clone(), cache.output_truncated);
    }

    if let Some(recovery) = cache.recovery.as_ref() {
        if cache.output_preview.len() <= threshold {
            return (cache.output_preview.clone(), cache.output_truncated);
        }
        let body = strip_recovery_marker_lines(&cache.output_preview);
        let mut completion_recovery = recovery.clone();
        completion_recovery.byte_truncated = true;
        if let Some(marker) = recovery_marker(&completion_recovery) {
            let capped = cap_completion_output_with_marker(&body, &marker, exit_ok);
            return (capped.text, true);
        }
    }

    let capped = cap_completion_output(&cache.output_preview, exit_ok);
    (capped.text, cache.output_truncated || capped.truncated)
}

fn is_gh_structured_command(command: &str) -> bool {
    let Some(normalized) = crate::compress::plain_command_for_structured_output(command) else {
        return false;
    };
    let tokens = shell_words_for_flags(&normalized);
    let Some(head) = tokens.first() else {
        return false;
    };
    let head_name = Path::new(head)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(head);
    if !(head_name == "gh" || head_name.eq_ignore_ascii_case("gh.exe")) {
        return false;
    }
    tokens.iter().any(|token| {
        matches!(token.as_str(), "--json" | "--jq" | "--template")
            || token.starts_with("--json=")
            || token.starts_with("--jq=")
            || token.starts_with("--template=")
    })
}

fn shell_words_for_flags(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if matches!(ch, ';' | '&' | '|') && !in_single && !in_double {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn is_structured_body(body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return false;
    }
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return true;
    }

    let mut saw_line = false;
    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        saw_line = true;
        if serde_json::from_str::<serde_json::Value>(line).is_err() {
            return false;
        }
    }
    saw_line
}

fn stream_starts_like_json(buffer: &BgBuffer, stream: StreamKind) -> bool {
    buffer
        .read_stream_bounded(stream, 512)
        .text
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| matches!(ch, '{' | '[' | '"' | '-' | '0'..='9' | 't' | 'f' | 'n'))
}

struct CompletionTokenCounts {
    original_tokens: Option<u32>,
    compressed_tokens: Option<u32>,
    original_bytes: Option<i64>,
    compressed_bytes: Option<i64>,
    tokens_skipped: bool,
}

impl CompletionTokenCounts {
    fn skipped() -> Self {
        Self {
            original_tokens: None,
            compressed_tokens: None,
            original_bytes: None,
            compressed_bytes: None,
            tokens_skipped: true,
        }
    }
}

fn completion_status_text(status: &BgTaskStatus, exit_code: Option<i32>) -> String {
    match status {
        BgTaskStatus::TimedOut => "timed out".to_string(),
        BgTaskStatus::Killed => "killed".to_string(),
        _ => exit_code
            .map(|code| format!("exit {code}"))
            .unwrap_or_else(|| format!("{status:?}").to_lowercase()),
    }
}

fn token_count_u32(text: &str) -> u32 {
    aft_tokenizer::count_tokens(text)
        .try_into()
        .unwrap_or(u32::MAX)
}

impl Default for BgTaskRegistry {
    fn default() -> Self {
        Self::new(Arc::new(Mutex::new(None)))
    }
}

fn modified_within(path: &Path, grace: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| age < grace)
        .unwrap_or(false)
}

fn canonicalized_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn started_instant_from_unix_millis(started_at: u64) -> Instant {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(started_at);
    let elapsed_ms = now_ms.saturating_sub(started_at);
    Instant::now()
        .checked_sub(Duration::from_millis(elapsed_ms))
        .unwrap_or_else(Instant::now)
}

fn gc_quarantine(storage_dir: &Path) {
    let quarantine_root = storage_dir.join("bash-tasks-quarantine");
    let Ok(session_dirs) = fs::read_dir(&quarantine_root) else {
        return;
    };
    for session_entry in session_dirs.flatten() {
        let session_quarantine_dir = session_entry.path();
        if !session_quarantine_dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(&session_quarantine_dir) {
            Ok(entries) => entries,
            Err(error) => {
                crate::slog_warn!(
                    "failed to read background task quarantine dir {}: {error}",
                    session_quarantine_dir.display()
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if modified_within(&path, QUARANTINE_GC_GRACE) {
                continue;
            }
            let result = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match result {
                Ok(()) => log::debug!(
                    "deleted old background task quarantine entry {}",
                    path.display()
                ),
                Err(error) => crate::slog_warn!(
                    "failed to delete old background task quarantine entry {}: {error}",
                    path.display()
                ),
            }
        }
        let _ = fs::remove_dir(&session_quarantine_dir);
    }
    let _ = fs::remove_dir(&quarantine_root);
}

fn read_for_token_count_from_disk(
    metadata: &PersistedTask,
    paths: &TaskPaths,
    max_bytes_per_stream: usize,
) -> TokenCountInput {
    if metadata.mode == BgMode::Pty {
        return TokenCountInput::Skipped;
    }
    // Read up to `max_bytes_per_stream` bytes per stream rather than
    // refusing to tokenize anything when the file exceeds the cap.
    // Mirror the in-memory `BgBuffer::read_for_token_count` policy
    // (see comment there) — large outputs are exactly the tasks that
    // benefit most from compression accounting, so silent-skipping
    // them defeats the purpose of token tracking.
    let stdout = read_file_tail_capped(paths, TaskArtifact::Stdout, max_bytes_per_stream);
    let stderr = read_file_tail_capped(paths, TaskArtifact::Stderr, max_bytes_per_stream);
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => TokenCountInput::Text(combine_streams(
            String::from_utf8_lossy(&stdout).as_ref(),
            String::from_utf8_lossy(&stderr).as_ref(),
        )),
        (Ok(stdout), Err(_)) => TokenCountInput::Text(combine_streams(
            String::from_utf8_lossy(&stdout).as_ref(),
            "",
        )),
        (Err(_), Ok(stderr)) => TokenCountInput::Text(combine_streams(
            "",
            String::from_utf8_lossy(&stderr).as_ref(),
        )),
        (Err(_), Err(_)) => TokenCountInput::Skipped,
    }
}

fn read_file_tail_capped(
    paths: &TaskPaths,
    artifact: TaskArtifact,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut file = open_task_artifact(paths, artifact)?;
    file.tail(max_bytes).map(|(bytes, _)| bytes)
}

fn task_bundle_is_absent(storage_dir: &Path, session_id: &str, task_id: &str) -> bool {
    let session_dir = session_tasks_dir(storage_dir, session_id);
    !session_dir.join(task_id).exists() && !session_dir.join(format!("{task_id}.json")).exists()
}

fn terminal_db_row_snapshot(row: BashTaskRow, metadata: PersistedTask) -> BgTaskSnapshot {
    let existing_path = |path: Option<String>| {
        path.filter(|path| {
            fs::metadata(path)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        })
    };
    let duration_ms = metadata.duration_ms.or_else(|| {
        metadata
            .finished_at
            .map(|finished_at| finished_at.saturating_sub(metadata.started_at))
    });
    BgTaskSnapshot {
        info: BgTaskInfo {
            task_id: metadata.task_id,
            status: metadata.status,
            command: metadata.command,
            mode: metadata.mode.clone(),
            started_at: metadata.started_at,
            duration_ms,
            status_reason: metadata.status_reason,
        },
        exit_code: metadata.exit_code,
        child_pid: metadata.child_pid,
        workdir: metadata.workdir.display().to_string(),
        output_preview: String::new(),
        output_truncated: false,
        output_path: existing_path(row.stdout_path),
        stderr_path: existing_path(row.stderr_path),
        pty_rows: (metadata.mode == BgMode::Pty).then_some(metadata.pty_rows.unwrap_or(24)),
        pty_cols: (metadata.mode == BgMode::Pty).then_some(metadata.pty_cols.unwrap_or(80)),
        pty_screen: None,
        scanner_report: metadata.scanner_report,
        sandbox_native: metadata.sandbox_native,
        sandbox_unavailable: false,
    }
}

impl BgTask {
    fn snapshot(&self, preview_bytes: usize) -> BgTaskSnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.snapshot_locked(&state, preview_bytes)
    }

    fn snapshot_locked(&self, state: &BgTaskState, preview_bytes: usize) -> BgTaskSnapshot {
        let metadata = &state.metadata;
        let duration_ms = metadata.duration_ms.or_else(|| {
            metadata
                .status
                .is_terminal()
                .then(|| self.started.elapsed().as_millis() as u64)
        });
        let (output_preview, output_truncated) = if metadata.mode == BgMode::Pty {
            (String::new(), false)
        } else if metadata.status.is_terminal() {
            state
                .terminal_output_cache
                .as_ref()
                .map(|cache| (cache.output_preview.clone(), cache.output_truncated))
                .unwrap_or_else(|| (String::new(), false))
        } else if preview_bytes == 0 {
            (String::new(), false)
        } else {
            state.buffer.read_tail(preview_bytes)
        };
        BgTaskSnapshot {
            info: BgTaskInfo {
                task_id: self.task_id.clone(),
                status: metadata.status.clone(),
                command: metadata.command.clone(),
                mode: metadata.mode.clone(),
                started_at: metadata.started_at,
                duration_ms,
                status_reason: metadata.status_reason.clone(),
            },
            exit_code: metadata.exit_code,
            child_pid: metadata.child_pid,
            workdir: metadata.workdir.display().to_string(),
            output_preview,
            output_truncated,
            output_path: state
                .buffer
                .output_path()
                .map(|path| path.display().to_string()),
            stderr_path: state
                .buffer
                .stderr_path()
                .map(|path| path.display().to_string()),
            pty_rows: (metadata.mode == BgMode::Pty).then_some(metadata.pty_rows.unwrap_or(24)),
            pty_cols: (metadata.mode == BgMode::Pty).then_some(metadata.pty_cols.unwrap_or(80)),
            pty_screen: None,
            scanner_report: metadata.scanner_report.clone(),
            sandbox_native: metadata.sandbox_native,
            sandbox_unavailable: metadata.sandbox_native
                && open_task_artifact(&self.paths, TaskArtifact::SandboxUnavailable)
                    .and_then(|mut file| file.read_all())
                    .is_ok_and(|bytes| bytes == b"sandbox_unavailable"),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.state
            .lock()
            .map(|state| {
                state.metadata.status == BgTaskStatus::Running
                    || (state.metadata.mode == BgMode::Pty
                        && state.metadata.status == BgTaskStatus::Killing)
            })
            .unwrap_or(false)
    }

    fn is_terminal(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.metadata.status.is_terminal())
            .unwrap_or(false)
    }

    fn mark_terminal_now(&self) {
        if let Ok(mut terminal_at) = self.terminal_at.lock() {
            if terminal_at.is_none() {
                *terminal_at = Some(Instant::now());
            }
        }
    }

    fn set_completion_delivered(
        &self,
        delivered: bool,
        registry: &BgTaskRegistry,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "background task lock poisoned".to_string())?;
        let updated = registry
            .update_task_metadata(&self.paths, |metadata| {
                metadata.completion_delivered = delivered;
            })
            .map_err(|e| format!("failed to update completion delivery: {e}"))?;
        state.metadata = updated;
        Ok(())
    }
}

/// Reap an exited direct child handle, then clear the slot.
///
/// Dropping a [`std::process::Child`] does NOT `wait()` on the underlying OS
/// process. On Unix a finished-but-unreaped child lingers as a `<defunct>`
/// zombie until the AFT process itself exits (issue #91: `[mv] <defunct>`).
/// The terminal-transition paths that learn of completion from the
/// exit-marker file — rather than from [`BgTaskRegistry::reap_child`]'s
/// `try_wait()` — must therefore reap the handle explicitly instead of just
/// nulling it.
///
/// The exit marker is written by the wrapper's final statement (an atomic
/// `mv` rename), so by the time we observe the marker the direct child has
/// finished its work and is exiting; `wait()` returns essentially
/// immediately. We attempt a non-blocking `try_wait()` first so the common
/// case never blocks at all, falling back to a (bounded) `wait()` only to
/// cover the microsecond window between the rename and process teardown.
///
/// Callers hold the task state mutex, so this is serialized against
/// `reap_child` — there is no double-`wait()` hazard: whichever path acquires
/// the lock first reaps and clears the slot, and the other observes `None`.
#[cfg(unix)]
fn reap_piped_child(child_slot: &mut Option<Child>) {
    if let Some(mut child) = child_slot.take() {
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.wait();
        }
    }
}

/// Windows has no zombie/`<defunct>` concept: dropping the [`Child`] closes
/// the process handle, which is the correct release. Preserve the historical
/// behavior of simply clearing the slot so the documented Windows PID-recycle
/// handling in `reap_child` is unaffected.
#[cfg(windows)]
fn reap_piped_child(child_slot: &mut Option<Child>) {
    *child_slot = None;
}

fn terminal_metadata_from_marker(
    mut metadata: PersistedTask,
    marker: ExitMarker,
    reason: Option<String>,
) -> PersistedTask {
    match marker {
        ExitMarker::Code(code) => {
            let status = if code == 0 {
                BgTaskStatus::Completed
            } else {
                BgTaskStatus::Failed
            };
            metadata.mark_terminal(status, Some(code), reason);
        }
        ExitMarker::Killed => metadata.mark_terminal(
            BgTaskStatus::Killed,
            terminal_exit_code_for_status(&BgTaskStatus::Killed),
            reason,
        ),
    }
    metadata
}

fn terminal_exit_code_for_status(status: &BgTaskStatus) -> Option<i32> {
    match status {
        BgTaskStatus::TimedOut => Some(124),
        BgTaskStatus::Killed => Some(137),
        _ => None,
    }
}

fn attach_sandbox_metadata(metadata: &mut PersistedTask, spawn_plan: &SpawnPlan) {
    metadata.sandbox_native = spawn_plan.is_native_launcher();
    metadata.sandbox_temp_dir = spawn_plan.temp_dir().map(Path::to_path_buf);
}

#[cfg(unix)]
pub(crate) fn resolve_posix_shell() -> PathBuf {
    static POSIX_SHELL: OnceLock<PathBuf> = OnceLock::new();
    POSIX_SHELL
        .get_or_init(|| {
            std::env::var_os("BASH")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.exists())
                .or_else(|| which::which("bash").ok())
                .or_else(|| which::which("zsh").ok())
                .unwrap_or_else(|| PathBuf::from("/bin/sh"))
        })
        .clone()
}

#[cfg(windows)]
fn detached_shell_command_for(
    shell: crate::windows_shell::WindowsShell,
    command: &str,
    exit_path: &Path,
    paths: &TaskPaths,
    creation_flags: u32,
) -> Result<Command, String> {
    use crate::windows_shell::WindowsShell;
    // Write the wrapper to a temp file alongside the other task files,
    // then invoke the shell with the file path as a single clean
    // argument. This sidesteps the entire Windows command-line quoting
    // mess (Rust std-lib quoting + cmd /C parser + PowerShell -Command
    // parser all interacting with embedded quotes in the wrapper).
    //
    // Path arguments don't need quoting in the same problematic way
    // because: (1) we use no-space task IDs (bash-XXXXXXXX) so the path
    // contains no characters that need shell escaping; (2) the wrapper
    // body's internal quotes never reach the shell command line — the
    // shell reads them from disk by file syntax rules, not command-line
    // parser rules.
    let wrapper_body = shell.wrapper_script_bytes(command, exit_path);
    let wrapper_ext = match shell {
        WindowsShell::Pwsh | WindowsShell::Powershell => "ps1",
        WindowsShell::Cmd => "bat",
        // POSIX shells (git-bash etc.) execute the wrapper through `-c`,
        // so the file extension is purely cosmetic; `.sh` matches what an
        // operator would expect when grepping the spill directory.
        WindowsShell::Posix(_) => "sh",
    };
    let wrapper_path = paths.dir.join(format!(
        "{}.{}",
        paths
            .json
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("wrapper"),
        wrapper_ext
    ));
    fs::write(&wrapper_path, wrapper_body)
        .map_err(|e| format!("failed to write background bash wrapper script: {e}"))?;

    let mut cmd = Command::new(shell.binary().as_ref());
    match shell {
        WindowsShell::Pwsh | WindowsShell::Powershell => {
            // -File runs the script with no quoting issues. `-NoLogo`,
            // `-NoProfile`, etc. apply to the host before the file runs.
            cmd.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            cmd.arg(&wrapper_path);
        }
        WindowsShell::Cmd => {
            // `cmd /D /C "<bat-file-path>"` — invoking a .bat
            // file via /C is well-defined; the file's contents are
            // read line-by-line by cmd's batch processor, NOT
            // re-interpreted by the /C parser. This avoids the
            // "filename syntax incorrect" errors that came from
            // having complex compound commands on the cmd line.
            cmd.args(["/D", "/C"]);
            cmd.arg(&wrapper_path);
        }
        WindowsShell::Posix(_) => {
            // git-bash and other POSIX shells run the wrapper script with
            // `<binary> <wrapper-path>` (the wrapper is just a shell
            // script). No special flags needed — the `trap` and atomic
            // exit-marker rename in `wrapper_script` are POSIX-standard.
            cmd.arg(&wrapper_path);
        }
    }

    // Win32 process creation flags. Caller selects whether to include
    // CREATE_BREAKAWAY_FROM_JOB — see `detached_shell_command_for` callers
    // for the breakaway-fallback strategy.
    cmd.creation_flags(creation_flags);
    Ok(cmd)
}

/// Spawn a detached background bash child process.
///
/// On Unix this is a single spawn against `/bin/sh`. On Windows it walks
/// `WindowsShell::shell_candidates()` (pwsh.exe → powershell.exe →
/// cmd.exe) and retries with the next candidate when the previous one
/// fails to spawn with `NotFound` — the same runtime safety net the
/// foreground bash path has, so issue #27 callers landing on cmd.exe
/// fallback can also use background bash. The wrapper script is
/// regenerated per attempt because PowerShell wrappers embed the shell
/// binary by name; the stdout/stderr capture handles are also reopened
/// per attempt because `Command::spawn()` consumes them.
///
/// Errors other than `NotFound` (PermissionDenied, OutOfMemory, etc.)
/// return immediately without retry — they indicate a problem with the
/// resolved shell that retrying with a different shell won't fix.
fn spawn_detached_child(
    spawn_plan: &SpawnPlan,
    command: &str,
    shell: super::BashShell,
    shell_path: &Path,
    paths: &TaskPaths,
    workdir: &Path,
    env: &HashMap<String, String>,
    io_handles: &mut TaskIoHandles,
    capture_pipeline_status: bool,
) -> Result<std::process::Child, String> {
    #[cfg(windows)]
    let _ = capture_pipeline_status;
    #[cfg(not(windows))]
    let _ = (command, shell);
    #[cfg(not(windows))]
    {
        use std::os::fd::AsRawFd;

        let stdout = io_handles
            .clone_file(TaskArtifact::Stdout)
            .map_err(|e| format!("failed to clone stdout capture handle: {e}"))?;
        let stderr = io_handles
            .clone_file(TaskArtifact::Stderr)
            .map_err(|e| format!("failed to clone stderr capture handle: {e}"))?;
        let prepared = spawn_plan
            .prepared_task()
            .ok_or_else(|| "background task payload was not prepared".to_string())?;
        let payload = prepared.invocation()?;
        let exit = io_handles
            .inheritable_file(TaskArtifact::Exit)
            .map_err(|e| format!("failed to inherit exit marker handle: {e}"))?;
        let failure = io_handles
            .inheritable_file(TaskArtifact::SandboxUnavailable)
            .map_err(|e| format!("failed to inherit sandbox failure marker handle: {e}"))?;
        let pipeline_status = capture_pipeline_status
            .then(|| io_handles.inheritable_file(TaskArtifact::PipelineStatus))
            .transpose()
            .map_err(|e| format!("failed to inherit pipeline status handle: {e}"))?;
        let shell_path = spawn_plan.host_shell_path().unwrap_or(shell_path);
        let pipeline_shell = super::process::pipeline_shell_kind(shell_path).unwrap_or("");
        let pipeline_status_fd = if capture_pipeline_status {
            crate::sandbox_spawn::CHILD_PIPE_STATUS_FD.to_string()
        } else {
            String::new()
        };
        let args = vec![
            OsString::from("-c"),
            payload.wrapper_text.clone(),
            OsString::from("aft-payload-wrapper"),
            shell_path.as_os_str().to_os_string(),
            payload.command_text.clone(),
            OsString::from(crate::sandbox_spawn::CHILD_EXIT_FD.to_string()),
            OsString::from(pipeline_status_fd),
            OsString::from(pipeline_shell),
        ];
        let (mut child_command, profile_handle) = crate::sandbox_spawn::detached_command_for_plan(
            spawn_plan,
            std::ffi::OsStr::new("/bin/sh"),
            &args,
            &paths.json,
            crate::sandbox_spawn::CHILD_EXIT_FD,
            crate::sandbox_spawn::CHILD_FAILURE_FD,
        )?;
        crate::sandbox_spawn::apply_marker_fd_allowlist(
            &mut child_command,
            exit.as_raw_fd(),
            failure.as_raw_fd(),
            pipeline_status.as_ref().map(|file| file.as_raw_fd()),
        )?;
        child_command
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        crate::agent_child_env::apply_to_command(&mut child_command, env);
        crate::sandbox_spawn::apply_sandbox_environment(spawn_plan, &mut child_command, env);
        let child = child_command
            .spawn()
            .map_err(|e| format!("failed to spawn background bash command: {e}"));
        drop((payload, exit, failure, pipeline_status, profile_handle));
        child
    }
    #[cfg(windows)]
    {
        let _ = shell_path;
        use crate::windows_shell::shell_candidates;
        match spawn_plan {
            SpawnPlan::Unsandboxed | SpawnPlan::Host { .. } => {}
            SpawnPlan::Refused { code, .. } => return Err((*code).to_string()),
            SpawnPlan::Launcher { .. } => return Err("sandbox_unavailable".to_string()),
        }
        // Spawn priority: pwsh → powershell → git-bash → cmd. Same as the
        // legacy foreground bash spawn path. v0.20 routes ALL bash through
        // this background spawn helper, including foreground tool calls
        // where the model writes PowerShell-syntax (`$var = ...`,
        // `Start-Sleep`, `Add-Content`) — those fail outright under cmd.
        // The earlier v0.18-era cmd-first override worked around a
        // PowerShell detached-output bug; that bug is fixed at the
        // process-flag layer (CREATE_NO_WINDOW instead of DETACHED_PROCESS,
        // see flag block below), so we no longer need to misroute PS
        // commands through cmd.
        let candidates: Vec<crate::windows_shell::WindowsShell> = if shell.is_powershell() {
            vec![crate::windows_shell::WindowsShell::Pwsh]
        } else {
            shell_candidates()
        };
        // Win32 process creation flags. We try with CREATE_BREAKAWAY_FROM_JOB
        // first (so the bg child outlives the AFT process when AFT is killed),
        // then fall back without it for environments where the parent is in a
        // Job Object that doesn't grant `JOB_OBJECT_LIMIT_BREAKAWAY_OK`. CI
        // runners (GitHub Actions windows-2022) and some MDM-managed corp
        // environments hit this — `CreateProcess` returns Access Denied (5).
        // Without breakaway, the child still runs detached but will be torn
        // down with the parent if the parent process group is signaled.
        //
        // CREATE_NO_WINDOW avoids a visible console while retaining the
        // hidden console services PowerShell needs for reliable redirected
        // stdout/stderr. DETACHED_PROCESS can drop redirected output.
        const FLAG_CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const FLAG_CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const FLAG_CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let with_breakaway =
            FLAG_CREATE_NO_WINDOW | FLAG_CREATE_NEW_PROCESS_GROUP | FLAG_CREATE_BREAKAWAY_FROM_JOB;
        let without_breakaway = FLAG_CREATE_NO_WINDOW | FLAG_CREATE_NEW_PROCESS_GROUP;
        let mut last_error: Option<String> = None;
        for (idx, shell) in candidates.iter().enumerate() {
            // Per-shell, try with breakaway first. If the process is in a
            // restrictive job, the breakaway flag triggers Access Denied
            // (os error 5). Retry once without breakaway.
            for &flags in &[with_breakaway, without_breakaway] {
                // Clone the pre-opened O_EXCL capture handles per attempt;
                // Command::spawn consumes each Stdio wrapper.
                let stdout = io_handles
                    .clone_file(TaskArtifact::Stdout)
                    .map_err(|e| format!("failed to clone stdout capture handle: {e}"))?;
                let stderr = io_handles
                    .clone_file(TaskArtifact::Stderr)
                    .map_err(|e| format!("failed to clone stderr capture handle: {e}"))?;
                let mut cmd =
                    detached_shell_command_for(shell.clone(), command, &paths.exit, paths, flags)?;
                cmd.current_dir(workdir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(stdout))
                    .stderr(Stdio::from(stderr));
                crate::agent_child_env::apply_to_command(&mut cmd, env);
                match cmd.spawn() {
                    Ok(child) => {
                        if idx > 0 {
                            crate::slog_warn!("background bash spawn fell back to {} after {} earlier candidate(s) failed; \
                             the cached PATH probe disagreed with runtime spawn — likely PATH \
                             inheritance, antivirus / AppLocker / Defender ASR, or sandbox policy.",
                            shell.binary(),
                            idx);
                        }
                        if flags == without_breakaway {
                            crate::slog_warn!(
                                "background bash spawn: CREATE_BREAKAWAY_FROM_JOB rejected \
                             (likely a restrictive Job Object — CI sandbox or MDM policy). \
                             Spawned without breakaway; the bg task will be torn down if the \
                             AFT process group is killed."
                            );
                        }
                        return Ok(child);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        crate::slog_warn!("background bash spawn: {} returned NotFound at runtime — trying next candidate",
                        shell.binary());
                        last_error = Some(format!("{}: {e}", shell.binary()));
                        // Skip the without-breakaway retry for NotFound — the
                        // binary itself is missing, breakaway flag is irrelevant.
                        break;
                    }
                    Err(e) if flags == with_breakaway && e.raw_os_error() == Some(5) => {
                        // Access Denied during breakaway — retry without it.
                        crate::slog_warn!(
                            "background bash spawn: CREATE_BREAKAWAY_FROM_JOB rejected with \
                         Access Denied — retrying {} without breakaway",
                            shell.binary()
                        );
                        last_error = Some(format!("{}: {e}", shell.binary()));
                        continue;
                    }
                    Err(e) => {
                        return Err(format!(
                            "failed to spawn background bash command via {}: {e}",
                            shell.binary()
                        ));
                    }
                }
            }
        }
        Err(format!(
            "failed to spawn background bash command: no Windows shell could be spawned. \
             Last error: {}. PATH-probed candidates: {:?}",
            last_error.unwrap_or_else(|| "no candidates were attempted".to_string()),
            candidates.iter().map(|s| s.binary()).collect::<Vec<_>>()
        ))
    }
}

#[cfg(test)]
fn random_slug() -> String {
    // 8 bytes = 64-bit entropy → `bash-{16hex}`, matching the documented contract
    // at `generate_unique_task_id`. The width is load-bearing for the subc
    // delivery dedup: a plugin can retain a delivered task id awaiting ack that
    // Rust has already dropped (a lost ack response), and Rust's uniqueness check
    // cannot see that plugin-side set — so id reuse must be made negligible by
    // entropy alone. 32-bit was reusable within a long session and could let a new
    // task collide with such a stale id and be silently skipped (audit R3 #3).
    let mut bytes = [0u8; 8];
    // getrandom is a transitive dependency; use it directly for OS entropy.
    getrandom::fill(&mut bytes).unwrap_or_else(|_| {
        // Extremely unlikely fallback: time + pid mix across all 8 bytes.
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let p = u64::from(std::process::id());
        bytes.copy_from_slice(&(t ^ p.rotate_left(32)).to_le_bytes());
    });
    // `bash-` + 16 lowercase hex chars — compact, OS-entropy backed.
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("bash-{hex}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use super::*;
    use crate::bash_background::persistence::{read_task, task_paths, write_task};

    #[cfg(unix)]
    const QUICK_SUCCESS_COMMAND: &str = "true";
    #[cfg(windows)]
    const QUICK_SUCCESS_COMMAND: &str = "cmd /c exit 0";

    #[cfg(unix)]
    const LONG_RUNNING_COMMAND: &str = "sleep 5";

    #[cfg(unix)]
    #[test]
    fn launcher_plans_disable_pipeline_status_capture() {
        let launcher = SpawnPlan::launcher_for_test(
            crate::sandbox_profile::SandboxProfile {
                v: crate::sandbox_profile::SANDBOX_PROFILE_VERSION,
                writable_roots: Vec::new(),
                write_deny: Vec::new(),
                write_deny_nested: Vec::new(),
                read_allow: Vec::new(),
                read_deny: Vec::new(),
                socket_deny: Vec::new(),
                cache_roots: Vec::new(),
                temp_dir: PathBuf::from("/tmp/aft-test-sandbox"),
            },
            PathBuf::from("/bin/true"),
        );
        assert!(!should_capture_pipeline_status(
            &launcher,
            true,
            Path::new("/bin/bash")
        ));
        assert!(should_capture_pipeline_status(
            &SpawnPlan::Unsandboxed,
            true,
            Path::new("/bin/bash")
        ));
    }

    #[cfg(windows)]
    const LONG_RUNNING_COMMAND: &str = "cmd /c timeout /t 5 /nobreak > nul";

    #[test]
    fn bash_memory_estimate_is_zero_when_empty_and_nonzero_for_completion_cache() {
        let registry = BgTaskRegistry::default();
        assert_eq!(registry.estimated_memory().estimated_bytes, Some(0));
        registry
            .inner
            .completions
            .lock()
            .unwrap()
            .push_back(BgCompletion {
                task_id: "bash-memory".to_string(),
                session_id: "session-memory".to_string(),
                status: BgTaskStatus::Completed,
                exit_code: Some(0),
                command: "printf memory".to_string(),
                output_preview: "resident completion output".to_string(),
                output_truncated: false,
                original_tokens: None,
                compressed_tokens: None,
                tokens_skipped: false,
                status_reason: None,
            });
        let estimate = registry.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["completion_caches"], 1);
        assert_eq!(estimate.counts["sessions"], 1);
    }

    #[test]
    fn gh_structured_detection_rejects_piped_commands() {
        assert!(is_gh_structured_command(
            "gh issue list --json number,title"
        ));
        assert!(is_gh_structured_command(
            "cd repo && gh issue list --json number,title"
        ));

        assert!(!is_gh_structured_command(
            "gh issue list --json number,title | jq '.[]'"
        ));
        assert!(!is_gh_structured_command(
            "gh issue list --json number,title |"
        ));
    }

    fn insert_terminal_piped_task(
        registry: &BgTaskRegistry,
        dir: &tempfile::TempDir,
        command: &str,
        stdout: &str,
        stderr: &str,
        compressed: bool,
    ) -> (String, Arc<BgTask>) {
        let task_id = random_slug();
        let paths = task_paths(dir.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.stdout, stdout).unwrap();
        fs::write(&paths.stderr, stderr).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            "session".to_string(),
            command.to_string(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            Some(30_000),
            true,
            compressed,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task(&paths.json, &metadata).unwrap();
        registry
            .insert_rehydrated_task(metadata, paths, true, None)
            .expect("insert terminal task");
        let task = registry.task_for_session(&task_id, "session").unwrap();
        (task_id, task)
    }

    #[test]
    fn bash_zero_preview_running_status_skips_output_read_while_explicit_preview_reads() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = random_slug();
        let paths = task_paths(dir.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.stdout, "live output\n").unwrap();
        fs::write(&paths.stderr, "").unwrap();
        let stdout_path = paths.stdout.clone();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            "session".to_string(),
            "sleep 60".to_string(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            Some(30_000),
            true,
            false,
        );
        metadata.status = BgTaskStatus::Running;
        write_task(&paths.json, &metadata).unwrap();
        registry
            .insert_rehydrated_task(metadata, paths, false, None)
            .expect("insert running task");

        crate::bash_background::buffer::reset_tail_read_count(&stdout_path);
        for _ in 0..5 {
            let snapshot = registry
                .status(&task_id, "session", Some(dir.path()), Some(dir.path()), 0)
                .expect("running snapshot");
            assert_eq!(snapshot.info.status, BgTaskStatus::Running);
            assert!(snapshot.output_preview.is_empty());
        }
        assert_eq!(
            crate::bash_background::buffer::tail_read_count(&stdout_path),
            0
        );

        let snapshot = registry
            .status(
                &task_id,
                "session",
                Some(dir.path()),
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .expect("explicit running snapshot");
        assert_eq!(snapshot.output_preview, "live output\n");
        assert_eq!(
            crate::bash_background::buffer::tail_read_count(&stdout_path),
            1
        );
    }

    #[test]
    fn artifact_read_capability_requires_exact_canonical_path_and_session() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let (_task_id, task) = insert_terminal_piped_task(
            &registry,
            &dir,
            "printf output",
            "stdout\n",
            "stderr\n",
            true,
        );
        fs::write(&task.paths.exit, "0\n").unwrap();

        assert!(registry.is_session_owned_artifact_path("session", &task.paths.stdout));
        assert!(registry.is_session_owned_artifact_path("session", &task.paths.stderr));
        assert!(registry.is_session_owned_artifact_path("session", &task.paths.exit));
        assert!(!registry.is_session_owned_artifact_path("different-session", &task.paths.stdout));
        assert!(!registry.is_session_owned_artifact_path("session", &task.paths.json));

        let unregistered = task.paths.dir.join("unregistered-output");
        fs::write(&unregistered, "not a task artifact\n").unwrap();
        assert!(!registry.is_session_owned_artifact_path("session", &unregistered));
    }

    #[cfg(unix)]
    #[test]
    fn artifact_directory_symlink_does_not_create_a_prefix_exception() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let (_task_id, task) =
            insert_terminal_piped_task(&registry, &dir, "printf output", "stdout\n", "", true);
        let link = project.join("task-artifacts");
        std::os::unix::fs::symlink(&task.paths.dir, &link).unwrap();
        let unregistered = task.paths.dir.join("unregistered-output");
        fs::write(&unregistered, "not registered\n").unwrap();

        assert!(!registry.is_session_owned_artifact_path("session", &link));
        assert!(
            !registry.is_session_owned_artifact_path("session", &link.join("unregistered-output"))
        );
        assert!(registry.is_session_owned_artifact_path(
            "session",
            &link.join(task.paths.stdout.file_name().unwrap())
        ));

        let outside = dir.path().join("outside-secret");
        fs::write(&outside, "must stay private\n").unwrap();
        fs::remove_file(&task.paths.stdout).unwrap();
        std::os::unix::fs::symlink(&outside, &task.paths.stdout).unwrap();
        assert!(!registry.is_session_owned_artifact_path("session", &task.paths.stdout));
    }

    #[test]
    fn recovery_footer_uses_bash_status_when_artifact_is_not_registered() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = "bash-1111111111111111";
        let paths = task_paths(dir.path(), "session", task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(
            &paths.stdout,
            format!("{}tail\n", "output-line\n".repeat(2_000)),
        )
        .unwrap();
        fs::write(&paths.stderr, "").unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            "session".to_string(),
            "printf output".to_string(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            Some(30_000),
            true,
            true,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task(&paths.json, &metadata).unwrap();

        let cache = registry
            .render_terminal_output_from_paths(&metadata, &paths)
            .expect("terminal render");

        assert!(cache
            .output_preview
            .contains("use bash_status({taskId: \"bash-1111111111111111\"})"));
        assert!(!cache.output_preview.contains("full output: read "));
    }

    fn insert_terminal_pty_task(
        registry: &BgTaskRegistry,
        dir: &tempfile::TempDir,
        pty_output: &str,
    ) -> (String, Arc<BgTask>) {
        let task_id = random_slug();
        let paths = task_paths(dir.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.pty, pty_output).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            "session".to_string(),
            "python".to_string(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            Some(30_000),
            true,
            true,
        );
        metadata.mode = BgMode::Pty;
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task(&paths.json, &metadata).unwrap();
        registry
            .insert_rehydrated_task(metadata, paths, true, None)
            .expect("insert terminal pty task");
        let task = registry.task_for_session(&task_id, "session").unwrap();
        (task_id, task)
    }

    #[cfg(unix)]
    fn wait_for_terminal_snapshot(
        registry: &BgTaskRegistry,
        task_id: &str,
        session_id: &str,
        project: &Path,
        storage: &Path,
    ) -> BgTaskSnapshot {
        let started = Instant::now();
        loop {
            let snapshot = registry
                .status(task_id, session_id, Some(project), Some(storage), 4096)
                .expect("spawned task should be visible to status");
            if snapshot.info.status.is_terminal() {
                return snapshot;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out waiting for task {task_id} to finish; last status={:?}",
                snapshot.info.status
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn write_running_project_task(storage: &Path, project: &Path, session: &str, task_id: &str) {
        let paths = task_paths(storage, session, task_id).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            session.to_string(),
            "sleep 60".to_string(),
            project.to_path_buf(),
            Some(project.to_path_buf()),
            Some(30_000),
            true,
            true,
        );
        metadata.status = BgTaskStatus::Running;
        // The harness's own PID is an always-alive process for READ-ONLY
        // paths (status replay never signals it). Kill-path tests must never
        // copy this: recording the harness PID where the product kills
        // child_pid takes down the whole libtest process on Windows - spawn
        // a disposable child instead (see bash_kill.rs tests).
        metadata.child_pid = Some(std::process::id());
        write_task(&paths.json, &metadata).unwrap();
        fs::write(&paths.stdout, "still running\n").unwrap();
        fs::write(&paths.stderr, "").unwrap();
    }

    #[test]
    fn status_replay_filters_same_session_by_project_root() {
        let project_a = tempfile::tempdir().unwrap();
        let project_b = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let session = "shared-session";
        let task_id = "bash-2222222222222222";
        write_running_project_task(storage.path(), project_a.path(), session, task_id);

        let actor_b = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        assert!(actor_b
            .status(
                task_id,
                session,
                Some(project_b.path()),
                Some(storage.path()),
                1024,
            )
            .is_none());
        assert!(actor_b.task_for_session(task_id, session).is_none());

        let actor_a = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let snapshot = actor_a
            .status(
                task_id,
                session,
                Some(project_a.path()),
                Some(storage.path()),
                1024,
            )
            .expect("owning project should replay its task");
        assert_eq!(snapshot.info.status, BgTaskStatus::Running);
    }

    #[cfg(unix)]
    #[test]
    fn multiline_pipeline_stdout_persists_all_lines_after_terminal_status() {
        let cases = [
            (
                "long-first",
                "sleep 0.5; printf 'one\\n' | cat\nprintf 'two\\n' | grep -c two\nprintf 'three\\n' | cat",
                vec!["one", "1", "three"],
            ),
            (
                "short-first",
                "printf 'one\\n' | cat\nsleep 0.2; printf 'two\\n' | grep -c two\nprintf 'three\\n' | cat",
                vec!["one", "1", "three"],
            ),
            (
                "failing-middle",
                "sleep 0.2; printf 'one\\n' | cat\nfalse; printf 'after-false\\n' | cat\nprintf 'three\\n' | cat",
                vec!["one", "after-false", "three"],
            ),
        ];

        for (name, command, expected_lines) in cases {
            let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
            let dir = tempfile::tempdir().unwrap();
            let session_id = format!("session-{name}");
            let task_id = registry
                .spawn(
                    SpawnPlan::Unsandboxed,
                    command,
                    session_id.clone(),
                    dir.path().to_path_buf(),
                    HashMap::new(),
                    Some(Duration::from_secs(30)),
                    dir.path().to_path_buf(),
                    10,
                    true,
                    true,
                    Some(dir.path().to_path_buf()),
                )
                .unwrap();

            let snapshot = wait_for_terminal_snapshot(
                &registry,
                &task_id,
                &session_id,
                dir.path(),
                dir.path(),
            );
            assert_eq!(
                snapshot.info.status,
                BgTaskStatus::Completed,
                "{name}: task should complete; snapshot={snapshot:?}"
            );
            assert_eq!(
                snapshot.exit_code,
                Some(0),
                "{name}: script should use the final command's exit code"
            );

            let stdout = String::from_utf8(
                registry
                    .read_artifact(&task_id, &session_id, TaskArtifact::Stdout)
                    .expect("read validated stdout artifact"),
            )
            .expect("stdout is UTF-8");
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(
                lines, expected_lines,
                "{name}: raw stdout artifact must include every newline-separated command's output"
            );
        }
    }

    #[test]
    fn recognizes_all_recovery_marker_forms() {
        assert!(is_recovery_marker(
            "[truncated output; full output: read \"/tmp/out\"]"
        ));
        assert!(is_recovery_marker(
            "[omitted output; see remaining: tail -n +42 \"/tmp/out\"]"
        ));
        assert!(is_recovery_marker(
            "[truncated output; full output unavailable]"
        ));
        assert!(is_recovery_marker(
            r#"[truncated 123 bytes from saved output prefix; retained output: read "/tmp/out"]"#
        ));
    }

    #[test]
    fn recovery_marker_reports_disk_prefix_truncation_as_retained_output() {
        let recovery = RecoveryContext {
            dropped_by_class: BTreeMap::new(),
            had_inner_drop: false,
            offset_hint_eligible: false,
            offset_start_line: None,
            byte_truncated: false,
            disk_truncated_prefix_bytes: 4096,
            output_path: Some("/tmp/stdout".to_string()),
            stderr_path: None,
            include_stderr_path: false,
            artifact_access: ArtifactRecoveryAccess {
                task_id: "bash-test".to_string(),
                readable: true,
            },
        };

        let marker = recovery_marker(&recovery).expect("disk truncation must emit marker");

        assert!(marker.contains("truncated 4096 bytes from saved output prefix"));
        assert!(marker.contains(r#"retained output: read "/tmp/stdout""#));
        assert!(!marker.contains("full output: read"));
    }

    #[test]
    fn killed_exit_marker_sets_nonzero_sentinel_exit_code() {
        let metadata = PersistedTask::starting(
            "task".to_string(),
            "session".to_string(),
            "cargo test".to_string(),
            PathBuf::from("/tmp"),
            None,
            None,
            true,
            true,
        );

        let terminal = terminal_metadata_from_marker(metadata, ExitMarker::Killed, None);

        assert_eq!(terminal.status, BgTaskStatus::Killed);
        assert_eq!(terminal.exit_code, Some(137));
    }

    #[test]
    fn terminal_status_polls_use_cached_render_once_and_off_lock() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let (_task_id, task) = insert_terminal_piped_task(
            &registry,
            &dir,
            "custom-tool --verbose",
            &"stdout line\n".repeat(200_000),
            "",
            true,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let saw_unlocked_state = Arc::new(AtomicBool::new(false));
        let task_holder = Arc::new(Mutex::new(Some(Arc::clone(&task))));
        let calls_for_closure = Arc::clone(&calls);
        let unlocked_for_closure = Arc::clone(&saw_unlocked_state);
        let task_for_closure = Arc::clone(&task_holder);
        registry.set_compressor_with_exit_code(move |_command, output, _exit_code| {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
            if let Some(task) = task_for_closure.lock().unwrap().as_ref() {
                if task.state.try_lock().is_ok() {
                    unlocked_for_closure.store(true, Ordering::SeqCst);
                }
            }
            CompressionResult::new(format!("compressed {} bytes", output.len()))
        });

        let first = registry
            .status(
                &task.task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();
        let second = registry
            .status(
                &task.task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();
        let listed = registry.list(RUNNING_OUTPUT_PREVIEW_BYTES);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "terminal render must be cached"
        );
        assert!(
            saw_unlocked_state.load(Ordering::SeqCst),
            "compressor must run after releasing the task state lock"
        );
        assert!(first.output_preview.starts_with("compressed "));
        assert_eq!(second.output_preview, first.output_preview);
        assert_eq!(listed[0].output_preview, first.output_preview);
    }

    #[test]
    fn completion_preview_success_keeps_tail_only() {
        // Exit-aware completion previews: a SUCCESSFUL task's reminder keeps a
        // short tail only — head context is noise when the command worked
        // (regression: the uniform 4 KiB head+tail cap flooded reminders with
        // ~1K tokens of build noise per completed task).
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let output = format!("HEAD-SIGNAL\n{}TAIL-SIGNAL\n", "middle\n".repeat(2_000));
        let (_task_id, task) =
            insert_terminal_piped_task(&registry, &dir, "cat big.log", &output, "", false);

        registry.post_terminal_transition(&task, true).unwrap();
        let completions = registry.drain_completions_for_session(Some("session"));
        assert_eq!(completions.len(), 1);
        let preview = &completions[0].output_preview;
        assert!(preview.contains("TAIL-SIGNAL"), "preview was {preview:?}");
        assert!(!preview.contains("HEAD-SIGNAL"), "preview was {preview:?}");
        assert!(completions[0].output_truncated);
    }

    #[test]
    fn completion_preview_failure_keeps_head_and_tail() {
        // A FAILED task keeps a small head (first error / command banner) plus
        // a larger tail (tracebacks and summaries land at the end).
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let output = format!("HEAD-SIGNAL\n{}TAIL-SIGNAL\n", "middle\n".repeat(2_000));
        let task_id = random_slug();
        let paths = task_paths(dir.path(), "session", &task_id).unwrap();
        fs::create_dir_all(&paths.dir).unwrap();
        fs::write(&paths.stdout, &output).unwrap();
        fs::write(&paths.stderr, "").unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.clone(),
            "session".to_string(),
            "cat big.log".to_string(),
            dir.path().to_path_buf(),
            Some(dir.path().to_path_buf()),
            Some(30_000),
            true,
            false,
        );
        metadata.mark_terminal(BgTaskStatus::Failed, Some(1), None);
        write_task(&paths.json, &metadata).unwrap();
        registry
            .insert_rehydrated_task(metadata, paths, true, None)
            .expect("insert terminal task");
        let task = registry.task_for_session(&task_id, "session").unwrap();

        registry.post_terminal_transition(&task, true).unwrap();
        let completions = registry.drain_completions_for_session(Some("session"));
        assert_eq!(completions.len(), 1);
        let preview = &completions[0].output_preview;
        assert!(preview.contains("HEAD-SIGNAL"), "preview was {preview:?}");
        assert!(preview.contains("TAIL-SIGNAL"), "preview was {preview:?}");
    }

    #[test]
    fn has_completions_for_session_matches_pending_delivery() {
        let registry = BgTaskRegistry::default();
        assert!(!registry.has_completions_for_session(Some("session")));
        assert!(!registry.has_completions_for_session(None));

        let dir = tempfile::tempdir().unwrap();
        let (_task_id, task) =
            insert_terminal_piped_task(&registry, &dir, QUICK_SUCCESS_COMMAND, "done\n", "", false);
        registry.post_terminal_transition(&task, true).unwrap();

        assert!(registry.has_completions_for_session(Some("session")));
        assert!(registry.has_completions_for_session(None));
        assert!(!registry.has_completions_for_session(Some("other-session")));

        let completions = registry.drain_completions_for_session(Some("session"));
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].task_id, task.task_id);
    }

    #[test]
    fn structured_gh_json_survives_intact_and_ignores_stderr() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_closure = Arc::clone(&calls);
        registry.set_compressor_with_exit_code(move |_command, output, _exit_code| {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
            CompressionResult::new(output)
        });
        let (task_id, _task) = insert_terminal_piped_task(
            &registry,
            &dir,
            "gh pr view 123 --json body",
            "{\"body\":\"hello\"}",
            "warning: stderr must not join json",
            true,
        );

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert_eq!(snapshot.output_preview, "{\"body\":\"hello\"}");
        assert!(!snapshot.output_preview.contains("warning"));
        assert!(!snapshot.output_truncated);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "structured JSON bypasses compression"
        );
    }

    #[test]
    fn registry_emits_single_recovery_marker_for_class_drops() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        registry.set_compressor_with_exit_code(move |_command, _output, _exit_code| {
            let mut dropped = BTreeMap::new();
            dropped.insert(DropClass::Error, 18);
            dropped.insert(DropClass::Warning, 6);
            CompressionResult::with_class_drops("kept diagnostic", dropped)
        });
        let (task_id, task) =
            insert_terminal_piped_task(&registry, &dir, "custom-tool", "raw", "", true);

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert_eq!(snapshot.output_preview.matches("full output:").count(), 1);
        assert!(snapshot.output_preview.contains("+18 more errors"));
        assert!(snapshot.output_preview.contains("+6 more warnings"));
        assert!(snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stdout.display())));
        assert!(!snapshot.output_preview.contains("tail -n +"));
        assert!(snapshot.output_truncated);
    }

    #[test]
    fn registry_marker_reports_semantic_and_byte_drops_once() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        registry.set_compressor_with_exit_code(move |_command, _output, _exit_code| {
            let mut dropped = BTreeMap::new();
            dropped.insert(DropClass::Error, 1);
            CompressionResult::with_class_drops(
                format!("HEAD-SIGNAL\n{}TAIL-SIGNAL", "middle\n".repeat(8_000)),
                dropped,
            )
        });
        let (task_id, _task) =
            insert_terminal_piped_task(&registry, &dir, "custom-tool", "raw", "", true);

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert_eq!(snapshot.output_preview.matches("full output:").count(), 1);
        assert!(snapshot.output_preview.contains("+1 more error"));
        assert!(snapshot.output_preview.contains("truncated output"));
        assert!(snapshot.output_preview.contains("HEAD-SIGNAL"));
        assert!(snapshot.output_preview.contains("TAIL-SIGNAL"));
        assert!(!snapshot.output_preview.contains("...<truncated"));
        assert!(snapshot.output_truncated);
    }

    #[test]
    fn cargo_stderr_class_drops_name_both_capture_paths() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let filter_registry = crate::compress::toml_filter::FilterRegistry::default();
        registry.set_compressor_with_exit_code(move |command, output, exit_code| {
            crate::compress::compress_with_registry_exit_code(
                command,
                &output,
                exit_code,
                &filter_registry,
            )
        });
        let stderr = (0..22)
            .map(|index| {
                format!(
                    "error: cargo failure {index}\n  --> src/lib.rs:{}:1\n   |\n{} | boom\n",
                    index + 1,
                    index + 1
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (task_id, task) = insert_terminal_piped_task(
            &registry,
            &dir,
            "cargo check",
            "Finished dev [unoptimized] target(s) in 0.01s\n",
            &stderr,
            true,
        );

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert!(snapshot.output_preview.contains("+2 more errors"));
        assert!(snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stdout.display())));
        assert!(snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stderr.display())));
        assert!(!snapshot.output_preview.contains("tail -n +"));
    }

    #[test]
    fn over_ceiling_structured_json_uses_pointer_not_partial_json() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let body = format!("{{\"body\":\"{}\"}}", "x".repeat(60 * 1024));
        let (task_id, task) = insert_terminal_piped_task(
            &registry,
            &dir,
            "cd /repo && gh pr view 123 --json body",
            &body,
            "",
            true,
        );

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert!(snapshot.output_preview.starts_with("[JSON output "));
        assert!(snapshot
            .output_preview
            .contains(&task.paths.stdout.display().to_string()));
        assert!(!snapshot.output_preview.contains(&"x".repeat(1024)));
        assert!(snapshot.output_truncated);
    }

    #[test]
    fn toml_strip_tail_cap_uses_full_output_hint_not_offset_hint() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let filter_registry = crate::compress::toml_filter::build_registry(
            crate::compress::builtin_filters::ALL,
            None,
            None,
        );
        registry.set_compressor_with_exit_code(move |command, output, exit_code| {
            crate::compress::compress_with_registry_exit_code(
                command,
                &output,
                exit_code,
                &filter_registry,
            )
        });
        let stdout = format!(
            "make[1]: Entering directory `/tmp`\n{}",
            (0..100)
                .map(|index| format!("compile line {index}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let (task_id, task) =
            insert_terminal_piped_task(&registry, &dir, "make all", &stdout, "", true);

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert!(snapshot.output_preview.contains("compile line 99"));
        assert!(snapshot.output_preview.contains(&format!(
            "full output: read \"{}\"",
            task.paths.stdout.display()
        )));
        assert!(!snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stderr.display())));
        assert!(!snapshot.output_preview.contains("tail -n +"));
    }

    #[test]
    fn compressed_false_raw_passthrough_uses_wider_head_tail_cap() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let output = format!("RAW-HEAD\n{}RAW-TAIL\n", "raw-middle\n".repeat(8_000));
        let (task_id, task) =
            insert_terminal_piped_task(&registry, &dir, "cat raw.log", &output, "RAW-ERR\n", false);

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert!(snapshot.output_preview.contains("RAW-HEAD"));
        assert!(snapshot.output_preview.contains("RAW-TAIL"));
        assert!(snapshot.output_preview.contains("truncated output"));
        assert!(snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stdout.display())));
        assert!(snapshot
            .output_preview
            .contains(&format!("read \"{}\"", task.paths.stderr.display())));
        assert!(!snapshot.output_preview.contains("tail -n +"));
        assert!(snapshot.output_preview.len() > 16 * 1024);
        assert!(snapshot.output_truncated);
    }

    #[test]
    fn pty_terminal_snapshot_bypasses_line_compression() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_closure = Arc::clone(&calls);
        registry.set_compressor_with_exit_code(move |_command, output, _exit_code| {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
            CompressionResult::new(output)
        });
        let (task_id, _task) = insert_terminal_pty_task(&registry, &dir, "raw\u{1b}[31m pty bytes");

        let snapshot = registry
            .status(
                &task_id,
                "session",
                None,
                Some(dir.path()),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .unwrap();

        assert_eq!(snapshot.info.mode, BgMode::Pty);
        assert_eq!(snapshot.output_preview, "");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pty_dimensions_are_persisted_and_returned_in_snapshot() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn_pty(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
                50,
                120,
            )
            .unwrap();

        let resolved =
            resolve_task_layout(&session_tasks_dir(dir.path(), "session"), &task_id).unwrap();
        let metadata = read_task_at(&resolved).unwrap();
        assert_eq!(
            metadata.schema_version,
            crate::bash_background::persistence::SCHEMA_VERSION
        );
        assert_eq!(metadata.mode, BgMode::Pty);
        assert_eq!(metadata.pty_rows, Some(50));
        assert_eq!(metadata.pty_cols, Some(120));

        let snapshot = registry
            .status(&task_id, "session", None, Some(dir.path()), 1024)
            .unwrap();
        assert_eq!(snapshot.pty_rows, Some(50));
        assert_eq!(snapshot.pty_cols, Some(120));
    }

    /// Spawn a child process that exits immediately and return it after
    /// it has terminated. Used by reap_child tests to simulate the
    /// "child exists and is dead" state when the watchdog has already
    /// nulled out the original child handle.
    fn spawn_dead_child() -> std::process::Child {
        #[cfg(unix)]
        let mut cmd = std::process::Command::new("true");
        #[cfg(windows)]
        let mut cmd = {
            let mut c = std::process::Command::new("cmd");
            c.args(["/c", "exit", "0"]);
            c
        };
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn replacement child for reap test");
        // Poll try_wait() until the child actually exits, instead of calling
        // wait() which closes the OS handle. On Windows, after wait()
        // closes the handle, subsequent try_wait() calls (which reap_child
        // depends on) return Err — the test was inadvertently giving
        // reap_child an unusable child handle. Polling try_wait() keeps the
        // handle open and observes natural exit, matching the production
        // shape where the watchdog discovers an exited child for the first
        // time.
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if started.elapsed() > Duration::from_secs(5) {
                        panic!("dead-child stand-in did not exit within 5s");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("dead-child try_wait failed: {error}"),
            }
        }
        child
    }

    #[test]
    fn ack_marks_delivered_even_when_completion_was_already_consumed_locally() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();
        registry
            .kill_with_status(&task_id, "session", BgTaskStatus::Killed)
            .unwrap();
        assert_eq!(
            registry
                .drain_completions_for_session(Some("session"))
                .len(),
            1
        );

        // Simulate the plugin consuming a sync bash_watch({ exit:true }) result
        // locally before the Rust completion queue is drained/acked.
        registry.inner.completions.lock().unwrap().clear();

        assert_eq!(
            registry.ack_completions_for_session(Some("session"), std::slice::from_ref(&task_id)),
            vec![task_id.clone()]
        );
        assert!(registry
            .drain_completions_for_session(Some("session"))
            .is_empty());

        let resolved =
            resolve_task_layout(&session_tasks_dir(dir.path(), "session"), &task_id).unwrap();
        let metadata = read_task_at(&resolved).unwrap();
        assert!(metadata.completion_delivered);

        let replayed = BgTaskRegistry::default();
        replayed
            .replay_session_inner(dir.path(), "session", None)
            .unwrap();
        assert!(replayed
            .drain_completions_for_session(Some("session"))
            .is_empty());
    }

    #[test]
    fn reclaimed_root_kills_running_task_and_persists_reason() {
        let registry = BgTaskRegistry::default();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                root.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                storage.path().to_path_buf(),
                10,
                true,
                false,
                Some(root.path().to_path_buf()),
            )
            .unwrap();
        let pid = registry
            .status(
                &task_id,
                "session",
                Some(root.path()),
                Some(storage.path()),
                0,
            )
            .unwrap()
            .child_pid
            .unwrap();
        assert!(is_process_alive(pid));

        assert_eq!(registry.kill_running_tasks_for_root(root.path()), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "reclaimed task process survived kill"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let snapshot = registry
            .status(
                &task_id,
                "session",
                Some(root.path()),
                Some(storage.path()),
                0,
            )
            .unwrap();
        assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
        assert_eq!(
            snapshot.info.status_reason.as_deref(),
            Some(ROOT_RECLAIMED_REASON)
        );
        let persisted = read_task(
            &registry
                .task_json_path(&task_id, "session")
                .expect("reclaimed task metadata path"),
        )
        .expect("persisted reclaimed task");
        assert_eq!(
            persisted.status_reason.as_deref(),
            Some(ROOT_RECLAIMED_REASON)
        );
        let completion = registry
            .drain_completions_for_session(Some("session"))
            .pop()
            .expect("reclaimed task completion");
        assert_eq!(
            completion.status_reason.as_deref(),
            Some(ROOT_RECLAIMED_REASON)
        );
        registry.detach();
    }

    #[test]
    fn reclaimed_root_kills_pty_task_and_preserves_reason() {
        let registry = BgTaskRegistry::default();
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let task_id = registry
            .spawn_pty(
                SpawnPlan::Unsandboxed,
                command,
                "session".to_string(),
                root.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(60)),
                storage.path().to_path_buf(),
                10,
                true,
                false,
                Some(root.path().to_path_buf()),
                24,
                80,
            )
            .unwrap();
        let pid = registry
            .status(
                &task_id,
                "session",
                Some(root.path()),
                Some(storage.path()),
                0,
            )
            .unwrap()
            .child_pid
            .unwrap();
        assert!(is_process_alive(pid));

        assert_eq!(registry.kill_running_tasks_for_root(root.path()), 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = registry
                .status(
                    &task_id,
                    "session",
                    Some(root.path()),
                    Some(storage.path()),
                    0,
                )
                .unwrap();
            if snapshot.info.status.is_terminal() {
                assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
                assert_eq!(
                    snapshot.info.status_reason.as_deref(),
                    Some(ROOT_RECLAIMED_REASON)
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reclaimed PTY task did not terminate"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_process_alive(pid));
        // The terminal status and the completion frame are written by
        // different actors (kill finalize vs the watchdog's terminal-transition
        // scan), so the completion can trail the observed Killed status. Poll
        // the drain rather than reading it once.
        let completion = loop {
            if let Some(completion) = registry
                .drain_completions_for_session(Some("session"))
                .pop()
            {
                break completion;
            }
            assert!(
                Instant::now() < deadline,
                "reclaimed PTY completion never arrived"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            completion.status_reason.as_deref(),
            Some(ROOT_RECLAIMED_REASON)
        );
        registry.detach();
    }

    #[test]
    fn register_watch_rejects_unknown_task() {
        let registry = BgTaskRegistry::default();

        let result = registry.register_watch(
            "missing-task".to_string(),
            WatchPattern::Substring("READY".into()),
            true,
        );

        assert_eq!(result, Err("task_not_found"));
    }

    #[test]
    fn register_watch_on_terminal_task_scans_existing_output() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&frames);
        let sender: crate::context::ProgressSender = Arc::new(Box::new(move |frame| {
            captured.lock().unwrap().push(frame);
        })
            as Box<dyn Fn(PushFrame) + Send + Sync>);
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(Some(sender))));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let task = registry.task_for_session(&task_id, "session").unwrap();
        std::fs::write(&task.paths.stdout, "READY\n").unwrap();
        registry
            .kill_with_status(&task_id, "session", BgTaskStatus::Killed)
            .unwrap();
        frames.lock().unwrap().clear();
        registry.inner.completions.lock().unwrap().clear();

        registry
            .register_watch(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                true,
            )
            .unwrap();

        let frames = frames.lock().unwrap();
        let frame = frames
            .iter()
            .find_map(|frame| match frame {
                PushFrame::BashPatternMatch(frame) => Some(frame),
                _ => None,
            })
            .expect("terminal watch registration should emit pattern frame");
        assert_eq!(frame.reason, "pattern_match");
        assert_eq!(frame.task_id, task_id);
        assert_eq!(frame.session_id, "session");
        assert_eq!(frame.match_text, "READY");
        assert_eq!(frame.match_offset, 0);
        assert_eq!(registry.active_watch_count(&frame.task_id), 0);
        let metadata = read_task(&task.paths.json).unwrap();
        assert!(metadata.completion_delivered);
    }

    #[test]
    fn cleanup_finished_removes_terminal_tasks_older_than_threshold() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();
        registry
            .kill_with_status(&task_id, "session", BgTaskStatus::Killed)
            .unwrap();
        let completions = registry.drain_completions_for_session(Some("session"));
        assert_eq!(completions.len(), 1);
        assert_eq!(
            registry.ack_completions_for_session(Some("session"), std::slice::from_ref(&task_id)),
            vec![task_id.clone()]
        );

        registry.cleanup_finished(Duration::ZERO);

        assert!(registry.inner.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn cleanup_finished_retains_undelivered_terminals() {
        let registry = BgTaskRegistry::default();
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();
        registry
            .kill_with_status(&task_id, "session", BgTaskStatus::Killed)
            .unwrap();

        registry.cleanup_finished(Duration::ZERO);

        assert!(registry.inner.tasks.lock().unwrap().contains_key(&task_id));
    }

    /// Verify that the live watchdog path (reap_child) gives an exited
    /// child one watchdog pass for its exit marker to land, then marks the
    /// task Failed if the next pass still sees no marker.
    ///
    /// Cross-platform: uses a quick-exiting command that does NOT go
    /// through the wrapper script (we manually clear the exit marker
    /// after spawn to simulate the wrapper crashing before write).
    #[test]
    fn reap_child_marks_failed_when_child_exits_without_exit_marker() {
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();

        let task = registry.task_for_session(&task_id, "session").unwrap();

        // Wait for the child to actually exit and the wrapper to either
        // write the marker or fail. Then nuke the marker to simulate
        // wrapper crash before write. Poll up to 5s; this is plenty for a
        // `true`/`cmd /c exit 0` invocation.
        let started = Instant::now();
        loop {
            let exited = {
                let mut state = task.state.lock().unwrap();
                match &mut state.runtime {
                    TaskRuntime::Piped(Some(child)) => matches!(child.try_wait(), Ok(Some(_))),
                    _ => true,
                }
            };
            if exited {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "child should exit quickly"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Stop the watchdog so it doesn't race with our manual reap_child.
        // On fast Windows runners the watchdog ticks (every 500ms) can
        // observe the child exit and reap it before this test's assertion
        // fires, leaving us with state.child = None and an already-terminal
        // status. We specifically want to test reap_child's logic when
        // invoked manually on a Running-but-actually-dead task, so we need
        // exclusive control over the reap path here.
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Give the watchdog at most one tick (500ms) to notice shutdown
        // before we touch task state. Without this, an in-flight watchdog
        // iteration could still race with our state setup below.
        std::thread::sleep(Duration::from_millis(550));

        // Wrapper likely wrote the marker by now; remove it to simulate
        // a wrapper crash that exited before persisting the exit code.
        let _ = std::fs::remove_file(&task.paths.exit);

        // The watchdog may have already reaped the child handle and
        // marked the task terminal before we got here. Reset both so
        // reap_child has the "Running task whose child just exited"
        // shape it's designed to handle. If the original child handle is
        // gone, install a quick-exited stand-in so the first reap exercises
        // the same try_wait path as production.
        //
        // CRITICAL on Windows: the watchdog ticks fast enough that the
        // JSON on disk may already say `Completed`. `update_task` (called
        // by `reap_child`) reads from disk, applies the closure, but
        // ROLLS BACK if the original on-disk state was already terminal
        // (see persistence.rs::update_task). So we must reset BOTH
        // in-memory metadata AND the JSON on disk to a Running state to
        // give reap_child the fresh shape it expects to operate on.
        {
            let mut state = task.state.lock().unwrap();
            state.metadata.status = BgTaskStatus::Running;
            state.metadata.status_reason = None;
            state.metadata.exit_code = None;
            state.metadata.finished_at = None;
            state.metadata.duration_ms = None;
            // Persist the reset state to disk so update_task's terminal
            // rollback guard sees a non-terminal starting point.
            crate::bash_background::persistence::write_task(&task.paths.json, &state.metadata)
                .expect("persist reset Running metadata for reap_child test");
            // If the watchdog already nulled state.child, we need to
            // simulate "child exists and is dead" so reap_child's
            // try_wait path runs. Spawn a quick-exit child as a stand-in.
            if matches!(state.runtime, TaskRuntime::Piped(None)) {
                state.runtime = TaskRuntime::Piped(Some(spawn_dead_child()));
            }
        }
        // Clear the terminal_at marker too so mark_terminal_now() can fire
        // again inside reap_child.
        *task.terminal_at.lock().unwrap() = None;

        // Sanity: task is still Running per metadata (replay/poll hasn't
        // observed the missing marker yet).
        assert!(
            task.is_running(),
            "precondition: metadata.status == Running"
        );
        assert!(
            !task.paths.exit.exists(),
            "precondition: exit marker absent"
        );

        // First watchdog observation is intentionally insufficient to
        // declare failure. A missing marker may just mean the wrapper is
        // still completing its tmp-file-to-marker rename, so reap_child only
        // drops the child handle and switches to detached PID monitoring.
        registry.reap_child(&task);

        {
            let state = task.state.lock().unwrap();
            assert_eq!(
                state.metadata.status,
                BgTaskStatus::Running,
                "first reap must leave status Running while waiting one pass for marker"
            );
            assert_eq!(
                state.metadata.status_reason, None,
                "first reap must not record a failure reason"
            );
            assert!(
                matches!(state.runtime, TaskRuntime::Piped(None)),
                "child handle must be released after first reap"
            );
            assert!(
                state.detached,
                "task must be marked detached after first reap"
            );
        }

        // Second watchdog observation sees the detached PID is dead and the
        // marker is still absent. That is strong enough evidence that the
        // wrapper exited without persisting an exit code.
        registry.reap_child(&task);

        let state = task.state.lock().unwrap();
        assert!(
            state.metadata.status.is_terminal(),
            "second reap must transition to terminal when PID dead and no marker. Got status={:?}",
            state.metadata.status
        );
        assert_eq!(
            state.metadata.status,
            BgTaskStatus::Failed,
            "must specifically be Failed (not Killed): status={:?}",
            state.metadata.status
        );
        assert_eq!(
            state.metadata.status_reason.as_deref(),
            Some("process exited without exit marker"),
            "reason must match replay path's wording: {:?}",
            state.metadata.status_reason
        );
        assert!(
            matches!(state.runtime, TaskRuntime::Piped(None)),
            "child handle must stay released after second reap"
        );
        assert!(
            state.detached,
            "task must remain detached after second reap"
        );
    }

    /// Companion to the above: when the exit marker DOES exist on disk
    /// at reap_child time, reap_child must NOT mark the task Failed.
    /// Instead it leaves status=Running and lets the next poll_task()
    /// cycle finalize via the marker.
    #[test]
    fn reap_child_preserves_running_when_exit_marker_exists() {
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();

        let task = registry.task_for_session(&task_id, "session").unwrap();

        // Wait for child to exit AND for the marker to land. Both happen
        // shortly after the wrapper finishes — but we want both observed.
        let started = Instant::now();
        loop {
            let exited = {
                let mut state = task.state.lock().unwrap();
                match &mut state.runtime {
                    TaskRuntime::Piped(Some(child)) => matches!(child.try_wait(), Ok(Some(_))),
                    _ => true,
                }
            };
            if exited && task.paths.exit.exists() {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "child should exit and write marker quickly"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Stop the watchdog so it doesn't race with our manual reap_child.
        // On fast Windows runners the watchdog can call poll_task (which
        // finalizes via marker) before this test asserts the
        // "marker exists, status still Running" invariant. We want
        // exclusive control over the reap path.
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(550));

        // If the watchdog already finalized the task before we stopped it,
        // restore the test setup: reset status to Running and ensure the
        // marker file is still on disk. We're testing reap_child's
        // behavior when called manually with both child-exited AND
        // marker-present, regardless of whether the watchdog beat us.
        {
            let mut state = task.state.lock().unwrap();
            state.metadata.status = BgTaskStatus::Running;
            state.metadata.status_reason = None;
            if matches!(state.runtime, TaskRuntime::Piped(None)) {
                state.runtime = TaskRuntime::Piped(Some(spawn_dead_child()));
            }
        }
        *task.terminal_at.lock().unwrap() = None;
        // Make sure the marker is still on disk (poll_task removes it on
        // finalization). Recreate it if needed.
        if !task.paths.exit.exists() {
            std::fs::write(&task.paths.exit, "0").expect("write replacement exit marker");
        }

        // reap_child sees: child exited, marker exists. It should:
        //  - drop state.child / set state.detached = true
        //  - NOT change status (poll_task will finalize via marker next tick)
        registry.reap_child(&task);

        let state = task.state.lock().unwrap();
        assert!(
            matches!(state.runtime, TaskRuntime::Piped(None)),
            "child handle still released even when marker exists"
        );
        assert!(
            state.detached,
            "task still marked detached even when marker exists"
        );
        // Status remains Running because reap_child defers to poll_task
        // when a marker exists. It would be wrong for reap to record the
        // marker outcome (poll_task does that with proper exit-code
        // parsing).
        assert_eq!(
            state.metadata.status,
            BgTaskStatus::Running,
            "reap_child must defer to poll_task when marker exists"
        );
    }

    /// Read a process's `ps` state string ("Z", "S", "R", etc). Returns
    /// `None` once the PID has been fully reaped (no row), which is the
    /// post-reap state we want.
    #[cfg(unix)]
    fn pid_stat(pid: u32) -> Option<String> {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stat.is_empty() {
            None
        } else {
            Some(stat)
        }
    }

    /// A `<defunct>` zombie carries `ps` state starting with 'Z'.
    #[cfg(unix)]
    fn is_zombie(pid: u32) -> bool {
        pid_stat(pid).is_some_and(|stat| stat.starts_with('Z'))
    }

    /// Spawn a child that exits immediately and wait — via `ps`, NOT
    /// `try_wait()`/`wait()` — until it is observably a `<defunct>` zombie,
    /// then return the still-unreaped handle. This reproduces the exact
    /// state issue #91 leaves behind: an exited OS child whose parent has
    /// not reaped it.
    #[cfg(unix)]
    fn spawn_unreaped_zombie() -> std::process::Child {
        let child = std::process::Command::new("true")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn zombie stand-in");
        let pid = child.id();
        let started = Instant::now();
        while !is_zombie(pid) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "stand-in child should become a zombie within 5s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Return WITHOUT reaping — the handle still owns an unwaited zombie.
        child
    }

    /// Regression test for issue #91: the exit-marker terminal path
    /// (`poll_task` -> `finalize_from_marker`) must REAP the direct child
    /// handle, not merely drop it. Dropping a `std::process::Child` does not
    /// `wait()` on Unix, so the exited child lingers as a `[mv] <defunct>`
    /// zombie until AFT exits.
    ///
    /// We install a known-unreaped zombie into the task's child slot and
    /// drive the marker finalize path, then assert the child is gone (reaped)
    /// rather than still `<defunct>`.
    #[cfg(unix)]
    #[test]
    fn finalize_from_marker_reaps_child_no_zombie() {
        use std::sync::atomic::Ordering;

        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();

        // Stop the watchdog so the ONLY terminal-transition path under test
        // is the exit-marker finalize (not reap_child's try_wait, which would
        // reap the child for us and mask the bug).
        registry.inner.shutdown.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(550));

        let task = registry.task_for_session(&task_id, "session").unwrap();

        // Wait for the wrapper's exit marker to land. We deliberately do NOT
        // call try_wait()/wait() on the real child here — doing so would reap
        // it and defeat the test.
        let started = Instant::now();
        while !task.paths.exit.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "exit marker should land quickly for `true`"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Reset to a fresh Running shape and install a guaranteed-unreaped
        // zombie as the child handle, so the finalize path's reap behavior is
        // exercised deterministically regardless of how the real child was
        // handled. Persist Running so update_task's terminal-rollback guard
        // sees a non-terminal starting point.
        let zombie_pid;
        {
            let mut state = task.state.lock().unwrap();
            state.metadata.status = BgTaskStatus::Running;
            state.metadata.status_reason = None;
            state.metadata.exit_code = None;
            state.metadata.finished_at = None;
            state.metadata.duration_ms = None;
            crate::bash_background::persistence::write_task(&task.paths.json, &state.metadata)
                .expect("persist reset Running metadata");
            let zombie = spawn_unreaped_zombie();
            zombie_pid = zombie.id();
            state.runtime = TaskRuntime::Piped(Some(zombie));
        }
        *task.terminal_at.lock().unwrap() = None;

        // Precondition: the installed child is genuinely a `<defunct>` zombie.
        assert!(
            is_zombie(zombie_pid),
            "precondition: stand-in child {zombie_pid} must be a zombie before finalize"
        );

        // Drive the exit-marker terminal path. Before the fix this nulled the
        // Child handle without wait(), leaving the zombie behind.
        registry.poll_task(&task).unwrap();

        {
            let state = task.state.lock().unwrap();
            assert!(
                matches!(state.runtime, TaskRuntime::Piped(None)),
                "child handle must be released after marker finalize"
            );
            assert!(
                state.metadata.status.is_terminal(),
                "task must be terminal after marker finalize: {:?}",
                state.metadata.status
            );
        }

        // The core assertion: the child must have been REAPED, not just
        // dropped. A reaped PID has no `ps` row (or at minimum is not 'Z').
        assert!(
            !is_zombie(zombie_pid),
            "issue #91 regression: child {zombie_pid} left as <defunct> zombie \
             after the exit-marker terminal transition"
        );
    }

    /// Companion to the above for the kill path: when a kill observes an
    /// already-present exit marker (the child finished on its own first), it
    /// must reap the child handle rather than dropping it.
    #[cfg(unix)]
    #[test]
    fn kill_with_existing_marker_reaps_child_no_zombie() {
        use std::sync::atomic::Ordering;

        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                QUICK_SUCCESS_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();

        registry.inner.shutdown.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(550));

        let task = registry.task_for_session(&task_id, "session").unwrap();

        let started = Instant::now();
        while !task.paths.exit.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "exit marker should land quickly for `true`"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let zombie_pid;
        {
            let mut state = task.state.lock().unwrap();
            state.metadata.status = BgTaskStatus::Running;
            state.metadata.status_reason = None;
            state.metadata.exit_code = None;
            state.metadata.finished_at = None;
            state.metadata.duration_ms = None;
            crate::bash_background::persistence::write_task(&task.paths.json, &state.metadata)
                .expect("persist reset Running metadata");
            let zombie = spawn_unreaped_zombie();
            zombie_pid = zombie.id();
            state.runtime = TaskRuntime::Piped(Some(zombie));
        }
        *task.terminal_at.lock().unwrap() = None;

        assert!(
            is_zombie(zombie_pid),
            "precondition: stand-in child {zombie_pid} must be a zombie before kill"
        );

        // Kill observes the existing marker and finalizes from it.
        registry
            .kill_with_status(&task_id, "session", BgTaskStatus::Killed)
            .expect("kill should succeed");

        {
            let state = task.state.lock().unwrap();
            assert!(
                matches!(state.runtime, TaskRuntime::Piped(None)),
                "child handle must be released after marker-aware kill"
            );
            assert!(state.metadata.status.is_terminal());
        }

        assert!(
            !is_zombie(zombie_pid),
            "issue #91 regression: child {zombie_pid} left as <defunct> zombie \
             after a marker-aware kill"
        );
    }

    #[test]
    fn cleanup_finished_keeps_running_tasks() {
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                true,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();

        registry.cleanup_finished(Duration::ZERO);

        assert!(registry.inner.tasks.lock().unwrap().contains_key(&task_id));
        let _ = registry.kill(&task_id, "session");
    }

    #[cfg(unix)]
    #[test]
    fn rehydrating_sandboxed_task_never_respawns_persisted_command() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let sandbox_temp = storage.path().join("sandbox-temp");
        fs::create_dir(&sandbox_temp).unwrap();
        let launcher_script = project.path().join("sandbox-launch");
        let launcher = PathBuf::from("/bin/sh");
        fs::write(
            &launcher_script,
            "while [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -- ]; then\n    shift\n    exec \"$@\"\n  fi\n  shift\ndone\nexit 78\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&launcher_script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&launcher_script, permissions).unwrap();

        let profile = crate::sandbox_profile::SandboxProfile::build(
            vec![project.path().to_path_buf()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            sandbox_temp,
        )
        .unwrap();
        let plan = SpawnPlan::launcher_for_test(profile, launcher);
        let spawn_marker = project.path().join("spawn-count");
        let stop_marker = project.path().join("stop-command");
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let command = format!(
            "printf 'spawn\\n' >> {}; while [ ! -e {} ]; do sleep 0.05; done",
            quote(&spawn_marker),
            quote(&stop_marker)
        );

        let original = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let task_id = original
            .spawn(
                plan,
                &command,
                "sandbox-rehydrate".to_string(),
                project.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                storage.path().to_path_buf(),
                10,
                true,
                false,
                Some(project.path().to_path_buf()),
            )
            .unwrap();
        let started = Instant::now();
        while !spawn_marker.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "original sandboxed task did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        original.detach();

        let restarted = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        restarted
            .replay_session(storage.path(), "sandbox-rehydrate")
            .unwrap();
        let replayed = restarted
            .status(
                &task_id,
                "sandbox-rehydrate",
                Some(project.path()),
                Some(storage.path()),
                4096,
            )
            .expect("rehydrated sandbox task");
        assert_eq!(replayed.info.status, BgTaskStatus::Running);
        assert!(replayed.sandbox_native);

        std::thread::sleep(Duration::from_millis(650));
        assert_eq!(
            fs::read_to_string(&spawn_marker).unwrap().lines().count(),
            1,
            "registry replay must observe the persisted process without spawning its command"
        );

        fs::write(&stop_marker, "stop").unwrap();
        let terminal = wait_for_terminal_snapshot(
            &restarted,
            &task_id,
            "sandbox-rehydrate",
            project.path(),
            storage.path(),
        );
        assert_eq!(terminal.info.status, BgTaskStatus::Completed);
        assert_eq!(
            fs::read_to_string(&spawn_marker).unwrap().lines().count(),
            1
        );
        restarted.detach();
    }

    #[cfg(windows)]
    fn wait_for_file(path: &Path) -> String {
        // Task io/ artifacts are now pre-created empty (O_EXCL) at spawn under
        // the control/io split, then filled by the child (stdout/stderr) or the
        // daemon after observing exit (the exit marker). Existence is therefore
        // no longer a readiness signal — wait for non-empty content, matching
        // production's read_exit_marker, which treats an empty marker as "not
        // yet written".
        let started = Instant::now();
        loop {
            if let Ok(content) = fs::read_to_string(path) {
                if !content.trim().is_empty() {
                    return content;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "timed out waiting for non-empty {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[cfg(windows)]
    fn spawn_windows_registry_command(
        command: &str,
    ) -> (BgTaskRegistry, tempfile::TempDir, String) {
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
        let dir = tempfile::tempdir().unwrap();
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                command,
                "session".to_string(),
                dir.path().to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                dir.path().to_path_buf(),
                10,
                false,
                false,
                Some(dir.path().to_path_buf()),
            )
            .unwrap();
        (registry, dir, task_id)
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_writes_exit_marker_for_zero_exit() {
        let (registry, _dir, task_id) = spawn_windows_registry_command("cmd /c exit 0");
        let exit_path = registry.task_exit_path(&task_id, "session").unwrap();

        let content = wait_for_file(&exit_path);

        assert_eq!(content.trim(), "0");
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_writes_exit_marker_for_nonzero_exit() {
        let (registry, _dir, task_id) = spawn_windows_registry_command("cmd /c exit 42");
        let exit_path = registry.task_exit_path(&task_id, "session").unwrap();

        let content = wait_for_file(&exit_path);

        assert_eq!(content.trim(), "42");
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_captures_stdout_to_disk() {
        let (registry, _dir, task_id) = spawn_windows_registry_command("cmd /c echo hello");
        let task = registry.task_for_session(&task_id, "session").unwrap();
        let stdout_path = task.paths.stdout.clone();
        let exit_path = task.paths.exit.clone();

        let _ = wait_for_file(&exit_path);
        let stdout = fs::read_to_string(stdout_path).expect("read stdout");

        assert!(stdout.contains("hello"), "stdout was {stdout:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_uses_pwsh_when_available() {
        // Without $SHELL set, $SHELL probe yields None and pwsh wins.
        // (We intentionally pass None for shell_env to keep this test
        // independent of the runner's actual env.)
        let candidates = crate::windows_shell::shell_candidates_with(
            |binary| match binary {
                "pwsh.exe" => Some(std::path::PathBuf::from(r"C:\pwsh\pwsh.exe")),
                "powershell.exe" => Some(std::path::PathBuf::from(r"C:\ps\powershell.exe")),
                _ => None,
            },
            || None,
        );
        let shell = candidates.first().expect("at least one candidate").clone();
        assert_eq!(shell, crate::windows_shell::WindowsShell::Pwsh);
        assert_eq!(shell.binary().as_ref(), "pwsh.exe");
    }

    /// Windows wrappers return the command's status; the daemon writes the
    /// authoritative exit marker through its retained handle.
    #[cfg(windows)]
    #[test]
    fn windows_shell_cmd_wrapper_writes_marker_via_temp_rename() {
        let exit_path = Path::new(r"C:\Temp\bash-test.exit");
        let script =
            crate::windows_shell::WindowsShell::Cmd.wrapper_script("cmd /c exit 42", exit_path);

        assert!(
            script.contains("set CODE=%ERRORLEVEL%"),
            "wrapper must capture the child exit code: {script}"
        );
        assert!(
            script.contains("exit /B %CODE%"),
            "wrapper must propagate the child exit code: {script}"
        );
        // The child records its own exit marker into io/ via temp-file +
        // rename so detached tasks whose spawning daemon is gone still report
        // exit. In-place writes are blocked by the daemon's retained io/exit
        // handle; the rename succeeds (FILE_SHARE_DELETE) and swaps atomically.
        assert!(
            script.contains("bash-test.exit"),
            "wrapper must target the exit marker path: {script}"
        );
        assert!(
            script.contains("move /Y"),
            "wrapper must write the marker atomically via temp-file + rename: {script}"
        );
    }

    /// `bg_command()` for Cmd no longer needs `/V:ON` — the wrapper is now
    /// written to a `.bat` file where batch-line evaluation captures
    /// `%ERRORLEVEL%` correctly without delayed expansion. We still need
    /// `/D` (skip AutoRun) and `/S` (simple quote-stripping for paths with
    /// internal `"`-quoting from `cmd_quote`).
    #[cfg(windows)]
    #[test]
    fn windows_shell_cmd_bg_command_uses_minimal_cmd_flags() {
        use crate::windows_shell::WindowsShell;
        let cmd = WindowsShell::Cmd.bg_command("echo wrapped");
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        let args_strs: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert_eq!(
            args_strs,
            vec!["/D", "/S", "/C", "echo wrapped"],
            "Cmd::bg_command must prepend /D /S /C"
        );
    }

    /// PowerShell variants don't need `/V:ON`-style flags; their
    /// `bg_command()` args stay on the standard `-Command` path.
    #[cfg(windows)]
    #[test]
    fn windows_shell_pwsh_bg_command_uses_standard_args() {
        use crate::windows_shell::WindowsShell;
        let cmd = WindowsShell::Pwsh.bg_command("Get-Date");
        let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
        let args_strs: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
        assert!(
            args_strs.contains(&"-Command"),
            "Pwsh::bg_command must use -Command: {args_strs:?}"
        );
        assert!(
            args_strs.contains(&"Get-Date"),
            "Pwsh::bg_command must include the user command body"
        );
    }

    fn registry_with_db_and_frames(
        storage: &Path,
    ) -> (
        BgTaskRegistry,
        Arc<Mutex<TrackedConnection>>,
        Arc<Mutex<Vec<PushFrame>>>,
    ) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&frames);
        let sender: crate::context::ProgressSender = Arc::new(Box::new(move |frame| {
            captured.lock().unwrap().push(frame);
        })
            as Box<dyn Fn(PushFrame) + Send + Sync>);
        let registry = BgTaskRegistry::new(Arc::new(Mutex::new(Some(sender))));
        registry.set_harness(Harness::Opencode);
        let conn = crate::db::open(&storage.join("aft.db")).expect("open test DB");
        let shared = Arc::new(Mutex::new(conn));
        registry.set_db_pool(shared.clone());
        (registry, shared, frames)
    }

    fn pattern_match_frames(frames: &Mutex<Vec<PushFrame>>) -> Vec<BashPatternMatchFrame> {
        frames
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| match frame {
                PushFrame::BashPatternMatch(frame) => Some(frame.clone()),
                _ => None,
            })
            .collect()
    }

    fn install_delivered_terminal_with_pending_watch(
        registry: &BgTaskRegistry,
        db: &Arc<Mutex<TrackedConnection>>,
        storage: &Path,
        task_id: &str,
    ) -> TaskPaths {
        let paths = task_paths(storage, "session", task_id).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            "session".to_string(),
            "false".to_string(),
            storage.to_path_buf(),
            Some(storage.to_path_buf()),
            None,
            true,
            true,
        );
        metadata.mark_terminal(BgTaskStatus::Failed, Some(1), None);
        metadata.completion_delivered = true;
        write_task(&paths.json, &metadata).unwrap();
        {
            let conn = db.lock().unwrap();
            crate::db::bash_tasks::upsert_bash_task(
                &conn,
                &metadata.to_bash_task_row("opencode", &paths).unwrap(),
            )
            .unwrap();
            crate::db::bash_watches::upsert_bash_pattern_watch(
                &conn,
                &BashPatternWatchRow {
                    harness: "opencode".into(),
                    session_id: "session".into(),
                    task_id: task_id.into(),
                    watch_id: "watch-00000001".into(),
                    pattern_kind: "substring".into(),
                    pattern: "(fail)".into(),
                    once: true,
                    created_at: 1,
                    stdout_offset: 756_243,
                    stderr_offset: 0,
                    pty_offset: 0,
                    scanning: false,
                    pending_match: true,
                    match_text: Some("(fail)".into()),
                    match_offset: Some(756_237),
                    match_context: Some("release output ... (fail)".into()),
                },
            )
            .unwrap();
        }
        registry
            .insert_rehydrated_task(metadata, paths.clone(), true, None)
            .unwrap();
        paths
    }

    /// The window a concurrent spawn leaves open: the task directory exists but
    /// neither `control/` nor the metadata do yet. GC must leave it alone; an
    /// identical directory older than the grace is still quarantined, so the
    /// skip is age-bound rather than a blanket exemption.
    #[test]
    fn gc_skips_a_task_directory_still_being_created_but_quarantines_an_abandoned_one() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let registry = BgTaskRegistry::default();
        let session_dir = session_tasks_dir(storage, "session");
        let young = session_dir.join("bash-0000000000000301");
        let abandoned = session_dir.join("bash-0000000000000302");
        fs::create_dir_all(&young).unwrap();
        fs::create_dir_all(&abandoned).unwrap();
        let old = SystemTime::now() - Duration::from_secs(6 * 60);
        filetime::set_file_mtime(&abandoned, filetime::FileTime::from_system_time(old)).unwrap();

        registry.maybe_gc_persisted(storage).unwrap();

        assert!(
            young.is_dir(),
            "a task directory younger than the grace was quarantined mid-creation"
        );
        assert!(
            !abandoned.is_dir(),
            "an empty task directory older than the grace must still be quarantined"
        );
        let quarantined = fs::read_dir(storage.join("bash-tasks-quarantine"))
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        assert_eq!(
            quarantined, 1,
            "exactly the abandoned layout is quarantined"
        );
    }

    #[cfg(unix)]
    #[test]
    fn gc_refuses_to_delete_or_quarantine_a_recorded_live_process() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, db, _frames) = registry_with_db_and_frames(storage);
        let task_id = "bash-0000000000000198";
        let paths = task_paths(storage, "session", task_id).unwrap();
        let mut running = PersistedTask::starting(
            task_id.to_string(),
            "session".to_string(),
            "live-process-canary".to_string(),
            storage.to_path_buf(),
            Some(storage.to_path_buf()),
            None,
            true,
            false,
        );
        // Own-PID is safe here because GC only CHECKS liveness (it refuses
        // to delete live bundles, never signals them). Kill-path tests must
        // spawn a disposable child instead - see bash_kill.rs.
        running.mark_running(std::process::id(), std::process::id() as i32);
        write_task(&paths.json, &running).unwrap();
        fs::write(&paths.stdout, b"").unwrap();
        fs::write(&paths.stderr, b"").unwrap();
        {
            let conn = db.lock().unwrap();
            crate::db::bash_tasks::upsert_bash_task(
                &conn,
                &running.to_bash_task_row("opencode", &paths).unwrap(),
            )
            .unwrap();
        }
        let running_json = fs::read(&paths.json).unwrap();
        let mut terminal: PersistedTask = serde_json::from_slice(&running_json).unwrap();
        terminal.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        terminal.completion_delivered = true;
        write_task_at(
            &resolve_task_layout(&paths.session_dir, task_id).unwrap(),
            &terminal,
        )
        .unwrap();
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(25 * 60 * 60))
            .unwrap();
        filetime::set_file_mtime(&paths.json, filetime::FileTime::from_system_time(old)).unwrap();

        assert_eq!(registry.maybe_gc_persisted(storage).unwrap(), 0);
        assert!(paths.io_dir.exists(), "GC deleted a live task bundle");

        fs::write(&paths.json, b"{corrupt").unwrap();
        filetime::set_file_mtime(&paths.json, filetime::FileTime::from_system_time(old)).unwrap();
        assert_eq!(registry.maybe_gc_persisted(storage).unwrap(), 0);
        assert!(
            paths.io_dir.exists(),
            "GC quarantined a live task with unreadable metadata"
        );

        fs::write(&paths.json, running_json).unwrap();
    }

    #[test]
    fn pattern_watch_survives_registry_teardown_and_rehydrate() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, _db, frames) = registry_with_db_and_frames(storage);
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                storage.to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                storage.to_path_buf(),
                10,
                true,
                false,
                Some(storage.to_path_buf()),
            )
            .unwrap();
        registry
            .register_watch(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                true,
            )
            .unwrap();
        let task = registry.task_for_session(&task_id, "session").unwrap();
        // Simulate bridge death: drop in-memory watches only. Durable rows remain.
        registry.clear_task_watch_state(&task_id);
        assert_eq!(registry.active_watch_count(&task_id), 0);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&task.paths.stdout)
            .unwrap()
            .write_all(b"READY\n")
            .unwrap();
        frames.lock().unwrap().clear();

        let (replayed, _db2, replay_frames) = registry_with_db_and_frames(storage);
        // Keep the original process's task row out of the way; replay loads from DB/disk.
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        replayed
            .replay_session_inner(storage, "session", None)
            .unwrap();

        let matches = pattern_match_frames(&replay_frames);
        assert!(
            matches.iter().any(|frame| {
                frame.task_id == task_id
                    && frame.reason == "pattern_match"
                    && frame.match_text == "READY"
            }),
            "rehydrate should deliver gap match: {matches:?}"
        );
    }

    #[test]
    fn pattern_watch_gap_match_between_teardown_and_rehydrate_delivers_once() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, _db, frames) = registry_with_db_and_frames(storage);
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                storage.to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                storage.to_path_buf(),
                10,
                true,
                false,
                Some(storage.to_path_buf()),
            )
            .unwrap();
        registry
            .register_watch(
                task_id.clone(),
                WatchPattern::Substring("GAP-HIT".into()),
                true,
            )
            .unwrap();
        let task = registry.task_for_session(&task_id, "session").unwrap();
        let cursor_before = registry.watch_stream_cursors(&task_id).0;
        registry.clear_task_watch_state(&task_id);

        // Bytes land while the watch registry is down.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&task.paths.stdout)
            .unwrap()
            .write_all(b"prefix GAP-HIT suffix\n")
            .unwrap();
        frames.lock().unwrap().clear();

        let (replayed, _db2, replay_frames) = registry_with_db_and_frames(storage);
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        replayed
            .replay_session_inner(storage, "session", None)
            .unwrap();

        let matches: Vec<_> = pattern_match_frames(&replay_frames)
            .into_iter()
            .filter(|frame| frame.task_id == task_id && frame.match_text.contains("GAP-HIT"))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "gap match must deliver exactly once: {matches:?}"
        );
        assert!(
            matches[0].match_offset >= cursor_before,
            "match offset should be at/after the persisted cursor ({cursor_before}), got {}",
            matches[0].match_offset
        );
    }

    #[test]
    fn pattern_watch_acked_match_does_not_redeliver_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, db, frames) = registry_with_db_and_frames(storage);
        let task_id = registry
            .spawn(
                SpawnPlan::Unsandboxed,
                LONG_RUNNING_COMMAND,
                "session".to_string(),
                storage.to_path_buf(),
                HashMap::new(),
                Some(Duration::from_secs(30)),
                storage.to_path_buf(),
                10,
                true,
                false,
                Some(storage.to_path_buf()),
            )
            .unwrap();
        registry
            .register_watch(
                task_id.clone(),
                WatchPattern::Substring("READY".into()),
                true,
            )
            .unwrap();
        let task = registry.task_for_session(&task_id, "session").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&task.paths.stdout)
            .unwrap()
            .write_all(b"READY\n")
            .unwrap();
        registry.scan_task_watch_output(&task);
        let delivered = pattern_match_frames(&frames);
        assert!(
            delivered
                .iter()
                .any(|frame| frame.task_id == task_id && frame.match_text == "READY"),
            "live path should deliver match: {delivered:?}"
        );
        // Ack via the same lane completions use.
        assert!(registry
            .ack_completions_for_session(Some("session"), std::slice::from_ref(&task_id))
            .contains(&task_id));
        {
            let conn = db.lock().unwrap();
            let rows = crate::db::bash_watches::list_bash_pattern_watches_for_task(
                &conn, "opencode", "session", &task_id,
            )
            .unwrap();
            assert!(
                rows.is_empty(),
                "acked once-watch rows must be deleted: {rows:?}"
            );
        }

        frames.lock().unwrap().clear();
        let (replayed, _db2, replay_frames) = registry_with_db_and_frames(storage);
        registry
            .inner
            .shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        replayed
            .replay_session_inner(storage, "session", None)
            .unwrap();
        let matches = pattern_match_frames(&replay_frames)
            .into_iter()
            .filter(|frame| frame.task_id == task_id)
            .collect::<Vec<_>>();
        assert!(
            matches.is_empty(),
            "acked match must not re-deliver after restart: {matches:?}"
        );
    }

    #[test]
    fn pending_watch_match_redelivers_after_terminal_cleanup_removes_task_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, db, frames) = registry_with_db_and_frames(storage);
        let task_id = "bash-aaaaaaaaaaaaaaa1";
        let paths = install_delivered_terminal_with_pending_watch(&registry, &db, storage, task_id);
        frames.lock().unwrap().clear();

        registry.cleanup_finished(Duration::ZERO);
        assert!(registry.task(task_id).is_none());
        assert!(
            !paths.json.exists(),
            "cleanup must remove the task metadata bundle"
        );

        let _ = registry.drain_completions_for_session(Some("session"));
        let matches = pattern_match_frames(&frames);
        assert!(
            matches.iter().any(|frame| {
                frame.task_id == task_id
                    && frame.watch_id == "watch-00000001"
                    && frame.match_text == "(fail)"
            }),
            "durable pending match must redeliver without an in-memory task: {matches:?}"
        );
        assert!(registry
            .ack_completions_for_session(Some("session"), &[task_id.to_string()])
            .contains(&task_id.to_string()));
        let rows = crate::db::bash_watches::list_bash_pattern_watches_for_task(
            &db.lock().unwrap(),
            "opencode",
            "session",
            task_id,
        )
        .unwrap();
        assert!(rows.is_empty(), "ack must end durable redelivery");
    }

    #[test]
    fn status_uses_intact_terminal_db_row_after_task_bundle_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, db, _frames) = registry_with_db_and_frames(storage);
        let task_id = "bash-aaaaaaaaaaaaaaa2";
        let paths = install_delivered_terminal_with_pending_watch(&registry, &db, storage, task_id);

        registry.cleanup_finished(Duration::ZERO);
        assert!(registry.task(task_id).is_none());
        assert!(
            !paths.json.exists(),
            "cleanup must remove the task metadata bundle"
        );
        assert!(
            crate::db::bash_tasks::get_bash_task(
                &db.lock().unwrap(),
                "opencode",
                "session",
                task_id
            )
            .unwrap()
            .is_some(),
            "cleanup must retain the terminal database row"
        );

        let snapshot = registry
            .status(
                task_id,
                "session",
                Some(storage),
                Some(storage),
                RUNNING_OUTPUT_PREVIEW_BYTES,
            )
            .expect("intact terminal row must remain visible after artifact cleanup");
        assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
        assert_eq!(snapshot.exit_code, Some(1));
        assert!(snapshot.info.duration_ms.is_some());
    }

    #[test]
    fn pattern_watch_rows_become_pending_tombstones_when_task_is_gc_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let (registry, db, _frames) = registry_with_db_and_frames(storage);
        let task_id = "bash-aaaaaaaaaaaaaaaa";
        let paths = task_paths(storage, "session", task_id).unwrap();
        let mut metadata = PersistedTask::starting(
            task_id.to_string(),
            "session".to_string(),
            "true".to_string(),
            storage.to_path_buf(),
            Some(storage.to_path_buf()),
            None,
            true,
            true,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        metadata.completion_delivered = true;
        write_task(&paths.json, &metadata).unwrap();
        {
            let conn = db.lock().unwrap();
            crate::db::bash_tasks::upsert_bash_task(
                &conn,
                &metadata.to_bash_task_row("opencode", &paths).unwrap(),
            )
            .unwrap();
            crate::db::bash_watches::upsert_bash_pattern_watch(
                &conn,
                &BashPatternWatchRow {
                    harness: "opencode".into(),
                    session_id: "session".into(),
                    task_id: task_id.into(),
                    watch_id: "watch-00000001".into(),
                    pattern_kind: "substring".into(),
                    pattern: "x".into(),
                    once: true,
                    created_at: 1,
                    stdout_offset: 0,
                    stderr_offset: 0,
                    pty_offset: 0,
                    scanning: true,
                    pending_match: false,
                    match_text: None,
                    match_offset: None,
                    match_context: None,
                },
            )
            .unwrap();
        }
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(25 * 60 * 60))
            .unwrap();
        filetime::set_file_mtime(&paths.json, filetime::FileTime::from_system_time(old)).unwrap();

        let deleted = registry.maybe_gc_persisted(storage).unwrap();
        assert!(
            deleted >= 1,
            "expected GC to delete the terminal task bundle"
        );
        let conn = db.lock().unwrap();
        let watches = crate::db::bash_watches::list_bash_pattern_watches_for_task(
            &conn, "opencode", "session", task_id,
        )
        .unwrap();
        assert_eq!(watches.len(), 1, "watch tombstone must remain until ack");
        assert!(!watches[0].scanning);
        assert!(watches[0].pending_match);
        assert_eq!(
            watches[0].match_text.as_deref(),
            Some(WATCH_TARGET_ERASED_TEXT)
        );
    }
}
