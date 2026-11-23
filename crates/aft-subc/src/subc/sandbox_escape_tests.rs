use super::super::sandbox_escape as escape;
use super::*;
use crate::config::SandboxMode;
use crate::db::sandbox_escape_records as records;
use aft_engine::sandbox_escape::EscapePlan;
use std::sync::atomic::AtomicU64;

static CORR: AtomicU64 = AtomicU64::new(20_000);

fn head(f: &Fixture) -> RouteIdentity {
    let root = f.ctx.config().project_root.clone().unwrap();
    let mut stamp = stamp("escape-head", "head", None);
    stamp.attributes.agent_id = Some("agent-escape".into());
    let identity = identity(&root, Some(stamp));
    access_grants::register(&identity, &f.ctx).unwrap();
    identity
}

fn request_plan(request_id: &str, command: &str, cwd: &Path) -> (Response, Value) {
    let arguments = json!({
        "command": command,
        "escape_sandbox": "Read this file with the person's login.",
    });
    let response = Response::error_with_data(
        request_id,
        aft_engine::sandbox_escape::REQUIRED_CODE,
        aft_engine::sandbox_escape::APPROVAL_REQUIRED,
        json!({"sandbox_escape": EscapePlan {
            grant_id: "esc_test".into(),
            command: command.into(),
            cwd: cwd.into(),
            shell: "/bin/sh".into(),
            escapes_level: SandboxMode::Strict,
            background: false,
            reason: "Read this file with the person's login.".into(),
        }}),
    );
    (response, arguments)
}

fn held_call(f: &Fixture, identity: &RouteIdentity, arguments: Value) -> HeldCall {
    let corr = CORR.fetch_add(1, Ordering::SeqCst);
    HeldCall {
        reply_deadline: Instant::now() + Duration::from_secs(625),
        identity: identity.clone(),
        tool: "bash".into(),
        arguments,
        request_id: format!("escape-{corr}"),
        executor: f.executor.clone(),
        route: RouteChannel {
            channel: 201,
            epoch: 1,
        },
        corr,
        active: f.active.clone(),
        connection_cancel: f.connection_cancel.clone(),
        drain: drain::ModuleDrainWindow::default(),
        ledger_key: None,
        shell_claim: None,
    }
}

async fn wait_for_request(fake: &FakeCingulate) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = fake.calls("consent.request").into_iter().next() {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sandbox escape card was filed")
}

fn answer(request: &Value, state: &str, choice: Option<&str>) -> Value {
    let mut answer = json!({
        "kind": "sandbox_escape",
        "state": state,
        "sandbox_escape": request["sandbox_escape"].clone(),
    });
    if let Some(choice) = choice {
        answer["answered_choice_id"] = json!(choice);
    }
    answer
}

#[derive(Clone, Copy)]
enum Behavior {
    Run,
    Refuse,
    Unknown,
}

struct FlowOptions<'a> {
    state: &'a str,
    choice: Option<&'a str>,
    behavior: Behavior,
    missing_cwd: bool,
    end_program: bool,
    refuse_report: bool,
}

impl Default for FlowOptions<'_> {
    fn default() -> Self {
        Self {
            state: "answered",
            choice: Some("run_once"),
            behavior: Behavior::Run,
            missing_cwd: false,
            end_program: false,
            refuse_report: false,
        }
    }
}

