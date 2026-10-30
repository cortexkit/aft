//! Codemode admission: a run route's tool call acts for the head agent its
//! stamp names, located only through the stamped parent link, and is refused
//! by name whenever that agent's own route can't be found.

use super::*;
use crate::subc::test_support::{route_identity, test_root};
use subc_protocol::call_mode::CallMode;
use subc_protocol::scope::ScopeStamp;

const OWNER: &str = "prefrontal-core";

fn head_stamp(scope_ref: &str, agent: &str) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": scope_ref, "scope_epoch": 3, "kind": "head",
        "attributes": {"agent_id": agent},
        "owner_authorized": true,
    }))
    .unwrap()
}

fn run_stamp(agent: &str, parent: &str, parent_state: Option<&str>) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": "run-scope", "scope_epoch": 1, "kind": "ephemeral",
        "attributes": {"agent_id": agent, "run_id": "run-1"},
        "parent": {"owner": {"kind": "reserved", "module_id": OWNER}, "ref": parent, "scope_epoch": 3},
        "parent_state": parent_state,
        "owner_authorized": true,
    }))
    .unwrap()
}

fn route(root: &ProjectRootId, session: &str, scope: ScopeStamp, mode: CallMode) -> RouteIdentity {
    let mut identity = route_identity(root, session);
    let data = Arc::get_mut(&mut identity.0).unwrap();
    data.scope = Some(scope);
    data.call_mode = mode;
    data.disabled_tools = Arc::new(vec![format!("disabled-by-{session}")]);
    identity
}

fn channel(channel: u16) -> RouteChannel {
    route_key(channel, 1)
}

struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    head_root: ProjectRootId,
    routes: HashMap<RouteChannel, RouteIdentity>,
}

/// A head route for `agent-a` on channel 1, a second head (`agent-b`) on
/// channel 2, and the run route on channel 9, bound under a third root.
fn fixture(run: ScopeStamp) -> Fixture {
    let (head_dir, head_root) = test_root("codemode-head");
    let (other_dir, other_root) = test_root("codemode-other-head");
    let (run_dir, run_root) = test_root("codemode-run");
    let routes = HashMap::from([
        (
            channel(1),
            route(
                &head_root,
                "head-a-session",
                head_stamp("head-a", "agent-a"),
                CallMode::Classic,
            ),
        ),
        (
            channel(2),
            route(
                &other_root,
                "head-b-session",
                head_stamp("head-b", "agent-b"),
                CallMode::Classic,
            ),
        ),
        (
            channel(9),
            route(&run_root, "run-session", run, CallMode::Codemode),
        ),
    ]);
    Fixture {
        _dirs: vec![head_dir, other_dir, run_dir],
        head_root,
        routes,
    }
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"name": name, "preset": "head", "arguments": arguments})
}

fn admit(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    body: Option<&Value>,
) -> Admission {
    super::admit(routes, route, body, |_| {
        Some(Ok(EffectiveSavedPolicy::default()))
    })
}

fn refusal(admission: Admission) -> subc_protocol::ErrorBody {
    match admission {
        Admission::Refused(error) => error,
        Admission::Owner(identity) => panic!("admitted as {}", identity.session),
        Admission::Unchanged => panic!("admission left the call unchanged"),
        Admission::AwaitPolicies(_) => panic!("policies were not read"),
    }
}

fn owner(admission: Admission) -> RouteIdentity {
    match admission {
        Admission::Owner(identity) => identity,
        Admission::Refused(error) => panic!("refused: {} {:?}", error.code, error.detail),
        Admission::Unchanged => panic!("admission left the call unchanged"),
        Admission::AwaitPolicies(_) => panic!("policies were not read"),
    }
}

#[test]
fn codemode_reply_format_belongs_to_run_while_access_belongs_to_head() {
    for head_format in [false, true] {
        let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
        let head = Arc::get_mut(&mut f.routes.get_mut(&channel(1)).unwrap().0).unwrap();
        head.tool_result_body = head_format;
        head.trust = BindTrust::Untrusted;
        head.consumer_elicitation_capable = true;
        let run_format = !head_format;
        Arc::get_mut(&mut f.routes.get_mut(&channel(9)).unwrap().0)
            .unwrap()
            .tool_result_body = run_format;
        let effective = owner(admit(
            &f.routes,
            channel(9),
            Some(&call("read", json!({"path":"notes.txt"}))),
        ));
        assert_eq!(effective.project_root, f.routes[&channel(1)].project_root);
        assert_eq!(effective.session, "head-a-session");
        assert_eq!(effective.trust, BindTrust::Untrusted);
        assert_eq!(
            effective.disabled_tools.as_ref(),
            &["disabled-by-head-a-session".to_owned()]
        );
        assert!(effective.consumer_elicitation_capable);
        assert_eq!(
            effective.tool_result_body, run_format,
            "the receiving run negotiates its own reply format"
        );
        let result = ToolCallResult {
            text: "outside".into(),
            response: Response::error("id", "path_outside_root", "outside"),
        };
        let mut reply = wire::build_tool_response_frame(
            PROTOCOL_VERSION,
            channel(9),
            1,
            control_flags(),
            &result,
            effective.reply_policy(),
        )
        .unwrap();
        reply.project_tool_result().unwrap();
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body.get("errorCode").is_some(), run_format);
    }
}

