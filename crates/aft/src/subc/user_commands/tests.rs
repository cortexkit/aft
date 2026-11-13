use super::*;
use sha2::{Digest, Sha256};

fn occipital() -> Principal {
    Principal::Reserved {
        module_id: "occipital".into(),
    }
}

fn request(op: &str) -> Value {
    let mut params = json!({
        "id":"status", "subject":{"kind":"session", "session":{
            "runner":"thalamus-harness", "harness":"opencode", "session":"chat"
        }}, "args":{}, "caller":{"app":"desktop","admission":"operator_app"}
    });
    if op == "commands.describe" {
        params = json!({});
    }
    if op == "commands.run" {
        params["preview_version"] = json!("version");
        params["request_id"] = json!("Run_1");
    }
    json!({"method":op, "params":params})
}

fn snapshot(building: bool) -> Value {
    json!({
        "search_index":{"status":"ready","files":10},
        "callgraph_store": if building { json!({"status":"building"}) } else { json!({"status":"ready"}) },
        "semantic_index":if building { json!({"status":"building","entries_done":4,"entries_total":10}) } else { json!({"status":"off"}) },
        "status_bar_values":{"errors":2,"warnings":3},
    })
}

fn context() -> (tempfile::TempDir, Arc<AppContext>, Arc<Executor>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Arc::new(AppContext::with_app_and_provider(
        App::default_shared(),
        Box::new(crate::language::StubProvider),
        crate::config::Config {
            project_root: Some(dir.path().into()),
            ..Default::default()
        },
    ));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(ProjectRootId::from_path(dir.path()).unwrap(), ctx.clone()));
    (dir, ctx, executor)
}

fn route(root: &ProjectRootId, principal: &str) -> RouteIdentity {
    session_route(root, principal, "opencode", "chat", "head")
}

fn session_route(
    root: &ProjectRootId,
    principal: &str,
    harness: &str,
    session: &str,
    scope_kind: &str,
) -> RouteIdentity {
    let mut route = crate::subc::test_support::route_identity(root, session);
    let data = Arc::get_mut(&mut route.0).unwrap();
    data.harness = harness
        .parse::<crate::harness::Harness>()
        .unwrap()
        .wire_label();
    let AuthenticatedPrincipal::RouteBind {
        principal_id,
        harness: raw,
        ..
    } = &mut data.spawn_principal
    else {
        unreachable!()
    };
    *principal_id = Some(principal.into());
    *raw = harness.into();
    data.scope = Some(
        serde_json::from_value(json!({
            "owner":{"kind":"reserved", "module_id":"prefrontal-core"},
            "ref":format!("{scope_kind}-scope"), "scope_epoch":1, "kind":scope_kind,
            "owner_authorized":true,
            "attributes": if scope_kind == "ephemeral" {
                json!({"agent_id":"agent", "run_id":"run"})
            } else {
                json!({"agent_id":"agent"})
            }
        }))
        .unwrap(),
    );
    if scope_kind == "ephemeral" {
        data.call_mode = subc_protocol::call_mode::CallMode::Codemode;
    }
    route
}

#[test]
fn user_commands_admission_precedes_params_and_state_for_all_operations() {
    for op in contract::MODULE_OPS {
        for principal in [
            None,
            Some(Principal::Direct),
            Some(Principal::Unverified),
            Some(Principal::Reserved {
                module_id: "broca".into(),
            }),
        ] {
            let error = call(
                principal.as_ref(),
                &json!({"method":op,"params":"malformed"}),
                |_| panic!("wrong caller read state"),
            )
            .unwrap_err();
            assert_eq!(error.code, contract::CALLER_NOT_ADMITTED);
        }
        let error = call(
            Some(&occipital()),
            &json!({"method":op,"params":"malformed"}),
            |_| panic!("malformed params read state"),
        )
        .unwrap_err();
        assert_eq!(error.code, contract::INVALID_PARAMS);
    }
}

