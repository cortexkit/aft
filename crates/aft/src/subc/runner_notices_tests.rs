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
        Self::new_runner(task, offers, "reserved:broca", "broca")
    }

    fn new_runner(task: &str, offers: usize, principal: &str, raw: &str) -> Self {
        use crate::bash_background::persistence::{
            create_task_layout, write_task_at, PersistedTask,
        };
        let (root_dir, root) = super::super::test_support::test_root("runner-notices");
        let storage = tempfile::tempdir().unwrap();
        let mut route = super::super::test_support::route_identity_with_harness(
            &root,
            task,
            BindTrust::FirstParty,
            raw,
        );
        if let AuthenticatedPrincipal::RouteBind {
            principal_id,
            harness,
            ..
        } = &mut Arc::get_mut(&mut route.0).unwrap().spawn_principal
        {
            *principal_id = Some(principal.into());
            *harness = raw.into();
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
        metadata.launching_raw_harness = Some(raw.into());
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

    fn frozen(&self) -> SendRequest {
        use crate::bash_background::persistence::{read_task_at, resolve_task};
        let _scope = crate::bash_background::task_identity_scope(Some(self.route.task_identity()));
        let paths = resolve_task(self.storage.path(), &self.task, &self.task).unwrap();
        let persisted = read_task_at(&paths)
            .unwrap()
            .runner_completion_notice
            .unwrap();
        let in_memory = self
            .registry
            .notice_metadata(&self.task)
            .unwrap()
            .runner_completion_notice
            .unwrap();
        assert_eq!(envelope(&persisted), envelope(&in_memory));
        persisted
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
fn runner_notices_startup_replay_offers_only_named_pending_runner_completions() {
    let task = Completion::new("bash-00000000000b0007", 100);
    let capture = test_endpoint::capture(&task.task, false);
    let replayed = vec![(
        task.task.clone(),
        vec!["bash-not-present".into(), task.task.clone()],
    )];
    let _scope = crate::bash_background::task_identity_scope(None);
    offer_replayed(task.registry.clone(), &[(task.task.clone(), vec![])]);
    assert!(capture.receiver.try_recv().is_err());
    offer_replayed(task.registry.clone(), &replayed);
    capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler().unwrap(), &task.task, true);
    assert!(task.delivered());
    offer_replayed(task.registry.clone(), &replayed);
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_consumed_terminal_result_suppresses_stale_completion_frame() {
    for (id, ack_before_offer) in [
        ("bash-00000000000b0008", true),
        ("bash-00000000000b0009", false),
    ] {
        let task = Completion::new(id, 0);
        let frame = task.frame();
        let capture = test_endpoint::capture(&task.task, false);
        let scheduler = Scheduler::default();
        let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
        if ack_before_offer {
            task.registry
                .ack_terminal_result_for_session(&task.task, &task.task);
        }
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            frame,
            PathBuf::new(),
            false,
            move || 0
        ));
        if !ack_before_offer {
            task.registry
                .ack_terminal_result_for_session(&task.task, &task.task);
        }
        assert!(task.delivered());
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        assert!(
            capture.receiver.try_recv().is_err(),
            "a consumed result must not be announced again"
        );
        assert_eq!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_completion_offers,
            0
        );
    }
}

struct HeldCompletion {
    task: Completion,
    db: Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    key: crate::db::call_ledger::Key,
}

impl HeldCompletion {
    fn new(id: &str) -> Self {
        use crate::db::call_ledger as ledger;
        let task = Completion::new_runner(id, 0, "reserved:thalamus-harness", "opencode2");
        let db = Arc::new(std::sync::Mutex::new(
            crate::db::open(&task.storage.path().join("aft.db")).unwrap(),
        ));
        task.registry.set_harness(crate::harness::Harness::Runner);
        task.registry.set_db_pool(db.clone());
        let key = ledger::Key {
            carrier: "reserved:thalamus-harness".into(),
            call_key: format!("held-{id}"),
        };
        let registered = task.registry.task_for_test(id).unwrap();
        let mut state = registered.lock_state().unwrap();
        state.metadata.call_key = Some(crate::bash_background::TaskCallKey {
            requester: key.carrier.clone(),
            key: key.call_key.clone(),
            minted: false,
        });
        let paths =
            crate::bash_background::persistence::resolve_task(task.storage.path(), id, id).unwrap();
        std::fs::write(&paths.stdout, "recovered held output\n").unwrap();
        let conn = db.lock().unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &state.metadata.to_bash_task_row("runner", &paths).unwrap(),
        )
        .unwrap();
        let scope = ledger::ScopeIdentity {
            owner: "reserved:prefrontal-core".into(),
            scope_ref: id.into(),
            scope_epoch: 1,
        };
        ledger::admit(
            &conn,
            &key,
            "held-command",
            Some(&scope),
            ledger::State::DispatchStarted,
        )
        .unwrap();
        ledger::link_task(&conn, &key, id).unwrap();
        ledger::hold_running(
            &conn,
            &key,
            id,
            br#"{"content":[],"held":{"reason":"running"}}"#,
            false,
        )
        .unwrap();
        drop(conn);
        drop(state);
        Self { task, db, key }
    }

    fn result(&self) -> serde_json::Value {
        let conn = self.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &self.key)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, crate::db::call_ledger::State::Settled);
        serde_json::from_str(&row.late_entry.unwrap()).unwrap()
    }

    fn offer(&self, scheduler: &Scheduler) {
        self.task
            .offer(scheduler, Arc::new(AtomicU64::new(self.task.finished)));
    }
}