#[test]
fn codemode_call_to_tool_without_codemode_is_refused_call_mode_unsupported() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let body = call("bash_watch", json!({"taskId": "absent"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(
        error.code,
        cortexkit_role_tool_provider::errors::CALL_MODE_UNSUPPORTED
    );
    assert_eq!(
        error.detail,
        Some(json!({"tool": "bash_watch", "mode": "codemode"}))
    );
}

#[test]
fn codemode_agent_named_in_arguments_never_replaces_stamp_agent() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    // The arguments name the other head's agent and scope. Only the stamp
    // decides whose access the call runs with.
    let body = call(
        "read",
        json!({
            "path": "notes.txt",
            "agent_id": "agent-b",
            "run_id": "other-run",
            "scope": "head-b",
            "session_id": "head-b-session",
        }),
    );
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    let owner = effective.run_owner.as_ref().unwrap();
    assert_eq!(
        owner.kind,
        RunKind::Agent {
            agent_id: "agent-a".into()
        }
    );
    assert_eq!(owner.run_id, "run-1");
    assert_eq!(effective.session, "head-a-session");
    assert_eq!(effective.root, f.head_root);
}

#[test]
fn codemode_route_stamped_parent_ended_serves_no_new_call() {
    for (state, reason) in [
        (Some("ended"), "parent_ended"),
        (Some("pending"), "parent_pending"),
        (None, "parent_state_missing"),
    ] {
        let f = fixture(run_stamp("agent-a", "head-a", state));
        let body = call("read", json!({"path": "notes.txt"}));
        let error = refusal(admit(&f.routes, channel(9), Some(&body)));
        assert_eq!(error.code, RUN_OWNER_NOT_BOUND, "{state:?}");
        assert_eq!(error.detail.as_ref().unwrap()["reason"], reason);
        assert_eq!(error.detail.as_ref().unwrap()["agent_id"], "agent-a");
        assert_eq!(error.detail.as_ref().unwrap()["run_id"], "run-1");
    }
}

#[test]
fn codemode_without_live_head_route_is_refused_run_owner_not_bound() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    f.routes.remove(&channel(1));
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(
        error.detail.as_ref().unwrap()["reason"],
        "head_route_not_bound"
    );
}

#[test]
fn codemode_mismatched_agent_is_refused_run_owner_not_bound() {
    // The run is on agent-b, but its parent link names agent-a's head.
    let f = fixture(run_stamp("agent-b", "head-a", Some("linked")));
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    assert_eq!(error.detail.as_ref().unwrap()["reason"], "agent_mismatch");
}

#[test]
fn codemode_call_runs_with_head_route_access_and_run_lifetime() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let head = f.routes[&channel(1)].clone();
    let run = f.routes[&channel(9)].clone();
    let body = call("edit", json!({"path": "notes.txt"}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    // Access is the head route's.
    assert_eq!(effective.root, head.root);
    assert_eq!(effective.project_root, head.project_root);
    assert_eq!(effective.session, head.session);
    assert_eq!(effective.harness, head.harness);
    assert_eq!(effective.trust, head.trust);
    assert_eq!(*effective.disabled_tools, *head.disabled_tools);
    assert_eq!(
        access_grants::policy_scope_key(&effective),
        access_grants::scope_key(&head)
    );
    assert_eq!(
        remote_policy::key(&effective, Some("worker")),
        remote_policy::key(&head, Some("worker"))
    );
    assert!(!access_grants::alfonso_worker(&effective));
    assert_eq!(
        access_grants::ask_route(&effective),
        crate::access_policy::AskRoute::Unavailable
    );
    // Lifetime stays the run's: its scope keeps tagging the work it starts.
    assert_eq!(effective.scope, run.scope);
    assert_eq!(effective.call_mode, CallMode::Codemode);
    assert_eq!(effective.role, run.role);
}

#[test]
fn codemode_provider_operations_keep_the_run_route_identity() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    f.routes.remove(&channel(1));
    for body in [
        json!({"op": "tool.catalog", "preset": "head"}),
        json!({"name": "late_results", "arguments": {}}),
    ] {
        assert!(matches!(
            admit(&f.routes, channel(9), Some(&body)),
            Admission::Unchanged
        ));
    }
}

#[test]
fn classic_route_calls_are_unchanged() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let body = call("read", json!({"path": "notes.txt"}));
    assert!(matches!(
        admit(&f.routes, channel(1), Some(&body)),
        Admission::Unchanged
    ));
}

/// Give `route`'s spawn principal the id `principal_id`.
fn with_principal_id(
    routes: &mut HashMap<RouteChannel, RouteIdentity>,
    route: RouteChannel,
    id: &str,
) {
    let identity = routes.get_mut(&route).unwrap();
    let data = Arc::get_mut(&mut identity.0).unwrap();
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
        *principal_id = Some(id.to_owned());
    }
}

#[test]
fn codemode_call_uses_head_principal_and_records_ledger_under_run_route() {
    let mut f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    // The run route's principal id would force the strict sandbox if it were
    // used for the call; the head's does not.
    with_principal_id(&mut f.routes, channel(9), "unverified");
    let head = f.routes[&channel(1)].clone();
    let run = f.routes[&channel(9)].clone();
    let body = call("read", json!({"path": "notes.txt"}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    // Sandbox and permission decisions read the head's principal, id included.
    assert_eq!(effective.spawn_principal, head.spawn_principal);
    assert!(!crate::sandbox_spawn::principal_requires_strict(
        &effective.spawn_principal
    ));
    // The call's ledger rows are keyed the way the run route's late_results
    // and tool.withdraw look them up.
    let call_key = "program-call";
    assert_eq!(
        tool_provider::LedgerEdge::key(&effective, call_key),
        tool_provider::LedgerEdge::key(&run, call_key)
    );
    assert_eq!(
        tool_provider::LedgerEdge::key(&effective, call_key).carrier,
        "unverified"
    );
    assert_eq!(
        effective.run_owner.as_ref().unwrap().ledger_carrier(),
        "unverified"
    );
}

fn audit_read(req: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&req, ctx)
}

#[test]
fn codemode_audit_line_is_written_once_per_admitted_call() {
    let f = fixture(run_stamp("agent-a", "head-a", Some("linked")));
    let file = f.head_root.as_path().join("audited.txt");
    let (reply, lines) = audited_read(&f.routes, &f.head_root, &file);
    assert!(reply.contains("audited contents"), "{reply}");
    let audit: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("codemode call: tool=read"))
        .collect();
    assert_eq!(audit.len(), 1, "{lines:#?}");
    for field in [
        "agent_id=agent-a",
        "run_id=run-1",
        "head_session=head-a-session",
    ] {
        assert!(
            audit[0].contains(field),
            "{field} missing from {}",
            audit[0]
        );
    }
}