async fn flow(options: FlowOptions<'_>) -> (Option<Response>, Arc<FakeCingulate>, Fixture) {
    let f = fixture(FakeCingulate::up());
    if options.refuse_report {
        f.fake.state.lock().unwrap().report_refusal = Some("elicitation_not_owner".into());
    }
    let root = f.ctx.config().project_root.clone().unwrap();
    let cwd = if options.missing_cwd {
        f.dir.path().join("gone")
    } else {
        root.clone()
    };
    let marker = f.dir.path().join(format!("marker-{}", options.state));
    let command = format!("printf x >> {}", marker.display());
    let (first, arguments) = request_plan("escape-outcome", &command, &cwd);
    let identity = if options.end_program {
        let identity = program_approvals::program(&f, false, "run:escape");
        f.executor
            .register_actor(identity.root.clone(), f.ctx.clone());
        identity
    } else {
        head(&f)
    };
    let call = held_call(&f, &identity, arguments);
    let request_id = call.request_id.clone();
    let fake = f.fake.clone();
    let behavior = options.behavior;
    let mut running = tokio::spawn(async move {
        run_with_consent(call, (first, String::new()), move |_route, grants| {
            assert_eq!(grants, ["esc_test"]);
            let result = match behavior {
                Behavior::Run => {
                    let output = std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg(&command)
                        .current_dir(&root)
                        .status()
                        .unwrap();
                    assert!(output.success());
                    Some((
                        Response::success(
                            &request_id,
                            json!({"status":"completed", "exit_code":0}),
                        ),
                        String::new(),
                    ))
                }
                Behavior::Refuse => Some((
                    Response::error(
                        &request_id,
                        aft_engine::sandbox_escape::REFUSED_CODE,
                        aft_engine::sandbox_escape::UNAVAILABLE,
                    ),
                    String::new(),
                )),
                Behavior::Unknown => None,
            };
            async move { result }
        })
        .await
    });
    let request = tokio::select! {
        request = wait_for_request(&fake) => request,
        done = &mut running => panic!("sandbox escape ended before filing: {done:?}"),
    };
    if options.end_program {
        identity.run_scope.get().unwrap().end_for_test();
    }
    fake.answer(
        request["sandbox_escape"]["call_key"].as_str().unwrap(),
        answer(&request, options.state, options.choice),
    );
    let response = tokio::time::timeout(Duration::from_secs(5), &mut running)
        .await
        .expect("settled sandbox escape call")
        .unwrap()
        .map(|result| result.0);
    let should_run = matches!(behavior, Behavior::Run)
        && options.state == "answered"
        && options.choice == Some("run_once")
        && !options.end_program;
    assert_eq!(marker.exists(), should_run);
    (response, fake, f)
}

async fn outcome_case(
    state: &str,
    choice: Option<&str>,
) -> (Option<Response>, Arc<FakeCingulate>, Fixture) {
    flow(FlowOptions {
        state,
        choice,
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn sandbox_escape_run_once_executes_once_and_reports_without_output() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (response, fake, f) = outcome_case("answered", Some("run_once")).await;
    let response = response.unwrap();
    assert!(response.success, "{response:?}");
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("marker-answered")).unwrap(),
        "x"
    );
    let requests = fake.calls("consent.request");
    assert_eq!(requests.len(), 1);
    let fields = &requests[0]["sandbox_escape"];
    assert_eq!(fields["expires_in_ms"], 600_000);
    assert_eq!(fields["escapes_level"], "strict");
    let reports = fake.calls("consent.report_execution");
    assert_eq!(
        reports,
        [json!({"elicitation_id":"el_1", "execution":"executed"})]
    );
    let call_key = requests[0]["sandbox_escape"]["call_key"].as_str().unwrap();
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let row = records::get(&conn, call_key).unwrap().unwrap();
    assert_eq!(row.agent_id.as_deref(), Some("agent-escape"));
    assert_eq!(row.decision, "run_once");
    assert_eq!(row.exit_status.as_deref(), Some("completed"));
    assert_eq!(row.exit_code, Some(0));
    let columns = conn
        .prepare("PRAGMA table_info(sandbox_escape_records)")
        .unwrap()
        .query_map([], |column| column.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!columns.iter().any(|name| name.contains("output")));
}

