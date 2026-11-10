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

struct RunnerRecovery {
    storage: tempfile::TempDir,
    cutoff: u64,
    root: ProjectRootId,
    owner: RouteIdentity,
    registry: BgTaskRegistry,
    metrics: DispatchPathMetrics,
    writer: WriterSender,
    output: mpsc::Receiver<WriterFrame>,
    routes: HashMap<RouteChannel, RouteIdentity>,
    channels: HashMap<ProjectRootId, HashSet<RouteChannel>>,
    buffered: HashMap<ReplayKey, VecDeque<PushFrame>>,
    retry: RetryBuffer,
}

impl RunnerRecovery {
    fn new(root: &ProjectRootId, principal: &str, harness: &str, capacity: usize) -> Self {
        let storage = tempfile::tempdir().unwrap();
        let owner = tests::caller_route(root, principal, harness);
        let registry = BgTaskRegistry::default();
        let db = crate::db::open(&storage.path().join("aft.db")).unwrap();
        let cutoff = crate::bash_background::persistence::unix_millis();
        crate::db::state::set_host_state(
            &db,
            crate::db::state::RUNNER_COMPLETION_DELIVERY_CUTOVER,
            &cutoff.to_string(),
            cutoff as i64,
        )
        .unwrap();
        registry.set_db_pool(Arc::new(std::sync::Mutex::new(db)));
        let (writer, output) = mpsc::channel(capacity);
        let mut recovery = Self {
            storage,
            cutoff,
            root: root.clone(),
            owner,
            registry,
            metrics: DispatchPathMetrics::new(),
            writer,
            output,
            routes: HashMap::new(),
            channels: HashMap::new(),
            buffered: HashMap::new(),
            retry: RetryBuffer::new(),
        };
        recovery.recover();
        recovery
    }

    fn add_completion(&mut self, task: &str, finished_at: u64) -> PushFrame {
        persisted_completion_at(
            self.storage.path(),
            &self.root,
            &self.owner,
            task,
            Some(finished_at),
        );
        self.registry
            .replay_session(self.storage.path(), SESSION)
            .unwrap();
        let _scope = crate::bash_background::task_identity_scope(Some(self.owner.task_identity()));
        self.registry
            .pending_completions_for_session(SESSION)
            .into_iter()
            .find(|completion| completion.task_id == task)
            .unwrap()
            .into_push_frame()
    }

    fn recover(&mut self) {
        recover_registry_completions(
            &self.writer,
            &self.metrics,
            &self.registry,
            &self.routes,
            &self.channels,
            &mut self.buffered,
            &self.root,
            None,
        );
    }

    fn bind(&mut self, channel: RouteChannel) {
        insert_route_channel(
            &mut self.routes,
            &mut self.channels,
            channel,
            self.owner.clone(),
        );
        self.recover();
    }

    fn close(&mut self, channel: RouteChannel) {
        remove_route_channel(&mut self.routes, &mut self.channels, channel);
        self.metrics.completion_delivery.route_closed(channel);
    }

    fn live(&mut self, frame: PushFrame) {
        process_reliable_push_frame(
            &self.writer,
            &self.metrics,
            &self.routes,
            &self.channels,
            &HashMap::new(),
            &mut self.retry,
            &mut self.buffered,
            &mut CompletedTaskIds::default(),
            self.root.clone(),
            frame,
            None,
        );
    }

    fn assert_frame(&mut self, channel: RouteChannel, task: &str) {
        let frame = self
            .output
            .try_recv()
            .expect("completion frame was not queued");
        assert_eq!(route_key(frame.header.channel, frame.header.epoch), channel);
        assert_eq!(push_frame_task_id(&frame).as_deref(), Some(task));
        assert!(
            self.output.try_recv().is_err(),
            "duplicate completion frame"
        );
    }

    fn assert_delivered(&self, task: &str) {
        let paths =
            crate::bash_background::persistence::resolve_task(self.storage.path(), SESSION, task)
                .unwrap();
        assert!(read_task_at(&paths).unwrap().completion_delivered);
        assert!(!self
            .registry
            .pending_completion_ids()
            .values()
            .any(|tasks| tasks.contains(task)));
        assert_eq!(
            self.metrics.completion_delivery.health()["unacked_total"],
            0
        );
    }

