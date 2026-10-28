use super::*;

const READS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "search",
    "outline",
    "zoom",
    "callgraph",
];
const CALLS: &[(&str, &str)] = &[
    ("bash_status", "bash_status"),
    ("bash_output", "bash_status"),
    ("bash_watch", "bash_watch"),
];
const PROOFS: ProofCapabilities = ProofCapabilities {
    call_modes: true,
    on_behalf_of: true,
};

struct Fixture {
    dir: tempfile::TempDir,
    ctx: Arc<AppContext>,
    carrier: RouteIdentity,
    policy: RouteIdentity,
}

fn identity(root: &Path, kind: &str, id: &str, channel: u16) -> RouteIdentity {
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "mcp:runner/raw harness".into(),
        session: "session".into(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: channel,
            route_epoch: 9,
            project_root: root.into(),
            harness: "mcp:runner/raw harness".into(),
            session_id: "session".into(),
            principal_id: Some(id.into()),
        },
        consumer_elicitation_capable: true,
        tool_result_body: true,
        disabled_tools: Arc::new(vec![]),
        scope: Some(
            serde_json::from_value(json!({
                "owner":{"kind":"reserved","module_id":"prefrontal-core"},
                "ref":"session-scope", "scope_epoch":7, "kind":kind, "owner_authorized":true
            }))
            .unwrap(),
        ),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
    }))
}

impl Fixture {
    fn new(kind: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ctx = Arc::new(AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.clone()),
                restrict_to_project_root: false,
                storage_dir: Some(root.join("storage")),
                ..Default::default()
            },
        ));
        ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&root.join("aft.db")).unwrap(),
        )));
        let mut carrier = identity(&root, kind, "reserved:occipital", 41);
        Arc::get_mut(&mut carrier.0).unwrap().trust = BindTrust::Untrusted;
        let policy = identity(
            &root,
            kind,
            if kind == "worker" {
                "reserved:broca"
            } else {
                "reserved:prefrontal-core"
            },
            43,
        );
        let fixture = Self {
            dir,
            ctx,
            carrier,
            policy,
        };
        if kind == "worker" {
            fixture.record(&fixture.policy, "worker");
        }
        fixture
    }

    fn record(&self, route: &RouteIdentity, preset: &str) {
        let key = remote_policy::session_preset_key(route, preset).unwrap();
        crate::db::rich_ui::record_preset(&self.ctx.db().unwrap().lock().unwrap(), &key, 10)
            .unwrap();
    }

    fn resolve(&self) -> Session {
        resolve(&self.ctx, &self.carrier, &[self.policy.clone()], PROOFS).unwrap()
    }

    fn code(&self, routes: &[RouteIdentity]) -> String {
        resolve(&self.ctx, &self.carrier, routes, PROOFS)
            .err()
            .expect("must refuse")
            .code
    }
}

fn reads_and_calls(f: &Fixture, caps: ProofCapabilities, calls: bool) {
    let before = f.ctx.db().unwrap().lock().unwrap().total_changes();
    let session = resolve(&f.ctx, &f.carrier, &[f.policy.clone()], caps).unwrap();
    for name in READS {
        session.admit(&f.ctx, name).unwrap();
    }
    for (id, name) in CALLS {
        let result = session.admit(&f.ctx, name);
        if calls {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code, "unknown_tool", "{id}");
        }
    }
    assert_eq!(
        f.ctx.db().unwrap().lock().unwrap().total_changes(),
        before,
        "resolution must be read-only"
    );
}

#[test]
fn head_and_worker_positive_without_head_rows() {
    for kind in ["head", "worker"] {
        reads_and_calls(&Fixture::new(kind), PROOFS, true);
    }
}

