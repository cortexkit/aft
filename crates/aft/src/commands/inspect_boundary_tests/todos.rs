#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use crate::config::Config;
    use crate::inspect::scanners::todos::run_todos_scan;
    use crate::inspect::InspectJob;
    use crate::parser::todo_markers::markers_lexical;
    use aft_engine::test_bridge::search_misc::inspect::{
        note_withheld_root, outcomes_for_test, todos_job, uncached_aggregate_for_test,
        TODOS_NO_INDEX_STORE,
    };
    use aft_store::content::TodoMarkers;
    use std::path::Path;
    use std::time::Instant;

    /// The markers extraction records for `source` at `path`, as the index
    /// store's records hold them.
    fn markers(path: &str, source: &str) -> TodoMarkers {
        let language = crate::data_layer::content::language_for(path, source.as_bytes())
            .expect("a language the store records");
        let mut record =
            crate::data_layer::content::extract(source.as_bytes(), &language).expect("extraction");
        crate::data_layer::content::add_config_markers(&mut record, path, source.as_bytes())
            .expect("configuration markers");
        record.todos.expect("the record holds TODO markers")
    }

    fn lines(todos: &TodoMarkers) -> Vec<(u32, String, String)> {
        todos
            .items
            .as_ref()
            .expect("text")
            .iter()
            .map(|item| (item.line, item.marker.clone(), item.text.clone()))
            .collect()
    }

    /// Every marker must open the comment; an upper-case marker word inside
    /// prose or a string is not a work item.
    #[test]
    fn markers_open_comments_and_strings_never_count() {
        let todos = markers(
            "src/lib.rs",
            "// BUG: leading marker counts\n\
             // this documents a BUG in prose\n\
             // the TODO list and the FIXME tag and a HACK and XXX are prose here\n\
             fn f() { let s = \"// TODO: not a comment\"; }\n\
             /* FIXME(ana): block */\n",
        );
        assert_eq!(todos.language, "rust");
        assert_eq!(
            lines(&todos),
            vec![
                (1, "BUG".into(), "leading marker counts".into()),
                (5, "FIXME".into(), "block".into()),
            ]
        );
        assert_eq!(
            todos.items.as_ref().unwrap()[1].author.as_deref(),
            Some("ana")
        );
    }

    /// Markdown, minified bundles, resolver configuration and headers no
    /// grammar resolves are not parsed by the call graph, and still hold
    /// their markers, read as today's count read them.
    #[test]
    fn files_the_call_graph_skips_still_hold_their_markers() {
        let markdown = markers(
            "docs/notes.md",
            "# TODO heading is not a comment\n<!-- TODO: real comment -->\n```\n<!-- FIXME: in a fence -->\n```\n",
        );
        assert_eq!(markdown.language, "markdown");
        assert_eq!(
            lines(&markdown),
            vec![(2, "TODO".into(), "real comment".into())]
        );

        let minified = format!("/* HACK: shim */{}\n", "var a=1;".repeat(400));
        let bundle = markers("dist-like/app.min.js", &minified);
        assert_eq!(bundle.language, "javascript");
        assert_eq!(lines(&bundle), vec![(1, "HACK".into(), "shim".into())]);

        let cargo = markers("Cargo.toml", "[package]\n# TODO: publish\nname = \"x\"\n");
        assert_eq!(cargo.language, "toml");
        assert_eq!(lines(&cargo), vec![(2, "TODO".into(), "publish".into())]);

        let not_text = crate::parser::todo_markers::markers_lexical(b"// TODO: a\0b\n");
        assert_eq!(not_text.items, None);
        let lexical = markers_lexical(b"x = \"// TODO: quoted\"\n# XXX: real\n");
        assert_eq!(lexical.language, "lexical");
        assert_eq!(lines(&lexical), vec![(2, "XXX".into(), "real".into())]);
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// After a cold start the status bar's TODO refresh can run while the
    /// index store is still building, and come back pending. When the
    /// store publishes, its status listener asks for the refresh again, so
    /// the count arrives with no edit and no `aft_inspect` call.
    #[test]
    fn a_count_pending_while_the_store_builds_is_taken_again_when_it_publishes() {
        use crate::inspect::tier2_scheduler::{
            TIER2_REFRESH_COLD_CACHE_DELAY, TIER2_REFRESH_MIN_INTERVAL,
        };
        use crate::inspect::Tier2TriggerReason;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        fs::create_dir_all(dir.path().join("project")).unwrap();
        fs::create_dir_all(&storage).unwrap();
        // Canonicalize only once the folder exists: the scan keys its outcome by
        // the job's resolved root, which differs from the spelled temp path
        // where the temp folder sits behind a link (/var on macOS) or a short
        // name (Windows).
        let checkout_root = fs::canonicalize(dir.path().join("project")).unwrap();
        let root = crate::path_identity::canonicalize_normalized(&checkout_root);
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        write(
            &root,
            "src/app.rs",
            "// TODO: counted once the store publishes\nfn app() {}\n",
        );
        // The checkout retains configure's canonical path, including Windows's
        // `\\?\` prefix. The build hold must use that exact key, not inspect's
        // normalized outcome key, or the build can publish before we release it.
        let hold = crate::data_layer::runtime::hold_builds_for_test(&checkout_root);
        // A refresh that finds the store building answers pending at once.
        crate::inspect::data_sources::set_todo_store_wait_for_test(&root, Duration::ZERO);

        let ctx = Arc::new(crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                storage_dir: Some(storage.clone()),
                ..Config::default()
            },
        ));
        ctx.bind_status_context();
        let doc = serde_json::json!({"indexes": {"trigram": false, "semantic": false, "callgraph": true}});
        let configure: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "configure",
            "command": "configure",
            "harness": "opencode",
            "project_root": root,
            "storage_dir": storage,
            "config": [{"tier": "user", "source": "/u/aft.jsonc", "doc": doc.to_string()}],
        }))
        .unwrap();
        let response = crate::commands::configure::handle_configure(&configure, &ctx);
        assert!(response.success, "configure failed: {response:?}");
        crate::runtime_drain::standalone_root_configured(&ctx);
        assert_eq!(
            ctx.data_checkout()
                .expect("the index store binds the root")
                .root(),
            checkout_root,
            "the build hold must name the bound checkout"
        );

        // The warm-up refresh runs while the build is held.
        let configured_at = Instant::now()
            .checked_sub(TIER2_REFRESH_COLD_CACHE_DELAY + Duration::from_secs(1))
            .unwrap();
        ctx.reset_tier2_refresh_scheduler_at(configured_at);
        let now = Instant::now();
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(now, 0),
            Some(Tier2TriggerReason::ConfigureWarm)
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        // The outcome is noted inside the scan, before its completion is queued.
        // Drain the completion too, so an in-flight job cannot hide a stray pull.
        while outcomes_for_test(&root).is_empty() || ctx.inspect_manager().tier2_any_in_flight() {
            assert!(Instant::now() < deadline, "the warm-up refresh never ran");
            crate::runtime_drain::drain_inspect_events(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        crate::runtime_drain::drain_inspect_events(&ctx);
        assert_eq!(outcomes_for_test(&root), ["pending"]);
        assert_eq!(ctx.status_bar_count_values().todos, None);
        // Nothing else asks for a refresh: no edit, no inspect call.
        let later = now + TIER2_REFRESH_MIN_INTERVAL;
        assert_eq!(ctx.tick_tier2_refresh_scheduler_at(later, 0), None);

        // The store builds and publishes; its status listener asks for the
        // refresh, which the next maintenance tick starts.
        drop(hold);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match ctx.tick_tier2_refresh_scheduler_at(later, 0) {
                Some(reason) => {
                    assert_eq!(reason, Tier2TriggerReason::Pull);
                    break;
                }
                None => {
                    assert!(Instant::now() < deadline, "no refresh was asked for");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while ctx.status_bar_count_values().todos != Some(1) {
            assert!(
                Instant::now() < deadline,
                "the count never arrived: {:?}",
                outcomes_for_test(&root)
            );
            crate::runtime_drain::drain_inspect_events(&ctx);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(outcomes_for_test(&root), ["pending", "counted"]);
        // Asked once for the change, not on every later status change.
        assert_eq!(
            ctx.tick_tier2_refresh_scheduler_at(later + TIER2_REFRESH_MIN_INTERVAL, 0),
            None
        );
    }

    /// Until a checkout binds or is explicitly withheld, the count is pending
    /// and must not report zero TODOs.
    #[test]
    fn a_root_without_records_is_pending_never_zero() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(dir.path());
        write(&root, "src/lib.rs", "// TODO: unseen\n");
        let result = run_todos_scan(&todos_job(&root, vec![root.join("src/lib.rs")]));
        let aggregate = result.outcome.unwrap().aggregate;
        assert_eq!(aggregate["status"], "pending", "{aggregate:#}");
        assert_eq!(
            aggregate,
            uncached_aggregate_for_test(&todos_job(&root, vec![root.join("src/lib.rs")]))
        );
        assert!(aggregate.get("count").is_none(), "{aggregate:#}");

        note_withheld_root(&root);
        let result = run_todos_scan(&todos_job(&root, vec![root.join("src/lib.rs")]));
        let aggregate = result.outcome.unwrap().aggregate;
        assert_eq!(aggregate["unavailable"], true, "{aggregate:#}");
        assert_eq!(
            aggregate,
            uncached_aggregate_for_test(&todos_job(&root, vec![root.join("src/lib.rs")]))
        );
        assert_eq!(aggregate["gaps"][0]["reason"], TODOS_NO_INDEX_STORE);
    }

    /// Counting the TODOs of a bound, published checkout reads them from the
    /// store's records: it opens no source file and parses nothing. After
    /// edits that add, change and remove markers, the count follows, and
    /// only the edited files are extracted, once each, by the store.
    #[test]
    fn counting_a_bound_checkout_opens_and_parses_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::path_identity::canonicalize_normalized(dir.path());
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let app = "// TODO: first unparsed marker\nfn app() {}\n";
        let notes = "# Notes\n<!-- FIXME: unparsed markdown marker -->\n";
        let cargo = "[package]\n# HACK: unparsed config marker\nname = \"todo-fixture\"\nversion = \"0.1.0\"\n";
        write(&root, "src/app.rs", app);
        write(&root, "src/plain.rs", "fn plain() {}\n");
        write(&root, "docs/notes.md", notes);
        write(&root, "Cargo.toml", cargo);
        write(&root, "data.json", "{\"note\": \"// TODO: never read\"}\n");
        write(&root, "src/binary.rs", "// TODO: not text\0\n");
        write(&root, "tests/fixture.rs", "// TODO: test-tree marker\n");
        write(&root, "src/huge.rs", &"// filler\n".repeat(120_000));

        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(root.clone()),
                ..Config::default()
            },
        );
        crate::data_layer::surfaces::serve_text_index_for_test(&ctx);
        let checkout = ctx.data_checkout().expect("bound");
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let view = loop {
            let view = checkout.view().expect("published");
            if view.status().callgraph_published && !checkout.checkout().status().building {
                break view;
            }
            assert!(Instant::now() < deadline, "the call graph never published");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let parses =
            |source: &str| crate::data_layer::content::parses_of_for_test(source.as_bytes());
        let parsed_before = [app, notes, cargo].map(parses);

        let probe = crate::project_content::Probe::new(&root);
        let job = todos_job(&root, scope.clone());
        let aggregate = run_todos_scan(&job).outcome.unwrap().aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!([app, notes, cargo].map(parses), parsed_before);
        assert_eq!(aggregate["count"], 3, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["TODO"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["FIXME"], 1, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["HACK"], 1, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_count"], 1, "{aggregate:#}");
        let files = aggregate["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["file"].as_str().unwrap(),
                    item["line"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![("Cargo.toml", 2), ("docs/notes.md", 2), ("src/app.rs", 1)]
        );
        assert_eq!(
            aggregate["skipped_files"],
            serde_json::json!([{"file": "src/huge.rs", "reason": "over the 1 MiB size limit"}])
        );

        // Add, change and remove markers.
        let plain = "fn plain() {}\n// XXX: added marker\n";
        let changed_app = "fn app() {}\n// TODO: moved and reworded\n";
        write(&root, "src/plain.rs", plain);
        write(&root, "src/app.rs", changed_app);
        write(&root, "docs/notes.md", "# Notes\n");
        checkout.note_paths(&[
            root.join("src/plain.rs"),
            root.join("src/app.rs"),
            root.join("docs/notes.md"),
        ]);
        // Recording the edits read the files; the count must read none.
        drop(probe);
        let probe = crate::project_content::Probe::new(&root);
        let view = checkout.view().unwrap();
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let aggregate = run_todos_scan(&todos_job(&root, scope))
            .outcome
            .unwrap()
            .aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!(aggregate["count"], 3, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["FIXME"], 0, "{aggregate:#}");
        assert_eq!(aggregate["by_kind"]["XXX"], 1, "{aggregate:#}");
        let items = aggregate["items"].as_array().unwrap();
        assert_eq!(items[1]["file"], "src/app.rs");
        assert_eq!(items[1]["line"], 2);
        assert_eq!(items[1]["text"], "moved and reworded");
        // The edited files were extracted once, for their changes.
        assert_eq!(parses(plain), 1);
        assert_eq!(parses(changed_app), 1);

        // Publish the changes, then fold the publication's segments into
        // one: the fold copies the stored records, markers and all, so the
        // count reads the same from the folded segment, still opening and
        // parsing nothing.
        checkout.checkout().refresh_callgraph().unwrap();
        checkout.checkout().publish_changes().unwrap();
        let crate::data_layer::checkout::Folded::Done(folded) = checkout.checkout().fold().unwrap()
        else {
            panic!("the fold did not finish");
        };
        assert!(folded.read_before > 1, "{folded:?}");
        assert!(folded.retired_segments > 0, "{folded:?}");
        let view = checkout.view().unwrap();
        let scope = crate::inspect::inventory::InspectInventory::from_checkout(&root, &view)
            .expect("the view stands in for a walk")
            .sources;
        let parsed_before_count = [plain, changed_app].map(parses);
        let folded_aggregate = run_todos_scan(&todos_job(&root, scope))
            .outcome
            .unwrap()
            .aggregate;
        assert!(probe.paths().is_empty(), "opened {:?}", probe.paths());
        assert_eq!(folded_aggregate, aggregate);
        assert_eq!([plain, changed_app].map(parses), parsed_before_count);

        let mut job = todos_job(
            &root,
            crate::inspect::inventory::InspectInventory::from_checkout(
                &root,
                &checkout.view().unwrap(),
            )
            .unwrap()
            .sources,
        );
        let parity = |job: &InspectJob| {
            let cached = run_todos_scan(job).outcome.unwrap().aggregate;
            assert_eq!(
                serde_json::to_vec(&cached).unwrap(),
                serde_json::to_vec(&uncached_aggregate_for_test(job)).unwrap()
            );
        };
        parity(&job);
        // Cross the detail boundary in one file, with test markers ahead of
        // it in the caller's order. They must not use up the detail-item limit.
        write(
            &root,
            "src/app.rs",
            &"// BUG(owner): beyond the first page\n".repeat(110),
        );
        checkout.note_paths(&[root.join("src/app.rs")]);
        job.scope_files.reverse();
        parity(&job);
        checkout.checkout().refresh_callgraph().unwrap();
        checkout.checkout().publish_changes().unwrap();
        parity(&job);
        crate::inspect::data_sources::set_todo_store_wait_for_test(
            &root,
            std::time::Duration::ZERO,
        );
        checkout.checkout().begin_rescan();
        parity(&job);
        assert_eq!(
            run_todos_scan(&job).outcome.unwrap().aggregate["status"],
            "pending"
        );
        checkout.checkout().end_rescan();
        // Discover an unreported addition and deletion, as a watcher rescan
        // does after a missed event, and then publish the new membership.
        write(
            &root,
            "src/rescanned.rs",
            "// FIXME: discovered on rescan\n",
        );
        std::fs::remove_file(root.join("src/plain.rs")).unwrap();
        checkout.checkout().rescan().unwrap();
        job.scope_files = crate::inspect::inventory::InspectInventory::from_checkout(
            &root,
            &checkout.view().unwrap(),
        )
        .unwrap()
        .sources;
        parity(&job);
        checkout.checkout().refresh_callgraph().unwrap();
        checkout.checkout().publish_changes().unwrap();
        parity(&job);
    }
}
