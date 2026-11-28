use super::*;

#[tokio::test(start_paused = true)]
async fn scope_watch_audit_push_during_inflight_describe_still_gets_confirmed() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let source = vec![scope.clone()];
    let (_handle, mut ended, worker) = start_with_scope_source(
        control,
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    turns().await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 1),
    );
    let confirmation = request(&mut frames, "scope.describe").await;
    assert!(
        ended.try_recv().is_err(),
        "the push and stale Live reply do not authorize cleanup"
    );
    response(
        &pending,
        &confirmation,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    assert_eq!(ended.recv().await.unwrap().stamp, scope);
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_pending_trigger_rechecks_unsynced_error_and_timeout_answers() {
    for failure in ["not_synced", "error", "timeout"] {
        let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
        let scope = stamp(0);
        let source = vec![scope.clone()];
        let (_handle, mut ended, worker) = start_with_scope_source(
            control,
            Arc::new(Executor::new()),
            Duration::from_secs(60),
            move || std::future::ready(source.clone()),
        );
        let watch = request(&mut frames, "scope.watch").await;
        snapshot(&pending, &watch);
        push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
        let query = request(&mut frames, "scope.describe").await;
        push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
        turns().await;
        match failure {
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
                        serde_json::to_vec(&json!({"code":"owner_unavailable", "message":"retry"}))
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
                value["status"] = json!("not_synced");
                response(&pending, &query, value);
            }
        }
        let confirmation = request(&mut frames, "scope.describe").await;
        assert!(
            ended.try_recv().is_err(),
            "{failure} does not establish an end"
        );
        response(
            &pending,
            &confirmation,
            answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
        );
        ended.recv().await.unwrap();
        worker.abort();
        let _ = worker.await;
    }
}

