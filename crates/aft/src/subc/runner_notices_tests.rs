use super::*;
use cortexkit_role_llm_runner::{
    send::SendReply,
    wire::{read_reply, reply_body},
};

fn owner(principal: &str, harness: &str) -> crate::bash_background::TaskIdentity {
    crate::bash_background::TaskIdentity {
        principal: Some(principal.into()),
        harness: harness.into(),
        session: "session-7".into(),
    }
}

struct Completion {
    _root: tempfile::TempDir,
    storage: tempfile::TempDir,
    registry: crate::bash_background::BgTaskRegistry,
    route: RouteIdentity,
    task: String,
    finished: u64,
}

impl Completion {
    fn new(task: &str, offers: usize) -> Self {
        use crate::bash_background::persistence::{
            create_task_layout, write_task_at, PersistedTask,
        };
        let (root_dir, root) = super::super::test_support::test_root("runner-notices");
        let storage = tempfile::tempdir().unwrap();
        let mut route = super::super::test_support::route_identity_with_harness(
            &root,
            task,
            BindTrust::FirstParty,
            "broca",
        );
        if let AuthenticatedPrincipal::RouteBind {
            principal_id,
            harness,
            ..
        } = &mut Arc::get_mut(&mut route.0).unwrap().spawn_principal
        {
            *principal_id = Some("reserved:broca".into());
            *harness = "broca".into();
        }
        let _scope = crate::bash_background::task_identity_scope(Some(route.task_identity()));
        let paths = create_task_layout(storage.path(), task, task).unwrap();
        let mut metadata = PersistedTask::starting(
            task.into(),
            task.into(),
            "echo durable".into(),
            root.as_path().into(),
            Some(root.as_path().into()),
            None,
            true,
            false,
        );
        metadata.mark_terminal(
            crate::bash_background::BgTaskStatus::Completed,
            Some(0),
            None,
        );
        metadata.launching_raw_harness = Some("broca".into());
        metadata.runner_completion_offers = offers;
        let finished = metadata.finished_at.unwrap();
        write_task_at(&paths, &metadata).unwrap();
        let registry = crate::bash_background::BgTaskRegistry::default();
        registry.replay_session(storage.path(), task).unwrap();
        Self {
            _root: root_dir,
            storage,
            registry,
            route,
            task: task.into(),
            finished,
        }
    }

    fn frame(&self) -> PushFrame {
        let _scope = crate::bash_background::task_identity_scope(Some(self.route.task_identity()));
        self.registry
            .pending_completions_for_session(&self.task)
            .pop()
            .unwrap()
            .into_push_frame()
    }

    fn delivered(&self) -> bool {
        use crate::bash_background::persistence::{read_task_at, resolve_task};
        let paths = resolve_task(self.storage.path(), &self.task, &self.task).unwrap();
        read_task_at(&paths).unwrap().completion_delivered
    }

    fn offer(&self, scheduler: &Scheduler, now: Arc<AtomicU64>) {
        assert!(offer(
            scheduler,
            self.registry.clone(),
            self.frame(),
            PathBuf::new(),
            false,
            move || now.load(Ordering::Relaxed)
        ));
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.registry.detach();
    }
}

fn step(scheduler: &Scheduler, now: std::time::Instant) -> Next {
    let (id, mut work) = Scheduler::take(&mut scheduler.entries.lock().unwrap(), now).unwrap();
    let next = work();
    let result = match &next {
        Next::Done => Next::Done,
        Next::Retry(delay) => Next::Retry(*delay),
    };
    scheduler.finish(&id, work, next, now);
    result
}

fn wait_for(scheduler: &Scheduler, task: &str, finished: bool) {
    let id = format!("aft:bash:{task}");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut entries = scheduler.entries.lock().unwrap();
    loop {
        if if finished {
            !entries.contains_key(&id)
        } else {
            entries
                .get(&id)
                .is_some_and(|entry| entry.retrying && entry.work.is_some())
        } {
            return;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "notice did not reach scheduled state: {task}"
        );
        entries = scheduler.wake.wait_timeout(entries, left).unwrap().0;
    }
}