#[tokio::test]
async fn sandbox_escape_approved_dispatch_outcomes_use_execution_report_contract() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for (missing_cwd, reason) in [(false, "refused_at_dispatch"), (true, "target_gone")] {
        let (response, fake, _f) = flow(FlowOptions {
            behavior: Behavior::Refuse,
            missing_cwd,
            ..Default::default()
        })
        .await;
        assert!(!response.unwrap().success);
        assert_eq!(
            fake.calls("consent.report_execution"),
            [
                json!({"elicitation_id":"el_1", "execution":"approved_not_executed", "reason":reason})
            ]
        );
    }

    let (response, fake, _f) = flow(FlowOptions {
        behavior: Behavior::Unknown,
        ..Default::default()
    })
    .await;
    assert!(response.is_none());
    assert_eq!(
        fake.calls("consent.report_execution"),
        [json!({"elicitation_id":"el_1", "execution":"outcome_unknown"})]
    );

    let (response, fake, _f) = flow(FlowOptions {
        behavior: Behavior::Refuse,
        end_program: true,
        ..Default::default()
    })
    .await;
    assert!(!response.unwrap().success);
    assert_eq!(
        fake.calls("consent.report_execution"),
        [
            json!({"elicitation_id":"el_1", "execution":"approved_not_executed", "reason":"session_ended"})
        ]
    );
}

#[tokio::test]
async fn sandbox_escape_pruned_execution_report_is_not_retried() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (response, fake, _f) = flow(FlowOptions {
        refuse_report: true,
        ..Default::default()
    })
    .await;
    assert!(response.unwrap().success);
    assert_eq!(fake.calls("consent.report_execution").len(), 1);
}

#[tokio::test]
async fn sandbox_escape_without_a_channel_runs_nothing() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    install_test_transport(None);
    let root = f.ctx.config().project_root.clone().unwrap();
    let (first, arguments) = request_plan("escape-no-channel", "printf never", &root);
    let call = held_call(&f, &head(&f), arguments);
    let response = run_with_consent(call, (first, String::new()), |_route, _grants| async {
        None
    })
    .await
    .unwrap()
    .0;
    assert_eq!(
        response.data["code"],
        aft_engine::sandbox_escape::UNAVAILABLE_CODE
    );
    assert_eq!(
        response.data["message"],
        aft_engine::sandbox_escape::UNAVAILABLE
    );
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.fake.calls("consent.report_execution").is_empty());
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let decision: String = conn
        .query_row("SELECT decision FROM sandbox_escape_records", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(decision, "unavailable");
}

#[tokio::test]
async fn sandbox_escape_decline_and_expiry_run_nothing_and_send_no_report() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for (state, choice, code, message) in [
        (
            "answered",
            Some("decline"),
            aft_engine::sandbox_escape::DECLINED_CODE,
            aft_engine::sandbox_escape::DECLINED,
        ),
        (
            "expired",
            None,
            aft_engine::sandbox_escape::EXPIRED_CODE,
            aft_engine::sandbox_escape::EXPIRED,
        ),
        (
            "withdrawn",
            None,
            aft_engine::sandbox_escape::WITHDRAWN_CODE,
            aft_engine::sandbox_escape::WITHDRAWN,
        ),
    ] {
        let (response, fake, _f) = outcome_case(state, choice).await;
        let response = response.unwrap();
        assert!(!response.success);
        assert_eq!(response.data["code"], code);
        assert_eq!(response.data["message"], message);
        assert!(fake.calls("consent.report_execution").is_empty());
    }
}

#[tokio::test]
async fn sandbox_escape_inapplicable_calls_are_refused_before_filing() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let head = head(&f);
    let long = "x".repeat(2_001);
    let cases = [
        (
            &f.worker,
            json!({"command":"pwd", "escape_sandbox":"worker request"}),
            aft_engine::sandbox_escape::HEAD_ONLY,
        ),
        (
            &head,
            json!({"command":"pwd", "runon":"linux", "escape_sandbox":"remote request"}),
            aft_engine::sandbox_escape::LOCAL_ONLY,
        ),
        (
            &head,
            json!({"command":long, "escape_sandbox":"long command"}),
            aft_engine::sandbox_escape::COMMAND_INVALID,
        ),
        (
            &head,
            json!({"command":"pwd", "escape_sandbox":"two\nlines"}),
            aft_engine::sandbox_escape::REASON_INVALID,
        ),
    ];
    for (identity, arguments, message) in cases {
        let response = escape::preflight_response(identity, "preflight", &arguments).unwrap();
        assert_eq!(
            response.data["code"],
            aft_engine::sandbox_escape::REFUSED_CODE
        );
        assert_eq!(response.data["message"], message);
    }
    assert!(f.fake.calls("consent.request").is_empty());
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn assert_roundtrip<T>(value: &Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_value(value.clone()).unwrap();
    let encoded = serde_json::to_value(decoded).unwrap();
    assert_eq!(&encoded, value);
    assert_eq!(
        serde_json::to_vec(&canonical(&encoded)).unwrap(),
        serde_json::to_vec(&canonical(value)).unwrap()
    );
}

