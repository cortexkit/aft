use super::*;

#[tokio::test(start_paused = true)]
async fn scope_watch_route_binds_and_live_reopens_make_zero_describes() {
    let (_dir, root) = test_support::test_root("scope-watch-bind-count");
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scopes = Arc::new(StdMutex::new(
        (0..25)
            .map(|index| {
                let mut scope = stamp(index);
                scope.owner = Principal::Reserved {
                    module_id: "prefrontal-core".into(),
                };
                scope.owner_authorized = true;
                scope
            })
            .collect::<Vec<_>>(),
    ));
    let source = scopes.clone();
    let (handle, mut ended, worker) = start_with_scope_source(
        control.clone(),
        Arc::new(Executor::new()),
        Duration::from_secs(5),
        move || std::future::ready(source.lock().unwrap().clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    assert_eq!(snapshot(&pending, &watch), 25);
    turns().await;
    assert!(
        frames.try_recv().is_err(),
        "25 live refs need zero initial describes"
    );
    for index in 25..28 {
        let mut scope = scopes.lock().unwrap()[0].clone();
        scope.scope_ref = format!("watched-{index}");
        scopes.lock().unwrap().push(scope.clone());
        let mut identity = test_support::route_identity(&root, "watch-session");
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(scope);
        handle.routes(&HashMap::from([(route_key(index as u16, 1), identity)]));
        scope_watch::changed(scope_watch::Source::Shell);
        let replacement = request(&mut frames, "scope.watch").await;
        tokio::time::advance(Duration::from_secs(5)).await;
        turns().await;
        assert!(
            frames.try_recv().is_err(),
            "a bind's handoff must not start fallback describes"
        );
        assert_eq!(snapshot(&pending, &replacement), index + 1);
        turns().await;
        assert!(
            frames.try_recv().is_err(),
            "a live reopen must not describe the shard again"
        );
    }
    assert_eq!(
        control
            .metrics
            .scope_descriptions
            .lock()
            .unwrap()
            .scope_watch_health()["descriptions"],
        0
    );
    assert!(ended.try_recv().is_err());
    worker.abort();
    let _ = worker.await;
}

#[tokio::test(start_paused = true)]
async fn scope_watch_handoff_deadline_survives_churn_and_failure_drops_coverage() {
    for fail in [false, true] {
        let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
        let scope = stamp(0);
        let mut scopes = HashMap::from([(stamp_key(&scope), scope)]);
        let mut watches = scope_watch::Watches::new(control.metrics.clone());
        watches.reconcile(&scopes, &control, Duration::from_secs(5));
        let watch = request(&mut frames, "scope.watch").await;
        snapshot(&pending, &watch);
        let event = watches.receiver.recv().await.unwrap();
        assert!(watches.event(event, scopes.len()).is_empty());
        scopes.insert(stamp_key(&stamp(1)), stamp(1));
        watches.reconcile(&scopes, &control, Duration::from_secs(5));
        let _replacement = request(&mut frames, "scope.watch").await;
        assert!(watches.coverage(scopes.len()).all);
        let deadline = watches.next_retry().unwrap();
        tokio::time::advance(describe_timeout() / 2).await;
        scopes.insert(stamp_key(&stamp(2)), stamp(2));
        watches.reconcile(&scopes, &control, Duration::from_secs(5));
        let replacement = request(&mut frames, "scope.watch").await;
        assert_eq!(watches.next_retry(), Some(deadline));
        if fail {
            assert!(pending.resolve(
                Frame::build(
                    FrameType::StreamEnd,
                    control_flags(),
                    0,
                    0,
                    replacement.header.corr,
                    vec![]
                )
                .unwrap()
            ));
            let event = watches.receiver.recv().await.unwrap();
            watches.event(event, scopes.len());
        } else {
            tokio::time::advance(describe_timeout() / 2).await;
        }
        assert!(watches.coverage(scopes.len()).keys.is_empty());
        assert!(!watches.coverage(scopes.len()).all);
    }
}

#[tokio::test(start_paused = true)]
async fn scope_watch_end_during_handoff_is_confirmed_once_from_replacement_snapshot() {
    let (control, pending, mut frames) = control(&["scope.describe", "scope.watch"]);
    let scope = stamp(0);
    let scopes = Arc::new(StdMutex::new(vec![scope.clone()]));
    let source = scopes.clone();
    let (_handle, mut ended, worker) = start_with_scope_source(
        control,
        Arc::new(Executor::new()),
        Duration::from_secs(5),
        move || std::future::ready(source.lock().unwrap().clone()),
    );
    let watch = request(&mut frames, "scope.watch").await;
    snapshot(&pending, &watch);
    turns().await;
    scopes.lock().unwrap().push(stamp(1));
    scope_watch::changed(scope_watch::Source::Shell);
    let replacement = request(&mut frames, "scope.watch").await;
    tokio::time::advance(Duration::from_secs(5)).await;
    turns().await;
    assert!(frames.try_recv().is_err());
    let body: Value = serde_json::from_slice(&replacement.body).unwrap();
    let selectors: Vec<subc_client_rs::ScopeWatchSelector> =
        serde_json::from_value(body["scopes"].clone()).unwrap();
    response(
        &pending,
        &replacement,
        ScopeWatchResponse::new(
            selectors
                .into_iter()
                .map(|selector| ScopeWatchState {
                    status: if selector.scope_ref.scope_ref == scope.scope_ref {
                        ScopeWatchStatus::Ended
                    } else {
                        ScopeWatchStatus::Live
                    },
                    scope_ref: selector.scope_ref,
                    epoch: selector.epoch,
                    by_epoch: None,
                })
                .collect(),
        ),
    );
    let query = request(&mut frames, "scope.describe").await;
    let body: Value = serde_json::from_slice(&query.body).unwrap();
    assert_eq!(body["ref"], scope.scope_ref);
    assert!(ended.try_recv().is_err(), "a snapshot is not an end proof");
    response(
        &pending,
        &query,
        answer(&scope, subc_protocol::scope::ScopeStatus::Ended, 1),
    );
    assert_eq!(ended.recv().await.unwrap().stamp, scope);
    turns().await;
    assert!(ended.try_recv().is_err());
    while let Ok(frame) = frames.try_recv() {
        if frame.header.ty == FrameType::Cancel {
            continue;
        }
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(body["op"], "scope.watch", "a confirmed end acts once");
        snapshot(&pending, &frame);
    }
    worker.abort();
    let _ = worker.await;
}
