//! Terminal output for a piped task whose reply is not compressed: structured
//! `gh` JSON output, or the two captures capped at the raw reply limit with a
//! recovery marker when the cap or disk truncation cut them. A capture that
//! could not be read is named after the output.

use super::*;
use crate::bash_background::buffer::BoundedRead;

pub(super) fn render_raw_passthrough(
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> TerminalOutputCache {
    let mut raw = buffer.read_combined_head_tail(
        RAW_PASSTHROUGH_CAP_BYTES,
        RAW_PASSTHROUGH_HEAD_BYTES,
        RAW_PASSTHROUGH_TAIL_BYTES,
    );
    let gaps = std::mem::take(&mut raw.read_gaps);
    let mut cache = render_raw_read(raw, buffer, disk_truncation, artifact_access);
    disclose_read_gaps(&mut cache, &gaps);
    cache
}

/// Append one line per capture that could not be read, after the output, and
/// mark the reply incomplete. A legitimately empty capture has no gap line.
pub(super) fn disclose_read_gaps(cache: &mut TerminalOutputCache, gaps: &[String]) {
    for gap in gaps {
        if !cache.output_preview.is_empty() && !cache.output_preview.ends_with('\n') {
            cache.output_preview.push('\n');
        }
        cache.output_preview.push_str(gap);
        cache.output_truncated = true;
    }
}

fn render_raw_read(
    raw: BoundedRead,
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> TerminalOutputCache {
    let output_path = buffer.output_path().map(|path| path.display().to_string());
    let stderr_path = buffer.stderr_path().map(|path| path.display().to_string());
    if !raw.truncated && disk_truncation.total_prefix_bytes() == 0 {
        return TerminalOutputCache {
            output_preview: raw.text,
            output_truncated: false,
            compression_input_line_count: None,
            kind: TerminalOutputKind::Raw,
            output_path,
            stderr_path,
            artifact_access,
            recovery: None,
            read_only_note: None,
        };
    }

    let include_stderr_path = buffer.stream_len(StreamKind::Stderr) > 0;
    let mut recovery = RecoveryContext {
        final_lines: Vec::new(),
        dropped_by_class: BTreeMap::new(),
        had_inner_drop: false,
        offset_hint_eligible: false,
        offset_start_line: None,
        byte_truncated: raw.truncated,
        disk_truncated_prefix_bytes: disk_truncation.total_prefix_bytes(),
        output_path: output_path.clone(),
        stderr_path: stderr_path.clone(),
        include_stderr_path,
        artifact_access: artifact_access.clone(),
        failed_exit: false,
    };
    let (output_preview, output_truncated) =
        render_raw_body_with_recovery_marker(&raw.text, &mut recovery);
    TerminalOutputCache {
        output_preview,
        output_truncated,
        compression_input_line_count: None,
        kind: TerminalOutputKind::Raw,
        output_path,
        stderr_path,
        artifact_access,
        recovery: Some(recovery),
        read_only_note: None,
    }
}

pub(super) fn render_structured_output(
    command: &str,
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> Option<TerminalOutputCache> {
    if !is_gh_structured_command(command) {
        return None;
    }

    let output_path = buffer
        .output_path()
        .map(|path| path.display().to_string())?;
    let stdout_bytes = buffer.stream_len(StreamKind::Stdout);
    if stdout_bytes == 0 {
        return None;
    }

    if stdout_bytes > STRUCTURED_OUTPUT_CAP_BYTES as u64 {
        if !stream_starts_like_json(buffer, StreamKind::Stdout) {
            return None;
        }
        let output_preview = structured_output_pointer(
            stdout_bytes,
            &output_path,
            disk_truncation.total_prefix_bytes(),
            &artifact_access,
        );
        return Some(TerminalOutputCache {
            output_preview,
            output_truncated: true,
            compression_input_line_count: None,
            kind: TerminalOutputKind::Structured,
            output_path: Some(output_path),
            stderr_path: buffer.stderr_path().map(|path| path.display().to_string()),
            artifact_access,
            recovery: None,
            read_only_note: None,
        });
    }

    let stdout = buffer.read_stream_bounded(StreamKind::Stdout, STRUCTURED_OUTPUT_CAP_BYTES);
    if stdout.truncated || !is_structured_body(&stdout.text) {
        return None;
    }

    Some(TerminalOutputCache {
        output_preview: stdout.text,
        output_truncated: false,
        compression_input_line_count: None,
        kind: TerminalOutputKind::Structured,
        output_path: Some(output_path),
        stderr_path: buffer.stderr_path().map(|path| path.display().to_string()),
        artifact_access,
        recovery: None,
        read_only_note: None,
    })
}
