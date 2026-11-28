mod callgraph_store_for_ops_tests {
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::inspect::{InspectCategory, InspectSnapshot, JobOutcome, JobScope};
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use crate::watcher_filter::WatcherDispatchEvent;
    use aft_engine::test_bridge::display_data::context::{
        callgraph_test_support::cold_build_fixture, install_pending_capture_hook_for_test,
        PendingCaptureBoundary,
    };
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn cold_build_context() -> ([TempDir; 2], Arc<AppContext>) {
        let (dirs, config) = cold_build_fixture();
        let ctx = Arc::new(crate::test_context(
            Box::new(TreeSitterProvider::new()),
            config,
        ));
        (dirs, ctx)
    }

    /// Build a successful call-graph answer and attach its pending-changes
    /// gaps the way dispatch does, with `hook` called at each stage boundary
    /// of the pending-changes capture.
    fn call_tree_answer_with_capture_hook(
        ctx: &AppContext,
        hook: impl FnMut(PendingCaptureBoundary) + 'static,
    ) -> crate::protocol::Response {
        let _hook = install_pending_capture_hook_for_test(hook);
        let (watcher, callgraph) = crate::response_finalize::query_pending(ctx, "call_tree");
        let mut response = crate::protocol::Response::success("cg", json!({"children": ["stale"]}));
        crate::response_finalize::attach_watcher_query_gap(&mut response, watcher);
        crate::response_finalize::attach_callgraph_pending_gaps(&mut response, callgraph.as_ref());
        response
    }

    fn names_pending_file(response: &crate::protocol::Response, path: &str) -> bool {
        response.data["complete"] == false
            && response.data["gaps"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|gap| gap["path"] == path)
    }

    /// The watcher thread hands a flush window's files to the dispatch
    /// channel and only then clears the window. A flush that lands while an
    /// answer is capturing what is pending must not hide the file from both
    /// reads: the window is read first, so it still names the file.
    #[test]
    fn a_flush_between_two_capture_reads_still_names_the_file() {
        let (_dirs, ctx) = cold_build_context();
        let root = std::fs::canonicalize(ctx.config().project_root.as_ref().unwrap())
            .expect("canonical root");
        ctx.set_canonical_cache_root(root.clone());
        let source = root.join("src/lib.rs");
        let (tx, rx) = crate::watcher_filter::watcher_dispatch_channel();
        *ctx.watcher_rx().lock() = Some(rx);
        let counters = ctx.watcher_counters();
        aft_engine::test_bridge::search_misc::watcher::note_unflushed_paths(&counters, [&source]);

        let mut flushed = false;
        let response = call_tree_answer_with_capture_hook(&ctx, move |boundary| {
            if boundary != PendingCaptureBoundary::AfterFlushWindow || flushed {
                return;
            }
            flushed = true;
            tx.send(WatcherDispatchEvent::Paths(vec![source.clone()]))
                .expect("dispatch the flushed file");
            aft_engine::test_bridge::search_misc::watcher::clear_unflushed_paths(&counters);
        });

        assert!(
            names_pending_file(&response, "src/lib.rs"),
            "the answer must be marked incomplete and name the flushed file: {:#}",
            response.data
        );
    }

    fn with_fake_home_env<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        let _guard = crate::test_env::process_env_lock();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", home);
            std::env::set_var("USERPROFILE", home);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn configure_request_with_params(params: serde_json::Value) -> RawRequest {
        RawRequest {
            id: "cfg".to_string(),
            command: "configure".to_string(),
            lsp_hints: None,
            session_id: None,
            params,
        }
    }

    fn user_tier(doc: serde_json::Value) -> serde_json::Value {
        json!({
            "tier": "user",
            "source": "/u/aft.jsonc",
            "doc": doc.to_string(),
        })
    }

    fn configure_context(project_root: &Path, storage_dir: &Path) -> AppContext {
        let ctx = crate::test_context(Box::new(TreeSitterProvider::new()), Config::default());
        let response = crate::commands::configure::handle_configure(
            &configure_request_with_params(json!({
                "project_root": project_root,
                "harness": "opencode",
                "storage_dir": storage_dir,
                "config": [user_tier(json!({
                    "indexes": { "trigram": true, "semantic": true, "callgraph": true },
                }))],
            })),
            &ctx,
        );
        assert!(response.success, "configure should succeed: {response:?}");
        ctx
    }

    fn inspect_snapshot(ctx: &AppContext) -> InspectSnapshot {
        InspectSnapshot::new(
            ctx.canonical_cache_root(),
            ctx.inspect_dir(),
            ctx.config(),
            ctx.symbol_cache(),
        )
    }

    #[test]
    fn home_root_callers_name_the_home_root_as_the_cause() {
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(
            source_dir.join("lib.rs"),
            "pub fn caller() { callee(); }\npub fn callee() {}\n",
        )
        .expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            let navigation = RawRequest {
                id: "home-callers".to_string(),
                command: "callers".to_string(),
                lsp_hints: None,
                session_id: None,
                params: json!({
                    "file": source_dir.join("lib.rs"),
                    "symbol": "caller",
                }),
            };
            let response = crate::commands::callers::handle_callers(&navigation, &ctx);
            assert!(!response.success);
            // HOME is an unavailable callgraph with a named cause, never a
            // retryable build.
            assert_eq!(response.data["code"], "callgraph_unavailable");
            assert_eq!(
                response.data["index"],
                json!({"callgraph": {"status": "unavailable", "reason": "home_root"}})
            );
            assert!(response.data["results"].is_null());
            assert!(response.data["message"]
                .as_str()
                .is_some_and(|message| message.contains("disabled for home roots")));
        });
    }

    #[test]
    fn home_root_gate_blocks_inspect_manager_submit_paths() {
        let home = TempDir::new().expect("home tempdir");
        let storage = TempDir::new().expect("storage tempdir");
        let source_dir = home.path().join("src");
        std::fs::create_dir_all(&source_dir).expect("source dir");
        std::fs::write(source_dir.join("lib.rs"), "pub fn one() {}\n").expect("source file");

        with_fake_home_env(home.path(), || {
            let ctx = configure_context(home.path(), storage.path());
            let snapshot = inspect_snapshot(&ctx);
            let scope = JobScope::for_project(snapshot.project_root.clone());
            let manager = ctx.inspect_manager();

            assert!(matches!(
                manager.submit_category(snapshot.clone(), InspectCategory::Metrics, scope.clone()),
                JobOutcome::Failed { .. }
            ));

            assert!(matches!(
                manager.tier2_run_with_reuse_blocking(snapshot, InspectCategory::DeadCode, scope),
                JobOutcome::Failed { .. }
            ));
            assert!(
                !manager.tier2_any_in_flight(),
                "HOME root gate must reject Tier-2 submission before any job is queued"
            );
        });
    }
}

