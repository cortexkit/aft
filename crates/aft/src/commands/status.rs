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
    let mut snapshot = ctx.build_status_snapshot_for_session(req.session());
    if let Some(removal) = removal_health_for_status(req) {
        snapshot["removal"] = removal;
    }
    Response::success(&req.id, snapshot)
}

/// Add removal-time state only when the management caller names a storage root.
///
/// Normal status refreshes are frequent and must stay independent from a global
/// SQLite aggregation. `aft doctor` supplies this parameter while the user is
/// deciding whether to remove AFT, and the read-only helper never creates or
/// migrates their database.
fn removal_health_for_status(req: &RawRequest) -> Option<serde_json::Value> {
    let storage_root = req.params.get("removal_storage_dir")?;
    let Some(storage_root) = storage_root.as_str() else {
        return Some(serde_json::json!({
            "available": false,
            "message": "removal_storage_dir must be a string",
        }));
    };

    Some(
        match crate::db::removal::removal_health_from_storage_root(std::path::Path::new(
            storage_root,
        )) {
            Ok(health) => serde_json::json!({
                "available": true,
                "usage_window_days": health.usage_window_days,
                "project_roots_served": health.project_roots_served,
                "sessions_served": health.sessions_served,
                "project_roots_source": health.project_roots_source,
                "running_background_tasks": health.running_background_tasks,
                "undo_history_sessions": health.undo_history_sessions,
            }),
            Err(message) => serde_json::json!({
                "available": false,
                "message": message,
            }),
        },
    )
}

impl AppContext {
    pub fn build_status_snapshot(&self) -> StatusPayload {
        self.build_status_snapshot_for_session(DEFAULT_SESSION_ID)
    }

    pub fn build_status_snapshot_for_session(&self, session_id: &str) -> StatusPayload {
        let config = self.config();

        // Search index status. Status is a control-path snapshot, so lock
        // pressure is represented directly instead of delaying the caller.
        let search_index_info = match self.search_index().try_read() {
            Ok(index) => match index.as_ref() {
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
                    let status = if config.search_index {
                        "loading"
                    } else {
                        "disabled"
                    };
                    serde_json::json!({ "status": status })
                }
            },
            Err(_) => serde_json::json!({ "status": "busy" }),
        };

