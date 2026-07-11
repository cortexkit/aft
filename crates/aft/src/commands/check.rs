//! Handler for the `check` command: on-demand type checker validation.

use std::path::Path;

use crate::context::AppContext;
use crate::format;
use crate::protocol::{RawRequest, Response};

/// Handle a `check` request.
///
/// Params:
///   - `file` (string, required) - absolute path to the file to check
///
/// Returns: `{ file, error_count, errors?, skipped_reason? }`
pub fn handle_check(req: &RawRequest, ctx: &AppContext) -> Response {
    let config = ctx.config();

    if !config.check.enabled {
        return Response::error(
            &req.id,
            "invalid_request",
            "check: tool is disabled (set check.enabled=true in config)",
        );
    }

    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "check: missing required param 'file'",
            );
        }
    };

    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    if !path.is_file() {
        return Response::error(
            &req.id,
            "invalid_request",
            format!("check: path must be a file: {}", path.display()),
        );
    }

    let (errors, skip_reason) = format::validate_full(&path, &config);

    let mut result = serde_json::json!({
        "file": file,
        "error_count": errors.len(),
    });

    if !errors.is_empty() {
        result["errors"] = serde_json::json!(errors);
    }
    if let Some(reason) = skip_reason {
        result["skipped_reason"] = serde_json::json!(reason);
    }

    Response::success(&req.id, result)
}
