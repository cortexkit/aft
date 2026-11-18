use super::*;
use serde::de::DeserializeOwned;

macro_rules! golden {
    ($name:literal) => {
        include_str!(concat!(
            "../../../tests/fixtures/settings_wire/",
            $name,
            ".json"
        ))
    };
}

fn json_as<T: DeserializeOwned>(json: &str) -> T {
    serde_json::from_str(json).unwrap()
}

fn leaf(kind: &str, default: Value) -> Value {
    let mut leaf = json!({"type":kind,"description":"Fixture value","default":default,"x-ck-apply":"live","x-ck-layers":["user"]});
    if kind == "array" {
        leaf["items"] = json!({"type":"string","description":"Fixture string","default":""});
        leaf["x-ck-apply"] = json!("module_restart");
    }
    leaf
}

pub(super) fn sample_schema() -> Value {
    json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{
        "format_on_edit":leaf("boolean", json!(true)), "disabled_tools":leaf("array", json!([])),
        "formatter_timeout_secs":leaf("integer", json!(30)),
    }})
}

#[test]
fn settings_golden_wire_shapes() {
    let _: ConfigDeclareRequest = json_as(golden!("config_declare_request"));
    let _: ConfigDeclareReply = json_as(golden!("config_declare_reply"));
    let _: ConfigGetRequest = json_as(golden!("config_get_request"));
    let _: ConfigAppliedReply = json_as(golden!("config_applied_reply"));
    let _: ConfigReviewRequest = json_as(golden!("config_review_request"));
    let _: ConfigReviewReply = json_as(golden!("config_review_reply"));
    let changed: ConfigChanged = json_as(golden!("config_changed"));
    let (corr, generation) = (0, changed.generation);
    let hint = frame(FrameType::Push, corr, json_as(golden!("config_changed")));
    let mut no_project: Value = serde_json::from_slice(&hint.body).unwrap();
    no_project.as_object_mut().unwrap().remove("project_id");
    assert!(is_changed(&frame(FrameType::Push, 0, no_project.clone())));
    assert!(!is_changed(&frame(
        FrameType::Request,
        0,
        no_project.clone()
    )));
    assert!(!is_changed(&frame(FrameType::Push, 1, no_project)));
    assert!(generation > 0);

    let mut pending: ConfigGetReply = json_as(golden!("config_get_reply_declaration_pending"));
    let ack: ConfigAppliedRequest = json_as(golden!("config_applied_request_ack"));
    pending.active.as_mut().unwrap().attempt = Some(ack.attempt);
    let view = view_from_reply(pending).unwrap();
    assert_eq!(view.status, ConfigStatus::Active);
    assert!(view.declaration_pending.unwrap().plan_digest.is_some());
    assert_eq!(view.attempt, Some(ack.attempt));
    assert_eq!(view.effective["limit"], 10);
}

