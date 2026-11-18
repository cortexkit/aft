//! Process-lifetime scope tracking, separate from immutable spawn authority.

use super::*;

impl BgTaskRegistry {
    /// Persist a confirmed rename only in process-lifetime tracking. The spawn
    /// scope and terminal execution facts remain unchanged for audit and speech.
    pub(crate) fn rehome_scope(
        &self,
        from: &subc_protocol::scope::ScopeStamp,
        to: &subc_protocol::scope::ScopeStamp,
    ) -> std::io::Result<usize> {
        if crate::access_policy::prefrontal::counterpart_owner(&from.owner).as_ref()
            != Some(&to.owner)
            || from.scope_ref != to.scope_ref
        {
            return Err(std::io::Error::other("not a prefrontal scope continuation"));
        }
        let mut changed = 0;
        for task in self.task_arcs() {
            if task.is_terminal() && !self.has_live_task_processes(&task.task_id, &task.session_id)
            {
                continue;
            }
            let mut db = DeferredDbWrites::new(self, &task);
            let mut state = task
                .lock_state()
                .map_err(|_| std::io::Error::other("background task lock poisoned"))?;
            let matches = |metadata: &PersistedTask| {
                metadata.lifecycle_scope().is_some_and(|scope| {
                    scope.owner == from.owner
                        && scope.scope_ref == from.scope_ref
                        && scope.scope_epoch == from.scope_epoch
                })
            };
            if !matches(&state.metadata) {
                continue;
            }
            state.metadata = self.update_task_metadata_locked(&task, &mut db, |metadata| {
                if matches(metadata) {
                    metadata.lifecycle_scope = Some(to.clone());
                }
            })?;
            changed += 1;
        }
        if changed > 0 {
            aft_base::scope_watch_events::changed(aft_base::scope_watch_events::Source::Shell);
        }
        Ok(changed)
    }

    /// Find held task scopes, including published tasks whose process is still
    /// starting. Run away from daemon frame dispatch: acquiring registry locks
    /// and checking whether descendant processes remain alive can block.
    pub(crate) fn live_scope_stamps(&self) -> Vec<subc_protocol::scope::ScopeStamp> {
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
