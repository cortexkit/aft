pub use aft_shell::bash_background::*;

pub fn task_storage_dir(ctx: &crate::context::AppContext) -> std::path::PathBuf {
    aft_shell::bash_background::task_storage_dir(ctx)
}

use crate::{
    bash_permissions::PermissionAsk, protocol::Response, sandbox_spawn::HostEscalationAttempt,
};
use std::{collections::HashMap, path::PathBuf};

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn spawn(
    request_id: &str,
    session_id: &str,
    command: &str,
    shell: BashShell,
    shell_path: PathBuf,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    hard_kill: HardKill,
    ctx: &crate::context::AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
    pty: bool,
    pty_rows: u16,
    pty_cols: u16,
    scanner_report: Vec<PermissionAsk>,
    scanner_requires_approval: bool,
    host_escalation: Option<HostEscalationAttempt>,
    remote: Option<RemoteLaunch>,
) -> Response {
    aft_shell::test_bridge::bash_background::spawn(
        request_id,
        session_id,
        command,
        shell,
        shell_path,
        workdir,
        env,
        hard_kill,
        ctx,
        require_background_flag,
        notify_on_completion,
        compressed,
        pty,
        pty_rows,
        pty_cols,
        scanner_report,
        scanner_requires_approval,
        host_escalation,
        remote,
    )
}

#[allow(clippy::too_many_arguments)]
#[doc(hidden)]
pub fn spawn_with_secrets(
    request_id: &str,
    session_id: &str,
    command: &str,
    shell: BashShell,
    shell_path: PathBuf,
    workdir: Option<PathBuf>,
    env: Option<HashMap<String, String>>,
    hard_kill: HardKill,
    ctx: &crate::context::AppContext,
    require_background_flag: bool,
    notify_on_completion: bool,
    compressed: bool,
    pty: bool,
    pty_rows: u16,
    pty_cols: u16,
    scanner_report: Vec<PermissionAsk>,
    scanner_requires_approval: bool,
    host_escalation: Option<HostEscalationAttempt>,
    remote: Option<RemoteLaunch>,
    #[cfg(unix)] secret_launch: Option<secrets::Launch>,
) -> Response {
    aft_shell::bash_background::spawn_with_secrets(
        request_id,
        session_id,
        command,
        shell,
        shell_path,
        workdir,
        env,
        hard_kill,
        ctx,
        require_background_flag,
        notify_on_completion,
        compressed,
        pty,
        pty_rows,
        pty_cols,
        scanner_report,
        scanner_requires_approval,
        host_escalation,
        remote,
        #[cfg(unix)]
        secret_launch,
    )
}

#[doc(hidden)]
pub mod secrets {
    pub use aft_shell::bash_background::secrets::*;

    #[cfg(unix)]
    #[doc(hidden)]
    pub fn preflight(
        ctx: &crate::context::AppContext,
        session: &str,
        pty: bool,
        background: bool,
        requirement: crate::sandbox_spawn::SandboxRequirement,
    ) -> Result<Preflight, String> {
        aft_shell::bash_background::secrets::preflight(ctx, session, pty, background, requirement)
    }
}