#[test]
fn non_core_head_positive() {
    let mut f = Fixture::new("head");
    for route in [&mut f.carrier, &mut f.policy] {
        Arc::get_mut(&mut route.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .owner = Principal::Reserved {
            module_id: "other-owner".into(),
        };
    }
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut Arc::get_mut(&mut f.policy.0).unwrap().spawn_principal
    {
        *principal_id = Some("reserved:other-owner".into());
    }
    reads_and_calls(&f, PROOFS, true);
}

#[test]
fn flow_head_is_reads_only() {
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .attributes
        .flow_id = Some("flow".into());
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn delegated_head_is_reads_only() {
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .attributes
        .on_behalf_of_agent = Some("worker".into());
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn expiring_head_is_reads_only() {
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .expires_at_ms = Some(u64::MAX);
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn unauthorized_head_is_reads_only() {
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .owner_authorized = false;
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn expiring_worker_is_reads_only() {
    let mut f = Fixture::new("worker");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .expires_at_ms = Some(u64::MAX);
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn unauthorized_worker_is_reads_only_even_with_row() {
    let mut f = Fixture::new("worker");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .owner_authorized = false;
    reads_and_calls(&f, PROOFS, false);
}

#[test]
fn stripped_expiry_is_not_head_or_worker_proof() {
    for kind in ["head", "worker"] {
        reads_and_calls(
            &Fixture::new(kind),
            ProofCapabilities {
                call_modes: false,
                on_behalf_of: true,
            },
            false,
        );
    }
    reads_and_calls(
        &Fixture::new("head"),
        ProofCapabilities {
            call_modes: true,
            on_behalf_of: false,
        },
        false,
    );
}

#[test]
fn role_proof_uses_declared_stamp_fields_not_missing_stripped_values() {
    let mut manifest = manifest::build_manifest();
    for missing in ["call-modes/v1", "scope-on-behalf-of/v1"] {
        let provides = &mut manifest.capabilities.as_mut().unwrap().provides;
        *provides = vec!["call-modes/v1".into(), "scope-on-behalf-of/v1".into()];
        provides.retain(|value| value != missing);
        reads_and_calls(
            &Fixture::new("head"),
            ProofCapabilities::from_manifest(&manifest),
            false,
        );
        if missing == "call-modes/v1" {
            reads_and_calls(
                &Fixture::new("worker"),
                ProofCapabilities::from_manifest(&manifest),
                false,
            );
        }
    }
    manifest.capabilities = None;
    reads_and_calls(
        &Fixture::new("head"),
        ProofCapabilities::from_manifest(&manifest),
        false,
    );
}

#[test]
fn ephemeral_and_unscoped_never_become_heads() {
    let mut f = Fixture::new("ephemeral");
    assert_eq!(f.code(&[f.policy.clone()]), "session_not_bound");
    Arc::get_mut(&mut f.carrier.0).unwrap().scope = None;
    assert_eq!(f.code(&[f.policy.clone()]), "session_not_bound");
}

#[test]
fn session_identity_is_scope_then_exact_labels() {
    let mut f = Fixture::new("head");
    let other = tempfile::tempdir().unwrap();
    for label in ["harness", "session", "root"] {
        let mut carrier = identity(&f.carrier.project_root, "head", "reserved:occipital", 41);
        let data = Arc::get_mut(&mut carrier.0).unwrap();
        match label {
            "harness" => data.harness = "RUNNER/raw harness".into(),
            "session" => data.session = "Session".into(),
            _ => {
                data.project_root = other.path().canonicalize().unwrap();
                data.root = ProjectRootId::from_path(&data.project_root).unwrap();
            }
        }
        let mut decoy = identity(
            &carrier.project_root,
            "head",
            "reserved:prefrontal-core",
            45,
        );
        let decoy_data = Arc::get_mut(&mut decoy.0).unwrap();
        decoy_data.harness = carrier.harness.clone();
        decoy_data.session = carrier.session.clone();
        decoy_data.scope.as_mut().unwrap().scope_ref = "different-session-scope".into();
        for routes in [
            vec![f.policy.clone(), decoy.clone()],
            vec![decoy, f.policy.clone()],
        ] {
            assert_eq!(
                resolve(&f.ctx, &carrier, &routes, PROOFS)
                    .err()
                    .unwrap()
                    .code,
                "session_not_bound",
                "{label}"
            );
        }
    }
    Arc::get_mut(&mut f.policy.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .scope_epoch += 1;
    assert_eq!(
        f.code(&[f.policy.clone()]),
        "session_not_bound",
        "matching labels never identify S"
    );
    assert_eq!(f.code(&[f.carrier.clone()]), "session_not_bound");
}

#[test]
fn worker_row_is_exact_and_missing_or_reader_is_not_head() {
    for field in [
        "missing",
        "reader",
        "root",
        "harness",
        "session",
        "principal",
        "owner",
        "ref",
        "epoch",
    ] {
        let f = Fixture::new("worker");
        let db = f.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        conn.execute("DELETE FROM rich_ui_session_presets", [])
            .unwrap();
        let mut key = remote_policy::session_preset_key(&f.policy, "worker").unwrap();
        match field {
            "missing" => {}
            "reader" => key.preset = "reader".into(),
            "root" => key.project_root.push('x'),
            "harness" => key.harness.push('x'),
            "session" => key.session.push('x'),
            "principal" => key.principal.push('x'),
            "owner" => key.owner.push('x'),
            "ref" => key.scope_ref.push('x'),
            "epoch" => key.epoch -= 1,
            _ => unreachable!(),
        }
        if field != "missing" {
            crate::db::rich_ui::record_preset(&conn, &key, 10).unwrap();
        }
        drop(conn);
        assert_eq!(f.code(&[f.policy.clone()]), "session_not_bound", "{field}");
        f.ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&f.dir.path().join("aft.db")).unwrap(),
        )));
        assert_eq!(
            f.code(&[f.policy.clone()]),
            "session_not_bound",
            "after reopen: {field}"
        );
        f.record(&f.policy, "worker");
        if field != "reader" {
            reads_and_calls(&f, PROOFS, true);
        }
    }
}

#[test]
fn policy_route_agreement_precedes_root_selection_in_both_orders() {
    for field in [
        "harness",
        "session",
        "disabled",
        "other-root-disabled",
        "unmatched-roots",
        "worker-principal",
    ] {
        let mut f = Fixture::new(if field == "worker-principal" {
            "worker"
        } else {
            "head"
        });
        let other_root = tempfile::tempdir().unwrap();
        let mut other = identity(
            &f.policy.project_root,
            if field == "worker-principal" {
                "worker"
            } else {
                "head"
            },
            principal(&f.policy).unwrap(),
            45,
        );
        let data = Arc::get_mut(&mut other.0).unwrap();
        match field {
            "harness" => data.harness.push('x'),
            "session" => data.session.push('x'),
            "disabled" => data.disabled_tools = Arc::new(vec!["read".into()]),
            "worker-principal" => {
                if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
                    &mut data.spawn_principal
                {
                    *principal_id = Some("reserved:other-worker".into());
                }
            }
            _ => {
                data.project_root = other_root.path().canonicalize().unwrap();
                data.root = ProjectRootId::from_path(&data.project_root).unwrap();
                if field == "other-root-disabled" {
                    data.disabled_tools = Arc::new(vec!["read".into()]);
                } else {
                    Arc::get_mut(&mut f.carrier.0).unwrap().project_root =
                        PathBuf::from("no-matching-root");
                }
            }
        }
        if field == "worker-principal" {
            f.record(&other, "worker");
        }
        for routes in [
            vec![f.policy.clone(), other.clone()],
            vec![other, f.policy.clone()],
        ] {
            assert_eq!(f.code(&routes), "session_route_ambiguous", "{field}");
        }
    }
}

#[test]
fn multi_root_and_duplicate_head_routes_select_before_root_agreement() {
    let f = Fixture::new("head");
    let other_root = tempfile::tempdir().unwrap();
    let other = identity(
        &other_root.path().canonicalize().unwrap(),
        "head",
        "reserved:prefrontal-core",
        45,
    );
    for other in [other, f.policy.clone()] {
        for routes in [
            vec![f.policy.clone(), other.clone()],
            vec![other.clone(), f.policy.clone()],
        ] {
            let selected = resolve(&f.ctx, &f.carrier, &routes, PROOFS).unwrap();
            assert_eq!(selected.identity.project_root, f.policy.project_root);
            assert_eq!(selected.identity.root, f.policy.root);
        }
    }
}

#[test]
fn transient_owner_route_never_qualifies_even_with_worker_row() {
    let f = Fixture::new("worker");
    let transient = identity(
        &f.policy.project_root,
        "worker",
        "reserved:prefrontal-core",
        45,
    );
    f.record(&transient, "worker");
    for routes in [
        vec![f.policy.clone(), transient.clone()],
        vec![transient, f.policy.clone()],
    ] {
        let selected = resolve(&f.ctx, &f.carrier, &routes, PROOFS).unwrap();
        assert_eq!(principal(&selected.identity), Some("reserved:broca"));
    }
}

#[test]
fn effective_principal_is_rebuilt_and_installed_for_read_and_watch_dispatch() {
    for kind in ["head", "worker"] {
        let f = Fixture::new(kind);
        let selected = f.resolve();
        assert_eq!(selected.identity.trust, BindTrust::Untrusted);
        assert!(!selected.identity.consumer_elicitation_capable);
        assert!(selected.identity.run_owner.is_none());
        assert!(selected.identity.run_scope.get().is_none());
        let expected = AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::Untrusted,
            route_channel: 41,
            route_epoch: 9,
            project_root: f.policy.project_root.clone(),
            harness: f.policy.harness.clone(),
            session_id: f.policy.session.clone(),
            principal_id: Some(principal(&f.policy).unwrap().into()),
        };
        assert_eq!(selected.identity.spawn_principal, expected);
        for name in ["read", "bash_watch"] {
            selected
                .with_policy(&f.ctx, "effective-principal", name, json!({}), |_| {
                    assert_eq!(
                        crate::sandbox_spawn::current_authenticated_principal(),
                        expected
                    );
                    assert!(matches!(
                        crate::access_policy::current_ask_route(),
                        crate::access_policy::AskRoute::Deny
                    ));
                    assert_eq!(
                        crate::access_policy::current_scope(),
                        Some(access_grants::policy_scope_key(&f.carrier))
                    );
                })
                .unwrap();
        }
        let mut mismatched = identity(
            &f.policy.project_root,
            kind,
            principal(&f.policy).unwrap(),
            43,
        );
        Arc::get_mut(&mut mismatched.0)
            .unwrap()
            .scope
            .as_mut()
            .unwrap()
            .scope_ref
            .push('x');
        assert_eq!(f.code(&[mismatched]), "session_not_bound");
    }
}

#[test]
fn policy_disables_and_reader_admission_have_native_precedence() {
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0).unwrap().disabled_tools =
        Arc::new(vec!["read".into(), "bash_watch".into()]);
    reads_and_calls(&f, PROOFS, true);
    Arc::get_mut(&mut f.policy.0).unwrap().disabled_tools = Arc::new(vec!["bash_status".into()]);
    let session = f.resolve();
    assert_eq!(
        session.admit(&f.ctx, "bash_status").unwrap_err().code,
        "tool_disabled"
    );
    session.admit(&f.ctx, "bash_watch").unwrap();
    Arc::get_mut(&mut f.policy.0).unwrap().disabled_tools = Arc::new(vec!["bash_watch".into()]);
    assert_eq!(
        f.resolve().admit(&f.ctx, "bash_watch").unwrap_err().code,
        "tool_disabled"
    );
    Arc::get_mut(&mut f.policy.0).unwrap().disabled_tools = Arc::new(vec!["bash".into()]);
    reads_and_calls(&f, PROOFS, true);
    Arc::get_mut(&mut f.policy.0).unwrap().disabled_tools = Arc::new(vec!["bash_status".into()]);
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .attributes
        .flow_id = Some("flow".into());
    assert_eq!(
        f.resolve().admit(&f.ctx, "bash_status").unwrap_err().code,
        "unknown_tool"
    );
}

#[test]
fn worker_two_presets_and_unavailable_database_fail_before_role() {
    let f = Fixture::new("worker");
    f.record(&f.policy, "reader");
    assert_eq!(f.code(&[f.policy.clone()]), "session_preset_ambiguous");
    f.ctx.clear_db();
    assert_eq!(f.code(&[f.policy.clone()]), "database_unavailable");
}

#[test]
fn worker_without_candidate_is_unbound_even_without_database() {
    let f = Fixture::new("worker");
    f.ctx.clear_db();
    assert_eq!(f.code(&[f.carrier.clone()]), "session_not_bound");
}

const MANIFEST: &[u8] = include_bytes!("../../../../tests/fixtures/rich_ui/manifest.jcs.json");

async fn exchange(f: &Fixture, name: &str, args: Value, preset: &str) -> Frame {
    exchange_keyed(f, name, args, preset, None, 71).await
}

fn decoded_request(
    name: &str,
    args: Value,
    preset: &str,
    key: Option<&str>,
    corr: u64,
) -> DecodedFrame {
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        9,
        corr,
        serde_json::to_vec(&{
            let mut body =
                json!({"op":"tool.call", "name":name, "arguments":args, "preset":preset});
            if let Some(key) = key {
                body["call_key"] = key.into();
            }
            body
        })
        .unwrap(),
    )
    .unwrap();
    DecodedFrame {
        frame,
        parsed_body: Default::default(),
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
        phase_trace: PhaseTrace::new(Instant::now()),
    }
}

