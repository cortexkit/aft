use super::BgTask;
use crate::bash_background::persistence::{PersistedTask, TaskPaths};
use crate::bash_background::registry::{
    BgCompletion, BgTaskRegistry, BgTaskSnapshot, ShellReader, ShellSnapshot,
};
use crate::bash_background::SpawnReceipt;
#[cfg(unix)]
use crate::bash_background::{
    registry::RegistryError, BashShell, HardKill, RemoteLaunch, TaskSlot,
};
use crate::blocking_wait::ReplyBudget;
use crate::db::bash_tasks::BashTaskRow;
#[cfg(unix)]
use crate::exec_remote::Verdict;
#[cfg(unix)]
use crate::sandbox_profile::SandboxProfile;
#[cfg(unix)]
use crate::sandbox_spawn::PreparedTask;
use crate::sandbox_spawn::SpawnPlan;
use serde_json::Value;
#[cfg(unix)]
use std::collections::HashMap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub trait BgTaskRegistryTestExt: Sized {
    fn uses_db_pool(
        &self,
        conn: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    ) -> bool;
    fn running_tasks(&self) -> Vec<std::sync::Arc<BgTask>>;
    fn compress_output(
        &self,
        command: &str,
        output: String,
        exit_code: Option<i64>,
    ) -> crate::compress::CompressionResult;
    fn try_has_running_tasks(&self) -> Option<bool>;
    fn cleanup_terminal_task_temp(&self, task: &BgTask) -> std::io::Result<bool>;
    fn cleanup_terminal_task_temp_at(&self, task: &BgTask, now_ms: u64) -> std::io::Result<bool>;
    fn reset_task_access_work(&self);
    fn task_access_work(&self) -> (usize, usize, usize);
    fn set_project_replay_scan_budget_for_test(&self, entries: usize, duration: Duration);
    fn project_replay_scan_total_examined_for_test(&self) -> Option<usize>;
    #[cfg(unix)]
    fn expire_project_replay_backoff_for_test(&self);
    #[cfg(unix)]
    fn pause_watchdog_registration_for_test(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<String>, impl Drop);
    fn scope_fence_unlocked_for_test(&self) -> bool;
    #[cfg(unix)]
    fn task_registration_identity_for_test(&self, task_id: &str) -> Option<usize>;
    #[cfg(unix)]
    fn forget_task_record_for_test(&self, task_id: &str);
    fn route_delivery_snapshot(&self) -> Arc<std::collections::HashSet<String>>;
    fn task_for_test(&self, task_id: &str) -> Option<Arc<BgTask>>;
    fn dual_write_task(&self, paths: &TaskPaths, metadata: &PersistedTask);

    fn status_relaxed(
        &self,
        task_id: &str,
        session_id: &str,
        project_root: &Path,
        storage_dir: &Path,
        preview_bytes: usize,
        allow_terminal_db_fallback: bool,
        kill_wait: Duration,
    ) -> Option<BgTaskSnapshot>;

    fn is_task_terminal(&self, task_id: &str) -> bool;

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    fn install_pty_seal_pause_for_test(
        &self,
        arrived: std::sync::mpsc::Sender<String>,
        release: std::sync::mpsc::Receiver<()>,
    );

    fn task_json_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf>;

    fn task_exit_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf>;

    #[cfg(unix)]
    fn hard_kill_limit_ms_for_test(&self, task_id: &str, session_id: &str) -> Option<u64>;

    #[cfg(unix)]
    fn set_hard_kill_limit_ms_for_test(&self, task_id: &str, session_id: &str, limit_ms: u64);

    #[cfg(unix)]
    fn secret_proxy_address_for_test(&self, task_id: &str) -> Option<std::net::SocketAddr>;

    #[cfg(target_os = "linux")]
    fn spawn_with_shell_in_slot_typed(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        shell: BashShell,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        hard_kill: HardKill,
        memory_limit_mib: Option<u64>,
        storage_dir: PathBuf,
        slot: TaskSlot,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
        linux_scope: bool,
    ) -> Result<String, RegistryError>;

    fn insert_rehydrated_task(
        &self,
        metadata: PersistedTask,
        paths: TaskPaths,
        detached: bool,
    ) -> Result<(), String>;

    fn completion_snapshot_for_task(&self, task: &Arc<BgTask>) -> Option<BgCompletion>;

    fn maybe_emit_long_running_reminder(&self, task: &Arc<BgTask>);

    #[cfg(unix)]
    fn bind_start_view(
        &self,
        task_id: &str,
        view: Option<crate::gh_shim_speech::start_view::StartView>,
    );

    #[cfg(unix)]
    fn arm_secret_proxy(&self, plan: &SpawnPlan, task_id: &str, pid: u32) -> Result<(), String>;

    #[cfg(unix)]
    fn stop_secret_proxy(&self, task_id: &str);

    #[cfg(unix)]
    fn spawn_remote(
        &self,
        launch: RemoteLaunch,
        plan: SpawnPlan,
        command: &str,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        hard_kill: HardKill,
        memory_limit_mib: Option<u64>,
        storage_dir: PathBuf,
        slot: TaskSlot,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
    ) -> Result<String, String>;

    #[cfg(unix)]
    fn remote_terminal(&self, task: &Arc<BgTask>, verdict: Verdict, error: Option<String>);

    fn insert_shell_fixture(&self, metadata: PersistedTask, paths: TaskPaths)
        -> Result<(), String>;

    fn update_shell_fixture(
        &self,
        task_id: &str,
        update: impl FnOnce(&mut PersistedTask),
    ) -> Result<(), String>;

    fn shell_snapshots(&self) -> Vec<ShellSnapshot>;

    fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String>;

    fn scan_task_watch_output(&self, task: &Arc<BgTask>) -> bool;
}

