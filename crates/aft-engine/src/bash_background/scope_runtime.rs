use super::*;

impl BgTaskRegistry {
    pub(crate) fn configure_task_scope_runtime(&self, daemon_mode: bool, max_runtime: Duration) {
        self.inner
            .task_scopes
            .configure_runtime(daemon_mode, max_runtime);
    }

    #[cfg(target_os = "linux")]
    pub(super) fn cleanup_recorded_task_scopes(
        &self,
        storage_dir: &Path,
        terminal_only: bool,
        pending_records: &mut Option<HashMap<String, super::super::persistence::TaskRecordRef>>,
    ) -> Result<usize, String> {
        let mut records = if let Some(records) = pending_records.take() {
            records
        } else {
            let mut cursor =
                match super::super::persistence::StorageTaskRecordCursor::new(storage_dir) {
                    Ok(cursor) => cursor,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                    Err(error) => return Err(error.to_string()),
                };
            let mut records = HashMap::new();
            let mut skipped_oversized = 0;
            loop {
                let batch = super::super::persistence::discover_storage_task_records(
                    storage_dir,
                    &mut cursor,
                )
                .map_err(|error| error.to_string())?;
                skipped_oversized += batch.skipped_oversized;
                for record in batch.records {
                    records.insert(record.task_id.clone(), record);
                }
                if batch.finished {
                    break;
                }
                std::thread::yield_now();
            }
            records.retain(|id, _| !cursor.rejected_task_ids.contains(id));
            if skipped_oversized > 0 {
                crate::slog_info!(
                    "bash task scope cleanup skipped {skipped_oversized} oversized records"
                );
            }
            records
        };
        if records.is_empty() {
            *pending_records = Some(records);
            return Ok(0);
        }
        let scope_records = records
            .values()
            .map(|record| super::super::task_scope::ScopeRecord {
                task_id: record.task_id.clone(),
                terminal: record.terminal,
            })
            .collect::<Vec<_>>();
        let mut pending_ids = HashSet::new();
        let result = self.inner.task_scopes.cleanup_owned(
            &scope_records,
            terminal_only,
            |id, description| {
                let Some(record) = records.get(id).filter(|record| {
                    super::super::persistence::task_store_root(&record.session_dir).is_some_and(
                        |store| {
                            super::super::task_scope::scope_description(id, store) == description
                        },
                    )
                }) else {
                    return Ok(false);
                };
                super::super::persistence::reread_storage_task_record(record)
                    .map(|metadata| {
                        let stop = !terminal_only || metadata.is_terminal();
                        if stop {
                            let reason = if terminal_only {
                                "startup_terminal_scope_cleanup"
                            } else {
                                "module_shutdown"
                            };
                            log_bash_kill(id, metadata.child_pid, metadata.pgid, reason);
                        }
                        stop
                    })
                    .map_err(|error| error.to_string())
            },
            &mut pending_ids,
        );
        records.retain(|id, _| pending_ids.contains(id));
        *pending_records = Some(records);
        result
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn cleanup_terminal_task_scopes(&self, storage_dir: &Path) -> Result<usize, String> {
        self.cleanup_recorded_task_scopes(storage_dir, true, &mut None)
    }

    /// Stops all task scopes with owned records in this storage root.
    /// Use this when module shutdown must stop tasks that can survive a restart.
    /// Do not call this from the frame loop or with the scheduler lock. It reads storage and waits for systemctl.
    pub fn end_all_task_scopes(&self, storage_dir: &Path) -> Result<usize, String> {
        #[cfg(target_os = "linux")]
        {
            self.cleanup_recorded_task_scopes(storage_dir, false, &mut None)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = storage_dir;
            Ok(0)
        }
    }

    pub(crate) fn start_task_scope_cleanup(&self, storage_dir: &Path) {
        #[cfg(target_os = "linux")]
        {
            if !self.inner.task_scopes.is_daemon() {
                return;
            }
            let key = canonicalized_path(storage_dir);
            let mut pending_records = {
                let mut slots = task_scope_cleanup_slots()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let slot = slots.entry(key.clone()).or_default();
                if slot.running
                    || slot.done
                    || slot.retry_at.is_some_and(|when| Instant::now() < when)
                {
                    return;
                }
                slot.running = true;
                #[cfg(test)]
                {
                    slot.runs += 1;
                }
                slot.pending_records.clone()
            };
            let registry = self.clone();
            let root = key.clone();
            let spawned = std::thread::Builder::new()
                .name("aft-task-scope-cleanup".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        registry.cleanup_recorded_task_scopes(&root, true, &mut pending_records)
                    }))
                    .unwrap_or_else(|_| Err("task scope cleanup worker panicked".into()));
                    {
                        let mut slots = task_scope_cleanup_slots()
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        let slot = slots.entry(root.clone()).or_default();
                        slot.pending_records = pending_records;
                        slot.running = false;
                        slot.done = result.is_ok();
                        slot.retry_at = result
                            .as_ref()
                            .err()
                            .map(|_| Instant::now() + CLEANUP_RETRY_BACKOFF);
                    }
                    if let Err(error) = result {
                        crate::slog_warn!("task scope cleanup failed: {error}");
                    }
                    registry.request_persisted_gc(&root);
                });
            if let Err(error) = spawned {
                let mut slots = task_scope_cleanup_slots()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let slot = slots.entry(key).or_default();
                slot.running = false;
                slot.retry_at = Some(Instant::now() + CLEANUP_RETRY_BACKOFF);
                crate::slog_warn!("task scope cleanup worker could not start: {error}");
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = storage_dir;
    }

    #[cfg(unix)]
    pub(super) fn cleanup_failed_scope_spawn(
        &self,
        task_id: &str,
        child: &mut Child,
        scope: Option<&super::super::task_scope::ScopeLaunch>,
    ) -> Result<(), String> {
        let result = self.kill_task_scope_with_reason(task_id, "spawn_scope_cleanup");
        if let Some(scope) = scope {
            // The scope's RuntimeMaxSec backstop and the next startup cleanup still end it.
            log_bash_kill(task_id, Some(child.id()), None, "spawn_scope_cleanup");
            let _ = scope.stop();
        }
        let _ = child.wait();
        result
    }

    #[cfg(unix)]
    pub(super) fn place_task_scope(
        &self,
        child: &mut Child,
        scope: &super::super::task_scope::ScopeLaunch,
    ) -> Result<bool, String> {
        // Keep the identity gate closed until placement. An open gate can cause a retry to run the payload two times.
        match scope.await_placement(child.id(), Duration::from_secs(1)) {
            Ok(handle) => {
                self.inner.task_scopes.register(handle);
                Ok(false)
            }
            Err(error) => {
                let exited = child.try_wait().map_err(|e| e.to_string())?.is_some();
                let gate_held = child.stdin.take().is_some();
                #[cfg(target_os = "linux")]
                log_bash_kill(
                    scope.task_id(),
                    Some(child.id()),
                    None,
                    "spawn_scope_placement_failed",
                );
                if !exited {
                    super::super::process::terminate_pgid(child.id() as i32, None);
                }
                let _ = scope.stop();
                let _ = child.wait();
                if exited && gate_held {
                    static LOGGED: std::sync::Once = std::sync::Once::new();
                    LOGGED.call_once(|| {
                        crate::slog_info!("task scope creation failed before payload release; spawning without a scope: {error}");
                    });
                    Ok(true)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn prepare_memory_limit(
        memory_limit_mib: Option<u64>,
        task_id: &str,
    ) -> Result<Option<super::super::memory_limit::Monitor>, String> {
        #[cfg(test)]
        if let Some(gate) = MEMORY_PREPARE_GATE_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
            gate();
        }
        let mib = memory_limit_mib.unwrap_or(0);
        if mib == 0 {
            return Ok(None);
        }
        super::super::memory_limit::Monitor::prepare(mib, task_id).map(Some)
    }

    pub(super) fn fail_memory_spawn(
        &self,
        metadata: &mut PersistedTask,
        paths: &TaskPaths,
        error: &str,
        canonical_project_root: PathBuf,
    ) {
        metadata.mark_terminal(BgTaskStatus::Failed, None, Some(error.into()));
        let _ = self.persist_task(paths, metadata);
        let _ = self.insert_rehydrated_task_with_root(
            metadata.clone(),
            paths.clone(),
            true,
            canonical_project_root,
        );
        self.record_live_delivery_session(&metadata.session_id);
        self.enqueue_completion_if_needed(metadata, Some(paths), true);
    }

    #[cfg(target_os = "linux")]
    pub(super) fn scoped_memory_limit(
        &self,
        mib: Option<u64>,
        task_id: &str,
    ) -> Result<Option<super::super::memory_limit::Monitor>, String> {
        if let Some(mib) = mib.filter(|mib| *mib > 0) {
            let scope = self
                .inner
                .task_scopes
                .get(task_id)
                .ok_or("task memory scope is absent")?;
            return super::super::memory_limit::Monitor::from_task_scope(mib, &scope).map(Some);
        }
        Ok(None)
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn kill_task_scope(&self, task_id: &str) -> Result<(), String> {
        self.kill_task_scope_with_reason(task_id, "kill_requested")
    }

    pub(crate) fn kill_task_scope_with_reason(
        &self,
        task_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            if self.inner.task_scopes.get(task_id).is_some() {
                let (pid, pgid) = self
                    .inner
                    .tasks
                    .try_lock()
                    .ok()
                    .and_then(|tasks| tasks.get(task_id).cloned())
                    .and_then(|task| {
                        let state = task.state.try_lock().ok()?;
                        Some((state.metadata.child_pid, state.metadata.pgid))
                    })
                    .unwrap_or_default();
                return self
                    .inner
                    .task_scopes
                    .kill_with_audit(task_id, None, pid, pgid, reason);
            }
            let (mut pid, mut pgid) = (None, None);
            let recorded = if let Some(task) = self.task(task_id) {
                let metadata = match resolve_task_layout(&task.paths.session_dir, task_id)
                    .and_then(|record| read_task_at(&record))
                {
                    Ok(metadata) => Some(metadata),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        // A lost record can still have an in-memory identity.
                        // Cleanup may already hold the state lock, so never
                        // block while recovering that identity.
                        task.state
                            .try_lock()
                            .ok()
                            .map(|state| state.metadata.clone())
                    }
                    Err(error) => {
                        log::warn!(
                            "task scope kill could not read metadata for {task_id}: {error}"
                        );
                        return Err(format!(
                            "task scope aft-task-{task_id}.scope could not be killed"
                        ));
                    }
                };
                if let Some(metadata) = metadata {
                    pid = metadata.child_pid;
                    pgid = metadata.pgid;
                    let policy = metadata.memory_policy.filter(|policy| {
                        policy.kind
                            == super::super::memory_limit::MemoryPolicy::TASK_SCOPE_MEMORY_KIND
                    });
                    // A saved unscoped launch needs only process-group
                    // termination, not an inferred scope kill.
                    if metadata.task_scope_runtime_ms.is_none() && policy.is_none() {
                        return Ok(());
                    }
                    policy.and_then(|policy| policy.cgroup)
                } else {
                    None
                }
            } else {
                None
            };
            self.inner
                .task_scopes
                .kill_with_audit(task_id, recorded.as_deref(), pid, pgid, reason)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (task_id, reason);
            Ok(())
        }
    }

    pub(crate) fn memory_limit_exceeded(&self, task: &Arc<BgTask>) -> Option<String> {
        let mut state = task.lock_state().ok()?;
        if (state.metadata.remote.is_some() && !state.metadata.local_fallback_started)
            || state.metadata.is_terminal()
        {
            return None;
        }
        let pgid = state
            .metadata
            .pgid
            .or(state.metadata.child_pid.map(|pid| pid as i32))?;
        let mut monitors = self.inner.memory_monitors.lock().ok()?;
        if !monitors.contains_key(&task.task_id) {
            let policy = state.metadata.memory_policy.clone()?;
            match super::super::memory_limit::Monitor::recover(policy, &task.task_id) {
                Ok(Some(monitor)) => {
                    #[cfg(target_os = "linux")]
                    if let Some(scope) = monitor.task_scope() {
                        self.inner.task_scopes.register(scope.clone());
                    }
                    monitors.insert(task.task_id.clone(), monitor);
                }
                Ok(None) => return None,
                Err(_) => {
                    let reason = "Command stopped: memory monitor recovery failed.".to_string();
                    state.pending_terminal_override = Some(BgTaskStatus::Failed);
                    state.metadata.status_reason = Some(reason.clone());
                    return Some(reason);
                }
            }
        }
        let monitor = monitors.get_mut(&task.task_id)?;
        if monitor.exceeded(pgid) {
            let reason = monitor.reason();
            // A terminal observer can run before the stop worker gets its turn.
            // Preserve the known cap outcome for that observer as well.
            state.pending_terminal_override = Some(BgTaskStatus::Failed);
            state.metadata.status_reason = Some(reason.clone());
            Some(reason)
        } else {
            None
        }
    }

    pub(crate) fn kill_for_memory(&self, task: &Arc<BgTask>, reason: String) {
        #[cfg(windows)]
        let mut pid = None;
        if let Ok(mut state) = task.lock_state() {
            #[cfg(windows)]
            {
                pid = state.metadata.child_pid;
            }
            if !state.metadata.is_terminal() {
                state.pending_terminal_override = Some(BgTaskStatus::Failed);
                state.metadata.status_reason = Some(reason.clone());
            }
        }
        if let Ok(monitors) = self.inner.memory_monitors.lock() {
            if let Some(monitor) = monitors.get(&task.task_id) {
                if !monitor.is_task_scope() {
                    #[cfg(windows)]
                    log_bash_kill(&task.task_id, pid, None, &reason);
                    monitor.terminate();
                }
            }
        }
        let _ = self.kill_with_status_reason(
            &task.task_id,
            &task.session_id,
            BgTaskStatus::Failed,
            Some(reason),
        );
    }

    pub(super) fn settle_memory_accounting(&self, task: &Arc<BgTask>) -> Result<(), String> {
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task.lock_state().map_err(|error| error.to_string())?;
        if !state.metadata.is_terminal() || state.kill_in_flight {
            return Ok(());
        }
        let mut monitors = self
            .inner
            .memory_monitors
            .lock()
            .map_err(|error| error.to_string())?;
        let Some(monitor) = monitors.get_mut(&task.task_id) else {
            return Ok(());
        };
        let pgid = state
            .metadata
            .pgid
            .or(state.metadata.child_pid.map(|pid| pid as i32))
            .unwrap_or(0);
        let mut updated = false;
        let mut kill_scope = false;
        if monitor.exceeded(pgid) {
            if monitor.is_task_scope() {
                kill_scope = true;
            } else {
                #[cfg(windows)]
                log_bash_kill(
                    &task.task_id,
                    state.metadata.child_pid,
                    None,
                    &monitor.reason(),
                );
                monitor.terminate();
            }
            state
                .metadata
                .mark_terminal(BgTaskStatus::Failed, Some(137), Some(monitor.reason()));
            state.terminal_output_cache = None;
            updated = true;
        }
        #[cfg(windows)]
        if let Some(note) = monitor
            .outcome_note()
            .filter(|_| state.metadata.exit_code != Some(0))
        {
            state.metadata.execution_note = Some(note.into());
            updated = true;
        }
        if updated {
            // Accounting refines an observed exit. Failed storage must not skip
            // the process-group survivor check in finish_terminal_transition
            // or prevent completion publication.
            task.terminal_persist_dirty.store(true, Ordering::SeqCst);
            if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                crate::slog_warn!(
                    "failed to persist terminal state for {}: {error}",
                    task.task_id
                );
            } else {
                task.terminal_persist_dirty.store(false, Ordering::SeqCst);
            }
        }
        drop(monitors);
        drop(state);
        if kill_scope {
            self.kill_task_scope_with_reason(&task.task_id, "terminal_memory_limit")?;
        }
        Ok(())
    }
}