async fn exchange_keyed(
    f: &Fixture,
    name: &str,
    args: Value,
    preset: &str,
    key: Option<&str>,
    corr: u64,
) -> Frame {
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(f.carrier.root.clone(), f.ctx.clone()));
    let routes = HashMap::from([
        (route_key(41, 9), f.carrier.clone()),
        (route_key(43, 9), f.policy.clone()),
    ]);
    let decoded = decoded_request(name, args, preset, key, corr);
    let reply = tool_provider::route_tests::exchange_decoded_with_capabilities(
        decoded,
        &routes,
        &executor,
        &Arc::new(DispatchPathMetrics::new()),
        |request, ctx| {
            assert_eq!(request.command, "read");
            assert!(matches!(
                crate::sandbox_spawn::current_authenticated_principal(),
                AuthenticatedPrincipal::RouteBind {
                    trust: PrincipalTrust::Untrusted,
                    route_channel: 41,
                    route_epoch: 9,
                    ..
                }
            ));
            crate::commands::read::handle_read(&request, ctx)
        },
        PROOFS,
    )
    .await;
    reply.frame
}

#[tokio::test]
async fn session_read_dispatch_uses_policy_disables_and_closed_envelope() {
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    for kind in ["head", "worker"] {
        let mut f = Fixture::new(kind);
        std::fs::write(f.dir.path().join("in-root.txt"), "inside\n").unwrap();
        access_grants::register(&f.policy, &f.ctx).unwrap();
        Arc::get_mut(&mut f.carrier.0).unwrap().disabled_tools = Arc::new(vec!["read".into()]);
        let args = json!({"rich_ui_op":"read@1", "path": f.dir.path().join("in-root.txt")});
        let reply = exchange(&f, "read", args.clone(), "reader").await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["isError"], false, "{body}");
        assert!(body["structuredContent"]["text"]
            .as_str()
            .unwrap()
            .contains("inside"));
        let operation = super::super::admission::prepare("read", &args).unwrap();
        super::super::carrier::validate_body(Some(&operation.entry), &body).unwrap();
        Arc::get_mut(&mut f.policy.0).unwrap().disabled_tools = Arc::new(vec!["read".into()]);
        let reply = exchange(&f, "read", args, "head").await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body, super::super::carrier::error_body("tool_disabled"));
    }
}

