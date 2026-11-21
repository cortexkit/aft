//! Fixtures for daemon tests that exercise shell execution and remote jobs.

use crate::bash_background::{self, frames, persistence, registry};
use crate::sandbox_spawn;
use std::path::{Path, PathBuf};

pub mod completion_queue {
    pub const MAX_QUEUED_PER_SESSION: usize =
        crate::bash_background::registry::completion_queue::MAX_QUEUED_PER_SESSION;
}

#[cfg(unix)]
pub use crate::pinned_dir::with_openat_file_hook;
pub use crate::pinned_dir::work_counts;

pub fn canonical_root_path(path: &Path) -> std::io::Result<PathBuf> {
    persistence::canonical_root_path(path)
}

pub fn is_system_temp_path(path: &Path) -> bool {
    crate::bash_permissions::is_system_temp_path(path)
}

#[cfg(unix)]
pub fn resolve_posix_shell() -> PathBuf {
    registry::resolve_posix_shell()
}

pub fn successful_spawn_response(
    request_id: &str,
    session_id: &str,
    task_id: &str,
    pty: bool,
    ctx: &crate::context::AppContext,
) -> crate::protocol::Response {
    bash_background::successful_spawn_response(request_id, session_id, task_id, pty, ctx)
}

pub fn bash_bodies(
    metadata: &persistence::PersistedTask,
    output_bytes: Option<u64>,
    streams: Option<(String, String)>,
) -> frames::BashBodies {
    frames::bash_bodies(metadata, output_bytes, streams)
}

pub fn recovered_bash_frame(
    snapshot: registry::BgTaskSnapshot,
    key: &crate::db::call_ledger::Key,
    root: &Path,
    scope: Option<crate::output_store::OutputScope>,
) -> Option<frames::RecoveredFrame> {
    frames::recovered_bash_frame(snapshot, key, root, scope)
}

pub fn record_service(root: &Path) -> std::sync::Arc<crate::gh_shim_speech::service::Service> {
    crate::gh_shim_speech::service::record_service(root)
}

pub fn worker_scope(stamp: &subc_protocol::scope::ScopeStamp) -> bool {
    crate::gh_shim_relay::worker_scope(stamp)
}

#[cfg(unix)]
pub fn create_task_temp_dir(task_bundle_dir: &Path) -> Result<PathBuf, String> {
    sandbox_spawn::create_task_temp_dir(task_bundle_dir)
}

#[cfg(unix)]
pub fn cleanup_task_temp_dir(path: &Path) -> std::io::Result<()> {
    sandbox_spawn::cleanup_task_temp_dir(path)
}

pub mod shell_metadata {
    use crate::bash_background::shell_metadata as metadata;

    pub fn current() -> Option<metadata::ShellCall> {
        metadata::current()
    }

    pub fn changed() {
        metadata::changed();
    }

    pub fn facts_changed() {
        metadata::facts_changed();
    }
}

pub mod bash_hints {
    use crate::bash_rewrite::bash_hints as hints;

    pub fn fresh_at(modified: std::time::SystemTime, now: std::time::SystemTime) -> bool {
        hints::fresh_at(modified, now)
    }

    pub fn operands(stage: &str, at: usize, windows: bool) -> Vec<String> {
        hints::operands(stage, at, windows)
    }

    pub fn statements(command: &str, windows: bool) -> Option<Vec<&str>> {
        hints::statements(command, windows)
    }

    pub fn pipeline_stages(command: &str, windows: bool) -> Option<Vec<&str>> {
        hints::pipeline_stages(command, windows)
    }

    pub fn token(command: &str, start: usize, windows: bool) -> Option<(String, usize)> {
        hints::token(command, start, windows)
    }
}

pub const QUEUE_CAPACITY: usize = bash_background::runner_notice_scheduler::QUEUE_CAPACITY;
pub const PREFER_TOOL: &str = crate::bash_rewrite::notices::PREFER_TOOL;
pub const PROXY_UNAVAILABLE: &str = bash_background::secrets::PROXY_UNAVAILABLE;
pub const COMMAND_BYTES: usize = bash_background::scoped_tasks::COMMAND_BYTES;
pub const OWNER_PAUSED_REASON: &str = registry::OWNER_PAUSED_REASON;

#[cfg(unix)]
pub const CHILD_FAILURE_FD: std::os::fd::RawFd = sandbox_spawn::CHILD_FAILURE_FD;

#[cfg(unix)]
pub fn remote_report(metadata: &persistence::PersistedTask) -> Option<String> {
    registry::remote::remote_report(metadata)
}

pub type SharedCallDeadline = crate::bash_changed_files::SharedCallDeadline;

pub fn changed_files_deadline_for_spawn() -> Option<SharedCallDeadline> {
    bash_background::changed_files_deadline_for_spawn()
}

pub fn call_key_for_new_task(task_id: &str) -> bash_background::TaskCallKey {
    bash_background::call_key_for_new_task(task_id)
}

