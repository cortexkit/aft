use super::*;

pub struct BgTask {
    pub(super) start_view: Mutex<Option<crate::gh_shim_speech::start_view::StartView>>,
    #[cfg(unix)]
    pub(super) provider_preflight: Mutex<Option<Arc<crate::shell_env_provider::PreflightLayout>>>,
    pub(super) changed_files: Option<Arc<crate::bash_changed_files::Capture>>,
    /// Absent after replay: secret values must never be persisted or recovered
    /// from payload files. Completion notices withhold output when this is absent.
    pub(super) completion_secrets: Mutex<Option<crate::log_redact::SecretValues>>,
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(super) launching_identity: Option<super::super::TaskIdentity>,
    pub(crate) paths: TaskPaths,
    pub(super) artifact_root: PathBuf,
    /// Canonical root identity captured at registration and retained on deletion.
    /// Reclaim probes compare it without filesystem calls on the frame loop.
    pub(super) project_root: PathBuf,
    pub(crate) started: Instant,
    /// Whether this task holds one of the project root's background slots
    /// (`max_background_bash_tasks`): true for a background or PTY launch and
    /// for a task reloaded after a restart, false for a foreground command
    /// while its caller waits on it. Promotion sets it (see
    /// [`super::super::TaskSlot::Foreground`]).
    pub(super) holds_background_slot: AtomicBool,
    /// Completion of the PTY reader thread, which owns an output-file handle
    /// independently of `PtyRuntime` while it drains the pseudoterminal. Piped
    /// and replayed tasks have no reader and start complete.
    pub(super) terminal_reader_done: Arc<AtomicBool>,
    /// Whether a delegated worker's wait may push this task's hard kill later
    /// ([`BgTaskRegistry::renew_hard_kill`]): true only for the implicit
    /// default kill, which the task's record carries (`default_hard_kill`) so
    /// a task reloaded after a restart stays renewable.
    pub(super) hard_kill_renewable: bool,
    /// The hard-kill limit last written to the task's record, so renewals
    /// write it at most about once a minute (see [`RENEWAL_PERSIST_STEP_MS`]).
    pub(super) persisted_hard_kill_ms: AtomicU64,
    pub(crate) last_reminder_at: Mutex<Option<Instant>>,
    pub(crate) terminal_at: Mutex<Option<Instant>>,
    /// Terminal launcher images cannot be created again. Once removal or an
    /// absent image is confirmed, later cleanup passes need no task/IO probes.
    pub(super) terminal_image_cleaned: AtomicBool,
    /// Serialize terminal notification routing, including watch safety nets.
    /// Delivery acknowledgement remains separate so pending results can replay.
    pub(super) terminal_notification_routed: Mutex<bool>,
    /// The project registry whose sessions receive this task's completion
    /// and cancelled-call notices, when the task is shared between registries
    /// (see `SavedRemoteTaskClaim`): the first registry that is not a
    /// `bash.kill_scope` recovery registry to restore or register it. `None`
    /// for a task only one registry holds; that registry publishes its notices.
    pub(super) notice_registry: Mutex<Option<std::sync::Weak<RegistryInner>>>,
    /// Lets the watchdog's per-tick exit-marker check skip the full marker
    /// read while the marker is still the empty file it found last time.
    pub(super) exit_marker_probe: Mutex<super::super::persistence::ExitMarkerProbe>,
    #[cfg(unix)]
    pub(super) adopted_exit: Mutex<super::super::exit_observer::AdoptedExit>,
    pub(crate) state: Mutex<BgTaskState>,
    /// The remote worker has observed a stream record or settled a submission
    /// failure. Kept in memory: launch callers need this only in this process.
    pub(crate) remote_launch_ready: AtomicBool,
    /// Launch-only projection, published while state is locked but read without
    /// that lock. Reply deadlines must not confuse a queued state read with a
    /// runner that has not accepted the job.
    pub(super) runon_launch_state: Mutex<Option<RunonLaunchState>>,
    /// A remote worker thread is running for this task. At most one may run
    /// per task: `start_remote_worker` claims this flag before spawning one,
    /// and the worker clears it as its last step.
    #[cfg(unix)]
    pub(crate) remote_worker_running: AtomicBool,
    pub(super) drain_reply: BashDrainReplyCache,
    /// Cache construction is serialized independently of task state: renderers
    /// can read artifacts and invoke callbacks without holding the state lock.
    pub(super) terminal_render: Mutex<()>,
    /// Notified, with `state` locked, when a kill clears
    /// `BgTaskState::kill_in_flight`. Readers that must answer with a settled
    /// status wait on it instead of holding `state` across the kill.
    pub(super) terminal_persist_dirty: AtomicBool,
    pub(crate) timeout_kill_queued: AtomicBool,
    pub(super) kill_settled: std::sync::Condvar,
    /// Notified when the first process-group check saves its result.
    pub(super) descendant_sample_saved: std::sync::Condvar,
    #[cfg(unix)]
    pub(super) remote_cancel_notify: tokio::sync::Notify,
    /// Harness namespace the task's aft.db row is keyed under: the harness of
    /// the route that started it (or of the row it was replayed from). The
    /// root context is shared by routes from several harnesses, so this can
    /// differ from the registry's current `db_harness`.
    pub(super) db_harness: Option<String>,
    /// Orders this task's aft.db row writes, which happen after `state` is
    /// released; see [`DbWriteOrder`]. Shared by every `BgTask` for the same
    /// session and task id in the process.
    pub(super) db_write_order: Arc<DbWriteOrder>,
}