#[test]
fn user_commands_describe_is_read_only_and_digest_is_jcs_array_sha256() {
    let reply = call(Some(&occipital()), &request("commands.describe"), |_| {
        panic!("describe read state")
    })
    .unwrap();
    let typed: contract::DescribeReply = serde_json::from_value(reply.clone()).unwrap();
    typed.check().unwrap();
    assert_eq!(typed.commands.len(), 1);
    assert_eq!(typed.commands[0].id, "status");
    assert!(typed.commands[0].run.is_none());
    assert_eq!(typed.commands[0].effects, ["read"]);
    // An independently ordered literal pins the array, not the envelope or
    // serde's declaration-field order.
    let jcs = r#"[{"args":[],"effects":["read"],"id":"status","subject":"session","summary":"Shows whether AFT's search and code map are ready for this chat's project, its open errors, and its running shells.","title":"AFT status"}]"#;
    assert_eq!(
        typed.digest,
        format!("{:x}", Sha256::digest(jcs.as_bytes()))
    );
    assert_eq!(
        typed.digest,
        contract::commands_digest(&typed.commands).unwrap()
    );
}

#[test]
fn user_commands_unknown_args_and_read_only_run_use_contract_refusals() {
    let mut body = request("commands.preview");
    body["params"]["args"] = json!({"surprise":true});
    let error = call(Some(&occipital()), &body, |_| {
        panic!("invalid arg read state")
    })
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert_eq!(error.detail.unwrap()["field"], "args.surprise");
    let error = call(Some(&occipital()), &request("commands.run"), |_| {
        panic!("read-only run read state")
    })
    .unwrap_err();
    assert_eq!(error.code, "invalid_params");
    assert_eq!(error.detail.unwrap()["field"], "id");
    body["params"]["id"] = json!("missing");
    assert_eq!(
        call(Some(&occipital()), &body, |_| unreachable!())
            .unwrap_err()
            .code,
        "unknown_command"
    );
}

#[test]
fn user_commands_session_lookup_matches_attested_runner_harness_and_session() {
    let (dir, ctx, executor) = context();
    let root = ProjectRootId::from_path(dir.path()).unwrap();
    let mut routes = HashMap::from([(route_key(1, 1), route(&root, "reserved:thalamus-harness"))]);
    let mut subject = contract::SessionRef::new("thalamus-harness", "opencode", "chat");
    assert!(Arc::ptr_eq(
        &resolve_session(&routes, &executor, &subject)
            .unwrap()
            .unwrap(),
        &ctx
    ));
    Arc::get_mut(&mut routes.get_mut(&route_key(1, 1)).unwrap().0)
        .unwrap()
        .call_mode = subc_protocol::call_mode::CallMode::Codemode;
    assert!(Arc::ptr_eq(
        &resolve_session(&routes, &executor, &subject)
            .unwrap()
            .unwrap(),
        &ctx
    ));
    subject.session = "missing".into();
    let mut body = request("commands.preview");
    body["params"]["subject"]["session"] = encode(&subject);
    let not_bound = call(Some(&occipital()), &body, |session| {
        resolve_session(&routes, &executor, session)
    })
    .unwrap();
    assert_eq!(not_bound["available"], false);
    assert_eq!(not_bound["reason"], "not_bound");
    subject.session = "chat".into();
    subject.harness = "pi".into();
    assert!(resolve_session(&routes, &executor, &subject)
        .unwrap()
        .is_none());
    subject.harness = "opencode".into();
    subject.runner = "broca".into();
    assert!(resolve_session(&routes, &executor, &subject)
        .unwrap()
        .is_none());
    subject.runner = "reserved:thalamus-harness".into();
    assert_eq!(
        resolve_session(&routes, &executor, &subject)
            .err()
            .unwrap()
            .code,
        "unknown_subject"
    );
    subject.runner = "unrecognised".into();
    assert_eq!(
        resolve_session(&routes, &executor, &subject)
            .err()
            .unwrap()
            .code,
        "unknown_subject"
    );
    subject.runner = "thalamus-harness".into();
    routes.insert(route_key(1, 1), route(&root, "direct"));
    assert!(resolve_session(&routes, &executor, &subject)
        .unwrap()
        .is_none());
}

