//! Verified capture reads and the checked masker used by the operator surface.
use super::*;

pub(crate) struct ShellSnapshot {
    pub metadata: PersistedTask,
    pub stdout: u64,
    pub stderr: u64,
    pub last_output_at_ms: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct ShellReader {
    live: Option<BgTaskRegistry>,
    metadata: PersistedTask,
    paths: TaskPaths,
    order: Arc<DbWriteOrder>,
}

impl ShellReader {
    pub(crate) fn is_persisted(&self) -> bool {
        self.live.is_none()
    }

    pub(crate) fn persisted(row: BashTaskRow) -> Option<(ShellSnapshot, Self)> {
        let output = Path::new(row.stdout_path.as_deref()?);
        let parent = output.parent()?;
        let session_dir = if parent.file_name()? == "io" {
            parent.parent()?.parent()?
        } else {
            parent
        };
        if session_dir.file_name()?.to_str()? != crate::backup::hash_session(&row.session_id) {
            return None;
        }
        let task = resolve_task_layout(session_dir, &row.task_id).ok()?;
        let mut metadata = read_task_at(&task).ok()?;
        if metadata.task_id != row.task_id
            || metadata.session_id != row.session_id
            || metadata
                .harness
                .as_deref()
                .is_some_and(|harness| harness != row.harness)
        {
            return None;
        }
        #[cfg(unix)]
        if !metadata.is_terminal() {
            let _ = super::super::remote::terminal_metadata_from_proof(&mut metadata);
        }
        if !metadata.is_terminal() {
            if let Some(marker) = read_exit_marker(&task.paths).ok().flatten() {
                metadata = terminal_metadata_from_marker(metadata, marker, None);
            } else if metadata.remote.is_none()
                && !BgTaskRegistry::persisted_task_process_is_alive(&metadata)
            {
                let reason = restart_fate_unknown_reason(&metadata, &task.paths);
                metadata.mark_terminal(BgTaskStatus::FateUnknown, None, Some(reason));
            }
        }
        let buffer = BgBuffer::registered(&task.paths, metadata.mode.clone());
        let stdout = buffer.stream_len(StreamKind::Stdout);
        let stderr = if metadata.mode == BgMode::Pty {
            0
        } else {
            buffer.stream_len(StreamKind::Stderr)
        };
        let last_output_at_ms = [&task.paths.stdout, &task.paths.stderr, &task.paths.pty]
            .iter()
            .filter_map(|path| {
                std::fs::metadata(path)
                    .ok()
                    .filter(|m| m.len() > 0)?
                    .modified()
                    .ok()?
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_millis() as u64)
            })
            .max();
        let reader = Self {
            live: None,
            order: DbWriteOrder::for_task(&metadata.session_id, &metadata.task_id),
            metadata: metadata.clone(),
            paths: task.paths,
        };
        metadata.command = if metadata.has_safe_completion_redaction_after_restart() {
            crate::log_redact::redact_command(&metadata.command).into_owned()
        } else {
            crate::log_redact::SECRET_PLACEHOLDER.into()
        };
        Some((
            ShellSnapshot {
                metadata,
                stdout,
                stderr,
                last_output_at_ms,
            },
            reader,
        ))
    }

    pub(crate) fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String> {
        if let Some(live) = &self.live {
            return live.shell_revision(task_id, fingerprint);
        }
        let mut order = self.order.lock_json_writes();
        let layout = resolve_task_layout(&self.paths.session_dir, task_id)
            .map_err(|error| error.to_string())?;
        let current = read_task_at(&layout).map_err(|error| error.to_string())?;
        if current.shell_fingerprint.as_deref() == Some(fingerprint)
            && (current.is_terminal() || !self.metadata.is_terminal())
        {
            return Ok(current.shell_revision);
        }
        let updated = update_task_at(&layout, |metadata| {
            if !metadata.is_terminal() && self.metadata.is_terminal() {
                metadata.status = self.metadata.status.clone();
                metadata.finished_at = self.metadata.finished_at;
                metadata.exit_code = self.metadata.exit_code;
                metadata.status_reason = self.metadata.status_reason.clone();
                metadata.duration_ms = self.metadata.duration_ms;
            }
            if metadata.shell_fingerprint.as_deref() != Some(fingerprint) {
                metadata.shell_revision = metadata.shell_revision.saturating_add(1);
                metadata.shell_fingerprint = Some(fingerprint.to_owned());
            }
        })
        .map_err(|error| error.to_string())?;
        *order += 1;
        Ok(updated.shell_revision)
    }

