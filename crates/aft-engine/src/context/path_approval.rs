use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use super::{path_error_response, AppContext, WorkerSearchTool};
use crate::access_policy::{self, AccessGrant, AccessOperation, AskRoute, GrantKind};
use crate::protocol::{Response, ERROR_PERMISSION_REQUIRED};

pub(super) struct ReadAsk<'a> {
    pub path: &'a Path,
    pub search_tool: Option<WorkerSearchTool>,
}

#[derive(PartialEq, Eq)]
enum AskTarget {
    Missing,
    File,
    Directory,
}

fn inspect_target(identity: &Path, read_path: Option<&Path>) -> io::Result<AskTarget> {
    if let Some(path) = read_path {
        // Probe the path the tool would open, not the policy identity, which
        // may already have followed a symlink or normalized away components.
        // Rejoin the last component so a trailing slash cannot force the OS
        // to follow the final symlink.
        let entry = path
            .parent()
            .zip(path.file_name())
            .map(|(parent, name)| parent.join(name));
        match std::fs::symlink_metadata(entry.as_deref().unwrap_or(path)) {
            Ok(entry) if identity == path && !entry.is_symlink() => {
                return Ok(if entry.is_dir() {
                    AskTarget::Directory
                } else {
                    AskTarget::File
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(AskTarget::Missing),
            Err(_) => return Ok(AskTarget::File),
        }
    }
    // A symlink is present even if its target is missing. The approval still
    // covers the resolved directory or exact file, as for any existing path.
    match std::fs::metadata(identity) {
        Ok(metadata) if metadata.is_dir() => Ok(AskTarget::Directory),
        Ok(_) => Ok(AskTarget::File),
        Err(error) if read_path.is_some() || error.kind() == ErrorKind::NotFound => {
            Ok(AskTarget::File)
        }
        Err(error) => Err(error),
    }
}

pub fn missing_read_response(
    req_id: &str,
    path: &Path,
    search_tool: Option<WorkerSearchTool>,
) -> Response {
    let prefix = match search_tool {
        Some(WorkerSearchTool::Grep) => "grep: search ",
        Some(WorkerSearchTool::Glob) => "glob: search ",
        Some(WorkerSearchTool::AstGrepSearch) => "ast_search: search ",
        Some(WorkerSearchTool::AftSearch) | None => "",
    };
    Response::error(
        req_id,
        "path_not_found",
        format!("{prefix}path does not exist: {}", path.display()),
    )
}

impl AppContext {
    /// Handle a policy `Ask` decision. Legacy CLI/library calls can proceed,
    /// but a host must either supply approval for the exact grant or receive
    /// a response carrying the path, operation and grant kind to request.
    /// Routes that forbid asking still refuse before any existence check.
    pub(super) fn answer_ask(
        &self,
        req_id: &str,
        original: &Path,
        identity: &Path,
        operation: AccessOperation,
        read: Option<ReadAsk<'_>>,
    ) -> Result<(), Response> {
        #[cfg(any(test, all(unix, feature = "test-support")))]
        super::confinement::note_ask();
        let route = access_policy::current_ask_route();
        // Codemode programs and fork calls cannot put a question to anyone.
        if let Some(refusal) =
            access_policy::unasked_refusal(&route, req_id, original, identity, operation)
        {
            return Err(refusal);
        }
        match &route {
            AskRoute::Lowered => return Ok(()),
            AskRoute::Deny => {
                let root = self.config().project_root.clone().unwrap_or_default();
                return Err(path_error_response(req_id, original, &root));
            }
            _ => {}
        }
        // A directory is asked for as a subtree so one answer covers what the
        // tool reads beneath it. Keep the filesystem probe on the bounded helper.
        let read_path = read.as_ref().map(|read| read.path);
        let target = if crate::bounded_io::current().is_some() {
            let read_path = read_path.map(Path::to_path_buf);
            crate::bounded_io::run(identity, None, move |path| {
                inspect_target(&path, read_path.as_deref())
            })
            .map_err(|error| {
                Response::error(
                    req_id,
                    crate::bounded_io::timeout_code(&error),
                    error.to_string(),
                )
            })?
        } else {
            inspect_target(identity, read_path).unwrap_or(AskTarget::File)
        };
        let is_dir = target == AskTarget::Directory;
        let kind = if is_dir {
            GrantKind::Subtree
        } else {
            GrantKind::File
        };
        let grant = AccessGrant {
            path: identity.to_path_buf(),
            kind,
            operation,
        };
        if let AskRoute::Consent { once, .. } | AskRoute::Elicitation { once } = &route {
            if once
                .iter()
                .any(|approved| approved.covers_kind(operation, identity, kind))
            {
                return Ok(());
            }
        }
        // Prefer a checkout correction for missing paths when one is available.
        if read.is_none() || target == AskTarget::Missing {
            if let Some(suggestion) = self.checkout_path_suggestion(original, operation) {
                return Err(Response::error(
                    req_id,
                    "path_not_found_did_you_mean",
                    format!(
                        "path not found: {}; did you mean {}?",
                        original.display(),
                        crate::path_identity::normalize_path(&suggestion).display()
                    ),
                ));
            }
        }
        if let Some(read) = read.filter(|_| target == AskTarget::Missing) {
            // Refuse here instead of admitting the read: a file could appear
            // after the probe but before the tool reaches its own stat or open.
            return Err(missing_read_response(req_id, read.path, read.search_tool));
        }

        if let AskRoute::Plugin { scope, once } = route {
            // A file approval must not become a tree approval if the target
            // changed into a directory while the host prompt was open.
            if once
                .iter()
                .any(|approved| approved.covers_kind(operation, identity, kind))
            {
                return Ok(());
            }
            let id = access_policy::plugin::issue(&scope, &grant);
            // OpenCode's `*` includes every descendant. Use the same folder
            // eligibility as consent cards so saving a file approval cannot
            // silently approve home, a volume root, or a system tree.
            let saved_scope = access_policy::directory_scope(&grant);
            let pattern = match saved_scope.kind {
                GrantKind::File => saved_scope.path,
                GrantKind::Subtree => saved_scope.path.join("*"),
            };
            let mut ask = serde_json::json!({
                "kind": "external_directory", "patterns": [pattern], "always": [pattern],
                "grant_id": id, "path": identity, "operation": operation,
                "parentDir": if is_dir { identity } else { identity.parent().unwrap_or(identity) },
            });
            // Expiry and route replacement invalidate the old approval, not
            // the request. The host can ask again for the new route's token.
            if let Some(reason) = scope.retry_refusal_reason(&grant, !once.is_empty()) {
                scope.log_retry_refusal(operation, identity, reason);
                if matches!(reason, "approval_expired" | "approval_route_replaced") {
                    ask["reason"] = serde_json::json!(reason);
                }
            }
            return Err(Response::error_with_data(
                req_id,
                ERROR_PERMISSION_REQUIRED,
                format!(
                    "path '{}' needs outside-access permission",
                    original.display()
                ),
                serde_json::json!({"asks": [ask]}),
            ));
        }
        Err(Response::error_with_data(
            req_id,
            access_policy::ACCESS_ASK_REQUIRED,
            format!(
                "path '{}' is outside this worker's worktree and needs permission",
                original.display()
            ),
            serde_json::json!({"ask": grant}),
        ))
    }
}
