use super::*;
use crate::log_redact::aft_redactor;
use std::borrow::Cow;

// Every host displays this Rust-owned text verbatim instead of reconstructing
// an unsafe preview from bash_status or the capture files after a restart.
pub(super) const RESTART_PREVIEW_WITHHELD: &str =
    "Output preview withheld: this task's secret values aren't available after the daemon restarted; read the output with bash_status.";
/// The watch-match form of [`RESTART_PREVIEW_WITHHELD`]. A watch usually
/// fires while its task still runs, and `bash_status` shows no output for a
/// running task, so the notice names the output file to read instead.
pub(super) const RESTART_WATCH_PREVIEW_WITHHELD: &str =
    "Output preview withheld: this task's secret values aren't available after the daemon restarted.";
pub(super) const TRUNCATED_PREVIEW_WITHHELD: &str =
    "Output preview withheld because intact output is unavailable for credential masking; use bash_status.";

const WATCH_SECRET_HALO_BYTES: usize = 1024;
/// A watch preview is masked over whole lines (see `watch_mask_region`). When
/// those lines exceed this many bytes, the preview is withheld instead.
pub(super) const WATCH_PREVIEW_LINE_CAP_BYTES: usize =
    super::super::watch_lines::MASKING_LINE_CAP_BYTES;

#[derive(Clone, Copy, Debug)]
pub(super) struct UnavailablePreview {
    pub line_limit: bool,
    /// The task's secret values are gone after a daemon restart. A watch
    /// notice then shows [`restart_watch_notice`] instead of `reason`.
    pub restart: bool,
    pub reason: &'static str,
}

impl UnavailablePreview {
    pub(super) fn context(reason: &'static str) -> Self {
        Self {
            line_limit: false,
            restart: false,
            reason,
        }
    }
    pub(super) fn line_limit() -> Self {
        Self {
            line_limit: true,
            restart: false,
            reason: TRUNCATED_PREVIEW_WITHHELD,
        }
    }
    pub(super) fn restart() -> Self {
        Self {
            line_limit: false,
            restart: true,
            reason: RESTART_PREVIEW_WITHHELD,
        }
    }
}

/// What a watch notice shows in place of output withheld after a restart:
/// [`RESTART_WATCH_PREVIEW_WITHHELD`] and, when known, the capture file the
/// match was found in, so the agent can read it directly.
pub(super) fn restart_watch_notice(output_file: Option<&std::path::Path>) -> String {
    let mut notice = RESTART_WATCH_PREVIEW_WITHHELD.to_string();
    if let Some(path) = output_file {
        notice.push_str("\nOutput file: ");
        notice.push_str(&aft_redactor(&path.to_string_lossy()));
    }
    notice
}

