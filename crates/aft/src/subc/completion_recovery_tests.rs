use super::super::test_support::*;
use super::*;
use crate::bash_background::persistence::{
    create_task_layout, read_task_at, write_task_at, PersistedTask,
};
use crate::bash_background::{BgTaskRegistry, BgTaskStatus};

const TASK: &str = "bash-0000000000000001";
const SESSION: &str = "moved-session";

fn persisted_completion(storage: &Path, root: &ProjectRootId, owner: &RouteIdentity) {
    let _scope = crate::bash_background::task_identity_scope(Some(owner.task_identity()));
    let task = create_task_layout(storage, SESSION, TASK).unwrap();
    let mut metadata = PersistedTask::starting(
        TASK.into(),
        SESSION.into(),
        "echo durable".into(),
        root.as_path().into(),
        Some(root.as_path().into()),
        None,
        true,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    assert!(!metadata.completion_delivered);
    write_task_at(&task, &metadata).unwrap();
}

#[test]
fn opencode2_completion_churn_recovers_from_registry_once_per_connection() {
    let (_dir, root) = test_root("opencode2-completion-churn");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "reserved:thalamus-harness", "opencode2");
    let core = tests::caller_route(&root, "reserved:prefrontal-core", "opencode2");
    let wrong_harness = tests::caller_route(&root, "reserved:thalamus-harness", "opencode");
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
    // same session, and a second runner uses a different harness.
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
fn opencode2_completion_survives_restart_and_late_configure_recovery_until_ack() {
    let (_dir, root) = test_root("opencode2-completion-restart");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "reserved:thalamus-harness", "opencode2");
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
fn opencode2_bind_before_configure_tail_recovers_unacked_completion() {
    let (_dir, root) = test_root("opencode2-late-completion-recovery");
    let storage = tempfile::tempdir().unwrap();
    let owner = tests::caller_route(&root, "reserved:thalamus-harness", "opencode2");
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
    let owner = tests::caller_route(&root, "reserved:thalamus-harness", "opencode2");
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
}
