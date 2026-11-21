//! AFT status command — returns the current state of indexes, features, and configuration.

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

// Preserve the command module's paths for the types moved to status_snapshot.
#[allow(unused_imports)]
use crate::status_snapshot::StatusSnapshotKind;
pub use crate::status_snapshot::{CompressionAggregateSerde, CompressionStats};

pub fn handle_status(req: &RawRequest, ctx: &AppContext) -> Response {
    // A remote embedding backend that a building index has not reached yet
    // has no outage recorded. The probe that finds out runs on its own thread
    // (status is polled and must never wait on the network); until its first
    // result arrives the stage says the backend is being checked.
    let backend_check = crate::semantic_index::check_remote_backend_while_building(ctx);
    let mut snapshot = ctx.build_status_snapshot_for_session(req.session());
    if matches!(
        backend_check,
        crate::semantic_index::RemoteBackendCheck::Checking { .. }
    ) && matches!(
        snapshot["semantic_index"]["status"].as_str(),
        Some("loading" | "building")
    ) {
        snapshot["semantic_index"]["stage"] =
            serde_json::json!(crate::semantic_index::CHECKING_EMBEDDING_BACKEND_STAGE);
    }
    if let Some(removal) = removal_health_for_status(req) {
        snapshot["removal"] = removal;
    }
    // Integration fixtures pin only the unpredictable producer input. Formatting
    // and legacy/result-body serialization below the handler remain unchanged.
    #[cfg(feature = "test-hooks")]
    if let Ok(input) = std::env::var("AFT_TEST_CONFORMANCE_STATUS_SNAPSHOT") {
        snapshot = serde_json::from_str(&input).expect("conformance status snapshot is JSON");
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

    fn registered_status_contexts(
        count: usize,
    ) -> (tempfile::TempDir, Vec<std::sync::Arc<AppContext>>) {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::context::App::default_shared();
        let contexts = (0..count)
            .map(|i| {
                let root = dir.path().join(format!("project-{i}"));
                std::fs::create_dir(&root).unwrap();
                let mut config = Config::default();
                config.project_root = Some(root.clone());
                config.storage_dir = Some(dir.path().join("storage"));
                config.backup.enabled = Some(false);
                let ctx = std::sync::Arc::new(AppContext::from_app(app.clone(), config));
                // Actor registration keys the census by project identity, not
                // the caller's spelling of a temporary or symlinked path.
                let memory_root = cortexkit_paths::ProjectRootId::from_path(&root)
                    .expect("fixture root identity")
                    .as_path()
                    .to_path_buf();
                app.register_memory_context(memory_root, &ctx);
                ctx
            })
            .collect();
        (dir, contexts)
    }

    #[test]
    fn status_push_skips_process_census_but_explicit_status_builds_one() {
        use std::sync::atomic::Ordering;

        let (_dir, contexts) = registered_status_contexts(2);
        let ctx = &contexts[0];
        ctx.app()
            .publish_lifecycle_census(crate::lifecycle_census::LifecycleCensusSnapshot {
                lsp: crate::lifecycle_census::LspCensus {
                    capacity_refusals: 3,
                    ..Default::default()
                },
                ..Default::default()
            });
        let census_builds = &ctx.status_emitter().memory_census_builds;
        let push = ctx.try_build_status_push_snapshot().unwrap();
        assert_eq!(
            census_builds.load(Ordering::Relaxed),
            0,
            "push built a process census"
        );
        assert!(push.get("memory").is_none());

        let explicit = handle_status(&request(), ctx).data;
        assert_eq!(explicit["lsp"]["capacity_refusals"], 3);
        assert_eq!(census_builds.load(Ordering::Relaxed), 1);
        assert_eq!(
            explicit["memory"]["roots_total"],
            2,
            "memory root keys: {:?}",
            explicit["memory"]["roots"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ctx.try_build_status_push_snapshot().unwrap(),
            aft_engine::test_bridge::display_data::project_status(ctx, explicit)
        );
        assert_eq!(census_builds.load(Ordering::Relaxed), 1);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn status_memory_actor_registration_keeps_one_row_for_aliased_current_root() {
        use cortexkit_paths::ProjectRootId;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        #[cfg(unix)]
        let root = {
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            alias
        };
        #[cfg(windows)]
        let root = std::fs::canonicalize(&real).unwrap();
        let root_id = ProjectRootId::from_path(&root).unwrap();
        assert_ne!(root, root_id.as_path());

        let app = crate::context::App::default_shared();
        let mut config = Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(dir.path().join("storage"));
        config.backup.enabled = Some(false);
        let ctx = Arc::new(AppContext::from_app(app.clone(), config));
        let executor = crate::context::Executor::new();
        assert!(executor.register_actor(root_id.clone(), ctx.clone()));
        let registered: Vec<_> = app
            .try_memory_contexts()
            .unwrap()
            .into_iter()
            .map(|(root, _)| root)
            .collect();
        eprintln!("configured root: {root:?}; actor census keys: {registered:?}");
        assert_eq!(registered, vec![root_id.as_path().to_path_buf()]);

        let explicit = handle_status(&request(), &ctx).data;
        let roots = explicit["memory"]["roots"].as_object().unwrap();
        eprintln!(
            "explicit memory root keys: {:?}",
            roots.keys().collect::<Vec<_>>()
        );
        assert_eq!(explicit["memory"]["roots_total"], 1);
        assert!(roots.contains_key(&root_id.as_path().display().to_string()));
    }

    #[test]
    #[ignore = "manual push snapshot timing with 200 registered projects"]
    fn status_push_snapshot_time_with_200_projects() {
        let (_dir, contexts) = registered_status_contexts(200);
        let ctx = &contexts[0];
        assert_eq!(ctx.app().try_memory_contexts().unwrap().len(), 200);
        for _ in 0..5 {
            std::hint::black_box(ctx.try_build_status_push_snapshot().unwrap());
        }
        let start = std::time::Instant::now();
        for _ in 0..50 {
            std::hint::black_box(ctx.try_build_status_push_snapshot().unwrap());
        }
        eprintln!(
            "200 projects, 50 push snapshots: {:?} total, {:?}/push",
            start.elapsed(),
            start.elapsed() / 50
        );
    }

    #[test]
    fn removal_status_reports_an_empty_storage_root_as_zero_state() {
        let storage = tempfile::tempdir().expect("create storage root");
        let request = RawRequest {
            params: json!({ "removal_storage_dir": storage.path() }),
            ..request()
        };

        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request, &ctx);

        assert_eq!(response.data["removal"]["available"], true);
        assert_eq!(response.data["removal"]["project_roots_served"], 0);
        assert_eq!(response.data["removal"]["sessions_served"], 0);
        assert_eq!(response.data["removal"]["running_background_tasks"], 0);
        assert_eq!(response.data["removal"]["undo_history_sessions"], 0);
    }

    #[test]
    fn status_exposes_cache_role_and_canonical_root() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request(), &ctx);
        assert_eq!(response.data["cache_role"], "not_initialized");
        assert!(response.data["canonical_root"].is_null());
        assert!(response.data["runtime"]["callgraph_commits_60s_total"].is_u64());
        assert!(response.data["runtime"]["callgraph_pages_or_bytes_written_60s_total"].is_u64());
        assert_eq!(response.data["watcher"]["raw_events_total"], 0);
        assert_eq!(response.data["watcher"]["raw_events_since_last_rescan"], 0);
        assert_eq!(response.data["watcher"]["rescans_kernel_dropped_total"], 0);
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
        let _ = crate::memory_census::snapshot_uncapped("ready", std::collections::BTreeMap::new());

        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
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
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let before = crate::memory::allocator_snapshot_calls_for_test();
        let memory = handle_status(&request(), &ctx).data["memory"]["process"].clone();

        assert_eq!(memory["allocator_slack_measured"], false);
        assert!(memory["allocator_observation_age_ms"].is_null());
        assert_eq!(crate::memory::allocator_snapshot_calls_for_test(), before);
    }

    #[test]
    fn memory_snapshot_reports_contended_subsystem_as_busy() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
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
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
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
    fn status_reports_backend_unavailable_instead_of_loading() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "embedding_symbols".to_string(),
                files: Some(1),
                entries_done: Some(0),
                entries_total: Some(10),
            };
        ctx.trip_semantic_refresh_circuit(1, "connection refused by embedding backend");

        let status = ctx.build_status_snapshot();
        let semantic = &status["semantic_index"];
        assert_eq!(semantic["status"], "backend_unavailable");
        assert_eq!(semantic["state"], "backend_unavailable");
        assert_eq!(
            semantic["reason"],
            "connection refused by embedding backend"
        );
        assert!(semantic["since_ms"].is_u64());
        assert!(semantic["next_retry_ms"].is_u64());
    }

    /// `aft status` is where a user goes to ask whether semantic search works.
    /// It used to label a loaded index object by its entry count alone, which
    /// cannot express failure at all: a root whose index had failed answered
    /// `ready` here while the sidebar and a search reply both said `failed`.
    #[test]
    fn status_reports_a_failed_semantic_index_as_failed_not_ready() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let project = std::env::current_dir().expect("current dir");
        *ctx.semantic_index().write().unwrap() =
            Some(crate::semantic_index::SemanticIndex::new(project, 384));
        *ctx.semantic_index_status().write().unwrap() = crate::context::SemanticIndexStatus::Failed(
            "embedding backend died mid-build".to_string(),
        );

        let semantic = ctx.build_status_snapshot()["semantic_index"].clone();

        assert_eq!(semantic["status"], "failed");
        assert_eq!(semantic["state"], "failed");
    }

    /// A project with no source files still finishes its semantic build: the
    /// index holds zero entries. That is ready, not an index still loading. A
    /// build that is genuinely still running for the same project keeps
    /// reporting `loading`. The old `semantic/<key>/` file it persists is not
    /// sized: from 0.64 status never reads the old index folders, which
    /// `aft cache prune-legacy` deletes.
    #[test]
    fn status_reports_an_empty_finished_semantic_index_as_ready_without_sizing_old_folders() {
        let project = tempfile::tempdir().expect("project dir");
        let storage = tempfile::tempdir().expect("storage dir");
        let root = std::fs::canonicalize(project.path()).expect("canonical project");
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        ctx.update_config(|config| {
            config.project_root = Some(root.clone());
            config.storage_dir = Some(storage.path().to_path_buf());
        });
        ctx.set_canonical_cache_root(root.clone());
        let key = ctx.memoized_artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &key, false);

        let index = crate::semantic_index::SemanticIndex::new(root.clone(), 384);
        assert!(
            index.write_to_disk(storage.path(), &key),
            "empty index persists"
        );
        *ctx.semantic_index().write().unwrap() = Some(index);
        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::ready();

        let ready = ctx.build_status_snapshot();
        assert_eq!(ready["semantic_index"]["status"], "ready");
        assert_eq!(ready["semantic_index"]["entries"], 0);
        assert_eq!(
            ready["disk"]["semantic_disk_bytes"], 0,
            "status sized the old semantic/ folder: {}",
            ready["disk"]
        );
        assert_eq!(ready["disk"]["project_cache_key"], key.as_str());

        *ctx.semantic_index_status().write().unwrap() =
            crate::context::SemanticIndexStatus::Building {
                stage: "embedding_symbols".to_string(),
                files: Some(0),
                entries_done: Some(0),
                entries_total: Some(0),
            };
        assert_eq!(
            ctx.build_status_snapshot()["semantic_index"]["status"],
            "loading"
        );
    }

    #[test]
    fn status_status_bar_is_null_until_every_independent_producer_is_populated() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let response = handle_status(&request(), &ctx);
        // No TODO scan has run yet, so the status-bar glance must be null
        // (never fabricated zeros). The key is always present so the TS
        // coercion can distinguish "field absent" from "not populated".
        assert!(response.data.get("status_bar").is_some());
        assert!(response.data["status_bar"].is_null());

        // The TODO count is only one input to the status bar. Keep `status_bar`
        // null until diagnostics and all other required inputs have reported.
        ctx.update_status_bar_todos(Some(5));
        let response = handle_status(&request(), &ctx);
        assert!(response.data["status_bar"].is_null());
    }

    // One unavailable producer (diagnostics with no language server) keeps
    // `status_bar` null, so renderers need the per-category values to
    // show what is known instead of reporting startup indefinitely.
    #[test]
    fn status_bar_values_report_each_category_independently() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        // Diagnostics counts stay "pending" until a project-wide applicability
        // snapshot exists. This fixture runs no server discovery, so record
        // that no language server applies to the project.
        ctx.lsp().store_project_applicability(Default::default());
        ctx.update_status_bar_todos(Some(5));
        let response = handle_status(&request(), &ctx);

        assert!(response.data["status_bar"].is_null());
        assert_eq!(
            response.data["status_bar_values"],
            serde_json::json!({
                "errors": null,
                "warnings": null,
                "diagnostics": "no_language_server",
                "diagnostics_gaps": [],
                "todos": 5,
            })
        );
    }

    /// Without a running language server nothing will ever report errors or
    /// warnings. The status must say so rather than leave both null, which
    /// every renderer shows as a count that is still on its way.
    #[test]
    fn status_bar_values_name_missing_language_server_instead_of_pending() {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        // Server discovery has run and found no language server for this
        // project. Before discovery has run, the counts would read "pending".
        ctx.lsp().store_project_applicability(Default::default());
        ctx.update_status_bar_todos(Some(0));
        let response = handle_status(&request(), &ctx);

        let values = &response.data["status_bar_values"];
        assert!(values["errors"].is_null());
        assert!(values["warnings"].is_null());
        assert_eq!(values["diagnostics"], "no_language_server");
    }
}