/// `masked`, the masked form of the output line `raw`, with `needle` (a
/// substring watch's own pattern) shown wherever masking hid it, at most once
/// per placeholder. Masking can swallow the pattern into a placeholder when
/// it is part of a credential, or is one; the agent supplied it, so showing
/// it reveals nothing the agent does not have.
///
/// The result is built only from `masked`'s own text, its placeholders and
/// parts of `needle`, so no other raw byte can reach it. Each placeholder is
/// matched to the raw bytes it replaced by aligning the visible text around
/// it; where a placeholder hid part of the pattern, that part of the pattern
/// is shown once and the rest of the hidden bytes stay behind placeholders. If
/// the alignment fails, `masked` is returned unchanged.
pub(super) fn keep_pattern_visible(raw: &str, masked: &str, needle: &str) -> String {
    use crate::log_redact::{SECRET_PLACEHOLDER, URL_CREDENTIAL_PLACEHOLDER};
    // Without a placeholder nothing was hidden. (A pattern can itself appear
    // inside placeholder text, so occurrences are not compared by count.)
    if needle.is_empty()
        || (!masked.contains(SECRET_PLACEHOLDER) && !masked.contains(URL_CREDENTIAL_PLACEHOLDER))
    {
        return masked.to_string();
    }
    // Split `masked` into visible text and placeholders, in order.
    let mut tokens: Vec<(bool, &str)> = Vec::new();
    let mut rest = masked;
    while !rest.is_empty() {
        let next = [SECRET_PLACEHOLDER, URL_CREDENTIAL_PLACEHOLDER]
            .iter()
            .filter_map(|placeholder| rest.find(placeholder).map(|at| (at, *placeholder)))
            .min_by_key(|(at, placeholder)| (*at, std::cmp::Reverse(placeholder.len())));
        match next {
            Some((at, placeholder)) => {
                if at > 0 {
                    tokens.push((false, &rest[..at]));
                }
                tokens.push((true, placeholder));
                rest = &rest[at + placeholder.len()..];
            }
            None => {
                tokens.push((false, rest));
                rest = "";
            }
        }
    }
    // The raw byte range each token stands for. A placeholder replaced at
    // least one byte and ends where the next visible text resumes.
    let mut spans = Vec::with_capacity(tokens.len());
    let mut position = 0;
    let mut pending: Option<usize> = None;
    for (index, (placeholder, text)) in tokens.iter().enumerate() {
        if *placeholder {
            if pending.is_some() {
                // Two adjacent placeholders cannot be told apart.
                return masked.to_string();
            }
            pending = Some(spans.len());
            spans.push(position..position);
            continue;
        }
        let found = match pending.take() {
            None => raw[position..].starts_with(text).then_some(position),
            Some(slot) => {
                // The placeholder replaced at least one whole character.
                let Some(earliest) = raw[position..]
                    .chars()
                    .next()
                    .map(|first| position + first.len_utf8())
                else {
                    return masked.to_string();
                };
                let found = if index + 1 == tokens.len() {
                    raw.ends_with(text)
                        .then(|| raw.len() - text.len())
                        .filter(|at| *at >= earliest)
                } else {
                    raw.get(earliest..)
                        .and_then(|tail| tail.find(text))
                        .map(|at| earliest + at)
                };
                if let Some(at) = found {
                    spans[slot] = position..at;
                }
                found
            }
        };
        let Some(at) = found else {
            return masked.to_string();
        };
        spans.push(at..at + text.len());
        position = at + text.len();
    }
    if let Some(slot) = pending {
        if position >= raw.len() {
            return masked.to_string();
        }
        spans[slot] = position..raw.len();
    } else if position != raw.len() {
        return masked.to_string();
    }
    let occurrences: Vec<std::ops::Range<usize>> = raw
        .match_indices(needle)
        .map(|(at, _)| at..at + needle.len())
        .collect();
    let mut out = String::with_capacity(masked.len() + needle.len());
    for ((placeholder, text), span) in tokens.iter().zip(spans) {
        if !*placeholder {
            out.push_str(text);
            continue;
        }
        // Show at most one occurrence per placeholder. Repeating a short
        // pattern across a credential would spell out its structure.
        let mut cursor = span.start;
        if let Some(occurrence) = occurrences
            .iter()
            .find(|occurrence| occurrence.start < span.end && occurrence.end > span.start)
        {
            let from = occurrence.start.max(span.start);
            let to = occurrence.end.min(span.end);
            if from > cursor {
                out.push_str(text);
            }
            out.push_str(&needle[from - occurrence.start..to - occurrence.start]);
            cursor = to;
        }
        if cursor < span.end {
            out.push_str(text);
        }
    }
    out
}
/// Bytes read beyond the largest maskable region on each side, so a JSON field
/// or private-key block that crosses a line break into the region can be seen
/// whole and the region widened to include it.
const WATCH_CROSS_LINE_HALO_BYTES: usize = 8 * 1024;
pub(super) const WATCH_PREVIEW_SOURCE_BYTES: usize =
    2 * (WATCH_PREVIEW_LINE_CAP_BYTES + WATCH_CROSS_LINE_HALO_BYTES);

impl BgTaskRegistry {
    pub(super) fn saved_completion_preview(
        &self,
        metadata: &PersistedTask,
    ) -> Option<crate::bash_background::persistence::PersistedCompletionPreview> {
        if metadata.completion_has_non_ticket_env_secrets.is_none() {
            return None;
        }
        self.task(&metadata.task_id)
            .and_then(|task| task.lock_state().ok()?.metadata.completion_preview.clone())
            .or_else(|| metadata.completion_preview.clone())
    }

    pub(super) fn save_completion_preview(
        &self,
        task_id: &str,
        output_preview: &str,
        output_truncated: bool,
        bash_output_list_envelope: &Option<crate::list_envelope::ListEnvelope>,
    ) -> crate::bash_background::persistence::PersistedCompletionPreview {
        let preview = crate::bash_background::persistence::PersistedCompletionPreview {
            output_preview: output_preview.to_string(),
            output_truncated,
            bash_output_list_envelope: bash_output_list_envelope.clone(),
        };
        let Some(task) = self.task(task_id) else {
            return preview;
        };
        let mut db = DeferredDbWrites::new(self, &task);
        let Ok(mut state) = task.lock_state() else {
            return preview;
        };
        if state
            .metadata
            .completion_has_non_ticket_env_secrets
            .is_some()
        {
            if let Some(saved) = &state.metadata.completion_preview {
                return saved.clone();
            }
        }
        state.metadata.completion_preview = Some(preview.clone());
        if let Err(error) = self.persist_task_locked(&task, &state.metadata, &mut db) {
            crate::slog_warn!("failed to persist masked completion preview for {task_id}: {error}");
        }
        preview
    }

