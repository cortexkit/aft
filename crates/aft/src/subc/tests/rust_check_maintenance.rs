use super::*;
use crate::lsp::client::FLYCHECK_PUBLISH_SETTLE;
use crate::lsp::manager::{
    regression_tests::{
        drain_flycheck_end_for_maintenance_test, finish_flycheck_for_maintenance_test,
        overflow_flycheck_inbox_for_maintenance_test,
    },
    RetryClock,
};

#[tokio::test]
async fn idle_lsp_maintenance_persists_rust_check_after_publish_settle() {
    assert_idle_flycheck_persistence(false).await;
}

#[tokio::test]
async fn idle_lsp_maintenance_persists_only_the_replacement_rust_check_after_overflow() {
    assert_idle_flycheck_persistence(true).await;
}

async fn assert_idle_flycheck_persistence(overflow: bool) {
    let (_dir, root) = test_root("idle-flycheck-settle");
    let storage = tempfile::tempdir().unwrap();
    std::fs::write(
        root.as_path().join("Cargo.toml"),
        "[package]\nname = \"settle-test\"\nversion = \"0.1.0\"\nbuild = false\n",
    )
    .unwrap();
    std::fs::write(root.as_path().join("lib.rs"), "pub fn ready() {}\n").unwrap();
    let ctx = Arc::new(AppContext::new(
        crate::context::default_language_provider_factory(),
        crate::config::Config::default(),
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    ctx.mark_subc_bound();
    let clock = Arc::new(parking_lot::Mutex::new(Instant::now()));
    let (key, settle_at) = {
        let mut lsp = ctx.lsp();
        let read_clock = Arc::clone(&clock);
        lsp.set_retry_clock(RetryClock::from_fn(move || *read_clock.lock()));
        let settle_at =
            drain_flycheck_end_for_maintenance_test(&mut lsp, root.as_path(), storage.path());
        assert_eq!(lsp.pending_event_count_for_test(), 0);
        settle_at
    };
    let persisted_record = || {
        std::fs::read_dir(storage.path().join("rust-completed-checks"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "json"))
    };
    let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
    let probe = |roots: &mut _| {
        due_maintenance_jobs(
            roots,
            Some(&executor),
            &BgSubsBySession::new(),
            &BgWakePending::new(),
            usize::MAX,
            &HashSet::new(),
        )
        .0
    };
    let end_at = settle_at - FLYCHECK_PUBLISH_SETTLE;
    *clock.lock() = end_at + DRAIN_TICK_PERIOD;
    assert!(probe(&mut live_roots).is_empty());
    assert!(persisted_record().is_none());

    if overflow {
        let replacement_order = overflow_flycheck_inbox_for_maintenance_test(
            &mut ctx.lsp(),
            &key,
            &key.root.join("lib.rs"),
        );
        *clock.lock() = settle_at;
        assert!(probe(&mut live_roots).is_empty());
        // Even an explicit drain must not persist the invalidated check.
        ctx.lsp().drain_events();
        assert!(
            persisted_record().is_none(),
            "the invalidated check was saved"
        );

        let mut lsp = ctx.lsp();
        finish_flycheck_for_maintenance_test(&mut lsp, &key, replacement_order);
        assert_eq!(lsp.pending_event_count_for_test(), 0);
        *clock.lock() = Instant::now() + FLYCHECK_PUBLISH_SETTLE;
    } else {
        *clock.lock() = end_at + DRAIN_TICK_PERIOD * 2;
    }

    // The next tick crosses the settle deadline. No new LSP event or tool call
    // may be responsible for admitting this maintenance job.
    assert_eq!(
        probe(&mut live_roots),
        vec![(root.clone(), MaintenanceDrainKind::Lsp)],
        "the idle daemon must schedule certification when the settle deadline is due",
    );
    let (tx, mut rx) = mpsc::channel(1);
    submit_maintenance_job(
        &executor,
        root.clone(),
        MaintenanceDrainKind::Lsp,
        Vec::new(),
        &tx,
        &Arc::new(DispatchPathMetrics::new()),
    );
    let completion = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(completion.response.success, "{:?}", completion.response);
    assert_eq!(completion.requeue_kind, None);
    note_maintenance_completion(live_roots.get_mut(&root).unwrap(), None, false, true);
    let record = persisted_record().expect("idle maintenance must write the completed check");
    let record: Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(record["record"]["root"], root.as_path().to_str().unwrap());
    assert_eq!(record["record"]["diagnostics"], serde_json::json!({}));
    assert!(
        probe(&mut live_roots).is_empty(),
        "a settled check must not keep maintenance active"
    );
}
