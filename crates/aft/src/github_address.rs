//! GitHub resource spellings must not be interpreted as filesystem paths.

use crate::protocol::Response;

pub(crate) fn is_github_resource_path(path: &str) -> bool {
    path.starts_with("issue://") || path.starts_with("pr://")
}

/// Where and how to turn GitHub comment writes on; shared by every refusal so
/// the agent always learns the exact setting.
pub(crate) const GITHUB_WRITE_ENABLE_HINT: &str = "set \"github\": { \"write\": true } in the user config (~/.config/cortexkit/aft.jsonc) and restart the host";

/// Error code for a GitHub address handed to a tool that only works on files.
pub(crate) const GITHUB_RESOURCE_NOT_FILE: &str = "github_resource_not_file";

/// Refusal text for a GitHub address (`issue://…`, `pr://…`) given to a tool
/// that can only touch local files. Such a spelling must never be resolved
/// into a project path: joined onto the root it becomes `<root>/issue:/N` and
/// a file or directory would be created there instead of a comment.
pub(crate) fn github_resource_not_file_message(operation: &str, path: &str) -> String {
    format!(
        "{operation}: '{path}' is a GitHub issue or pull-request address, not a local file, so nothing was changed. \
         Only `write` (post a new comment) and `edit` with one edits[] find/replace entry on issue://N/comments/K \
         (change a comment) accept GitHub addresses, and only while github.write is enabled: {GITHUB_WRITE_ENABLE_HINT}."
    )
}

/// Returns an error response when `path` is a GitHub address. Mutating
/// handlers call this before resolving a path so the address can never become
/// a file on disk.
pub(crate) fn reject_github_resource_path(
    req_id: &str,
    operation: &str,
    path: &str,
) -> Result<(), Response> {
    if is_github_resource_path(path) {
        return Err(Response::error(
            req_id,
            GITHUB_RESOURCE_NOT_FILE,
            github_resource_not_file_message(operation, path),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_prefixes_keep_the_same_filesystem_refusal() {
        for address in ["issue://7", "pr://owner/repo/8/comments/2", "issue://"] {
            let refusal = reject_github_resource_path("request", "file operation", address)
                .expect_err("GitHub prefixes are not local files");
            assert!(!refusal.success);
            assert_eq!(refusal.data["code"], "github_resource_not_file");
        }
        for path in ["issue:/7", "pr/8", "ISSUE://7", "src/pr.rs"] {
            assert!(reject_github_resource_path("request", "file operation", path).is_ok());
        }
    }
}