mod status_emitter_tests {
    use crate::config::Config;
    use crate::parser::TreeSitterProvider;
    use aft_engine::test_bridge::display_data::AppContextTestExt as _;
    use std::path::PathBuf;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn status_counts_do_not_acquire_a_held_lsp_manager() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = Arc::new(crate::test_context(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(PathBuf::from("/proj")),
                ..Config::default()
            },
        ));
        ctx.update_status_bar_todos(Some(4));
        ctx.lsp().store_project_applicability(Default::default());
        ctx.lsp().diagnostics_store_mut_for_test().publish(
            crate::lsp::roots::ServerKey {
                kind: crate::lsp::registry::ServerKind::Rust,
                root: PathBuf::from("/proj"),
            },
            file.clone(),
            vec![],
        );
        ctx.lsp().certify_diagnostic_content_for_test(&file);
        let (client, mut fleet_rx) = crate::fleet_status::FleetStatusClient::dial_channel(2);
        ctx.install_fleet_status_client(Some(client));
        let mut first = String::from("first");
        let mut response = crate::protocol::Response::success("status", serde_json::json!({}));
        crate::response_finalize::finalize_tool_response(
            &mut response,
            &mut first,
            &ctx,
            "s",
            "read",
            false,
        );
        assert!(first.contains("T4"));
        assert!(first.contains("E0 W0"));
        fleet_rx
            .try_recv()
            .expect("initial fleet publish")
            .complete_unavailable();
        ctx.update_status_bar_todos(Some(9));
        let held = ctx.lsp();
        let worker_ctx = Arc::clone(&ctx);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let counts_unavailable = worker_ctx.try_status_bar_count_values().is_none();
            let push_unavailable = worker_ctx.try_build_status_snapshot().is_none();
            let mut text = String::from("file contents");
            crate::response_finalize::finalize_tool_response(
                &mut response,
                &mut text,
                &worker_ctx,
                "s",
                "read",
                false,
            );
            tx.send((text, counts_unavailable, push_unavailable))
                .unwrap();
        });
        let values = rx.recv_timeout(Duration::from_secs(5));
        drop(held);
        worker.join().unwrap();
        let (text, counts_unavailable, push_unavailable) =
            values.expect("status counts must not acquire a contended manager");
        assert!(counts_unavailable && push_unavailable);
        assert!(
            fleet_rx.try_recv().is_err(),
            "a contended publish must not clear the fleet segment"
        );
        assert_eq!(
            text, "file contents",
            "contention must leave the previous bar alone"
        );
        let mut retry = String::from("next signal");
        let mut response = crate::protocol::Response::success("retry", serde_json::json!({}));
        crate::response_finalize::finalize_tool_response(
            &mut response,
            &mut retry,
            &ctx,
            "s",
            "read",
            false,
        );
        assert!(
            retry.contains("T9"),
            "a skipped emission must not consume the change"
        );
    }
}
