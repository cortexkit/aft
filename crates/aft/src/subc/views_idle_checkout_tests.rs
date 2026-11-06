use super::*;

/// Whether `ctx`'s checkout in the data layer is bound, without binding it.
fn checkout_bound(ctx: &AppContext) -> bool {
    matches!(
        ctx.probe_data_checkout(),
        Some(crate::data_layer::surfaces::DataCheckoutProbe::Bound(_))
    )
}

#[test]
fn idle_connected_checkout_ages_by_use_blocks_in_flight_and_reopens_on_first_reply() {
    let _env = crate::test_env::hermetic_git_env_guard();

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("a.ts"),
        "export function target() { return 1; }\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&root)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "fixture"]);
    let root = ProjectRootId::from_path(&root).unwrap();
    let storage = temp.path().join("storage");
    let mut config = crate::config::Config {
        project_root: Some(root.as_path().to_path_buf()),
        storage_dir: Some(storage),
        ..Default::default()
    };
    config.idle.checkout_hours = 1;
    config.indexes.semantic = false;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx.set_canonical_cache_root(root.as_path().to_path_buf());
    ctx.set_standing_artifact_exempt(true);
    ctx.note_request();
    // Bind the checkout and let its build publish before the first grep, so
    // grep answers from the publication and the release below drops a
    // settled checkout. (A grep that ran before the publication would fall
    // back to today's search index and start a build of it, which nothing in
    // this test drains, and which blocks the release as a build in flight.)
    ctx.data_checkout().expect("the data layer binds the root");
    let started = Instant::now();
    while crate::data_layer::tools::search::DataText::ready(&ctx).is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the checkout never published its text plane"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let request: crate::protocol::RawRequest = serde_json::from_value(json!({
        "id": "idle-checkout-grep", "command": "grep", "pattern": "target"
    }))
    .unwrap();
    let first = crate::commands::grep::handle_grep(&request, &ctx);
    assert!(first.success, "{:?}", first.data);
    assert_eq!(first.data["index_status"], "Ready", "{:?}", first.data);
    assert!(checkout_bound(&ctx));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx.clone()));
    let channels = HashMap::from([(
        root.clone(),
        HashSet::from([RouteChannel {
            channel: 1,
            epoch: 1,
        }]),
    )]);
    let mut roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
    let metrics = DispatchPathMetrics::new();
    let sweep = |roots: &mut HashMap<ProjectRootId, RootMeta>, now| {
        reap_idle_roots_with_presence(
            now,
            roots,
            &HashMap::new(),
            &channels,
            &executor,
            &metrics,
            &RootPresence::default(),
        );
    };
    let used = ctx.last_request_at();
    sweep(&mut roots, used + Duration::from_secs(3599));
    assert!(checkout_bound(&ctx), "use inside the window stays resident");

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let call = executor.submit(
        root.clone(),
        crate::executor::Lane::PureRead,
        "held-call".into(),
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::protocol::Response::success("held-call", json!({}))
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let due = used + Duration::from_secs(3601);
    sweep(&mut roots, due);
    assert!(
        checkout_bound(&ctx),
        "an in-flight call keeps the connected checkout"
    );
    release_tx.send(()).unwrap();
    assert!(call.recv_timeout(Duration::from_secs(30)).unwrap().success);
    let deadline = Instant::now() + Duration::from_secs(30);
    while checkout_bound(&ctx) {
        sweep(&mut roots, due);
        assert!(
            Instant::now() < deadline,
            "the idle connected checkout was not released; blocker: {:?}",
            ctx.try_artifact_eviction_blocker(false)
        );
        std::thread::yield_now();
    }
    assert!(executor.actor_registered(&root));
    assert!(!ctx.subc_unbound_quiesced(), "the route remains connected");

    ctx.note_request();
    let reply = crate::commands::grep::handle_grep(&request, &ctx);
    assert!(
        reply.success,
        "the first reply after the release must answer: {:?}",
        reply.data
    );
    // The publication stayed on disk: the next use binds the checkout again
    // and answers from it at once, without a walk.
    assert_eq!(reply.data["index_status"], "Ready", "{:?}", reply.data);
    assert_eq!(reply.data["total_matches"], 1, "{:?}", reply.data);
    assert!(checkout_bound(&ctx));
}
