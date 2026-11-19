use super::*;

async fn wait_for_start(root: &Path, corr: u64) {
    let marker = root.join(format!("started-{corr}"));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("shell {corr} never started"));
}

fn shell(corr: u64, timeout: u64) -> Value {
    let command =
        format!("echo output-{corr}; touch started-{corr}; sleep 60; touch survived-{corr}");
    tool(
        "bash",
        json!({"command":command, "timeout":timeout, "background":false, "pty":false}),
        corr,
    )
}

async fn yield_call(peer: &mut Peer, channel: u16, corr: u64, reason: &str) {
    let answer = reply(peer.call(channel, corr + 1000, json!({"name":"tool.yield", "arguments":{"call_key":format!("key-{corr}"), "reason":reason}})).await);
    assert_eq!(answer["answer"], "accepted", "{answer}");
    let hold = reply(peer.receive(channel, corr).await);
    assert_eq!(hold["held"]["reason"], "running", "{hold}");
    assert_eq!(hold["held"]["yielded"], true, "{hold}");
    assert!(hold["held"].get("task_id").is_none(), "{hold}");
}

async fn late(peer: &mut Peer, channel: u16, corr: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        for attempt in 0.. {
            let feed = reply(peer.call(channel, corr + 2000 + attempt, feed()).await);
            if let Some(entry) = feed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["call_key"] == format!("key-{corr}"))
            {
                assert_eq!(entry["kind"], "result", "{entry}");
                assert_eq!(entry["result"]["isError"], true, "{entry}");
                assert!(
                    text(&entry["result"]).contains(&format!("output-{corr}")),
                    "{entry}"
                );
                return entry["result"].clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        unreachable!()
    })
    .await
    .unwrap_or_else(|_| panic!("no late result for shell {corr}"))
}

#[test]
fn remote_shell_wire_holds_and_settles_after_scope_end_revoke_and_timeout() {
    // Drive the full module loop on the same stack as the production runtime.
    with_module_runtime_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Box::pin(exercise_shell_lifetime()))
    })
    .unwrap();
}

