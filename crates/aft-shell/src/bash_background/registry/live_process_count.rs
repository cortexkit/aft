use super::*;

impl BgTaskRegistry {
    /// Count background task PIDs that are still alive without keeping registry
    /// locks across the OS liveness probes used by the lifecycle health rollup.
    #[doc(hidden)]
    pub fn detached_live_process_count(&self) -> usize {
        let Some(pids) = self.inner.tasks.try_lock().ok().map(|tasks| {
            tasks
                .values()
                .filter_map(|task| {
                    task.state
                        .try_lock()
                        .ok()
                        .and_then(|state| state.metadata.child_pid)
                })
                .collect::<Vec<_>>()
        }) else {
            return 0;
        };
        pids.into_iter()
            .filter(|pid| is_process_alive(*pid))
            .count()
    }
}
