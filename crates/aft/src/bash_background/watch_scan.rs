//! Feeding a task's newly captured output to its pattern watches.

use super::*;

impl BgTaskRegistry {
    pub(crate) fn scan_task_watch_output(&self, task: &Arc<BgTask>) {
        if self.active_watch_count(&task.task_id) == 0 {
            return;
        }
        let mode = match task.lock_state() {
            Ok(state) => state.metadata.mode.clone(),
            Err(_) => return,
        };
        let mut stdout = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stdout))
            .transpose()
            .ok()
            .flatten();
        let mut stderr = (mode == BgMode::Pipes)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Stderr))
            .transpose()
            .ok()
            .flatten();
        let mut pty = (mode == BgMode::Pty)
            .then(|| open_task_artifact(&task.paths, TaskArtifact::Pty))
            .transpose()
            .ok()
            .flatten();
        let drain = !task.is_running();
        let mut matches = Vec::new();
        // Catch up using the same fresh handles in bounded stdout/stderr rounds.
        // The chunk order matters: a once watch can match either stream, so a
        // later stdout chunk must not overtake stderr's first chunk. Terminal
        // drains keep their existing stdout-before-stderr ordering.
        let rounds = if drain { 1 } else { 16 };
        for _ in 0..rounds {
            let before = self.watch_stream_cursors(&task.task_id);
            match mode {
                BgMode::Pipes => {
                    let (Some(stdout), Some(stderr)) = (stdout.as_mut(), stderr.as_mut()) else {
                        return;
                    };
                    matches.extend(self.scan_watch_file(
                        &task.task_id,
                        &format!("{}:stdout", task.task_id),
                        stdout,
                        drain,
                    ));
                    matches.extend(self.scan_watch_file(
                        &task.task_id,
                        &format!("{}:stderr", task.task_id),
                        stderr,
                        drain,
                    ));
                }
                BgMode::Pty => {
                    let Some(pty) = pty.as_mut() else { return };
                    matches.extend(self.scan_watch_file(
                        &task.task_id,
                        &format!("{}:pty", task.task_id),
                        pty,
                        drain,
                    ));
                }
            }
            if self.active_watch_count(&task.task_id) == 0
                || self.watch_stream_cursors(&task.task_id) == before
            {
                break;
            }
        }
        matches.extend(self.take_watch_line_batches(&task.task_id, drain));
        let (stdout_offset, stderr_offset, pty_offset) = self.watch_stream_cursors(&task.task_id);
        if matches.is_empty() {
            // Advance durable cursors even when nothing matched so a restart
            // does not re-scan already-observed bytes.
            if self.task_has_watch_control(&task.task_id) {
                self.persist_task_watch_cursors(
                    &task.session_id,
                    &task.task_id,
                    stdout_offset,
                    stderr_offset,
                    pty_offset,
                );
            }
            return;
        }
        for mut pattern_match in matches {
            let mut fresh = pattern_match.clone();
            self.persist_watch_match(
                &task.session_id,
                &task.task_id,
                &mut pattern_match,
                stdout_offset,
                stderr_offset,
                pty_offset,
            );
            fresh.copy_delivery_identity(&pattern_match);
            self.emit_bash_pattern_match(&task.session_id, fresh);
        }
        self.persist_task_watch_cursors(
            &task.session_id,
            &task.task_id,
            stdout_offset,
            stderr_offset,
            pty_offset,
        );
    }

    pub(super) fn scan_watch_file(
        &self,
        task_id: &str,
        cursor_key: &str,
        file: &mut crate::bash_background::persistence::ValidatedArtifact,
        drain: bool,
    ) -> Vec<PatternMatch> {
        const CHUNK: u64 = 64 * 1024;
        // Charge reads even when another scanner advances the cursor, so a
        // contended running scan still yields after one chunk.
        let mut bytes_read = 0;
        let mut matches = Vec::new();
        // The capture file a withheld watch preview names, so the agent can
        // read the matched output itself.
        let output_file =
            self.task(task_id)
                .and_then(|task| match cursor_key.rsplit(':').next()? {
                    "stdout" => Some(task.paths.stdout.clone()),
                    "stderr" => Some(task.paths.stderr.clone()),
                    "pty" => Some(task.paths.pty.clone()),
                    _ => None,
                });
        loop {
            if !drain && bytes_read >= CHUNK as usize {
                break;
            }
            let cursor = match self.inner.watch_registry.lock() {
                Ok(registry) if registry.active_count(task_id) > 0 => {
                    registry.file_cursor(cursor_key)
                }
                _ => break,
            };
            let start = cursor.unwrap_or_else(|| file.len().unwrap_or(0));
            let Ok(bytes) = file.read_range(start, CHUNK) else {
                break;
            };
            if bytes.is_empty() && !drain {
                break;
            }
            let len = bytes.len();
            bytes_read += len;
            let Ok(mut registry) = self.inner.watch_registry.lock() else {
                break;
            };
            if registry.active_count(task_id) == 0 {
                break;
            }
            if registry.file_cursor(cursor_key) != cursor {
                continue;
            }
            let mut ranges = Vec::new();
            // A finished task's capture ends at its current length; a running
            // task may still extend the line its last bytes belong to.
            let at_end = drain && file.len().is_ok_and(|total| start + len as u64 >= total);
            let mut found = registry.scan_chunk(
                cursor_key,
                task_id,
                &bytes,
                start,
                at_end,
                &mut |_, _, _, from, to| {
                    ranges.push(from..to);
                    (String::new(), String::new())
                },
            );
            drop(registry);
            // Keep cursor and one-shot updates atomic, but never read presentation
            // bytes or mask credentials while holding the watch registry lock.
            for (matched, range) in found.iter_mut().zip(ranges) {
                let rendered = file
                    .len()
                    .ok()
                    .and_then(|len| {
                        if len >= crate::bash_background::buffer::DISK_LIMIT_BYTES
                            || range.end > len
                        {
                            return None;
                        }
                        let source = completion_preview::watch_preview_source_range(
                            range.start as usize..range.end as usize,
                            len as usize,
                        )?;
                        let bytes = file
                            .read_range(source.start as u64, source.len() as u64)
                            .ok()?;
                        (bytes.len() == source.len()).then(|| {
                            self.render_watch_preview_window(
                                task_id,
                                &bytes,
                                range.start as usize - source.start
                                    ..range.end as usize - source.start,
                                source.start == 0,
                                source.end == len as usize,
                                output_file.as_deref(),
                            )
                        })
                    })
                    .unwrap_or_else(|| {
                        (
                            crate::log_redact::SECRET_PLACEHOLDER.to_string(),
                            completion_preview::TRUNCATED_PREVIEW_WITHHELD.to_string(),
                        )
                    });
                let (match_text, context) = rendered;
                matched.context = context;
                if !matched.literal_pattern {
                    matched.match_text = match_text;
                }
            }
            matches.extend(found);
            if len < CHUNK as usize {
                break;
            }
        }
        matches
    }
}
