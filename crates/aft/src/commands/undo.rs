use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use std::path::Path;

/// Handle the `undo` command: restore the latest operation, or one file when requested.
///
/// Params: `file` (string, optional) — path to a single file to undo.
/// Returns: `{ path, backup_id }` on success, or `no_undo_history` error.
pub fn handle_undo(req: &RawRequest, ctx: &AppContext) -> Response {
    let mut backup = ctx.backup().borrow_mut();

    let Some(file) = req.params.get("file").and_then(|v| v.as_str()) else {
        return match backup.restore_last_operation(req.session()) {
            Ok(operation) => Response::success(
                &req.id,
                serde_json::json!({
                    "operation": true,
                    "op_id": operation.op_id,
                    "restored_count": operation.restored.len(),
                    "restored": operation.restored.into_iter().map(|file| {
                        serde_json::json!({
                            "path": file.path.display().to_string(),
                            "backup_id": file.backup_id,
                        })
                    }).collect::<Vec<_>>(),
                    "warnings": operation.warnings,
                }),
            ),
            Err(e) => Response::error(&req.id, e.code(), e.to_string()),
        };
    };

    let resolved = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    match backup.restore_latest(req.session(), &resolved) {
        Ok((entry, warning)) => {
            let mut result = serde_json::json!({
                "path": file,
                "backup_id": entry.backup_id,
            });
            if let Some(w) = warning {
                result["warning"] = serde_json::Value::String(w);
            }
            Response::success(&req.id, result)
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}
