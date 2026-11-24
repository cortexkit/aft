use std::sync::Arc;

use crate::{config::BashChangedFiles, context::AppContext, sandbox_spawn::SpawnPlan};
pub use aft_shell::bash_changed_files::*;

#[cfg(test)]
pub(crate) fn start(ctx: &AppContext) -> Option<Arc<Capture>> {
    start_for_spawn(ctx, "bash_changed_files", &SpawnPlan::Unsandboxed)
}

pub(crate) fn start_for_spawn(
    ctx: &AppContext,
    request_id: &str,
    plan: &SpawnPlan,
) -> Option<Arc<Capture>> {
    let mode = ctx.config().bash.changed_files;
    if mode == BashChangedFiles::Off {
        return None;
    }
    let root = ctx.config().project_root.clone()?;
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let journal = ctx
        .watcher_counters()
        .bash_changes
        .get_or_init(|| Arc::new(Journal::default()));
    let mut capture = Capture::begin(root, journal, mode, ctx.watcher_runtime_active());
    let access = (mode == BashChangedFiles::Diff)
        .then(|| ctx.access_policy(request_id).ok())
        .flatten();
    Arc::get_mut(&mut capture).unwrap().setup_for_spawn(
        access,
        plan.changed_files_read_profile(),
        ctx.watcher_counters().bash_flush.clone(),
        crate::bash_background::changed_files_deadline_for_spawn(),
    );
    Some(capture)
}
