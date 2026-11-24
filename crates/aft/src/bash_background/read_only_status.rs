//! The note a failed remote job gets when the build server refused a write to
//! a tracked file. The note comes from scanning the job's stderr capture, so
//! the scan runs once, when terminal output is rendered, and reads at most
//! [`READ_ONLY_NOTE_SCAN_BYTES`] of the capture.

use std::io::{BufRead, Read};

use super::*;

/// Most stderr bytes one scan for the note reads.
const READ_ONLY_NOTE_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// The read-only note for a terminal task, or `None` when the job was not a
/// failed read-only remote job or its stderr shows no refused checkout write.
pub(super) fn source_read_only_note_for(
    metadata: &PersistedTask,
    buffer: &BgBuffer,
) -> Option<String> {
    let source_read_only = metadata
        .remote
        .as_ref()
        .and_then(|remote| remote.terminal.as_ref())
        .and_then(|terminal| terminal.source_read_only())
        == Some(true);
    let failed = metadata.exit_code.is_some_and(|code| code != 0)
        || matches!(
            metadata.status,
            BgTaskStatus::Killed | BgTaskStatus::TimedOut
        );
    if !source_read_only || !failed {
        return None;
    }
    let (Some(root), Some(reader)) = (metadata.project_root.as_deref(), buffer.stderr_reader())
    else {
        return None;
    };
    crate::exec_remote::read_only_note::source_read_only_note(
        source_read_only,
        failed,
        reader
            .take(READ_ONLY_NOTE_SCAN_BYTES)
            .lines()
            .map_while(Result::ok),
        &metadata.workdir,
        root,
    )
}

/// Append `note` on its own line.
pub(super) fn push_read_only_note(output: &mut String, note: Option<&str>) {
    let Some(note) = note else {
        return;
    };
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(note);
}

#[cfg(test)]
pub(super) fn append_source_read_only_note(
    output: &mut String,
    metadata: &PersistedTask,
    buffer: &BgBuffer,
) {
    push_read_only_note(
        output,
        source_read_only_note_for(metadata, buffer).as_deref(),
    );
}
