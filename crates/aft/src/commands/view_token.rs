//! Internal provisioning request, deliberately not an agent tool. Tokens are
//! advisory and are never a substitute for a driver's checkout verification.

use crate::{
    context::AppContext,
    protocol::{RawRequest, Response},
};

pub fn handle_view_token(request: &RawRequest, context: &AppContext) -> Response {
    if !context.config().views.enabled {
        return Response::error(
            &request.id,
            "VIEW_TOKEN",
            "view tokens require views to be enabled",
        );
    }
    let Some(root) = request.params.get("root").and_then(|v| v.as_str()) else {
        return Response::error(&request.id, "VIEW_TOKEN", "root is required");
    };
    let Some(tree) = request.params.get("expected_tree").and_then(|v| v.as_str()) else {
        return Response::error(&request.id, "VIEW_TOKEN", "expected_tree is required");
    };
    let Some(project) = context.canonical_cache_root_opt() else {
        return Response::error(
            &request.id,
            "VIEW_TOKEN",
            "view token requires a configured repository",
        );
    };
    let storage = context.storage_paths().artifacts;
    let family = match crate::search_index::artifact_cache_key_read_only(&project, &storage) {
        Ok(family) => family,
        Err(error) => return Response::error(&request.id, "VIEW_TOKEN", &error.to_string()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    match crate::views::registry::mint_view_token(
        &storage,
        &family,
        std::path::Path::new(root),
        tree,
        now,
    ) {
        Ok(token) => Response::success(
            &request.id,
            serde_json::json!({ "token": token.token, "expires_ms": token.expires_ms, "advisory": true }),
        ),
        Err(error) => Response::error(&request.id, "VIEW_TOKEN", &error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn view_token_request_with_views_off_creates_no_v3_files() {
        let storage = tempfile::tempdir().unwrap();
        let context = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                storage_dir: Some(storage.path().to_path_buf()),
                ..Default::default()
            },
        );
        let request = serde_json::from_value(serde_json::json!({ "id": "token", "command": "view_token", "root": "/future", "expected_tree": "a".repeat(40) })).unwrap();
        assert!(!super::handle_view_token(&request, &context).success);
        assert!(!storage.path().join("blobs/v3").exists());
    }
}