#[test]
fn worker_run_audit_line_names_on_behalf_of_agent_never_agent_id() {
    let (f, worker_root) = worker_fixture(worker_run_stamp("worker-a", 5, Some("linked")));
    let file = worker_root.as_path().join("audited.txt");
    let (reply, lines) = audited_read(&f.routes, &worker_root, &file);
    assert!(reply.contains("audited contents"), "{reply}");
    let audit: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("codemode call: tool=read"))
        .collect();
    assert_eq!(audit.len(), 1, "{lines:#?}");
    for field in [
        "worker_run",
        "on_behalf_of_agent=agent-a",
        "run_id=run-w",
        "worker_session=worker-session",
    ] {
        assert!(
            audit[0].contains(field),
            "{field} missing from {}",
            audit[0]
        );
    }
    assert!(!audit[0].contains("agent_id="), "{}", audit[0]);
    assert!(!audit[0].contains("head-a-session"), "{}", audit[0]);
}

/// Admit a codemode `read` of `file` (written with known contents) on the run
/// route (channel 9), run it on an executor rooted at `root`, and return the
/// reply body with the log lines written meanwhile.
fn audited_read(
    routes: &HashMap<RouteChannel, RouteIdentity>,
    root: &ProjectRootId,
    file: &Path,
) -> (String, Vec<String>) {
    std::fs::write(file, "audited contents\n").unwrap();
    let ctx = crate::subc::test_support::test_ctx();
    ctx.update_config(|config| config.project_root = Some(root.as_path().to_path_buf()));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));
    let body = call("read", json!({"path": file}));
    let effective = owner(admit(routes, channel(9), Some(&body)));
    let routes = overlay(routes, channel(9), effective);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    crate::logging::capture_log_lines(|| {
        runtime.block_on(async {
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                9,
                1,
                77,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            let (writer, mut replies) = mpsc::channel(8);
            let (bash_tx, _bash_rx) = mpsc::channel(8);
            let (touch_tx, _touch_rx) = mpsc::channel(8);
            let (entries, _deferred_rx) = mpsc::unbounded_channel();
            let deferred_tx = DeferredResponseSender {
                entries,
                wake: crate::response_finalize::DeferredResponseWake::default(),
            };
            handle_tool_call(
                &writer,
                &frame,
                None,
                None,
                PhaseTrace::new(Instant::now()),
                &routes,
                &HashMap::new(),
                &ReclaimedRoutes::default(),
                &mut HashMap::new(),
                &executor,
                &ActiveToolCalls::default(),
                &Arc::new(PendingDeferredSetups::default()),
                &Arc::new(Notify::new()),
                &PersistentCancelSignal::new(),
                &bash_tx,
                &touch_tx,
                &Arc::new(DispatchPathMetrics::new()),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut 1,
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                audit_read,
                &deferred_tx,
                false,
                1024 * 1024,
                &drain::ModuleDrainWindow::default(),
                None,
            )
            .await
            .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
                .await
                .expect("the audited call terminates")
                .expect("writer stays open");
            String::from_utf8_lossy(&reply.body).into_owned()
        })
    })
}
/// Change the agent-a head route (channel 1) in place.
fn edit_head(f: &mut Fixture, change: impl FnOnce(&mut RouteIdentityData)) {
    let identity = f.routes.get_mut(&channel(1)).unwrap();
    change(Arc::get_mut(&mut identity.0).unwrap());
}

/// Change the agent-a head route's scope stamp in place.
fn edit_head_stamp(f: &mut Fixture, change: impl FnOnce(&mut ScopeStamp)) {
    edit_head(f, |data| change(data.scope.as_mut().unwrap()));
}

