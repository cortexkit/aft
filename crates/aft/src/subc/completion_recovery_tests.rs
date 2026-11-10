use super::super::test_support::*;
use super::*;
use crate::bash_background::persistence::{
    create_task_layout, read_task_at, write_task_at, PersistedTask,
};
use crate::bash_background::{BgTaskRegistry, BgTaskStatus};

const TASK: &str = "bash-0000000000000001";
const SESSION: &str = "moved-session";

fn persisted_completion(storage: &Path, root: &ProjectRootId, owner: &RouteIdentity) {
    persisted_completion_at(storage, root, owner, TASK, None);
}

fn persisted_completion_at(
    storage: &Path,
    root: &ProjectRootId,
    owner: &RouteIdentity,
    task_id: &str,
    finished_at: Option<u64>,
) {
    let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
    let task = create_task_layout(storage, SESSION, task_id).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.into(),
        SESSION.into(),
        "echo durable".into(),
        root.as_path().into(),
        Some(root.as_path().into()),
        None,
        true,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    metadata.launching_raw_harness = match &owner.spawn_principal {
        AuthenticatedPrincipal::RouteBind { harness, .. } => Some(harness.clone()),
        _ => None,
    };
    if let Some(finished_at) = finished_at {
        metadata.started_at = finished_at.saturating_sub(1);
        metadata.finished_at = Some(finished_at);
        metadata.duration_ms = Some(1);
    }
    assert!(!metadata.completion_delivered);
    write_task_at(&task, &metadata).unwrap();
}

#[test]
fn plugin_completion_churn_recovers_from_registry_once_per_connection() {
    let (_dir, root) = test_root("opencode2-completion-churn");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "direct", "opencode");
    let core = tests::caller_route(&root, "reserved:prefrontal-core", "opencode");
    let wrong_harness = tests::caller_route(&root, "direct", "pi");
    persisted_completion(storage.path(), &root, &owner);
    let registry = BgTaskRegistry::default();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let completion = {
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        registry
            .pending_completions_for_session(SESSION)
            .pop()
            .unwrap()
            .into_push_frame()
    };
    let metrics = DispatchPathMetrics::new();
    let (writer, mut output) = mpsc::channel(16);
    let mut routes = HashMap::from([(route_key(2, 1), core), (route_key(3, 1), wrong_harness)]);
    let mut channels = HashMap::from([(
        root.clone(),
        HashSet::from([route_key(2, 1), route_key(3, 1)]),
    )]);
    let mut buffered = HashMap::new();
    let mut retry = RetryBuffer::new();
    let mut completed = CompletedTaskIds::default();
    // The launching route closed; a core catalog route is still open on the
    // same session, and a second plugin uses a different harness.
    process_reliable_push_frame(
        &writer,
        &metrics,
        &routes,
        &channels,
        &HashMap::new(),
        &mut retry,
        &mut buffered,
        &mut completed,
        root.clone(),
        completion.clone(),
        None,
    );
    assert!(output.try_recv().is_err());
    assert_eq!(buffered[&ReplayKey::from_identity(&owner)].len(), 1);
    assert_eq!(metrics.completion_delivery.health()["unacked_total"], 1);
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(output.try_recv().is_err());
    assert_eq!(metrics.completion_delivery.health()["unacked_total"], 1);

    insert_route_channel(&mut routes, &mut channels, route_key(4, 9), owner.clone());
    // The registry and memory hold describe the same task. Only one push may
    // enter the writer even when bind and maintenance both offer recovery.
    for _ in 0..2 {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut buffered,
            &root,
            None,
        );
    }
    let pushed = output.try_recv().unwrap();
    assert_eq!((pushed.header.channel, pushed.header.epoch), (4, 9));
    assert_eq!(push_frame_task_id(&pushed).as_deref(), Some(TASK));
    assert!(output.try_recv().is_err());
    assert!(buffered.is_empty());
    // A watchdog emission queued before bind recovery must not duplicate it.
    process_reliable_push_frame(
        &writer,
        &metrics,
        &routes,
        &channels,
        &HashMap::new(),
        &mut retry,
        &mut buffered,
        &mut completed,
        root.clone(),
        completion,
        None,
    );
    assert!(output.try_recv().is_err());
    registry.detach();
}

