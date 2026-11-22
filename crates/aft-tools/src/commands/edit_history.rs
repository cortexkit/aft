use std::path::Path;

use crate::backup::{BackupEntry, BackupEntryKind, PathFingerprint};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Number each entry (oldest first) by which file it belongs to. A new file
/// begins where AFT saw the path go from empty to occupied: an AFT mutation
/// that created it, or a snapshot finding it back after AFT had removed it.
/// Content changes made outside AFT (editor saves, `mv` over the file) are
/// marked on the entry but do not start a new file, because by content alone
/// they cannot be told apart from an edit to the same file.
fn file_generations(history: &[BackupEntry]) -> Vec<u64> {
    let mut generation = 0;
    let mut previous: Option<&BackupEntry> = None;
    history
        .iter()
        .map(|entry| {
            if let Some(previous) = previous {
                let recreated_outside_aft = entry.external_change_before
                    && previous.post_state == Some(PathFingerprint::Absent);
                if entry.kind == BackupEntryKind::Tombstone || recreated_outside_aft {
                    generation += 1;
                }
            }
            previous = Some(entry);
            generation
        })
        .collect()
}

/// Lifecycle events AFT observed for one entry.
fn entry_markers(entry: &BackupEntry) -> Vec<&'static str> {
    let mut markers = Vec::new();
    if entry.external_change_before {
        markers.push("changed_outside_aft_before");
    }
    if entry.kind == BackupEntryKind::Tombstone {
        markers.push("created");
    } else if entry.post_state == Some(PathFingerprint::Absent) {
        markers.push("deleted");
    }
    if entry.external_change_checkpoint.is_some() {
        markers.push("external_change_checkpoint");
    }
    markers
}

/// Handle the `edit_history` command: return the backup stack for a file.
///
/// Params: `file` (string, required) — path to query history for.
/// Returns: `{ file, entries: [{ backup_id, timestamp, description, markers, external_change_checkpoint, generation, previous_file }, ...] }`
/// (most recent first). `previous_file` is true for entries that belong to an
/// earlier file AFT saw at this path before the current one was created.
/// `external_change_checkpoint` names the checkpoint an undo saved when it
/// stopped at this entry and found the file changed outside AFT.
pub fn handle_edit_history(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "edit_history: missing required param 'file'",
            );
        }
    };

    // Resolve relative paths against the bound project root so the backup key
    // matches the path the mutating tool recorded. A relative path passed
    // straight to `canonicalize_key` would be joined against the daemon's cwd
    // and miss the stack.
    let resolved = ctx.resolve_relative_path(Path::new(file));

    let backup = ctx.backup().lock();
    let history = backup.history(req.session(), &resolved);
    let generations = file_generations(&history);
    let current_generation = generations.last().copied().unwrap_or(0);

    let entries: Vec<serde_json::Value> = history
        .iter()
        .zip(&generations)
        .rev() // Most recent first for the response
        .map(|(entry, &generation)| {
            serde_json::json!({
                "backup_id": entry.backup_id,
                "timestamp": entry.timestamp,
                "description": entry.description,
                "markers": entry_markers(entry),
                "external_change_checkpoint": entry.external_change_checkpoint,
                "generation": generation,
                "previous_file": generation < current_generation,
            })
        })
        .collect();

    Response::success(
        &req.id,
        serde_json::json!({
            "file": file,
            "entries": entries,
        }),
    )
}

#[cfg(test)]
#[path = "edit_history_backup_tests.rs"]
mod backup_tests;
