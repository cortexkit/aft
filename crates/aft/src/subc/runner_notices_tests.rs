use super::*;

#[test]
fn bus_runner_targeted_not_delivered_applies_before_ack_and_uses_new_id() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let task = Completion::new("bash-00000000000fb001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    let first = capture.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    capture.reply(undelivered("handover_failed"));
    let mut receipts = scheduler.bus_wake(Some(&task.frozen().send_id));
    assert_eq!(receipts.len(), 1);
    assert!(matches!(
        receipts[0].try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    let Next::Retry(delay) = step(&scheduler, std::time::Instant::now()) else {
        panic!("new generation remains scheduled")
    };
    assert_eq!(receipts[0].try_recv(), Ok(()));
    let status = capture.receiver.try_recv().unwrap();
    assert_eq!(status.request["method"], "session.send_status");
    assert_eq!(
        status.request["params"]["send_id"],
        first.request["params"]["send_id"]
    );
    assert_eq!(task.frozen().send_id, format!("aft:bash:{}:2", task.task));
    due = std::time::Instant::now();
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let resent = capture.receiver.try_recv().unwrap();
    assert_ne!(
        resent.request["params"]["send_id"],
        first.request["params"]["send_id"]
    );
    assert!(task.delivered());
}

#[test]
fn bus_runner_unknown_or_absent_id_wakes_bounded_queue_and_detach_restores_schedule() {
    let first = Completion::new("bash-00000000000fb002", 0);
    let second = Completion::new("bash-00000000000fb003", 0);
    let a = test_endpoint::capture(&first.task, false);
    let b = test_endpoint::capture(&second.task, false);
    for capture in [&a, &b] {
        capture.send_status(true);
        capture.reply(SendReply::new("pending"));
    }
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(first.finished.max(second.finished)));
    first.offer(&scheduler, clock.clone());
    second.offer(&scheduler, clock.clone());
    let due = std::time::Instant::now();
    for _ in 0..2 {
        assert!(
            matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5))
        );
    }
    a.receiver.try_recv().unwrap();
    b.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    let mut targeted = scheduler.bus_wake(Some(&first.frozen().send_id));
    a.reply(SendReply::new("pending"));
    assert!(
        matches!(step(&scheduler, std::time::Instant::now()), Next::Retry(delay) if delay == Duration::from_secs(600))
    );
    assert_eq!(targeted[0].try_recv(), Ok(()));
    a.receiver.try_recv().unwrap();
    assert!(
        b.receiver.try_recv().is_err(),
        "known send id narrows the lookup"
    );
    scheduler.attachment_changed(false);
    a.reply(SendReply::new("pending"));
    b.reply(SendReply::new("pending"));
    let mut restored = Vec::new();
    for _ in 0..2 {
        let Next::Retry(delay) = step(&scheduler, std::time::Instant::now()) else {
            panic!("pending sends stay followed")
        };
        restored.push(delay.as_secs());
    }
    restored.sort_unstable();
    assert_eq!(
        restored,
        vec![30, 120],
        "detach restores each send's original follow schedule"
    );
    a.receiver.try_recv().unwrap();
    b.receiver.try_recv().unwrap();
    scheduler.attachment_changed(true);
    for id in [Some("unknown"), None] {
        let mut receipts = scheduler.bus_wake(id);
        assert_eq!(receipts.len(), 2);
        a.reply(SendReply::new("pending"));
        b.reply(SendReply::new("pending"));
        for _ in 0..2 {
            assert!(
                matches!(step(&scheduler, std::time::Instant::now()), Next::Retry(delay) if delay == Duration::from_secs(600))
            );
        }
        for receipt in &mut receipts {
            assert_eq!(receipt.try_recv(), Ok(()));
        }
        assert_eq!(
            a.receiver.try_recv().unwrap().request["method"],
            "session.send_status"
        );
        assert_eq!(
            b.receiver.try_recv().unwrap().request["method"],
            "session.send_status"
        );
    }
    assert_eq!(pending_delay(0), Duration::from_secs(5));
    assert_eq!(pending_delay(1), Duration::from_secs(30));
    assert_eq!(pending_delay(2), Duration::from_secs(120));
}
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

    fn reminder(&self, elapsed_ms: u64) -> PushFrame {
        let mut frame = crate::protocol::BashLongRunningFrame::new(
            &self.task,
            &self.task,
            "echo durable",
            elapsed_ms,
        );
        frame.launching_identity = Some(self.route.task_identity());
        PushFrame::BashLongRunning(frame)
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

    fn restart(&mut self) {
        self.registry.detach();
        self.registry = crate::bash_background::BgTaskRegistry::default();
        self.registry
            .replay_session(self.storage.path(), &self.task)
            .unwrap();
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

fn advance_notice(
    scheduler: &Scheduler,
    clock: &AtomicU64,
    due: &mut std::time::Instant,
    delay: Duration,
) -> Next {
    clock.fetch_add(delay.as_millis() as u64, Ordering::Relaxed);
    *due += delay;
    step(scheduler, *due)
}

fn undelivered(reason: &str) -> SendReply {
    SendReply::new("not_delivered").with_reason(reason)
}

// Serialize the tests that assert process-wide not_delivered counter deltas.
static DELIVERY_COUNTER_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn runner_notices_not_delivered_completion_uses_new_generation_and_same_body() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let task = Completion::new("bash-00000000000f0001", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.delivery_order(Some(true));
    capture.reply(undelivered("handover_failed"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, due) else {
        panic!("must retry")
    };
    assert_eq!(delay, Duration::from_millis(250));
    assert!(!task.delivered());
    let first = capture.receiver.try_recv().unwrap();
    let frozen = task.frozen();
    assert_eq!(frozen.send_id, format!("aft:bash:{}:2", task.task));
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let second = capture.receiver.try_recv().unwrap();
    assert_ne!(
        first.request["params"]["send_id"],
        second.request["params"]["send_id"]
    );
    let mut expected = first.request;
    expected["params"]["send_id"] = json!(frozen.send_id);
    assert_eq!(expected, second.request);
    assert_eq!(
        second.request["params"]["dedupe_key"],
        format!("aft:bash-task:{}", task.task)
    );
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    assert!(task.delivered());
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 1);
}

#[test]
fn runner_notices_not_delivered_restart_replays_persisted_new_id() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let mut task = Completion::new("bash-00000000000f0002", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.reply(undelivered("routing_refused"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, due) else {
        panic!("must retry")
    };
    capture.receiver.try_recv().unwrap();
    let frozen = envelope(&task.frozen());
    drop(scheduler);
    task.restart();
    let scheduler = Scheduler::default();
    task.offer(&scheduler, clock.clone());
    due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(left) if left == delay));
    assert!(
        capture.receiver.try_recv().is_err(),
        "restart must respect persisted backoff"
    );
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, delay),
        Next::Done
    ));
    let replay = capture.receiver.try_recv().unwrap();
    assert_eq!(replay.bytes, frozen);
    assert_eq!(
        replay.request["params"]["send_id"],
        format!("aft:bash:{}:2", task.task)
    );
    assert!(capture.receiver.try_recv().is_err());
}