#[tokio::test]
async fn settings_review_names_invalid_field_without_echoing_values() {
    let schema = Arc::new(Schema::new(sample_schema()).unwrap());
    let candidate = ConfigLayerSnapshot::file(
        "aft",
        ConfigLayer::User,
        "/config/aft.jsonc",
        Some(br#"{"formatter_timeout_secs":601}"#.to_vec()),
    );
    let request = ConfigReviewRequest::new(0, vec![candidate], None);
    let reply = review::response(&schema, &frame(FrameType::Request, 8, json!(request)))
        .await
        .unwrap()
        .unwrap();
    let response: ConfigReviewReply = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(reply.header.corr, 8);
    assert!(response.fields.is_empty());
    assert_eq!(response.refusals[0].paths, ["/formatter_timeout_secs"]);
    assert!(!String::from_utf8_lossy(&reply.body).contains("601"));
}

#[tokio::test]
async fn settings_review_classifies_live_and_restart_fields() {
    let schema = Arc::new(Schema::new(sample_schema()).unwrap());
    let candidate = ConfigLayerSnapshot::file(
        "aft",
        ConfigLayer::User,
        "/config/aft.jsonc",
        Some(br#"{"format_on_edit":false,"disabled_tools":["search"]}"#.to_vec()),
    );
    let request = ConfigReviewRequest::new(0, vec![candidate], None);
    let reply = review::response(&schema, &frame(FrameType::Request, 8, json!(request)))
        .await
        .unwrap()
        .unwrap();
    let response: ConfigReviewReply = serde_json::from_slice(&reply.body).unwrap();
    assert!(response.refusals.is_empty());
    for (path, kind) in [
        ("/disabled_tools", ApplyKind::ModuleRestart),
        ("/format_on_edit", ApplyKind::Live),
    ] {
        assert_eq!(
            response
                .fields
                .iter()
                .find(|field| field.path == path)
                .unwrap()
                .apply,
            kind
        );
    }
}

fn frame(kind: FrameType, corr: u64, body: Value) -> Frame {
    Frame::build(
        kind,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn incoming(tx: &mpsc::Sender<Result<DecodedFrame, SubcError>>, frame: Frame) {
    tx.send(Ok(DecodedFrame {
        frame,
        phase_trace: PhaseTrace::new(Instant::now()),
        parsed_body: Default::default(),
        confinement: None,
        codemode_policies: None,
        launch_restore: None,
    }))
    .await
    .unwrap();
}

async fn next(rx: &mut mpsc::Receiver<WriterFrame>) -> (u64, Value) {
    let frame = rx.recv().await.unwrap();
    (
        frame.header.corr,
        serde_json::from_slice(&frame.body).unwrap(),
    )
}

#[tokio::test]
async fn settings_no_approved_view_never_admits_a_tool_before_ready() {
    tokio::time::timeout(Duration::from_secs(15), exercise_bootstrap())
        .await
        .unwrap();
}

async fn exercise_bootstrap() {
    let (writer, mut written) = mpsc::channel(16);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
    let replies = readiness::PendingControlReplies::default();
    let metrics = Arc::new(DispatchPathMetrics::new());
    let control = readiness::DaemonControl::new(
        writer.clone(),
        metrics.clone(),
        PROTOCOL_VERSION,
        vec![subc_protocol::session::MODULE_TO_SUBC_OP_CATALOG_UPDATE.into()],
        build_manifest_without(&[]).provides,
        replies.clone(),
        2,
    );
    let temp = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let ctx = Arc::new(AppContext::from_app(
        App::default_shared(),
        crate::config::Config {
            storage_dir: Some(temp.path().join("storage")),
            ..Default::default()
        },
    ));
    let file = temp.path().join("aft.jsonc");
    std::fs::write(&file, r#"{"format_on_edit":true,"disabled_tools":[]}"#).unwrap();
    let mut schema = sample_schema();
    schema["properties"]["storage"] = json!({"type":"object","unevaluatedProperties":false,"properties":{"cache_dir":leaf("string", json!(cache.path()))}});
    let declaration = ConfigDeclareRequest::new(schema.clone(), file.to_str().unwrap(), vec![]);
    let (boot, ()) = tokio::join!(
        startup::drive_first_view(
            &control,
            &ctx,
            Some((declaration, Schema::new(schema).unwrap())),
            &mut incoming_rx,
            &replies,
            &writer,
            &metrics
        ),
        async {
            let (corr, declared) = next(&mut written).await;
            assert_eq!(declared["op"], "config.declare");
            assert_eq!(declared["project_dirs"], json!([]));
            incoming(
                &incoming_tx,
                frame(
                    FrameType::Response,
                    corr,
                    json_as(golden!("config_declare_reply")),
                ),
            )
            .await;
            let (corr, get) = next(&mut written).await;
            assert_eq!(get, json_as::<Value>(golden!("config_get_request")));
            incoming(
                &incoming_tx,
                frame(
                    FrameType::Error,
                    corr,
                    json_as(golden!("config_get_error_gated")),
                ),
            )
            .await;

            let rejected = Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                7,
                1,
                40,
                serde_json::to_vec(&json!({"op":"tool.call","name":"search"})).unwrap(),
            )
            .unwrap();
            incoming(&incoming_tx, rejected).await;
            let rejected = written.recv().await.unwrap();
            assert_eq!(rejected.header.ty, FrameType::Error);
            assert_eq!(rejected.header.channel, 7);
            assert_eq!(
                serde_json::from_slice::<Value>(&rejected.body).unwrap()["code"],
                "module_warming"
            );
            incoming(
                &incoming_tx,
                frame(FrameType::Request, 41, json!({"op":"health.check"})),
            )
            .await;
            let (_, health) = next(&mut written).await;
            assert_eq!(health["detail"], WAITING_HEALTH);
            assert_eq!(
                health["metrics"]["settings"]["reason"],
                "no_approved_config"
            );
            assert!(ctx.config_unpinned().cache_dir.is_none());

            let mut changed = json_as::<Value>(golden!("config_changed"));
            changed.as_object_mut().unwrap().remove("project_id");
            incoming(&incoming_tx, frame(FrameType::Push, 0, changed)).await;
            let (corr, _) = next(&mut written).await;
            let mut active = json_as::<Value>(golden!("config_get_reply_declaration_pending"));
            active["active"]["effective"] = json!({"format_on_edit":false,"disabled_tools":["aft_search"],"storage":{"cache_dir":cache.path()}});
            active["active"]["sources"] = json!({"/format_on_edit":"user","/disabled_tools":"user","/storage/cache_dir":"user"});
            active["active"]["layers"] = json!([]);
            active["active"]["attempt"] = json!(39);
            incoming(&incoming_tx, frame(FrameType::Response, corr, active)).await;
            let (corr, catalog) = next(&mut written).await;
            assert_eq!(catalog["op"], "catalog.update");
            assert_eq!(catalog["ready"], false);
            assert!(!catalog["provides"].to_string().contains("\"aft_search\""));
            assert!(!ctx.config_unpinned().format_on_edit);
            assert_eq!(
                ctx.config_unpinned().cache_dir.as_deref(),
                Some(cache.path())
            );
            incoming(
                &incoming_tx,
                frame(FrameType::Response, corr, json!({"op":"catalog.update"})),
            )
            .await;
            let (corr, report) = next(&mut written).await;
            let mut expected: Value = json_as(golden!("config_applied_request_ack"));
            expected.as_object_mut().unwrap().remove("project_id");
            expected["generation"] = json!(3);
            expected["attempt"] = json!(39);
            assert_eq!(report, expected);
            incoming(
                &incoming_tx,
                frame(
                    FrameType::Response,
                    corr,
                    json_as(golden!("config_applied_reply")),
                ),
            )
            .await;
        },
    );
    let session = boot.unwrap().ok().unwrap();
    assert!(session.pending());
    let mut report = subc_protocol::session::HealthReport::ok();
    note_health(&mut report, Some(&session));
    assert_eq!(report.detail.as_deref(), Some(PENDING_HEALTH));
}

#[tokio::test]
async fn settings_changed_fetches_and_applies_pending_generation_before_ack() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let (writer, mut written) = mpsc::channel(16);
        let replies = readiness::PendingControlReplies::default();
        let metrics = Arc::new(DispatchPathMetrics::new());
        let control = Arc::new(readiness::DaemonControl::new(
            writer.clone(),
            metrics.clone(),
            PROTOCOL_VERSION,
            vec![],
            vec![],
            replies.clone(),
            2,
        ));
        let old = ConfigView::new(
            3,
            json!({"format_on_edit":true}),
            Default::default(),
            vec![],
            ConfigStatus::Active,
        );
        let tier = validate_user(&old.effective).unwrap();
        let session = Arc::new(Session::new(
            Arc::new(Schema::new(sample_schema()).unwrap()),
            tier,
            old,
        ));
        session.mark_ready();
        let task = {
            let session = session.clone();
            tokio::spawn(async move { session.run(control, Arc::new(Executor::new())).await })
        };
        let mut changed = json_as::<Value>(golden!("config_changed"));
        changed.as_object_mut().unwrap().remove("project_id");
        assert!(session
            .intercept(&frame(FrameType::Push, 0, changed), &writer, &metrics)
            .await
            .unwrap());
        let (corr, get) = next(&mut written).await;
        assert_eq!(get, json_as::<Value>(golden!("config_get_request")));
        let mut first = json_as::<Value>(golden!("config_get_reply_first_delivery"));
        first["pending"]["effective"] = json!({"format_on_edit":false});
        first["pending"]["sources"] = json!({"/format_on_edit":"user"});
        first["pending"]["layers"] = json!([]);
        first["pending"]["generation"] = json!(4);
        first["pending"]["attempt"] = json!(8);
        assert!(replies.resolve(frame(FrameType::Response, corr, first)));
        let (corr, report) = next(&mut written).await;
        assert_eq!(
            report,
            json_as::<Value>(golden!("config_applied_request_ack"))
        );
        assert_eq!(session.user.snapshot().doc, r#"{"format_on_edit":false}"#);
        assert!(replies.resolve(frame(
            FrameType::Response,
            corr,
            json_as(golden!("config_applied_reply"))
        )));
        task.abort();
        let _ = task.await;
    })
    .await
    .unwrap();
}

#[test]
fn settings_standalone_observation_still_reads_file_and_project() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    std::fs::create_dir_all(root.join(".cortexkit")).unwrap();
    let file = directory.path().join("aft.jsonc");
    std::fs::write(
        &file,
        "{ // existing JSONC comment\n\"format_on_edit\": false,\n}",
    )
    .unwrap();
    let project = root.join(".cortexkit/aft.jsonc");
    std::fs::write(project, r#"{"restrict_to_project_root":true}"#).unwrap();
    let snapshot = crate::subc_config::PreparedBindConfig::read(
        Some(&file),
        &root,
        &crate::harness::Harness::Pi,
    )
    .unwrap();
    assert!(!snapshot.resolved().format_on_edit);
    assert!(snapshot.resolved().restrict_to_project_root);
    let approved = ApprovedUser::new(validate_user(&json!({"format_on_edit":true})).unwrap());
    let snapshot = crate::subc_config::PreparedBindConfig::read_with_approved(
        Some(&file),
        Some(&approved),
        &root,
        &crate::harness::Harness::Pi,
    )
    .unwrap();
    assert!(snapshot.resolved().format_on_edit);
    assert!(snapshot.resolved().restrict_to_project_root);
}
