mod tests {
    use crate::context::AppContext;
    use crate::data_layer::checkout::DataError;
    #[cfg(target_os = "linux")]
    use crate::data_layer::surfaces::disclose_index_gaps;
    use crate::data_layer::surfaces::{reason, report, zoom_callgraph_field, DataCheckoutProbe};
    use crate::memory::MemoryEstimate;
    use crate::protocol::RawRequest;
    #[cfg(target_os = "linux")]
    use aft_engine::test_bridge::display_data::surfaces::{
        bind_for_test, serve_text_index_for_test,
    };
    use aft_engine::test_bridge::display_data::surfaces::{
        root_fixture_config, settled, write, READY,
    };
    use serde_json::{json, Value};
    use std::path::Path;
    #[cfg(target_os = "linux")]
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn context(root: &Path, storage: &Path) -> AppContext {
        context_with(root, storage, |_| {})
    }

    fn context_with(
        root: &Path,
        storage: &Path,
        edit: impl FnOnce(&mut crate::config::Config),
    ) -> AppContext {
        let config = root_fixture_config(root, storage, edit);
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.set_canonical_cache_root(root.to_path_buf());
        ctx
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_folders_publish_with_health_and_query_gaps_then_rescan_recovers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = context_with(&root, storage.path(), |config| {
            config.indexes.semantic = false
        });
        write(&root, "open.txt", "needle open\n");
        write(&root, "locked/hidden.txt", "needle hidden\n");
        let locked = root.join("locked");
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let restore = Restore(locked.clone());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0)).unwrap();
        assert_eq!(
            std::fs::read_dir(&locked)
                .err()
                .expect("run without DAC override privileges")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        serve_text_index_for_test(&ctx);
        let health = report(&ctx).unwrap().health();
        assert_eq!(health.skipped_folders, 1);
        assert_eq!(
            health.skipped_folder_note,
            Some(format!(
                "{}: index skipped 1 unreadable folder",
                root.display()
            ))
        );
        assert_eq!(health.reason, None);
        assert!(health.failure.is_none());
        let request = |command: &str| {
            serde_json::from_value(serde_json::json!({
                "id": format!("gap-{command}"), "command": command,
                "pattern": if command == "glob" { "**/*.txt" } else { "needle" },
                "query": "needle",
            }))
            .unwrap()
        };
        let grep = crate::commands::grep::handle_grep(&request("grep"), &ctx);
        let glob = crate::commands::glob::handle_glob(&request("glob"), &ctx);
        let search = crate::commands::semantic_search::handle_semantic_search(
            &request("semantic_search"),
            &ctx,
        );
        let expected = format!("(Index skipped 1 unreadable folder; results may miss files in them:\n{}: permission denied (os error 13)\n)", locked.display());
        for response in [&grep, &glob, &search] {
            assert!(response.success, "{:?}", response.data);
            assert_eq!(response.data["complete"], false, "{:?}", response.data);
            assert_eq!(response.data["skipped_folders"], 1);
            assert_eq!(
                response.data["gaps"][0],
                serde_json::json!({"kind":"unreadable_folder", "path": locked, "reason":"permission denied (os error 13)"})
            );
            assert_eq!(
                response.data["text"]
                    .as_str()
                    .unwrap()
                    .split("\n\n")
                    .next()
                    .unwrap(),
                expected
            );
            assert!(response.data["text"].as_str().unwrap().contains("open.txt"));
        }
        assert_eq!(grep.data["index_status"], "Ready");
        let mut scoped = crate::protocol::Response::success(
            "scope",
            serde_json::json!({"text":"unchanged", "complete":true}),
        );
        disclose_index_gaps(&ctx, &[root.join("src")], &mut scoped);
        assert_eq!(scoped.data["text"], "unchanged");
        drop(restore);
        ctx.data_checkout().unwrap().checkout().rescan().unwrap();
        let recovered = crate::commands::grep::handle_grep(&request("grep"), &ctx);
        assert!(recovered.success, "{:?}", recovered.data);
        assert!(
            recovered.data["text"]
                .as_str()
                .unwrap()
                .contains("hidden.txt"),
            "{:?}",
            recovered.data
        );
        assert!(recovered.data["skipped_folders"].is_null());
        assert_eq!(report(&ctx).unwrap().health().skipped_folders, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreadable_root_publishes_a_named_gap_and_rescan_retries_after_access_returns() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = context_with(&root, storage.path(), |config| {
            config.indexes.semantic = false
        });
        write(&root, "readable-later.txt", "needle\n");
        let hold = crate::data_layer::runtime::hold_builds_for_test(&root);
        bind_for_test(&ctx);
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let restore = Restore(root.clone());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0)).unwrap();
        assert_eq!(
            std::fs::read_dir(&root)
                .err()
                .expect("run without DAC override privileges")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        drop(hold);
        serve_text_index_for_test(&ctx);
        let checkout = ctx.data_checkout().unwrap();
        assert!(checkout.view().unwrap().current_state().paths().is_empty());
        let health = report(&ctx).unwrap().health();
        assert_eq!(
            health.skipped_folder_note,
            Some(format!(
                "{}: index skipped 1 unreadable folder",
                root.display()
            ))
        );
        assert_eq!(health.reason, None);
        assert!(health.failure.is_none());
        let req = serde_json::from_value(
            serde_json::json!({"id":"root-gap", "command":"grep", "pattern":"needle"}),
        )
        .unwrap();
        let response = crate::commands::grep::handle_grep(&req, &ctx);
        assert!(response.success, "{:?}", response.data);
        assert_eq!(response.data["complete"], false);
        assert_eq!(response.data["gaps"][0]["path"], serde_json::json!(root));
        assert_eq!(response.data["text"].as_str().unwrap().split("\n\n").next().unwrap(), format!("(Index skipped 1 unreadable folder; results may miss files in them:\n{}: permission denied (os error 13)\n)", root.display()));
        drop(restore);
        checkout.checkout().rescan().unwrap();
        let recovered = crate::commands::grep::handle_grep(&req, &ctx);
        assert!(recovered.success, "{:?}", recovered.data);
        assert!(
            recovered.data["text"]
                .as_str()
                .unwrap()
                .contains("readable-later.txt"),
            "{:?}",
            recovered.data
        );
        assert!(recovered.data["skipped_folders"].is_null());
        assert_eq!(report(&ctx).unwrap().health().skipped_folders, 0);
    }

    fn zoom(ctx: &AppContext, file: &Path) -> Value {
        let request = RawRequest {
            id: "zoom".into(),
            command: "zoom".into(),
            lsp_hints: None,
            session_id: None,
            params: json!({"file": file, "symbol": "caller", "callgraph": true}),
        };
        let response = crate::commands::zoom::handle_zoom(&request, ctx);
        assert!(response.success, "{:?}", response.data);
        response.data
    }

    #[test]
    fn status_health_and_zoom_follow_the_checkout_through_each_state() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let lib = root.join("src/lib.rs");

        // Not bound: off, with the reason, and settled; reading never binds.
        let payload = ctx.build_status_snapshot();
        assert_eq!(
            payload["search_index"],
            json!({"status": "off", "reason": "not_bound"})
        );
        assert_eq!(payload["callgraph_store"]["status"], "off");
        assert_eq!(payload["data_store"]["reason"], "not_bound");
        assert!(payload.get("callgraph_refresh").is_none());
        assert!(payload["disk"].get("data_store_bytes").is_none());
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.search_index.unwrap().status, "off");
        assert_eq!(health.callgraph_store.unwrap().status, "off");
        assert_eq!(
            health.data_store.unwrap().reason,
            Some(aft_engine::test_bridge::display_data::surfaces::NOT_BOUND)
        );
        assert_eq!(
            zoom(&ctx, &lib)["callgraph"],
            json!({"status": "unavailable", "code": "callgraph_unavailable",
                "index": {"status": "off", "reason": "not_bound"}})
        );
        assert!(
            matches!(ctx.probe_data_checkout(), Some(DataCheckoutProbe::NotBound)),
            "status, health and zoom bound nothing"
        );

        // A bind in progress: building everywhere, never ready.
        {
            let _binding = ctx.hold_data_checkout_slot_for_test();
            let payload = ctx.build_status_snapshot();
            assert_eq!(payload["search_index"]["status"], "building");
            assert_eq!(payload["callgraph_store"]["status"], "building");
            let health = ctx.try_health_snapshot(&root);
            assert_eq!(health.search_index.unwrap().status, "building");
            assert_eq!(health.callgraph_store.unwrap().status, "building");
            assert_eq!(
                zoom_callgraph_field(&ctx),
                Some(Some(
                    json!({"status": "unavailable", "code": "callgraph_building",
                    "index": {"status": "building", "reason": null}})
                ))
            );
        }

        // Bound and built: ready, with the file count and the store's size.
        assert!(ctx.data_checkout().is_some());
        let ready = settled(&ctx);
        assert_eq!(ready.files, Some(3));
        let payload = ctx.build_status_snapshot();
        assert_eq!(
            payload["search_index"],
            json!({"status": "ready", "files": 3})
        );
        assert_eq!(
            payload["callgraph_store"],
            json!({"status": "ready", "pending_edits": 0, "unparsed_files": 0})
        );
        assert!(payload["disk"]["data_store_bytes"].as_u64().unwrap() > 0);
        assert!(payload["disk"]["data_store_dir"]
            .as_str()
            .unwrap()
            .starts_with(&storage.path().display().to_string()));
        assert!(payload.get("views").is_none());
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.search_index.unwrap().status, "ready");
        assert_eq!(health.callgraph_store.unwrap().status, "ready");
        assert_eq!(
            serde_json::to_value(health.data_store.unwrap()).unwrap(),
            json!({"pending_edits": 0, "unparsed_files": 0})
        );
        assert!(zoom(&ctx, &lib).get("callgraph").is_none());

        // An edit waits for the lazy refresh: still ready, the edit counted,
        // zoom quiet.
        let extra = write(
            &root,
            "src/extra.rs",
            "pub fn other() { crate::util::callee(); }\n",
        );
        ctx.data_note_paths(&[extra]);
        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["callgraph_store"]["status"], "ready");
        assert!(
            payload["callgraph_store"]["pending_edits"]
                .as_u64()
                .unwrap()
                >= 1,
            "{payload:?}"
        );
        assert_eq!(
            ctx.try_health_snapshot(&root)
                .callgraph_store
                .unwrap()
                .status,
            "ready"
        );
        assert!(zoom(&ctx, &lib).get("callgraph").is_none());

        // A failed refresh: unavailable with its reason and error, in health
        // with when it failed, and in zoom; the next success clears it.
        let checkout = ctx.data_checkout().expect("bound");
        checkout.note_outcome_for_test::<()>(
            "refresh",
            &Err(DataError::Resolve("resolver gave up".into())),
        );
        let payload = ctx.build_status_snapshot();
        assert_eq!(payload["callgraph_store"]["status"], "unavailable");
        assert_eq!(payload["callgraph_store"]["reason"], "refresh_failed");
        assert_eq!(
            payload["callgraph_store"]["error"],
            "AFT could not build this checkout's call graph: resolver gave up"
        );
        assert_eq!(payload["search_index"]["status"], "ready");
        let health = ctx.try_health_snapshot(&root);
        assert_eq!(health.callgraph_store.unwrap().status, "unavailable");
        let failure = health.data_store.unwrap().failure.unwrap();
        assert_eq!((failure.stage, failure.busy), ("refresh", false));
        assert!(failure.at_ms > 0);
        assert_eq!(
            zoom_callgraph_field(&ctx).unwrap().unwrap()["code"],
            "callgraph_unavailable"
        );
        checkout.note_outcome_for_test("refresh", &Ok(()));
        assert_eq!(report(&ctx).unwrap().callgraph.status, READY);
    }

    #[test]
    fn the_memory_census_counts_the_checkout_and_the_shared_block_cache() {
        let dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let ctx = context(&root, storage.path());
        let data_layer_bytes =
            |plane: &MemoryEstimate| plane.counts.get("data_layer_bytes").copied();
        // The root's census row and the sum of its planes.
        let census_row = |memory: &crate::memory::RootMemorySnapshot| {
            let snapshot = crate::memory_census::snapshot(
                "ready",
                [(root.display().to_string(), memory.clone())].into(),
            );
            let census = crate::commands::memory_census::render_memory_census(&snapshot, None);
            let row = census["roots"][root.display().to_string()].clone();
            let planes = row["planes"]
                .as_object()
                .unwrap()
                .values()
                .map(|bytes| bytes.as_u64().unwrap())
                .sum::<u64>();
            (row, planes)
        };

        // Not bound: nothing held, said so.
        let unbound = ctx.memory_root_snapshot();
        let shared = unbound.data_layer.clone().unwrap();
        assert_eq!(shared.estimated_bytes, Some(0));
        assert_eq!(shared.counts["bound_checkouts"], 0);
        for plane in [&unbound.trigram, &unbound.callgraph, &unbound.semantic] {
            assert_eq!(data_layer_bytes(plane), Some(0));
        }
        {
            let _binding = ctx.hold_data_checkout_slot_for_test();
            let binding = ctx.memory_root_snapshot();
            assert_eq!(binding.data_layer.as_ref().unwrap().status, "busy");
            assert_eq!(binding.status, "busy", "a busy part makes the root busy");
        }

        ctx.data_checkout().unwrap();
        settled(&ctx);
        // A query reads through the block cache.
        let view = ctx.data_view().unwrap();
        view.path_text(b"src/lib.rs").unwrap();
        drop(view);
        let root_memory = ctx.memory_root_snapshot();
        let shared = root_memory.data_layer.clone().unwrap();
        assert_eq!(shared.status, "ready");
        // Every part is measured: nothing is left as a gap.
        assert_eq!(shared.bytes_status, "estimated");
        assert!(shared.not_estimated.is_empty(), "{shared:?}");
        assert!(shared.estimated_bytes.unwrap() > 0);
        assert_eq!(shared.counts["bound_checkouts"], 1);
        assert_eq!(shared.counts["state_paths"], 3);
        // Each plane's share is in the plane, and itemised.
        let search = shared.counts["search_bytes"];
        let callgraph = shared.counts["callgraph_bytes"];
        assert!(search > 0 && callgraph > 0, "{shared:?}");
        assert_eq!(data_layer_bytes(&root_memory.trigram), Some(search));
        assert_eq!(data_layer_bytes(&root_memory.callgraph), Some(callgraph));
        assert_eq!(
            data_layer_bytes(&root_memory.semantic),
            Some(shared.counts["semantic_bytes"])
        );
        assert!(root_memory.trigram.estimated_bytes.unwrap() >= search);
        assert!(root_memory.callgraph.estimated_bytes.unwrap() >= callgraph);

        let snapshot = crate::memory_census::snapshot(
            "ready",
            [(root.display().to_string(), root_memory.clone())].into(),
        );
        let cache = snapshot.data_layer_block_cache.clone().unwrap();
        assert_eq!(cache.bytes_status, "estimated");
        assert!(cache.counts["budget_bytes"] > 0);
        assert!(cache.counts["blocks"] > 0);
        assert!(
            cache.estimated_bytes.unwrap() > cache.counts["resident_bytes"],
            "the cache's figure is its blocks and its tables: {cache:?}"
        );
        assert_eq!(
            snapshot.process.data_layer_block_cache_bytes,
            cache.estimated_bytes
        );
        let (row, planes_sum) = census_row(&root_memory);
        assert_eq!(
            row["planes"]["data_layer"],
            json!(shared.estimated_bytes.unwrap())
        );
        assert_eq!(
            row["planes"]["search"],
            json!(root_memory.trigram.estimated_bytes.unwrap())
        );
        assert_eq!(row["data_layer"]["search_bytes"], json!(search));

        // Nothing is counted twice: the root's total is the sum of its
        // planes, and the process's total is the sum of its roots and of
        // what it counts once for every root.
        assert_eq!(row["attributed_bytes"], json!(planes_sum));
        assert_eq!(root_memory.attributed_bytes, planes_sum);
        let process = &snapshot.process;
        assert_eq!(
            process.total_attributed_bytes,
            planes_sum
                + snapshot.shared_semantic_bases.estimated_bytes.unwrap_or(0)
                + process.data_layer_block_cache_bytes.unwrap()
                + process.embedding_model_bytes
                + process.sqlite.memory_used_bytes
        );

        // An edit adds its overlay, and its refresh starts the call graph's
        // engine, whose bytes are the call graph's.
        let extra = write(&root, "src/extra.rs", "pub fn other() {}\n");
        ctx.data_note_paths(&[extra]);
        let checkout = ctx.data_checkout().unwrap();
        checkout.checkout().refresh_callgraph().unwrap();
        let edited = ctx.memory_root_snapshot();
        let edited_shared = edited.data_layer.clone().unwrap();
        assert_eq!(edited_shared.counts["changed_paths"], 1);
        assert!(edited_shared.counts["text_overlay_bytes"] > 0);
        assert!(edited_shared.counts["search_bytes"] >= edited_shared.counts["text_overlay_bytes"]);
        assert_eq!(edited_shared.counts["callgraph_engine_started"], 1);
        let engine = edited_shared.counts["callgraph_engine_bytes"];
        assert!(engine > 0);
        assert!(edited_shared.counts["callgraph_bytes"] > engine);
        let (edited_row, edited_planes) = census_row(&edited);
        assert_eq!(edited_row["attributed_bytes"], json!(edited_planes));
        assert_eq!(edited.attributed_bytes, edited_planes);

        // Releasing the idle checkout drops its share.
        drop(checkout);
        settled(&ctx);
        let started = Instant::now();
        while !ctx.try_release_idle_checkout_views() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the idle release never ran"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let released = ctx.memory_root_snapshot();
        assert_eq!(
            released.data_layer.as_ref().unwrap().counts["bound_checkouts"],
            0
        );
        assert_eq!(
            released.data_layer.as_ref().unwrap().estimated_bytes,
            Some(0)
        );
        for plane in [&released.trigram, &released.callgraph, &released.semantic] {
            assert_eq!(data_layer_bytes(plane), Some(0));
        }
        assert!(
            released.attributed_bytes
                <= edited.attributed_bytes
                    - edited_shared.estimated_bytes.unwrap()
                    - edited_shared.counts["search_bytes"]
                    - edited_shared.counts["callgraph_bytes"],
            "{} after release, {} before",
            released.attributed_bytes,
            edited.attributed_bytes
        );
    }

    #[cfg(test)]
    use aft_engine::test_bridge::display_data::AppContextTestExt as _;
    #[cfg(test)]
    use aft_engine::test_bridge::display_data::DataStoreReportTestExt as _;
    #[cfg(test)]
    use aft_engine::test_bridge::display_data::IndexHost as _;
}
