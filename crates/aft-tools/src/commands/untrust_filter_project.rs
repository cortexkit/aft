use std::path::PathBuf;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub fn handle_untrust_filter_project(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = req.params.get("params").unwrap_or(&req.params);
    let project_root = match params.get("project_root").and_then(|value| value.as_str()) {
        Some(project_root) => PathBuf::from(project_root),
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "untrust_filter_project: missing required param 'project_root'",
            );
        }
    };
    if !project_root.exists() {
        return Response::error(
            &req.id,
            "path_not_found",
            format!("project_root not found: {}", project_root.display()),
        );
    }
    let storage_dir = match ctx.config().storage_dir.clone() {
        Some(storage_dir) => storage_dir,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "untrust_filter_project: storage_dir is not configured",
            );
        }
    };

    match crate::compress::trust::untrust_project(&storage_dir, &project_root) {
        Ok(()) => {
            ctx.reset_filter_registry();
            Response::success(&req.id, serde_json::json!({ "trusted": false }))
        }
        Err(error) => Response::error(&req.id, "untrust_failed", error),
    }
}
