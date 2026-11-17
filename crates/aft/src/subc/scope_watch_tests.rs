use super::super::wire::WriterFrame;
use super::*;
use subc_client_rs::{
    ScopeRef, ScopeWatchEvent, ScopeWatchPush, ScopeWatchResponse, ScopeWatchState,
    ScopeWatchStatus,
};

#[path = "scope_watch_audit_tests.rs"]
mod audit;
#[cfg(unix)]
#[path = "scope_continuity_tests.rs"]
mod continuity;
#[path = "scope_watch_reopen_tests.rs"]
mod reopen;

fn stamp(index: usize) -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Direct,
        format!("watched-{index}"),
        1,
        subc_protocol::scope::ScopeKind::Head,
    )
}

#[cfg(unix)]
fn spawn_scoped(
    ctx: &AppContext,
    root: &ProjectRootId,
    scope: &ScopeStamp,
    command: &str,
) -> String {
    use crate::bash_background::{BashShell, HardKill, TaskSlot};
    crate::bash_background::registry::with_scope_spawn(Some(scope.clone()), || {
        ctx.bash_background().spawn_with_shell_in_slot(
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            command,
            BashShell::Bash,
            crate::bash_background::registry::resolve_posix_shell(),
            "watch-session".into(),
            root.as_path().into(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(30)),
            None,
            ctx.storage_dir(),
            TaskSlot::Background { max: 10 },
            true,
            false,
            Some(root.as_path().into()),
        )
    })
    .unwrap()
}
fn answer(
    _stamp: &ScopeStamp,
    status: subc_protocol::scope::ScopeStatus,
    epoch: u64,
) -> ModuleControlResponseToModule {
    ModuleControlResponseToModule::ScopeDescribe {
        status,
        scope_epoch: Some(epoch),
        daemon_incarnation: "scope-watch-test".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn scope_watch_end_kills_only_after_confirmation_and_live_rebind_kills_nothing() {
    use crate::bash_background::BgTaskStatus;
    use crate::db::{access_grants as grants, call_ledger as ledger};
    let (dir, root) = test_support::test_root("scope-watch-kill");
    let ctx = test_support::test_ctx();
    ctx.update_config(|config| {
        config.project_root = Some(root.as_path().into());
        config.storage_dir = Some(dir.path().join("store"));
    });
    let db = Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx.clone()));
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
    let relay = crate::gh_shim_relay::GhRelay::new(dir.path().join("unused-connection.json"));
    let speech = crate::gh_shim_speech::service::start(
        dir.path(),
        &relay,
        Arc::new(|_| Box::pin(async { Err("describe_unavailable".into()) })),
        "watch-speech-generation".into(),
        true,
    );
    let (_handle, mut ended, worker) = start_for_speech(control.clone(), executor, runs, speech);
    turns().await;
    let scope = stamp(0);
    let task = spawn_scoped(&ctx, &root, &scope, "sleep 30");
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(
        snapshot(&pending, &watch),
        1,
        "shell publication must wake selector reconciliation"
    );
    turns().await;
    assert!(frames.try_recv().is_err(), "a live snapshot needs no probe");
    let status = || {
        ctx.bash_background()
            .status(
                &task,
                "watch-session",
                Some(root.as_path()),
                Some(&ctx.storage_dir()),
                0,
            )
            .unwrap()
            .info
    };
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    assert_eq!(
        status().status,
        BgTaskStatus::Running,
        "a push alone is not an end proof"
    );
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 1),
    );
    turns().await;
    assert_eq!(status().status, BgTaskStatus::Running);
    assert!(ended.try_recv().is_err());
    push(&pending, &watch, &scope, ScopeWatchEvent::Superseded);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 2),
    );
    let rebound = request(&mut frames, "scope.watch").await;
    let body: Value = serde_json::from_slice(&rebound.body).unwrap();
    assert_eq!(body["scopes"][0]["epoch"], 2);
    snapshot(&pending, &rebound);
    turns().await;
    assert_eq!(
        status().status,
        BgTaskStatus::Running,
        "a rebind must preserve the old epoch's process"
    );
    assert!(ended.try_recv().is_err());
    let scope_key = grants::ScopeKey::from_stamp(&scope);
    let ledger_key = ledger::Key {
        carrier: "direct".into(),
        call_key: "watch-held".into(),
    };
    {
        let conn = db.lock().unwrap();
        grants::register_scope(&conn, &scope_key, None, &crate::config::Config::default()).unwrap();
        grants::insert(
            &conn,
            &scope_key,
            &crate::access_policy::AccessGrant {
                path: root.as_path().join("grant"),
                kind: crate::access_policy::GrantKind::File,
                operation: crate::access_policy::AccessOperation::Read,
            },
        )
        .unwrap();
        ledger::admit(
            &conn,
            &ledger_key,
            "digest",
            Some(&ledger::ScopeIdentity {
                owner: "direct".into(),
                scope_ref: scope.scope_ref.clone(),
                scope_epoch: 1,
            }),
            ledger::State::Prepared,
        )
        .unwrap();
    }
    let mut current = scope.clone();
    current.scope_epoch = 2;
    let coverage = scope_watch::Coverage {
        keys: HashSet::from([stamp_key(&current)]),
        all: false,
    };
    let swept = access_grants::sweep::tick_covered(db.clone(), &coverage, |_| async {
        panic!("the newer watched epoch covers the saved old-epoch grant")
    })
    .await;
    assert_eq!(swept.described, 0);
    assert_eq!(
        grants::list(&db.lock().unwrap(), &scope_key).unwrap().len(),
        1
    );
    push(&pending, &rebound, &current, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    assert_eq!(status().status, BgTaskStatus::Running);
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 2),
    );
    tokio::time::timeout(Duration::from_secs(5), ended.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status().status, BgTaskStatus::Killed);
    assert_eq!(status().status_reason.as_deref(), Some("scope_revoked"));
    let conn = db.lock().unwrap();
    assert!(grants::list(&conn, &scope_key).unwrap().is_empty());
    assert_eq!(
        ledger::get(&conn, &ledger_key).unwrap().unwrap().state,
        ledger::State::Settled
    );
    drop(conn);
    assert!(
        !crate::db::gh_speech::Store::new(dir.path().join("aft.db"))
            .scope_ended(&scope)
            .unwrap(),
        "ordinary cleanup must not mint an epoch-mismatched speech authority proof"
    );
    assert!(crate::gh_shim_speech::scope::validate_scope_identity(
        &scope,
        crate::gh_shim_speech::ScopeRole::Authority,
        &answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 2),
        "generation",
        "generation",
        0
    )
    .is_err());
    worker.abort();
    let _ = worker.await;
}

