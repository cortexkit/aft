//! Bash permission grants that a caller writes into its own tool arguments.
//!
//! A grant is not a secret: it is the permission pattern the command scanner
//! derives from the command text (`touch *` for `touch file`), so a model can
//! predict it, or copy it from the refusal its first call received. These
//! tests drive the real subc route handler and pin down which routes accept
//! grants in the arguments:
//!
//! - a tool-provider v1 route relays the model's own arguments, and its
//!   admission refuses any argument the served schema does not list, so a
//!   forged grant never reaches the bash permission loop;
//! - an untrusted route asks the consumer through an elicitation whatever the
//!   arguments carry;
//! - a legacy first-party route is how AFT's own OpenCode and Pi plugins call
//!   bash. They build the arguments themselves and add grants only after their
//!   own prompt was approved, so that route honours the grants.

use super::*;

const SESSION: &str = "bash-grant-forgery";
const COMMAND: &str = "touch forged-proof";

#[test]
fn bash_permissions_unknown_cwd_forwards_rm_rule_resource() {
    let project = tempfile::tempdir().unwrap();
    let command = "cd \"$DEST\" && rm -rf build";
    let plan =
        bash::prepare_bash_elicitation_plan(&json!({"command": command}), project.path()).unwrap();
    let body = bash_elicitation_request_body(command, &plan.asks);
    let asks = body["params"]["_meta"]["aft"]["asks"].as_array().unwrap();
    assert!(
        asks.iter().any(|ask| ask["kind"] == "bash"
            && ask["patterns"] == json!(["rm -rf build"])
            && ask["always"] == json!(["rm *"])),
        "delegated consumers must receive the individual rm command resource: {asks:?}"
    );
    assert!(asks.iter().any(|ask| ask["patterns"] == json!([command])
        && ask["reason"] == crate::bash_permissions::scan::UNKNOWN_CWD_PERMISSION_REASON));
    assert_eq!(plan.grants, ["rm *", command]);
}

#[test]
fn bash_permissions_unknown_cwd_reason_in_consent() {
    let project = tempfile::tempdir().unwrap();
    let command = "cd \"$DEST\" && shasum -c checksums";
    let asks = crate::bash_permissions::scan::scan_with_project_root(
        command,
        project.path(),
        project.path(),
    );
    let body = bash_elicitation_request_body(command, &asks);
    let reason = crate::bash_permissions::scan::UNKNOWN_CWD_PERMISSION_REASON;
    assert!(body["params"]["message"].as_str().unwrap().contains(reason));
    let forwarded = body["params"]["_meta"]["aft"]["asks"].as_array().unwrap();
    let exact = forwarded
        .iter()
        .find(|ask| ask["reason"] == reason)
        .unwrap();
    assert_eq!(exact["patterns"], json!([command]));
    assert_eq!(exact["always"], json!([]));
}

fn grant_test_context(root: &ProjectRootId, storage: &Path) -> Arc<AppContext> {
    Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.as_path().into()),
            storage_dir: Some(storage.into()),
            // Every command is scanned and asked about, as with a user who
            // turned bash permission prompts on.
            bash_permissions: true,
            // A rewritten command would run as a file tool and skip the scan.
            experimental_bash_rewrite: false,
            sandbox: crate::config::SandboxConfig {
                profile: crate::config::SandboxRoles {
                    head: crate::config::SandboxMode::Off,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
    ))
}

fn grant_test_route(
    root: &ProjectRootId,
    role: tool_provider::RouteRole,
    trust: BindTrust,
) -> HashMap<RouteChannel, RouteIdentity> {
    let identity = RouteIdentity(Arc::new(RouteIdentityData {
        root: root.clone(),
        project_root: root.as_path().into(),
        harness: "runner".into(),
        session: SESSION.into(),
        role,
        trust,
        spawn_principal: AuthenticatedPrincipal::FirstParty,
        consumer_elicitation_capable: trust == BindTrust::Untrusted,
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        run_owner: None,
        run_scope: OnceLock::new(),
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        scope: None,
        made_tool_call: AtomicBool::new(false),
    }));
    HashMap::from([(route_key(41, 1), identity)])
}

/// What one tool call produced: the bash result, or the first frame the route
/// wrote back on its own (an admission refusal or an elicitation request).
#[derive(Debug)]
enum Answer {
    Bash(Value),
    Frame(FrameType, Value),
}

impl Answer {
    fn bash(self) -> Value {
        match self {
            Self::Bash(value) => value,
            other => panic!("expected a bash result, got {other:?}"),
        }
    }
}

/// Sends one bash tool call through the route handler and returns its answer.
async fn call_bash(
    executor: &Arc<Executor>,
    routes: &HashMap<RouteChannel, RouteIdentity>,
    corr: u64,
    arguments: Value,
) -> Answer {
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        41,
        1,
        corr,
        serde_json::to_vec(&json!({"name": "bash", "arguments": arguments})).unwrap(),
    )
    .unwrap();
    let (writer, mut replies) = mpsc::channel(8);
    let (bash_tx, mut bash_rx) = mpsc::channel(8);
    let (touch_tx, _touch_rx) = mpsc::channel(8);
    let (deferred_tx, _deferred_rx) = mpsc::unbounded_channel();
    let deferred_tx = super::DeferredResponseSender {
        entries: deferred_tx,
        wake: crate::response_finalize::DeferredResponseWake::default(),
    };
    handle_tool_call(
        &writer,
        &frame,
        None,
        None,
        PhaseTrace::new(Instant::now()),
        routes,
        &HashMap::new(),
        &ReclaimedRoutes::default(),
        &mut HashMap::new(),
        executor,
        &Arc::default(),
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
        |request, ctx| crate::commands::bash::handle(&request, ctx),
        &deferred_tx,
        false,
        1024 * 1024,
        &drain::ModuleDrainWindow::default(),
        None,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            done = bash_rx.recv() => {
                Answer::Bash(serde_json::to_value(done.unwrap().response_for_test()).unwrap())
            }
            written = replies.recv() => {
                let written: WriterFrame = written.unwrap();
                Answer::Frame(
                    written.frame.header.ty,
                    serde_json::from_slice(&written.frame.body).unwrap(),
                )
            }
        }
    })
    .await
    .expect("the call must answer")
}