#[test]
fn sandbox_escape_release_vectors_roundtrip_through_request_and_answer_types() {
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/sandbox-escape-v1/cases.json")).unwrap();
    let valid = [
        "file-worker",
        "replay",
        "wrong-filer",
        "missing-agent-stamp",
    ];
    let invalid = [
        "unknown-key",
        "long-command",
        "control-character",
        "text-direction",
        "relative-cwd",
        "long-expiry",
    ];
    let mut requests = 0;
    let mut answers = 0;
    for (name, case) in cases.as_object().unwrap() {
        if case["method"] == "consent.request" {
            requests += 1;
            let params = &case["params"];
            let decoded = serde_json::from_value::<escape::CardRequest>(params.clone());
            if valid.contains(&name.as_str()) {
                let decoded = decoded.unwrap();
                decoded.sandbox_escape.validate().unwrap();
                assert_roundtrip::<escape::CardRequest>(params);
            } else if invalid.contains(&name.as_str()) {
                let invalid_request = match decoded {
                    Err(_) => true,
                    Ok(request) => request.sandbox_escape.validate().is_err(),
                };
                assert!(invalid_request, "{name} must be invalid");
            } else {
                panic!("unreviewed sandbox escape request vector: {name}");
            }
        }
        let result = &case["reply"]["result"];
        let record = if result["kind"] == "sandbox_escape" {
            Some(result)
        } else if result["elicitation"]["kind"] == "sandbox_escape" {
            Some(&result["elicitation"])
        } else {
            None
        };
        if let Some(record) = record {
            answers += 1;
            assert_roundtrip::<escape::AnswerRecord>(record);
        }
    }
    assert_eq!(requests, 10);
    assert_eq!(answers, 7);
}

#[test]
fn sandbox_escape_development_vectors_roundtrip_through_answer_types() {
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/sandbox-escape-v1/development.json")).unwrap();
    assert_eq!(cases.as_object().unwrap().len(), 2);
    for case in cases.as_object().unwrap().values() {
        let result = &case["reply"]["result"];
        let record = if result["kind"] == "sandbox_escape" {
            result
        } else {
            &result["elicitation"]
        };
        assert_roundtrip::<escape::AnswerRecord>(record);
    }
}

#[cfg(unix)]
fn actual_bash(request: RawRequest, identity: &RouteIdentity, ctx: &AppContext) -> Response {
    crate::sandbox_spawn::with_authenticated_principal(identity.spawn_principal.clone(), || {
        crate::sandbox_spawn::with_worker_jail(false, || {
            crate::commands::bash::handle(&request, ctx)
        })
    })
}

#[cfg(unix)]
fn bash_request(identity: &RouteIdentity, id: &str, params: Value) -> RawRequest {
    RawRequest {
        id: id.into(),
        command: "bash".into(),
        lsp_hints: None,
        session_id: Some(identity.session.clone()),
        params,
    }
}

