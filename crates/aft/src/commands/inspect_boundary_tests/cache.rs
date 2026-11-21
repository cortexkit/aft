mod tests {
    use crate::config::Config;
    use crate::inspect::{InspectCategory, InspectSnapshot, JobKey};
    use aft_engine::test_bridge::search_misc::inspect::{
        contribution_set_hash_for_version_for_test, TIER2_CONTRIBUTION_CACHE_VERSION,
    };
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn upgrade_context(root: &Path, storage: &Path) -> crate::context::AppContext {
        let mut config = Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            harness: Some(crate::harness::Harness::Opencode),
            ..Config::default()
        };
        config.indexes.trigram = false;
        config.indexes.semantic = false;
        config.inspect.categories = serde_json::from_value(serde_json::json!({
            "diagnostics": false, "todos": false, "duplicates": false,
            "cycles": false, "complexity": false,
        }))
        .unwrap();
        let ctx = crate::test_context(Box::new(crate::parser::TreeSitterProvider::new()), config);
        ctx.isolate_cold_build_limiter_for_test(2);
        ctx.set_harness(crate::harness::Harness::Opencode);
        let request = crate::protocol::RawRequest {
            id: "upgrade-configure".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params: serde_json::json!({
                "project_root": root, "storage_dir": storage, "harness": "opencode",
                "config": {
                    "indexes": {"trigram": false, "semantic": false, "callgraph": true},
                    "inspect": {"categories": {
                        "diagnostics": false, "todos": false, "duplicates": false,
                        "cycles": false, "complexity": false,
                    }},
                },
            }),
        };
        let response = crate::commands::configure::handle_configure(&request, &ctx);
        assert!(response.success, "{}", response.data);
        ctx.inspect_manager()
            .set_automatic_tier2_refresh_allowed(false);
        // Dead code answers nothing, fresh or cached, until the root's
        // index-store checkout has published a call graph. Bind the checkout,
        // wait for that graph, and let the context's inspect manager read it,
        // as an `inspect` request does.
        let checkout = ctx.data_checkout().expect("the index store binds the root");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !checkout
            .view()
            .is_some_and(|view| view.status().callgraph_published)
        {
            assert!(Instant::now() < deadline, "the call graph never published");
            std::thread::sleep(Duration::from_millis(20));
        }
        ctx.inspect_manager()
            .read_dead_code_from_data_layer(&ctx.canonical_cache_root(), &checkout);
        ctx
    }

    fn upgrade_snapshot(ctx: &crate::context::AppContext) -> InspectSnapshot {
        InspectSnapshot::new(
            ctx.canonical_cache_root(),
            ctx.inspect_dir(),
            ctx.config(),
            ctx.symbol_cache(),
        )
    }

    fn upgrade_aggregate(
        ctx: &crate::context::AppContext,
        category: InspectCategory,
    ) -> serde_json::Value {
        // Dead code reads the call graph of the checkout `upgrade_context`
        // bound, so no graph is passed in.
        let result = ctx.inspect_manager().tier2_run_with_reuse_result(
            upgrade_snapshot(ctx),
            category,
            None,
        );
        result.outcome.expect("upgrade scan succeeds").aggregate
    }

    fn store_pre_campaign_aggregate(
        ctx: &crate::context::AppContext,
        category: InspectCategory,
        aggregate: serde_json::Value,
    ) -> PathBuf {
        // Version 34 is the frozen pre-upgrade format, not current minus one:
        // later format bumps must continue to invalidate this persisted data.
        const PRE_CAMPAIGN_VERSION: u32 = 34;
        assert!(PRE_CAMPAIGN_VERSION < TIER2_CONTRIBUTION_CACHE_VERSION);
        let cache = ctx
            .inspect_manager()
            .cache_for_snapshot(&upgrade_snapshot(ctx))
            .unwrap();
        let old_hash = contribution_set_hash_for_version_for_test(
            &cache,
            category,
            &ctx.config(),
            PRE_CAMPAIGN_VERSION,
        )
        .unwrap();
        cache
            .store_tier2_aggregate(JobKey::for_project_category(category), &old_hash, aggregate)
            .unwrap();
        cache.sqlite_path().to_path_buf()
    }

    fn assert_pre_campaign_aggregate_reopen_pages_all_findings(category: InspectCategory) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"upgrade-app","private":true}"#,
        )
        .unwrap();
        fs::write(root.join("src/main.ts"), "console.log('entry');\n").unwrap();
        fs::write(
            root.join("src/spares.ts"),
            (0..150)
                .map(|i| format!("export function spare{i:03}() {{ return {i}; }}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let root = fs::canonicalize(root).unwrap();
        let storage = temp.path().join("storage");
        let ctx = upgrade_context(&root, &storage);
        let expected = upgrade_aggregate(&ctx, category);
        assert_eq!(expected["count"], 150, "{category:?}: {expected}");
        assert_eq!(expected["items"].as_array().unwrap().len(), 150);
        let mut truncated = expected.clone();
        truncated["items"].as_array_mut().unwrap().truncate(100);
        let sqlite_path = store_pre_campaign_aggregate(&ctx, category, truncated);
        drop(ctx);

        // Opening a new context discards all in-memory caches. Only the old
        // persisted aggregates and unchanged source/contribution rows survive.
        let reopened = upgrade_context(&root, &storage);
        let cache = reopened
            .inspect_manager()
            .cache_for_snapshot(&upgrade_snapshot(&reopened))
            .unwrap();
        assert_eq!(cache.sqlite_path(), sqlite_path);
        assert!(
            cache
                .get_aggregated_for_config(
                    &JobKey::for_project_category(category),
                    &reopened.config()
                )
                .unwrap()
                .is_none(),
            "{category:?}: the old truncated aggregate must not be reused"
        );
        let rebuilt = upgrade_aggregate(&reopened, category);
        assert_eq!(rebuilt["count"], expected["count"]);
        assert_eq!(rebuilt["items"], expected["items"]);

        let list = category.as_str();
        let envelope = format!("{list}_list_envelope");
        let page = |offset| {
            let request = crate::protocol::RawRequest {
                id: format!("upgrade-{list}-{offset}"),
                command: "inspect".to_string(),
                lsp_hints: None,
                session_id: None,
                params: serde_json::json!({
                    "scope": root, "sections": [list], "topK": 100, "offset": offset,
                }),
            };
            let response = crate::commands::inspect::handle_inspect(&request, &reopened);
            assert!(response.success, "{}", response.data);
            response.data["details"].clone()
        };
        let first = page(0);
        assert_eq!(first[list].as_array().unwrap().len(), 100);
        assert_eq!(first[&envelope]["next_offset"], 100);
        assert_eq!(first, page(0), "order must be stable within a generation");
        let second = page(100);
        assert_eq!(second[list].as_array().unwrap().len(), 50);
        assert_eq!(second[&envelope]["offset"], 100);
        assert_eq!(second[&envelope]["next_offset"], serde_json::Value::Null);
        let exhausted = page(150);
        assert_eq!(exhausted[list], serde_json::json!([]));
        assert_eq!(exhausted[&envelope]["next_offset"], serde_json::Value::Null);
        for details in [&first, &second, &exhausted] {
            assert_eq!(details[&envelope]["total"]["value"], 150);
        }
        let rows = first[list]
            .as_array()
            .unwrap()
            .iter()
            .chain(second[list].as_array().unwrap())
            .map(|row| {
                (
                    row["file"].to_string(),
                    row["symbol"].to_string(),
                    row["line"].to_string(),
                )
            })
            .collect::<HashSet<_>>();
        assert_eq!(rows.len(), 150, "pages must be disjoint and exhaustive");
    }

    #[test]
    fn pre_campaign_aggregate_reopen_pages_all_150_dead_code_findings() {
        assert_pre_campaign_aggregate_reopen_pages_all_findings(InspectCategory::DeadCode);
    }

    #[test]
    fn pre_campaign_aggregate_reopen_pages_all_150_unused_exports() {
        assert_pre_campaign_aggregate_reopen_pages_all_findings(InspectCategory::UnusedExports);
    }

    #[test]
    fn pre_campaign_aggregate_reopen_rebuilds_go_initializer_liveness() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("go.mod"),
            "module example.com/upgrade\n\ngo 1.22\n",
        )
        .unwrap();
        fs::write(root.join("main.go"), "package main\nvar Boot = Register()\nfunc Register() int { return 1 }\nfunc main() {}\n").unwrap();
        let root = fs::canonicalize(root).unwrap();
        let storage = temp.path().join("storage");
        let ctx = upgrade_context(&root, &storage);
        let clean = upgrade_aggregate(&ctx, InspectCategory::DeadCode);
        assert_eq!(clean["count"], 0, "{clean}");
        let mut legacy = clean.clone();
        legacy["count"] = serde_json::json!(1);
        legacy["items"] =
            serde_json::json!([{"file":"main.go", "symbol":"Register", "line":3, "language":"go"}]);
        store_pre_campaign_aggregate(&ctx, InspectCategory::DeadCode, legacy);
        drop(ctx);
        let reopened = upgrade_context(&root, &storage);
        let rebuilt = upgrade_aggregate(&reopened, InspectCategory::DeadCode);
        assert_eq!(rebuilt["count"], clean["count"]);
        assert_eq!(rebuilt["items"], clean["items"]);
        assert!(!rebuilt["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["symbol"] == "Register"));
    }
}