#[tokio::test]
async fn scope_watch_grant_and_held_mutations_update_selectors_without_census_timer() {
    use crate::db::{access_grants as grants, call_ledger as ledger};
    let (dir, root) = test_support::test_root("scope-watch-membership");
    let ctx = test_support::test_ctx();
    let db = Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
    let (_handle, _ended, worker) = start(control, executor, runs);
    turns().await;
    let scope = stamp(9);
    let key = grants::ScopeKey::from_stamp(&scope);
    let grant = crate::access_policy::AccessGrant {
        path: root.as_path().join("grant"),
        kind: crate::access_policy::GrantKind::File,
        operation: crate::access_policy::AccessOperation::Read,
    };
    {
        let conn = db.lock().unwrap();
        grants::register_scope(&conn, &key, None, &crate::config::Config::default()).unwrap();
        grants::insert(&conn, &key, &grant).unwrap();
    }
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &watch), 1);
    turns().await;
    grants::revoke(&db.lock().unwrap(), &key, &grant.path).unwrap();
    let cancel = cancellation(&pending, &mut frames, watch.header.corr).await;
    assert_eq!(
        cancel.header.ty,
        FrameType::Cancel,
        "last grant removal must remove its selector"
    );
    let held = ledger::Key {
        carrier: "direct".into(),
        call_key: "parked".into(),
    };
    ledger::admit(
        &db.lock().unwrap(),
        &held,
        "digest",
        Some(&ledger::ScopeIdentity {
            owner: "direct".into(),
            scope_ref: scope.scope_ref.clone(),
            scope_epoch: 1,
        }),
        ledger::State::Prepared,
    )
    .unwrap();
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &watch), 1);
    turns().await;
    ledger::settle(
        &db.lock().unwrap(),
        &held,
        &ledger::not_started(&held, "withdrawn"),
        "not_started",
        None,
        ledger::now_ms(),
    )
    .unwrap();
    let cancel = cancellation(&pending, &mut frames, watch.header.corr).await;
    assert_eq!(
        cancel.header.ty,
        FrameType::Cancel,
        "settling the final held row must remove its selector"
    );
    let policy = crate::db::remote_exec::PolicyKey {
        project_root: root.as_path().to_string_lossy().into_owned(),
        harness: "test".into(),
        session: "test".into(),
        principal: "direct".into(),
        owner: key.owner.clone(),
        scope_ref: key.scope_ref.clone(),
        epoch: key.epoch,
        preset: "head".into(),
    };
    grants::materialize(&db.lock().unwrap(), &policy, &json!({}), &[(grant, None)]).unwrap();
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(
        snapshot(&pending, &watch),
        1,
        "materialized grants must wake the selector set"
    );
    worker.abort();
    let _ = worker.await;
}