impl BgTaskRegistryTestExt for BgTaskRegistry {
    fn uses_db_pool(
        &self,
        conn: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    ) -> bool {
        self.uses_db_pool(conn)
    }
    fn running_tasks(&self) -> Vec<std::sync::Arc<BgTask>> {
        self.running_tasks()
    }
    fn compress_output(
        &self,
        command: &str,
        output: String,
        exit_code: Option<i64>,
    ) -> crate::compress::CompressionResult {
        self.compress_output(command, output, exit_code)
    }
    fn try_has_running_tasks(&self) -> Option<bool> {
        self.try_has_running_tasks()
    }
    fn cleanup_terminal_task_temp(&self, task: &BgTask) -> std::io::Result<bool> {
        Self::cleanup_terminal_task_temp(self, task)
    }
    fn cleanup_terminal_task_temp_at(&self, task: &BgTask, now_ms: u64) -> std::io::Result<bool> {
        Self::cleanup_terminal_task_temp_at(self, task, now_ms)
    }
    fn reset_task_access_work(&self) {
        Self::reset_task_access_work(self);
    }
    fn task_access_work(&self) -> (usize, usize, usize) {
        Self::task_access_work(self)
    }
    fn set_project_replay_scan_budget_for_test(&self, entries: usize, duration: Duration) {
        Self::set_project_replay_scan_budget_for_test(self, entries, duration);
    }
    fn project_replay_scan_total_examined_for_test(&self) -> Option<usize> {
        Self::project_replay_scan_total_examined_for_test(self)
    }
    #[cfg(unix)]
    fn expire_project_replay_backoff_for_test(&self) {
        Self::expire_project_replay_backoff_for_test(self);
    }
    #[cfg(unix)]
    fn pause_watchdog_registration_for_test(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<String>, impl Drop) {
        Self::pause_watchdog_registration_for_test(self)
    }
    fn scope_fence_unlocked_for_test(&self) -> bool {
        Self::scope_fence_unlocked_for_test(self)
    }
    #[cfg(unix)]
    fn task_registration_identity_for_test(&self, task_id: &str) -> Option<usize> {
        Self::task_registration_identity_for_test(self, task_id)
    }
    #[cfg(unix)]
    fn forget_task_record_for_test(&self, task_id: &str) {
        Self::forget_task_record_for_test(self, task_id);
    }
    fn route_delivery_snapshot(&self) -> Arc<std::collections::HashSet<String>> {
        Self::route_delivery_snapshot(self)
    }
    fn task_for_test(&self, task_id: &str) -> Option<Arc<BgTask>> {
        Self::task_for_test(self, task_id)
    }
    fn dual_write_task(&self, paths: &TaskPaths, metadata: &PersistedTask) {
        BgTaskRegistry::dual_write_task(self, paths, metadata)
    }