#[test]
fn plugin_completion_survives_restart_and_late_configure_recovery_until_ack() {
    let (_dir, root) = test_root("opencode2-completion-restart");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "direct", "opencode");
    let core = tests::caller_route(&root, "reserved:prefrontal-core", "opencode2");
    persisted_completion(storage.path(), &root, &owner);
    {
        let registry = BgTaskRegistry::default();
        registry.replay_session(storage.path(), SESSION).unwrap();
        let (writer, mut output) = mpsc::channel(4);
        let metrics = DispatchPathMetrics::new();
        let mut buffered = HashMap::new();
        let completion = {
            let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
            registry
                .pending_completions_for_session(SESSION)
                .pop()
                .unwrap()
                .into_push_frame()
        };
        process_reliable_push_frame(
            &writer,
            &metrics,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &mut RetryBuffer::new(),
            &mut buffered,
            &mut CompletedTaskIds::default(),
            root.clone(),
            completion,
            None,
        );
        assert_eq!(buffered.len(), 1);
        assert!(output.try_recv().is_err());
        registry.detach();
        // Drop every in-memory hold and the original registry, as AFT restart does.
    }
    let registry = BgTaskRegistry::default();
    let metrics = DispatchPathMetrics::new();
    let (writer, mut output) = mpsc::channel(4);
    let mut buffered = HashMap::new();
    let mut routes = HashMap::from([(route_key(2, 1), core)]);
    let mut channels = HashMap::from([(root.clone(), HashSet::from([route_key(2, 1)]))]);
    registry.replay_session(storage.path(), SESSION).unwrap();
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(
        output.try_recv().is_err(),
        "core must never consume the durable owner completion"
    );
    insert_route_channel(&mut routes, &mut channels, route_key(5, 2), owner.clone());
    for _ in 0..2 {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut buffered,
            &root,
            None,
        );
    }
    let pushed = output.try_recv().unwrap();
    assert_eq!((pushed.header.channel, pushed.header.epoch), (5, 2));
    assert_eq!(push_frame_task_id(&pushed).as_deref(), Some(TASK));
    assert!(output.try_recv().is_err());
    let task =
        crate::bash_background::persistence::resolve_task(storage.path(), SESSION, TASK).unwrap();
    assert!(
        !read_task_at(&task).unwrap().completion_delivered,
        "push enqueue is not an ack"
    );
    {
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        assert_eq!(
            registry.ack_completions_for_session(Some(SESSION), &[TASK.into()]),
            vec![TASK]
        );
    }
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert_eq!(metrics.completion_delivery.health()["unacked_total"], 0);
    registry.detach();

    // Deferred configure maintenance may recover tasks after a successful
    // bind. A new connection must also see an acked task as settled then.
    let after_ack = BgTaskRegistry::default();
    let after_ack_metrics = DispatchPathMetrics::new();
    recover_registry_completions(
        &writer,
        &after_ack_metrics,
        &after_ack,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    after_ack.replay_session(storage.path(), SESSION).unwrap();
    recover_registry_completions(
        &writer,
        &after_ack_metrics,
        &after_ack,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(output.try_recv().is_err());
    after_ack.detach();
}

#[test]
fn plugin_bind_before_configure_tail_recovers_unacked_completion() {
    let (_dir, root) = test_root("opencode2-late-completion-recovery");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "direct", "opencode");
    persisted_completion(storage.path(), &root, &owner);
    let registry = BgTaskRegistry::default();
    let metrics = DispatchPathMetrics::new();
    let (writer, mut output) = mpsc::channel(4);
    let routes = HashMap::from([(route_key(7, 1), owner)]);
    let channels = HashMap::from([(root.clone(), HashSet::from([route_key(7, 1)]))]);
    let mut buffered = HashMap::new();
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(output.try_recv().is_err());
    registry.replay_session(storage.path(), SESSION).unwrap();
    for _ in 0..2 {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut buffered,
            &root,
            None,
        );
    }
    assert_eq!(output.try_recv().unwrap().header.channel, 7);
    assert!(output.try_recv().is_err());
    let completion = {
        let owner = routes.get(&route_key(7, 1)).unwrap();
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        registry
            .pending_completions_for_session(SESSION)
            .pop()
            .unwrap()
            .into_push_frame()
    };
    process_reliable_push_frame(
        &writer,
        &metrics,
        &routes,
        &channels,
        &HashMap::new(),
        &mut RetryBuffer::new(),
        &mut buffered,
        &mut CompletedTaskIds::default(),
        root.clone(),
        completion,
        None,
    );
    assert!(
        output.try_recv().is_err(),
        "queued live emission duplicated registry recovery"
    );
    assert_eq!(metrics.completion_delivery.health()["unacked_total"], 1);
    registry.detach();
}

