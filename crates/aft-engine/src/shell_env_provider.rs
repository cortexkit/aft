pub use aft_shell::shell_env_provider::*;

#[cfg(unix)]
#[doc(hidden)]
pub fn host_preflight(
    ctx: &crate::context::AppContext,
    cwd: &std::path::Path,
    environment: &mut std::collections::HashMap<String, String>,
) -> (Option<String>, Option<std::sync::Arc<PreflightLayout>>) {
    aft_shell::shell_env_provider::host_preflight(ctx, cwd, environment)
}
