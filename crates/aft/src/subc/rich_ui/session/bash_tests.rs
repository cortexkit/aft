use super::*;
use crate::bash_background::{
    persistence::{self, BgMode, PersistedTask, ResolvedTask},
    BgTaskStatus, TaskCallKey,
};

const TASK: &str = "bash-0000000000000081";
const TASK_TWO: &str = "bash-0000000000000082";

fn seed(
    f: &Fixture,
    id: &str,
    session: &str,
    requester: &str,
    key: &str,
    running: bool,
    pty: bool,
) -> (ResolvedTask, PersistedTask) {
    let harness: crate::harness::Harness = f.policy.harness.parse().unwrap();
    let storage = f.dir.path().join("storage").join(harness.storage_segment());
    let layout = persistence::create_task_layout(&storage, session, id).unwrap();
    let mut task = crate::db::rich_ui::terminal_task_metadata_for_test(None);
    task.task_id = id.into();
    task.session_id = session.into();
    task.harness = Some(harness.storage_segment());
    task.workdir = f.policy.project_root.clone();
    task.project_root = Some(f.policy.project_root.clone());
    task.call_key = Some(TaskCallKey {
        requester: requester.into(),
        key: key.into(),
        minted: false,
    });
    task.mode = if pty { BgMode::Pty } else { BgMode::Pipes };
    if running {
        task.status = BgTaskStatus::Running;
        task.finished_at = None;
        task.duration_ms = None;
        task.exit_code = None;
        task.completion_preview = None;
        task.child_pid = Some(std::process::id());
        task.timeout_ms = Some(1000);
        task.default_hard_kill = true;
    }
    persistence::write_task_at(&layout, &task).unwrap();
    std::fs::write(&layout.paths.stdout, b"one\n").unwrap();
    std::fs::write(&layout.paths.stderr, b"two\n").unwrap();
    if pty {
        std::fs::write(&layout.paths.pty, b"\x1b[31mpty\x1b[0m").unwrap();
    }
    save_row(f, &layout, &task);
    (layout, task)
}

fn save_row(f: &Fixture, layout: &ResolvedTask, task: &PersistedTask) {
    let harness: crate::harness::Harness = f.policy.harness.parse().unwrap();
    let row = task
        .to_bash_task_row(&harness.storage_segment(), &layout.paths)
        .unwrap();
    crate::db::rich_ui::upsert_task_row_with_view(&f.ctx.db().unwrap().lock().unwrap(), &row, task)
        .unwrap();
}

async fn query(f: &Fixture, id: &str, key: &str, extra: Value) -> Value {
    let name = if id == "bash_output" {
        "bash_status"
    } else {
        id
    };
    let mut args = extra;
    args["rich_ui_op"] = format!("{id}@1").into();
    args["task"] = key.into();
    let operation = super::super::super::admission::prepare(name, &args).unwrap();
    let reply = exchange(f, name, args, "worker").await;
    assert_eq!(reply.header.ty, FrameType::Response);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    super::super::super::carrier::validate_body(Some(&operation.entry), &body).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 3, "no session sidecars");
    body
}

fn reopen(f: &Fixture) {
    f.ctx.clear_db();
    f.ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&f.dir.path().join("aft.db")).unwrap(),
    )));
}

#[tokio::test]
async fn command_views_resolve_storage_segment_and_fresh_status_after_exit() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    for kind in ["head", "worker"] {
        let f = Fixture::new(kind);
        let requester = principal(&f.policy).unwrap();
        let (layout, mut task) = seed(&f, TASK, &f.policy.session, requester, "call", true, false);
        let hard_kill = task.timeout_ms;
        let before = f.ctx.db().unwrap().lock().unwrap().total_changes();
        let body = query(&f, "bash_status", "call", json!({})).await;
        assert_eq!(
            body["structuredContent"],
            json!({"running":true, "exitCode":null}),
            "{body}"
        );
        assert_eq!(f.ctx.db().unwrap().lock().unwrap().total_changes(), before);
        assert_eq!(
            persistence::read_task_at(&layout).unwrap().timeout_ms,
            hard_kill
        );
        task.mark_terminal(BgTaskStatus::Completed, Some(3), None);
        persistence::write_task_at(&layout, &task).unwrap();
        save_row(&f, &layout, &task);
        let body = query(&f, "bash_status", "call", json!({})).await;
        assert_eq!(
            body["structuredContent"],
            json!({"running":false, "exitCode":3})
        );
        reopen(&f);
        let body = query(&f, "bash_status", "call", json!({})).await;
        assert_eq!(body["structuredContent"]["exitCode"], 3);
    }
}