#[test]
fn user_commands_session_lookup_core_and_runner_routes_preserve_project_status() {
    let (dir, ctx, executor) = context();
    let root = ProjectRootId::from_path(dir.path()).unwrap();
    for (runner, harness, core_harness, core_routes, runner_routes) in [
        ("thalamus-harness", "opencode2", "opencode2", 32, 1),
        ("broca", "broca", "runner", 44, 9),
        ("broca", "broca", "broca", 1, 1),
        ("broca", "runner", "runner", 1, 1),
    ] {
        for (core_count, runner_count) in [
            (core_routes, 0),
            (0, runner_routes),
            (core_routes, runner_routes),
        ] {
            let session = "chat";
            let subject = contract::SessionRef::new(
                runner,
                if harness == "runner" {
                    "broca"
                } else {
                    harness
                },
                session,
            );
            let mut routes = HashMap::new();
            let mut channel = 1;
            for (principal, raw_harness, count) in [
                (
                    "reserved:prefrontal-core".to_string(),
                    core_harness,
                    core_count,
                ),
                (format!("reserved:{runner}"), harness, runner_count),
            ] {
                for _ in 0..count {
                    routes.insert(
                        route_key(channel, 1),
                        session_route(&root, &principal, raw_harness, session, "head"),
                    );
                    channel += 1;
                }
            }
            for building in [false, true] {
                *ctx.semantic_index_status().write().unwrap() = if building {
                    crate::context::SemanticIndexStatus::Building {
                        stage: "embedding".into(),
                        files: Some(10),
                        entries_done: Some(4),
                        entries_total: Some(10),
                    }
                } else {
                    crate::context::SemanticIndexStatus::ready()
                };
                let mut body = request("commands.preview");
                body["params"]["subject"]["session"] = encode(&subject);
                let reply = call(Some(&occipital()), &body, |session| {
                    let resolved = resolve_session(&routes, &executor, session)?;
                    let bound = resolved.as_ref().expect("working head must be bound");
                    assert!(Arc::ptr_eq(bound, &ctx));
                    assert_eq!(bound.config().project_root.as_deref(), Some(dir.path()));
                    Ok(resolved)
                })
                .unwrap();
                let preview: contract::Preview = serde_json::from_value(reply).unwrap();
                preview.check().unwrap();
                assert!(preview.available);
                assert_eq!(preview.reason, None);
                let meaning = &preview.table.as_ref().unwrap().rows[2].cells[0];
                assert_eq!(
                    meaning.state.as_deref(),
                    Some(if building { "building" } else { "ready" })
                );
            }
        }
    }
}

