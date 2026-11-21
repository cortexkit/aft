use crate::bash_background::persistence::ResolvedTask;
use crate::context::AppContext;
use crate::sandbox_spawn::{
    self, AuthenticatedPrincipal, HostEscalationAttempt, RequestedSandboxTier, SandboxRequirement,
    SandboxTaskKind, SpawnPlan,
};
use std::path::Path;
#[cfg(unix)]
use std::{collections::HashMap, path::PathBuf};

pub type ChildEnvironment = sandbox_spawn::ChildEnvironment;

pub fn resolve_command_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
    requirement: Option<SandboxRequirement>,
) -> SpawnPlan {
    sandbox_spawn::resolve_command_sandbox_spawn(
        ctx,
        principal,
        requested_tier,
        task_kind,
        task_bundle_dir,
        host_escalation,
        requirement,
    )
}

#[cfg(unix)]
pub fn prepare_task_payload(
    task: &ResolvedTask,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell_path: &Path,
    environment: &ChildEnvironment,
) -> Result<sandbox_spawn::PreparedTask, String> {
    sandbox_spawn::prepare_task_payload(
        task,
        command,
        root,
        cwd,
        principal,
        shell_path,
        environment,
    )
}

#[cfg(unix)]
pub fn save_local_launch(
    plan: &SpawnPlan,
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell: &Path,
    linux_scope: bool,
    memory_limit_mib: Option<u64>,
    linux_scope_max_runtime_secs: u64,
) -> Result<String, String> {
    sandbox_spawn::save_local_launch(
        plan,
        root,
        cwd,
        principal,
        shell,
        linux_scope,
        memory_limit_mib,
        linux_scope_max_runtime_secs,
    )
}

#[cfg(unix)]
pub fn restore_local_launch(
    task: &ResolvedTask,
    expected: &str,
) -> Result<
    (
        SpawnPlan,
        PathBuf,
        HashMap<String, String>,
        bool,
        Option<u64>,
        u64,
    ),
    String,
> {
    sandbox_spawn::restore_local_launch(task, expected)
}
#[cfg(target_os = "macos")]
pub fn probe_command_for_plan(
    plan: &crate::sandbox_spawn::SpawnPlan,
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<std::process::Command, String> {
    crate::sandbox_spawn::probe_command_for_plan(plan, program, args, cwd, env)
}