#[cfg(unix)]
#[tokio::test]
async fn scope_watch_shell_terminal_notification_removes_selector_without_end_probe() {
    let (dir, root) = test_support::test_root("scope-watch-terminal");
    let ctx = test_support::test_ctx();
    ctx.update_config(|config| {
        config.project_root = Some(root.as_path().into());
        config.storage_dir = Some(dir.path().join("store"));
    });
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    )));
    let executor = Arc::new(Executor::new());
    executor.register_actor(root.clone(), ctx.clone());
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
    let (_handle, mut ended, worker) = start(control, executor, runs);
    turns().await;
    let scope = stamp(10);
    let task = spawn_scoped(
        &ctx,
        &root,
        &scope,
        "while [ ! -f finish ]; do sleep 0.01; done",
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    turns().await;
    std::fs::write(root.as_path().join("finish"), b"finish").unwrap();
    let cancel = tokio::time::timeout(Duration::from_secs(5), frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.header.ty, FrameType::Cancel);
    assert_eq!(cancel.header.corr, watch.header.corr);
    let status = ctx
        .bash_background()
        .status(
            &task,
            "watch-session",
            Some(root.as_path()),
            Some(&ctx.storage_dir()),
            0,
        )
        .unwrap();
    assert_eq!(
        status.info.status,
        crate::bash_background::BgTaskStatus::Completed
    );
    assert!(
        ended.try_recv().is_err(),
        "ordinary process exit is not daemon scope end"
    );
    assert!(
        frames.try_recv().is_err(),
        "terminal notification must not describe the scope"
    );
    worker.abort();
    let _ = worker.await;
}