#[test]
fn user_commands_thalamus_runner_alias_requires_unique_live_concrete_evidence() {
    let (dir, ctx, executor) = context();
    let root = ProjectRootId::from_path(dir.path()).unwrap();
    let subject = contract::SessionRef::new("thalamus-harness", "opencode2", "chat");
    let mut routes = HashMap::from([(
        route_key(1, 1),
        session_route(&root, "reserved:prefrontal-core", "runner", "chat", "head"),
    )]);
    let mut body = request("commands.preview");
    body["params"]["subject"]["session"] = encode(&subject);
    let not_bound = call(Some(&occipital()), &body, |session| {
        resolve_session(&routes, &executor, session)
    })
    .unwrap();
    assert_eq!(not_bound["available"], false);
    assert_eq!(not_bound["reason"], "not_bound");

    // Concrete labels on unrelated sessions or principals are not evidence.
    for (channel, principal, session) in [
        (2, "reserved:thalamus-harness", "other-chat"),
        (3, "reserved:broca", "chat"),
        (4, "direct", "chat"),
    ] {
        routes.insert(
            route_key(channel, 1),
            session_route(&root, principal, "opencode2", session, "head"),
        );
    }
    assert!(resolve_session(&routes, &executor, &subject)
        .unwrap()
        .is_none());

    // Harness evidence is independent of whether the route has a project actor.
    let evidence_dir = tempfile::tempdir().unwrap();
    let evidence_root = ProjectRootId::from_path(evidence_dir.path()).unwrap();
    for principal in ["reserved:thalamus-harness", "reserved:prefrontal-core"] {
        routes.insert(
            route_key(5, 1),
            session_route(&evidence_root, principal, "opencode2", "chat", "head"),
        );
        assert!(Arc::ptr_eq(
            &resolve_session(&routes, &executor, &subject)
                .unwrap()
                .unwrap(),
            &ctx
        ));
        routes.insert(
            route_key(6, 1),
            session_route(&evidence_root, principal, "claude-code", "chat", "head"),
        );
        assert!(resolve_session(&routes, &executor, &subject)
            .unwrap()
            .is_none());
        routes.remove(&route_key(6, 1));
        routes.remove(&route_key(5, 1));
    }
}

#[test]
fn user_commands_session_lookup_scopes_only_conflict_for_another_project_root() {
    let (dir, ctx, executor) = context();
    let root = ProjectRootId::from_path(dir.path()).unwrap();
    let (other_dir, other_ctx, _) = context();
    let other_root = ProjectRootId::from_path(other_dir.path()).unwrap();
    assert!(executor.register_actor(other_root.clone(), other_ctx));
    let subject = contract::SessionRef::new("thalamus-harness", "opencode2", "chat");
    let mut routes = HashMap::new();
    for (channel, kind) in [(1, "head"), (2, "worker"), (3, "ephemeral")] {
        routes.insert(
            route_key(channel, 1),
            session_route(&root, "reserved:prefrontal-core", "opencode2", "chat", kind),
        );
    }
    for (channel, principal, harness, session) in [
        (4, "reserved:prefrontal-core", "opencode2", "other-chat"),
        (5, "direct", "opencode2", "chat"),
        (6, "reserved:broca", "opencode2", "chat"),
        (7, "reserved:prefrontal-core", "claude-code", "chat"),
    ] {
        routes.insert(
            route_key(channel, 1),
            session_route(&other_root, principal, harness, session, "head"),
        );
    }
    assert!(Arc::ptr_eq(
        &resolve_session(&routes, &executor, &subject)
            .unwrap()
            .unwrap(),
        &ctx
    ));
    let mut body = request("commands.preview");
    body["params"]["subject"]["session"] = encode(&subject);
    for kind in ["head", "worker", "ephemeral"] {
        routes.insert(
            route_key(8, 1),
            session_route(
                &other_root,
                "reserved:prefrontal-core",
                "opencode2",
                "chat",
                kind,
            ),
        );
        let not_bound = call(Some(&occipital()), &body, |session| {
            resolve_session(&routes, &executor, session)
        })
        .unwrap();
        assert_eq!(not_bound["available"], false);
        assert_eq!(not_bound["reason"], "not_bound");
    }
}

