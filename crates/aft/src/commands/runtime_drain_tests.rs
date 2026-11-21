use crate::config::Config;
use crate::context::{default_language_provider_factory, AppContext};
use crate::runtime_drain::{drain_inspect_events, drain_watcher_events_bounded};
use crate::watcher_filter::{RescanReason, WatcherDispatchEvent};
use aft_engine::test_bridge::display_data::watcher_context_with;
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn inspect_failure_drain_does_not_delay_unrelated_read() {
    use crate::inspect::job::{InspectCategory, JobOutcome};
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::write(root.join("hello.txt"), "hello from read\n").unwrap();
    let (ctx, _) = watcher_context_with(&root, crate::test_context);
    let _writer = aft_engine::test_bridge::search_misc::foreign_writer(
        &root,
        &ctx.inspect_dir(),
        false,
        "commands::read::runtime_drain_tests::foreign_writer_process",
    );
    let request = crate::protocol::RawRequest {
        id: "unrelated-read".into(),
        command: "read".into(),
        params: serde_json::json!({"file": root.join("hello.txt")}),
        session_id: None,
        lsp_hints: None,
    };
    assert!(crate::commands::read::handle_read(&request, &ctx).success);
    for _ in 0..3 {
        aft_engine::test_bridge::search_misc::record_tier2_attempt_outcome_for_test(
            &ctx.inspect_manager(),
            InspectCategory::Duplicates,
            JobOutcome::Failed {
                message: "failed to open inspect cache: writer_lease_timeout: inspect writer lease deadline elapsed".into(),
            },
        );
        let start = Instant::now();
        drain_inspect_events(&ctx);
        let response = crate::commands::read::handle_read(&request, &ctx);
        assert!(response.success, "{response:?}");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "inspect status blocked an unrelated read for {:?}",
            start.elapsed()
        );
        assert_eq!(
            ctx.inspect_manager()
                .tier2_builder_state(InspectCategory::Duplicates),
            crate::inspect::InspectBuilderState::WaitingForCacheWriterLease
        );
    }
}

#[test]
fn foreign_writer_process() {
    aft_engine::test_bridge::search_misc::foreign_writer_process();
}