#[cfg(unix)]
#[tokio::test]
async fn scope_watch_connection_handoff_replays_durable_end_and_cleanup_is_idempotent() {
    scope_watch_handoff_case(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn scope_watch_completed_end_eviction_repeats_one_notice_without_another_kill() {
    scope_watch_handoff_case(true).await;
}

#[cfg(unix)]
async fn scope_watch_handoff_case(evict_completed: bool) {
    use crate::db::{access_grants as grants, call_ledger as ledger};
    let (dir, root) = test_support::test_root("scope-watch-handoff");
    let ctx = test_support::test_ctx();
    ctx.update_config(|config| {
        config.project_root = Some(root.as_path().into());
        config.storage_dir = Some(dir.path().join("store"));
    });
    let db = Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let executor = Arc::new(Executor::new());
    executor.register_actor(root.clone(), ctx.clone());
    let scope = stamp(12);
    let task = spawn_scoped(&ctx, &root, &scope, "sleep 30");
    let grant_key = grants::ScopeKey::from_stamp(&scope);
    let ledger_key = ledger::Key {
        carrier: "direct".into(),
        call_key: "handoff-held".into(),
    };
    {
        let conn = db.lock().unwrap();
        grants::register_scope(&conn, &grant_key, None, &crate::config::Config::default()).unwrap();
        grants::insert(
            &conn,
            &grant_key,
            &crate::access_policy::AccessGrant {
                path: root.as_path().join("grant"),
                kind: crate::access_policy::GrantKind::File,
                operation: crate::access_policy::AccessOperation::Read,
            },
        )
        .unwrap();
        ledger::admit(
            &conn,
            &ledger_key,
            "digest",
            Some(&ledger::ScopeIdentity {
                owner: "direct".into(),
                scope_ref: scope.scope_ref.clone(),
                scope_epoch: 1,
            }),
            ledger::State::Prepared,
        )
        .unwrap();
    }
    let (entered, started) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let blocker = executor.submit_async(
        root.clone(),
        Lane::CallLedger,
        "handoff-block".into(),
        Box::new(move |_| {
            let _ = entered.send(());
            let _ = held.recv_timeout(Duration::from_secs(20));
            Response::success("handoff-block", json!({}))
        }),
    );
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .unwrap()
        .unwrap();
    let relay = crate::gh_shim_relay::GhRelay::new(dir.path().join("unused-connection.json"));
    let service = crate::gh_shim_speech::service::start(
        dir.path(),
        &relay,
        Arc::new(|_| Box::pin(async { Err("describe_unavailable".into()) })),
        "handoff-one".into(),
        true,
    );
    let (first_control, first_pending, mut first_frames) =
        control(&["scope.describe", "scope.watch"]);
    let first_runs = Arc::new(run_scope::RunScopes::new(first_control.clone()));
    let (_first_handle, _first_ended, first_worker) =
        start_for_speech(first_control, executor.clone(), first_runs, service.clone());
    let watch = opening_watch(&first_pending, &mut first_frames).await;
    snapshot(&first_pending, &watch);
    turns().await;
    push(&first_pending, &watch, &scope, ScopeWatchEvent::Ended);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service.store.scope_ended(&scope).unwrap()
            || executor
                .queued_lane_for_test(&root, "scope-ended")
                .is_none()
        {
            tokio::select! {
                Some(query) = first_frames.recv() => {
                    if query.header.ty == FrameType::Cancel { continue; }
                    let body: Value = serde_json::from_slice(&query.body).unwrap();
                    assert_eq!(body["op"], "scope.describe");
                    response(&first_pending, &query, answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1));
                }
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        ledger::get(&db.lock().unwrap(), &ledger_key)
            .unwrap()
            .unwrap()
            .state,
        ledger::State::Prepared
    );
    first_worker.abort();
    let _ = first_worker.await;
    let replacement_service = service.clone();
    let (second_control, second_pending, mut second_frames) =
        control(&["scope.describe", "scope.watch"]);
    let second_runs = Arc::new(run_scope::RunScopes::new(second_control.clone()));
    let (second_handle, mut second_ended, second_worker) = start_for_speech(
        second_control,
        executor.clone(),
        second_runs,
        replacement_service,
    );
    let rearm = opening_watch(&second_pending, &mut second_frames).await;
    snapshot_status(&second_pending, &rearm, ScopeWatchStatus::Ended);
    turns().await;
    release.send(()).unwrap();
    blocker.await.unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                notice = second_ended.recv() => break notice,
                Some(query) = second_frames.recv() => {
                    if query.header.ty == FrameType::Cancel { continue; }
                    let body: Value = serde_json::from_slice(&query.body).unwrap();
                    assert_eq!(body["op"], "scope.describe");
                    response(&second_pending, &query, answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1));
                }
            }
        }
    })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stamp_key(&end.stamp), stamp_key(&scope));
    let status = || {
        ctx.bash_background()
            .status(
                &task,
                "watch-session",
                Some(root.as_path()),
                Some(&ctx.storage_dir()),
                0,
            )
            .unwrap()
            .info
    };
    assert_eq!(
        status().status,
        crate::bash_background::BgTaskStatus::Killed
    );
    assert_eq!(status().status_reason.as_deref(), Some("scope_revoked"));
    let sequence = ledger::get(&db.lock().unwrap(), &ledger_key)
        .unwrap()
        .unwrap()
        .seq;
    let replay = revoke(executor, &scope).await;
    assert!(replay.complete && replay.keys.is_empty());
    let conn = db.lock().unwrap();
    let row = ledger::get(&conn, &ledger_key).unwrap().unwrap();
    assert_eq!(row.state, ledger::State::Settled);
    assert_eq!(
        row.seq, sequence,
        "repeated cleanup must not create another settlement"
    );
    assert!(grants::list(&conn, &grant_key).unwrap().is_empty());
    drop(conn);
    second_handle.observed.send(scope.clone()).unwrap();
    second_handle.observed.send(scope.clone()).unwrap();
    turns().await;
    assert!(
        second_ended.try_recv().is_err(),
        "duplicate committed notices must not repeat cleanup notifications"
    );
    assert_eq!(
        status().status,
        crate::bash_background::BgTaskStatus::Killed
    );
    if evict_completed {
        let duration = status().duration_ms;
        {
            let mut completed = second_handle.completed.lock().unwrap();
            assert!(completed.contains(&stamp_key(&scope)));
            for (_, at) in &mut completed.0 {
                *at -= COMPLETED_END_TTL;
            }
        }
        second_handle.observed.send(scope.clone()).unwrap();
        let redundant = tokio::time::timeout(Duration::from_secs(5), second_ended.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stamp_key(&redundant.stamp), stamp_key(&scope));
        assert!(redundant.keys.is_empty());
        turns().await;
        second_handle.observed.send(scope).unwrap();
        turns().await;
        assert!(
            second_ended.try_recv().is_err(),
            "one redundant notice after expiry"
        );
        assert_eq!(
            status().duration_ms,
            duration,
            "the task was not killed again"
        );
        assert_eq!(
            ledger::get(&db.lock().unwrap(), &ledger_key)
                .unwrap()
                .unwrap()
                .seq,
            sequence
        );
    }
    second_worker.abort();
    let _ = second_worker.await;
}
fn response(
    pending: &readiness::PendingControlReplies,
    request: &Frame,
    body: impl serde::Serialize,
) {
    assert!(pending.resolve(
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&body).unwrap()
        )
        .unwrap()
    ));
}
fn snapshot(pending: &readiness::PendingControlReplies, request: &Frame) -> usize {
    snapshot_status(pending, request, ScopeWatchStatus::Live)
}
fn snapshot_status(
    pending: &readiness::PendingControlReplies,
    request: &Frame,
    status: ScopeWatchStatus,
) -> usize {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let selectors: Vec<subc_client_rs::ScopeWatchSelector> =
        serde_json::from_value(body["scopes"].clone()).unwrap();
    let count = selectors.len();
    assert!(count > 0 && count <= 256);
    response(
        pending,
        request,
        ScopeWatchResponse::new(
            selectors
                .into_iter()
                .map(|selector| ScopeWatchState {
                    scope_ref: selector.scope_ref,
                    epoch: selector.epoch,
                    status: status.clone(),
                    by_epoch: None,
                })
                .collect(),
        ),
    );
    count
}
fn push(
    pending: &readiness::PendingControlReplies,
    request: &Frame,
    stamp: &ScopeStamp,
    event: ScopeWatchEvent,
) {
    assert!(pending.resolve(
        Frame::build(
            FrameType::StreamData,
            control_flags(),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&ScopeWatchPush::new(
                event,
                ScopeRef {
                    owner: stamp.owner.clone(),
                    scope_ref: stamp.scope_ref.clone()
                },
                stamp.scope_epoch,
                None
            ))
            .unwrap()
        )
        .unwrap()
    ));
}
async fn turns() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}
async fn request(frames: &mut mpsc::Receiver<WriterFrame>, op: &str) -> WriterFrame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .expect("control frame deadline")
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["op"], op, "unexpected control frame: {body}");
        return frame;
    }
}
async fn opening_watch(
    pending: &readiness::PendingControlReplies,
    frames: &mut mpsc::Receiver<WriterFrame>,
) -> WriterFrame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .unwrap()
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        match body["op"].as_str().unwrap() {
            "scope.watch" => return frame,
            // Coverage begins at the snapshot, so pre-open fallback requests
            // are valid and must not be mistaken for post-registration probes.
            "scope.describe" => response(
                pending,
                &frame,
                answer(&stamp(0), subc_protocol::scope::ScopeStatus::Live, 1),
            ),
            op => panic!("unexpected opening op {op}"),
        }
    }
}
async fn cancellation(
    pending: &readiness::PendingControlReplies,
    frames: &mut mpsc::Receiver<WriterFrame>,
    corr: u64,
) -> WriterFrame {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .unwrap()
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            assert_eq!(frame.header.corr, corr);
            return frame;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["op"], "scope.describe");
        response(
            pending,
            &frame,
            answer(&stamp(0), subc_protocol::scope::ScopeStatus::Live, 1),
        );
    }
}
fn control(
    ops: &[&str],
) -> (
    Arc<readiness::DaemonControl>,
    readiness::PendingControlReplies,
    mpsc::Receiver<WriterFrame>,
) {
    let (writer, frames) = mpsc::channel(1024);
    let pending = readiness::PendingControlReplies::default();
    let control = Arc::new(readiness::DaemonControl::new(
        writer,
        Arc::new(DispatchPathMetrics::new()),
        PROTOCOL_VERSION,
        ops.iter().map(|op| (*op).into()).collect(),
        vec![],
        pending.clone(),
        HELLO_CORR + 1,
    ));
    (control, pending, frames)
}

