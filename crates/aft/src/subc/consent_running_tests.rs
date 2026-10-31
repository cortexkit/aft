fn shell_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::bash::handle(&request, ctx)
}

async fn task_for(f: &Fixture, run: &RouteIdentity, corr: u64) -> String {
    let carrier = tool_provider::LedgerEdge::key(run, "").carrier;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(task) = f
                .ctx
                .bash_background()
                .task_id_for_call_key(&carrier, &format!("program-{corr}"))
            {
                return task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the shell task is registered before handoff")
}

async fn drain_running(f: &Fixture, run: &RouteIdentity, corr: u64, writer: &WriterSender) {
    let metrics = f.metrics.lock().unwrap()[&corr].clone();
    let routes = HashMap::from([(route_key(41, 1), run.clone())]);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if bash::answer_held_bash_calls_from_module_loop(writer, &routes, &metrics, true)
                .await
                .unwrap()
                > 0
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the run's held shell reaches the drain boundary");
}

struct KillTask(crate::bash_background::BgTaskRegistry, String, String);
impl Drop for KillTask {
    fn drop(&mut self) {
        let _ = self.0.kill(&self.1, &self.2);
    }
}

fn gated_command(root: &Path) -> (String, PathBuf) {
    let release = root.join("release-shell");
    (
        format!(
            "while [ ! -e '{}' ]; do sleep 0.01; done; printf running-result",
            release.display()
        ),
        release,
    )
}

#[tokio::test]
async fn program_reply_window_hands_off_running_output_to_one_late_result() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    let run = program(&f, false, "run:reply-window");
    f.deadlines
        .lock()
        .unwrap()
        .insert(9090, ledger::now_ms() as u64 + 3000);
    let (command, release) = gated_command(&run.project_root);
    let mut replies = f
        .send_tool(
            &run,
            "bash",
            json!({"command":command,"timeout":30_000}),
            9090,
            shell_dispatch,
        )
        .await;
    let task = task_for(&f, &run, 9090).await;
    let _kill = KillTask(
        f.ctx.bash_background().clone(),
        task.clone(),
        run.session.clone(),
    );
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Response);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(
        body,
        json!({"content":[],"held":{"reason":"running","detail":{"task_id":task}}})
    );
    std::fs::write(release, "go").unwrap();
    let result = late(&f, &run, 9090).await;
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("running-result"),
        "{result}"
    );
    assert_ne!(result["isError"], true);
}