#[test]
fn completion_buffer_and_drop_health_counts_transport_outcomes() {
    let (_dir, root) = test_root("completion-push-counters");
    let owner = tests::caller_route(&root, "direct", "opencode");
    let key = ReplayKey::from_identity(&owner);
    let frame = completion_frame_with_session(TASK, SESSION);
    let metrics = DispatchPathMetrics::new();
    let mut buffered = HashMap::new();
    buffer_push_frame(&metrics, &mut buffered, key.clone(), frame);
    let (writer, output) = mpsc::channel(1);
    drop(output);
    assert_eq!(
        replay_buffered_push_frames(
            &writer,
            &metrics,
            route_key(1, 1),
            &mut buffered,
            &key,
            BindTrust::FirstParty,
            None
        ),
        0
    );
    let health = metrics.snapshot(&HashMap::new());
    assert_eq!(health["reliable_push"]["completions"]["buffered_total"], 1);
    assert_eq!(health["reliable_push"]["completions"]["dropped_total"], 1);
    buffer_push_frame(
        &metrics,
        &mut buffered,
        key,
        completion_frame_with_session(TASK, SESSION),
    );
    purge_completion_root(&metrics, &root, &buffered, &RetryBuffer::new());
    assert_eq!(metrics.completion_delivery.health()["dropped_total"], 2);
    assert!(metrics
        .completion_delivery
        .state
        .lock()
        .unwrap()
        .admitted
        .is_empty());
}

#[test]
fn unacked_completion_reoffers_only_after_every_recipient_route_closes() {
    let (_dir, root) = test_root("completion-offered-route-lifetime");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "direct", "opencode");
    persisted_completion(storage.path(), &root, &owner);
    let registry = BgTaskRegistry::default();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let completion = {
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        registry
            .pending_completions_for_session(SESSION)
            .pop()
            .unwrap()
            .into_push_frame()
    };
    let metrics = DispatchPathMetrics::new();
    let (writer, mut output) = mpsc::channel(8);
    let first = route_key(1, 1);
    let second = route_key(2, 1);
    let next = route_key(3, 1);
    let mut routes = HashMap::from([(first, owner.clone()), (second, owner.clone())]);
    let mut channels = HashMap::from([(root.clone(), HashSet::from([first, second]))]);
    let mut buffered = HashMap::new();
    process_reliable_push_frame(
        &writer,
        &metrics,
        &routes,
        &channels,
        &HashMap::new(),
        &mut RetryBuffer::new(),
        &mut buffered,
        &mut CompletedTaskIds::default(),
        root.clone(),
        completion,
        None,
    );
    let sent = HashSet::from([
        output.try_recv().unwrap().header.channel,
        output.try_recv().unwrap().header.channel,
    ]);
    assert_eq!(sent, HashSet::from([1, 2]));
    insert_route_channel(&mut routes, &mut channels, next, owner.clone());
    for closing in [first, second] {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut buffered,
            &root,
            None,
        );
        assert!(output.try_recv().is_err(), "an old recipient is still open");
        remove_route_channel(&mut routes, &mut channels, closing);
        metrics.completion_delivery.route_closed(closing);
    }
    for _ in 0..2 {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut buffered,
            &root,
            None,
        );
    }
    assert_eq!(output.try_recv().unwrap().header.channel, 3);
    assert!(output.try_recv().is_err());
    {
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        assert_eq!(
            registry.ack_completions_for_session(Some(SESSION), &[TASK.into()]),
            vec![TASK]
        );
    }
    remove_route_channel(&mut routes, &mut channels, next);
    metrics.completion_delivery.route_closed(next);
    insert_route_channel(&mut routes, &mut channels, route_key(3, 2), owner);
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(
        output.try_recv().is_err(),
        "acked task must not reoffer on a new epoch"
    );
    registry.detach();
}

#[test]
fn closing_recipient_does_not_duplicate_an_outstanding_retry_copy() {
    let (_dir, root) = test_root("completion-closing-recipient-retry");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "direct", "opencode");
    persisted_completion(storage.path(), &root, &owner);
    let registry = BgTaskRegistry::default();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let completion = {
        let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
        registry
            .pending_completions_for_session(SESSION)
            .pop()
            .unwrap()
            .into_push_frame()
    };
    let metrics = DispatchPathMetrics::new();
    let (writer, mut output) = mpsc::channel(1);
    let mut routes = HashMap::from([
        (route_key(1, 1), owner.clone()),
        (route_key(2, 1), owner.clone()),
    ]);
    let mut channels = HashMap::from([(
        root.clone(),
        HashSet::from([route_key(1, 1), route_key(2, 1)]),
    )]);
    let mut buffered = HashMap::new();
    let mut retry = RetryBuffer::new();
    process_reliable_push_frame(
        &writer,
        &metrics,
        &routes,
        &channels,
        &HashMap::new(),
        &mut retry,
        &mut buffered,
        &mut CompletedTaskIds::default(),
        root.clone(),
        completion,
        None,
    );
    let sent = output.try_recv().unwrap();
    let closing = route_key(sent.header.channel, sent.header.epoch);
    remove_route_channel(&mut routes, &mut channels, closing);
    metrics.completion_delivery.route_closed(closing);
    insert_route_channel(&mut routes, &mut channels, route_key(3, 1), owner);
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(
        output.try_recv().is_err(),
        "registry duplicated the queued retry"
    );
    assert_eq!(
        drain_retry_buffers_for_bound_routes(&writer, &metrics, &routes, &mut retry),
        1
    );
    assert_ne!(output.try_recv().unwrap().header.channel, 3);
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut buffered,
        &root,
        None,
    );
    assert!(output.try_recv().is_err());
    registry.detach();
}

