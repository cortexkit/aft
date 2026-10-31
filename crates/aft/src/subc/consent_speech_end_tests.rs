struct ScopeEndControl {
    control: Arc<readiness::DaemonControl>,
    runs: Arc<run_scope::RunScopes>,
    frames: mpsc::Receiver<WriterFrame>,
    pending: readiness::PendingControlReplies,
}

impl ScopeEndControl {
    async fn bind_live(run: &mut RouteIdentity) -> Self {
        let (writer, frames) = mpsc::channel(8);
        let pending = readiness::PendingControlReplies::default();
        let control = Arc::new(readiness::DaemonControl::new(
            writer,
            Arc::new(DispatchPathMetrics::new()),
            PROTOCOL_VERSION,
            vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
            vec![],
            pending.clone(),
            HELLO_CORR + 1,
        ));
        let identity = Arc::get_mut(&mut run.0).unwrap();
        identity.scope.as_mut().unwrap().expires_at_ms = Some(ledger::now_ms() as u64 + 120_000);
        identity.run_scope = OnceLock::new();
        let runs = Arc::new(run_scope::RunScopes::new(control.clone()));
        runs.bind(run).unwrap();
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            9439,
            serde_json::to_vec(&json!({"name":"bash","arguments":{"command":"true"}})).unwrap(),
        )
        .unwrap();
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame,
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let (ready, mut queued) = mpsc::unbounded_channel();
        assert!(runs.defer(decoded, run, &ready).is_ok());
        let mut fixture = Self {
            control,
            runs,
            frames,
            pending,
        };
        let request = fixture.next().await;
        fixture.reply(
            &request,
            subc_protocol::scope::ScopeStatus::Live,
            run.scope.as_ref().unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(10), queued.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(run.run_scope.get().unwrap().cached_budget().is_ok());
        fixture
    }

    async fn next(&mut self) -> WriterFrame {
        let frame = tokio::time::timeout(Duration::from_secs(10), self.frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frame.header.ty,
            FrameType::Request,
            "{}",
            String::from_utf8_lossy(&frame.body)
        );
        frame
    }

    fn reply(
        &self,
        request: &WriterFrame,
        status: subc_protocol::scope::ScopeStatus,
        stamp: &subc_protocol::scope::ScopeStamp,
    ) {
        let live = status == subc_protocol::scope::ScopeStatus::Live;
        let answer = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
            status,
            scope_epoch: Some(stamp.scope_epoch),
            daemon_incarnation: "scope-end-test".into(),
            owner_synced: true,
            owner_configured: true,
            scope: live.then(|| stamp.clone()),
        };
        assert!(self.pending.resolve(
            Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&answer).unwrap()
            )
            .unwrap()
        ));
    }
}

struct ActorLaneBlock {
    release: Option<std::sync::mpsc::Sender<()>>,
    response: Option<oneshot::Receiver<Response>>,
}

