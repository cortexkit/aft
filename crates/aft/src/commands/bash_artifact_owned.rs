//! `bash_artifact_owned`: query the session's read-only task-content capability.
//!
//! Plugins use this answer before raising an external-directory permission
//! prompt. Eligible paths are exact registered bash output files, files and
//! directories inside scratch roots recorded on the session's existing tasks,
//! and missing paths under such roots, including roots cleanup has already
//! removed after their command exited (the read tool answers those as
//! not-found). Scratch ownership comes from the registry's spawn metadata,
//! never a path's shape. Canonical targets outside the recorded root are not owned.
//!
//! This command grants no access. The read tools still enforce policy floors,
//! reject live SQLite content, and check opened scratch handles for escapes.

use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Deserialize)]
struct BashArtifactOwnedParams {
    path: String,
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashArtifactOwnedParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_artifact_owned: invalid params: {e}"),
            );
        }
    };

    let requested = Path::new(&params.path);
    // Relative paths are interpreted from the project root, the same base the
    // read tool uses. Without a project root there is nothing to anchor them
    // to, so they are never treated as owned.
    let path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        match ctx.config().project_root.clone() {
            Some(root) => root.join(requested),
            None => return Response::success(&req.id, json!({ "owned": false })),
        }
    };

    let owned = ctx
        .bash_background()
        .is_session_owned_artifact_path(req.session(), &path)
        || ctx
            .bash_background()
            .is_session_owned_scratch_path(req.session(), &path)
        // A missing path under the session's own scratch has no content to
        // protect: the read tool answers not-found, so the host need not ask.
        || ctx.bash_background().is_session_missing_scratch_path(
            req.session(),
            &crate::policy_path::policy_path(&path),
        );
    Response::success(&req.id, json!({ "owned": owned }))
}