    fn status_relaxed(
        &self,
        task_id: &str,
        session_id: &str,
        project_root: &Path,
        storage_dir: &Path,
        preview_bytes: usize,
        allow_terminal_db_fallback: bool,
        kill_wait: Duration,
    ) -> Option<BgTaskSnapshot> {
        BgTaskRegistry::status_relaxed(
            self,
            task_id,
            session_id,
            project_root,
            storage_dir,
            preview_bytes,
            allow_terminal_db_fallback,
            kill_wait,
        )
    }

    fn is_task_terminal(&self, task_id: &str) -> bool {
        BgTaskRegistry::is_task_terminal(self, task_id)
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    fn install_pty_seal_pause_for_test(
        &self,
        arrived: std::sync::mpsc::Sender<String>,
        release: std::sync::mpsc::Receiver<()>,
    ) {
        BgTaskRegistry::install_pty_seal_pause_for_test(self, arrived, release)
    }

    fn task_json_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf> {
        BgTaskRegistry::task_json_path(self, task_id, session_id)
    }

    fn task_exit_path(&self, task_id: &str, session_id: &str) -> Option<PathBuf> {
        BgTaskRegistry::task_exit_path(self, task_id, session_id)
    }

    #[cfg(unix)]
    fn hard_kill_limit_ms_for_test(&self, task_id: &str, session_id: &str) -> Option<u64> {
        BgTaskRegistry::hard_kill_limit_ms_for_test(self, task_id, session_id)
    }

    #[cfg(unix)]
    fn set_hard_kill_limit_ms_for_test(&self, task_id: &str, session_id: &str, limit_ms: u64) {
        BgTaskRegistry::set_hard_kill_limit_ms_for_test(self, task_id, session_id, limit_ms)
    }

    #[cfg(unix)]
    fn secret_proxy_address_for_test(&self, task_id: &str) -> Option<std::net::SocketAddr> {
        BgTaskRegistry::secret_proxy_address_for_test(self, task_id)
    }

    #[cfg(target_os = "linux")]
    fn spawn_with_shell_in_slot_typed(
        &self,
        spawn_plan: SpawnPlan,
        command: &str,
        shell: BashShell,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        hard_kill: HardKill,
        memory_limit_mib: Option<u64>,
        storage_dir: PathBuf,
        slot: TaskSlot,
        notify_on_completion: bool,
        compressed: bool,
        project_root: Option<PathBuf>,
        linux_scope: bool,
    ) -> Result<String, RegistryError> {
        BgTaskRegistry::spawn_with_shell_in_slot_typed(
            self,
            spawn_plan,
            command,
            shell,
            shell_path,
            session_id,
            workdir,
            env,
            hard_kill,
            memory_limit_mib,
            storage_dir,
            slot,
            notify_on_completion,
            compressed,
            project_root,
            linux_scope,
        )
    }

    fn insert_rehydrated_task(
        &self,
        metadata: PersistedTask,
        paths: TaskPaths,
        detached: bool,
    ) -> Result<(), String> {
        BgTaskRegistry::insert_rehydrated_task(self, metadata, paths, detached)
    }

    fn completion_snapshot_for_task(&self, task: &Arc<BgTask>) -> Option<BgCompletion> {
        BgTaskRegistry::completion_snapshot_for_task(self, task)
    }

    fn maybe_emit_long_running_reminder(&self, task: &Arc<BgTask>) {
        BgTaskRegistry::maybe_emit_long_running_reminder(self, task)
    }

    #[cfg(unix)]
    fn bind_start_view(
        &self,
        task_id: &str,
        view: Option<crate::gh_shim_speech::start_view::StartView>,
    ) {
        BgTaskRegistry::bind_start_view(self, task_id, view)
    }

    #[cfg(unix)]
    fn arm_secret_proxy(&self, plan: &SpawnPlan, task_id: &str, pid: u32) -> Result<(), String> {
        BgTaskRegistry::arm_secret_proxy(self, plan, task_id, pid)
    }

    #[cfg(unix)]
    fn stop_secret_proxy(&self, task_id: &str) {
        BgTaskRegistry::stop_secret_proxy(self, task_id)
    }

    #[cfg(unix)]
    fn spawn_remote(
        &self,
        launch: RemoteLaunch,
        plan: SpawnPlan,
        command: &str,
        shell_path: PathBuf,
        session_id: String,
        workdir: PathBuf,
        env: HashMap<String, String>,
        hard_kill: HardKill,
        memory_limit_mib: Option<u64>,
        storage_dir: PathBuf,
        slot: TaskSlot,
        notify: bool,
        compressed: bool,
        root: Option<PathBuf>,
    ) -> Result<String, String> {
        BgTaskRegistry::spawn_remote(
            self,
            launch,
            plan,
            command,
            shell_path,
            session_id,
            workdir,
            env,
            hard_kill,
            memory_limit_mib,
            storage_dir,
            slot,
            notify,
            compressed,
            root,
        )
    }

    #[cfg(unix)]
    fn remote_terminal(&self, task: &Arc<BgTask>, verdict: Verdict, error: Option<String>) {
        BgTaskRegistry::remote_terminal(self, task, verdict, error)
    }

    fn insert_shell_fixture(
        &self,
        metadata: PersistedTask,
        paths: TaskPaths,
    ) -> Result<(), String> {
        BgTaskRegistry::insert_shell_fixture(self, metadata, paths)
    }

    fn update_shell_fixture(
        &self,
        task_id: &str,
        update: impl FnOnce(&mut PersistedTask),
    ) -> Result<(), String> {
        BgTaskRegistry::update_shell_fixture(self, task_id, update)
    }

    fn shell_snapshots(&self) -> Vec<ShellSnapshot> {
        BgTaskRegistry::shell_snapshots(self)
    }

    fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String> {
        BgTaskRegistry::shell_revision(self, task_id, fingerprint)
    }

    fn scan_task_watch_output(&self, task: &Arc<BgTask>) -> bool {
        BgTaskRegistry::scan_task_watch_output(self, task)
    }
}

pub trait SpawnReceiptTestExt: Sized {
    fn new(deadline: std::time::Instant) -> Self;
}

impl SpawnReceiptTestExt for SpawnReceipt {
    fn new(deadline: std::time::Instant) -> Self {
        SpawnReceipt::new(deadline)
    }
}

pub trait ReplyBudgetTestExt: Sized {
    fn with_suspend_clock(self, clock: &'static std::sync::atomic::AtomicU64) -> Self;

    fn with_runtime_clock(self) -> Self;

    fn from_clocks(received_at: Instant, body: &Value, now: Instant, wall_ms: u64) -> Self;
}

impl ReplyBudgetTestExt for ReplyBudget {
    fn with_suspend_clock(self, clock: &'static std::sync::atomic::AtomicU64) -> Self {
        ReplyBudget::with_suspend_clock(self, clock)
    }