#[test]
fn runner_notices_recovered_held_call_after_route_reopens_delivers_only_call_result() {
    let held = HeldCompletion::new("bash-00000000000b0101");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    // Retain an already-queued completion across the runner's route restart.
    held.offer(&scheduler);
    route_unbound(&held.task.route);
    let reopened = held.task.route.clone();
    route_bound(held.task.registry.clone(), &reopened);
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(held.result()["result"]["content"]
        .to_string()
        .contains("recovered held output"));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    wait_for(super::scheduler().unwrap(), &held.task.task, true);
    assert!(
        capture.receiver.try_recv().is_err(),
        "the real result already reached the held call"
    );
}

#[test]
fn runner_notices_recovered_held_call_cancelled_before_completion_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0102");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    {
        let conn = held.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &held.key)
            .unwrap()
            .unwrap();
        let withdrawal = crate::db::call_ledger::withdraw(
            &conn,
            &held.key,
            row.scope.as_ref(),
            crate::db::call_ledger::now_ms(),
            1_000_000,
        )
        .unwrap();
        assert!(matches!(
            withdrawal,
            crate::db::call_ledger::Withdrawal::Answer {
                kill_task: Some(_),
                ..
            }
        ));
    }
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(
        capture.receiver.try_iter().count(),
        1,
        "withdrawn calls need their completion notice"
    );
}

#[test]
fn runner_notices_recovered_plain_background_task_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0103");
    let capture = test_endpoint::capture(&held.task.task, false);
    {
        let conn = held.db.lock().unwrap();
        // A background launch has already settled its call with task guidance,
        // not a command result or a retained foreground hold.
        let frame = crate::db::call_ledger::RecordedFrame {
            ty: FrameType::Response,
            body: br#"{"content":[{"type":"text","text":"Background task started"}]}"#.to_vec(),
        };
        crate::db::call_ledger::settle(
            &conn,
            &held.key,
            &frame,
            "ok",
            None,
            crate::db::call_ledger::now_ms(),
        )
        .unwrap();
    }
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
}

#[test]
fn runner_notices_recovered_held_settlement_races_send_with_one_delivery() {
    let held = HeldCompletion::new("bash-00000000000b0104");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let registry = held.task.registry.clone();
        let task = held.task.task.clone();
        let start = &start;
        threads.spawn(move || {
            start.wait();
            registry.settle_detached_call(&task);
        });
        start.wait();
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
    });
    assert!(held.result()["result"]["content"]
        .to_string()
        .contains("recovered held output"));
    assert_eq!(capture.receiver.try_iter().count(), 0);
}

#[test]
fn runner_notices_recovered_held_failed_settlement_releases_deferred_notice() {
    let held = HeldCompletion::new("bash-00000000000b0105");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let conn = held.db.lock().unwrap();
    conn.execute_batch("CREATE TRIGGER fail_held_settlement BEFORE UPDATE OF state ON call_ledger WHEN NEW.state='Settled' BEGIN SELECT RAISE(ABORT, 'injected settlement failure'); END;").unwrap();
    std::thread::scope(|threads| {
        let sending = threads.spawn(|| step(&scheduler, std::time::Instant::now()));
        assert!(
            capture
                .receiver
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "notice waits for the settlement decision"
        );
        drop(conn);
        assert!(matches!(sending.join().unwrap(), Next::Done));
    });
    assert_eq!(
        capture.receiver.try_iter().count(),
        1,
        "failed recovery must not swallow the only result delivery"
    );
}