#[test]
fn user_commands_bound_startup_keeps_five_facts_without_inventing_diagnostics() {
    let (dir, ctx, _executor) = context();
    let db = Arc::new(std::sync::Mutex::new(
        crate::db::open(&dir.path().join("aft.db")).unwrap(),
    ));
    ctx.set_db(db.clone());
    let _db_guard = db.lock().unwrap();
    let started = std::time::Instant::now();
    let reply = call(Some(&occipital()), &request("commands.preview"), |_| {
        Ok(Some(ctx.clone()))
    })
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let preview: contract::Preview = serde_json::from_value(reply).unwrap();
    preview.check().unwrap();
    let facts = preview.facts.unwrap();
    assert_eq!(
        facts
            .iter()
            .map(|fact| fact.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Text search",
            "Code map",
            "Errors",
            "Warnings",
            "Shells running"
        ]
    );
    for fact in &facts[2..4] {
        assert_eq!(fact.data.state.as_deref(), Some("unavailable"));
        assert_eq!(fact.data.value, None);
    }
    assert_eq!(facts[4].data.value, Some(0.0));
    assert_eq!(facts[4].data.unit.as_deref(), Some("count"));
}

#[test]
fn user_commands_preview_shapes_progress_versions_and_low_disk() {
    let ready = preview(&snapshot(false), Some(1));
    let building = preview(&snapshot(true), Some(1));
    ready.check().unwrap();
    building.check().unwrap();
    assert_ne!(ready.version, building.version);
    assert_ne!(ready.version, preview(&snapshot(false), Some(2)).version);
    let facts = building.facts.as_ref().unwrap();
    assert_eq!(facts[0].data.state.as_deref(), Some("ready"));
    assert_eq!(facts[1].data.state.as_deref(), Some("building"));
    assert_eq!(facts[1].data.progress, None);
    assert_eq!(facts[1].data.value, None);
    assert_eq!(facts[2].data.unit.as_deref(), Some("count"));
    assert_eq!(facts[2].data.tone.as_deref(), Some("danger"));
    assert_eq!(facts[3].data.value, Some(3.0));
    assert_eq!(facts[4].data.value, Some(1.0));
    let table = building.table.as_ref().unwrap();
    assert_eq!(table.columns, ["State", "Files"]);
    assert_eq!(table.rows.len(), 3);
    assert!(table.rows.iter().all(|row| row.cells.len() == 2));
    assert_eq!(
        building.table.as_ref().unwrap().rows[2].cells[0].progress,
        Some(0.4)
    );
    let mut text_building = snapshot(false);
    text_building["search_index"] = json!({"status":"building","indexed_files":4,"files":10});
    assert_eq!(
        preview(&text_building, Some(1)).facts.unwrap()[0]
            .data
            .progress,
        Some(0.4)
    );
    let mut counts = snapshot(false);
    counts["status_bar_values"]["errors"] = json!(0);
    assert!(preview(&counts, None).facts.unwrap()[2].data.tone.is_none());
    counts["search_index"] = json!({"status":"unavailable","reason":"disk_reserve"});
    let paused = preview(&counts, Some(0));
    paused.check().unwrap();
    assert!(!paused.available);
    assert_eq!(paused.reason.as_deref(), Some("paused_low_disk"));
}

#[test]
fn user_commands_building_preview_carries_real_meaning_progress_without_starting_work() {
    let (_dir, ctx, _executor) = context();
    *ctx.semantic_index_status().write().unwrap() = crate::context::SemanticIndexStatus::Building {
        stage: "embedding".into(),
        files: Some(10),
        entries_done: Some(4),
        entries_total: Some(10),
    };
    let reply = call(Some(&occipital()), &request("commands.preview"), |_| {
        Ok(Some(ctx.clone()))
    })
    .unwrap();
    let preview: contract::Preview = serde_json::from_value(reply).unwrap();
    preview.check().unwrap();
    let meaning = &preview.table.as_ref().unwrap().rows[2].cells[0];
    assert_eq!(meaning.state.as_deref(), Some("building"));
    assert_eq!(meaning.progress, Some(0.4));
    assert_eq!(ctx.lsp_server_count(), 0);
}

fn vector(name: &str) -> Value {
    serde_json::from_slice(
        &std::fs::read(
            crate::test_support::manifest_dir()
                .join("tests/fixtures/user-commands-v1")
                .join(name),
        )
        .unwrap(),
    )
    .unwrap()
}