async fn exercise_shell_lifetime() {
    let _env = crate::test_env::process_env_lock();
    let _transport = RestoreTransport;
    let (opened, mut cards) = mpsc::unbounded_channel();
    consent::install_test_transport(Some(Arc::new(Cards {
        opened,
        answers: StdMutex::default(),
        count: AtomicU64::new(0),
    })));
    let storage = tempfile::tempdir().unwrap();
    let (_project, root) = test_root("remote-shell-on-b");
    let folder = root.as_path().to_str().unwrap();
    let launcher = storage.path().join("mock-launcher");
    // The native integration test exercises the real launcher. This fixture
    // isolates the wire and process-supervision paths from kernel availability.
    crate::executable_cache::write_executable(
        &launcher,
        b"#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec 3>&-\nexec \"$@\"\n",
    )
    .unwrap();
    let app = App::default_shared();
    let db = app.open_db(&storage.path().join("aft.db")).unwrap();
    let save = |pairing: &str| {
        grants::save(
            &db.lock().unwrap(),
            &grants::Key {
                pairing_id: pairing.into(),
                caller_id: "remote-agent".into(),
                project_id: "B-project".into(),
                root_kind: "label",
                root_key: "B-root".into(),
            },
            folder,
            Mode::ReadWrite,
            "Remote on B",
            "acc_seed",
        )
        .unwrap()
    };
    let agent_grant = save("agent");
    let _run_grant = save("run");
    let entries = registry(root.as_path());
    let executor = Arc::new(Executor::new());
    let (daemon, stream) = tokio::io::duplex(1024 * 1024);
    let mut peer = Peer {
        daemon,
        ended: HashSet::new(),
        probes: HashSet::new(),
        waiting: vec![],
    };
    let (read, write) = tokio::io::split(stream);
    let connection = storage.path().join("connection.json");
    let module = remote_bash::TEST_LAUNCHER.scope(
        launcher,
        scope_describe::remote::TEST_POLL_INTERVAL.scope(
            Duration::from_millis(10),
            TEST_REGISTRY.scope(
                Arc::new(move || entries.clone()),
                run_module_loop(
                    read,
                    write,
                    &connection,
                    app,
                    executor.clone(),
                    edit_dispatch,
                    Some(bind_config_without_indexes(storage.path())),
                    false,
                    usize::MAX,
                    None,
                    storage.path(),
                    QuiescedLsp::ReapInBackground,
                    None,
                    None,
                    None,
                ),
            ),
        ),
    );
    let exercise = async {
        hello(
            &mut peer.daemon,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        )
        .await;
        let mut local = bind(90, "opencode", Value::Null, root.as_path(), "tool_provider");
        local["principal"] = json!({"kind":"direct"});
        reply(peer.call(0, 50, local).await);
        let mut operator = bind(
            91,
            "cingulate",
            Value::Null,
            root.as_path(),
            "management_surface",
        );
        operator["principal"] = json!({"kind":"reserved", "module_id":"cingulate"});
        reply(peer.call(0, 49, operator).await);
        let ctx = executor.actor_context(&root).unwrap();
        ctx.update_config(|cfg| cfg.github.shim = false);
        let mut agent = remote_bind(1, "agent", &[HELD, RESULT, YIELD]);
        agent["scope"]["ref"] = json!("agent-shell");
        reply(peer.call(0, 51, agent.clone()).await);
        let mut run = remote_bind(2, "run", &[HELD, RESULT, YIELD]);
        run["scope"]["ref"] = json!("run-shell");
        run["scope"]["attributes"]["federated"]["remote_claim"] =
            json!({"kind":"run", "agent_id":"remote-agent", "run_id":"run-remote"});
        reply(peer.call(0, 52, run).await);
        reply(
            peer.call(0, 53, worker_bind(3, "worker", "worker-shell"))
                .await,
        );

        peer.send(1, 100, shell(100, 30_000)).await;
        peer.send(1, 101, shell(101, 30_000)).await;
        peer.send(2, 200, shell(200, 30_000)).await;
        peer.send(3, 300, shell(300, 30_000)).await;
        allow(card(&mut cards, "write").await);
        for corr in [100, 101, 200, 300] {
            wait_for_start(root.as_path(), corr).await;
        }
        yield_call(&mut peer, 1, 100, "runner_restart").await;
        yield_call(&mut peer, 1, 101, "person_message").await;
        peer.ended.insert("agent-shell".into());
        peer.ended.insert("worker-shell".into());
        let worker_result = reply(peer.receive(3, 300).await);
        assert_eq!(worker_result["isError"], true, "{worker_result}");
        assert!(
            text(&worker_result).contains("output-300"),
            "{worker_result}"
        );
        assert!(
            text(&worker_result).contains(&authorization::denied().message),
            "{worker_result}"
        );
        assert!(!peer.waiting.iter().any(|frame| frame.header.corr == 200));
        // A new route can retrieve a late result without issuing a tool call first.
        let mut rebound = remote_bind(4, "agent", &[HELD, RESULT, YIELD]);
        rebound["scope"]["ref"] = agent["scope"]["ref"].clone();
        reply(peer.call(0, 54, rebound).await);
        for corr in [100, 101] {
            let result = late(&mut peer, 4, corr).await;
            assert!(
                text(&result).contains(&authorization::denied().message),
                "{result}"
            );
        }
        let cursor = reply(peer.call(4, 54, feed()).await)["cursor"].clone();
        reply(
            peer.call(4, 55, json!({"op":"late_results.ack", "through":cursor}))
                .await,
        );
        assert_eq!(reply(peer.call(4, 56, feed()).await)["entries"], json!([]));
        peer.ended.insert("run-shell".into());
        let run_result = reply(peer.receive(2, 200).await);
        assert!(
            text(&run_result).contains(&authorization::denied().message),
            "{run_result}"
        );

        let mut agent = remote_bind(5, "agent", &[HELD, RESULT, YIELD]);
        agent["scope"]["ref"] = json!("agent-shell-new");
        reply(peer.call(0, 57, agent).await);
        let mut run = remote_bind(6, "run", &[HELD, RESULT, YIELD]);
        run["scope"]["ref"] = json!("run-shell-new");
        reply(peer.call(0, 58, run).await);
        peer.send(5, 500, shell(500, 30_000)).await;
        peer.send(6, 600, shell(600, 1800)).await;
        wait_for_start(root.as_path(), 500).await;
        wait_for_start(root.as_path(), 600).await;
        yield_call(&mut peer, 5, 500, "person_message").await;
        yield_call(&mut peer, 6, 600, "person_message").await;
        let revoked = reply(
            peer.call(
                91,
                59,
                json!({"method":"permissions.revoke", "params":{"id":agent_grant}}),
            )
            .await,
        );
        assert_eq!(revoked["result"]["stopping"], true, "{revoked}");
        let result = late(&mut peer, 5, 500).await;
        assert!(
            text(&result).contains(&authorization::denied().message),
            "{result}"
        );
        let result = late(&mut peer, 6, 600).await;
        assert!(
            !text(&result).contains(&authorization::denied().message),
            "{result}"
        );
        assert_eq!(result["errorCode"], "timeout", "{result}");

        write_frame(
            &mut peer.daemon,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 799, vec![]).unwrap(),
        )
        .await
        .unwrap();
    };
    let (end, ()) = tokio::join!(module, exercise);
    end.unwrap();
}