#[tokio::test]
async fn program_running_hold_survives_drain_and_finishes_once_for_head_and_worker() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for worker in [false, true] {
        let f = fixture(FakeCingulate::up());
        f.ctx.update_config(|config| {
            config.sandbox.profile.head = crate::config::SandboxMode::Off;
            config.sandbox.profile.worker = crate::config::SandboxMode::Off;
        });
        let run = program(
            &f,
            worker,
            if worker {
                "run:worker-running"
            } else {
                "run:head-running"
            },
        );
        let (command, release) = gated_command(&run.project_root);
        let (writer, mut replies) = f
            .send_tool_with_writer(
                &run,
                "bash",
                json!({"command":command,"timeout":30_000}),
                9100,
                shell_dispatch,
            )
            .await;
        let task = task_for(&f, &run, 9100).await;
        let _kill = KillTask(
            f.ctx.bash_background().clone(),
            task.clone(),
            run.session.clone(),
        );
        drain_running(&f, &run, 9100, &writer).await;
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            body,
            json!({"content":[],"held":{"reason":"running","detail":{"task_id":task}}})
        );
        let key = tool_provider::LedgerEdge::key(&run, "program-9100");
        let db = f.ctx.db().unwrap();
        assert!(ledger::get(&db.lock().unwrap(), &key)
            .unwrap()
            .unwrap()
            .late_entry
            .is_none());
        std::fs::write(release, "go").unwrap();
        let result = late(&f, &run, 9100).await;
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("running-result"),
            "{result}"
        );
        assert_ne!(result["isError"], true);
        let conn = db.lock().unwrap();
        let row = ledger::get(&conn, &key).unwrap().unwrap();
        let page = ledger::late_results(
            &conn,
            &row.scope.unwrap().owner,
            ledger::provider_incarnation(),
            None,
            None,
            ledger::now_ms(),
            1024 * 1024,
        )
        .unwrap()
        .unwrap();
        let page: Value = serde_json::from_slice(&page).unwrap();
        assert_eq!(
            page["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["call_key"] == "program-9100")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn running_hold_restart_unknown_and_withdraw_are_honest() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    let run = program(&f, false, "run:withdraw-running");
    let (command, _) = gated_command(&run.project_root);
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &run,
            "bash",
            json!({"command":command,"timeout":30_000}),
            9110,
            shell_dispatch,
        )
        .await;
    let task = task_for(&f, &run, 9110).await;
    let _kill = KillTask(
        f.ctx.bash_background().clone(),
        task.clone(),
        run.session.clone(),
    );
    drain_running(&f, &run, 9110, &writer).await;
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let held: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(held["held"]["reason"], "running");
    let key = tool_provider::LedgerEdge::key(&run, "program-9110");
    let original = ledger::get(&f.ctx.db().unwrap().lock().unwrap(), &key)
        .unwrap()
        .unwrap();
    // Keep the admitted call but omit its task record, as when a restart has
    // lost the evidence needed to determine the command's fate. Never re-run it.
    let restart = crate::db::open(&f.dir.path().join("restart.db")).unwrap();
    ledger::admit(
        &restart,
        &key,
        &original.digest,
        original.scope.as_ref(),
        ledger::State::DispatchStarted,
    )
    .unwrap();
    ledger::link_task(&restart, &key, &task).unwrap();
    ledger::hold_running(&restart, &key, &task, &reply.body, true).unwrap();
    ledger::recover(&restart).unwrap();
    let recovered = ledger::get(&restart, &key).unwrap().unwrap();
    let entry: Value = serde_json::from_str(&recovered.late_entry.unwrap()).unwrap();
    assert_eq!(entry["kind"], "result");
    assert_eq!(
        entry["result"],
        json!({"content":[],"isError":true,"errorCode":"outcome_unknown"})
    );
    let answer = f
        .checkout_tool(
            &run,
            "tool.withdraw",
            json!({"call_key":"program-9110"}),
            9111,
            shell_dispatch,
        )
        .await;
    assert!(answer.to_string().contains("already_started"), "{answer}");
    assert!(f
        .ctx
        .bash_background()
        .status(&task, &run.session, None, None, 0)
        .unwrap()
        .info
        .status
        .is_terminal());
    assert_eq!(late(&f, &run, 9110).await["isError"], true);
}

#[tokio::test]
async fn head_route_keeps_detached_even_when_it_declares_held_replies() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx.bash_background().set_db_pool(f.ctx.db().unwrap());
    f.ctx
        .update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    let mut head = identity(&f.head_root, f.head.scope.clone());
    Arc::get_mut(&mut head.0).unwrap().held_reply = true;
    let (command, _) = gated_command(&head.project_root);
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &head,
            "bash",
            json!({"command":command,"timeout":30_000}),
            9120,
            shell_dispatch,
        )
        .await;
    let task = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(task) = f.ctx.bash_background().list(0).first() {
                return task.info.task_id.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let _kill = KillTask(f.ctx.bash_background().clone(), task, head.session.clone());
    drain_running(&f, &head, 9120, &writer).await;
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "detached");
    assert_eq!(body["message"], "call detached; command still running");
}

#[tokio::test]
async fn approved_running_command_does_not_spend_the_permission_allowance() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx
        .update_config(|config| config.sandbox.profile.head = crate::config::SandboxMode::Off);
    let controlled = Controlled::install(f.fake.clone());
    let run = program(&f, false, "run:approved-long");
    let (command, release) = gated_command(&run.project_root);
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &run,
            "bash",
            json!({"command":command,"timeout":30_000,"sandbox":"host"}),
            9130,
            escalation_dispatch,
        )
        .await;
    held(&mut replies).await;
    let request = controlled.request(0).await;
    controlled.answer(&request, "allow_once");
    let task = task_for(&f, &run, 9130).await;
    let _kill = KillTask(f.ctx.bash_background().clone(), task, run.session.clone());
    run.run_scope
        .get()
        .unwrap()
        .permission_budget
        .advance(Duration::from_secs(11 * 60));
    assert!(!run
        .run_scope
        .get()
        .unwrap()
        .permission_budget
        .remaining()
        .is_zero());
    drain_running(&f, &run, 9130, &writer).await;
    assert!(
        replies.try_recv().is_err(),
        "an existing consent hold is not replaced by another held reply"
    );
    std::fs::write(release, "go").unwrap();
    assert_ne!(late(&f, &run, 9130).await["isError"], true);
}
