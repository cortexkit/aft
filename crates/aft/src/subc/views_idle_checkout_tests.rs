use super::*;

#[test]
fn views_idle_connected_checkout_ages_by_use_blocks_in_flight_and_reopens_on_first_reply() {
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
    config.views.enabled = true;
    config.views.idle_checkout_hours = 1;
    config.indexes.semantic = false;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_harness(crate::harness::Harness::Opencode);
    ctx.set_standing_artifact_exempt(true);
    ctx.note_request();
    let request: crate::protocol::RawRequest = serde_json::from_value(json!({
        "id": "idle-views-grep", "command": "grep", "pattern": "target"
    }))
    .unwrap();
    let first = crate::commands::grep::handle_grep(&request, &ctx);
    assert!(first.success, "{:?}", first.data);
    assert!(ctx.lexical_view_is_ready());
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
    let metrics = DispatchPathMetrics::default();
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
    assert!(
        ctx.lexical_view_is_ready(),
        "use inside the window stays resident"
    );

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let call = executor.submit(
        root.clone(),
        crate::executor::Lane::PureRead,
        "held-call".into(),
        Box::new(move |_| {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::protocol::Response::ok("held-call", json!({}))
        }),
    );
    started_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let due = used + Duration::from_secs(3601);
    sweep(&mut roots, due);
    assert!(
        ctx.lexical_view_is_ready(),
        "an in-flight call keeps the connected checkout"
    );
    release_tx.send(()).unwrap();
    assert!(call.recv_timeout(Duration::from_secs(30)).unwrap().success);
    let deadline = Instant::now() + Duration::from_secs(30);
    while ctx.lexical_view_is_ready() {
        sweep(&mut roots, due);
        assert!(
            Instant::now() < deadline,
            "idle connected views were not released"
        );
        std::thread::yield_now();
    }
    assert!(executor.actor_registered(&root));
    assert!(!ctx.subc_unbound_quiesced(), "the route remains connected");
    let paths = ctx.storage_paths();
    let family = ctx.memoized_artifact_cache_key(root.as_path());
    let clock = crate::pins::now_ms() + 3_601_000;
    let mut removed = 0;
    while removed == 0 {
        let report =
            crate::storage_retention::sweep_checkout_views_at_for_test(&paths, &family, clock);
        assert!(report.errors.is_empty(), "{report:?}");
        removed += report.removed_roots;
        assert!(
            Instant::now() < deadline,
            "checkout member stayed protected: {report:?}"
        );
        std::thread::yield_now();
    }
    ctx.note_request();
    let reply = crate::commands::grep::handle_grep(&request, &ctx);
    assert!(
        reply.success,
        "the first reply after eviction must answer: {:?}",
        reply.data
    );
    assert_eq!(reply.data["index_status"], "Ready", "{:?}", reply.data);
    assert!(ctx.lexical_view_is_ready());
}