    /// Render a watch match over `bytes`, the task's complete capture.
    #[cfg(test)]
    pub(super) fn render_watch_preview(
        &self,
        task_id: &str,
        bytes: &[u8],
        range: std::ops::Range<usize>,
    ) -> (String, String) {
        let Some(source) = watch_preview_source_range(range.clone(), bytes.len()) else {
            return withheld_watch_preview();
        };
        let range = range.start.min(bytes.len()).saturating_sub(source.start)
            ..range.end.min(bytes.len()).saturating_sub(source.start);
        self.render_watch_preview_window(
            task_id,
            &bytes[source.clone()],
            range,
            source.start == 0,
            source.end == bytes.len(),
            None,
        )
    }

    /// Render a watch match from `bytes`, a window of the capture read with
    /// [`watch_preview_source_range`]. `starts_capture` and `ends_capture` say
    /// whether the window reaches the capture's first or last byte.
    /// `output_file` is the capture file the match was found in, named when
    /// the preview has to be withheld after a restart.
    pub(super) fn render_watch_preview_window(
        &self,
        task_id: &str,
        bytes: &[u8],
        range: std::ops::Range<usize>,
        starts_capture: bool,
        ends_capture: bool,
        output_file: Option<&std::path::Path>,
    ) -> (String, String) {
        self.render_watch_preview_window_checked(
            task_id,
            bytes,
            range,
            starts_capture,
            ends_capture,
        )
        .unwrap_or_else(|unavailable| {
            let context = if unavailable.restart {
                restart_watch_notice(output_file)
            } else {
                unavailable.reason.to_string()
            };
            (crate::log_redact::SECRET_PLACEHOLDER.to_string(), context)
        })
    }

    pub(super) fn render_watch_preview_window_checked(
        &self,
        task_id: &str,
        bytes: &[u8],
        range: std::ops::Range<usize>,
        starts_capture: bool,
        ends_capture: bool,
    ) -> Result<(String, String), UnavailablePreview> {
        let text = String::from_utf8_lossy(bytes);
        // A scan window can end inside a character that is intact in the full
        // capture. Decode the expanded source neighborhood rather than separate
        // match prefixes, then include the whole character at either match edge.
        let mut range = lossy_watch_offset(bytes, range.start, false)
            ..lossy_watch_offset(bytes, range.end, true);
        align_watch_range(&text, &mut range);
        let task = self.task(task_id);
        let ticket_digest = task.as_ref().and_then(|task| {
            task.lock_state()
                .ok()
                .and_then(|state| state.metadata.completion_ticket_digest.clone())
        });
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let Some(secrets) = guard.as_deref().and_then(Option::as_ref) else {
            return Err(UnavailablePreview::restart());
        };
        if secrets.longest_value_len() > WATCH_SECRET_HALO_BYTES {
            return Err(UnavailablePreview::context(TRUNCATED_PREVIEW_WITHHELD));
        }
        // A range can fit the byte window while its complete line exceeds the
        // audited masking bound. Report that separately from a detected secret.
        if whole_lines(&text, range.clone(), starts_capture, ends_capture)
            .is_none_or(|line| line.len() > WATCH_PREVIEW_LINE_CAP_BYTES)
        {
            return Err(UnavailablePreview::line_limit());
        }
        let Some(region) = watch_mask_region(&text, &range, secrets, starts_capture, ends_capture)
        else {
            return Err(UnavailablePreview::context(TRUNCATED_PREVIEW_WITHHELD));
        };
        let mut range = range.start - region.start..range.end - region.start;
        let text = &text[region];
        // Exact environment values need their complete spelling before the
        // smaller ticket-only window is selected.
        let masked_values = secrets.redact_range(text, Some(&mut range));
        let text = masked_values.as_ref();
        // Hash the displayed context plus 31 bytes on either side, enough to
        // include any 32-hex ticket crossing a display boundary.
        let context = super::super::watches::context_range(&text, range.start, range.end);
        let start = crate::compress::generic::floor_char_boundary(
            &text,
            context
                .start
                .saturating_sub(crate::log_redact::TICKET_HALO_BYTES),
        );
        let end = crate::compress::generic::ceil_char_boundary(
            &text,
            context
                .end
                .saturating_add(crate::log_redact::TICKET_HALO_BYTES)
                .min(text.len()),
        );
        range = range.start - start..range.end - start;
        let text = &text[start..end];
        // The scanner finds the watch on raw bytes. Only its presentation is
        // masked, before the context window can split a credential.
        let Ok(masked) = crate::log_redact::redact_ticket_digest_range(
            text,
            &mut range,
            ticket_digest.as_deref(),
        ) else {
            return Err(UnavailablePreview::context(TRUNCATED_PREVIEW_WITHHELD));
        };
        align_watch_range(&masked, &mut range);
        let Some(matched) = masked.get(range.clone()) else {
            return Err(UnavailablePreview::context(TRUNCATED_PREVIEW_WITHHELD));
        };
        Ok((
            matched.to_string(),
            super::super::watches::context_snippet(&masked, range.start, range.end),
        ))
    }