#[test]
fn runner_notices_decode_open_states_and_error_envelopes() {
    for state in [
        "pending",
        "active",
        "finished",
        "paused",
        "future_notice_state",
    ] {
        let bytes = serde_json::to_vec(&json!({"result":{"state":state,"notice":true}})).unwrap();
        assert!(matches!(decode_send_reply(&bytes), Outcome::Accepted));
    }
    for body in [
        json!({"error":{"code":"session_released","message":"gone"}}),
        json!({"code":"session_released","message":"gone"}),
    ] {
        assert!(
            matches!(decode_send_reply(&serde_json::to_vec(&body).unwrap()),
            Outcome::Final(failure) if failure.code.as_deref() == Some("session_released") && failure.detail.contains("gone"))
        );
    }
    assert!(
        matches!(decode_send_reply(br#"{"error":{"code":"module_warming","message":"starting"}}"#),
        Outcome::Retry(failure) if failure.code.as_deref() == Some("module_warming"))
    );
    assert!(
        matches!(decode_send_reply(br#"{"result":{"status":"pending"}}"#),
        Outcome::Retry(failure) if failure.class == FailureClass::ReplyDecode && failure.detail.contains("state"))
    );
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
            return retry(FailureClass::TestEndpoint, "response lost");
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
            retry(FailureClass::TestEndpoint, "offline"),
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
fn runner_notices_delivery_refusal_decodes_field_from_wire_and_module_errors() {
    let request = request("aft:bash:field".into(), "completion".into(), true, false);
    for field in [json!("delivery"), json!("prompt"), Value::Null, json!(7)] {
        let error =
            json!({"code": "invalid_params", "message": "refused", "detail": {"field": field}});
        let outcomes = [
            decode_send_reply(&serde_json::to_vec(&error).unwrap()),
            decode_send_reply(&serde_json::to_vec(&json!({"error": error})).unwrap()),
            request_failure(CallError::Module(serde_json::from_value(error).unwrap())),
        ];
        for outcome in outcomes {
            assert_eq!(
                idle_start_fallback(&request, &outcome).is_some(),
                field == "delivery"
            );
        }
    }
}

#[test]
fn runner_notices_completion_delivery_skips_session_head() {
    for (id, principal, raw, delivery) in [
        (
            "bash-00000000000b0f01",
            "reserved:thalamus-harness",
            "opencode2",
            json!(["tool_result", "steer", "idle_start"]),
        ),
        (
            "bash-00000000000b0f02",
            "reserved:broca",
            "broca",
            json!(["tool_result", "steer"]),
        ),
    ] {
        let task = Completion::new_runner(id, 0, principal, raw);
        let capture = test_endpoint::capture(&task.task, false);
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let calls: Vec<_> = capture.receiver.try_iter().collect();
        assert!(!calls
            .iter()
            .any(|call| call.request["method"] == "session.head"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].request["method"], "session.send");
        assert_eq!(calls[0].request["params"]["delivery"], delivery);
        assert_eq!(envelope(&task.frozen()), calls[0].bytes);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_delivery_refusal_freezes_fallback_before_retry() {
    let task = Completion::new_runner(
        "bash-00000000000b0f03",
        0,
        "reserved:thalamus-harness",
        "pi",
    );
    let capture = test_endpoint::capture(&task.task, true);
    capture.refuse("invalid_params", Some("delivery"));
    let scheduler = Scheduler::default();
    let before = health().2;
    task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
    let now = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, now) else {
        panic!("lost fallback response must retry");
    };
    let first = capture.receiver.try_recv().unwrap();
    let second = capture.receiver.try_recv().unwrap();
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(
        first.request["params"]["delivery"],
        json!(["tool_result", "steer", "idle_start"])
    );
    let mut expected = first.request;
    expected["params"]["delivery"] = json!(["tool_result", "steer"]);
    assert_eq!(second.request, expected);
    assert_eq!(
        second.request["params"]["send_id"],
        format!("aft:bash:{}", task.task)
    );
    assert_eq!(second.request["params"]["notice"], true);
    assert_eq!(envelope(&task.frozen()), second.bytes);
    assert!(health().2 > before);
    assert!(!task.delivered());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(matches!(step(&scheduler, now + delay), Next::Done));
    let third = capture.receiver.try_recv().unwrap();
    assert_eq!(third.bytes, second.bytes);
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(task.delivered());
}

#[test]
fn runner_notices_unrelated_refusals_do_not_remove_idle_start() {
    for (index, code, field) in [
        (0, "invalid_params", Some("prompt")),
        (1, "invalid_params", None),
        (2, "scope_owner_mismatch", None),
        (3, "delivery_unsupported", Some("delivery")),
    ] {
        let id = format!("bash-{:016x}", 0xb0f10 + index);
        let task = Completion::new_runner(&id, 0, "reserved:thalamus-harness", "pi");
        let capture = test_endpoint::capture(&task.task, false);
        capture.refuse(code, field);
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let first = capture.receiver.try_recv().unwrap();
        assert_eq!(
            first.request["params"]["delivery"],
            json!(["tool_result", "steer", "idle_start"])
        );
        assert_eq!(envelope(&task.frozen()), first.bytes);
        assert!(capture.receiver.try_recv().is_err(), "{code} {field:?}");
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_delivery_refusal_falls_back_only_once_and_never_for_workers() {
    for worker in [true, false] {
        let task = if worker {
            Completion::new("bash-00000000000b0f20", 0)
        } else {
            Completion::new_runner(
                "bash-00000000000b0f21",
                0,
                "reserved:thalamus-harness",
                "pi",
            )
        };
        let capture = test_endpoint::capture(&task.task, false);
        capture.refuse("invalid_params", Some("delivery"));
        capture.refuse("invalid_params", Some("delivery"));
        let scheduler = Scheduler::default();
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let calls: Vec<_> = capture.receiver.try_iter().collect();
        assert_eq!(calls.len(), if worker { 1 } else { 2 });
        let last = calls.last().unwrap();
        assert_eq!(
            last.request["params"]["delivery"],
            json!(["tool_result", "steer"])
        );
        assert_eq!(envelope(&task.frozen()), last.bytes);
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_address_never_uses_runner_alias() {
    let broca = owner("reserved:broca", "runner");
    assert_eq!(
        Address::new(&broca, "runner".into(), None)
            .unwrap()
            .identity
            .harness,
        "broca"
    );
    assert_eq!(
        Address::new(&broca, "opencode2".into(), None)
            .unwrap()
            .identity
            .harness,
        "opencode2"
    );
    let mut head = owner("reserved:thalamus-harness", "runner");
    head.session = "unknown-concrete-head".into();
    assert!(Address::new(&head, "runner".into(), None).is_none());
    for raw in ["opencode2", "pi", "claude-code"] {
        assert_eq!(
            Address::new(&head, raw.into(), None)
                .unwrap()
                .identity
                .harness,
            raw
        );
    }
}

#[test]
fn runner_notices_legacy_thalamus_waits_for_concrete_session_binding() {
    let mut task = Completion::new_runner(
        "bash-00000000000b0010",
        100,
        "reserved:thalamus-harness",
        "runner",
    );
    let capture = test_endpoint::capture(&task.task, false);
    assert!(enqueue(task.registry.clone(), task.frame()));
    wait_for(scheduler().unwrap(), &task.task, false);
    assert!(!task.delivered());
    assert_eq!(
        task.registry
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_offers,
        100
    );
    assert!(capture.receiver.try_recv().is_err());

    let route = Arc::get_mut(&mut task.route.0).unwrap();
    route.harness = "opencode2".into();
    if let AuthenticatedPrincipal::RouteBind { harness, .. } = &mut route.spawn_principal {
        *harness = "opencode2".into();
    }
    route_bound(task.registry.clone(), &task.route);
    let received = capture
        .receiver
        .recv_timeout(Duration::from_millis(500))
        .unwrap();
    wait_for(scheduler().unwrap(), &task.task, true);
    assert_eq!(received.harness, "opencode2");
    assert!(task.delivered());
    assert_eq!(
        Address::new(&task.route.task_identity(), "pi".into(), None)
            .unwrap()
            .identity
            .harness,
        "pi"
    );
    route_unbound(&task.route);
    assert!(
        Address::new(&task.route.task_identity(), "runner".into(), None).is_none(),
        "a closed route is not a harness source"
    );
}

#[test]
fn runner_notices_wire_params_have_no_model_or_plan() {
    for worker in [true, false] {
        let body: Value = serde_json::from_slice(&envelope(&request(
            "aft:bash:keys".into(),
            "completion".into(),
            true,
            worker,
        )))
        .unwrap();
        let keys: std::collections::BTreeSet<_> = body["params"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from(["prompt", "send_id", "notice", "delivery"])
        );
    }
}