#[tokio::test]
async fn command_output_cursor_survives_reopen_and_uses_database_key() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = Fixture::new("worker");
    let (layout, task) = seed(
        &f,
        TASK,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "call",
        false,
        false,
    );
    seed(
        &f,
        TASK_TWO,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "second",
        false,
        false,
    );
    let first = query(&f, "bash_output", "call", json!({"limit":1})).await;
    assert_eq!(
        first["structuredContent"]["items"],
        json!([{"stream":"stdout","text":"one\n","bytes":4,"continued":false,"firstAvailableOffset":null,"prefixDropped":true}])
    );
    let cursor = first["structuredContent"]["next"].clone();
    let key: Vec<u8> = f
        .ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .query_row(
            "SELECT key FROM rich_ui_cursor_key WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    reopen(&f);
    let second = query(
        &f,
        "bash_output",
        "call",
        json!({"limit":1,"cursor":cursor}),
    )
    .await;
    assert_eq!(
        second["structuredContent"]["items"],
        json!([{"stream":"stderr","text":"two\n","bytes":4,"continued":false,"firstAvailableOffset":null,"prefixDropped":true}])
    );
    assert!(second["structuredContent"]["next"].is_null());
    assert_eq!(
        persistence::read_task_at(&layout).unwrap().timeout_ms,
        task.timeout_ms
    );
    let replay = query(&f, "bash_output", "second", json!({"cursor":cursor})).await;
    assert_eq!(replay["structuredContent"]["code"], "malformed_cursor");
    let mut tampered = cursor.as_str().unwrap().as_bytes().to_vec();
    tampered[5] = if tampered[5] == b'A' { b'B' } else { b'A' };
    let tampered = query(
        &f,
        "bash_output",
        "call",
        json!({"cursor":String::from_utf8(tampered).unwrap()}),
    )
    .await;
    assert_eq!(tampered["structuredContent"]["code"], "malformed_cursor");
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("UPDATE rich_ui_cursor_key SET key = ?1", [vec![0u8; 32]])
        .unwrap();
    let changed = query(&f, "bash_output", "call", json!({"cursor":cursor})).await;
    assert_eq!(changed["structuredContent"]["code"], "malformed_cursor");
    f.ctx
        .db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("UPDATE rich_ui_cursor_key SET key = ?1", [key])
        .unwrap();
    assert_eq!(
        query(&f, "bash_output", "call", json!({"cursor":cursor})).await,
        second
    );
    f.ctx.clear_db();
    let unavailable = query(&f, "bash_output", "call", json!({"cursor":cursor})).await;
    assert_eq!(
        unavailable["structuredContent"]["code"],
        "database_unavailable"
    );
}

#[tokio::test]
async fn command_views_refuse_foreign_minted_duplicates_and_erased_rows() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    for fault in ["foreign", "minted", "duplicate", "erased", "worker-owned"] {
        let f = Fixture::new("head");
        let requester = principal(&f.policy).unwrap();
        let (layout, mut task) = seed(
            &f,
            TASK,
            if fault == "worker-owned" {
                "worker-session"
            } else {
                &f.policy.session
            },
            if fault == "foreign" {
                "reserved:other"
            } else {
                requester
            },
            "call",
            false,
            false,
        );
        if fault == "minted" {
            task.call_key.as_mut().unwrap().minted = true;
            persistence::write_task_at(&layout, &task).unwrap();
            save_row(&f, &layout, &task);
        }
        if fault == "duplicate" {
            seed(
                &f,
                TASK_TWO,
                &f.policy.session,
                requester,
                "call",
                false,
                false,
            );
        }
        if fault == "erased" {
            crate::db::bash_tasks::delete_bash_task(
                &f.ctx.db().unwrap().lock().unwrap(),
                task.harness.as_deref().unwrap(),
                &task.session_id,
                TASK,
            )
            .unwrap();
        }
        for id in ["bash_status", "bash_output"] {
            assert_eq!(
                query(&f, id, "call", json!({})).await["structuredContent"]["code"],
                "task_not_found",
                "{fault}/{id}"
            );
        }
    }
}

#[tokio::test]
async fn command_output_pty_has_no_stderr_and_keeps_escape_bytes() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = Fixture::new("head");
    seed(
        &f,
        TASK,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "call",
        false,
        true,
    );
    let body = query(&f, "bash_output", "call", json!({})).await;
    assert_eq!(
        body["structuredContent"],
        json!({"items":[{"stream":"pty","text":"\x1b[31mpty\x1b[0m","bytes":12,"continued":false,"firstAvailableOffset":null,"prefixDropped":true}],"next":null})
    );
}