fn roundtrip<T: DeserializeOwned + serde::Serialize>(body: &Value) -> T {
    let decoded: T = serde_json::from_value(body.clone()).unwrap();
    let request = Frame::build(FrameType::Request, control_flags(), 1, 1, 3, vec![]).unwrap();
    let reply = response_frame(&request, &encode(&decoded)).unwrap();
    // Fixture whitespace is not wire data. Compare compact frame-body bytes
    // after parsing the published JSON, exactly as the transport writes them.
    assert_eq!(reply.body, serde_json::to_vec(body).unwrap());
    decoded
}

#[test]
fn user_commands_pinned_vectors_roundtrip_through_real_body_serializer() {
    let directory = crate::test_support::manifest_dir().join("tests/fixtures/user-commands-v1");
    let sums = std::fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
    let mut files = 0;
    for line in sums.lines() {
        let (hash, file) = line.split_once("  ").unwrap();
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(directory.join(file)).unwrap())
            ),
            hash
        );
        files += 1;
    }
    assert_eq!(files, 9);
    roundtrip::<contract::DescribeReply>(&vector("describe.json"))
        .check()
        .unwrap();
    for case in vector("previews.json").as_array().unwrap() {
        roundtrip::<contract::Preview>(&case["body"])
            .check()
            .unwrap();
    }
    let requests = vector("requests.json");
    roundtrip::<contract::DescribeRequest>(&requests["describe"]);
    roundtrip::<contract::ModulePreviewRequest>(&requests["module_preview"]);
    roundtrip::<contract::ModuleRunRequest>(&requests["module_run"]);
    let invalid = vector("invalid.json");
    for case in invalid.as_array().unwrap() {
        let error = match case["kind"].as_str().unwrap() {
            "command" => roundtrip::<contract::Command>(&case["body"])
                .check()
                .unwrap_err(),
            "preview" => roundtrip::<contract::Preview>(&case["body"])
                .check()
                .unwrap_err(),
            "run" => roundtrip::<contract::ModuleRunRequest>(&case["body"])
                .check(&status_command())
                .unwrap_err(),
            _ => unreachable!(),
        };
        assert_eq!(error.field, case["field"]);
        let refusal = error.refusal();
        let bytes = serde_json::to_vec(&refusal).unwrap();
        assert_eq!(
            serde_json::to_vec(&serde_json::from_slice::<ErrorBody>(&bytes).unwrap()).unwrap(),
            bytes
        );
    }
}