/// The refusal reason for a read on the run route.
fn refusal_reason(f: &Fixture) -> String {
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    error.detail.as_ref().unwrap()["reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn linked_fixture() -> Fixture {
    fixture(run_stamp("agent-a", "head-a", Some("linked")))
}

#[test]
fn codemode_head_with_another_scope_owner_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head_stamp(&mut f, |stamp| {
        stamp.owner = subc_protocol::Principal::Reserved {
            module_id: "another-core".into(),
        };
    });
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_head_with_another_scope_ref_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head_stamp(&mut f, |stamp| stamp.scope_ref = "head-a-other".into());
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_head_at_another_scope_epoch_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head_stamp(&mut f, |stamp| stamp.scope_epoch = 4);
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_parent_scope_that_is_not_a_head_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head_stamp(&mut f, |stamp| {
        stamp.kind = subc_protocol::scope::ScopeKind::Worker;
    });
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_head_with_unauthorized_owner_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head_stamp(&mut f, |stamp| stamp.owner_authorized = false);
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_route_bound_in_codemode_is_not_the_runs_head() {
    let mut f = linked_fixture();
    edit_head(&mut f, |data| data.call_mode = CallMode::Codemode);
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_identity_acting_for_a_run_is_not_the_runs_head() {
    let mut f = linked_fixture();
    let other = f.routes[&channel(2)].clone();
    let other_principal = other.spawn_principal.clone();
    edit_head(&mut f, |data| {
        data.run_owner = Some(Arc::new(RunOwner {
            kind: RunKind::Agent {
                agent_id: "agent-a".into(),
            },
            run_id: "earlier-run".into(),
            owner_route: other,
            owner_route_channel: channel(2),
            run_route: channel(5),
            run_principal: other_principal,
        }));
    });
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

#[test]
fn codemode_same_agent_on_a_head_under_another_scope_is_not_the_runs_head() {
    let mut f = linked_fixture();
    // The run's own agent has a live head route, but under another scope
    // (other ref and epoch): the parent link, not the agent, finds the head.
    edit_head_stamp(&mut f, |stamp| {
        stamp.scope_ref = "head-c".into();
        stamp.scope_epoch = 9;
    });
    assert_eq!(refusal_reason(&f), "head_route_not_bound");
}

/// A second agent-a head route on channel 3, otherwise like channel 1. Its
/// principal carries its own channel and epoch, as every production bind does.
fn second_head(f: &mut Fixture, root: &ProjectRootId, session: &str, trust: BindTrust) {
    let mut identity = crate::subc::test_support::route_identity_with_trust(root, session, trust);
    {
        let data = Arc::get_mut(&mut identity.0).unwrap();
        let primary = &f.routes[&channel(1)];
        data.project_root = primary.project_root.clone();
        data.harness = primary.harness.clone();
        data.spawn_principal = primary.spawn_principal.clone();
        if let AuthenticatedPrincipal::RouteBind {
            route_channel,
            route_epoch,
            ..
        } = &mut data.spawn_principal
        {
            *route_channel = 3;
            *route_epoch += 1;
        }
        data.consumer_elicitation_capable = primary.consumer_elicitation_capable;
        data.disabled_tools = Arc::clone(&primary.disabled_tools);
        data.scope = Some(head_stamp("head-a", "agent-a"));
        data.call_mode = CallMode::Classic;
    }
    f.routes.insert(channel(3), identity);
}

fn assert_ambiguous_field(f: &Fixture, field: &str) {
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    let detail = error.detail.as_ref().unwrap();
    assert_eq!(detail["reason"], "head_route_ambiguous");
    assert!(detail["fields"].as_array().unwrap().contains(&json!(field)));
    assert_eq!(detail["channels"], json!([1, 3]));
    assert!(error.message.contains(field), "{}", error.message);
}

#[test]
fn codemode_head_routes_that_disagree_on_root_are_ambiguous() {
    let mut f = linked_fixture();
    let (_other_dir, other_root) = test_root("codemode-second-head");
    second_head(&mut f, &other_root, "head-a-session", BindTrust::FirstParty);
    assert_ambiguous_field(&f, "root");
}

#[test]
fn codemode_head_routes_that_disagree_on_session_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "another-head-session", BindTrust::FirstParty);
    assert_ambiguous_field(&f, "session");
}

#[test]
fn codemode_head_routes_that_disagree_on_trust_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::Untrusted);
    assert_ambiguous_field(&f, "trust");
}

#[test]
fn codemode_head_routes_that_disagree_on_project_root_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0)
        .unwrap()
        .project_root
        .push("different-project-root");
    assert_ambiguous_field(&f, "project_root");
}

#[test]
fn codemode_head_routes_that_disagree_on_harness_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0).unwrap().harness = "another-harness".into();
    assert_ambiguous_field(&f, "harness");
}

#[test]
fn codemode_head_routes_that_disagree_on_spawn_principal_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0).unwrap().spawn_principal =
        crate::sandbox_spawn::AuthenticatedPrincipal::FirstParty;
    assert_ambiguous_field(&f, "spawn_principal");
}

#[test]
fn codemode_head_routes_with_equivalent_first_party_principals_are_served() {
    for (first, second) in [
        ("direct", "reserved:prefrontal-core"),
        ("reserved:prefrontal-core", "direct"),
    ] {
        let mut f = linked_fixture();
        let root = f.head_root.clone();
        second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
        with_principal_id(&mut f.routes, channel(1), first);
        with_principal_id(&mut f.routes, channel(3), second);
        let body = call("read", json!({"path": "notes.txt"}));
        let effective = owner(admit(&f.routes, channel(9), Some(&body)));
        let head = &f.routes[&channel(1)];
        assert_eq!(effective.spawn_principal, head.spawn_principal);
        assert_eq!(effective.root, head.root);
        assert_eq!(effective.project_root, head.project_root);
        assert_eq!(effective.session, head.session);
        assert_eq!(effective.harness, head.harness);
        assert_eq!(effective.trust, head.trust);
        assert_eq!(effective.disabled_tools, head.disabled_tools);
        assert!(Arc::ptr_eq(
            &effective.run_owner.as_ref().unwrap().owner_route.0,
            &head.0
        ));
    }
}

fn saved_policy_fixture() -> (Fixture, tempfile::TempDir, Arc<AppContext>) {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    with_principal_id(&mut f.routes, channel(1), "direct");
    with_principal_id(&mut f.routes, channel(3), "reserved:prefrontal-core");
    let storage = tempfile::tempdir().unwrap();
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(f.routes[&channel(1)].project_root.clone()),
            storage_dir: Some(storage.path().to_owned()),
            ..Default::default()
        },
    );
    ctx.set_db(Arc::new(std::sync::Mutex::new(
        crate::db::open(&storage.path().join("aft.db")).unwrap(),
    )));
    (f, storage, Arc::new(ctx))
}

fn policy_identity(identity: &RouteIdentity) -> RouteIdentity {
    let mut saved = route(
        &identity.root,
        &identity.session,
        identity.scope.clone().unwrap(),
        CallMode::Classic,
    );
    let data = Arc::get_mut(&mut saved.0).unwrap();
    data.project_root = identity.project_root.clone();
    data.harness = identity.harness.clone();
    data.trust = identity.trust;
    data.spawn_principal = identity.spawn_principal.clone();
    saved
}

fn save_policy(ctx: &AppContext, identity: &RouteIdentity, params: Value) {
    remote_policy::catalog(
        json!({"op":"tool.catalog", "preset":"worker", "params":params}),
        identity,
        ctx,
    )
    .unwrap();
}