#[tokio::test]
async fn command_output_reports_unknown_origin_for_trimmed_and_legacy_artifacts() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = Fixture::new("head");
    let (layout, _) = seed(
        &f,
        TASK,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "call",
        true,
        false,
    );
    let live = query(&f, "bash_output", "call", json!({"limit":1})).await;
    assert_eq!(
        live["structuredContent"]["items"][0]["firstAvailableOffset"],
        0
    );
    assert_eq!(
        live["structuredContent"]["items"][0]["prefixDropped"],
        false
    );
    let mut task = persistence::read_task_at(&layout).unwrap();
    task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    persistence::write_task_at(&layout, &task).unwrap();
    save_row(&f, &layout, &task);
    let dropped = persistence::replace_artifact_with_tail(
        &layout.paths,
        persistence::TaskArtifact::Stdout,
        2,
    )
    .unwrap();
    assert_eq!(dropped, 2);
    let trimmed = query(&f, "bash_output", "call", json!({})).await;
    assert_eq!(trimmed["structuredContent"]["items"][0]["text"], "e\n");
    for item in trimmed["structuredContent"]["items"].as_array().unwrap() {
        assert_eq!(item["firstAvailableOffset"], Value::Null);
        assert_eq!(item["prefixDropped"], true);
    }
}

#[tokio::test]
async fn command_status_observation_leaves_completion_for_agents_next_call() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = Fixture::new("head");
    let (layout, _) = seed(
        &f,
        TASK,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "call",
        false,
        false,
    );
    let harness: crate::harness::Harness = f.policy.harness.parse().unwrap();
    f.ctx.bash_background().set_harness(harness.clone());
    f.ctx.bash_background().set_db_pool(f.ctx.db().unwrap());
    f.ctx
        .bash_background()
        .replay_session(
            &f.dir.path().join("storage").join(harness.storage_segment()),
            &f.policy.session,
        )
        .unwrap();
    assert!(f
        .ctx
        .bash_background()
        .has_completions_for_session(Some(&f.policy.session)));
    let observed = query(&f, "bash_status", "call", json!({})).await;
    assert_eq!(observed["structuredContent"]["running"], false);
    assert!(
        !persistence::read_task_at(&layout)
            .unwrap()
            .completion_delivered
    );
    let mut ordinary = Response::success("ordinary", json!({}));
    let mut text = String::new();
    crate::response_finalize::finalize_tool_response_with_visibility(
        &mut ordinary,
        &mut text,
        &f.ctx,
        &f.policy.session,
        "read",
        true,
        true,
    );
    assert_eq!(ordinary.data["bg_completions"][0]["task_id"], TASK);
}

#[tokio::test]
async fn command_status_frame_keys_replay_only_the_carrier_frame_not_the_viewed_call() {
    let _storage_env = crate::test_env::without_storage_override();
    let _manifest =
        super::super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let f = Fixture::new("head");
    let (layout, mut task) = seed(
        &f,
        TASK,
        &f.policy.session,
        principal(&f.policy).unwrap(),
        "viewed",
        true,
        false,
    );
    let args = json!({"rich_ui_op":"bash_status@1", "task":"viewed"});
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(f.carrier.root.clone(), f.ctx.clone()));
    let routes = HashMap::from([
        (route_key(41, 9), f.carrier.clone()),
        (route_key(43, 9), f.policy.clone()),
    ]);
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (edge, mut ready) = tool_provider::LedgerEdge::new();
    let (writer, mut replies) = mpsc::channel(10);
    let (proxy, proxy_task) = edge.proxy(writer, executor.clone(), metrics.clone());
    let mut first_bytes = Vec::new();
    for (key, corr, replay) in [
        ("frame-1", 71, false),
        ("frame-1", 72, true),
        ("frame-2", 73, false),
    ] {
        let request = decoded_request("bash_status", args.clone(), "worker", Some(key), corr);
        assert!(edge
            .defer(request, &routes, &executor, &proxy, &metrics)
            .is_ok());
        if !replay {
            let admitted = tokio::time::timeout(Duration::from_secs(5), ready.recv())
                .await
                .unwrap()
                .unwrap();
            let outgoing = tool_provider::route_tests::exchange_decoded_with_capabilities(
                admitted,
                &routes,
                &executor,
                &metrics,
                |_request, _ctx| panic!("status must use its observation-only handler"),
                PROOFS,
            )
            .await;
            proxy.send(outgoing).await.unwrap();
        }
        let mut reply = tokio::time::timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        reply.project_tool_result().unwrap();
        assert_eq!(reply.header.corr, corr);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        if corr == 71 {
            assert_eq!(body["structuredContent"]["running"], true);
            first_bytes = reply.body.clone();
            task.mark_terminal(BgTaskStatus::Completed, Some(5), None);
            persistence::write_task_at(&layout, &task).unwrap();
            save_row(&f, &layout, &task);
        } else if replay {
            assert_eq!(reply.body, first_bytes);
            assert!(
                ready.try_recv().is_err(),
                "a settled retry must not dispatch again"
            );
        } else {
            assert_eq!(
                body["structuredContent"],
                json!({"running":false,"exitCode":5})
            );
        }
    }
    proxy_task.abort();
}