#[tokio::test(start_paused = true)]
async fn scope_watch_trigger_on_a_queued_key_is_not_consumed_by_its_first_probe() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scopes: Vec<_> = (0..5).map(stamp).collect();
    let source = scopes.clone();
    let (_handle, mut ended, worker) = start_with_scope_source(
        control,
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot_status(&pending, &watch, ScopeWatchStatus::Ended);
    let mut active = Vec::new();
    let mut references = HashSet::new();
    for _ in 0..4 {
        let query = request(&mut frames, "scope.describe").await;
        let body: Value = serde_json::from_slice(&query.body).unwrap();
        references.insert(body["ref"].as_str().unwrap().to_owned());
        active.push(query);
    }
    let queued = scopes
        .iter()
        .find(|scope| !references.contains(&scope.scope_ref))
        .unwrap();
    push(&pending, &watch, queued, ScopeWatchEvent::Ended);
    turns().await;
    response(
        &pending,
        &active[0],
        answer(queued, subc_protocol::scope::ScopeStatus::Live, 1),
    );
    let first = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &first,
        answer(queued, subc_protocol::scope::ScopeStatus::Live, 1),
    );
    let confirmation = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &confirmation,
        answer(queued, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    assert_eq!(
        ended.recv().await.unwrap().stamp.scope_ref,
        queued.scope_ref
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_snapshot_rearm_during_inflight_probe_retains_every_nonlive_status() {
    for status in [
        ScopeWatchStatus::Ended,
        ScopeWatchStatus::Superseded,
        ScopeWatchStatus::UnknownScope,
    ] {
        let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
        let scope = stamp(0);
        let scopes = Arc::new(StdMutex::new(vec![scope.clone()]));
        let source = scopes.clone();
        let (_handle, mut ended, worker) = start_with_scope_source(
            control,
            Arc::new(Executor::new()),
            Duration::from_secs(60),
            move || std::future::ready(source.lock().unwrap().clone()),
        );
        let watch = request(&mut frames, "scope.watch").await;
        snapshot(&pending, &watch);
        push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
        let stale = request(&mut frames, "scope.describe").await;
        scopes.lock().unwrap().push(stamp(1));
        scope_watch::changed(scope_watch::Source::Shell);
        let replacement = request(&mut frames, "scope.watch").await;
        assert_eq!(snapshot_status(&pending, &replacement, status), 2);
        turns().await;
        response(
            &pending,
            &stale,
            answer(&scope, subc_protocol::scope::ScopeStatus::Live, 1),
        );
        let mut rechecked = false;
        for _ in 0..3 {
            let query = request(&mut frames, "scope.describe").await;
            let body: Value = serde_json::from_slice(&query.body).unwrap();
            if body["ref"] == scope.scope_ref {
                rechecked = true;
                response(
                    &pending,
                    &query,
                    answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
                );
                break;
            }
            response(
                &pending,
                &query,
                answer(&stamp(1), subc_protocol::scope::ScopeStatus::Live, 1),
            );
        }
        assert!(
            rechecked,
            "a terminal or unknown replacement snapshot must invalidate a pre-registration probe"
        );
        assert_eq!(ended.recv().await.unwrap().stamp, scope);
        worker.abort();
        let _ = worker.await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn scope_watch_pending_push_with_already_ended_answer_kills_once() {
    let (dir, root) = test_support::test_root("scope-watch-ended-race");
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
    let scope = stamp(0);
    let task = spawn_scoped(&ctx, &root, &scope, "sleep 30");
    let source = vec![scope.clone()];
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let (_handle, mut ended, worker) =
        start_with_scope_source(control, executor, Duration::from_secs(60), move || {
            std::future::ready(source.clone())
        });
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    turns().await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    tokio::time::timeout(Duration::from_secs(5), ended.recv())
        .await
        .unwrap()
        .unwrap();
    turns().await;
    assert!(ended.try_recv().is_err());
    while let Ok(frame) = frames.try_recv() {
        assert_eq!(
            frame.header.ty,
            FrameType::Cancel,
            "a definitive answer must consume the dirty trigger without another describe"
        );
    }
    let status = ctx
        .bash_background()
        .status(
            &task,
            "watch-session",
            Some(root.as_path()),
            Some(&ctx.storage_dir()),
            0,
        )
        .unwrap()
        .info;
    assert_eq!(status.status, crate::bash_background::BgTaskStatus::Killed);
    assert_eq!(status.status_reason.as_deref(), Some("scope_revoked"));
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_coverage_waits_for_snapshot_and_verification_is_spread_out() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scopes: HashMap<_, _> = (0..2)
        .map(|i| {
            let scope = stamp(i);
            (stamp_key(&scope), scope)
        })
        .collect();
    let mut watches = scope_watch::Watches::new(control.metrics.clone());
    watches.reconcile(&scopes, &control, Duration::from_secs(60));
    assert!(
        watches.coverage(scopes.len()).keys.is_empty(),
        "an opening task is not coverage"
    );
    let watch = request(&mut frames, "scope.watch").await;
    assert!(
        !watches.coverage(scopes.len()).all,
        "a sent request still has no snapshot"
    );
    snapshot(&pending, &watch);
    let event = watches.receiver.recv().await.unwrap();
    watches.event(event, scopes.len());
    let coverage = watches.coverage(scopes.len());
    assert!(coverage.all);
    let mut checks = scope_watch::Verifications::default();
    checks.reconcile(&coverage.keys, &scopes);
    assert!(checks.take_due().is_none());
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    let first = checks.take_due().unwrap();
    assert!(
        checks.take_due().is_none(),
        "verification cannot burst across the selected set"
    );
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    let second = checks.take_due().unwrap();
    assert_ne!(first, second);
    assert!(checks.take_due().is_none());
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    assert_eq!(checks.take_due().unwrap(), first);
}

#[tokio::test(start_paused = true)]
async fn scope_watch_slow_verification_confirms_a_lost_push_and_counts_it() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let source = vec![scope.clone()];
    let (_handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    turns().await;
    tokio::time::advance(scope_watch::VERIFY_INTERVAL - Duration::from_secs(1)).await;
    turns().await;
    assert!(frames.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(1)).await;
    let verification = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &verification,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    ended.recv().await.unwrap();
    turns().await;
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["verifications"],
        1
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_verification_shared_epochs_keep_one_ref_cooldown_across_rearm() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let mut newer = scope.clone();
    newer.scope_epoch = 2;
    let source = vec![scope.clone(), newer];
    let (_handle, _ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    push(&pending, &watch, &scope, ScopeWatchEvent::Superseded);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 2),
    );
    let rearm = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &rearm), 1);
    turns().await;
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 2),
    );
    turns().await;
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    turns().await;
    assert!(
        frames.try_recv().is_err(),
        "the other retained epoch must share the first epoch's verification cooldown"
    );
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 2),
    );
    turns().await;
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["verifications"],
        2
    );
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_verification_new_epoch_inherits_its_refs_existing_cooldown() {
    let mut scopes: HashMap<_, _> = (0..2)
        .map(|i| {
            let scope = stamp(i);
            (stamp_key(&scope), scope)
        })
        .collect();
    let mut checks = scope_watch::Verifications::default();
    checks.reconcile(&scopes.keys().cloned().collect(), &scopes);
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    let first = checks.take_due().unwrap();
    checks.verified(std::iter::once(first.clone()));
    let mut additional_epoch = scopes[&first].clone();
    additional_epoch.scope_epoch += 1;
    scopes.insert(stamp_key(&additional_epoch), additional_epoch);
    checks.reconcile(&scopes.keys().cloned().collect(), &scopes);
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 2).await;
    assert_ne!(checks.take_due().unwrap().1, first.1);
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 3).await;
    assert!(
        checks.take_due().is_none(),
        "a newly retained epoch must not bypass the ref's fifteen-minute cooldown"
    );
    tokio::time::advance(scope_watch::VERIFY_INTERVAL / 6).await;
    assert_eq!(checks.take_due().unwrap().1, first.1);
}

