use super::*;
use std::cell::RefCell;
use test_support::route_identity_with_harness;

thread_local! {
    static POLICY_VISITS: RefCell<Vec<ProjectRootId>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn record_policy_visit(root: &ProjectRootId) {
    POLICY_VISITS.with_borrow_mut(|visits| visits.push(root.clone()));
}

#[test]
fn one_bind_updates_only_its_root_and_publishes_one_shared_snapshot() {
    let (dir, _) = test_support::test_root("route-delivery-fleet");
    let app = App::default_shared();
    let executor = Executor::new();
    let metrics = DispatchPathMetrics::new();
    let delivery = &app.route_delivery_sessions;
    let roots: Vec<_> = (0..120)
        .map(|i| {
            let path = dir.path().join(i.to_string());
            std::fs::create_dir(&path).unwrap();
            let root = ProjectRootId::from_path(&path).unwrap();
            let ctx = Arc::new(AppContext::from_app(Arc::clone(&app), Config::default()));
            assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
            (root, ctx)
        })
        .collect();
    let mut routes = HashMap::new();
    let mut root_channels = HashMap::new();
    let observing = |root: &ProjectRootId, session: &str| {
        route_identity_with_harness(root, session, BindTrust::FirstParty, "runner")
    };
    for i in 0..250 {
        let root = &roots[i % roots.len()].0;
        let mut identity = observing(root, &format!("existing-{i}"));
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(
            serde_json::from_value(json!({
                "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
                "ref": "worker", "scope_epoch": 1, "kind": "worker", "owner_authorized": true
            }))
            .unwrap(),
        );
        insert_route_channel(
            &mut routes,
            &mut root_channels,
            route_key(i as u16 + 1, 1),
            identity,
        );
    }
    publish_sessions(delivery, &routes);
    for (root, _) in &roots {
        prepare_bind(&executor, delivery, &routes, &root_channels, root, &metrics);
    }
    let (root, ctx) = &roots[0];
    assert!(!ctx.background_callgraph_refresh_allowed());
    let previous = ctx.bash_background().route_delivery_snapshot();
    POLICY_VISITS.with_borrow_mut(Vec::clear);

    prepare_bind(&executor, delivery, &routes, &root_channels, root, &metrics);
    let after_prepare = ctx.bash_background().route_delivery_snapshot();
    assert!(Arc::ptr_eq(&previous, &after_prepare));
    let channel = route_key(251, 1);
    install_route(
        &executor,
        delivery,
        &mut routes,
        &mut root_channels,
        channel,
        observing(root, "new-session"),
        root,
        &metrics,
    );

    let visits = POLICY_VISITS.with_borrow_mut(std::mem::take);
    assert_eq!(visits, [root.clone(), root.clone()]);
    assert!(ctx.background_callgraph_refresh_allowed());
    assert!(!roots[1].1.background_callgraph_refresh_allowed());
    let installed = ctx.bash_background().route_delivery_snapshot();
    assert_eq!(previous.len(), 250);
    assert_eq!(installed.len(), 251);
    assert!(installed.contains("new-session"));
    let deep_clones = roots
        .iter()
        .filter(|(_, ctx)| {
            !Arc::ptr_eq(&installed, &ctx.bash_background().route_delivery_snapshot())
        })
        .count();
    assert_eq!(
        deep_clones, 0,
        "no registry gets a separate copy of the set"
    );

    // A duplicate observing route and a non-observing route do not change
    // membership. Removing the first observing route still leaves it live.
    let duplicate = route_key(252, 1);
    for (channel, identity) in [
        (duplicate, observing(root, "new-session")),
        (
            route_key(253, 1),
            route_identity_with_harness(root, "untrusted", BindTrust::Untrusted, "runner"),
        ),
    ] {
        install_route(
            &executor,
            delivery,
            &mut routes,
            &mut root_channels,
            channel,
            identity,
            root,
            &metrics,
        );
        assert!(Arc::ptr_eq(
            &installed,
            &ctx.bash_background().route_delivery_snapshot()
        ));
    }
    remove_route_channel(&mut routes, &mut root_channels, channel).unwrap();
    sync_route_change(&executor, delivery, &routes, &root_channels, root, &metrics);
    assert!(Arc::ptr_eq(
        &installed,
        &ctx.bash_background().route_delivery_snapshot()
    ));
    remove_route_channel(&mut routes, &mut root_channels, duplicate).unwrap();
    sync_route_change(&executor, delivery, &routes, &root_channels, root, &metrics);
    assert!(!roots[1]
        .1
        .bash_background()
        .route_delivery_snapshot()
        .contains("new-session"));
}

#[cfg(unix)]
#[test]
fn bound_legacy_spawn_pushes_completion_without_another_route_sync() {
    use crate::bash_background::{BgTaskStatus, HardKill};

    let (dir, root) = test_support::test_root("route-delivery-bash");
    let ctx = test_support::test_ctx();
    let app = ctx.app();
    let delivery = &app.route_delivery_sessions;
    let executor = Executor::new();
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let metrics = DispatchPathMetrics::new();
    let mut routes = HashMap::new();
    let mut root_channels = HashMap::new();
    prepare_bind(
        &executor,
        delivery,
        &routes,
        &root_channels,
        &root,
        &metrics,
    );
    let session = "legacy-completion";
    install_route(
        &executor,
        delivery,
        &mut routes,
        &mut root_channels,
        route_key(1, 1),
        test_support::route_identity(&root, session),
        &root,
        &metrics,
    );

    let (tx, rx) = std::sync::mpsc::channel();
    ctx.set_progress_sender(Some(Arc::new(Box::new(move |frame| {
        if let crate::protocol::PushFrame::BashCompleted(done) = frame {
            let _ = tx.send(done);
        }
    }))));
    // No retained launch identity: the completion push must use route liveness.
    let task_id = ctx
        .bash_background()
        .spawn(
            crate::sandbox_spawn::SpawnPlan::Unsandboxed,
            "printf 'finished\\n'",
            session.to_owned(),
            dir.path().to_path_buf(),
            HashMap::new(),
            HardKill::After(Duration::from_secs(10)),
            None,
            ctx.storage_dir(),
            1,
            true,
            false,
            Some(dir.path().to_path_buf()),
        )
        .unwrap();
    let done = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(done.task_id, task_id);
    assert_eq!(done.session_id, session);
    assert_eq!(done.status, BgTaskStatus::Completed);
    assert!(done.launching_identity.is_none());
}