    fn with_runtime_clock(self) -> Self {
        ReplyBudget::with_runtime_clock(self)
    }

    fn from_clocks(received_at: Instant, body: &Value, now: Instant, wall_ms: u64) -> Self {
        ReplyBudget::from_clocks(received_at, body, now, wall_ms)
    }
}

pub trait SpawnPlanTestExt: Sized {
    fn is_native_launcher(&self) -> bool;
    fn refused_for_test(code: &'static str) -> Self;

    #[cfg(unix)]
    fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self;

    #[cfg(unix)]
    fn with_prepared_task(self, task: PreparedTask) -> Self;

    #[cfg(unix)]
    fn protects_secret_memory(&self) -> bool;
    #[cfg(unix)]
    fn prepared_task(&self) -> Option<&PreparedTask>;
    fn temp_dir(&self) -> Option<&Path>;
}

impl SpawnPlanTestExt for SpawnPlan {
    fn is_native_launcher(&self) -> bool {
        self.is_native_launcher()
    }
    fn refused_for_test(code: &'static str) -> Self {
        SpawnPlan::refused_for_test(code)
    }

    #[cfg(unix)]
    fn launcher_for_test(profile: SandboxProfile, launcher_path: PathBuf) -> Self {
        SpawnPlan::launcher_for_test(profile, launcher_path)
    }

    #[cfg(unix)]
    fn with_prepared_task(self, task: PreparedTask) -> Self {
        SpawnPlan::with_prepared_task(self, task)
    }