pub enum TaskRuntime {
    Piped(Option<Child>),
    Pty(Option<PtyRuntime>),
}

pub struct BgTaskState {
    pub(crate) metadata: PersistedTask,
    /// Only the latest pre-acceptance line, never written to task metadata.
    pub(crate) remote_admission_note: Option<String>,
    /// The path stays in persisted metadata until this worker result succeeds,
    /// so a crash or a full queue leaves enough information for the stale sweep.
    pub(super) temp_cleanup: Option<crossbeam_channel::Receiver<std::io::Result<()>>>,
    /// Spaces out removal attempts after this task's scratch removal failed.
    pub(super) temp_cleanup_retry: super::super::temp_cleanup::RetryBackoff,
    /// The scratch directory this task used, kept in memory after cleanup has
    /// deleted it and cleared `metadata.sandbox_temp_dir`. A later read of a
    /// path inside it by the same session can then fail as not-found instead
    /// of asking for permission to read a file that no longer exists.
    pub(crate) removed_temp_dir: Option<PathBuf>,
    /// Delete the scratch directory without waiting for its retention time:
    /// the retention sweep found it due, the session's scope ended, or the
    /// task record is being pruned. In memory only; a restart re-derives the
    /// time from the persisted `metadata.sandbox_temp_delete_at`.
    pub(super) scratch_released: bool,
    pub(crate) runtime: TaskRuntime,
    /// Pinned task-io directory and original O_EXCL output handles retained for
    /// the process lifetime. Daemon writes never reopen child-writable names.
    pub(crate) io_handles: Option<TaskIoHandles>,
    pub(crate) detached: bool,
    /// True once an owned child handle has observed the direct child's exit
    /// via `try_wait()` or `wait()`. Used by the two-pass watchdog to skip the racy
    /// `is_process_alive(child_pid)` probe on the second pass — we already
    /// have authoritative evidence that the child is dead, no need to
    /// re-verify via PID liveness which is unreliable on Windows where
    /// PIDs can be recycled within seconds.
    ///
    /// Remains `false` on replay-restored tasks (those have a `child_pid`
    /// but never observed exit via this process's `try_wait()`), so those
    /// continue to fall through to the `is_process_alive` probe path.
    pub(crate) child_exit_observed: bool,
    /// Prevent duplicate completion delivery while the post-exit process-group
    /// sample is waiting for its grace period.
    pub(crate) descendant_sampling_started: bool,
    /// The first check of the process group has started but its result is
    /// not saved yet. Set together with `descendant_sampling_started`, so a
    /// publisher that skips sampling because it has started waits for this to
    /// clear (see `finish_terminal_transition`) instead of sending a notice
    /// built before any check.
    pub(crate) descendant_sample_pending: bool,
    /// Reclamation found no saved process identity still in the terminal
    /// task's group. A stale descendant list, or a reused group number, must
    /// not retain the deleted root. Rechecked after a registry restart.
    pub(super) root_reclaim_group_unowned: bool,
    /// True while a kill has released this lock to signal and reap the
    /// process group. That kill owns the terminal transition until it clears
    /// the flag: the watchdog must not declare the task dead without an exit
    /// marker, and a second kill waits for the first rather than signaling
    /// again.
    pub(crate) kill_in_flight: bool,
    pub(crate) buffer: BgBuffer,
    pub(super) terminal_output_cache: Option<TerminalOutputCache>,
    // The terminal render is immutable until invalidated. Keep its payload
    // total outside the render so the estimate's existing size formula stays
    // unchanged and health does not scan retained recovery lines every tick.
    pub(super) terminal_output_cache_bytes: u64,
    /// PTY timeout intent, a known memory stop, or a piped kill's committed
    /// outcome after observing no natural marker before the signal decision.
    pub(crate) pending_terminal_override: Option<BgTaskStatus>,
}