#[test]
fn direct_plugin_callers_acknowledge_completions() {
    let (_dir, root) = test_root("plugin-completion-ack-policy");
    for harness in ["opencode", "pi"] {
        let route = tests::caller_route(&root, "direct", harness);
        assert!(caller_acknowledges_completions(
            route.task_identity().principal.as_deref()
        ));
    }
}

#[test]
fn broca_runner_caller_does_not_acknowledge_completions() {
    let (_dir, root) = test_root("broca-completion-ack-policy");
    let route = tests::caller_route(&root, "reserved:broca", "runner");
    assert!(!caller_acknowledges_completions(
        route.task_identity().principal.as_deref()
    ));
}

#[test]
fn legacy_and_unverified_callers_keep_explicit_ack_policy() {
    assert!(caller_acknowledges_completions(None));
    assert!(caller_acknowledges_completions(Some("unverified")));
}

#[test]
fn runner_notice_recovery_retries_same_id_and_stays_delivered_after_restart() {
    let (_dir, root) = test_root("notice-restart");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "reserved:thalamus-harness", "opencode2");
    let task_id = format!("bash-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    persisted_completion_at(storage.path(), &root, &owner, &task_id, None);
    let registry = BgTaskRegistry::default();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let capture = super::super::runner_notices::test_endpoint::capture(&task_id, true);
    let metrics = DispatchPathMetrics::new();
    let (writer, mut frames) = mpsc::channel(16);
    let routes = HashMap::from([(route_key(1, 1), owner)]);
    let channels = HashMap::from([(root.clone(), HashSet::from([route_key(1, 1)]))]);
    for _ in 0..2 {
        recover_registry_completions(
            &writer,
            &metrics,
            &registry,
            &routes,
            &channels,
            &mut HashMap::new(),
            &root,
            None,
        );
    }
    let first = capture
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let retry = capture
        .receiver
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    assert_eq!(first.harness, "opencode2");
    assert_eq!(first.request, retry.request);
    assert_eq!(
        first.request["params"]["send_id"],
        format!("aft:bash:{task_id}")
    );
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    let paths =
        crate::bash_background::persistence::resolve_task(storage.path(), SESSION, &task_id)
            .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !read_task_at(&paths).unwrap().completion_delivered {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(read_task_at(&paths).unwrap().runner_completion_offers, 2);
    registry.detach();
    let restarted = BgTaskRegistry::default();
    restarted.replay_session(storage.path(), SESSION).unwrap();
    recover_registry_completions(
        &writer,
        &DispatchPathMetrics::new(),
        &restarted,
        &routes,
        &channels,
        &mut HashMap::new(),
        &root,
        None,
    );
    assert!(capture.receiver.try_recv().is_err());
    assert!(frames.try_recv().is_err());
    assert!(restarted
        .pending_completions_for_session(SESSION)
        .is_empty());
}

#[test]
fn retired_thalamus_pi_completion_is_unsendable_without_frames_or_retries() {
    let (_dir, root) = test_root("retired-runner");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "reserved:thalamus-pi", "pi");
    let task_id = format!("bash-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    persisted_completion_at(storage.path(), &root, &owner, &task_id, None);
    let registry = BgTaskRegistry::default();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let metrics = DispatchPathMetrics::new();
    let (writer, mut frames) = mpsc::channel(16);
    let routes = HashMap::from([(route_key(1, 1), owner)]);
    let channels = HashMap::from([(root.clone(), HashSet::from([route_key(1, 1)]))]);
    recover_registry_completions(
        &writer,
        &metrics,
        &registry,
        &routes,
        &channels,
        &mut HashMap::new(),
        &root,
        None,
    );
    completion_settlement::wait_for_settlements();
    let paths =
        crate::bash_background::persistence::resolve_task(storage.path(), SESSION, &task_id)
            .unwrap();
    assert!(read_task_at(&paths).unwrap().completion_delivered);
    assert_eq!(read_task_at(&paths).unwrap().runner_completion_offers, 0);
    assert!(frames.try_recv().is_err());
}
