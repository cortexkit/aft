//! `bash_artifact_owned`: ask whether a path is one of the requesting
//! session's own background-bash output files.
//!
//! Plugins call this before raising an external-directory permission prompt
//! for a read. Background task output (stdout, stderr, exit and pty files)
//! lives under the AFT storage root, outside every project, and each task gets
//! a freshly named directory, so a host "always allow" rule can never cover
//! the next task. Truncated output points the agent at these files, which made
//! every follow-up read stall an unattended session on a prompt.
//!
//! The answer comes from
//! [`crate::bash_background::BgTaskRegistry::is_session_owned_artifact_path`]:
//! the canonical path must equal a registered artifact of a task owned by the
//! requesting session. Anything else — another session's artifact, an
//! unregistered file beside an artifact, a symlink or `..` path resolving
//! elsewhere — answers `owned: false`, and the plugin prompts as usual. The
//! command only answers a question; it grants nothing and reads no file.

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
        .is_session_owned_artifact_path(req.session(), &path);
    Response::success(&req.id, json!({ "owned": owned }))
}