fn admit_saved(f: &Fixture, ctx: &Arc<AppContext>) -> Admission {
    let body = call("read", json!({"path":"notes.txt"}));
    for _ in 0..3 {
        let policies = SavedPolicies::read(
            [1, 3]
                .into_iter()
                .map(|ch| (f.routes[&channel(ch)].clone(), Arc::clone(ctx)))
                .collect(),
        );
        let admission = super::admit(&f.routes, channel(9), Some(&body), |identity| {
            policies.get(ctx, identity)
        });
        if !matches!(admission, Admission::AwaitPolicies(_)) {
            return admission;
        }
    }
    panic!("saved policies did not stabilize");
}

#[test]
fn codemode_head_worker_preflight_does_not_save_policy() {
    for params in [
        json!({"remote_exec":{"enabled":true}, "siblings":[]}),
        json!({"confine":"root", "siblings":[]}),
    ] {
        let (f, _storage, ctx) = saved_policy_fixture();
        remote_policy::catalog(
            json!({"op":"tool.catalog", "preset":"worker", "params":params}),
            &f.routes[&channel(3)],
            &ctx,
        )
        .unwrap();
        let effective = owner(admit_saved(&f, &ctx));
        assert_eq!(effective.session, "head-a-session");
        let count: i64 = ctx
            .db()
            .unwrap()
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM remote_exec_policies", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "head preflight must not persist a worker policy");
    }
}

#[test]
fn codemode_head_routes_ignore_preexisting_worker_policies() {
    for params in [
        json!({"remote_exec":{"enabled":true}}),
        json!({"confine":"root"}),
    ] {
        for older_epoch in [false, true] {
            let (f, _storage, ctx) = saved_policy_fixture();
            let head = &f.routes[&channel(3)];
            let scope = head.scope.as_ref().unwrap();
            let key = crate::db::remote_exec::PolicyKey {
                project_root: head.project_root.display().to_string(),
                harness: head.harness.clone(),
                session: head.session.clone(),
                principal: "reserved:prefrontal-core".into(),
                owner: serde_json::to_string(&scope.owner).unwrap(),
                scope_ref: scope.scope_ref.clone(),
                epoch: scope.scope_epoch - u64::from(older_epoch),
                preset: "worker".into(),
            };
            let policy =
                crate::exec_remote::FrozenParams::decode(params.as_object().unwrap()).unwrap();
            let db = ctx.db().unwrap();
            crate::db::remote_exec::freeze(
                &db.lock().unwrap(),
                &key,
                &policy,
                crate::bash_background::persistence::unix_millis(),
                || {},
            )
            .unwrap();
            for unreadable in [false, true] {
                if unreadable {
                    db.lock()
                        .unwrap()
                        .execute("DROP TABLE remote_exec_policies", [])
                        .unwrap();
                }
                assert!(!remote_policy::confined(&ctx, head).unwrap());
                let effective = owner(admit_saved(&f, &ctx));
                assert_eq!(effective.session, "head-a-session");
                assert!(remote_policy::key(head, Some("worker")).is_none());
                assert!(remote_policy::key(&effective, Some("worker")).is_none());
                assert!(
                    remote_policy::lookup(&ctx, &remote_policy::source(head, Some("worker")))
                        .is_none()
                );
            }
        }
    }
}

#[test]
fn codemode_head_routes_with_different_saved_policies_are_ambiguous() {
    for params in [None, Some(json!({"remote_exec":{"enabled":true}}))] {
        for saved_channel in [1, 3] {
            let (f, _storage, _ctx) = saved_policy_fixture();
            let error = refusal(super::admit(
                &f.routes,
                channel(9),
                Some(&call("read", json!({"path":"notes.txt"}))),
                |identity| {
                    Some(Ok(
                        if Arc::ptr_eq(&identity.0, &f.routes[&channel(saved_channel)].0) {
                            EffectiveSavedPolicy {
                                confined: params.is_none(),
                                params: params.clone(),
                            }
                        } else {
                            EffectiveSavedPolicy::default()
                        },
                    ))
                },
            ));
            assert_eq!(
                error.detail.as_ref().unwrap()["reason"],
                "head_route_ambiguous"
            );
            assert_eq!(
                error.detail.as_ref().unwrap()["fields"],
                json!(["saved_policy"])
            );
            assert_eq!(error.detail.as_ref().unwrap()["channels"], json!([1, 3]));
            assert!(error.message.contains("saved_policy"));
        }
    }
}

#[test]
fn codemode_head_routes_with_identical_saved_policies_keep_first_owner() {
    for params in [
        json!({"confine":"root"}),
        json!({"remote_exec":{"enabled":true}}),
    ] {
        let (f, _storage, _ctx) = saved_policy_fixture();
        let effective = owner(super::admit(
            &f.routes,
            channel(9),
            Some(&call("read", json!({"path":"notes.txt"}))),
            |_| {
                Some(Ok(EffectiveSavedPolicy {
                    confined: false,
                    params: Some(params.clone()),
                }))
            },
        ));
        assert!(Arc::ptr_eq(
            &effective.run_owner.as_ref().unwrap().owner_route.0,
            &f.routes[&channel(1)].0
        ));
        assert_eq!(
            effective.spawn_principal,
            f.routes[&channel(1)].spawn_principal
        );
    }
}