#[test]
fn runner_notices_completion_survives_long_outage_and_acks_only_acceptance() {
    let task = Completion::new("bash-00000000000b0001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.retry(30);
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let start = std::time::Instant::now();
    let mut due = start;
    for attempt in 0..30 {
        clock.store(
            task.finished + due.duration_since(start).as_millis() as u64,
            Ordering::Relaxed,
        );
        let Next::Retry(delay) = step(&scheduler, due) else {
            panic!("retired attempt {attempt}")
        };
        assert!(delay <= Duration::from_secs(60));
        if attempt >= 8 {
            assert_eq!(delay, Duration::from_secs(60));
        }
        assert!(!task.delivered(), "retry must not ack attempt {attempt}");
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        due += delay;
    }
    assert!(due.duration_since(start) > Duration::from_secs(20 * 60));
    assert!(matches!(step(&scheduler, due), Next::Done));
    assert!(task.delivered());
    assert_eq!(capture.receiver.try_iter().count(), 31);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn runner_notices_completion_expires_from_finish_even_with_exhausted_old_budget() {
    let task = Completion::new("bash-00000000000b0002", 100);
    let capture = test_endpoint::capture(&task.task, false);
    let scheduler = Scheduler::default();
    let before = EXPIRED.load(Ordering::Relaxed);
    task.offer(
        &scheduler,
        Arc::new(AtomicU64::new(task.finished + MAX_AGE_MS)),
    );
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert!(task.delivered());
    assert!(EXPIRED.load(Ordering::Relaxed) > before);
    assert!(
        capture.receiver.try_recv().is_err(),
        "expired task must not be sent"
    );
}

#[test]
fn runner_notices_restart_offers_pending_completion_with_high_persisted_count() {
    let task = Completion::new("bash-00000000000b0003", 100);
    task.registry.detach();
    let restarted = crate::bash_background::BgTaskRegistry::default();
    restarted
        .replay_session(task.storage.path(), &task.task)
        .unwrap();
    let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
    let frame = restarted
        .pending_completions_for_session(&task.task)
        .pop()
        .unwrap()
        .into_push_frame();
    let capture = test_endpoint::capture(&task.task, false);
    let scheduler = Scheduler::default();
    let now = task.finished;
    assert!(offer(
        &scheduler,
        restarted.clone(),
        frame,
        PathBuf::new(),
        false,
        move || now
    ));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
    assert!(task.delivered());
    assert_eq!(
        restarted
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_offers,
        101
    );
    restarted.detach();
}

#[test]
fn runner_notices_route_bind_expedites_retry_without_parallel_send() {
    let task = Completion::new("bash-00000000000b0004", 100);
    let capture = test_endpoint::capture(&task.task, false);
    capture.retry(1);
    assert!(enqueue(task.registry.clone(), task.frame()));
    let scheduler = scheduler().unwrap();
    wait_for(scheduler, &task.task, false);
    let first = capture.receiver.try_recv().unwrap();
    let id = format!("aft:bash:{}", task.task);
    assert!(
        scheduler.entries.lock().unwrap()[&id].due
            > std::time::Instant::now() + Duration::from_secs(50)
    );
    let (entered, release) = capture.block_next();
    route_bound(task.registry.clone(), &task.route);
    entered.recv_timeout(Duration::from_millis(500)).unwrap();
    for _ in 0..20 {
        route_bound(task.registry.clone(), &task.route);
    }
    assert!(capture.receiver.try_recv().is_ok());
    assert!(capture.receiver.try_recv().is_err());
    release.send(()).unwrap();
    wait_for(scheduler, &task.task, true);
    assert!(task.delivered());
    assert_eq!(capture.max_active(), 1);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert_eq!(first.request["params"]["send_id"], id);
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_waiting_retries_do_not_occupy_workers() {
    let scheduler = scheduler().unwrap();
    let mut waiting = Vec::new();
    for n in 0..20 {
        let task = Completion::new(&format!("bash-{:016x}", 0xc0000 + n), 100);
        let capture = test_endpoint::capture(&task.task, false);
        capture.retry(u64::MAX);
        assert!(enqueue(task.registry.clone(), task.frame()));
        wait_for(scheduler, &task.task, false);
        waiting.push((task, capture));
    }
    let task = Completion::new("bash-00000000000b0005", 0);
    let capture = test_endpoint::capture(&task.task, false);
    assert!(enqueue(task.registry.clone(), task.frame()));
    capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler, &task.task, true);
    assert!(task.delivered());
    assert!(health().0 >= 20);
    for (task, _) in waiting {
        assert!(!task.delivered());
        scheduler
            .entries
            .lock()
            .unwrap()
            .remove(&format!("aft:bash:{}", task.task));
    }
}

#[test]
fn runner_notices_bounded_queue_defers_without_ack() {
    let scheduler = Scheduler::default();
    for n in 0..QUEUE_CAPACITY {
        assert!(scheduler.offer(n.to_string(), false, Box::new(|| Next::Done), false));
    }
    let task = Completion::new("bash-00000000000b0006", 0);
    let capture = test_endpoint::capture(&task.task, false);
    assert!(!offer(
        &scheduler,
        task.registry.clone(),
        task.frame(),
        PathBuf::new(),
        false,
        move || 0
    ));
    assert!(!task.delivered());
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(scheduler.entries.lock().unwrap().len(), QUEUE_CAPACITY);
}

#[test]
fn runner_alias_switch_preserves_task_ownership_and_persisted_keys() {
    let principal = |raw: &str| AuthenticatedPrincipal::RouteBind {
        trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
        route_channel: 1,
        route_epoch: 1,
        project_root: PathBuf::from("/project"),
        harness: raw.into(),
        session_id: "session-7".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let canonical =
        crate::bash_background::TaskIdentity::from_principal(&principal("runner")).unwrap();
    for raw in ["runner", "opencode2", "broca"] {
        let identity =
            crate::bash_background::TaskIdentity::from_principal(&principal(raw)).unwrap();
        assert_eq!(identity, canonical);
        assert_eq!(
            super::super::tool_provider::ledger_carrier(&principal(raw)),
            super::super::tool_provider::ledger_carrier(&principal("runner"))
        );
        let saved: crate::bash_background::TaskIdentity = serde_json::from_value(
            json!({"principal":"reserved:broca", "harness":raw, "session":"session-7"}),
        )
        .unwrap();
        assert_eq!(saved, canonical);
        assert_eq!(
            serde_json::to_vec(&identity).unwrap(),
            serde_json::to_vec(&canonical).unwrap()
        );
    }
}

#[derive(Default)]
struct FakeRunner {
    calls: Vec<Value>,
    stored: HashMap<String, Vec<u8>>,
    notices: usize,
    lose_first_response: bool,
    refuse: Option<&'static str>,
}

impl FakeRunner {
    fn bind(
        &self,
        address: &Address,
        versions: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), &'static str> {
        if versions != &std::collections::BTreeMap::from([("llm-runner".into(), "v1".into())]) {
            return Err("role_version_unsupported");
        }
        if !matches!(address.module.as_str(), "thalamus-harness" | "broca") {
            return Err("module_not_found");
        }
        let identity = serde_json::to_value(&address.identity).unwrap();
        assert!(identity.get("project_root").is_none());
        Ok(())
    }

    fn call(&mut self, bytes: &[u8]) -> Outcome {
        let body: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert_eq!(body["method"], "session.send");
        let request: SendRequest = serde_json::from_value(body["params"].clone()).unwrap();
        request.check_prompt().unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap()["notice"], true);
        self.calls.push(body);
        if let Some(code) = self.refuse {
            return refusal(code);
        }
        let bytes = serde_json::to_vec(&request).unwrap();
        if let Some(first) = self.stored.get(&request.send_id) {
            if first != &bytes {
                return refusal("send_id_reuse");
            }
        } else {
            self.notices += 1;
            self.stored.insert(request.send_id.clone(), bytes);
        }
        if self.lose_first_response && self.calls.len() == 1 {
            return Outcome::Retry("response_lost");
        }
        let response = reply_body(&SendReply::new("pending").with_submission_id("send-7")).unwrap();
        read_reply::<SendReply>(&response).unwrap();
        Outcome::Accepted
    }
}

#[test]
fn runner_notices_retry_unknown_outcome_with_same_id_and_dedupe() {
    let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
    let mut runner = FakeRunner {
        lose_first_response: true,
        ..Default::default()
    };
    let mut retry_logged = false;
    assert!(matches!(deliver(
        &request.send_id,
        0,
        true,
        runner.call(&envelope(&request)),
        &mut retry_logged,
    ), Next::Retry(delay) if delay == Duration::from_millis(250)));
    assert!(matches!(
        deliver(
            &request.send_id,
            1,
            true,
            runner.call(&envelope(&request)),
            &mut retry_logged
        ),
        Next::Done
    ));
    assert_eq!(runner.calls.len(), 2);
    assert_eq!(runner.notices, 1);
    assert_eq!(runner.calls[0], runner.calls[1]);
    assert!(retry_logged);
}

#[test]
fn runner_notices_final_refusals_stop_without_retry() {
    for code in [
        "scope_owner_mismatch",
        "expired",
        "session_released",
        "send_id_reuse",
        "delivery_unsupported",
        "invalid_params",
    ] {
        let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
        let mut runner = FakeRunner {
            refuse: Some(code),
            ..Default::default()
        };
        assert!(matches!(
            deliver(
                &request.send_id,
                0,
                true,
                runner.call(&envelope(&request)),
                &mut false,
            ),
            Next::Done
        ));
        assert_eq!(runner.calls.len(), 1, "{code}");
    }
}

#[test]
fn runner_notices_transients_keep_short_retry_budget() {
    for attempt in 0..MAX_ATTEMPTS {
        let next = deliver(
            "aft:warning:7",
            attempt,
            false,
            Outcome::Retry("offline"),
            &mut false,
        );
        assert_eq!(matches!(next, Next::Done), attempt + 1 == MAX_ATTEMPTS);
    }
}

#[test]
fn runner_notices_raw_harness_bind_back_and_worker_delivery() {
    let stamp = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "worker-7",
        3,
        subc_protocol::scope::ScopeKind::Worker,
    );
    for (principal, raw, worker) in [
        ("reserved:thalamus-harness", "opencode2", false),
        ("reserved:thalamus-harness", "pi", false),
        ("reserved:broca", "broca", true),
    ] {
        let owner = owner(principal, "runner");
        let address = Address::new(&owner, raw.into(), None).unwrap();
        assert_eq!(address.identity.harness, raw);
        assert_eq!(address.identity.session, "session-7");
        assert_eq!(address.worker, worker);
        let versions = role_versions();
        assert_eq!(
            serde_json::to_value(target(&address)).unwrap(),
            json!({"kind":"management_surface", "module_id":address.module})
        );
        let runner = FakeRunner::default();
        runner.bind(&address, &versions).unwrap();
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("llm-runner".into(), "v2".into())])
            )
            .is_err());
        assert!(runner
            .bind(
                &address,
                &std::collections::BTreeMap::from([("tool-provider".into(), "v1".into())])
            )
            .is_err());
        let request = request("aft:bash:7".into(), "text".into(), true, address.worker);
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["delivery"],
            if worker {
                json!(["tool_result", "steer"])
            } else {
                json!(["tool_result", "steer", "idle_start"])
            }
        );
        let scoped = Address::new(&owner, raw.into(), Some(&stamp)).unwrap();
        assert!(scoped.worker);
        assert_eq!(
            scoped.scope.unwrap(),
            ScopeSelector::new(stamp.owner.clone(), "worker-7", Some(3))
        );
    }
    assert!(Address::new(&owner("reserved:thalamus-pi", "pi"), "pi".into(), None).is_none());
    assert!(!is_runner(&owner("direct", "opencode")));
    assert!(!is_runner(&owner("direct", "pi")));
}

#[test]
fn runner_notices_only_completions_on_heads_may_start_idle_runs() {
    for completion in [true, false] {
        for worker in [true, false] {
            let request = request("test".into(), "text".into(), completion, worker);
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(
                value["delivery"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("idle_start")),
                completion && !worker
            );
        }
    }
    assert!(read_reply::<SendReply>(br#"{"result":{"state":"pending"},"extra":true}"#).is_err());
    assert!(read_reply::<SendReply>(br#"{"state":"pending"}"#).is_err());
}

#[test]
fn runner_notices_readable_sender_grant_can_disable_idle_start() {
    use cortexkit_role_llm_runner::read::{HeadMeta, HeadSender};
    let head = HeadMeta::empty(1, "incarnation");
    assert_eq!(idle_start_grant(&head), None);
    let limited = head.clone().with_sender(HeadSender::new(
        vec!["tool_result".into(), "steer".into()],
        false,
    ));
    assert_eq!(idle_start_grant(&limited), Some(false));
    let full = head.with_sender(HeadSender::new(
        vec!["tool_result".into(), "steer".into(), "idle_start".into()],
        false,
    ));
    assert_eq!(idle_start_grant(&full), Some(true));
}
