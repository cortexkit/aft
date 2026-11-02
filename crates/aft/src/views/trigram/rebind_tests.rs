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
    let second = session();
    measure(&second, "second_session_bind", "bind");
    measure(&second, "second_session_first_query", "search");
}
