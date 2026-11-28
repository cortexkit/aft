//! Process-lifetime scope tracking, separate from immutable spawn authority.

use super::*;

impl BgTaskRegistry {
    /// Find held task scopes, including published tasks whose process is still
    /// starting. Run away from daemon frame dispatch: acquiring registry locks
    /// and checking whether descendant processes remain alive can block.
    #[doc(hidden)]
    pub fn live_scope_stamps(&self) -> Vec<subc_protocol::scope::ScopeStamp> {
        // Snapshot in spawn's publication order without holding both locks.
        let published = self.task_arcs();
        let mut candidates = self.running_tasks();
        let mut seen: HashSet<_> = candidates.iter().map(|task| task.task_id.clone()).collect();
        for task in published {
            // A caller can find a new task before watchdog registration. Include
            // those nonterminal tasks, not retired terminal history that would
            // otherwise require extra process-group samples.
            if !seen.contains(&task.task_id) && !task.is_terminal() {
                seen.insert(task.task_id.clone());
                candidates.push(task);
            }
        }
        // Only unix extends the task scopes with secret proxy authorizations.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut scopes: Vec<_> = candidates
            .into_iter()
            .filter_map(|task| {
                let scope = task
                    .lock_state()
                    .ok()?
                    .metadata
                    .lifecycle_scope()
                    .cloned()?;
                (!task.is_terminal()
                    || self.has_live_task_processes(&task.task_id, &task.session_id))
                .then_some(scope)
            })
            .collect();
        #[cfg(unix)]
        scopes.extend(self.secret_authorization_scope_stamps());
        scopes
    }
}