    pub(super) fn completion_preview_for_notice(
        &self,
        metadata: &PersistedTask,
        cache: &TerminalOutputCache,
        paths: Option<&TaskPaths>,
    ) -> (String, bool) {
        let task = self.task(&metadata.task_id);
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let empty_secrets = crate::log_redact::SecretValues::default();
        let secrets = guard.as_deref().and_then(Option::as_ref).or_else(|| {
            metadata
                .has_safe_completion_redaction_after_restart()
                .then_some(&empty_secrets)
        });
        let Some(secrets) = secrets else {
            return withheld_preview(RESTART_PREVIEW_WITHHELD, cache);
        };
        if cache.output_preview.is_empty() {
            return (String::new(), cache.output_truncated);
        }
        if cache
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.disk_truncated_prefix_bytes > 0)
        {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        }
        let Some(paths) = task.as_ref().map(|task| &task.paths).or(paths) else {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        };
        let buffer = BgBuffer::registered(paths, BgMode::Pipes);
        let Some((stdout, stderr)) = buffer.read_complete_for_notice() else {
            return withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache);
        };
        let mut raw = combine_streams(&stdout, &stderr);
        normalize_piped_display_output(&mut raw);
        let masked_raw = secrets.redact(&raw);
        let secret_redaction_changed = matches!(&masked_raw, Cow::Owned(_));
        let mut safe_cache = cache.clone();
        if let Some(recovery) = &mut safe_cache.recovery {
            for line in &mut recovery.final_lines {
                let masked_values = secrets.redact(line);
                *line = masked_values.into_owned();
            }
        }
        if secret_redaction_changed {
            let masked = masked_raw.to_string();
            // Re-render only credential-bearing output. Ordinary completions
            // reuse the terminal cache and never invoke the compressor twice.
            let body = if cache.kind == TerminalOutputKind::Compressed {
                // Repeat the terminal render's per-stream ending repair, or a
                // compressor that keeps stderr's last line can drop stdout's
                // verdict. Each stream is masked the same way as the body, so
                // a restored line carries placeholders, never the credential.
                let [stdout, stderr] = [stdout, stderr].map(|mut stream| {
                    normalize_piped_display_output(&mut stream);
                    let masked_values = secrets.redact(&stream);
                    masked_values.into_owned()
                });
                let endings = stream_endings([&stdout, &stderr]);
                let mut text = self
                    .compress_output(&metadata.command, masked, metadata.exit_code)
                    .text;
                let final_lines = restore_stream_endings(&mut text, &endings);
                if let Some(recovery) = &mut safe_cache.recovery {
                    recovery.final_lines = final_lines;
                }
                text
            } else {
                masked
            };
            // Replacing the body also removes terminal-rendering trailers.
            // Restore recovery notes and pipeline warnings before the smaller
            // completion cap, including when the new body fits under that cap.
            safe_cache.output_preview = if let Some(recovery) = &mut safe_cache.recovery {
                let (preview, truncated) = if cache.kind == TerminalOutputKind::Raw {
                    render_raw_body_with_recovery_marker(&body, recovery)
                } else {
                    render_body_with_recovery_marker(&body, recovery)
                };
                safe_cache.output_truncated |= truncated;
                preview
            } else {
                body
            };
            normalize_piped_display_output(&mut safe_cache.output_preview);
            append_pipeline_warning(
                &mut safe_cache,
                metadata,
                task.as_ref().map(|task| &task.paths),
            );
        }
        let masked_values = secrets.redact(&safe_cache.output_preview);
        safe_cache.output_preview = masked_values.into_owned();
        let (preview, truncated) =
            completion_preview_pieces_for_cache(&safe_cache, metadata.exit_code);
        let Some(digest) = metadata.completion_ticket_digest.as_deref() else {
            return (preview.text, truncated);
        };
        let intact_cache = !safe_cache
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.byte_truncated);
        match mask_rendered_ticket_preview(
            &masked_raw,
            &safe_cache.output_preview,
            intact_cache,
            safe_cache
                .recovery
                .as_ref()
                .map(|recovery| recovery.final_lines.as_slice())
                .unwrap_or(&[]),
            &preview,
            digest,
        ) {
            Ok(preview) => (preview, truncated),
            Err(_) => withheld_preview(TRUNCATED_PREVIEW_WITHHELD, cache),
        }
    }

    pub(super) fn mask_completion_text(&self, task_id: &str, text: &mut String) {
        let task = self.task(task_id);
        let ticket_digest = task.as_ref().and_then(|task| {
            task.lock_state()
                .ok()
                .and_then(|state| state.metadata.completion_ticket_digest.clone())
        });
        let guard = task.as_ref().map(|task| {
            task.completion_secrets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let masked = match guard.as_deref().and_then(Option::as_ref) {
            Some(secrets) => secrets.redact(text),
            None => aft_redactor(text),
        };
        let secret_redaction_changed = matches!(&masked, Cow::Owned(_));
        let Ok(masked) = crate::log_redact::redact_ticket_digest(&masked, ticket_digest.as_deref())
        else {
            *text = TRUNCATED_PREVIEW_WITHHELD.to_string();
            if let Some(task) = task.as_ref() {
                for path in [&task.paths.stdout, &task.paths.stderr] {
                    text.push_str("\nOutput file: ");
                    text.push_str(&aft_redactor(&path.to_string_lossy()));
                }
            }
            return;
        };
        let ticket_redaction_changed = matches!(&masked, Cow::Owned(_));
        if secret_redaction_changed || ticket_redaction_changed {
            *text = masked.into_owned();
        }
    }
}

/// The window of the capture to read for rendering a watch match at `range`:
/// enough to find the whole lines of the largest maskable region around it
/// plus a halo for credentials that cross a line break. `None` when the match
/// alone is larger than that region.
pub(super) fn watch_preview_source_range(
    range: std::ops::Range<usize>,
    len: usize,
) -> Option<std::ops::Range<usize>> {
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    if end - start > WATCH_PREVIEW_LINE_CAP_BYTES {
        return None;
    }
    // Any region of at most the cap that contains the match lies between
    // end - cap and start + cap.
    let reach = WATCH_PREVIEW_LINE_CAP_BYTES + WATCH_CROSS_LINE_HALO_BYTES;
    let window = end.saturating_sub(reach)..start.saturating_add(reach).min(len);
    debug_assert!(window.len() <= WATCH_PREVIEW_SOURCE_BYTES);
    Some(window)
}

/// The part of `text` (a capture window) to mask for a watch preview of
/// `matched`. Masking a cut slice can miss a credential whose label or start
/// lies outside it, and the displayed context would then show the rest of the
/// value. So the region covers the displayed context extended to whole lines:
/// every redactor pattern but two stops at a newline (see `log_redact`). The
/// region is widened further for the two that do not, JSON secret fields and
/// private-key blocks, and for exact credential values containing a newline.
/// `None` means the preview must be withheld: the lines holding the match
/// would exceed [`WATCH_PREVIEW_LINE_CAP_BYTES`] or reach past the window, or
/// the window cannot show whether the region starts inside a key block or ends
/// inside a JSON secret value. Context lines that cannot be included this way
/// are left out of the preview instead.
fn watch_mask_region(
    text: &str,
    matched: &std::ops::Range<usize>,
    secrets: &crate::log_redact::SecretValues,
    starts_capture: bool,
    ends_capture: bool,
) -> Option<std::ops::Range<usize>> {
    let shown = super::super::watches::context_range(text, matched.start, matched.end);
    // Prefer the full displayed context, side by side; a context side whose
    // line runs out of the window falls back to the match's own line.
    let start = if whole_lines(text, shown.start..matched.end, starts_capture, true).is_some() {
        shown.start
    } else {
        matched.start
    };
    let end = if whole_lines(text, matched.start..shown.end, true, ends_capture).is_some() {
        shown.end
    } else {
        matched.end
    };
    let spanning = {
        let mut spans = crate::log_redact::json_secret_field_spans(text);
        spans.extend(secrets.cross_line_spans(text));
        spans
    };
    let key_blocks = crate::log_redact::private_key_spans(text);
    [start..end, matched.clone()].into_iter().find_map(|seed| {
        mask_region_from(
            text,
            seed,
            &spanning,
            &key_blocks,
            starts_capture,
            ends_capture,
        )
    })
}

fn mask_region_from(
    text: &str,
    mut region: std::ops::Range<usize>,
    spanning: &[std::ops::Range<usize>],
    key_blocks: &[std::ops::Range<usize>],
    starts_capture: bool,
    ends_capture: bool,
) -> Option<std::ops::Range<usize>> {
    loop {
        region = whole_lines(text, region, starts_capture, ends_capture)?;
        if region.len() > WATCH_PREVIEW_LINE_CAP_BYTES {
            return None;
        }
        let mut widened = region.clone();
        for span in spanning {
            if span.start < widened.start && span.end > widened.start {
                widened.start = span.start;
            }
            if span.start < widened.end && span.end > widened.end {
                widened.end = span.end;
            }
        }
        // A key block that opens inside the region and runs past its end is
        // still masked to the region's end, so only its start matters.
        for span in key_blocks {
            if span.start < widened.start && span.end > widened.start {
                widened.start = span.start;
            }
        }
        if widened == region {
            break;
        }
        region = widened;
    }
    // A JSON secret value still open at the window's end may close beyond it;
    // its visible part would not be recognized.
    if !ends_capture && crate::log_redact::json_secret_value_open_at_end(text, region.end) {
        return None;
    }
    // With no BEGIN or END line before the region in the window, a key block
    // that began before the window could still be open. Its body lines are the
    // secret, so withhold a region showing a body-shaped line before any marker.
    if !starts_capture && !crate::log_redact::has_private_key_marker(&text[..region.start]) {
        let unmarked = &text[region.clone()];
        let unmarked = unmarked
            .find("PRIVATE KEY-----")
            .map_or(unmarked, |marker| &unmarked[..marker]);
        if unmarked
            .lines()
            .any(crate::log_redact::looks_like_private_key_body)
        {
            return None;
        }
    }
    Some(region)
}

/// Extend `range` to the start and end of the lines it touches, including the
/// last line's newline. `None` when a needed newline lies outside the window
/// and the window does not reach that end of the capture.
fn whole_lines(
    text: &str,
    range: std::ops::Range<usize>,
    starts_capture: bool,
    ends_capture: bool,
) -> Option<std::ops::Range<usize>> {
    let start = match text[..range.start].rfind('\n') {
        Some(newline) => newline + 1,
        None if starts_capture => 0,
        None => return None,
    };
    let end = if range.end > range.start && text.as_bytes()[range.end - 1] == b'\n' {
        range.end
    } else {
        match text[range.end..].find('\n') {
            Some(newline) => range.end + newline + 1,
            None if ends_capture => text.len(),
            None => return None,
        }
    };
    Some(start..end.max(start))
}

#[cfg(test)]
fn withheld_watch_preview() -> (String, String) {
    (
        crate::log_redact::SECRET_PLACEHOLDER.to_string(),
        TRUNCATED_PREVIEW_WITHHELD.to_string(),
    )
}

fn mask_rendered_ticket_preview(
    raw: &str,
    cache: &str,
    intact_cache: bool,
    final_lines: &[String],
    preview: &super::super::output::PiecedText,
    digest: &str,
) -> Result<String, crate::log_redact::TicketRedactionBudgetExceeded> {
    use super::super::output::CapPiece;
    use crate::log_redact::{
        TicketRedactionBudgetExceeded, TICKET_HALO_BYTES, TICKET_REDACTION_BYTES,
        TICKET_REDACTION_WINDOWS,
    };
    if preview.text.len() > TICKET_REDACTION_BYTES || cache.len() > TICKET_REDACTION_BYTES {
        return Err(TicketRedactionBudgetExceeded);
    }
    // Locate whole bounded raw head/tail slices in the terminal cache first.
    // A displayed line inside one of them has its raw neighbors at a known
    // offset. A line elsewhere can use its cache neighbors only if no byte cut
    // destroyed them; otherwise withhold rather than guess.
    let width = preview.text.len() + TICKET_HALO_BYTES;
    let head_end = crate::compress::generic::ceil_char_boundary(raw, width.min(raw.len()));
    let tail_start =
        crate::compress::generic::floor_char_boundary(raw, raw.len().saturating_sub(width));
    let sources = [
        (&raw[..head_end], false, head_end < raw.len()),
        (&raw[tail_start..], tail_start > 0, false),
    ];
    let mappings = sources.map(|(source, cut_start, cut_end)| {
        cache
            .find(source)
            .map(|start| (source, start, cut_start, cut_end))
    });
    let mut remaining = TICKET_REDACTION_WINDOWS;
    let mut output = String::new();
    let mut mask = |slice: &str,
                    mut range: std::ops::Range<usize>,
                    output: &mut String|
     -> Result<(), TicketRedactionBudgetExceeded> {
        remaining = remaining
            .checked_sub(crate::log_redact::ticket_digest_windows(slice))
            .ok_or(TicketRedactionBudgetExceeded)?;
        let masked =
            crate::log_redact::redact_ticket_digest_range(slice, &mut range, Some(digest))?;
        output.push_str(&masked[range]);
        Ok(())
    };
    for piece in &preview.pieces {
        let piece_range = match piece {
            CapPiece::Input(range) => range.clone(),
            // Markers and separators the cap wrote have no capture neighbors.
            CapPiece::Inserted(text) => {
                mask(text, 0..text.len(), &mut output)?;
                continue;
            }
        };
        let Some(copied) = cache.get(piece_range.clone()) else {
            return Err(TicketRedactionBudgetExceeded);
        };
        // Each displayed line is checked at the cache position it was copied
        // from, so an identical line elsewhere never decides its masking.
        let mut cache_start = piece_range.start;
        for line in copied.split_inclusive('\n') {
            let line_start = cache_start;
            cache_start += line.len();
            let newline = line.ends_with('\n');
            let line = line.strip_suffix('\n').unwrap_or(line);
            if !line.is_empty() {
                let edge_hex = line.as_bytes()[0].is_ascii_hexdigit()
                    || line.as_bytes()[line.len() - 1].is_ascii_hexdigit();
                let origin = mappings.iter().flatten().find_map(
                    |(source, mapped_start, cut_start, cut_end)| {
                        let start = line_start.checked_sub(*mapped_start)?;
                        let end = start + line.len();
                        if end > source.len()
                            || (*cut_start && start < TICKET_HALO_BYTES)
                            || (*cut_end && end + TICKET_HALO_BYTES > source.len())
                        {
                            return None;
                        }
                        Some((*source, start))
                    },
                );
                // Compression can remove lines before the stream ending, leaving
                // no complete tail slice in the cache. If a displayed line matches
                // a saved final line and a complete line at the raw output's end,
                // its displayed bytes can still be located in the raw tail.
                let ending_origin = final_lines
                    .iter()
                    .any(|final_line| final_line == line)
                    .then(|| {
                        let tail = &raw[tail_start..];
                        let prefix = tail.trim_end_matches('\n').strip_suffix(line)?;
                        (prefix.ends_with('\n') || (tail_start == 0 && prefix.is_empty()))
                            .then_some((tail, prefix.len()))
                    })
                    .flatten();
                let (source, start) = match origin.or(ending_origin) {
                    Some(origin) => origin,
                    None if intact_cache || !edge_hex => (cache, line_start),
                    None => return Err(TicketRedactionBudgetExceeded),
                };
                let from = crate::compress::generic::floor_char_boundary(
                    source,
                    start.saturating_sub(TICKET_HALO_BYTES),
                );
                let to = crate::compress::generic::ceil_char_boundary(
                    source,
                    (start + line.len() + TICKET_HALO_BYTES).min(source.len()),
                );
                mask(
                    &source[from..to],
                    start - from..start - from + line.len(),
                    &mut output,
                )?;
            }
            if newline {
                output.push('\n');
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod ticket_preview_tests {
    use super::*;

    #[test]
    fn completion_ticket_fragment_requires_cache_source_mapping() {
        let ticket = "d34db33f89ab01cd7654ef2098ac1357";
        let fragment = &ticket[16..];
        let raw = format!("{fragment}\n{}{ticket}\n", "x".repeat(10_000));
        let cache = format!("unrelated compressed head\n[cut]\n{fragment}\n");
        let digest = blake3::hash(ticket.as_bytes()).to_hex().to_string();
        let at = cache.find(fragment).unwrap();
        let preview = crate::bash_background::output::PiecedText {
            text: fragment.to_string(),
            pieces: vec![crate::bash_background::output::CapPiece::Input(
                at..at + fragment.len(),
            )],
        };
        // A matching fragment at the start of raw output is not evidence for
        // the bytes neighboring the displayed, already-truncated occurrence.
        assert!(mask_rendered_ticket_preview(&raw, &cache, false, &[], &preview, &digest).is_err());
    }

    #[test]
    fn completion_ticket_fragment_is_checked_where_it_was_copied_from() {
        use crate::bash_background::output::{CapPiece, PiecedText};
        let ticket = "d34db33f89ab01cd7654ef2098ac1357";
        let fragment = &ticket[16..];
        // The same 16 hex digits stand alone on the first line and end the
        // ticket on the second.
        let cache = format!("{fragment}\n{}{fragment}\n", &ticket[..16]);
        let digest = blake3::hash(ticket.as_bytes()).to_hex().to_string();
        let copied_from = |at: usize| PiecedText {
            text: fragment.to_string(),
            pieces: vec![CapPiece::Input(at..at + fragment.len())],
        };
        let in_ticket = cache.rfind(fragment).unwrap();
        assert_eq!(
            mask_rendered_ticket_preview(
                &cache,
                &cache,
                true,
                &[],
                &copied_from(in_ticket),
                &digest
            )
            .unwrap(),
            crate::log_redact::SECRET_PLACEHOLDER
        );
        assert_eq!(
            mask_rendered_ticket_preview(&cache, &cache, true, &[], &copied_from(0), &digest)
                .unwrap(),
            fragment
        );
    }
}

#[cfg(test)]
mod keep_pattern_tests {
    use super::keep_pattern_visible;
    use crate::log_redact::SECRET_PLACEHOLDER as HIDDEN;

    #[test]
    fn a_pattern_masked_as_part_of_a_credential_stays_visible() {
        let token = format!("ghp_{}", "A".repeat(36));
        let raw = format!("token {token} done");
        let masked = format!("token {HIDDEN} done");
        // The rest of the credential stays hidden behind a placeholder.
        assert_eq!(
            keep_pattern_visible(&raw, &masked, "ghp_"),
            format!("token ghp_{HIDDEN} done")
        );
        // A pattern that is the whole credential is shown as given.
        assert_eq!(keep_pattern_visible(&raw, &masked, &token), raw);
        // Only the hidden part of a pattern that starts in visible text is
        // restored, so the visible part is not repeated.
        assert_eq!(
            keep_pattern_visible(&raw, &masked, "n ghp_AA"),
            format!("token ghp_AA{HIDDEN} done")
        );
        // A pattern in the middle of a credential keeps both sides hidden.
        assert_eq!(
            keep_pattern_visible(&raw, &masked, "AAAA"),
            format!("token {HIDDEN}AAAA{HIDDEN} done")
        );
    }

    #[test]
    fn every_hidden_occurrence_and_trailing_credential_is_handled() {
        let raw = "a=SECRETVALUE b=SECRETVALUE";
        let masked = format!("a={HIDDEN} b={HIDDEN}");
        assert_eq!(keep_pattern_visible(raw, &masked, "SECRETVALUE"), raw);
        assert_eq!(
            keep_pattern_visible(raw, &masked, "SECRET"),
            format!("a=SECRET{HIDDEN} b=SECRET{HIDDEN}")
        );
    }

    #[test]
    fn visible_patterns_and_unalignable_text_are_left_as_masked() {
        let raw = "READY token ghp_x";
        let masked = format!("READY token {HIDDEN}");
        assert_eq!(keep_pattern_visible(raw, &masked, "READY"), masked);
        // Masked text that does not line up with the raw line is never
        // patched with guessed raw bytes.
        let unrelated = format!("other {HIDDEN}");
        assert_eq!(keep_pattern_visible(raw, &unrelated, "ghp_"), unrelated);
    }
}

fn withheld_preview(reason: &str, cache: &TerminalOutputCache) -> (String, bool) {
    let mut text = reason.to_string();
    for path in [&cache.output_path, &cache.stderr_path]
        .into_iter()
        .flatten()
    {
        text.push_str("\nOutput file: ");
        text.push_str(&aft_redactor(path));
    }
    (text, true)
}

fn align_watch_range(text: &str, range: &mut std::ops::Range<usize>) {
    range.start = text.floor_char_boundary(range.start.min(text.len()));
    range.end = text.ceil_char_boundary(range.end.min(text.len()).max(range.start));
}

fn lossy_watch_offset(bytes: &[u8], offset: usize, round_up: bool) -> usize {
    let mut source_start = 0;
    let mut text_start = 0;
    // utf8_chunks groups invalid bytes exactly as from_utf8_lossy does, so an
    // invalid sequence contributes one replacement character in both mappings.
    for chunk in bytes.utf8_chunks() {
        let valid_end = source_start + chunk.valid().len();
        if offset <= valid_end {
            return text_start + offset.saturating_sub(source_start);
        }
        text_start += chunk.valid().len();
        source_start = valid_end + chunk.invalid().len();
        if offset < source_start {
            return text_start + if round_up { '\u{FFFD}'.len_utf8() } else { 0 };
        }
        if !chunk.invalid().is_empty() {
            text_start += '\u{FFFD}'.len_utf8();
        }
    }
    text_start
}