#[tokio::test]
async fn carrier_presets_never_promote_flow_head_and_task_seam_fails_closed() {
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .attributes
        .flow_id = Some("flow".into());
    for preset in ["head", "worker", "reader"] {
        let reply = exchange(
            &f,
            "bash_status",
            json!({"rich_ui_op":"bash_status@1", "task":"call"}),
            preset,
        )
        .await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body, super::super::carrier::error_body("unknown_tool"));
    }
    for kind in ["head", "worker"] {
        let f = Fixture::new(kind);
        for op in ["bash_status", "bash_output", "bash_watch"] {
            let name = if op == "bash_output" {
                "bash_status"
            } else {
                op
            };
            let mut args = json!({"rich_ui_op":format!("{op}@1"), "task":"call"});
            if op == "bash_watch" {
                args["pattern"] = "done".into();
            }
            let reply = exchange(&f, name, args, "reader").await;
            assert_eq!(
                serde_json::from_slice::<Value>(&reply.body).unwrap(),
                super::super::carrier::error_body("task_not_found")
            );
        }
    }
}

#[tokio::test]
async fn worker_opening_database_waits_for_every_carrier_operation() {
    use std::future::Future;
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    for op in ["read", "bash_status", "bash_output", "bash_watch"] {
        let name = if op == "bash_output" {
            "bash_status"
        } else {
            op
        };
        let f = Fixture::new("worker");
        f.ctx.clear_db();
        f.ctx.mark_database_runtime_initializing_for_test();
        let executor = Executor::new();
        executor.register_actor(f.carrier.root.clone(), f.ctx.clone());
        let args = if name == "read" {
            json!({"rich_ui_op":"read@1","path":"x"})
        } else if name == "bash_watch" {
            json!({"rich_ui_op":"bash_watch@1","task":"call","pattern":"done"})
        } else {
            json!({"rich_ui_op":format!("{op}@1"),"task":"call"})
        };
        let decoded = DecodedFrame {
            frame: Frame::build(
                FrameType::Request,
                control_flags(),
                41,
                9,
                71,
                serde_json::to_vec(&json!({"op":"tool.call", "name":name,"arguments":args}))
                    .unwrap(),
            )
            .unwrap(),
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let routes = HashMap::from([
            (route_key(41, 9), f.carrier.clone()),
            (route_key(43, 9), f.policy.clone()),
        ]);
        let mut waits = persistence::DatabaseWaits::default();
        assert!(
            waits.defer(decoded, &routes, &executor).is_ok(),
            "{name} must wait"
        );
        let resumed = waits.next();
        tokio::pin!(resumed);
        std::future::poll_fn(|cx| {
            assert!(resumed.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        f.ctx.set_db(Arc::new(std::sync::Mutex::new(
            crate::db::open(&f.dir.path().join("aft.db")).unwrap(),
        )));
        f.ctx.finish_database_runtime(Ok(()));
        resumed.await;
        reads_and_calls(&f, PROOFS, true);
    }
}

fn read_under_policy(f: &Fixture, path: &Path, carried: bool) -> Response {
    let args = json!({"file":path});
    let run = |_| {
        let request: RawRequest =
            serde_json::from_value(json!({"id":"policy-read","command":"read","file":path}))
                .unwrap();
        crate::commands::read::handle_read(&request, &f.ctx)
    };
    if carried {
        f.resolve()
            .with_policy(&f.ctx, "policy-read", "read", args, run)
            .unwrap_or_else(|r| r)
    } else {
        crate::sandbox_spawn::with_authenticated_principal(f.policy.spawn_principal.clone(), || {
            call_policy::with_tool_call_policy(
                &f.ctx,
                &f.policy,
                "policy-read",
                crate::access_policy::AskRoute::Deny,
                BindTrust::FirstParty,
                "read",
                args,
                run,
                |r| r.unwrap_or_else(|r| r),
            )
        })
    }
}

#[test]
fn outside_allow_paths_cannot_override_carrier_force_restriction_or_ask_deny() {
    for kind in ["head", "worker"] {
        let f = Fixture::new(kind);
        access_grants::register(&f.policy, &f.ctx).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.txt");
        std::fs::write(&path, "outside\n").unwrap();
        let db = f.ctx.db().unwrap();
        let grant = crate::access_policy::AccessGrant {
            path: crate::access_policy::policy_path(outside.path()),
            kind: crate::access_policy::GrantKind::Subtree,
            operation: crate::access_policy::AccessOperation::Read,
        };
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.policy),
            &grant,
        )
        .unwrap();
        assert!(
            read_under_policy(&f, &path, false).success,
            "session's own grant is the control"
        );
        assert_eq!(
            read_under_policy(&f, &path, true).data["code"],
            "path_outside_root"
        );
        let ungranted = tempfile::tempdir().unwrap();
        let ungranted_path = ungranted.path().join("not-granted.txt");
        std::fs::write(&ungranted_path, "ask\n").unwrap();
        assert_eq!(
            read_under_policy(&f, &ungranted_path, true).data["code"],
            "path_outside_root"
        );
        let inside = f.dir.path().join("inside.txt");
        std::fs::write(&inside, "inside\n").unwrap();
        assert!(read_under_policy(&f, &inside, true).success);
    }
}

#[test]
fn confined_worker_uses_policy_principal_and_keeps_native_fence_code() {
    let f = Fixture::new("worker");
    remote_policy::catalog(
        json!({"preset":"worker","params":{"confine":"root"}}),
        &f.policy,
        &f.ctx,
    )
    .unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("outside.txt");
    std::fs::write(&path, "outside\n").unwrap();
    assert_eq!(
        read_under_policy(&f, &path, true).data["code"],
        "path_outside_confined_root"
    );
}

#[cfg(unix)]
#[test]
fn carrier_symlink_inside_root_cannot_use_agent_outside_grant() {
    for kind in ["head", "worker"] {
        let f = Fixture::new(kind);
        access_grants::register(&f.policy, &f.ctx).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("outside.txt");
        std::fs::write(&target, "outside\n").unwrap();
        let link = f.dir.path().join("inside-link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let grant = crate::access_policy::AccessGrant {
            path: crate::access_policy::policy_path(outside.path()),
            kind: crate::access_policy::GrantKind::Subtree,
            operation: crate::access_policy::AccessOperation::Read,
        };
        crate::db::access_grants::insert(
            &f.ctx.db().unwrap().lock().unwrap(),
            &access_grants::scope_key(&f.policy),
            &grant,
        )
        .unwrap();
        assert!(read_under_policy(&f, &link, false).success);
        assert_eq!(
            read_under_policy(&f, &link, true).data["code"],
            "path_outside_root"
        );
        assert!(
            read_under_policy(&f, &target, false).success,
            "the carrier guard must not leak into the next agent read"
        );
    }
}

#[tokio::test]
async fn flow_read_carrier_keeps_earlier_principal_refusal() {
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let mut f = Fixture::new("ephemeral");
    Arc::get_mut(&mut f.carrier.0)
        .unwrap()
        .scope
        .as_mut()
        .unwrap()
        .scope_ref = "flow-read/probe".into();
    let reply = exchange(
        &f,
        "read",
        json!({"rich_ui_op":"read@1","path":"x"}),
        "head",
    )
    .await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "file_read_exact_route_not_admitted", "{body}");
}

#[tokio::test]
async fn unscoped_occipital_tool_call_is_not_session_bound() {
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.carrier.0).unwrap().scope = None;
    let reply = exchange(
        &f,
        "read",
        json!({"rich_ui_op":"read@1","path":"x"}),
        "head",
    )
    .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap()["code"],
        "session_not_bound"
    );
}

