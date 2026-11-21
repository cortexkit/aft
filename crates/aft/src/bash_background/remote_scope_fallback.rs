use super::*;

impl BgTaskRegistry {
    pub(super) fn remote_fallback(
        &self,
        task: &Arc<BgTask>,
        local: LocalFallback,
        reason: &str,
        refusal_detail: Option<&str>,
    ) -> Result<(), String> {
        let mut db = DeferredDbWrites::new(self, task);
        let mut state = task.state.lock().map_err(|_| "task lock poisoned")?;
        let metadata = state.metadata.clone();
        // Automatic prefix routing may fall back; an explicit remote demand
        // must fail before any local launch state or process is created. A
        // refusal of return_paths is final too: running the command here
        // would not copy back the files it was asked for.
        if metadata.remote.as_ref().is_some_and(|r| r.explicit_runon)
            || return_paths_unsupported(reason, refusal_detail)
        {
            drop(state);
            drop(db);
            return self.refuse_remote_executor(task, reason, refusal_detail);
        }
        let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
            .map_err(|e| e.to_string())?;
        let durable = read_task_at(&layout).map_err(|e| e.to_string())?;
        // Checked under the state lock that every cancellation intent is
        // recorded under, so an intent recorded first always prevents the
        // local start.
        let cancel_requested = state
            .metadata
            .remote
            .as_ref()
            .is_some_and(|r| r.cancel_requested)
            || durable.remote.as_ref().is_some_and(|r| r.cancel_requested);
        if cancel_requested {
            if let Some(remote) = state.metadata.remote.as_mut() {
                remote.cancel_requested = true;
            }
            drop(state);
            drop(db);
            // When the owning session pauses, a refused remote job needs no
            // further cancellation if its local fallback has not started.
            if self.finish_owner_pause_cancel(task, Ok(())) {
                return Ok(());
            }
            self.remote_terminal(
                task,
                Verdict::RunLocally {
                    reason: RefusalReason::Unknown(reason.into()),
                    refusal_detail: refusal_detail.map(str::to_owned),
                },
                Some("remote refused before start; local fallback skipped because cancellation was requested".into()),
            );
            return Ok(());
        }
        // Commit the switch to local before spawning. A crash before the PID
        // write must report an uncertain local start, never submit it again.
        state.metadata.remote = None;
        state.metadata.local_fallback_started = true;
        state.metadata.started_at = unix_millis();
        let reason = rendered_refusal_reason(reason, refusal_detail).replace(['\n', '\r'], " ");
        state.metadata.execution_note = Some(format!(
            "ran locally on {}: remote refused: {reason}",
            local_os_name()
        ));
        if metadata
            .remote
            .as_ref()
            .is_some_and(|remote| !remote.return_paths.is_empty())
        {
            let note = state.metadata.execution_note.as_mut().unwrap();
            note.push('\n');
            note.push_str(super::super::return_files::LOCAL_RUN_NOTE);
        }
        if let Some(changes) = metadata
            .remote
            .as_ref()
            .and_then(|r| r.terminal.as_ref())
            .and_then(|t| t.workspace_changes.as_ref())
            .filter(|c| !c.is_empty())
        {
            state
                .metadata
                .execution_note
                .as_mut()
                .unwrap()
                .push_str(&format!(
                    "\nremote workspace_changes (not copied back): {}",
                    serde_json::to_string(changes).unwrap()
                ));
        }
        append_output_loss(&mut state.metadata);
        self.persist_task_locked(task, &state.metadata, &mut db)
            .map_err(|e| e.to_string())?;
        #[cfg(test)]
        seams::after_local_fallback_marker(&task.task_id);
        let hard_kill = if metadata.default_hard_kill {
            super::super::super::HardKill::Default
        } else {
            super::super::super::HardKill::After(Duration::from_millis(
                metadata.timeout_ms.unwrap_or(0),
            ))
        };
        let mut scope = self
            .inner
            .task_scopes
            .policy(
                local.linux_scope,
                Some(Duration::from_secs(local.linux_scope_max_runtime_secs)),
                local.plan.is_native_launcher(),
            )
            .map(|policy| {
                self.inner.task_scopes.prepare(
                    policy,
                    &task.task_id,
                    hard_kill,
                    local.memory_limit_mib,
                    crate::bash_background::persistence::task_store_root(&task.paths.session_dir)
                        .ok_or("task store root is unavailable")?,
                )
            })
            .transpose()?
            .flatten();
        #[cfg(target_os = "linux")]
        {
            state.metadata.task_scope_runtime_ms = scope.as_ref().map(|scope| scope.runtime_ms());
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|error| error.to_string())?;
        }
        let mut memory_monitor = if scope.is_some() {
            None
        } else {
            Self::prepare_memory_limit(local.memory_limit_mib, &task.task_id)?
        };
        let gated = scope.is_some() || memory_monitor.is_some();
        let spawn_child = |scope: Option<&super::super::super::task_scope::ScopeLaunch>,
                           io_handles: &mut TaskIoHandles| {
            spawn_detached_child(
                &local.plan,
                &metadata.command,
                super::super::super::BashShell::Bash,
                &local.shell_path,
                &task.paths,
                &metadata.workdir,
                &local.env,
                io_handles,
                local.capture_pipeline,
                local.memory_limit_mib.is_some(),
                scope,
                if gated {
                    LocalIdentityGate::Required
                } else {
                    LocalIdentityGate::CommittedFallback
                },
            )
        };
        let child_result = spawn_child(
            scope.as_ref(),
            state.io_handles.as_mut().ok_or("task handles absent")?,
        )
        .and_then(|mut child| {
            if let Some(launch) = scope.as_ref() {
                if self.place_task_scope(&mut child, launch)? {
                    scope = None;
                    state.metadata.task_scope_runtime_ms = None;
                    self.persist_task_locked(task, &state.metadata, &mut db)
                        .map_err(|error| error.to_string())?;
                    memory_monitor =
                        Self::prepare_memory_limit(local.memory_limit_mib, &task.task_id)?;
                    return spawn_child(
                        None,
                        state.io_handles.as_mut().ok_or("task handles absent")?,
                    );
                }
            }
            Ok(child)
        });
        let mut child = match child_result {
            Ok(child) => child,
            Err(error) => {
                // If spawning or scope placement fails before the command can
                // run, do not report the proposed runtime as a task deadline.
                state.metadata.task_scope_runtime_ms = None;
                self.persist_task_locked(task, &state.metadata, &mut db)
                    .map_err(|error| error.to_string())?;
                return Err(error);
            }
        };
        if scope.is_none() {
            state.metadata.task_scope_runtime_ms = None;
        }
        #[cfg(target_os = "linux")]
        if scope.is_some() {
            match self.scoped_memory_limit(local.memory_limit_mib, &task.task_id) {
                Ok(monitor) => memory_monitor = monitor,
                Err(error) => {
                    drop(child.stdin.take());
                    if let Some(scope) = scope.as_ref() {
                        log_bash_kill(
                            &task.task_id,
                            Some(child.id()),
                            None,
                            "fallback_memory_setup_failed",
                        );
                        let _ = scope.stop();
                    }
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }
        if let Some(mut monitor) = memory_monitor.take() {
            if let Err(error) = monitor.attach(child.id()) {
                drop(state);
                log_bash_kill(
                    &task.task_id,
                    Some(child.id()),
                    Some(child.id() as i32),
                    "fallback_memory_attach_failed",
                );
                super::super::super::process::terminate_pgid_with(child.id() as i32, || {
                    matches!(child.try_wait(), Ok(Some(_)))
                });
                if let Err(kill_error) =
                    self.cleanup_failed_scope_spawn(&task.task_id, &mut child, scope.as_ref())
                {
                    crate::slog_warn!("failed spawn scope cleanup: {kill_error}");
                }
                return Err(error);
            }
            state.metadata.memory_policy = Some(monitor.policy.clone());
            // Persist the chosen enforcer before the gated fallback command can run.
            if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                drop(child.stdin.take());
                if let Some(scope) = scope.as_ref() {
                    log_bash_kill(
                        &task.task_id,
                        Some(child.id()),
                        None,
                        "fallback_memory_policy_persist_failed",
                    );
                    let _ = scope.stop();
                }
                let _ = child.wait();
                return Err(error.to_string());
            }
            self.inner
                .memory_monitors
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(task.task_id.clone(), monitor);
        }
        if gated {
            if local.memory_limit_mib.is_none() {
                state.metadata.status = BgTaskStatus::Starting;
                state.metadata.child_pid = Some(child.id());
                state.metadata.pgid = Some(child.id() as i32);
                state.metadata.child_start_time_ms =
                    crate::root_cache::process_start_time_ms(child.id());
                if let Err(error) = self.persist_task_locked(task, &state.metadata, &mut db) {
                    drop(child.stdin.take());
                    if let Some(scope) = scope.as_ref() {
                        let _ = scope.stop();
                    }
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            }
            use std::io::Write;
            child
                .stdin
                .take()
                .ok_or("local identity launch gate missing")?
                .write_all(b"run\n")
                .map_err(|error| error.to_string())?;
        }
        #[cfg(test)]
        if seams::simulate_crash_before_local_pid(&task.task_id) {
            return Ok(());
        }
        state.metadata.mark_running(child.id(), child.id() as i32);
        state.runtime = TaskRuntime::Piped(Some(child));
        state.detached = false;
        let persisted = (|| {
            #[cfg(test)]
            seams::fail_running_metadata_after_spawn(&task.task_id)?;
            self.persist_task_locked(task, &state.metadata, &mut db)
                .map_err(|e| e.to_string())
        })();
        drop(state);
        drop(db);
        if let Err(error) = persisted {
            // Spawn succeeded. This is not a no-start refusal, even when the
            // PID publication fails; both live and restarted callers end here.
            self.remote_terminal(
                task,
                Verdict::OutcomeUnknown,
                Some(format!(
                    "the command started locally; AFT lost track of it: {error}; never rerun"
                )),
            );
        }
        Ok(())
    }

    pub(super) fn restored_remote_fallback(
        &self,
        task: &Arc<BgTask>,
        remote: &RemoteTask,
        reason: &str,
        refusal_detail: Option<&str>,
    ) -> Result<(), String> {
        // Decide before even restoring a local launch plan: this also covers
        // a refusal persisted just before the previous AFT process stopped.
        if remote.explicit_runon || return_paths_unsupported(reason, refusal_detail) {
            return self.refuse_remote_executor(task, reason, refusal_detail);
        }
        let restore = (|| {
            let layout = resolve_task_layout(&task.paths.session_dir, &task.task_id)
                .map_err(|e| e.to_string())?;
            let (
                plan,
                shell_path,
                env,
                linux_scope,
                memory_limit_mib,
                linux_scope_max_runtime_secs,
            ) = crate::sandbox_spawn::restore_local_launch(
                &layout,
                remote
                    .fallback_digest
                    .as_deref()
                    .ok_or("local snapshot digest absent")?,
            )?;
            let metadata = task
                .state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .metadata
                .clone();
            let capture_pipeline = should_capture_pipeline_status(
                &plan,
                !metadata.pipeline_segments.is_empty(),
                &shell_path,
            );
            // Replay has validated handles, but no local runtime retained them.
            let handles = TaskIoHandles::reopen(&layout).map_err(|e| e.to_string())?;
            task.state
                .lock()
                .map_err(|_| "task lock poisoned")?
                .io_handles = Some(handles);
            self.remote_fallback(
                task,
                LocalFallback {
                    plan,
                    shell_path,
                    env,
                    capture_pipeline,
                    linux_scope,
                    linux_scope_max_runtime_secs,
                    memory_limit_mib,
                },
                reason,
                refusal_detail,
            )
        })();
        if let Err(error) = restore {
            self.remote_terminal(
                task,
                Verdict::RunLocally {
                    reason: RefusalReason::Unknown(reason.into()),
                    refusal_detail: refusal_detail.map(str::to_owned),
                },
                Some(format!("remote refused before start; the local fallback could not be restored after restart, so the command did not run: {error}")),
            );
        }
        Ok(())
    }
}