#[test]
fn runner_notices_not_delivered_cap_is_five_and_is_retired_durably() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let mut task = Completion::new("bash-00000000000f0003", 0);
    let capture = test_endpoint::capture(&task.task, false);
    for _ in 0..5 {
        capture.reply(undelivered("handover_failed"));
    }
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    let mut logs = Vec::new();
    for generation in 1..=5 {
        let (next, lines) = crate::logging::capture_log_lines(|| step(&scheduler, due));
        logs.extend(lines);
        let call = capture.receiver.try_recv().unwrap();
        let expected = if generation == 1 {
            format!("aft:bash:{}", task.task)
        } else {
            format!("aft:bash:{}:{generation}", task.task)
        };
        assert_eq!(call.request["params"]["send_id"], expected);
        if generation < 5 {
            let Next::Retry(delay) = next else {
                panic!("retired too soon")
            };
            clock.fetch_add(delay.as_millis() as u64, Ordering::Relaxed);
            due += delay;
        } else {
            assert!(matches!(next, Next::Done));
        }
    }
    assert!(capture.receiver.try_recv().is_err());
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 5);
    assert_eq!(logs.iter().filter(|line| line.starts_with("ERROR ")).collect::<Vec<_>>(), vec![&format!("ERROR runner notice task={} session={} not_delivered after 5 generations reason=handover_failed", task.task, task.task)]);
    assert!(task.delivered());
    let _scope = crate::bash_background::task_identity_scope(Some(task.route.task_identity()));
    assert!(matches!(
        task.registry
            .notice_metadata(&task.task)
            .unwrap()
            .runner_completion_delivery
            .unwrap()
            .phase,
        RunnerCompletionPhase::Retired
    ));
    task.restart();
    assert!(task
        .registry
        .pending_completions_for_session(&task.task)
        .is_empty());
}

