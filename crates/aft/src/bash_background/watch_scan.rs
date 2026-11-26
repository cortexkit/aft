//! Feeding a task's newly captured output to its pattern watches.

use super::*;

/// How long one call may spend feeding a task's output to its watches. The
/// watchdog makes these calls for every task of every project on one thread,
/// so a task with a large backlog continues on the next pass instead of
/// holding that thread. Each stream still reads at least one chunk per call.
const WATCH_SCAN_BUDGET: Duration = Duration::from_millis(50);

#[cfg(test)]
thread_local! {
    static WATCH_SCAN_BUDGET_FOR_TEST: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

fn watch_scan_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = WATCH_SCAN_BUDGET_FOR_TEST.with(std::cell::Cell::get) {
        return budget;
    }
    WATCH_SCAN_BUDGET
}

/// Give watch scans on this thread `budget` instead of [`WATCH_SCAN_BUDGET`].
#[cfg(test)]
pub(crate) fn set_watch_scan_budget_for_test(budget: Option<Duration>) {
    WATCH_SCAN_BUDGET_FOR_TEST.with(|cell| cell.set(budget));
}

#[cfg(test)]
thread_local! {
    static MATCH_WRITES_BEFORE_STOP: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Make watch scans on this thread stop right after saving `writes` matches,
/// leaving the rest of the scan undone as a restart at that point would.
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn set_match_writes_before_stop_for_test(writes: Option<usize>) {
    MATCH_WRITES_BEFORE_STOP.with(|cell| cell.set(writes));
}

#[cfg(test)]
fn stop_after_match_write_for_test() -> bool {
    MATCH_WRITES_BEFORE_STOP.with(|cell| match cell.get() {
        Some(left) if left <= 1 => true,
        Some(left) => {
            cell.set(Some(left - 1));
            false
        }
        None => false,
    })
}

/// Each watched stream's saved cursor from a task's watch `row`, with the
/// capture bytes just before it (the overlap a scan keeps), so restored
/// watches resume scanning as if the restart had not happened. `files` are
/// the task's stdout, stderr and pseudoterminal captures; a stream whose
/// capture is missing or unreadable resumes with no overlap.
pub(super) fn resumed_cursors(
    task: &BgTask,
    mode: &BgMode,
    row: &BashPatternWatchRow,
    files: [&mut Option<crate::bash_background::persistence::ValidatedArtifact>; 3],
) -> Vec<(String, u64, Vec<u8>)> {
    let [stdout, stderr, pty] = files;
    let streams = match mode {
        BgMode::Pipes => vec![
            ("stdout", row.stdout_offset, stdout),
            ("stderr", row.stderr_offset, stderr),
        ],
        BgMode::Pty => vec![("pty", row.pty_offset, pty)],
    };
    streams
        .into_iter()
        .map(|(stream, offset, file)| {
            let offset = offset.max(0) as u64;
            let overlap = offset.min(crate::bash_background::watches::SCAN_OVERLAP_BYTES as u64);
            let before = file
                .as_mut()
                .and_then(|file| file.read_range(offset - overlap, overlap).ok())
                .filter(|bytes| bytes.len() as u64 == overlap)
                .unwrap_or_default();
            (format!("{}:{stream}", task.task_id), offset, before)
        })
        .collect()
}

/// Each scanned task's streams that its latest watch scan could not open,
/// for the watch's exit notice. Bounded: a full map forgets its entries
/// rather than grow, which can only drop a note, never invent one.
static UNREADABLE_STREAMS: std::sync::LazyLock<
    Mutex<HashMap<String, std::collections::BTreeSet<&'static str>>>,
> = std::sync::LazyLock::new(Default::default);

const MAX_TASKS_WITH_UNREADABLE_STREAMS: usize = 256;

fn record_stream_readable(task_id: &str, stream: &'static str, readable: bool) {
    let mut tasks = UNREADABLE_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if readable {
        if let Some(streams) = tasks.get_mut(task_id) {
            streams.remove(stream);
            if streams.is_empty() {
                tasks.remove(task_id);
            }
        }
        return;
    }
    if !tasks.contains_key(task_id) && tasks.len() >= MAX_TASKS_WITH_UNREADABLE_STREAMS {
        tasks.clear();
    }
    tasks.entry(task_id.to_string()).or_default().insert(stream);
}

/// The lines a watch's exit notice adds for `task_id`'s streams that its
/// latest scan could not read, so the notice does not suggest the whole
/// output was checked.
pub(super) fn unreadable_streams_note(task_id: &str) -> Option<String> {
    let tasks = UNREADABLE_STREAMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let streams = tasks.get(task_id)?;
    Some(
        streams
            .iter()
            .map(|stream| {
                format!("watch: {stream} could not be read, so it was not checked for the pattern")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

impl BgTaskRegistry {
    /// Run a finished task's final watch scan, then its terminal transition,
    /// which routes the exit to watches that never matched. A backlog the scan
    /// budget cannot finish is drained on its own thread, which then runs the
    /// transition: the watchdog is not held, and the output printed before the
    /// exit still reaches its watches before the exit is routed.
    pub(super) fn scan_watches_then_post_terminal(&self, task: &Arc<BgTask>) -> Result<(), String> {
        if self.scan_task_watch_output(task) {
            return self.post_terminal_transition(task, true);
        }
        let registry = self.clone();
        let drained = Arc::clone(task);
        let spawned = std::thread::Builder::new()
            .name("aft-watch-drain".into())
            .spawn(move || {
                registry.finish_watch_drain(&drained);
                if let Err(error) = registry.post_terminal_transition(&drained, true) {
                    crate::slog_warn!(
                        "failed to finish background task {} after its watch drain: {error}",
                        drained.task_id
                    );
                }
            });
        if spawned.is_err() {
            self.finish_watch_drain(task);
            return self.post_terminal_transition(task, true);
        }
        Ok(())
    }

    /// Scan until the watches have read all of a finished task's output, or a
    /// scan makes no progress.
    fn finish_watch_drain(&self, task: &Arc<BgTask>) {
        loop {
            let before = self.watch_stream_cursors(&task.task_id);
            if self.scan_task_watch_output(task)
                || self.watch_stream_cursors(&task.task_id) == before
            {
                return;
            }
        }
    }
    /// Feed `task`'s output written since the last scan to its watches,
    /// within [`WATCH_SCAN_BUDGET`]. Returns whether every watched stream was
    /// read to its end, which for a finished task means its watches have seen
    /// all of its output.
    pub(crate) fn scan_task_watch_output(&self, task: &Arc<BgTask>) -> bool {
        if self.active_watch_count(&task.task_id) == 0 {
            return true;
        }
        let mode = match task.lock_state() {
            Ok(state) => state.metadata.mode.clone(),
            Err(_) => return true,
        };
        let open = |stream: &'static str, artifact| {
            let file = open_task_artifact(&task.paths, artifact)
                .inspect_err(|error| {
                    crate::slog_debug!(
                        "watch scan for {} could not open its {stream} capture: {error}",
                        task.task_id
                    );
                })
                .ok();
            record_stream_readable(&task.task_id, stream, file.is_some());
            file
        };
        // A stream that cannot be opened is left out of this scan; the other
        // stream is still scanned, so its matches are not lost with it. The
        // watch's exit notice names a stream the last scan could not read.
        let mut files: Vec<_> = match mode {
            BgMode::Pipes => vec![
                ("stdout", open("stdout", TaskArtifact::Stdout)),
                ("stderr", open("stderr", TaskArtifact::Stderr)),
            ],
            BgMode::Pty => vec![("pty", open("pty", TaskArtifact::Pty))],
        }
        .into_iter()
        .filter_map(|(stream, file)| Some((stream, file?)))
        .collect();
        if files.is_empty() {
            // Nothing can be scanned; waiting would not change that.
            return true;
        }
        let deadline = Instant::now() + watch_scan_budget();
        let drain = !task.is_running();
        let mut matches = Vec::new();
        // Catch up using the same fresh handles in stdout/stderr rounds of one
        // chunk each. The chunk order matters: a once watch can match either
        // stream, so a later stdout chunk must not overtake stderr's first
        // chunk. Terminal drains keep their stdout-before-stderr ordering, so
        // a drain the budget stops does not start the next stream.
        let mut at_end;
        loop {
            let before = self.watch_stream_cursors(&task.task_id);
            at_end = true;
            for (stream, file) in &mut files {
                if drain && !at_end {
                    break;
                }
                let (found, reached_end) = self.scan_watch_file_until(
                    &task.task_id,
                    &format!("{}:{stream}", task.task_id),
                    file,
                    drain,
                    Some(deadline),
                );
                matches.extend(found);
                at_end &= reached_end;
            }
            if self.active_watch_count(&task.task_id) == 0 {
                at_end = true;
            }
            if drain
                || at_end
                || Instant::now() >= deadline
                || self.watch_stream_cursors(&task.task_id) == before
            {
                break;
            }
        }
        // Partial last lines are final only once the drain read everything.
        matches.extend(self.take_watch_line_batches(&task.task_id, drain && at_end));
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
            return at_end;
        }
        // Each match is saved without moving the watch's saved cursors, which
        // move past these bytes only once every match in them is saved: a
        // restart in between rescans from the old cursors instead of skipping
        // a match that was never saved.
        let mut saved = true;
        for mut pattern_match in matches {
            let mut fresh = pattern_match.clone();
            saved &=
                self.persist_watch_match(&task.session_id, &task.task_id, &mut pattern_match, None);
            fresh.copy_delivery_identity(&pattern_match);
            self.emit_bash_pattern_match(&task.session_id, fresh);
            #[cfg(test)]
            if stop_after_match_write_for_test() {
                return at_end;
            }
        }
        if saved {
            self.persist_task_watch_cursors(
                &task.session_id,
                &task.task_id,
                stdout_offset,
                stderr_offset,
                pty_offset,
            );
        }
        at_end
    }

    pub(super) fn scan_watch_file(
        &self,
        task_id: &str,
        cursor_key: &str,
        file: &mut crate::bash_background::persistence::ValidatedArtifact,
        drain: bool,
    ) -> Vec<PatternMatch> {
        self.scan_watch_file_until(task_id, cursor_key, file, drain, None)
            .0
    }

    /// Scan one stream's unread bytes: one chunk for a running task, or up to
    /// the end for a drain, stopping at `deadline` once a chunk was read. Also
    /// returns whether the scan reached the end of what the stream holds (or
    /// nothing is left to watch).
    fn scan_watch_file_until(
        &self,
        task_id: &str,
        cursor_key: &str,
        file: &mut crate::bash_background::persistence::ValidatedArtifact,
        drain: bool,
        deadline: Option<Instant>,
    ) -> (Vec<PatternMatch>, bool) {
        const CHUNK: u64 = 64 * 1024;
        // Charge reads even when another scanner advances the cursor, so a
        // contended running scan still yields after one chunk.
        let mut bytes_read = 0;
        let mut matches = Vec::new();
        // A scan that cannot go on (no watch left, an unreadable stream) has
        // nothing more to wait for either.
        let mut reached_end = true;
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
        // A drain reads to the end of what is written; that end is where the
        // stream ends only once the task has finished. A running task (a
        // restart's catch-up scan) may still extend the line its bytes end in.
        let finished = self.task(task_id).is_none_or(|task| !task.is_running());
        loop {
            let out_of_budget =
                bytes_read > 0 && deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if (!drain && bytes_read >= CHUNK as usize) || out_of_budget {
                reached_end = false;
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
            let at_end =
                drain && finished && file.len().is_ok_and(|total| start + len as u64 >= total);
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
            for ((matched, more), range) in found.iter_mut().zip(ranges) {
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
                if let Some(line) = super::super::watches::more_matches_line(*more) {
                    matched.context.push('\n');
                    matched.context.push_str(&line);
                }
                if !matched.literal_pattern {
                    matched.match_text = match_text;
                }
            }
            matches.extend(found.into_iter().map(|(found, _more)| found));
            if len < CHUNK as usize {
                break;
            }
        }
        (matches, reached_end)
    }
}
