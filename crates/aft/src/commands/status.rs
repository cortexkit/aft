//! AFT status command — returns the current state of indexes, features, and configuration.

use crate::context::AppContext;
use crate::context::SemanticIndexStatus;
use crate::db::compression_events::CompressionAggregate;
use crate::protocol::{RawRequest, Response, StatusPayload, DEFAULT_SESSION_ID};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CompressionStats {
    pub project: CompressionAggregateSerde,
    pub session: CompressionAggregateSerde,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CompressionAggregateSerde {
    pub events: u64,
    pub original_tokens: u64,
    pub compressed_tokens: u64,
    pub savings_tokens: u64,
}

impl From<CompressionAggregate> for CompressionAggregateSerde {
    fn from(agg: CompressionAggregate) -> Self {
        Self {
            events: agg.events,
            original_tokens: agg.original_tokens,
            compressed_tokens: agg.compressed_tokens,
            savings_tokens: agg.savings_tokens(),
        }
    }
}

pub fn handle_status(req: &RawRequest, ctx: &AppContext) -> Response {
    Response::success(
        &req.id,
        ctx.build_status_snapshot_for_session(req.session()),
    )
}

impl AppContext {
    pub fn build_status_snapshot(&self) -> StatusPayload {
        self.build_status_snapshot_for_session(DEFAULT_SESSION_ID)
    }

    pub fn build_status_snapshot_for_session(&self, session_id: &str) -> StatusPayload {
        let config = self.config();

        // Search index status
        let search_index_info = {
            let index = self.search_index().borrow();
            match index.as_ref() {
                Some(idx) if idx.ready => {
                    let file_count = idx.file_count();
                    let trigram_count = idx.trigram_count();
                    serde_json::json!({
                        "status": "ready",
                        "files": file_count,
                        "trigrams": trigram_count,
                    })
                }
                Some(_) => serde_json::json!({ "status": "building" }),
                None => {
                    let status = if self.config().search_index {
                        "loading"
                    } else {
                        "disabled"
                    };
                    serde_json::json!({ "status": status })
                }
            }
        };

        // Semantic index status
        let semantic_index_info = {
            let index = self.semantic_index().borrow();
            match index.as_ref() {
                Some(idx) => {
                    serde_json::json!({
                        "status": idx.status_label(),
                        "entries": idx.entry_count(),
                        "dimension": idx.dimension(),
                        "backend": idx.backend_label().unwrap_or(config.semantic_backend_label()),
                        "model": idx.model_label().unwrap_or(config.semantic.model.as_str()),
                    })
                }
                None => match &*self.semantic_index_status().borrow() {
                    SemanticIndexStatus::Disabled => serde_json::json!({
                        "status": "disabled",
                        "backend": config.semantic_backend_label(),
                        "model": config.semantic.model.as_str(),
                    }),
                    SemanticIndexStatus::Building {
                        stage,
                        files,
                        entries_done,
                        entries_total,
                    } => serde_json::json!({
                        "status": "loading",
                        "stage": stage,
                        "files": files,
                        "entries_done": entries_done,
                        "entries_total": entries_total,
                        "backend": config.semantic_backend_label(),
                        "model": config.semantic.model.as_str(),
                    }),
                    SemanticIndexStatus::Ready => serde_json::json!({
                        "status": "ready",
                        "backend": config.semantic_backend_label(),
                        "model": config.semantic.model.as_str(),
                    }),
                    SemanticIndexStatus::Failed(error) => serde_json::json!({
                        "status": "failed",
                        "error": error,
                        "backend": config.semantic_backend_label(),
                        "model": config.semantic.model.as_str(),
                    }),
                },
            }
        };

        // Disk cache sizes — scoped to the **current project** only.
        //
        // Both trigram (`<storage_dir>/index/<key>/`) and semantic
        // (`<storage_dir>/semantic/<key>/`) caches are partitioned per project by
        // `project_cache_key(project_root)`. Earlier this function reported the
        // recursive size of the entire `index/` and `semantic/` directories,
        // which summed disk usage across **every** project the user had ever
        // opened. The TUI sidebar surfaced that total as if it were the current
        // project's footprint, which was misleading (e.g. a 4.8 MB project with
        // 9 sibling projects appeared to use 16+ GB).
        //
        // We now resolve the per-project key from `config.project_root` and
        // size only that project's slice. When the project key can't be
        // resolved (no project_root), fall back to zeros — the cross-project
        // total is never the right answer to display per-session.
        let storage_dir = config.storage_dir.as_ref().map(|d| d.display().to_string());
        let disk_info = match (&config.storage_dir, &config.project_root) {
            (Some(dir), Some(root)) => {
                let key = crate::search_index::project_cache_key(root);
                let trigram_size = dir_size(&dir.join("index").join(&key));
                let semantic_size = dir_size(&dir.join("semantic").join(&key));
                serde_json::json!({
                    "storage_dir": dir.display().to_string(),
                    "project_cache_key": key,
                    "trigram_disk_bytes": trigram_size,
                    "semantic_disk_bytes": semantic_size,
                })
            }
            (Some(dir), None) => serde_json::json!({
                "storage_dir": dir.display().to_string(),
                "project_cache_key": null,
                "trigram_disk_bytes": 0,
                "semantic_disk_bytes": 0,
            }),
            _ => serde_json::json!({
                "storage_dir": null,
                "project_cache_key": null,
                "trigram_disk_bytes": 0,
                "semantic_disk_bytes": 0,
            }),
        };

        // LSP servers
        let lsp_count = self.lsp_server_count();

        // Symbol cache stats
        let symbol_cache_stats = self.symbol_cache_stats();

        // Per-session undo/checkpoint counts (issue #14 — one shared bridge serves
        // many sessions; surface both the global footprint and the current
        // session's own slice so `/aft-status` can split them in the UI).
        let checkpoint_total = self.checkpoint().borrow().total_count();
        let session_checkpoints = self.checkpoint().borrow().list(session_id).len();
        let session_tracked_files = self.backup().borrow().tracked_files(session_id).len();
        let compression = self.compression_stats_for_session(session_id);

        // Degraded-mode reasons recorded by `handle_configure` when the
        // project root doesn't look like a real project (`home_root`) or the
        // file count exceeds the search-index threshold
        // (`search_too_many_files:N`). Heavy subsystems are auto-disabled in
        // these modes; the plugin / TUI sidebar surface the reasons so users
        // know why and can decide whether to open a project subdirectory.
        // Empty list = full-featured mode.
        let degraded_reasons = self.degraded_reasons();
        let degraded = !degraded_reasons.is_empty();

        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "project_root": config.project_root.as_ref().map(|p| p.display().to_string()),
            "canonical_root": self.canonical_cache_root_opt().map(|p| p.display().to_string()),
            "cache_role": self.cache_role(),
            "degraded": degraded,
            "degraded_reasons": degraded_reasons,
            "features": {
                "format_on_edit": config.format_on_edit,
                "validate_on_edit": config.validate_on_edit.as_deref().unwrap_or("off"),
                "restrict_to_project_root": config.restrict_to_project_root,
                "search_index": config.search_index,
                "semantic_search": config.semantic_search,
            },
            "search_index": search_index_info,
            "semantic_index": semantic_index_info,
            "disk": disk_info,
            "lsp_servers": lsp_count,
            "symbol_cache": symbol_cache_stats,
            "compression": compression,
            "storage_dir": storage_dir,
            // Project-wide (all sessions): total in-memory checkpoint count.
            "checkpoints_total": checkpoint_total,
            // Current session slice: only when the caller passed `session_id`.
            "session": {
                "id": session_id,
                "tracked_files": session_tracked_files,
                "checkpoints": session_checkpoints,
            },
        })
    }

    fn compression_stats_for_session(&self, session_id: &str) -> CompressionStats {
        let mut compression = CompressionStats::default();
        let Some(project_root) = self.config().project_root.clone() else {
            return compression;
        };
        let Some(db) = self.db() else {
            return compression;
        };
        let Ok(conn) = db.lock() else {
            return compression;
        };

        let harness = self.harness().as_str();
        let project_key = crate::search_index::project_cache_key(&project_root);
        if let Ok(project_agg) =
            crate::db::compression_events::aggregate_for_project(&conn, harness, &project_key)
        {
            compression.project = project_agg.into();
        }
        if let Ok(session_agg) = crate::db::compression_events::aggregate_for_session(
            &conn,
            harness,
            &project_key,
            session_id,
        ) {
            compression.session = session_agg.into();
        }

        compression
    }
}

/// Recursively compute the total size of a directory.
fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    dir_size_recursive(path)
}

fn dir_size_recursive(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else if ft.is_dir() {
            total += dir_size_recursive(&entry.path());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::handle_status;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use serde_json::json;

    fn request() -> RawRequest {
        RawRequest {
            id: "status".to_string(),
            command: "status".to_string(),
            lsp_hints: None,
            session_id: None,
            params: json!({}),
        }
    }

    #[test]
    fn status_exposes_cache_role_and_canonical_root() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "not_initialized");
        assert!(response.data["canonical_root"].is_null());

        let temp = tempfile::tempdir().unwrap();
        ctx.config_mut().project_root = Some(temp.path().to_path_buf());
        ctx.set_canonical_cache_root(std::fs::canonicalize(temp.path()).unwrap());
        ctx.set_cache_role(false, None);
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "main");
        assert!(response.data["canonical_root"].as_str().is_some());

        ctx.set_cache_role(true, None);
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "worktree");
    }
}