#[tokio::test]
async fn unresolved_policy_is_typed_and_database_and_preset_errors_precede_mode() {
    let _manifest = super::super::chunks::set_test_manifest_for_test(Some(MANIFEST.to_vec()));
    let mut f = Fixture::new("head");
    Arc::get_mut(&mut f.policy.0).unwrap().session.push('x');
    let reply = exchange(
        &f,
        "read",
        json!({"rich_ui_op":"read@1","path":"x"}),
        "head",
    )
    .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap()["code"],
        "session_not_bound"
    );
    let mut f = Fixture::new("worker");
    remote_policy::catalog(json!({"preset":"reader"}), &f.policy, &f.ctx).unwrap();
    Arc::get_mut(&mut f.carrier.0).unwrap().call_mode =
        subc_protocol::call_mode::CallMode::Unknown("unsupported".into());
    let reply = exchange(
        &f,
        "read",
        json!({"rich_ui_op":"read@1","path":"x"}),
        "worker",
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap(),
        super::super::carrier::error_body("session_preset_ambiguous")
    );
    f.ctx.clear_db();
    for op in ["read", "bash_status", "bash_output", "bash_watch"] {
        let name = if op == "bash_output" {
            "bash_status"
        } else {
            op
        };
        let args = match op {
            "read" => json!({"rich_ui_op":"read@1","path":"x"}),
            "bash_watch" => json!({"rich_ui_op":"bash_watch@1","task":"call","pattern":"done"}),
            _ => json!({"rich_ui_op":format!("{op}@1"),"task":"call"}),
        };
        let reply = exchange(&f, name, args, "head").await;
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap(),
            super::super::carrier::error_body("database_unavailable")
        );
    }
}

#[path = "bash_tests.rs"]
mod bash_tests;