    fn reconnect_three_times(&mut self, first: RouteChannel) {
        let mut previous = first;
        for epoch in 2..=4 {
            self.close(previous);
            let next = route_key(1, epoch);
            self.bind(next);
            assert!(
                self.output.try_recv().is_err(),
                "reconnect repeated a delivered completion"
            );
            previous = next;
        }
    }
}

impl Drop for RunnerRecovery {
    fn drop(&mut self) {
        self.registry.detach();
    }
}

#[test]
fn runner_live_completion_is_delivered_once_across_three_reconnects() {
    let (_dir, root) = test_root("runner-live-completion-once");
    for (principal, harness) in [
        ("reserved:thalamus-harness", "opencode2"),
        ("reserved:thalamus-harness", "pi"),
        ("reserved:broca", "runner"),
    ] {
        let mut recovery = RunnerRecovery::new(&root, principal, harness, 8);
        let first = route_key(1, 1);
        recovery.bind(first);
        let frame = recovery.add_completion(TASK, recovery.cutoff + 1);
        recovery.live(frame.clone());
        recovery.assert_frame(first, TASK);
        recovery.assert_delivered(TASK);
        recovery.reconnect_three_times(first);
        recovery.live(frame);
        assert!(
            recovery.output.try_recv().is_err(),
            "late live emission repeated delivery"
        );
        assert_eq!(
            recovery.metrics.completion_delivery.health()["unacked_total"],
            0
        );
    }
}

#[test]
fn runner_completion_without_route_is_sent_once_and_stays_delivered_after_restart() {
    let (_dir, root) = test_root("runner-deferred-completion-once");
    let mut recovery = RunnerRecovery::new(&root, "reserved:thalamus-harness", "opencode2", 8);
    let frame = recovery.add_completion(TASK, recovery.cutoff + 1);
    recovery.live(frame);
    assert!(recovery.output.try_recv().is_err());
    assert_eq!(
        recovery.metrics.completion_delivery.health()["unacked_total"],
        1
    );
    let first = route_key(1, 1);
    recovery.bind(first);
    recovery.assert_frame(first, TASK);
    recovery.assert_delivered(TASK);
    recovery.reconnect_three_times(first);

    recovery.registry.detach();
    recovery.metrics = DispatchPathMetrics::new();
    recovery.registry = BgTaskRegistry::default();
    let db = crate::db::open(&recovery.storage.path().join("aft.db")).unwrap();
    recovery
        .registry
        .set_db_pool(Arc::new(std::sync::Mutex::new(db)));
    assert_eq!(
        recovery
            .registry
            .runner_completion_delivery_cutover()
            .unwrap(),
        recovery.cutoff
    );
    recovery
        .registry
        .replay_session(recovery.storage.path(), SESSION)
        .unwrap();
    recovery.close(route_key(1, 4));
    recovery.bind(route_key(1, 5));
    assert!(
        recovery.output.try_recv().is_err(),
        "restart repeated a delivered completion"
    );
    recovery.assert_delivered(TASK);
}

#[test]
fn runner_backlog_before_cutover_is_retired_without_a_frame() {
    let (_dir, root) = test_root("runner-completion-backlog-cutover");
    let mut recovery = RunnerRecovery::new(&root, "reserved:thalamus-harness", "pi", 8);
    const NEW_TASK: &str = "bash-0000000000000002";
    persisted_completion_at(
        recovery.storage.path(),
        &root,
        &recovery.owner,
        TASK,
        Some(recovery.cutoff - 1),
    );
    let fresh = recovery.add_completion(NEW_TASK, recovery.cutoff + 1);
    let old = {
        let _scope =
            crate::bash_background::task_identity_scope(Some(recovery.owner.task_identity()));
        recovery
            .registry
            .pending_completions_for_session(SESSION)
            .into_iter()
            .find(|completion| completion.task_id == TASK)
            .unwrap()
            .into_push_frame()
    };
    // Live emissions can be queued in memory while their durable completions
    // are also pending. Recovery must suppress the old copy in both places.
    recovery.live(old.clone());
    recovery.live(fresh);
    let first = route_key(1, 1);
    recovery.bind(first);
    recovery.assert_frame(first, NEW_TASK);
    recovery.assert_delivered(TASK);
    recovery.assert_delivered(NEW_TASK);
    recovery.live(old);
    assert!(recovery.output.try_recv().is_err());
    recovery.reconnect_three_times(first);
}