/// Waits up to 60 s for `ctx`'s checkout in the data layer to publish
/// both its text search index and its call graph, with no recorded edit
/// pending. The text index is published first, and the call graph's
/// publication is a new one that changes the view's text generation, so
/// a test counting the reads of one text generation waits for both.
fn wait_for_data_layer_settled(ctx: &AppContext) {
    use crate::data_layer::surfaces::report;
    use aft_engine::test_bridge::display_data::surfaces::READY;
    let started = Instant::now();
    loop {
        let report = report(ctx).expect("the data layer is on");
        if report.text.status == READY
            && report.callgraph().status == READY
            && report.pending_edits() == 0
        {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the data layer never settled: {report:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A configured context over `root`, with the data layer on for the
/// calling thread and its storage folder (where the data layer keeps its
/// index store) at `storage`.
fn data_layer_context(root: &Path, storage: &Path) -> AppContext {
    let ctx = crate::test_context(
        default_language_provider_factory(),
        Config {
            project_root: Some(root.to_path_buf()),
            storage_dir: Some(storage.to_path_buf()),
            ..Config::default()
        },
    );
    ctx.set_canonical_cache_root(root.to_path_buf());
    ctx
}

#[test]
fn exact_memo_is_retired_by_recorded_edits_but_keeps_warm_hits() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap();
    let seed = root.join("seed.rs");
    let edited = root.join("edited.rs");
    std::fs::write(&seed, "// quartz meadow beacon\n").unwrap();
    std::fs::write(&edited, "// unrelated\n").unwrap();
    let ctx = data_layer_context(&root, storage.path());
    *ctx.semantic_index_status().write().unwrap() = crate::context::SemanticIndexStatus::Disabled;
    ctx.data_checkout().expect("the data layer binds the root");
    wait_for_data_layer_settled(&ctx);
    let request = serde_json::from_value(serde_json::json!({
        "id": "exact-memo-publication",
        "command": "semantic_search",
        "query": "quartz meadow beacon",
        "hint": "literal",
        "top_k": 10,
    }))
    .unwrap();
    let search = || {
        let response = serde_json::to_value(
            crate::commands::semantic_search::handle_semantic_search(&request, &ctx),
        )
        .unwrap();
        assert_eq!(response["success"], true, "{response}");
        response
    };
    // A repeated query is answered from the memo: one verification.
    assert_eq!(search()["result_count"], 1);
    assert_eq!(search()["result_count"], 1);
    assert_eq!(ctx.search_exact_memo().verifier_call_count(), 1);

    // An edit recorded as the watcher records it moves the view's text
    // generation on, so the memo's answer for the old text is retired
    // and the new one is verified once.
    std::fs::write(&edited, "// quartz meadow beacon\n").unwrap();
    ctx.data_note_paths(std::slice::from_ref(&edited));
    assert_eq!(search()["result_count"], 2);
    assert_eq!(search()["result_count"], 2);
    assert_eq!(ctx.search_exact_memo().verifier_call_count(), 2);

    std::fs::write(&seed, "// retired\n").unwrap();
    ctx.data_note_paths(std::slice::from_ref(&seed));
    assert_eq!(search()["result_count"], 1);
    assert_eq!(search()["result_count"], 1);
    assert_eq!(ctx.search_exact_memo().verifier_call_count(), 3);
}

/// A watcher overflow drops the changed paths; the rescan that follows
/// must reach the data layer, or a file changed only during the overflow
/// stays out of grep until the root is bound again.
#[test]
fn a_watcher_rescan_reaches_the_data_layer() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(project.path()).unwrap();
    std::fs::write(root.join("seed.rs"), "fn seed_marker() {}\n").unwrap();
    let ctx = data_layer_context(&root, storage.path());
    let (tx, rx) = crossbeam_channel::unbounded();
    *ctx.watcher_rx().lock() = Some(rx);
    ctx.data_checkout().expect("the data layer binds the root");
    wait_for_data_layer_settled(&ctx);
    let grep = |pattern: &str| {
        let request: crate::protocol::RawRequest = serde_json::from_value(serde_json::json!({
            "id": "rescan-grep",
            "command": "grep",
            "pattern": pattern,
        }))
        .unwrap();
        let response = crate::commands::grep::handle_grep(&request, &ctx);
        assert!(response.success, "{:?}", response.data);
        response.data["total_matches"].clone()
    };
    assert_eq!(grep("seed_marker"), 1, "grep reads the data layer's view");

    std::fs::write(
        root.join("overflow_only.rs"),
        "fn overflow_rescan_marker() {}\n",
    )
    .unwrap();
    ctx.watcher_counters().note_overflow_ignore_relevance(false);
    tx.send(WatcherDispatchEvent::RescanRequired(RescanReason::Unknown))
        .unwrap();
    for _ in 0..64 {
        if !drain_watcher_events_bounded(&ctx, 16).has_more {
            break;
        }
    }
    assert_eq!(
        ctx.watcher_counters().snapshot().rescans_unknown_total(),
        1,
        "the rescan ran"
    );
    // The checkout compares its files with its state in the background,
    // so grep finds the file once that comparison has recorded it.
    let started = Instant::now();
    loop {
        let found = grep("overflow_rescan_marker");
        if found == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "a file changed during the overflow must reach grep after the rescan: {found}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
use aft_engine::test_bridge::display_data::AppContextTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::display_data::DataStoreReportTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::search_misc::InspectManagerTestExt as _;
#[cfg(test)]
use aft_engine::test_bridge::search_misc::WatcherCountersTestExt as _;

use aft_engine::test_bridge::search_misc::WatcherCountersSnapshotTestExt as _;