pub fn enter_bash_startup_step(step: bash_background::BashStartupStep) {
    bash_background::enter_bash_startup_step(step);
}

pub fn commit_spawn_receipt_inline() -> Result<(), String> {
    bash_background::commit_spawn_receipt_inline()
}

pub fn with_spawn_commit_time_for_test<T>(now: std::time::Instant, run: impl FnOnce() -> T) -> T {
    bash_background::with_spawn_commit_time_for_test(now, run)
}

pub fn with_spawn_plan_for_test<T>(plan: sandbox_spawn::SpawnPlan, run: impl FnOnce() -> T) -> T {
    sandbox_spawn::with_spawn_plan_for_test(plan, run)
}

mod sandbox;
#[cfg(target_os = "linux")]
pub mod task_scope;
pub use sandbox::*;

pub use crate::bash_background::buffer::buffer_types::BgBuffer;
pub use crate::bash_background::persistence::test_faults::{with_task_io_fault, TaskIoFault};
pub use crate::bash_background::registry::task_types::{BgTask, BgTaskState, TaskRuntime};
pub mod registry_support {
    pub use crate::bash_background::registry::test_support::*;
}
pub use crate::bash_background::registry::return_files::return_types::{
    ReturnOutcome, ReturnedPath,
};
pub use crate::bash_parse::LITERAL_APPEND_CASES;
pub use crate::windows_shell::testable_types::{CandidateFailure, PipedAttemptError, WindowsShell};

pub fn spawn_piped_candidates<C>(
    candidates: &[WindowsShell],
    spawn: impl FnMut(&WindowsShell, bool) -> Result<C, PipedAttemptError>,
) -> Result<(C, usize, bool), CandidateFailure<PipedAttemptError>> {
    crate::windows_shell::spawn_piped_candidates(candidates, spawn)
}

pub fn windows_shell_args<'a>(shell: &'a WindowsShell, command: &'a str) -> Vec<&'a str> {
    shell.args(command)
}

pub fn windows_shell_binary(shell: &WindowsShell) -> std::borrow::Cow<'_, str> {
    shell.binary()
}

pub fn running(metadata: &persistence::PersistedTask) -> bash_background::scoped_tasks::Running {
    bash_background::scoped_tasks::running(metadata)
}

pub fn should_suppress_grep_footer(path: Option<&str>, project_root: &Path) -> bool {
    crate::bash_rewrite::rules::should_suppress_grep_footer(path, project_root)
}

pub fn grep_basic_regex_has_dialect_conflict(pattern: &str) -> bool {
    crate::bash_rewrite::rules::grep_basic_regex_has_dialect_conflict(pattern)
}

pub fn find_request(command: &str) -> Option<serde_json::Value> {
    crate::bash_rewrite::rules::find_request(command)
}

pub fn grep_request(command: &str, binary: &str) -> Option<serde_json::Value> {
    crate::bash_rewrite::rules::grep_request(command, binary)
}

pub fn with_artifact_opens<T>(run: impl FnOnce(&std::cell::Cell<u64>) -> T) -> T {
    bash_background::buffer::ARTIFACT_OPENS.with(run)
}

impl BgTask {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn paths(&self) -> &persistence::TaskPaths {
        &self.paths
    }

    pub fn test_lock_state(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, BgTaskState>> {
        self.lock_state()
    }
}

impl BgTaskState {
    pub fn metadata(&self) -> &persistence::PersistedTask {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut persistence::PersistedTask {
        &mut self.metadata
    }

    pub fn buffer(&self) -> &BgBuffer {
        &self.buffer
    }

    pub fn buffer_mut(&mut self) -> &mut BgBuffer {
        &mut self.buffer
    }

    pub fn runtime_mut(&mut self) -> &mut TaskRuntime {
        &mut self.runtime
    }
}

pub fn poll_task(
    registry: &registry::BgTaskRegistry,
    task: &std::sync::Arc<BgTask>,
) -> Result<(), String> {
    registry.poll_task(task)
}

pub mod remote_wire {
    pub use crate::exec_remote::wire_fixture::*;
}

#[cfg(unix)]
pub use registry::runon_capture::set_test_cap;

pub fn child_polls(registry: &registry::BgTaskRegistry) -> u64 {
    registry
        .inner
        .child_polls
        .load(std::sync::atomic::Ordering::Relaxed)
}

pub fn snapshots_built(registry: &registry::BgTaskRegistry) -> u64 {
    registry
        .inner
        .snapshots_built
        .load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(unix)]
pub mod remote_support {
    pub use crate::bash_background::registry::remote::{fixture::*, seams::*};
}

#[cfg(target_os = "linux")]
pub mod git_policy_bench_support {
    pub use crate::sandbox_spawn::git_policy_bench_support::*;
}

#[cfg(unix)]
pub mod git_policy_cache_support {
    pub use crate::sandbox_spawn::git_policy_cache_fixture::{commit, git_ok, Fixture};
}

pub mod secrets_support {
    pub use crate::bash_background::secrets::test_support::*;
}

mod speech;
pub use speech::*;