    pub(crate) fn shell_read(
        &self,
        _task_id: &str,
        artifact: TaskArtifact,
        from: u64,
        limit: u64,
    ) -> Result<Vec<u8>, String> {
        let mut file =
            open_task_artifact(&self.paths, artifact).map_err(|error| error.to_string())?;
        file.read_range(from, limit)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn shell_longest_secret(&self, task_id: &str) -> usize {
        self.live
            .as_ref()
            .map_or(crate::log_redact::TICKET_HALO_BYTES, |live| {
                live.shell_longest_secret(task_id)
            })
    }

    pub(crate) fn shell_dimensions(&self, _task_id: &str) -> Option<(u16, u16)> {
        Some((self.metadata.pty_rows?, self.metadata.pty_cols?))
    }

    pub(crate) fn shell_masked_range(
        &self,
        task_id: &str,
        artifact: TaskArtifact,
        from: u64,
        to: u64,
    ) -> String {
        let mut text = String::new();
        let mut start = from;
        while start < to {
            let mut end = (start + 4096).min(to);
            if end < to {
                let edge = self
                    .shell_read(task_id, artifact, end, 4)
                    .unwrap_or_default();
                end = (end + edge.iter().take_while(|byte| **byte & 0xc0 == 0x80).count() as u64)
                    .min(to);
            }
            text.push_str(&self.masked_part(task_id, artifact, start, end));
            start = end;
        }
        text
    }

    fn masked_part(&self, task_id: &str, artifact: TaskArtifact, from: u64, to: u64) -> String {
        if let Some(live) = &self.live {
            return live.shell_masked_range(task_id, artifact, from, to);
        }
        let masked = (|| {
            if !self.metadata.has_safe_completion_redaction_after_restart() {
                return None;
            }
            let mut file = open_task_artifact(&self.paths, artifact).ok()?;
            let length = file.len().ok()? as usize;
            let source =
                completion_preview::watch_preview_source_range(from as usize..to as usize, length)?;
            let bytes = file
                .read_range(source.start as u64, source.len() as u64)
                .ok()?;
            BgTaskRegistry::mask_window_with_values(
                &bytes,
                from as usize - source.start..to as usize - source.start,
                source.start == 0,
                source.end == length,
                &crate::log_redact::SecretValues::default(),
                self.metadata.completion_ticket_digest.as_deref(),
            )
            .ok()
            .map(|masked| masked.0)
        })();
        masked.unwrap_or_else(|| crate::log_redact::SECRET_PLACEHOLDER.into())
    }
}

impl BgTaskRegistry {
    pub(crate) fn shell_reader(&self, task_id: &str) -> Option<ShellReader> {
        let task = self.task(task_id)?;
        let state = task.lock_state().ok()?;
        Some(ShellReader {
            live: Some(self.clone()),
            metadata: state.metadata.clone(),
            paths: task.paths.clone(),
            order: task.db_write_order.clone(),
        })
    }
    pub(crate) fn shell_snapshots(&self) -> Vec<ShellSnapshot> {
        let tasks: Vec<_> = self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        tasks
            .into_iter()
            .filter_map(|task| {
                let state = task.lock_state().ok()?;
                let stdout = state.buffer.stream_len(StreamKind::Stdout);
                let stderr = if state.metadata.mode == BgMode::Pty {
                    0
                } else {
                    state.buffer.stream_len(StreamKind::Stderr)
                };
                let path = if state.metadata.mode == BgMode::Pty {
                    &task.paths.pty
                } else {
                    &task.paths.stdout
                };
                let last_output_at_ms = (stdout + stderr > 0)
                    .then(|| {
                        [path, &task.paths.stderr]
                            .iter()
                            .filter_map(|path| {
                                std::fs::metadata(path)
                                    .ok()?
                                    .modified()
                                    .ok()?
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .ok()
                                    .map(|duration| duration.as_millis() as u64)
                            })
                            .max()
                    })
                    .flatten();
                let mut metadata = state.metadata.clone();
                let secrets = task
                    .completion_secrets
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                metadata.command = match secrets.as_ref() {
                    Some(secrets) => {
                        crate::log_redact::redact_command(&secrets.redact(&metadata.command))
                            .into_owned()
                    }
                    None => crate::log_redact::redact_command(&metadata.command).into_owned(),
                };
                Some(ShellSnapshot {
                    metadata,
                    stdout,
                    stderr,
                    last_output_at_ms,
                })
            })
            .collect()
    }

    pub(crate) fn shell_revision(&self, task_id: &str, fingerprint: &str) -> Result<u64, String> {
        let task = self.task(task_id).ok_or("shell no longer registered")?;
        let mut db = DeferredDbWrites::new(self, &task);
        let mut state = task.lock_state().map_err(|_| "task lock poisoned")?;
        if state.metadata.shell_fingerprint.as_deref() != Some(fingerprint) {
            state.metadata.shell_revision = state.metadata.shell_revision.saturating_add(1);
            state.metadata.shell_fingerprint = Some(fingerprint.to_owned());
            self.persist_task_locked(&task, &state.metadata, &mut db)
                .map_err(|error| error.to_string())?;
        }
        Ok(state.metadata.shell_revision)
    }

    pub(crate) fn shell_longest_secret(&self, task_id: &str) -> usize {
        self.task(task_id)
            .and_then(|task| {
                task.completion_secrets.lock().ok().and_then(|secrets| {
                    secrets
                        .as_ref()
                        .map(crate::log_redact::SecretValues::longest_value_len)
                })
            })
            .unwrap_or(0)
            .max(crate::log_redact::TICKET_HALO_BYTES)
    }

    pub(crate) fn shell_masked_range(
        &self,
        task_id: &str,
        artifact: TaskArtifact,
        from: u64,
        to: u64,
    ) -> String {
        self.masked_watch_range_checked(task_id, artifact, from, to)
            .unwrap_or_else(|_| crate::log_redact::SECRET_PLACEHOLDER.to_owned())
    }
}
