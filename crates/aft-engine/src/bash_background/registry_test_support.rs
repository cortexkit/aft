use super::*;
use crate::bash_background::persistence::{task_paths, write_task};
use std::io::Write;

#[cfg(unix)]
pub(super) const LONG_RUNNING_COMMAND: &str = "sleep 5";
#[cfg(windows)]
pub(super) const LONG_RUNNING_COMMAND: &str = "ping -n 6 127.0.0.1";

pub fn wait_fixture(
    explicit_timeout: Option<u64>,
) -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = crate::context::AppContext::from_app(
        crate::context::App::default_shared(),
        crate::config::Config {
            project_root: Some(dir.path().to_path_buf()),
            storage_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    let registry = ctx.bash_background();
    let task_id = "bash-0000000000000001";
    let session_id = "head-session";
    let paths = task_paths(dir.path(), session_id, task_id).unwrap();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.into(),
        session_id.into(),
        "fake running command".into(),
        dir.path().to_path_buf(),
        Some(dir.path().to_path_buf()),
        Some(explicit_timeout.unwrap_or(1_800_000)),
        false,
        false,
    );
    metadata.default_hard_kill = explicit_timeout.is_none();
    metadata.started_at = unix_millis().saturating_sub(1_900);
    // Insert a terminal fixture to avoid restart renewal, then make it running.
    // Its backdated start exercises elapsed-time renewal without sleeping or
    // spawning a process; no watchdog runs in this fixture.
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    registry
        .insert_rehydrated_task(metadata, paths, false)
        .unwrap();
    let task = registry.task_for_session(task_id, session_id).unwrap();
    assert!(task.started.elapsed() >= Duration::from_millis(1_900));
    assert_eq!(task.hard_kill_renewable, explicit_timeout.is_none());
    {
        let mut state = task.state.lock().unwrap();
        state.metadata.status = BgTaskStatus::Running;
        state.metadata.finished_at = None;
        state.metadata.exit_code = None;
        state.metadata.duration_ms = None;
        write_task(&task.paths.json, &state.metadata).unwrap();
    }
    (dir, ctx, task)
}

pub fn register_unpersisted_watch(
    registry: &BgTaskRegistry,
    id: String,
    pattern: WatchPattern,
    once: bool,
) {
    registry
        .inner
        .watch_registry
        .lock()
        .unwrap()
        .register(id, pattern, once)
        .unwrap();
}

pub use super::completion_preview::capture_reads::DISPLAY_LINE_FINISH_BYTES;

pub fn display_bodies(
    registry: &BgTaskRegistry,
    task: &Arc<BgTask>,
    metadata: &PersistedTask,
    buffer: &BgBuffer,
) -> crate::bash_background::frames::BashBodies {
    registry.display_bodies(task, metadata, buffer)
}

#[cfg(unix)]
#[path = "ordering_fixture.rs"]
pub mod ordering;

pub fn set_completion_secrets(task: &BgTask, values: crate::log_redact::SecretValues) {
    *task.completion_secrets.lock().unwrap() = Some(values);
}

pub fn capture_display_terminal(
    registry: &BgTaskRegistry,
    task: &Arc<BgTask>,
    stale: &PersistedTask,
    buffer: &BgBuffer,
) {
    registry.capture_display_terminal(task, stale, buffer);
}

#[path = "conpty_fixture.rs"]
pub(crate) mod conpty;
pub use conpty::Fixture as ConPtyFixture;

#[cfg(target_os = "linux")]
#[path = "scope_fixture.rs"]
pub mod scopes;

#[path = "watchdog_fixture.rs"]
mod watchdog;
pub use watchdog::watchdog_fixture;

pub fn cleanup_finished_at(registry: &BgTaskRegistry, retention: Duration, now: Instant) {
    registry.cleanup_finished_at(retention, now);
}

pub fn enqueue_counted_completion(
    registry: &BgTaskRegistry,
    metadata: &PersistedTask,
    paths: &TaskPaths,
) {
    registry.enqueue_completion_from_parts(
        metadata,
        None,
        Some(paths),
        false,
        None,
        CompletionTokenPolicy::Count,
    );
}

pub mod registry_cases {
    use super::*;
    pub use super::{
        append_each_line_output, each_line_running_fixture, each_line_running_fixture_with_once,
        insert_terminal_piped_task, lifecycle_running_fixture, registry_with_db_and_frames,
        KillTasksOnDrop,
    };

    pub fn reset_full_artifact_read_calls() {
        ARTIFACT_READ_ALL_CALLS.with(|calls| calls.set(0));
    }
    pub fn full_artifact_read_calls() -> usize {
        ARTIFACT_READ_ALL_CALLS.with(std::cell::Cell::get)
    }
    pub fn random_slug() -> String {
        super::super::random_slug()
    }
    pub fn terminal_metadata_from_marker(
        metadata: PersistedTask,
        marker: ExitMarker,
        reason: Option<String>,
    ) -> PersistedTask {
        super::super::terminal_metadata_from_marker(metadata, marker, reason)
    }
    pub fn terminal_db_row_snapshot(row: BashTaskRow, metadata: PersistedTask) -> BgTaskSnapshot {
        super::super::terminal_db_row_snapshot(row, metadata)
    }
    pub fn persist_watch_match(
        registry: &BgTaskRegistry,
        session: &str,
        task: &str,
        matched: &mut PatternMatch,
        stdout: u64,
        stderr: u64,
        pty: u64,
    ) {
        registry.persist_watch_match(session, task, matched, Some((stdout, stderr, pty)));
    }
    pub fn ack_persisted_watches_for_task(
        registry: &BgTaskRegistry,
        session: &str,
        task: &str,
        terminal: bool,
    ) {
        registry.ack_persisted_watches_for_task(session, task, terminal);
    }
}