#[tokio::test(start_paused = true)]
async fn scope_watch_probe_panic_releases_the_ref_and_pending_keys() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    control.panic_next_describe.store(true, Ordering::Relaxed);
    let scope = stamp(0);
    let source = vec![scope.clone()];
    let (_handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot_status(&pending, &watch, ScopeWatchStatus::Ended);
    turns().await;
    assert!(ended.try_recv().is_err());
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["polled"],
        1
    );
    tokio::time::advance(Duration::from_secs(60)).await;
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
            "scope.watch" => {
                snapshot(&pending, &frame);
            }
            "scope.describe" => {
                response(
                    &pending,
                    &frame,
                    answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
                );
                break;
            }
            op => panic!("unexpected op {op}"),
        }
    }
    ended.recv().await.unwrap();
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_completed_and_removed_keys_leave_no_poll_or_confirmed_bookkeeping() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let scopes = Arc::new(StdMutex::new(vec![scope.clone()]));
    let source = scopes.clone();
    let (_handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || std::future::ready(source.lock().unwrap().clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Live, 1),
    );
    turns().await;
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["poll_keys"],
        1
    );
    push(&pending, &watch, &scope, ScopeWatchEvent::Ended);
    let query = request(&mut frames, "scope.describe").await;
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    ended.recv().await.unwrap();
    turns().await;
    scopes.lock().unwrap().clear();
    scope_watch::changed(scope_watch::Source::Shell);
    turns().await;
    tokio::time::advance(scope_watch::DEBOUNCE).await;
    turns().await;
    let health = control
        .metrics
        .scope_descriptions
        .lock()
        .unwrap()
        .scope_watch_health();
    assert_eq!(health["poll_keys"], 0);
    assert_eq!(health["confirmed_cleanup"], 0);
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_mutation_hints_share_one_fixed_debounce_window() {
    let (control, _pending, _frames) = control(&["scope.describe", "scope.watch"]);
    let reads = Arc::new(AtomicUsize::new(0));
    let source = reads.clone();
    let (_handle, _ended, worker) = start_with_scope_source(
        control,
        Arc::new(Executor::new()),
        Duration::from_secs(60),
        move || {
            source.fetch_add(1, Ordering::Relaxed);
            std::future::ready(vec![])
        },
    );
    turns().await;
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    for _ in 0..100 {
        scope_watch::changed(scope_watch::Source::Held);
    }
    turns().await;
    tokio::time::advance(scope_watch::DEBOUNCE - Duration::from_millis(1)).await;
    turns().await;
    assert_eq!(reads.load(Ordering::Relaxed), 1);
    for _ in 0..100 {
        scope_watch::changed(scope_watch::Source::Held);
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    turns().await;
    assert_eq!(
        reads.load(Ordering::Relaxed),
        2,
        "ongoing churn must not postpone the fixed deadline or read once per hint"
    );
    worker.abort();
    let _ = worker.await;
}

fn database_fixture(
    name: &str,
) -> (
    tempfile::TempDir,
    Arc<AppContext>,
    Arc<StdMutex<crate::db::TrackedConnection>>,
    Arc<Executor>,
) {
    let (dir, root) = test_support::test_root(name);
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
    executor.register_actor(root, ctx.clone());
    (dir, ctx, db, executor)
}

#[tokio::test]
async fn scope_watch_census_skips_bad_grant_and_held_rows_without_hiding_valid_scopes() {
    use crate::db::{access_grants as grants, call_ledger as ledger};
    let (dir, _ctx, db, executor) = database_fixture("scope-watch-corrupt-census");
    let good = stamp(1);
    let good_key = grants::ScopeKey::from_stamp(&good);
    let bad_key = grants::ScopeKey {
        owner: "not-json".into(),
        scope_ref: "bad-grant".into(),
        epoch: 1,
    };
    let grant = crate::access_policy::AccessGrant {
        path: dir.path().join("grant"),
        kind: crate::access_policy::GrantKind::File,
        operation: crate::access_policy::AccessOperation::Read,
    };
    {
        let conn = db.lock().unwrap();
        // Model malformed persisted fields, not an allowed grant/ledger write.
        conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
            .unwrap();
        for key in [&bad_key, &good_key] {
            grants::register_scope(&conn, key, None, &crate::config::Config::default()).unwrap();
            grants::insert(&conn, key, &grant).unwrap();
        }
        conn.execute(
            "UPDATE access_grants SET epoch='broken-epoch' WHERE scope_ref='bad-grant'",
            [],
        )
        .unwrap();
        let owner_bad = grants::ScopeKey {
            owner: "not-json".into(),
            scope_ref: "bad-owner".into(),
            epoch: 1,
        };
        grants::register_scope(&conn, &owner_bad, None, &crate::config::Config::default()).unwrap();
        grants::insert(&conn, &owner_bad, &grant).unwrap();
        for (name, owner, epoch) in [
            ("held-good", "direct", 1),
            ("held-bad-owner", "invalid", 1),
            ("held-bad-epoch", "direct", 2),
        ] {
            ledger::admit(
                &conn,
                &ledger::Key {
                    carrier: "direct".into(),
                    call_key: name.into(),
                },
                "digest",
                Some(&ledger::ScopeIdentity {
                    owner: owner.into(),
                    scope_ref: name.into(),
                    scope_epoch: epoch,
                }),
                ledger::State::Prepared,
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE call_ledger SET scope_epoch=-1 WHERE scope_ref='held-bad-epoch'",
            [],
        )
        .unwrap();
    }
    let (control, _pending, _frames) = control(&["scope.describe", "scope.watch"]);
    let runs = Arc::new(run_scope::RunScopes::new(control));
    let inventory = work_scopes(executor, runs).await;
    assert!(
        inventory.failed.is_empty(),
        "row corruption is not a database read failure"
    );
    assert_eq!(inventory.skipped, [2, 2]);
    let scopes = inventory.scopes();
    assert_eq!(scopes.len(), 2);
    assert!(scopes.contains_key(&stamp_key(&good)));
    assert!(scopes.keys().any(|key| key.1 == "held-good"));
}

#[tokio::test]
async fn scope_watch_confirmed_grant_cleanup_keeps_retrying_under_full_coverage() {
    use crate::db::access_grants as grants;
    let (dir, _ctx, db, executor) = database_fixture("scope-watch-persistent-cleanup");
    let scope = stamp(0);
    let key = grants::ScopeKey::from_stamp(&scope);
    {
        let conn = db.lock().unwrap();
        grants::register_scope(&conn, &key, None, &crate::config::Config::default()).unwrap();
        grants::insert(
            &conn,
            &key,
            &crate::access_policy::AccessGrant {
                path: dir.path().join("grant"),
                kind: crate::access_policy::GrantKind::File,
                operation: crate::access_policy::AccessOperation::Read,
            },
        )
        .unwrap();
        conn.execute_batch("CREATE TRIGGER reject_grant_cleanup BEFORE DELETE ON access_grants BEGIN SELECT RAISE(ABORT,'temporary grant cleanup failure'); END;").unwrap();
    }
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
    let (_handle, mut ended, worker) = start(control.clone(), executor, runs);
    let watch = opening_watch(&pending, &mut frames).await;
    snapshot_status(&pending, &watch, ScopeWatchStatus::Ended);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                Some(_) = ended.recv() => break,
                Some(query) = frames.recv() => {
                    if query.header.ty == FrameType::Cancel { continue; }
                    let body: Value = serde_json::from_slice(&query.body).unwrap();
                    assert_eq!(body["op"], "scope.describe");
                    response(&pending, &query, answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1));
                }
            }
        }
    }).await.unwrap();
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert_eq!(grants::list(&db.lock().unwrap(), &key).unwrap().len(), 1);
    assert!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .cleanup_health()["failed_attempts"]
            .as_u64()
            .unwrap()
            >= 4
    );
    db.lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_grant_cleanup")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(7), ended.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(grants::list(&db.lock().unwrap(), &key).unwrap().is_empty());
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["descriptions"],
        1,
        "cleanup retries reuse the confirmed end, without resuming describe polling"
    );
    worker.abort();
    let _ = worker.await;
}
