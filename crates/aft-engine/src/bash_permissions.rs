use crate::context::AppContext;

pub use aft_shell::bash_permissions::*;

pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
    aft_shell::bash_permissions::scan(command, ctx)
}

pub mod scan {
    use std::path::Path;

    use crate::{bash_permissions::PermissionAsk, context::AppContext};
    pub use aft_shell::bash_permissions::scan::*;

    pub fn scan(command: &str, ctx: &AppContext) -> Vec<PermissionAsk> {
        aft_shell::bash_permissions::scan::scan(command, ctx)
    }

    pub fn scan_with_cwd(command: &str, ctx: &AppContext, cwd: &Path) -> Vec<PermissionAsk> {
        aft_shell::bash_permissions::scan::scan_with_cwd(command, ctx, cwd)
    }

    pub fn scan_with_cwd_for_session(
        command: &str,
        ctx: &AppContext,
        cwd: &Path,
        session_id: &str,
    ) -> Vec<PermissionAsk> {
        aft_shell::bash_permissions::scan::scan_with_cwd_for_session(command, ctx, cwd, session_id)
    }
}