#[test]
fn runner_notices_not_delivered_reminder_drops_once_without_resend() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    let before = NOT_DELIVERED.load(Ordering::Relaxed);
    let task = Completion::new("bash-00000000000f0004", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.reply(undelivered("handover_failed"));
    let scheduler = Scheduler::default();
    assert!(offer(
        &scheduler,
        task.registry.clone(),
        task.reminder(600_000),
        PathBuf::new(),
        false,
        || 0
    ));
    let (next, logs) =
        crate::logging::capture_log_lines(|| step(&scheduler, std::time::Instant::now()));
    assert!(matches!(next, Next::Done));
    assert!(scheduler.entries.lock().unwrap().is_empty());
    assert_eq!(capture.receiver.try_iter().count(), 1);
    assert_eq!(
        logs,
        vec![format!(
            "INFO runner reminder task={} not_delivered reason=handover_failed; dropped",
            task.task
        )]
    );
    assert_eq!(NOT_DELIVERED.load(Ordering::Relaxed) - before, 1);
}

#[test]
fn runner_notices_pending_check_not_delivered_sends_exactly_one_new_generation() {
    let _guard = DELIVERY_COUNTER_TEST.lock().unwrap();
    for (index, typed) in [true, false].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000f001{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        capture.send_status(typed);
        capture.reply(SendReply::new("pending"));
        capture.reply(undelivered("handover_failed"));
        let scheduler = Scheduler::default();
        let clock = Arc::new(AtomicU64::new(task.finished));
        task.offer(&scheduler, clock.clone());
        let mut due = std::time::Instant::now();
        assert!(
            matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5))
        );
        let first = capture.receiver.try_recv().unwrap();
        assert!(
            capture.receiver.try_recv().is_err(),
            "pending initially sends only once"
        );
        assert!(!task.delivered());
        let Next::Retry(delay) =
            advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5))
        else {
            panic!("must retry new generation")
        };
        let check = capture.receiver.try_recv().unwrap();
        assert_eq!(
            check.request["params"]["send_id"],
            first.request["params"]["send_id"]
        );
        if typed {
            assert_eq!(check.request["method"], "session.send_status");
        } else {
            assert_eq!(check.bytes, first.bytes);
        }
        assert!(matches!(
            advance_notice(&scheduler, &clock, &mut due, delay),
            Next::Done
        ));
        assert_eq!(
            capture.receiver.try_recv().unwrap().request["params"]["send_id"],
            format!("aft:bash:{}:2", task.task)
        );
        assert!(capture.receiver.try_recv().is_err());
        assert!(task.delivered());
    }
}