#[test]
fn codemode_saved_policy_reads_do_not_outlive_catalog_save() {
    for params in [
        json!({"confine":"root"}),
        json!({"remote_exec":{"enabled":true}}),
    ] {
        let (f, _storage, ctx) = saved_policy_fixture();
        let mut worker = policy_identity(&f.routes[&channel(3)]);
        Arc::get_mut(&mut worker.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .kind = subc_protocol::scope::ScopeKind::Worker;
        let policies = SavedPolicies::read(vec![(worker.clone(), Arc::clone(&ctx))]);
        assert!(policies.get(&ctx, &worker).unwrap().is_ok());
        save_policy(&ctx, &worker, params);
        assert!(policies.get(&ctx, &worker).is_none());
    }
}

#[test]
fn codemode_worker_with_unreadable_saved_policy_refuses() {
    let (f, _storage, ctx) = saved_policy_fixture();
    let mut worker = policy_identity(&f.routes[&channel(3)]);
    Arc::get_mut(&mut worker.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .kind = subc_protocol::scope::ScopeKind::Worker;
    save_policy(&ctx, &worker, json!({"confine":"root"}));
    ctx.db()
        .unwrap()
        .lock()
        .unwrap()
        .execute("UPDATE remote_exec_policies SET params='not-json'", [])
        .unwrap();
    let policies = SavedPolicies::read(vec![(worker.clone(), Arc::clone(&ctx))]);
    let error = policies.get(&ctx, &worker).unwrap().unwrap_err();
    assert_eq!(error.code, remote_policy::policy_unavailable().code);
}

#[test]
fn codemode_saved_policy_read_errors_refuse() {
    let (f, _storage, _ctx) = saved_policy_fixture();
    let error = refusal(super::admit(
        &f.routes,
        channel(9),
        Some(&call("read", json!({"path":"notes.txt"}))),
        |_| Some(Err(remote_policy::policy_unavailable())),
    ));
    assert_eq!(error.code, remote_policy::policy_unavailable().code);
}

#[test]
fn codemode_head_routes_with_unverified_or_absent_principals_are_ambiguous() {
    for id in [Some("unverified"), None] {
        let mut f = linked_fixture();
        let root = f.head_root.clone();
        second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
        with_principal_id(&mut f.routes, channel(1), "direct");
        let second = f.routes.get_mut(&channel(3)).unwrap();
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
            &mut Arc::get_mut(&mut second.0).unwrap().spawn_principal
        {
            *principal_id = id.map(str::to_owned);
        }
        assert_ambiguous_field(&f, "spawn_principal");
    }
}

#[test]
fn codemode_head_routes_with_different_flow_read_carrier_access_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    with_principal_id(&mut f.routes, channel(1), "direct");
    with_principal_id(&mut f.routes, channel(3), "reserved:basal");
    // Hold sandbox access equal to isolate the carrier's additional read capability.
    assert_eq!(
        principal_access_class(&f.routes[&channel(1)].spawn_principal).strict_sandbox,
        principal_access_class(&f.routes[&channel(3)].spawn_principal).strict_sandbox
    );
    assert_ambiguous_field(&f, "spawn_principal");
}

#[test]
fn codemode_head_routes_that_disagree_on_consumer_elicitation_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0)
        .unwrap()
        .consumer_elicitation_capable = true;
    assert_ambiguous_field(&f, "consumer_elicitation_capable");
}

#[test]
fn codemode_head_routes_that_disagree_on_disabled_tools_are_ambiguous() {
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0).unwrap().disabled_tools = Arc::new(vec!["read".into()]);
    assert_ambiguous_field(&f, "disabled_tools");
}

#[test]
fn codemode_head_routes_that_agree_are_served() {
    // Control for the ambiguity tests: a second channel of the same head is
    // not a disagreement.
    let mut f = linked_fixture();
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::FirstParty);
    let body = call("read", json!({"path": "notes.txt"}));
    let effective = owner(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(effective.session, "head-a-session");
}

fn multi_checkout_head(harness: &str, run_at_head_root: bool) -> Fixture {
    let mut f = linked_fixture();
    // Both plugins cache tool routes by root, harness and session. Working in
    // another checkout opens another route carrying the same head scope.
    let (other_dir, other_root) = test_root("codemode-second-checkout");
    f._dirs.push(other_dir);
    let mut other = route(
        &other_root,
        "head-a-session",
        head_stamp("head-a", "agent-a"),
        CallMode::Classic,
    );
    Arc::get_mut(&mut other.0).unwrap().disabled_tools = Arc::new(vec!["edit".into()]);
    f.routes.insert(channel(3), other);
    for id in [channel(1), channel(3)] {
        let data = Arc::get_mut(&mut f.routes.get_mut(&id).unwrap().0).unwrap();
        data.harness = harness.into();
        data.consumer_elicitation_capable = true;
        if let AuthenticatedPrincipal::RouteBind {
            route_channel,
            route_epoch,
            harness: principal_harness,
            ..
        } = &mut data.spawn_principal
        {
            *route_channel = id.channel;
            *route_epoch = id.epoch;
            *principal_harness = harness.into();
        }
    }
    if run_at_head_root {
        f.routes.insert(
            channel(9),
            route(
                &f.head_root,
                "run-session",
                run_stamp("agent-a", "head-a", Some("linked")),
                CallMode::Codemode,
            ),
        );
    }
    f
}

#[test]
fn codemode_head_route_regression_multi_checkout_selects_runs_root() {
    for harness in ["opencode", "pi"] {
        let f = multi_checkout_head(harness, true);
        let head = &f.routes[&channel(1)];
        let body = call("read", json!({"path": "README.md", "endLine": 5}));
        let effective = owner(admit(&f.routes, channel(9), Some(&body)));
        // Keep one route's principal, disabled tools and elicitation capability
        // together; never combine access settings across the two checkouts.
        assert!(Arc::ptr_eq(
            &effective.run_owner.as_ref().unwrap().owner_route.0,
            &head.0
        ));
        assert_eq!(effective.root, head.root);
        assert_eq!(effective.project_root, head.project_root);
        assert_eq!(effective.spawn_principal, head.spawn_principal);
        assert_eq!(effective.disabled_tools, head.disabled_tools);
        assert_eq!(
            effective.consumer_elicitation_capable,
            head.consumer_elicitation_capable
        );
    }
}

#[test]
fn codemode_multi_checkout_head_without_matching_root_refuses_naming_root() {
    for harness in ["opencode", "pi"] {
        let f = multi_checkout_head(harness, false);
        let body = call("read", json!({"path": "README.md"}));
        let error = refusal(admit(&f.routes, channel(9), Some(&body)));
        let detail = error.detail.as_ref().unwrap();
        assert_eq!(detail["reason"], "head_route_ambiguous");
        assert!(detail["fields"]
            .as_array()
            .unwrap()
            .contains(&json!("root")));
        assert_eq!(detail["channels"], json!([1, 3]));
    }
}