pub fn insert_terminal_piped_task(
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
        .insert_rehydrated_task(metadata, paths, true)
        .expect("insert terminal task");
    let task = registry.task_for_session(&task_id, "session").unwrap();
    // This fixture models a live command; replay behavior has separate tests.
    *task.completion_secrets.lock().unwrap() = Some(Default::default());
    (task_id, task)
}

pub fn lifecycle_running_fixture(
    registry: &BgTaskRegistry,
    dir: &tempfile::TempDir,
) -> Arc<BgTask> {
    let (_, task) = insert_terminal_piped_task(registry, dir, "true", "", "", false);
    let mut state = task.lock_state().unwrap();
    state.metadata.status = BgTaskStatus::Running;
    state.metadata.notify_on_completion = false;
    state.metadata.completion_delivered = false;
    write_task(&task.paths.json, &state.metadata).unwrap();
    drop(state);
    task.holds_background_slot.store(false, Ordering::SeqCst);
    task
}

pub fn registry_with_db_and_frames(
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

/// Kills every task the registry still holds when the test ends, whether
/// it passed or panicked. Background children are detached so they can
/// outlive a bridge; one a test leaves running outlives the test process
/// too and, on Windows, keeps the test runner's output handles open.
pub struct KillTasksOnDrop(BgTaskRegistry);

impl KillTasksOnDrop {
    pub fn new(registry: BgTaskRegistry) -> Self {
        Self(registry)
    }
}

impl Drop for KillTasksOnDrop {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

pub fn each_line_running_fixture(
    storage: &Path,
) -> (
    BgTaskRegistry,
    Arc<BgTask>,
    Arc<Mutex<Vec<PushFrame>>>,
    String,
) {
    each_line_running_fixture_with_once(storage, false)
}

pub fn each_line_running_fixture_with_once(
    storage: &Path,
    once: bool,
) -> (
    BgTaskRegistry,
    Arc<BgTask>,
    Arc<Mutex<Vec<PushFrame>>>,
    String,
) {
    let (registry, _, frames) = registry_with_db_and_frames(storage);
    let task_id = registry
        .spawn(
            SpawnPlan::Unsandboxed,
            LONG_RUNNING_COMMAND,
            "session".into(),
            storage.to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            None,
            storage.to_path_buf(),
            10,
            true,
            false,
            Some(storage.to_path_buf()),
        )
        .unwrap();
    registry
        .inner
        .shutdown
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let watch = registry
        .register_watch_delivery(
            task_id.clone(),
            WatchPattern::Substring("READY".into()),
            once,
            true,
        )
        .unwrap();
    let task = registry.task_for_session(&task_id, "session").unwrap();
    (registry, task, frames, watch)
}

pub fn append_each_line_output(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

pub fn terminal_fixture() -> (tempfile::TempDir, crate::context::AppContext, Arc<BgTask>) {
    let (dir, ctx, task) = wait_fixture(None);
    assert_eq!(ctx.config().bash.worker_wait_max_ms, 1_800_000);
    {
        let mut state = task.state.lock().unwrap();
        state
            .metadata
            .mark_terminal(BgTaskStatus::Completed, Some(0), None);
        state.metadata.compressed = true;
        state.terminal_output_cache = None;
        write_task(&task.paths.json, &state.metadata).unwrap();
    }
    std::fs::write(&task.paths.stdout, "identical output\n").unwrap();
    std::fs::write(&task.paths.stderr, "").unwrap();
    (dir, ctx, task)
}

pub fn terminal_task(
    registry: &BgTaskRegistry,
    fixture: &tempfile::TempDir,
    stdout: &str,
    compressed: bool,
) -> std::sync::Arc<BgTask> {
    let (_, task) = insert_terminal_piped_task(registry, fixture, "cmd", stdout, "", compressed);
    task
}

pub fn with_secret(task: &BgTask, value: &str) {
    *task.completion_secrets.lock().unwrap() =
        Some(crate::log_redact::SecretValues::from_environment([(
            std::ffi::OsStr::new("API_TOKEN"),
            std::ffi::OsStr::new(value),
        )]));
}

/// Lines of `filler` text, `bytes` long in total.
pub fn filler(bytes: usize) -> String {
    let line = "compiling crate foo v0.1.0 (/src/foo)\n";
    let mut text = line.repeat(bytes / line.len());
    text.push_str(&"x".repeat(bytes - text.len()));
    text
}

thread_local! {
    /// Sleep added to every page read on this thread, so a test can make
    /// draining a captured backlog slower than a caller's reply budget.
    pub static ARTIFACT_PAGE_DELAY: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

/// Checks frame-loop lock ownership directly, without treating scheduler latency
/// as evidence of a blocking lock. This guard must stay on its creating thread.
pub struct TaskStateLockBanForTest {
    previous: bool,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

pub fn forbid_task_state_locks_for_test() -> TaskStateLockBanForTest {
    TaskStateLockBanForTest {
        previous: TASK_STATE_LOCKS_FORBIDDEN.with(|flag| flag.replace(true)),
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for TaskStateLockBanForTest {
    fn drop(&mut self) {
        TASK_STATE_LOCKS_FORBIDDEN.with(|flag| flag.set(self.previous));
    }
}
