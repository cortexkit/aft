impl Capture {
    #[cfg(test)]
    pub(crate) fn start(ctx: &crate::context::AppContext) -> Option<Arc<Self>> {
        Self::start_for_spawn(
            ctx,
            "bash_changed_files",
            &crate::sandbox_spawn::SpawnPlan::Unsandboxed,
        )
    }
    pub(crate) fn start_for_spawn(
        ctx: &crate::context::AppContext,
        request_id: &str,
        plan: &crate::sandbox_spawn::SpawnPlan,
    ) -> Option<Arc<Self>> {
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
        let mut capture = Self::begin(root, journal, mode, ctx.watcher_runtime_active());
        if mode == BashChangedFiles::Diff {
            Arc::get_mut(&mut capture).unwrap().read_policy = ctx
                .access_policy(request_id)
                .ok()
                .map(|access| ReadPolicy::new(access, plan.changed_files_read_profile()));
        }
        Arc::get_mut(&mut capture).unwrap().flush = Some(ctx.watcher_counters().bash_flush.clone());
        if let Some(deadline) = crate::bash_background::changed_files_deadline_for_spawn() {
            Arc::get_mut(&mut capture).unwrap().call_deadline = deadline;
        }
        Some(capture)
    }
}