/// Every grant string a refusal names, exactly what a caller would copy.
fn grants_named_by(refusal: &Value) -> Vec<String> {
    refusal["asks"]
        .as_array()
        .expect("a permission refusal lists its asks")
        .iter()
        .flat_map(|ask| {
            ["patterns", "always"].into_iter().flat_map(move |key| {
                ask[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str().map(str::to_owned))
            })
        })
        .collect()
}

struct GrantCase {
    _dir: tempfile::TempDir,
    proof: PathBuf,
    executor: Arc<Executor>,
    routes: HashMap<RouteChannel, RouteIdentity>,
}

fn grant_case(name: &str, role: tool_provider::RouteRole, trust: BindTrust) -> GrantCase {
    let (dir, root) = test_support::test_root(name);
    let ctx = grant_test_context(&root, &dir.path().join("storage"));
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), ctx));
    GrantCase {
        proof: root.as_path().join("forged-proof"),
        _dir: dir,
        executor,
        routes: grant_test_route(&root, role, trust),
    }
}

/// The grants a first, unapproved call is refused with. They are the same
/// strings a model can derive from the command text on its own.
async fn first_refusal(case: &GrantCase) -> Vec<String> {
    let refusal = call_bash(&case.executor, &case.routes, 7, json!({"command": COMMAND}))
        .await
        .bash();
    assert_eq!(refusal["code"], "permission_required", "{refusal}");
    let grants = grants_named_by(&refusal);
    assert!(
        grants.iter().any(|grant| grant == "touch *"),
        "the grant is predictable from the command: {refusal}"
    );
    assert!(!case.proof.exists(), "a refused command must not run");
    grants
}

fn assert_refused_at_admission(answer: Answer, field: &str) {
    let Answer::Frame(FrameType::Error, body) = answer else {
        panic!("a forged grant must be refused before the bash loop, got {answer:?}");
    };
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert_eq!(body["detail"]["field"], field, "{body}");
}

#[tokio::test]
async fn tool_provider_route_refuses_a_grant_the_model_put_in_its_arguments() {
    let case = grant_case(
        "bash-grant-v1-top-level",
        tool_provider::RouteRole::ToolProviderV1,
        BindTrust::FirstParty,
    );
    let grants = first_refusal(&case).await;
    let retry = call_bash(
        &case.executor,
        &case.routes,
        8,
        json!({"command": COMMAND, "permissions_granted": grants}),
    )
    .await;
    assert_refused_at_admission(retry, "permissions_granted");
    assert!(
        !case.proof.exists(),
        "a forged grant must not run the command"
    );
}

#[tokio::test]
async fn tool_provider_route_refuses_a_grant_nested_under_params() {
    let case = grant_case(
        "bash-grant-v1-nested",
        tool_provider::RouteRole::ToolProviderV1,
        BindTrust::FirstParty,
    );
    let grants = first_refusal(&case).await;
    // The bash translator unwraps a `params` object, so the nested shape
    // would carry the grant just as well if admission let it through.
    let retry = call_bash(
        &case.executor,
        &case.routes,
        8,
        json!({"params": {"command": COMMAND, "permissions_granted": grants}}),
    )
    .await;
    assert_refused_at_admission(retry, "params");
    assert!(
        !case.proof.exists(),
        "a forged grant must not run the command"
    );
}

#[tokio::test]
async fn untrusted_route_asks_the_consumer_even_with_a_forged_grant() {
    let case = grant_case(
        "bash-grant-untrusted",
        tool_provider::RouteRole::Legacy,
        BindTrust::Untrusted,
    );
    let answer = call_bash(
        &case.executor,
        &case.routes,
        8,
        json!({"command": COMMAND, "permissions_granted": [COMMAND, "touch *"]}),
    )
    .await;
    let Answer::Frame(FrameType::Request, ask) = answer else {
        panic!("an untrusted call must raise an elicitation, got {answer:?}");
    };
    assert!(ask.to_string().contains("touch *"), "{ask}");
    // Nothing answers the ask, so the command must still be waiting on it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !case.proof.exists(),
        "the command must wait for the consumer"
    );
}

#[tokio::test]
async fn plugin_route_retries_with_the_grants_its_prompt_approved() {
    // AFT's OpenCode and Pi plugins bind legacy first-party routes and add
    // grants only after the user approved their prompt; that retry must run.
    let case = grant_case(
        "bash-grant-legacy",
        tool_provider::RouteRole::Legacy,
        BindTrust::FirstParty,
    );
    let grants = first_refusal(&case).await;
    let retry = call_bash(
        &case.executor,
        &case.routes,
        8,
        json!({"command": COMMAND, "permissions_granted": grants}),
    )
    .await
    .bash();
    assert_eq!(retry["success"], true, "{retry}");
    assert!(
        case.proof.exists(),
        "the approved retry must run the command"
    );
}