#[cfg(unix)]
async fn wait_task(
    f: &Fixture,
    identity: &RouteIdentity,
    response: &Response,
) -> crate::bash_background::registry::BgTaskSnapshot {
    let task_id = response.data["task_id"].as_str().expect("task id");
    let project = f.ctx.config().project_root.clone();
    let storage = f.ctx.config().storage_dir.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(snapshot) = f.ctx.bash_background().status(
                task_id,
                &identity.session,
                project.as_deref(),
                storage.as_deref(),
                8192,
            ) {
                if snapshot.info.status.is_terminal() {
                    return snapshot;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bash task reached a terminal state")
}

#[cfg(unix)]
#[tokio::test]
async fn sandbox_escape_off_level_runs_without_a_card() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.ctx.update_config(|config| {
        config.sandbox.profile.head = SandboxMode::Off;
        config.experimental_bash_background = true;
    });
    let identity = head(&f);
    let marker = f.dir.path().join("off-marker");
    let response = actual_bash(
        bash_request(
            &identity,
            "off-escape",
            json!({
                "command": format!("printf off > {}", marker.display()),
                "escape_sandbox": "No sandbox is active.",
                "background": true,
            }),
        ),
        &identity,
        &f.ctx,
    );
    assert!(response.success, "{response:?}");
    let terminal = wait_task(&f, &identity, &response).await;
    assert_eq!(terminal.exit_code, Some(0));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "off");
    assert!(f.fake.calls("consent.request").is_empty());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn sandbox_escape_run_once_reads_a_blocked_path_at_off() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let identity = head(&f);
    let root = f.ctx.config().project_root.clone().unwrap();
    let blocked = f.dir.path().join("blocked.txt");
    let marker = root.join("escaped-marker");
    let count = root.join("escaped-count");
    std::fs::write(&blocked, "blocked-fixture").unwrap();
    f.ctx.update_config(|config| {
        config.sandbox.profile.head = SandboxMode::Strict;
        config.sandbox.read_deny = vec![blocked.clone()];
        config.experimental_bash_background = true;
    });
    f.ctx.bash_background().set_db_pool(f.ctx.db().unwrap());

    let denied_marker = root.join("denied-marker");
    let denied = actual_bash(
        bash_request(
            &identity,
            "ordinary-confined-read",
            json!({
                "command": format!("/bin/cat {} > {}", blocked.display(), denied_marker.display()),
                "background": true,
            }),
        ),
        &identity,
        &f.ctx,
    );
    let denied = wait_task(&f, &identity, &denied).await;
    assert_eq!(denied.info.status, crate::protocol::BgTaskStatus::Failed);
    assert!(!denied_marker.exists());

    let command = format!(
        "/bin/cat {} > {} && printf x >> {}",
        blocked.display(),
        marker.display(),
        count.display()
    );
    let arguments = json!({
        "command": command,
        "escape_sandbox": "Read the blocked fixture once.",
        "background": true,
    });
    let first = actual_bash(
        bash_request(&identity, "approved-off-read", arguments.clone()),
        &identity,
        &f.ctx,
    );
    assert_eq!(
        first.data["code"],
        aft_engine::sandbox_escape::REQUIRED_CODE
    );
    let call = held_call(&f, &identity, arguments.clone());
    let fake = f.fake.clone();
    let principal_identity = identity.clone();
    let ctx = f.ctx.clone();
    let running = tokio::spawn(async move {
        run_with_consent(call, (first, String::new()), move |_route, grants| {
            let mut params = arguments.clone();
            params["permissions_granted"] = json!(grants);
            let response = actual_bash(
                bash_request(&principal_identity, "approved-off-read-rerun", params),
                &principal_identity,
                &ctx,
            );
            async move { Some((response, String::new())) }
        })
        .await
    });
    let request = wait_for_request(&fake).await;
    assert_eq!(request["sandbox_escape"]["command"], command);
    fake.answer(
        request["sandbox_escape"]["call_key"].as_str().unwrap(),
        answer(&request, "answered", Some("run_once")),
    );
    let response = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .0;
    assert!(response.success, "{response:?}");
    let terminal = wait_task(&f, &identity, &response).await;
    assert_eq!(terminal.exit_code, Some(0), "{terminal:?}");
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "blocked-fixture");
    assert_eq!(std::fs::read_to_string(count).unwrap(), "x");
    assert_eq!(
        fake.calls("consent.report_execution"),
        [json!({"elicitation_id":"el_1", "execution":"executed"})]
    );
}
