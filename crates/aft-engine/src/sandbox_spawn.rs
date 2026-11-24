pub use aft_shell::sandbox_spawn::*;

#[cfg(test)]
pub(crate) use aft_shell::test_bridge::{pty_command_for_plan, with_spawn_plan_for_test};

#[cfg(all(test, unix))]
#[path = "sandbox_scope_grants_tests.rs"]
mod scope_grant_tests;

use crate::{config::SandboxMode, context::AppContext};
#[cfg(unix)]
use std::collections::HashMap;
use std::path::Path;

#[cfg(unix)]
pub fn save_provider_approval(ctx: &AppContext, id: &str, snapshot: ProviderApprovalSnapshot) {
    aft_shell::sandbox_spawn::save_provider_approval(ctx, id, snapshot);
}

#[cfg(unix)]
pub fn frozen_provider_approval(
    ctx: &AppContext,
    id: &str,
    principal: &AuthenticatedPrincipal,
    input: &HashMap<String, String>,
) -> Option<ProviderApprovalSnapshot> {
    aft_shell::sandbox_spawn::frozen_provider_approval(ctx, id, principal, input)
}

pub fn effective_sandbox_mode(ctx: &AppContext, principal: &AuthenticatedPrincipal) -> SandboxMode {
    aft_shell::sandbox_spawn::effective_sandbox_mode(ctx, principal)
}

pub fn sandbox_required(ctx: &AppContext) -> bool {
    aft_shell::sandbox_spawn::sandbox_required(ctx)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn mint_host_escalation_grant(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    command: &[u8],
    root: &Path,
    cwd: &Path,
    shell_path: &Path,
    environment: &ChildEnvironment,
    storage_dir: &Path,
    session_id: &str,
) -> Result<String, String> {
    aft_shell::sandbox_spawn::mint_host_escalation_grant(
        ctx,
        principal,
        command,
        root,
        cwd,
        shell_path,
        environment,
        storage_dir,
        session_id,
    )
}

pub fn native_sandbox_enforced(ctx: &AppContext, principal: &AuthenticatedPrincipal) -> bool {
    aft_shell::sandbox_spawn::native_sandbox_enforced(ctx, principal)
}

pub fn unsupported_platform_sandbox_refusal(ctx: &AppContext) -> Option<SpawnPlan> {
    aft_shell::sandbox_spawn::unsupported_platform_sandbox_refusal(ctx)
}

pub fn resolve_sandbox_spawn(
    ctx: &AppContext,
    principal: &AuthenticatedPrincipal,
    requested_tier: RequestedSandboxTier,
    task_kind: SandboxTaskKind,
    task_bundle_dir: &Path,
    host_escalation: Option<&HostEscalationAttempt>,
) -> SpawnPlan {
    aft_shell::sandbox_spawn::resolve_sandbox_spawn(
        ctx,
        principal,
        requested_tier,
        task_kind,
        task_bundle_dir,
        host_escalation,
    )
}

pub mod remote {
    use crate::context::AppContext;
    use std::path::{Path, PathBuf};

    pub use aft_shell::sandbox_spawn::remote::*;

    #[doc(hidden)]
    pub fn prepare(
        ctx: &AppContext,
        folder: &Path,
        workdir: PathBuf,
        command: String,
    ) -> Result<Start, String> {
        aft_shell::sandbox_spawn::remote::prepare(ctx, folder, workdir, command)
    }

    #[cfg(all(target_os = "linux", feature = "test-hooks"))]
    pub async fn run_for_test(
        ctx: &AppContext,
        folder: &Path,
        launcher: &Path,
        command: String,
        timeout: std::time::Duration,
    ) -> Result<crate::tool_call_result::ToolCallResult, String> {
        aft_shell::sandbox_spawn::remote::run_for_test(ctx, folder, launcher, command, timeout)
            .await
    }
}

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
pub use remote::run_for_test as run_remote_shell_for_test;