        let semantic_status = self
            .semantic_index_status()
            .try_read()
            .ok()
            .map(|status| status.clone());
        let semantic_index_info = match semantic_status {
            None => serde_json::json!({ "status": "busy", "state": "busy" }),
            Some(status) => match self.semantic_index().try_read() {
                Err(_) => serde_json::json!({ "status": "busy", "state": "busy" }),
                Ok(index) => {
                    let refreshing_count = status.refreshing_count();
                    match index.as_ref() {
                        Some(idx) => {
                            let status_label = match status {
                                SemanticIndexStatus::Ready { .. } => "ready",
                                _ => idx.status_label(),
                            };
                            serde_json::json!({
                                "status": status_label,
                                "state": status_label,
                                "refreshing_count": refreshing_count,
                                "entries": idx.entry_count(),
                                "dimension": idx.dimension(),
                                "backend": idx.backend_label().unwrap_or(config.semantic_backend_label()),
                                "model": idx.model_label().unwrap_or(config.semantic.model.as_str()),
                            })
                        }
                        None => match status {
                            SemanticIndexStatus::Disabled => serde_json::json!({
                                "status": "disabled",
                                "state": "disabled",
                                "refreshing_count": 0,
                                "backend": config.semantic_backend_label(),
                                "model": config.semantic.model.as_str(),
                            }),
                            SemanticIndexStatus::Building {
                                stage,
                                files,
                                entries_done,
                                entries_total,
                            } => {
                                let mut snapshot = serde_json::json!({
                                    "status": "loading",
                                    "state": "loading",
                                    "refreshing_count": 0,
                                    "stage": stage,
                                    "files": files,
                                    "entries_done": entries_done,
                                    "entries_total": entries_total,
                                    "backend": config.semantic_backend_label(),
                                    "model": config.semantic.model.as_str(),
                                });
                                if let Some(progress) = self.semantic_build_progress() {
                                    let progress = progress.snapshot();
                                    snapshot["embedded_chunks"] =
                                        serde_json::json!(progress.embedded_chunks);
                                    snapshot["total_chunks"] =
                                        serde_json::json!(progress.total_chunks);
                                    snapshot["current_batch"] =
                                        serde_json::json!(progress.current_batch);
                                    snapshot["total_batches"] =
                                        serde_json::json!(progress.total_batches);
                                }
                                snapshot
                            }
                            SemanticIndexStatus::Ready { refreshing, .. } => serde_json::json!({
                                "status": "ready",
                                "state": "ready",
                                "refreshing_count": refreshing.len(),
                                "backend": config.semantic_backend_label(),
                                "model": config.semantic.model.as_str(),
                            }),
                            SemanticIndexStatus::Failed(error) => serde_json::json!({
                                "status": "failed",
                                "state": "failed",
                                "refreshing_count": 0,
                                "error": error,
                                "backend": config.semantic_backend_label(),
                                "model": config.semantic.model.as_str(),
                            }),
                        },
                    }
                }
            },
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
                let key_root = self
                    .canonical_cache_root_opt()
                    .unwrap_or_else(|| root.clone());
                // Passive read only: status must never trigger a key
                // derivation (git probe). Artifact-backed features derive and
                // memoize the key at configure; when none are enabled there is
                // no per-project artifact slice to size.
                match self.cached_artifact_cache_key(&key_root) {
                    Some(key) => {
                        let trigram_size = dir_size(&dir.join("index").join(&key));
                        let semantic_size = dir_size(&dir.join("semantic").join(&key));
                        serde_json::json!({
                            "storage_dir": dir.display().to_string(),
                            "project_cache_key": key,
                            "trigram_disk_bytes": trigram_size,
                            "semantic_disk_bytes": semantic_size,
                        })
                    }
                    None => serde_json::json!({
                        "storage_dir": dir.display().to_string(),
                        "project_cache_key": null,
                        "trigram_disk_bytes": 0,
                        "semantic_disk_bytes": 0,
                    }),
                }
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
        let backups_enabled = config.backup.enabled.unwrap_or(true);
        let checkpoint_total = if backups_enabled {
            self.checkpoint().lock().total_count()
        } else {
            0
        };
        let session_checkpoints = if backups_enabled {
            self.checkpoint()
                .lock()
                .list(session_id)
                .map(|checkpoints| checkpoints.len())
                .unwrap_or_else(|error| {
                    crate::slog_warn!("status checkpoint hydration failed: {}", error);
                    0
                })
        } else {
            0
        };
        let session_tracked_files = if backups_enabled {
            self.backup().lock().tracked_files(session_id).len()
        } else {
            0
        };
        let compression = self.compression_stats_for_session(session_id);
        let (backup_skipped_too_large_total, backup_skipped_temp_path_total) =
            crate::backup::backup_skipped_totals();

        // Degraded-mode reasons recorded by `handle_configure` when the
        // project root doesn't look like a real project (`home_root`). Heavy
        // subsystems are auto-disabled in that mode; the plugin / TUI sidebar
        // surface the reason so users know why and can decide whether to open a
        // project subdirectory. Empty list = full-featured mode.
        let degraded_reasons = self.degraded_reasons();
        let degraded = !degraded_reasons.is_empty();
        let artifact_owner = self
            .artifact_owner_status()
            .map(|status| serde_json::to_value(status).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null);

        // Agent status-bar counts (the `[AFT E· W· | D· U· C· | T·]` glance).
        // Surfaced for the TUI sidebar so users see the same code-health view
        // agents get. `None` until the Tier-2 cache is populated at least once
        // (so we never render fabricated zeros) — emitted as JSON null then,
        // and the sidebar hides the section.
        let status_bar = match self.status_bar_counts() {
            Some(counts) => serde_json::json!({
                "errors": counts.errors,
                "warnings": counts.warnings,
                "dead_code": counts.dead_code,
                "unused_exports": counts.unused_exports,
                "duplicates": counts.duplicates,
                "todos": counts.todos,
                "tier2_stale": counts.tier2_stale,
            }),
            None => serde_json::Value::Null,
        };
        let memory_root = self
            .canonical_cache_root_opt()
            .or_else(|| config.project_root.clone());
        let callgraph_write_metrics = memory_root
            .as_deref()
            .and_then(|root| self.cached_artifact_cache_key(root))
            .map(|project_key| {
                crate::callgraph_store::callgraph_write_metrics_for_project(&project_key)
            })
            .unwrap_or_default();
        let callgraph_write_metrics_total = crate::callgraph_store::callgraph_write_metrics_total();
        // `MemorySnapshot::new` uses the process-wide allocator observation so
        // status never walks allocator zones on a request worker.
        let memory = serde_json::to_value(self.memory_snapshot(memory_root.as_deref()))
            .unwrap_or(serde_json::Value::Null);
        // The control-path status response reads the health worker's published
        // lifecycle snapshot; it never probes processes or opens a database.
        let lifecycle = self.app().lifecycle_census_snapshot();
        let mut runtime = serde_json::json!({
            "live_watchers": self.app().watcher_count(),
            "live_actor_roots": self.app().actor_root_count(),
            "open_routes": self.app().open_route_count(),
            "callgraph_commits_60s_total": callgraph_write_metrics_total.commits_60s,
            "callgraph_pages_or_bytes_written_60s_total": callgraph_write_metrics_total
                .pages_or_bytes_written_60s,
        });
        if callgraph_write_metrics.commits_60s > 0 {
            runtime["callgraph_commits_60s"] =
                serde_json::json!(callgraph_write_metrics.commits_60s);
        }
        if callgraph_write_metrics.pages_or_bytes_written_60s > 0 {
            runtime["callgraph_pages_or_bytes_written_60s"] =
                serde_json::json!(callgraph_write_metrics.pages_or_bytes_written_60s);
        }

        let mut payload = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "project_root": config.project_root.as_ref().map(|p| p.display().to_string()),
            "canonical_root": self.canonical_cache_root_opt().map(|p| p.display().to_string()),
            // Machine field. Human renderers must treat worktree/read_only as a
            // shared-index borrow, never as a degraded_reasons entry.
            "cache_role": self.cache_role(),
            "artifact_owner": artifact_owner,
            "degraded": degraded,
            "degraded_reasons": degraded_reasons,
            "features": {
                "format_on_edit": config.format_on_edit,
                "validate_on_edit": config.validate_on_edit.as_deref().unwrap_or("off"),
                "restrict_to_project_root": config.restrict_to_project_root,
                "search_index": config.search_index,
                "semantic_search": config.semantic_search,
                "callgraph_store": config.callgraph_store,
                "backup": backups_enabled,
            },
            "search_index": search_index_info,
            "semantic_index": semantic_index_info,
            "status_bar": status_bar,
            "disk": disk_info,
            "lsp_servers": lsp_count,
            "symbol_cache": symbol_cache_stats,
            "memory": memory,
            "lsp": lifecycle.lsp,
            "threads": lifecycle.threads,
            "sqlite": lifecycle.sqlite,
            "children": lifecycle.children,
            "fds": lifecycle.fds,
            "runtime": runtime,
            "compression": compression,
            "storage_dir": storage_dir,
            // Project-wide (all sessions): total in-memory checkpoint count.
            "checkpoints_total": checkpoint_total,
            "backup_skipped_too_large_total": backup_skipped_too_large_total,
            "backup_skipped_temp_path_total": backup_skipped_temp_path_total,
            // Current session slice: only when the caller passed `session_id`.
            "session": {
                "id": session_id,
                "tracked_files": session_tracked_files,
                "checkpoints": session_checkpoints,
            },
        });
        if config.views.enabled {
            payload["views"] = serde_json::to_value(self.view_health_snapshot())
                .unwrap_or(serde_json::Value::Null);
        }
        payload
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

        let harness = self.harness().storage_segment();
        let project_key = crate::path_identity::project_scope_key(&project_root);
        if let Ok((project, session)) = self.compression_aggregate_cache().aggregates_for_session(
            &conn,
            &harness,
            &project_key,
            session_id,
        ) {
            compression.project = project.into();
            compression.session = session.into();
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
    fn removal_status_reports_an_empty_storage_root_as_zero_state() {
        let storage = tempfile::tempdir().expect("create storage root");
        let request = RawRequest {
            params: json!({ "removal_storage_dir": storage.path() }),
            ..request()
        };

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request, &ctx);

        assert_eq!(response.data["removal"]["available"], true);
        assert_eq!(response.data["removal"]["project_roots_served"], 0);
        assert_eq!(response.data["removal"]["sessions_served"], 0);
        assert_eq!(response.data["removal"]["running_background_tasks"], 0);
        assert_eq!(response.data["removal"]["undo_history_sessions"], 0);
    }

    #[test]
    fn status_exposes_cache_role_and_canonical_root() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "not_initialized");
        assert!(response.data["canonical_root"].is_null());
        assert!(response.data["runtime"]["callgraph_commits_60s_total"].is_u64());
        assert!(response.data["runtime"]["callgraph_pages_or_bytes_written_60s_total"].is_u64());
        assert!(response.data["backup_skipped_too_large_total"].is_u64());
        assert!(response.data["backup_skipped_temp_path_total"].is_u64());

        let temp = tempfile::tempdir().unwrap();
        ctx.update_config(|config| {
            config.project_root = Some(temp.path().to_path_buf());
        });
        ctx.set_canonical_cache_root(std::fs::canonicalize(temp.path()).unwrap());
        ctx.set_cache_role(false, None);
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "main");
        assert!(response.data["canonical_root"].as_str().is_some());

        ctx.set_cache_role(true, None);
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "worktree");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn status_reuses_cached_allocator_observation_for_repeated_requests() {
        let _allocator_test_lock = crate::memory::allocator_observation_test_lock();
        crate::memory::reset_allocator_observation_for_test();
        let _ =
            crate::memory::MemorySnapshot::new_uncapped("ready", std::collections::BTreeMap::new());

        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let before = crate::memory::allocator_snapshot_calls_for_test();
        for _ in 0..50 {
            let response = handle_status(&request(), &ctx);
            assert!(response.data["memory"]["process"]["allocator_slack_measured"].is_boolean());
        }
        let memory = handle_status(&request(), &ctx).data["memory"]["process"].clone();
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
        assert!(memory["allocator_observation_age_ms"].is_u64());
    }

    #[test]
    fn status_reports_cold_allocator_observation_without_measuring() {
        let _allocator_test_lock = crate::memory::allocator_observation_test_lock();
        crate::memory::reset_allocator_observation_for_test();
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let before = crate::memory::allocator_snapshot_calls_for_test();
        let memory = handle_status(&request(), &ctx).data["memory"]["process"].clone();

        assert_eq!(memory["allocator_slack_measured"], false);
        assert!(memory["allocator_observation_age_ms"].is_null());
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
    }

    #[test]
    fn memory_snapshot_reports_contended_subsystem_as_busy() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let _semantic_writer = ctx.semantic_index().write().unwrap();
        let status = ctx.build_status_snapshot();
        assert_eq!(status["semantic_index"]["status"], "busy");
        assert_eq!(
            status["memory"]["roots"]["<unconfigured>"]["semantic"]["status"],
            "busy"
        );
        assert_eq!(status["memory"]["process"]["sqlite"]["status"], "measured");
        assert!(status["memory"]["process"]["allocator"]["status"].is_string());
        assert!(status["memory"]["process"]["allocator"]
            .get("retained_slack_bytes")
            .is_some());
    }

    #[test]
    fn status_exposes_live_semantic_build_progress_only_while_building() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let progress = crate::context::SemanticBuildProgress::default();
        progress.report(3, 10, 2);
        ctx.set_semantic_build_progress(Some(progress));
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "embedding_symbols".to_string(),
                files: Some(1),
                entries_done: Some(3),
                entries_total: Some(10),
            };

        let building = ctx.build_status_snapshot();
        let semantic = &building["semantic_index"];
        assert_eq!(semantic["stage"], "embedding_symbols");
        assert_eq!(semantic["embedded_chunks"], 3);
        assert_eq!(semantic["total_chunks"], 10);
        assert_eq!(semantic["current_batch"], 2);
        assert_eq!(semantic["total_batches"], 5);

        ctx.set_semantic_build_progress(None);
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::ready();
        let ready = ctx.build_status_snapshot();
        assert!(ready["semantic_index"].get("embedded_chunks").is_none());
        assert!(ready["semantic_index"].get("total_chunks").is_none());
    }

    #[test]
    fn status_status_bar_is_null_until_tier2_populated() {
        let ctx = AppContext::new(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request(), &ctx);
        // No Tier-2 scan has run yet, so the status-bar glance must be null
        // (never fabricated zeros). The key is always present so the TS
        // coercion can distinguish "field absent" from "not populated".
        assert!(response.data.get("status_bar").is_some());
        assert!(response.data["status_bar"].is_null());

        // Once Tier-2 counts are populated, the snapshot carries the glance.
        ctx.update_status_bar_tier2(Some(3), Some(2), Some(1), Some(5), false);
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["status_bar"]["dead_code"], 3);
        assert_eq!(response.data["status_bar"]["unused_exports"], 2);
        assert_eq!(response.data["status_bar"]["duplicates"], 1);
        assert_eq!(response.data["status_bar"]["tier2_stale"], false);
    }
}
