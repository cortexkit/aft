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
        assert!(false);
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
    super::super::verification::take_policy_input_visits_for_test();
    // After the last connection detaches, the old view's configure-generation
    // check refuses queries. The root actor and watcher survive for the next session.
    ctx.mark_subc_unbound();
    ctx.mark_subc_bound();
    snapshot(&ctx, "bind");
    snapshot(&ctx, "search");
    let work = take_measurements();
    assert_eq!(work.reconciles, 0, "unchanged second session: {work:?}");
    assert_eq!(work.binds, 0);
    assert_eq!(
        super::super::verification::take_policy_input_visits_for_test(),
        (0, 0)
    );
    assert!(Arc::ptr_eq(&source, &ctx.lexical_source_for_test()));
    ctx.mark_subc_unbound();
    ctx.mark_subc_bound();
    snapshot(&ctx, "search");
    assert_eq!(
        take_measurements().reconciles,
        0,
        "a query may be the first bind"
    );
}

struct Fixture {
    root: PathBuf,
    ctx: crate::context::AppContext,
    watcher: Option<crate::watcher_backend::subscription::Subscription>,
}

fn fixture(test: impl FnOnce(Fixture)) {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let root = base.join("project");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.rs"), "fn first_needle() {}\n").unwrap();
    let _attributes = super::super::verification::known_attribute_sources_for_test(&root);
    let watcher = Some(crate::watcher_backend::subscription::start(
        &root,
        crate::watcher_backend::InitialCoverage::Complete,
    ));
    let ctx = context(&root, &base.join("storage"));
    test(Fixture { root, ctx, watcher });
}

impl Fixture {
    fn bind(&self) {
        snapshot(&self.ctx, "bind");
        assert!(self
            .ctx
            .lexical_source_for_test()
            .steady_revision()
            .is_some());
        super::super::first_load::rebind_tests::take_measurements();
    }

    fn rebind(&self) -> crate::search_index::SearchIndexSnapshot {
        self.ctx.mark_subc_unbound();
        self.ctx.mark_subc_bound();
        let result = snapshot(&self.ctx, "bind");
        let work = super::super::first_load::rebind_tests::take_measurements();
        assert!(
            work.reconciles > 0,
            "changed checkout must reconcile: {work:?}"
        );
        result
    }

    fn matches(&self, snapshot: &crate::search_index::SearchIndexSnapshot, needle: &str) -> usize {
        snapshot
            .grep(needle, true, &[], &[], &self.root, 10)
            .unwrap()
            .matches
            .len()
    }

