//! Verified capture reads and the checked masker used by the operator surface.
use super::*;

// One default output chunk of context covers ordinary credential headers and
// complete log lines without buffering an arbitrarily long line or key block.
// Longer undecidable formats are withheld; exact secrets add their full length.
const SHELL_MASK_CONTEXT_BYTES: usize = 16 * 1024;
pub(crate) const SHELL_MASK_WITHHELD: &str = "[output withheld: masking context unavailable]";

#[derive(Default)]
struct FormatScan {
    length: u64,
    possible: bool,
    quotes: bool,
    private_key: bool,
}

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
    format_scans: Arc<Mutex<HashMap<TaskArtifact, FormatScan>>>,
    stop: Option<tokio_util::sync::CancellationToken>,
}

impl ShellReader {
    pub(crate) fn with_stop(mut self, stop: tokio_util::sync::CancellationToken) -> Self {
        self.stop = Some(stop);
        self
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.as_ref().is_some_and(|stop| stop.is_cancelled())
    }
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
            let _ = super::remote::terminal_metadata_from_proof(&mut metadata);
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
            format_scans: Default::default(),
            stop: None,
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
        let mut updated = current;
        if !updated.is_terminal() && self.metadata.is_terminal() {
            updated.status = self.metadata.status.clone();
            updated.finished_at = self.metadata.finished_at;
            updated.exit_code = self.metadata.exit_code;
            updated.status_reason = self.metadata.status_reason.clone();
            updated.duration_ms = self.metadata.duration_ms;
        }
        if updated.shell_fingerprint.as_deref() != Some(fingerprint) {
            updated.shell_revision = updated.shell_revision.saturating_add(1);
            updated.shell_fingerprint = Some(fingerprint.to_owned());
        }
        write_task_at(&layout, &updated).map_err(|error| error.to_string())?;
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
        if self.stopped() {
            return Err("shell output read cancelled".into());
        }
        let mut file =
            open_task_artifact(&self.paths, artifact).map_err(|error| error.to_string())?;
        file.read_range(from, limit)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn shell_longest_secret(&self, task_id: &str) -> usize {
        self.live
            .as_ref()
            .map_or(crate::log_redact::TICKET_HALO_BYTES + 1, |live| {
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
        let masked = (|| {
            if self.stopped() {
                return None;
            }
            let mut file = open_task_artifact(&self.paths, artifact).ok()?;
            let length = file.len().ok()?;
            if from > to || to > length {
                return None;
            }
            let (possible, quotes, private_key) =
                self.format_context_possible(artifact, length).ok()?;
            let no_format_context = !possible;
            let context = self
                .shell_longest_secret(task_id)
                .max(SHELL_MASK_CONTEXT_BYTES) as u64;
            let start = from.saturating_sub(context);
            let end = to.saturating_add(context).min(length);
            let bytes = file.read_range(start, end - start).ok()?;
            let range = (from - start) as usize..(to - start) as usize;
            if let Some(live) = &self.live {
                let task = live.task(task_id)?;
                let digest = task
                    .lock_state()
                    .ok()?
                    .metadata
                    .completion_ticket_digest
                    .clone();
                let values = task.completion_secrets.lock().ok()?;
                BgTaskRegistry::shell_mask_window_with_values(
                    &bytes,
                    range,
                    start == 0,
                    end == length,
                    no_format_context,
                    (quotes && start > 0, private_key && start > 0),
                    values.as_ref()?,
                    digest.as_deref(),
                )
            } else {
                if !self.metadata.has_safe_completion_redaction_after_restart() {
                    return None;
                }
                BgTaskRegistry::shell_mask_window_with_values(
                    &bytes,
                    range,
                    start == 0,
                    end == length,
                    no_format_context,
                    (quotes && start > 0, private_key && start > 0),
                    &crate::log_redact::SecretValues::default(),
                    self.metadata.completion_ticket_digest.as_deref(),
                )
            }
        })();
        masked.unwrap_or_else(|| SHELL_MASK_WITHHELD.into())
    }

    fn format_context_possible(
        &self,
        artifact: TaskArtifact,
        length: u64,
    ) -> Result<(bool, bool, bool), String> {
        let mut scans = self
            .format_scans
            .try_lock()
            .map_err(|_| "mask scan unavailable")?;
        let scan = scans.entry(artifact).or_default();
        if length < scan.length {
            *scan = FormatScan::default();
        }
        if scan.quotes && scan.private_key {
            return Ok((true, true, true));
        }
        let overlap = crate::log_redact::shells_format_scan_overlap() as u64;
        let mut offset = scan.length.saturating_sub(overlap);
        let mut file =
            open_task_artifact(&self.paths, artifact).map_err(|error| error.to_string())?;
        while offset < length {
            if self.stopped() {
                return Err("shell output read cancelled".into());
            }
            let bytes = file
                .read_range(
                    offset,
                    (SHELL_MASK_CONTEXT_BYTES as u64).min(length - offset),
                )
                .map_err(|error| error.to_string())?;
            if bytes.is_empty() {
                return Err("capture changed during masking scan".into());
            }
            let text = String::from_utf8_lossy(&bytes);
            scan.possible |= crate::log_redact::shells_may_need_format_context(&text);
            scan.quotes |= text.contains('"');
            scan.private_key |= text.contains("PRIVATE KEY-----");
            if scan.quotes && scan.private_key {
                scan.length = length;
                return Ok((true, true, true));
            }
            let next = offset + bytes.len() as u64;
            if next == length {
                break;
            }
            offset = next.saturating_sub(overlap);
        }
        scan.length = length;
        Ok((scan.possible, scan.quotes, scan.private_key))
    }
}

impl BgTaskRegistry {
    #[cfg(test)]
    pub(crate) fn insert_shell_fixture(
        &self,
        metadata: PersistedTask,
        paths: TaskPaths,
    ) -> Result<(), String> {
        self.insert_rehydrated_task(metadata, paths, false)
    }

    pub(crate) fn shell_reader(&self, task_id: &str) -> Option<ShellReader> {
        let task = self.task(task_id)?;
        let state = task.lock_state().ok()?;
        Some(ShellReader {
            live: Some(self.clone()),
            metadata: state.metadata.clone(),
            paths: task.paths.clone(),
            order: task.db_write_order.clone(),
            format_scans: Default::default(),
            stop: None,
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
                    None if metadata.has_safe_completion_redaction_after_restart() => {
                        crate::log_redact::redact_command(&metadata.command).into_owned()
                    }
                    None => crate::log_redact::SECRET_PLACEHOLDER.into(),
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
            .max(crate::log_redact::TICKET_HALO_BYTES + 1)
    }
}