impl Drop for ActorLaneBlock {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

async fn block_actor_lanes(f: &Fixture) -> Vec<ActorLaneBlock> {
    let (started, mut entered) = mpsc::unbounded_channel();
    let mut blocks = Vec::new();
    // Queue a blocker before cleanup on every actor, including executors that
    // serialize mutations globally and cannot start all blockers together.
    for (index, (root, _)) in f.executor.actor_entries().into_iter().enumerate() {
        let (release, released) = std::sync::mpsc::channel();
        let started = started.clone();
        let response = f.executor.submit_async(
            root,
            Lane::Mutating,
            format!("speech-end-block-{index}"),
            Box::new(move |_| {
                let _ = started.send(());
                let _ = released.recv();
                Response::success("speech-end-block", json!({}))
            }),
        );
        blocks.push(ActorLaneBlock {
            release: Some(release),
            response: Some(response),
        });
    }
    tokio::time::timeout(Duration::from_secs(10), entered.recv())
        .await
        .unwrap()
        .unwrap();
    blocks
}

struct SpeechEndWriteFault {
    db: Arc<StdMutex<crate::db::TrackedConnection>>,
    service: Arc<crate::gh_shim_speech::service::Service>,
    stamp: subc_protocol::scope::ScopeStamp,
}

impl SpeechEndWriteFault {
    fn clear(&self) {
        self.db
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER IF EXISTS reject_speech_end_write")
            .unwrap();
    }
}

impl Drop for SpeechEndWriteFault {
    fn drop(&mut self) {
        self.clear();
        // Let the service's real retry thread finish before the temporary store
        // disappears, including when a test assertion unwinds.
        let deadline = Instant::now() + Duration::from_secs(7);
        while Instant::now() < deadline {
            if self.service.store.scope_ended(&self.stamp).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_failed_speech_end_write_fences_received_approval_before_actor_admission() {
    use crate::gh_shim_speech::{model::LaunchAuthority, Code};
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let consent = Controlled::install(f.fake.clone());
    let mut run = program(&f, false, "run:audit-speech-write-failure");
    let mut end_control = ScopeEndControl::bind_live(&mut run).await;
    let stamp = run.scope.as_ref().unwrap().clone();
    let relay = crate::gh_shim_relay::GhRelay::new(f.dir.path().join("unused-connection.json"));
    let service = crate::gh_shim_speech::service::start(
        &f.ctx.storage_dir(),
        &relay,
        Arc::new(|_| Box::pin(async { Err("describe_unavailable".into()) })),
        "speech-end-test-generation".into(),
        true,
    );
    let gate = FinishGateOverride::install("subc-41-9442", false);
    END_FENCE_READ_DISPATCHES.store(0, Ordering::SeqCst);
    let mut replies = f
        .send_with(
            &run,
            &f.dir.path().join("elsewhere.txt"),
            9442,
            end_fence_read_dispatch,
        )
        .await;
    held(&mut replies).await;
    consent.answer(&consent.request(0).await, "allow_scope");
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    let db = f.ctx.db().unwrap();
    let ask = asks::for_scope(&db.lock().unwrap(), &access_grants::scope_key(&run))
        .unwrap()
        .pop()
        .unwrap();
    run.run_scope.get().unwrap().park_ask(ParkedAsk {
        call: ConsentCall {
            scope: serde_json::from_value(ask.selector).unwrap(),
            project_root: run.project_root.clone(),
            session: run.session.clone(),
            method: "consent.withdraw",
            params: json!({"elicitation_id":ask.elicitation_id.unwrap()}),
            timeout: UNARY_TIMEOUT,
        },
        key: ask.key,
        db: db.clone(),
    });
    db.lock().unwrap().execute_batch("CREATE TRIGGER reject_speech_end_write BEFORE INSERT ON gh_speech_scope_ends BEGIN SELECT RAISE(ABORT, 'speech end write blocked'); END;").unwrap();
    let fault = SpeechEndWriteFault {
        db: db.clone(),
        service: service.clone(),
        stamp: stamp.clone(),
    };
    let mut blocks = block_actor_lanes(&f).await;
    let (describes, mut ended, worker) = scope_describe::start_for_speech(
        end_control.control.clone(),
        f.executor.clone(),
        end_control.runs.clone(),
        service.clone(),
    );
    describes.goodbye(stamp.clone());
    let request = end_control.next().await;
    end_control.reply(&request, subc_protocol::scope::ScopeStatus::Ended, &stamp);
    let launch = LaunchAuthority {
        authority: run
            .run_owner
            .as_ref()
            .unwrap()
            .owner_route
            .scope
            .clone()
            .unwrap(),
        run: Some(stamp.clone()),
        origin: None,
        head_origin: None,
    };
    wait_for(|| {
        service
            .check_pending(None, &launch, &run.session, "local-read")
            .is_err_and(|error| error.code == Code::ScopeEnded)
    })
    .await;
    assert!(!service.store.scope_ended(&stamp).unwrap());
    assert_eq!(
        ledger::get(
            &db.lock().unwrap(),
            &tool_provider::LedgerEdge::key(&run, "program-9442")
        )
        .unwrap()
        .unwrap()
        .state,
        ledger::State::Prepared
    );
    let run_fenced = run.run_scope.get().unwrap().revoked();
    let registry_fenced = f
        .ctx
        .bash_background()
        .with_live_scope(&stamp, || ())
        .is_none();
    gate.release();
    wait_for(|| gate.0.completed.load(Ordering::SeqCst)).await;
    let grants_while_failing =
        crate::db::access_grants::list(&db.lock().unwrap(), &access_grants::policy_scope_key(&run))
            .unwrap();
    let entries_while_blocked = END_FENCE_READ_DISPATCHES.load(Ordering::SeqCst);
    let health_while_failing = end_control
        .control
        .metrics
        .scope_descriptions
        .lock()
        .unwrap()
        .cleanup_health();
    assert!(!service.store.scope_ended(&stamp).unwrap());
    fault.clear();
    for block in &mut blocks {
        if let Some(release) = block.release.take() {
            let _ = release.send(());
        }
    }
    for block in &mut blocks {
        if let Some(response) = block.response.take() {
            tokio::time::timeout(Duration::from_secs(10), response)
                .await
                .unwrap()
                .unwrap();
        }
    }
    wait_for(|| service.store.scope_ended(&stamp).unwrap()).await;
    tokio::time::timeout(Duration::from_secs(10), ended.recv())
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    assert!(
        run_fenced,
        "validated speech end must revoke the live run before its store write succeeds"
    );
    assert!(
        registry_fenced,
        "registry fencing must not wait for a mutating actor lane"
    );
    assert!(
        grants_while_failing.is_empty(),
        "a failing speech write allowed a post-end head grant"
    );
    assert_eq!(entries_while_blocked, 1);
    assert_eq!(health_while_failing["pending_scopes"], 1);
    assert!(health_while_failing["failed_attempts"].as_u64().unwrap() >= 1);
    wait_for(|| {
        ledger::get(
            &db.lock().unwrap(),
            &tool_provider::LedgerEdge::key(&run, "program-9442"),
        )
        .unwrap()
        .is_some_and(|row| row.state == ledger::State::Settled)
    })
    .await;
    assert_eq!(END_FENCE_READ_DISPATCHES.load(Ordering::SeqCst), 1);
    assert!(crate::db::access_grants::list(
        &db.lock().unwrap(),
        &access_grants::policy_scope_key(&run)
    )
    .unwrap()
    .is_empty());
}

#[tokio::test]
async fn audit_started_shell_is_killed_by_scope_cleanup_and_never_not_started() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    let mut run = program(&f, false, "run:audit-started-shell-end");
    let mut end_control = ScopeEndControl::bind_live(&mut run).await;
    let (command, _) = gated_command(&run.project_root);
    let mut replies = f
        .send_tool(
            &run,
            "bash",
            json!({"command":command,"workdir":run.project_root,"timeout":30_000}),
            9443,
            shell_dispatch,
        )
        .await;
    let task = task_for(&f, &run, 9443).await;
    let _kill = KillTask(
        f.ctx.bash_background().clone(),
        task.clone(),
        run.session.clone(),
    );
    let key = tool_provider::LedgerEdge::key(&run, "program-9443");
    assert_eq!(
        ledger::get(&f.ctx.db().unwrap().lock().unwrap(), &key)
            .unwrap()
            .unwrap()
            .state,
        ledger::State::DispatchStarted
    );
    let (describes, mut ended, worker) = scope_describe::start(
        end_control.control.clone(),
        f.executor.clone(),
        end_control.runs.clone(),
    );
    let stamp = run.scope.as_ref().unwrap();
    describes.goodbye(stamp.clone());
    let request = end_control.next().await;
    end_control.reply(&request, subc_protocol::scope::ScopeStatus::Ended, stamp);
    tokio::time::timeout(Duration::from_secs(10), ended.recv())
        .await
        .unwrap()
        .unwrap();
    worker.abort();
    let snapshot = f
        .ctx
        .bash_background()
        .status(
            &task,
            &run.session,
            Some(&run.project_root),
            Some(&f.ctx.storage_dir()),
            0,
        )
        .unwrap();
    assert_eq!(
        snapshot.info.status,
        crate::bash_background::BgTaskStatus::Killed
    );
    assert_eq!(
        snapshot.info.status_reason.as_deref(),
        Some("run_ended")
    );
    assert!(run.run_scope.get().unwrap().revoked());
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Response);
    let row = ledger::get(&f.ctx.db().unwrap().lock().unwrap(), &key)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ledger::State::Settled);
    assert_ne!(row.outcome.as_deref(), Some("not_started"));
    assert_eq!(
        serde_json::from_str::<Value>(&row.late_entry.unwrap()).unwrap()["kind"],
        "result"
    );
}