    fn git(&self, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(&self.root)
            .args(["-c", "user.name=Test", "-c", "user.email=test@invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn lexical_rebind_recorded_change_reverifies_even_if_bytes_are_unchanged() {
    fixture(|f| {
        f.bind();
        f.ctx.record_checkout_watcher_change(&f.root.join("a.rs"));
        let result = f.rebind();
        assert_eq!(f.matches(&result, "first_needle"), 1);
    });
}

#[test]
fn lexical_rebind_watcher_gap_finds_an_unreported_edit() {
    fixture(|f| {
        f.bind();
        fs::write(f.root.join("a.rs"), "fn edited_needle() {}\n").unwrap();
        crate::watcher_backend::subscription::gap(&f.root);
        let result = f.rebind();
        assert_eq!(f.matches(&result, "first_needle"), 0);
        assert_eq!(f.matches(&result, "edited_needle"), 1);
    });
}

#[test]
fn lexical_rebind_stopped_or_restarted_subscription_cannot_reuse_verification() {
    for restart in [false, true] {
        fixture(|mut f| {
            f.bind();
            drop(f.watcher.take());
            fs::write(f.root.join("b.rs"), "fn added_needle() {}\n").unwrap();
            if restart {
                f.watcher = Some(crate::watcher_backend::subscription::start(
                    &f.root,
                    crate::watcher_backend::InitialCoverage::Complete,
                ));
            }
            let result = f.rebind();
            assert_eq!(f.matches(&result, "added_needle"), 1);
        });
    }
}

#[test]
fn lexical_rebind_pending_write_refuses_reuse_until_it_is_reconciled() {
    fixture(|f| {
        f.bind();
        // Active-write accounting belongs to the root's intent registration;
        // the lexical driver also receives the before/after notifications.
        let cut = f.ctx.lexical_source_for_test().acquire().unwrap();
        let delta = Arc::new(std::sync::Mutex::new(
            super::super::snapshot::LiveDelta::new(cut.generation().clone()),
        ));
        super::super::intent::register(&f.root, &delta);
        let path = f.root.join("a.rs");
        let write = crate::write_intent::record_paths([path.as_path()]);
        assert!(super::super::intent::active(&f.root));
        fs::write(f.root.join("a.rs"), "fn written_needle() {}\n").unwrap();
        f.ctx.mark_subc_unbound();
        f.ctx.mark_subc_bound();
        let error = f
            .ctx
            .lexical_view_snapshot("bind")
            .unwrap()
            .err()
            .expect("active write refused");
        assert!(
            error.contains(super::super::first_load::CHECKOUT_WRITE_ACTIVE),
            "{error}"
        );
        drop(write);
        assert!(!super::super::intent::active(&f.root));
        assert!(f.ctx.lexical_source_for_test().steady_revision().is_none());
        super::super::first_load::rebind_tests::take_measurements();
        f.ctx.view_load_retry_due_for_test();
        let result = f.rebind();
        assert_eq!(f.matches(&result, "written_needle"), 1);
        assert_eq!(f.matches(&result, "first_needle"), 0);
    });
}

#[test]
fn lexical_rebind_unreported_git_index_change_reverifies() {
    fixture(|f| {
        f.git(&["init", "-q"]);
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "initial"]);
        f.bind();
        f.git(&["update-index", "--chmod=+x", "a.rs"]);
        let result = f.rebind();
        assert_eq!(f.matches(&result, "first_needle"), 1);
    });
}

#[test]
fn lexical_rebind_unreported_head_tree_change_reverifies() {
    fixture(|f| {
        f.git(&["init", "-q"]);
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "initial"]);
        fs::write(f.root.join("b.rs"), "fn second_needle() {}\n").unwrap();
        f.git(&["add", "."]);
        f.git(&["commit", "-qm", "second"]);
        f.bind();
        // Move HEAD without changing the index or working files, and without
        // delivering a watcher event for Git metadata.
        f.git(&["update-ref", "HEAD", "HEAD^"]);
        let result = f.rebind();
        assert_eq!(f.matches(&result, "second_needle"), 1);
    });
}

#[test]
fn lexical_rebind_unreported_ignore_change_replaces_membership() {
    for name in [
        ".gitignore",
        ".ignore",
        ".aftignore",
        "nested/.gitignore",
        ".git/info/exclude",
    ] {
        fixture(|f| {
            f.git(&["init", "-q"]);
            fs::create_dir_all(f.root.join("nested")).unwrap();
            fs::write(f.root.join("nested/a.rs"), "fn nested_needle() {}\n").unwrap();
            f.bind();
            fs::write(f.root.join(name), "a.rs\n").unwrap();
            let result = f.rebind();
            assert_eq!(f.matches(&result, "nested_needle"), 0, "{name}");
            assert_eq!(
                f.matches(&result, "first_needle"),
                usize::from(name == "nested/.gitignore"),
                "{name}"
            );
        });
    }
}

#[test]
fn lexical_rebind_reported_new_directory_discovers_its_policy() {
    fixture(|f| {
        f.bind();
        fs::create_dir(f.root.join("nested")).unwrap();
        fs::write(f.root.join("nested/.ignore"), "hidden.rs\n").unwrap();
        fs::write(f.root.join("nested/hidden.rs"), "fn hidden_needle() {}\n").unwrap();
        fs::write(f.root.join("nested/visible.rs"), "fn visible_needle() {}\n").unwrap();
        f.ctx.record_checkout_watcher_change(&f.root.join("nested"));
        let result = f.rebind();
        assert_eq!(f.matches(&result, "hidden_needle"), 0);
        assert_eq!(f.matches(&result, "visible_needle"), 1);
    });
}