    #[cfg(unix)]
    fn protects_secret_memory(&self) -> bool {
        SpawnPlan::protects_secret_memory(self)
    }
    #[cfg(unix)]
    fn prepared_task(&self) -> Option<&PreparedTask> {
        SpawnPlan::prepared_task(self)
    }
    fn temp_dir(&self) -> Option<&Path> {
        SpawnPlan::temp_dir(self)
    }
}

#[cfg(unix)]
pub trait PreparedTaskTestExt: Sized {
    #[cfg(unix)]
    fn set_secret_runtime(&self, runtime: crate::bash_background::secrets::SharedRuntime);
}

#[cfg(unix)]
impl PreparedTaskTestExt for PreparedTask {
    #[cfg(unix)]
    fn set_secret_runtime(&self, runtime: crate::bash_background::secrets::SharedRuntime) {
        PreparedTask::set_secret_runtime(self, runtime)
    }
}

pub trait ShellReaderTestExt: Sized {
    fn persisted(row: BashTaskRow) -> Option<(ShellSnapshot, Self)>;

    fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String>;
}

impl ShellReaderTestExt for ShellReader {
    fn persisted(row: BashTaskRow) -> Option<(ShellSnapshot, Self)> {
        ShellReader::persisted(row)
    }

    fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String> {
        ShellReader::shell_revision(self, task_id, fingerprint)
    }
}

#[cfg(unix)]
pub trait PreflightTestExt {
    fn plan(&self) -> &SpawnPlan;
    fn task(&self) -> &Option<crate::bash_background::persistence::ResolvedTask>;
    fn task_mut(&mut self) -> &mut Option<crate::bash_background::persistence::ResolvedTask>;
}

#[cfg(unix)]
impl PreflightTestExt for crate::bash_background::secrets::Preflight {
    fn plan(&self) -> &SpawnPlan {
        &self.plan
    }
    fn task(&self) -> &Option<crate::bash_background::persistence::ResolvedTask> {
        &self.task
    }
    fn task_mut(&mut self) -> &mut Option<crate::bash_background::persistence::ResolvedTask> {
        &mut self.task
    }
}

#[cfg(unix)]
pub trait LaunchTestExt {
    fn preflight(&self) -> &crate::bash_background::secrets::Preflight;
    fn preflight_mut(&mut self) -> &mut crate::bash_background::secrets::Preflight;
    fn runtime(&self) -> &crate::bash_background::secrets::SharedRuntime;
}

#[cfg(unix)]
impl LaunchTestExt for crate::bash_background::secrets::Launch {
    fn preflight(&self) -> &crate::bash_background::secrets::Preflight {
        &self.preflight
    }
    fn preflight_mut(&mut self) -> &mut crate::bash_background::secrets::Preflight {
        &mut self.preflight
    }
    fn runtime(&self) -> &crate::bash_background::secrets::SharedRuntime {
        &self.runtime
    }
}

#[cfg(unix)]
pub trait RuntimeTestExt {
    fn address(&self) -> std::net::SocketAddr;
    fn arm(&mut self, pid: u32) -> Result<(), String>;
}

#[cfg(unix)]
impl RuntimeTestExt for crate::bash_background::secrets::Runtime {
    fn address(&self) -> std::net::SocketAddr {
        Self::address(self)
    }
    fn arm(&mut self, pid: u32) -> Result<(), String> {
        Self::arm(self, pid)
    }
}

pub trait PersistedTaskTestExt {
    fn call_key_mut(&mut self) -> &mut Option<crate::bash_background::TaskCallKey>;
    fn memory_policy_mut(&mut self) -> &mut Option<crate::bash_background::MemoryPolicyForTest>;
    fn task_scope_runtime_ms_mut(&mut self) -> &mut Option<u64>;
    fn lifecycle_scope(&self) -> &Option<subc_protocol::scope::ScopeStamp>;
    fn shell_fingerprint_mut(&mut self) -> &mut Option<String>;
    fn lifecycle_scope_mut(&mut self) -> &mut Option<subc_protocol::scope::ScopeStamp>;
}

impl PersistedTaskTestExt for PersistedTask {
    fn call_key_mut(&mut self) -> &mut Option<crate::bash_background::TaskCallKey> {
        &mut self.call_key
    }
    fn memory_policy_mut(&mut self) -> &mut Option<crate::bash_background::MemoryPolicyForTest> {
        &mut self.memory_policy
    }
    fn task_scope_runtime_ms_mut(&mut self) -> &mut Option<u64> {
        &mut self.task_scope_runtime_ms
    }
    fn lifecycle_scope(&self) -> &Option<subc_protocol::scope::ScopeStamp> {
        &self.lifecycle_scope
    }
    fn shell_fingerprint_mut(&mut self) -> &mut Option<String> {
        &mut self.shell_fingerprint
    }
    fn lifecycle_scope_mut(&mut self) -> &mut Option<subc_protocol::scope::ScopeStamp> {
        &mut self.lifecycle_scope
    }
}

pub trait BgTaskStateTestExt {
    fn remote_admission_note(&self) -> &Option<String>;
    fn remote_admission_note_mut(&mut self) -> &mut Option<String>;
}

impl BgTaskStateTestExt for super::BgTaskState {
    fn remote_admission_note(&self) -> &Option<String> {
        &self.remote_admission_note
    }
    fn remote_admission_note_mut(&mut self) -> &mut Option<String> {
        &mut self.remote_admission_note
    }
}

pub trait ToolLaunchPolicyTestExt {
    fn command(
        &self,
        program: &std::ffi::OsStr,
        args: &[std::ffi::OsString],
    ) -> Result<std::process::Command, String>;
    fn resolve(ctx: &crate::context::AppContext)
        -> crate::sandbox_spawn::ToolLaunchPolicy<'static>;
    fn unconfined_for_test() -> crate::sandbox_spawn::ToolLaunchPolicy<'static>;
    #[cfg(unix)]
    fn shared(&self) -> std::rc::Rc<crate::sandbox_spawn::RequestToolLaunch>;
}

impl ToolLaunchPolicyTestExt for crate::sandbox_spawn::ToolLaunchPolicy<'_> {
    fn command(
        &self,
        program: &std::ffi::OsStr,
        args: &[std::ffi::OsString],
    ) -> Result<std::process::Command, String> {
        self.command(program, args)
    }
    fn resolve(
        ctx: &crate::context::AppContext,
    ) -> crate::sandbox_spawn::ToolLaunchPolicy<'static> {
        Self::resolve(ctx)
    }
    fn unconfined_for_test() -> crate::sandbox_spawn::ToolLaunchPolicy<'static> {
        Self::unconfined_for_test()
    }
    #[cfg(unix)]
    fn shared(&self) -> std::rc::Rc<crate::sandbox_spawn::RequestToolLaunch> {
        Self::shared(self)
    }
}