#[test]
fn codemode_head_route_regression_same_root_conflict_names_fields_and_channels() {
    let mut f = multi_checkout_head("opencode", true);
    let root = f.head_root.clone();
    second_head(&mut f, &root, "head-a-session", BindTrust::Untrusted);
    let second = f.routes.get_mut(&channel(3)).unwrap();
    Arc::get_mut(&mut second.0).unwrap().role = tool_provider::RouteRole::ToolProviderV1;
    let body = call("read", json!({"path": "notes.txt"}));
    let (error, lines) =
        crate::logging::capture_log_lines(|| refusal(admit(&f.routes, channel(9), Some(&body))));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    let detail = error.detail.as_ref().unwrap();
    assert_eq!(detail["reason"], "head_route_ambiguous");
    assert_eq!(detail["fields"], json!(["trust"]));
    assert_eq!(detail["channels"], json!([1, 3]));
    assert!(error.message.contains("trust"), "{}", error.message);
    let diagnostic = lines
        .iter()
        .find(|line| line.contains("codemode call refused:"))
        .expect("the refusal is logged");
    assert!(
        diagnostic.contains("\"fields\":[\"trust\"]"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("\"channels\":[1,3]"), "{diagnostic}");
    let info = lines
        .iter()
        .find(|line| line.contains("codemode head route ambiguity:"))
        .expect("the ambiguity is logged at INFO");
    assert!(
        info.contains("run_id=run-1 fields=trust channels=1,3"),
        "{info}"
    );
    for root in f._dirs.iter().map(|dir| dir.path().to_string_lossy()) {
        assert!(!error.message.contains(root.as_ref()));
        assert!(!detail.to_string().contains(root.as_ref()));
        assert!(!diagnostic.contains(root.as_ref()));
    }
}

#[test]
fn codemode_run_with_unauthorized_owner_is_refused() {
    let mut run = run_stamp("agent-a", "head-a", Some("linked"));
    run.owner_authorized = false;
    let f = fixture(run);
    assert_eq!(refusal_reason(&f), "run_owner_unauthorized");
}

/// A worker's own task scope, under the agent-a head scope.
fn worker_stamp(scope_ref: &str) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": scope_ref, "scope_epoch": 5, "kind": "worker",
        "parent": {"owner": {"kind": "reserved", "module_id": OWNER}, "ref": "head-a", "scope_epoch": 3},
        "parent_state": "linked",
        "owner_authorized": true,
    }))
    .unwrap()
}

/// A worker run: `run_id` and `on_behalf_of_agent`, no `agent_id`, parented
/// on the scope `parent` at `parent_epoch`.
fn worker_run_stamp(parent: &str, parent_epoch: u64, parent_state: Option<&str>) -> ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": OWNER},
        "ref": "worker-run-scope", "scope_epoch": 1, "kind": "ephemeral",
        "attributes": {"on_behalf_of_agent": "agent-a", "run_id": "run-w"},
        "parent": {"owner": {"kind": "reserved", "module_id": OWNER}, "ref": parent, "scope_epoch": parent_epoch},
        "parent_state": parent_state,
        "owner_authorized": true,
    }))
    .unwrap()
}

/// The agent-a head (channel 1), the agent-b head (channel 2), agent-a's
/// worker on channel 4 under its own root, and the worker run on channel 9.
fn worker_fixture(run: ScopeStamp) -> (Fixture, ProjectRootId) {
    let mut f = fixture(run);
    let (worker_dir, worker_root) = test_root("codemode-worker");
    f.routes.insert(
        channel(4),
        route(
            &worker_root,
            "worker-session",
            worker_stamp("worker-a"),
            CallMode::Classic,
        ),
    );
    f._dirs.push(worker_dir);
    (f, worker_root)
}

fn worker_refusal(f: &Fixture) -> Value {
    let body = call("read", json!({"path": "notes.txt"}));
    let error = refusal(admit(&f.routes, channel(9), Some(&body)));
    assert_eq!(error.code, RUN_OWNER_NOT_BOUND);
    error.detail.unwrap()
}

#[test]
fn worker_run_runs_with_its_workers_own_access_never_the_heads() {
    let (f, worker_root) = worker_fixture(worker_run_stamp("worker-a", 5, Some("linked")));
    let head = f.routes[&channel(1)].clone();
    let worker = f.routes[&channel(4)].clone();
    let run = f.routes[&channel(9)].clone();
    // `call` names the head preset: the preset sets only the call's role,
    // and the access is the worker route's either way.
    let effective = owner(admit(
        &f.routes,
        channel(9),
        Some(&call("edit", json!({"path": "notes.txt"}))),
    ));
    assert_eq!(effective.root, worker_root);
    assert_eq!(effective.project_root, worker.project_root);
    assert_eq!(effective.session, "worker-session");
    assert_eq!(effective.spawn_principal, worker.spawn_principal);
    assert_eq!(*effective.disabled_tools, *worker.disabled_tools);
    assert_ne!(effective.session, head.session);
    // Grants, frozen worker params and confinement come from the worker's
    // own route, never the head's.
    assert_eq!(
        access_grants::policy_scope_key(&effective),
        access_grants::scope_key(&worker)
    );
    assert_ne!(
        access_grants::policy_scope_key(&effective),
        access_grants::scope_key(&head)
    );
    let worker_key = remote_policy::key(&worker, Some("worker"));
    assert!(worker_key.is_some());
    assert_eq!(remote_policy::key(&effective, Some("worker")), worker_key);
    // Judged as the worker (worker jail, worker refusals), and nobody is
    // asked on anyone's behalf.
    assert!(access_grants::alfonso_worker(&effective));
    assert_eq!(
        access_grants::ask_route(&effective),
        crate::access_policy::AskRoute::Unavailable
    );
    let owner = effective.run_owner.as_ref().unwrap();
    assert_eq!(
        owner.kind,
        RunKind::Worker {
            on_behalf_of_agent: "agent-a".into()
        }
    );
    assert!(owner.is_worker_run());
    assert_eq!(owner.run_id, "run-w");
    assert!(std::ptr::eq(
        worker_authority_route(&effective),
        &owner.owner_route
    ));
    // Lifetime and ledger stay the run's.
    assert_eq!(effective.scope, run.scope);
    assert_eq!(effective.call_mode, CallMode::Codemode);
    assert_eq!(
        tool_provider::LedgerEdge::key(&effective, "k"),
        tool_provider::LedgerEdge::key(&run, "k")
    );
}