#[test]
fn user_commands_samples() {
    let describe = call(
        Some(&occipital()),
        &request("commands.describe"),
        |_| unreachable!(),
    )
    .unwrap();
    let not_bound = call(Some(&occipital()), &request("commands.preview"), |_| {
        Ok(None)
    })
    .unwrap();
    let refusal = call(
        Some(&Principal::Direct),
        &request("commands.preview"),
        |_| unreachable!(),
    )
    .unwrap_err();
    let samples = [
        ("describe.json", describe),
        ("ready.json", encode(&preview(&snapshot(false), Some(1)))),
        ("building.json", encode(&preview(&snapshot(true), Some(1)))),
        ("not-bound.json", not_bound),
        ("wrong-caller.json", encode(&refusal)),
    ];
    for (_, body) in &samples {
        assert!(body.is_object());
    }
    if let Ok(directory) = std::env::var("AFT_USER_COMMAND_SAMPLES_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        for (name, body) in samples {
            std::fs::write(
                std::path::Path::new(&directory).join(name),
                serde_json::to_vec_pretty(&body).unwrap(),
            )
            .unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn user_commands_real_management_routes_admit_only_occipital_and_keep_bodies_unwrapped() {
    async fn receive(daemon: &mut tokio::io::DuplexStream, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let frame = read_frame(daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("command route response timed out")
    }
    async fn exchange(
        daemon: &mut tokio::io::DuplexStream,
        ty: FrameType,
        channel: u16,
        corr: u64,
        body: Value,
    ) -> Frame {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        write_frame(daemon, &frame).await.unwrap();
        receive(daemon, channel, corr).await
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let app = App::default_shared();
    app.open_db(&storage.join("aft.db")).unwrap();
    let (mut daemon, module) = tokio::io::duplex(64 * 1024);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (read, write) = tokio::io::split(module);
        runtime.block_on(run_module_loop(
            read,
            write,
            &storage.join("absent-connection.json"),
            app,
            Arc::new(Executor::new()),
            |_, _| panic!("person command dispatched as an agent tool"),
            None,
            false,
            usize::MAX,
            None,
            &storage,
            QuiescedLsp::ReapInBackground,
            None,
            None,
            None,
        ))
    });
    assert_eq!(
        receive(&mut daemon, 0, HELLO_CORR).await.header.ty,
        FrameType::Hello
    );
    let ack = Frame::build(
        FrameType::HelloAck,
        control_flags(),
        0,
        0,
        HELLO_CORR,
        b"{}".to_vec(),
    )
    .unwrap();
    write_frame(&mut daemon, &ack).await.unwrap();
    for (channel, principal) in [(41, occipital()), (42, Principal::Direct)] {
        let bind = serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target: RouteTarget::ManagementSurface {
                module_id: "aft".into(),
            },
            identity: subc_protocol::BindIdentity::new("operator", format!("commands:{channel}")),
            principal: Some(principal),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        })
        .unwrap();
        assert_eq!(
            exchange(&mut daemon, FrameType::Request, 0, u64::from(channel), bind)
                .await
                .header
                .ty,
            FrameType::Response
        );
    }
    let describe = exchange(
        &mut daemon,
        FrameType::Request,
        41,
        1,
        request("commands.describe"),
    )
    .await;
    assert_eq!(describe.header.ty, FrameType::Response);
    let reply: contract::DescribeReply = serde_json::from_slice(&describe.body).unwrap();
    reply.check().unwrap();
    assert_eq!(reply.commands[0].id, "status");
    for op in contract::MODULE_OPS {
        let refused = exchange(
            &mut daemon,
            FrameType::Request,
            42,
            2,
            json!({"method":op,"params":"malformed"}),
        )
        .await;
        assert_eq!(refused.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<ErrorBody>(&refused.body)
                .unwrap()
                .code,
            "caller_not_admitted"
        );
    }
    let not_bound = exchange(
        &mut daemon,
        FrameType::Request,
        41,
        3,
        request("commands.preview"),
    )
    .await;
    assert_eq!(not_bound.header.ty, FrameType::Response);
    assert_eq!(
        serde_json::from_slice::<contract::Preview>(&not_bound.body)
            .unwrap()
            .reason
            .as_deref(),
        Some("not_bound")
    );
    let mut unknown = request("commands.preview");
    unknown["params"]["subject"]["session"]["runner"] = json!("opencode-plugin");
    let refused = exchange(&mut daemon, FrameType::Request, 41, 4, unknown).await;
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&refused.body)
            .unwrap()
            .code,
        "unknown_subject"
    );
    let run = exchange(
        &mut daemon,
        FrameType::Request,
        41,
        5,
        request("commands.run"),
    )
    .await;
    let error: ErrorBody = serde_json::from_slice(&run.body).unwrap();
    assert_eq!(error.code, "invalid_params");
    assert_eq!(error.detail.unwrap()["field"], "id");
    let unrelated = exchange(
        &mut daemon,
        FrameType::Request,
        41,
        6,
        json!({"op":"health.digest","params":{}}),
    )
    .await;
    assert_eq!(unrelated.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<ErrorBody>(&unrelated.body)
            .unwrap()
            .code,
        "route_not_admitted"
    );
    drop(daemon);
    thread.join().unwrap().unwrap();
}