pub trait BgTaskTestExt {
    fn lock_state(
        &self,
    ) -> std::sync::LockResult<
        std::sync::MutexGuard<'_, crate::bash_background::registry::BgTaskState>,
    >;
    fn is_running(&self) -> bool;
    fn task_id(&self) -> &String;
    fn session_id(&self) -> &String;
    fn state(&self) -> &std::sync::Mutex<super::BgTaskState>;
    fn paths(&self) -> &TaskPaths;
    fn last_reminder_at(&self) -> &std::sync::Mutex<Option<Instant>>;
}

impl BgTaskTestExt for BgTask {
    fn lock_state(
        &self,
    ) -> std::sync::LockResult<
        std::sync::MutexGuard<'_, crate::bash_background::registry::BgTaskState>,
    > {
        self.lock_state()
    }
    fn is_running(&self) -> bool {
        self.is_running()
    }
    fn task_id(&self) -> &String {
        &self.task_id
    }
    fn session_id(&self) -> &String {
        &self.session_id
    }
    fn state(&self) -> &std::sync::Mutex<super::BgTaskState> {
        &self.state
    }
    fn paths(&self) -> &TaskPaths {
        &self.paths
    }
    fn last_reminder_at(&self) -> &std::sync::Mutex<Option<Instant>> {
        &self.last_reminder_at
    }
}

#[cfg(unix)]
pub trait TaskSinkTestExt: Sized {
    fn new(registry: &BgTaskRegistry, task: Arc<BgTask>) -> std::io::Result<Self>;
}

#[cfg(unix)]
impl TaskSinkTestExt for crate::bash_background::registry::remote::TaskSink {
    fn new(registry: &BgTaskRegistry, task: Arc<BgTask>) -> std::io::Result<Self> {
        Self::new(registry, task)
    }
}

#[cfg(unix)]
pub trait EscalationGrantStoreTestExt {
    fn len_for_test(&self) -> usize;
}

#[cfg(unix)]
impl EscalationGrantStoreTestExt for crate::sandbox_spawn::EscalationGrantStore {
    fn len_for_test(&self) -> usize {
        Self::len_for_test(self)
    }
}

pub trait TaskRuntimeTestExt: Sized {
    fn empty_pty() -> Self;
}

impl TaskRuntimeTestExt for super::TaskRuntime {
    fn empty_pty() -> Self {
        Self::Pty(None)
    }
}