#[test]
fn runner_notices_pending_stays_durable_until_24h_without_new_send() {
    let task = Completion::new("bash-00000000000f0020", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let before = PENDING_UNRESOLVED.load(Ordering::Relaxed);
    let mut due = std::time::Instant::now();
    let mut next = step(&scheduler, due);
    assert_eq!(
        capture.receiver.try_recv().unwrap().request["method"],
        "session.send"
    );
    let mut delays = Vec::new();
    let mut logs = Vec::new();
    while let Next::Retry(delay) = next {
        assert!(!task.delivered());
        delays.push(delay.as_secs());
        let (result, lines) = crate::logging::capture_log_lines(|| {
            advance_notice(&scheduler, &clock, &mut due, delay)
        });
        next = result;
        logs.extend(lines);
    }
    assert_eq!(&delays[..4], &[5, 30, 120, 600]);
    assert_eq!(clock.load(Ordering::Relaxed), task.finished + MAX_AGE_MS);
    let calls: Vec<_> = capture.receiver.try_iter().collect();
    assert_eq!(calls.len(), 146);
    assert!(calls
        .iter()
        .all(|call| call.request["method"] == "session.send_status"
            && call.request["params"]["send_id"] == format!("aft:bash:{}", task.task)));
    assert_eq!(PENDING_UNRESOLVED.load(Ordering::Relaxed) - before, 1);
    assert_eq!(
        logs,
        vec![format!(
            "INFO runner notice task={} session={} send still pending after 24 hours",
            task.task, task.task
        )]
    );
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_restart_keeps_next_check_and_never_resends() {
    let mut task = Completion::new("bash-00000000000f0021", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    capture.reply(SendReply::new("finished"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    capture.receiver.try_recv().unwrap();
    drop(scheduler);
    task.restart();
    let scheduler = Scheduler::default();
    task.offer(&scheduler, clock.clone());
    due = std::time::Instant::now();
    assert!(matches!(step(&scheduler, due), Next::Retry(delay) if delay == Duration::from_secs(5)));
    assert!(capture.receiver.try_recv().is_err());
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)),
        Next::Done
    ));
    assert_eq!(
        capture.receiver.try_recv().unwrap().request,
        json!({"method":"session.send_status", "params":{"send_id":format!("aft:bash:{}", task.task)}})
    );
    assert!(capture.receiver.try_recv().is_err());
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_status_failures_retry_once_then_resume_schedule() {
    let task = Completion::new("bash-00000000000f0022", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.send_status(true);
    capture.reply(SendReply::new("pending"));
    let scheduler = Scheduler::default();
    let clock = Arc::new(AtomicU64::new(task.finished));
    task.offer(&scheduler, clock.clone());
    let mut due = std::time::Instant::now();
    step(&scheduler, due);
    capture.receiver.try_recv().unwrap();
    capture.refuse("invalid_params", None);
    capture.retry(1);
    assert!(
        matches!(advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(5)), Next::Retry(delay) if delay == Duration::from_millis(250))
    );
    assert!(
        matches!(advance_notice(&scheduler, &clock, &mut due, Duration::from_millis(250)), Next::Retry(delay) if delay == Duration::from_secs(30))
    );
    assert!(!task.delivered());
    capture.reply(SendReply::new("finished"));
    assert!(matches!(
        advance_notice(&scheduler, &clock, &mut due, Duration::from_secs(30)),
        Next::Done
    ));
    let calls: Vec<_> = capture.receiver.try_iter().collect();
    assert_eq!(calls.len(), 3);
    assert!(calls
        .iter()
        .all(|call| call.request["method"] == "session.send_status"));
    assert!(task.delivered());
}

#[test]
fn runner_notices_pending_followup_stops_only_on_receipt_or_contract_final_state() {
    for state in ["finished", "superseded", "expired", "template_part_gone"] {
        assert!(matches!(
            decode_reply(&reply_body(&SendReply::new(state)).unwrap(), true),
            Outcome::Accepted
        ));
    }
    for state in ["pending", "active", "paused", "future_state"] {
        assert!(matches!(
            decode_reply(&reply_body(&SendReply::new(state)).unwrap(), true),
            Outcome::Pending
        ));
    }
    let reply = SendReply::new("active")
        .with_delivered(cortexkit_role_llm_runner::send::Delivered::new("step").with_ref("row-1"));
    assert!(matches!(
        decode_reply(&reply_body(&reply).unwrap(), true),
        Outcome::Accepted
    ));
}

fn wait_for(scheduler: &Scheduler, task: &str, finished: bool) {
    let id = format!("aft:bash:{task}");
    wait_for_id(scheduler, &id, finished);
}

fn wait_for_id(scheduler: &Scheduler, id: &str, finished: bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut entries = scheduler.entries.lock().unwrap();
    loop {
        if if finished {
            !entries.contains_key(id)
        } else {
            entries
                .get(id)
                .is_some_and(|entry| entry.retrying && entry.work.is_some())
        } {
            return;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !left.is_zero(),
            "notice did not reach scheduled state: {id}"
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
        crate::bash_background::persistence::write_task_at(&paths, &state.metadata).unwrap();
        std::fs::write(&paths.paths.stdout, "recovered held output\n").unwrap();
        let conn = db.lock().unwrap();
        crate::db::bash_tasks::upsert_bash_task(
            &conn,
            &state
                .metadata
                .to_bash_task_row("runner", &paths.paths)
                .unwrap(),
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
        let page: serde_json::Value = serde_json::from_slice(
            &crate::db::call_ledger::late_results(
                &conn,
                &self.key.carrier,
                "held-notice-test",
                None,
                None,
                crate::db::call_ledger::now_ms(),
                1_000_000,
            )
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["call_key"], self.key.call_key);
        entries[0].clone()
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
    let mut reopened = super::super::test_support::route_identity_with_harness(
        &held.task.route.root,
        &held.task.task,
        BindTrust::FirstParty,
        "opencode2",
    );
    Arc::get_mut(&mut reopened.0).unwrap().spawn_principal =
        held.task.route.spawn_principal.clone();
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
fn runner_notices_recovered_held_call_withdrawn_before_completion_keeps_notice() {
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
fn runner_notices_recovered_held_call_cancelled_before_completion_keeps_notice() {
    let held = HeldCompletion::new("bash-00000000000b0106");
    let capture = test_endpoint::capture(&held.task.task, false);
    let scheduler = Scheduler::default();
    held.offer(&scheduler);
    let cancelled = crate::db::call_ledger::RecordedFrame {
        ty: FrameType::Error,
        body: br#"{"code":"cancelled","message":"call cancelled by runner"}"#.to_vec(),
    };
    crate::db::call_ledger::settle(
        &held.db.lock().unwrap(),
        &held.key,
        &cancelled,
        "cancelled",
        None,
        crate::db::call_ledger::now_ms(),
    )
    .unwrap();
    held.task.registry.settle_detached_call(&held.task.task);
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    assert_eq!(capture.receiver.try_iter().count(), 1);
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
    {
        let conn = held.db.lock().unwrap();
        let row = crate::db::call_ledger::get(&conn, &held.key)
            .unwrap()
            .unwrap();
        let retained: serde_json::Value = serde_json::from_slice(&row.frame.unwrap().body).unwrap();
        assert_eq!(
            retained["_aft_recovery_notice"], true,
            "startup must retain the fallback lane"
        );
        conn.execute_batch("DROP TRIGGER fail_held_settlement")
            .unwrap();
    }
    wait_for_id(
        super::scheduler().unwrap(),
        &format!("aft:bash-recovery:{}", held.task.task),
        true,
    );
    let result = held.result();
    assert_eq!(result["result"]["errorCode"], "outcome_unknown");
    assert_eq!(result["result"]["content"][0]["text"], "");
    assert!(
        capture.receiver.try_recv().is_err(),
        "ending the hold must not send a second notice"
    );
}

#[test]
fn runner_notices_recovered_notice_fallback_survives_restart_observation() {
    let held = HeldCompletion::new("bash-00000000000b0107");
    {
        let conn = held.db.lock().unwrap();
        conn.execute_batch("CREATE TRIGGER fail_fallback_end BEFORE UPDATE OF state ON call_ledger WHEN NEW.state='Settled' BEGIN SELECT RAISE(ABORT, 'injected fallback failure'); END;").unwrap();
        assert!(crate::db::call_ledger::settle_task_notice_fallback(
            &conn,
            &held.key,
            &held.task.task
        )
        .is_err());
        conn.execute_batch("DROP TRIGGER fail_fallback_end")
            .unwrap();
    }
    let reopened = crate::db::open(&held.task.storage.path().join("aft.db")).unwrap();
    let real = crate::db::call_ledger::RecordedFrame {
        ty: FrameType::Response,
        body: br#"{"content":[{"type":"text","text":"real output must stay in the notice"}]}"#
            .to_vec(),
    };
    crate::db::call_ledger::observe_task(
        &reopened,
        &held.key.carrier,
        &held.key.call_key,
        &held.task.task,
        Some(&real),
        "ok",
    )
    .unwrap();
    assert_eq!(held.result()["result"]["errorCode"], "outcome_unknown");
}

#[test]
fn runner_notices_distinguishes_queued_submission_from_delivered_notice() {
    for following in [false, true] {
        assert!(matches!(
            decode_reply(
                br#"{"result":{"state":"pending","submission_id":"notice-1"}}"#,
                following,
            ),
            Outcome::Pending
        ));
        assert!(matches!(
            decode_reply(
                br#"{"result":{"state":"active","delivered":{"as":"step","ref":"notice-1"}}}"#,
                following,
            ),
            Outcome::Accepted
        ));
    }
}

#[test]
fn runner_notices_decode_open_states_and_error_envelopes() {
    assert!(matches!(
        decode_send_reply(br#"{"result":{"state":"pending"}}"#),
        Outcome::Pending
    ));
    for (reason, expected) in [
        (Some("handover_failed"), "handover_failed"),
        (Some("future_reason"), "future_reason"),
    ] {
        let mut reply = json!({"state":"not_delivered"});
        if let Some(reason) = reason {
            reply["reason"] = json!(reason);
        }
        let bytes = cortexkit_role_llm_runner::wire::reply_body(&reply).unwrap();
        assert!(
            matches!(decode_send_reply(&bytes), Outcome::NotDelivered(reason) if reason == expected)
        );
    }
    for state in ["active", "finished", "paused", "future_notice_state"] {
        let bytes = serde_json::to_vec(&json!({"result":{"state":state,"notice":true}})).unwrap();
        assert!(matches!(decode_send_reply(&bytes), Outcome::Accepted));
    }
    assert!(
        matches!(decode_send_reply(br#"{"result":{"state":"not_delivered"}}"#), Outcome::Retry(failure) if failure.class == FailureClass::ReplyDecode)
    );
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
        let response = reply_body(&SendReply::new("active").with_delivered(
            cortexkit_role_llm_runner::send::Delivered::new("step").with_ref("send-7"),
        ))
        .unwrap();
        decode_send_reply(&response)
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
fn runner_notices_session_unconverted_is_final_but_transient_retries() {
    let request = request("aft:bash:bash-7".into(), "completion".into(), true, false);
    for (code, final_refusal) in [("session_unconverted", true), ("transient", false)] {
        let mut runner = FakeRunner {
            refuse: Some(code),
            ..Default::default()
        };
        let outcome = runner.call(&envelope(&request));
        if final_refusal {
            assert!(
                matches!(&outcome, Outcome::Final(failure) if failure.code.as_deref() == Some(code))
            );
        } else {
            assert!(
                matches!(&outcome, Outcome::Retry(failure) if failure.code.as_deref() == Some(code))
            );
        }
        let next = deliver(&request.send_id, 0, true, outcome, &mut false);
        assert_eq!(matches!(next, Next::Done), final_refusal);
        if !final_refusal {
            assert!(matches!(next, Next::Retry(delay) if delay == Duration::from_millis(250)));
        }
        assert_eq!(runner.calls.len(), 1);
    }
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
        // A head scope is never a worker, whichever runner carries it: Broca
        // hosts heads too, and their completions must be able to wake them.
        let head_stamp = subc_protocol::scope::ScopeStamp::new(
            stamp.owner.clone(),
            "head-7",
            3,
            subc_protocol::scope::ScopeKind::Head,
        );
        let head = Address::new(&owner, raw.into(), Some(&head_stamp)).unwrap();
        assert!(!head.worker, "{principal} head");
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

#[test]
fn runner_notices_delivery_order_capability_cache_is_incarnation_scoped() {
    let mut cache = RunnerCapabilities::default();
    let first = RunnerIncarnation {
        daemon: "daemon-1".into(),
        generation: 1,
    };
    let restarted = RunnerIncarnation {
        generation: 2,
        ..first.clone()
    };
    let daemon_restarted = RunnerIncarnation {
        daemon: "daemon-2".into(),
        generation: 1,
    };
    let path = Path::new("connection.json");
    assert_eq!(cache.get(path, "broca", &first, "delivery_order"), None);
    cache.insert(path, "broca", first.clone(), true, "delivery_order");
    assert_eq!(
        cache.get(path, "broca", &first, "delivery_order"),
        Some(true)
    );
    assert_eq!(
        cache.get(path, "thalamus-harness", &first, "delivery_order"),
        None
    );
    assert_eq!(
        cache.get(Path::new("other.json"), "broca", &first, "delivery_order"),
        None
    );
    assert_eq!(cache.get(path, "broca", &restarted, "delivery_order"), None);
    assert_eq!(
        cache.get(path, "broca", &daemon_restarted, "delivery_order"),
        None
    );
    cache.insert(path, "broca", restarted.clone(), false, "delivery_order");
    assert_eq!(
        cache.get(path, "broca", &restarted, "delivery_order"),
        Some(false)
    );
    assert_eq!(cache.get(path, "broca", &first, "delivery_order"), None);
    for (reply, expected) in [
        (
            json!({"capabilities":["steer","delivery_order"]}),
            Some(true),
        ),
        (json!({"capabilities":["steer"]}), Some(false)),
        (json!({"capabilities":"delivery_order"}), None),
        (json!({"capabilities":["delivery_order", 1]}), None),
        (json!({"error":{"code":"unavailable"}}), None),
        (json!({}), None),
    ] {
        assert_eq!(
            described_delivery_order(&cortexkit_role_llm_runner::wire::reply_body(&reply).unwrap()),
            expected
        );
    }
    assert_eq!(
        described_delivery_order(br#"{"capabilities":["delivery_order"]}"#),
        None
    );
}

#[test]
fn runner_notices_optional_members_are_gated_and_task_choice_is_frozen() {
    for (index, capability) in [None, Some(false), Some(true)].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000e000{index}"), 0);
        let capture = test_endpoint::capture(&task.task, false);
        capture.delivery_order(capability);
        let scheduler = Scheduler::default();
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            task.reminder(600_321),
            PathBuf::new(),
            false,
            || 0
        ));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let reminder = capture.receiver.try_recv().unwrap();
        let metadata = task.registry.notice_metadata(&task.task).unwrap();
        let policy = metadata.runner_notice_policy.unwrap();
        assert_eq!(policy.dedupe, capability == Some(true));
        assert_eq!(policy.reminder_interval_ms, 600_000);
        assert_eq!(metadata.runner_reminder_elapsed_ms, Some(600_321));
        let mut expected = json!({"method":"session.send","params":{
            "prompt":super::super::bash_notice_text::long_running(&serde_json::to_value(match task.reminder(600_321) { PushFrame::BashLongRunning(frame) => frame, _ => unreachable!() }).unwrap()),
            "send_id":format!("aft:bash-reminder:{}:600321", task.task),
            "notice":true,"delivery":["tool_result","steer"]
        }});
        if capability == Some(true) {
            expected["params"]["dedupe_key"] = json!(format!("aft:bash-task:{}", task.task));
            expected["params"]["expires_at_ms"] = json!(metadata.started_at + 1_200_321);
        }
        assert_eq!(reminder.request, expected);
        // Later discovery and configuration cannot change an accepted send's identity.
        capture.delivery_order(if capability == Some(true) {
            None
        } else {
            Some(true)
        });
        task.registry.configure_long_running_reminders(true, 1_000);
        let registered = task.registry.task_for_test(&task.task).unwrap();
        let recent = std::time::Instant::now() - Duration::from_secs(10);
        *registered.last_reminder_at.lock().unwrap() = Some(recent);
        task.registry.maybe_emit_long_running_reminder(&registered);
        assert_eq!(
            *registered.last_reminder_at.lock().unwrap(),
            Some(recent),
            "the frozen interval must also govern actual scheduling"
        );
        let completion_text = super::super::bash_notice_text::completion(
            &serde_json::to_value(task.frame()).unwrap(),
        );
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        assert!(matches!(
            step(&scheduler, std::time::Instant::now()),
            Next::Done
        ));
        let completion = capture.receiver.try_recv().unwrap();
        let mut expected = json!({"prompt":completion_text,"send_id":format!("aft:bash:{}", task.task),
            "notice":true,"delivery":["tool_result","steer"]});
        if capability == Some(true) {
            expected["dedupe_key"] = json!(format!("aft:bash-task:{}", task.task));
        }
        assert_eq!(
            completion.request,
            json!({"method":"session.send","params":expected})
        );
        assert!(completion.request["params"].get("expires_at_ms").is_none());
        assert_eq!(
            completion.request["params"].get("dedupe_key"),
            reminder.request["params"].get("dedupe_key")
        );
        assert_eq!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .reminder_interval_ms,
            600_000
        );
    }
}

#[test]
fn runner_notices_reminder_retry_and_restart_replay_preserve_bytes() {
    let task = Completion::new("bash-00000000000e0010", 0);
    let capture = test_endpoint::capture(&task.task, false);
    capture.delivery_order(Some(true));
    capture.retry(1);
    let scheduler = Scheduler::default();
    assert!(offer(
        &scheduler,
        task.registry.clone(),
        task.reminder(600_123),
        PathBuf::new(),
        false,
        || 0
    ));
    let now = std::time::Instant::now();
    let Next::Retry(delay) = step(&scheduler, now) else {
        panic!("must retry")
    };
    let first = capture.receiver.try_recv().unwrap();
    capture.delivery_order(None);
    task.registry.configure_long_running_reminders(true, 1);
    assert!(matches!(step(&scheduler, now + delay), Next::Done));
    let second = capture.receiver.try_recv().unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);

    task.registry.detach();
    let restarted = crate::bash_background::BgTaskRegistry::default();
    restarted
        .replay_session(task.storage.path(), &task.task)
        .unwrap();
    let metadata = restarted.notice_metadata(&task.task).unwrap();
    let id = format!(
        "aft:bash-reminder:{}:{}",
        task.task,
        metadata.runner_reminder_elapsed_ms.unwrap()
    );
    // The saved identity, not time since restart, supplies the replay's elapsed value.
    let elapsed = id.rsplit(':').next().unwrap().parse().unwrap();
    assert!(offer(
        &scheduler,
        restarted.clone(),
        task.reminder(elapsed),
        PathBuf::new(),
        false,
        || u64::MAX
    ));
    assert!(matches!(
        step(&scheduler, std::time::Instant::now()),
        Next::Done
    ));
    let replay = capture.receiver.try_recv().unwrap();
    assert_eq!(first.bytes, replay.bytes);
    assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
    restarted.detach();
}

#[test]
fn runner_notices_member_refusal_drops_reminder_and_freezes_completion_fallback() {
    for (index, field) in ["dedupe_key", "expires_at_ms"].into_iter().enumerate() {
        let task = Completion::new(&format!("bash-00000000000e002{index}"), 0);
        let capture = test_endpoint::capture(&task.task, true);
        capture.delivery_order(Some(true));
        capture.refuse("invalid_params", Some(field));
        let scheduler = Scheduler::default();
        assert!(offer(
            &scheduler,
            task.registry.clone(),
            task.reminder(600_000),
            PathBuf::new(),
            false,
            || 0
        ));
        let (next, logs) =
            crate::logging::capture_log_lines(|| step(&scheduler, std::time::Instant::now()));
        assert!(matches!(next, Next::Done));
        assert_eq!(logs, vec![format!("INFO runner reminder task={} send_id=aft:bash-reminder:{}:600000 dropped after optional member refusal", task.task, task.task)]);
        let reminder = capture.receiver.try_recv().unwrap();
        assert!(reminder.request["params"].get("dedupe_key").is_some());
        assert!(
            capture.receiver.try_recv().is_err(),
            "a refused reminder must not be resent"
        );
        assert_eq!(capture.effects.load(Ordering::Relaxed), 0);
        assert!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .dedupe
        );

        capture.refuse("invalid_params", Some(field));
        task.offer(&scheduler, Arc::new(AtomicU64::new(task.finished)));
        let now = std::time::Instant::now();
        let (next, logs) = crate::logging::capture_log_lines(|| step(&scheduler, now));
        let Next::Retry(delay) = next else {
            panic!("lost resend response must retry")
        };
        assert_eq!(logs, vec![format!("INFO runner notice task={} send_id=aft:bash:{} session={} retrying without dedupe_key/expires_at_ms after invalid_params refusal", task.task, task.task, task.task)]);
        let first = capture.receiver.try_recv().unwrap();
        let resend = capture.receiver.try_recv().unwrap();
        let mut expected = first.request;
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("dedupe_key");
        expected["params"]
            .as_object_mut()
            .unwrap()
            .remove("expires_at_ms");
        assert_eq!(resend.request, expected);
        assert_eq!(envelope(&task.frozen()), resend.bytes);
        assert!(capture.receiver.try_recv().is_err());
        assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
        assert!(matches!(step(&scheduler, now + delay), Next::Done));
        assert_eq!(capture.receiver.try_recv().unwrap().bytes, resend.bytes);
        assert_eq!(capture.effects.load(Ordering::Relaxed), 1);
        assert!(task.delivered());
        assert!(
            task.registry
                .notice_metadata(&task.task)
                .unwrap()
                .runner_notice_policy
                .unwrap()
                .dedupe
        );
    }
}