#[test]
fn runner_retry_completion_is_delivered_only_when_writer_accepts_it() {
    let (_dir, root) = test_root("runner-retry-completion-once");
    let mut recovery = RunnerRecovery::new(&root, "reserved:broca", "runner", 1);
    let first = route_key(1, 1);
    recovery.bind(first);
    let frame = recovery.add_completion(TASK, recovery.cutoff + 1);
    assert!(matches!(
        try_send_push_frame(
            &recovery.writer,
            &recovery.metrics,
            first,
            &completion_frame("writer-filler")
        ),
        PushSendOutcome::Sent
    ));
    recovery.live(frame);
    assert_eq!(recovery.retry[&first].len(), 1);
    let paths =
        crate::bash_background::persistence::resolve_task(recovery.storage.path(), SESSION, TASK)
            .unwrap();
    assert!(
        !read_task_at(&paths).unwrap().completion_delivered,
        "backpressure is not delivery"
    );
    recovery.assert_frame(first, "writer-filler");
    assert_eq!(
        drain_retry_buffers_for_bound_routes(
            &recovery.writer,
            &recovery.metrics,
            &recovery.routes,
            &mut recovery.retry
        ),
        1
    );
    recovery.assert_frame(first, TASK);
    recovery.assert_delivered(TASK);
    recovery.reconnect_three_times(first);
}

#[test]
fn runner_failed_durable_delivery_stays_admitted_and_unacked() {
    let (_dir, root) = test_root("runner-completion-persist-failure");
    let mut recovery = RunnerRecovery::new(&root, "reserved:broca", "runner", 8);
    let first = route_key(1, 1);
    recovery.bind(first);
    let frame = recovery.add_completion(TASK, recovery.cutoff + 1);
    let json = {
        let _scope =
            crate::bash_background::task_identity_scope(Some(recovery.owner.task_identity()));
        recovery.registry.task_json_path(TASK, SESSION).unwrap()
    };
    let original = std::fs::read(&json).unwrap();
    std::fs::remove_file(&json).unwrap();
    std::fs::create_dir(&json).unwrap();
    recovery.live(frame.clone());
    recovery.assert_frame(first, TASK);
    assert!(recovery.registry.pending_completion_ids()[SESSION].contains(TASK));
    assert_eq!(
        recovery.metrics.completion_delivery.health()["unacked_total"],
        1
    );
    recovery.reconnect_three_times(first);
    recovery.live(frame);
    assert!(recovery.output.try_recv().is_err());
    assert_eq!(
        recovery.metrics.completion_delivery.health()["unacked_total"],
        1
    );
    std::fs::remove_dir(&json).unwrap();
    std::fs::write(&json, original).unwrap();
    let paths =
        crate::bash_background::persistence::resolve_task(recovery.storage.path(), SESSION, TASK)
            .unwrap();
    assert!(!read_task_at(&paths).unwrap().completion_delivered);
}

#[test]
fn runner_cutover_is_recorded_once_on_the_owning_database() {
    let storage = tempfile::tempdir().unwrap();
    let db = Arc::new(std::sync::Mutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    let registry = BgTaskRegistry::default();
    registry.set_db_pool(db.clone());
    let cutoff = registry.runner_completion_delivery_cutover().unwrap();
    assert!(cutoff > 0);
    assert_eq!(
        crate::db::state::get_host_state(
            &db.lock().unwrap(),
            crate::db::state::RUNNER_COMPLETION_DELIVERY_CUTOVER
        )
        .unwrap(),
        Some(cutoff.to_string())
    );
    assert_eq!(
        crate::db::state::runner_completion_delivery_cutover(&db.lock().unwrap(), cutoff + 1000)
            .unwrap(),
        cutoff,
        "cutover must not move on a later open"
    );
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
fn thalamus_runner_callers_do_not_acknowledge_completions() {
    let (_dir, root) = test_root("thalamus-completion-ack-policy");
    for (principal, harness) in [
        ("reserved:thalamus-harness", "opencode2"),
        ("reserved:thalamus-harness", "pi"),
        ("reserved:thalamus-pi", "pi"),
    ] {
        let route = tests::caller_route(&root, principal, harness);
        assert!(!caller_acknowledges_completions(
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
