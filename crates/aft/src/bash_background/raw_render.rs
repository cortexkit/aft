//! Terminal output for a piped task whose reply is not compressed: the two
//! captures, capped at the raw reply limit, with a recovery marker when the
//! cap or disk truncation cut them.

use super::*;

pub(super) fn render_raw_passthrough(
    buffer: &BgBuffer,
    disk_truncation: DiskTruncation,
    artifact_access: ArtifactRecoveryAccess,
) -> TerminalOutputCache {
    let raw = buffer.read_combined_head_tail(
        RAW_PASSTHROUGH_CAP_BYTES,
        RAW_PASSTHROUGH_HEAD_BYTES,
        RAW_PASSTHROUGH_TAIL_BYTES,
    );
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