#[tokio::test(start_paused = true)]
async fn scope_watch_healthy_does_not_poll_and_live_rearm_needs_no_confirmation() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scopes = vec![stamp(0), stamp(1), stamp(2)];
    let source = scopes.clone();
    let (_handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &watch), 3);
    let mut describes = 0;
    turns().await;
    tokio::time::advance(Duration::from_secs(240)).await;
    turns().await;
    assert!(frames.try_recv().is_err(), "healthy watch emitted a poll");
    assert!(ended.try_recv().is_err());
    assert_eq!(describes, 0, "live snapshots need no describe");
    assert!(pending.resolve(
        Frame::build(
            FrameType::StreamEnd,
            control_flags(),
            0,
            0,
            watch.header.corr,
            vec![]
        )
        .unwrap()
    ));
    turns().await;
    let mut opening_probes = Vec::new();
    let rearm = loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv())
            .await
            .unwrap()
            .unwrap();
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        if body["op"] == "scope.watch" {
            break frame;
        }
        assert_eq!(body["op"], "scope.describe");
        opening_probes.push(frame);
    };
    turns().await;
    while let Ok(frame) = frames.try_recv() {
        if frame.header.ty != FrameType::Cancel {
            opening_probes.push(frame);
        }
    }
    snapshot(&pending, &rearm);
    turns().await;
    let mut refs = HashSet::new();
    for query in opening_probes {
        let body: Value = serde_json::from_slice(&query.body).unwrap();
        assert!(refs.insert(body["ref"].as_str().unwrap().to_owned()));
        response(
            &pending,
            &query,
            answer(&scopes[0], subc_protocol::scope::ScopeStatus::Live, 1),
        );
        describes += 1;
    }
    // A real stream failure may start fallback probes before the replacement
    // snapshot. The live snapshot itself must not retrigger those probes.
    while let Ok(Some(query)) = tokio::time::timeout(Duration::from_millis(1), frames.recv()).await
    {
        if query.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&query.body).unwrap();
        assert_eq!(body["op"], "scope.describe");
        assert!(refs.insert(body["ref"].as_str().unwrap().to_owned()));
        response(
            &pending,
            &query,
            answer(&scopes[0], subc_protocol::scope::ScopeStatus::Live, 1),
        );
        describes += 1;
    }
    assert!(
        describes <= scopes.len(),
        "at most one failure poll per ref, with no live rearm fanout"
    );
    tokio::time::advance(Duration::from_secs(59)).await;
    turns().await;
    assert!(frames.try_recv().is_err());
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_refusal_retains_polling_and_never_proves_end() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let source = scope.clone();
    let (_handle, mut ended, worker) = start_with_scope_source(
        control,
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(vec![source.clone()]),
    );
    let watch = request(&mut frames, "scope.watch").await;
    assert!(pending.resolve(
        Frame::build(
            FrameType::Error,
            control_flags(),
            0,
            0,
            watch.header.corr,
            serde_json::to_vec(&json!({"code":"unknown_control_op", "message":"old daemon"}))
                .unwrap()
        )
        .unwrap()
    ));
    turns().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    let mut polls = 0;
    // Re-opening and the ordinary fallback tick can become ready together.
    while polls == 0 {
        let frame = frames.recv().await.unwrap();
        let body: Value = serde_json::from_slice(&frame.body).unwrap_or_default();
        match body["op"].as_str() {
            Some("scope.watch") => {
                assert!(pending.resolve(
                    Frame::build(
                        FrameType::Error,
                        control_flags(),
                        0,
                        0,
                        frame.header.corr,
                        serde_json::to_vec(
                            &json!({"code":"unknown_control_op", "message":"old daemon"})
                        )
                        .unwrap()
                    )
                    .unwrap()
                ));
            }
            Some("scope.describe") => {
                polls += 1;
                response(
                    &pending,
                    &frame,
                    answer(&scope, subc_protocol::scope::ScopeStatus::Live, 1),
                );
            }
            _ => assert_eq!(frame.header.ty, FrameType::Cancel),
        }
    }
    assert_eq!(polls, 1);
    assert!(ended.try_recv().is_err());
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_indeterminate_confirmation_holds_state_and_falls_back() {
    for case in ["not_synced", "not_live_unsynced", "error", "timeout"] {
        let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
        let scope = stamp(0);
        let source = scope.clone();
        let (_handle, mut ended, worker) = start_with_scope_source(
            control.clone(),
            Arc::new(Executor::new()),
            Duration::from_secs(60),
            move || std::future::ready(vec![source.clone()]),
        );
        let watch = request(&mut frames, "scope.watch").await;
        snapshot(&pending, &watch);
        turns().await;
        push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
        let query = request(&mut frames, "scope.describe").await;
        match case {
            "timeout" => {
                tokio::time::advance(describe_timeout()).await;
            }
            "error" => {
                assert!(pending.resolve(
                    Frame::build(
                        FrameType::Error,
                        control_flags(),
                        0,
                        0,
                        query.header.corr,
                        serde_json::to_vec(
                            &json!({"code":"owner_unavailable", "message":"unknown"})
                        )
                        .unwrap()
                    )
                    .unwrap()
                ));
            }
            _ => {
                let mut value = serde_json::to_value(answer(
                    &scope,
                    subc_protocol::scope::ScopeStatus::NotLive,
                    1,
                ))
                .unwrap();
                value["owner_synced"] = json!(false);
                if case == "not_synced" {
                    value["status"] = json!("not_synced");
                }
                response(&pending, &query, value);
            }
        }
        turns().await;
        assert!(ended.try_recv().is_err(), "{case} must not end a scope");
        let watch_health = control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health();
        assert_eq!(
            watch_health["polled"], 1,
            "{case}: inconclusive confirmation needs fallback coverage"
        );
        worker.abort();
        let _ = worker.await;
    }
}

