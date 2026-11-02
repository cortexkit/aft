use super::*;

#[cfg(target_os = "linux")]
#[test]
#[ignore = "opt-in AFT checkout session rebind measurement"]
fn measurement_lexical_rebind_frozen_checkout() {
    use super::super::first_load::rebind_tests::take_measurements;
    let root =
        fs::canonicalize(std::env::var_os("AFT_HOT_SPOT_CORPUS").expect("frozen AFT checkout"))
            .unwrap();
    assert!(root.join("crates/aft/src/views/trigram.rs").is_file());
    let storage = tempfile::tempdir().unwrap();
    let _watcher = crate::watcher_backend::subscription::start(
        &root,
        crate::watcher_backend::InitialCoverage::Complete,
    );
    let session = || {
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(storage.path().to_owned());
        config.views.enabled = true;
        config.indexes.trigram = true;
        config.indexes.semantic = false;
        config.indexes.callgraph = false;
        let ctx = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        );
        ctx.set_canonical_cache_root(root.clone());
        ctx.mark_subc_bound();
        let family = ctx.memoized_artifact_cache_key(&root);
        crate::root_cache::configure_artifact_access(&root, &family, false);
        assert!(ctx.lexical_views_enabled());
        ctx
    };
    let measure = |ctx: &crate::context::AppContext, phase: &str, tool: &str| {
        take_measurements();
        super::super::verification::take_policy_input_visits_for_test();
        let started = std::time::Instant::now();
        let snapshot = ctx
            .lexical_view_snapshot(tool)
            .unwrap()
            .unwrap()
            .into_ready()
            .unwrap();
        let elapsed = started.elapsed();
        let work = take_measurements();
        let visits = super::super::verification::take_policy_input_visits_for_test();
        assert!(snapshot.file_entries().len() > 1000);
        assert!(!snapshot
            .grep("CheckoutLexicalSource", true, &[], &[], &root, 10)
            .unwrap()
            .matches
            .is_empty());
        eprintln!("LEXICAL_REBIND phase={phase} files={} wall_ms={:.3} verified_bind={} bind_ms={:.3} reconciled_snapshot={} reconcile_ms={:.3} policy_visits={visits:?}",
            snapshot.file_entries().len(), elapsed.as_secs_f64() * 1000.0,
            work.binds, work.bind_time.as_secs_f64() * 1000.0,
            work.reconciles, work.reconcile_time.as_secs_f64() * 1000.0);
    };
    let first = session();
    measure(&first, "initial_bind", "bind");
    measure(&first, "initial_first_query", "search");
    first.mark_subc_unbound();
    first.mark_subc_bound();
    measure(&first, "second_session_bind", "bind");
    measure(&first, "second_session_first_query", "search");
}

fn context(root: &Path, storage: &Path) -> crate::context::AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_owned());
    config.storage_dir = Some(storage.to_owned());
    config.views.enabled = true;
    config.indexes.trigram = true;
    config.indexes.semantic = false;
    config.indexes.callgraph = false;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    ctx.set_canonical_cache_root(root.to_owned());
    ctx.mark_subc_bound();
    let family = ctx.memoized_artifact_cache_key(root);
    crate::root_cache::configure_artifact_access(root, &family, false);
    ctx
}

fn snapshot(
    ctx: &crate::context::AppContext,
    tool: &str,
) -> crate::search_index::SearchIndexSnapshot {
    ctx.lexical_view_snapshot(tool)
        .unwrap()
        .unwrap()
        .into_ready()
        .unwrap()
}

#[test]
fn lexical_rebind_second_session_reuses_verified_checkout() {
    use super::super::first_load::rebind_tests::take_measurements;
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap().join("project");
    let storage = temp.path().join("storage");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn needle() {}\n").unwrap();
    let _attributes = super::super::verification::known_attribute_sources_for_test(&root);
    let _watcher = crate::watcher_backend::subscription::start(
        &root,
        crate::watcher_backend::InitialCoverage::Complete,
    );
    let ctx = context(&root, &storage);
    snapshot(&ctx, "bind");
    snapshot(&ctx, "search");
    let source = ctx.lexical_source_for_test();
    assert!(source.steady_revision().is_some());
    take_measurements();
    // After the last connection detaches, the old view's configure-generation
    // check refuses queries. The root actor and watcher survive for the next session.
    ctx.mark_subc_unbound();
    ctx.mark_subc_bound();
    snapshot(&ctx, "bind");
    snapshot(&ctx, "search");
    let work = take_measurements();
    assert_eq!(work.reconciles, 0, "unchanged second session: {work:?}");
    assert!(Arc::ptr_eq(&source, &ctx.lexical_source_for_test()));
}