#[test]
fn agent_run_keeps_no_worker_authority_route() {
    let f = linked_fixture();
    let effective = owner(admit(
        &f.routes,
        channel(9),
        Some(&call("read", json!({"path": "notes.txt"}))),
    ));
    assert!(!effective.run_owner.as_ref().unwrap().is_worker_run());
    assert!(std::ptr::eq(worker_authority_route(&effective), &effective));
}

#[test]
fn worker_runs_shell_has_no_bot_identity_to_speak_as() {
    let (f, _) = worker_fixture(worker_run_stamp("worker-a", 5, Some("linked")));
    let effective = owner(admit(
        &f.routes,
        channel(9),
        Some(&call("bash", json!({"command": "true"}))),
    ));
    // The gh relay finds a shell's scope by the session its ticket names: a
    // worker run's shell runs in the worker's session, so it gets the
    // worker's stamp, which names no agent to act as. Neither stamp grants
    // the Head authority required for bot writes.
    let storage = tempfile::tempdir().unwrap();
    let speech = crate::gh_shim_speech::service::record_service(storage.path());
    let scopes = gh_relay_route_scopes(&f.routes, &speech);
    let stamp = scopes
        .for_session(&effective.session)
        .expect("the worker session has one stamp");
    assert_eq!(stamp, f.routes[&channel(4)].scope.as_ref().unwrap());
    assert!(crate::gh_shim_relay::worker_scope(stamp));
    assert_eq!(crate::gh_shim_relay::delegating_agent(stamp), None);
    // The run's own stamp names no agent either.
    let run_stamp = effective.scope.as_ref().unwrap();
    assert_eq!(crate::gh_shim_relay::delegating_agent(run_stamp), None);
}

#[test]
fn worker_run_under_a_head_scope_is_refused_parent_not_worker() {
    let (f, _) = worker_fixture(worker_run_stamp("head-a", 3, Some("linked")));
    let detail = worker_refusal(&f);
    assert_eq!(detail["reason"], "parent_not_worker");
    assert_eq!(detail["on_behalf_of_agent"], "agent-a");
    assert_eq!(detail["run_id"], "run-w");
    assert!(detail.get("agent_id").is_none(), "{detail}");
}

#[test]
fn worker_run_without_live_worker_route_is_refused_never_served_by_the_head() {
    let (mut f, _) = worker_fixture(worker_run_stamp("worker-a", 5, Some("linked")));
    f.routes.remove(&channel(4));
    assert_eq!(worker_refusal(&f)["reason"], "worker_route_not_bound");
    // A worker route under another epoch of the scope is not this run's.
    let (f, _) = worker_fixture(worker_run_stamp("worker-a", 6, Some("linked")));
    assert_eq!(worker_refusal(&f)["reason"], "worker_route_not_bound");
    // Nor is a route of the worker's scope bound in codemode.
    let (mut f, _) = worker_fixture(worker_run_stamp("worker-a", 5, Some("linked")));
    let worker = f.routes.get_mut(&channel(4)).unwrap();
    Arc::get_mut(&mut worker.0).unwrap().call_mode = CallMode::Codemode;
    assert_eq!(worker_refusal(&f)["reason"], "worker_route_not_bound");
}

#[test]
fn worker_run_needs_a_linked_parent() {
    for (state, reason) in [
        (Some("pending"), "parent_pending"),
        (Some("ended"), "parent_ended"),
        (None, "parent_state_missing"),
    ] {
        let (f, _) = worker_fixture(worker_run_stamp("worker-a", 5, state));
        assert_eq!(worker_refusal(&f)["reason"], reason, "{state:?}");
    }
    let mut run = worker_run_stamp("worker-a", 5, Some("linked"));
    run.parent = None;
    let (f, _) = worker_fixture(run);
    assert_eq!(worker_refusal(&f)["reason"], "parent_missing");
}

#[test]
fn worker_run_carrying_delegation_flow_or_an_agent_is_refused() {
    let mut delegating = worker_run_stamp("worker-a", 5, Some("linked"));
    delegating.attributes.delegates = true;
    let (f, _) = worker_fixture(delegating);
    assert_eq!(worker_refusal(&f)["reason"], "worker_run_delegates");

    let mut flow = worker_run_stamp("worker-a", 5, Some("linked"));
    flow.attributes.flow_id = Some("flow-1".into());
    let (f, _) = worker_fixture(flow);
    assert_eq!(worker_refusal(&f)["reason"], "worker_run_flow");

    let mut both = worker_run_stamp("worker-a", 5, Some("linked"));
    both.attributes.agent_id = Some("agent-a".into());
    let (f, _) = worker_fixture(both);
    assert_eq!(worker_refusal(&f)["reason"], "run_attribution_conflict");

    let mut unauthorized = worker_run_stamp("worker-a", 5, Some("linked"));
    unauthorized.owner_authorized = false;
    let (f, _) = worker_fixture(unauthorized);
    assert_eq!(worker_refusal(&f)["reason"], "run_owner_unauthorized");
}