#[tokio::test(start_paused = true)]
async fn scope_watch_retained_epochs_share_one_describe_for_a_real_end() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let first = stamp(0);
    let mut second = first.clone();
    second.scope_epoch = 2;
    let source = vec![first.clone(), second.clone()];
    let (_handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &watch), 2);
    push(&pending, &watch, &first, ScopeWatchEvent::Superseded);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&first, subc_protocol::scope::ScopeStatus::Live, 2),
    );
    let rebound = request(&mut frames, "scope.watch").await;
    assert_eq!(
        snapshot(&pending, &rebound),
        1,
        "retained epochs share the current epoch selector after a rebind"
    );
    turns().await;
    assert!(frames.try_recv().is_err());
    push(&pending, &rebound, &second, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&first, subc_protocol::scope::ScopeStatus::Ended, 2),
    );
    let mut epochs = HashSet::new();
    for _ in 0..2 {
        epochs.insert(ended.recv().await.unwrap().stamp.scope_epoch);
    }
    assert_eq!(epochs, HashSet::from([1, 2]));
    turns().await;
    while let Ok(frame) = frames.try_recv() {
        assert_eq!(
            frame.header.ty,
            FrameType::Cancel,
            "one real ref end must not describe each retained epoch separately"
        );
    }
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["descriptions"],
        2,
        "one superseded ref probe and one confirming end probe; no live re-arm probe"
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_five_minute_backstop_repairs_unnotified_membership() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scopes = Arc::new(StdMutex::new(vec![stamp(0)]));
    let source = scopes.clone();
    let (_handle, _ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.lock().unwrap().clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    turns().await;
    scopes.lock().unwrap().push(stamp(1));
    tokio::time::advance(scope_watch::BACKSTOP).await;
    let repaired = opening_watch(&pending, &mut frames).await;
    assert_eq!(
        snapshot(&pending, &repaired),
        2,
        "the census repairs drift by re-arming, not by polling the daemon"
    );
    turns().await;
    assert!(
        frames.try_recv().is_err(),
        "live census repair needs no describe"
    );
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["drift_repairs"],
        1
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_partitions_more_than_256_and_keeps_overflow_polled() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let mut scopes: HashMap<_, _> = (0..4100)
        .map(|index| {
            let scope = stamp(index);
            (stamp_key(&scope), scope)
        })
        .collect();
    let mut watches = scope_watch::Watches::new(control.metrics.clone());
    watches.reconcile(&scopes, &control, Duration::from_secs(60));
    let mut selected = HashSet::new();
    for _ in 0..16 {
        let watch = request(&mut frames, "scope.watch").await;
        let body: Value = serde_json::from_slice(&watch.body).unwrap();
        for selector in body["scopes"].as_array().unwrap() {
            assert!(selected.insert(selector.to_string()));
        }
        assert_eq!(snapshot(&pending, &watch), 256);
    }
    for _ in 0..16 {
        let event = watches.receiver.recv().await.unwrap();
        assert!(watches.event(event, scopes.len()).is_empty());
    }
    assert_eq!(selected.len(), 4096);
    assert_eq!(watches.health.watched, 4096);
    assert_eq!(watches.health.polled, 4);
    assert!(!watches.coverage(scopes.len()).all);
    let removed: HashSet<_> = watches
        .healthy_keys()
        .into_iter()
        .filter(|key| key.1.ends_with('0'))
        .collect();
    for key in &removed {
        scopes.remove(key);
    }
    watches.reconcile(&scopes, &control, Duration::from_secs(60));
    let reopened = 16 - watches.health.watches;
    for _ in 0..reopened {
        let watch = request(&mut frames, "scope.watch").await;
        let body: Value = serde_json::from_slice(&watch.body).unwrap();
        for selector in body["scopes"].as_array().unwrap() {
            let reference = selector["scope_ref"]["ref"].as_str().unwrap();
            assert!(scopes.keys().any(|key| key.1 == reference));
            assert!(!removed.iter().any(|key| key.1 == reference));
        }
        snapshot(&pending, &watch);
    }
    for _ in 0..reopened {
        let event = watches.receiver.recv().await.unwrap();
        watches.event(event, scopes.len());
    }
    // All four overflow scopes fit after removals. Every remaining key is
    // assigned exactly once, with no stale removed selector left in a shard.
    let coverage = watches.coverage(scopes.len());
    assert!(coverage.all);
    assert_eq!(coverage.keys, scopes.keys().cloned().collect());
    assert!(coverage.keys.is_disjoint(&removed));
    assert!(watches.health.polled <= scopes.len());
    assert!(
        serde_json::to_vec(&watches.health).unwrap().len() < 256,
        "bounded counters must fit the 12 KiB health envelope"
    );
    drop(watches);
    turns().await;
}
