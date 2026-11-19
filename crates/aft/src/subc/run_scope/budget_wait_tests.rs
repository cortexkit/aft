use super::*;
use subc_protocol::{
    scope::{ScopeStamp, ScopeStatus},
    session::ModuleControlResponseToModule,
};

#[tokio::test]
async fn cancelled_run_budget_wait_never_resumes_after_live_describe() {
    let (_dir, root) = test_support::test_root("run-budget-cancel");
    let (writer, mut daemon) = mpsc::channel(8);
    let replies = readiness::PendingControlReplies::default();
    let runs = RunScopes::new(Arc::new(readiness::DaemonControl::new(
        writer,
        Arc::new(DispatchPathMetrics::new()),
        PROTOCOL_VERSION,
        vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        vec![],
        replies.clone(),
        200,
    )));
    let stamp: ScopeStamp = serde_json::from_value(json!({
        "owner":{"kind":"reserved","module_id":"broca"},
        "ref":"run:cancel-first-budget", "scope_epoch":1, "kind":"ephemeral",
        "owner_authorized":true, "attributes":{"run_id":"cancel-first-budget"},
        "expires_at_ms":crate::db::call_ledger::now_ms() as u64 + 120_000
    }))
    .unwrap();
    let mut identity = test_support::route_identity(&root, "cancel-first-budget");
    let route_data = Arc::get_mut(&mut identity.0).unwrap();
    route_data.scope = Some(stamp.clone());
    route_data.call_mode = subc_protocol::call_mode::CallMode::Codemode;
    let route = route_key(41, 1);
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        route.channel,
        route.epoch,
        10,
        serde_json::to_vec(&json!({"name":"bash","arguments":{"command":"true"}})).unwrap(),
    )
    .unwrap();
    let (ready, mut answers) = mpsc::unbounded_channel();
    assert!(runs
        .defer(
            DecodedFrame {
                frame,
                phase_trace: PhaseTrace::new(Instant::now()),
                parsed_body: Default::default(),
                confinement: None,
                codemode_policies: None,
                launch_restore: None
            },
            &identity,
            &ready
        )
        .is_ok());
    let describe = tokio::time::timeout(Duration::from_secs(5), daemon.recv())
        .await
        .unwrap()
        .unwrap();
    let terminal = queued_call::cancelled_frame(
        route,
        10,
        runs.cancel(route, 10)
            .expect("Cancel must claim the budget wait"),
    )
    .unwrap();
    assert_eq!(terminal.header.ty, FrameType::Error);
    assert_eq!(terminal.header.corr, 10);
    assert_eq!(
        serde_json::from_slice::<Value>(&terminal.body).unwrap()["code"],
        "cancelled"
    );
    assert!(
        runs.cancel(route, 10).is_none(),
        "only the first Cancel owns the terminal"
    );
    let live = ModuleControlResponseToModule::ScopeDescribe {
        status: ScopeStatus::Live,
        scope_epoch: Some(stamp.scope_epoch),
        daemon_incarnation: "budget-test".into(),
        owner_synced: true,
        owner_configured: true,
        scope: Some(stamp),
    };
    assert!(replies.resolve(
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            describe.header.corr,
            serde_json::to_vec(&live).unwrap()
        )
        .unwrap()
    ));
    // Wait for the successful lookup to finish before checking the dispatch handoff.
    let finished = tokio::time::timeout(Duration::from_secs(5), answers.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        runs.resume(finished).is_none(),
        "a cancelled request must not start a command"
    );
    assert!(
        daemon.try_recv().is_err(),
        "lookup sent a duplicate terminal"
    );
}
