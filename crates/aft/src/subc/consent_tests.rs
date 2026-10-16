use super::*;

// Consent workflow/vector cases use the consent service's '/'-rooted path
// contract on Unix, pinned to Cingulate 6bb5f4d. Windows drive and UNC paths
// have no agreed representation; fail-closed tests below check zero traffic.

use crate::sandbox_spawn::{
    resolve_sandbox_spawn, with_worker_jail, RequestedSandboxTier, SandboxTaskKind, SpawnPlan,
};

/// Serializes the consent tests: they share the process-wide test transport
/// and the test override for how long a call is held for an answer.
static CONSENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct FakeState {
    up: bool,
    calls: Vec<(String, Value)>,
    /// Elicitation id per consent call key: the same key returns the same ask.
    asks: HashMap<String, String>,
    /// Settled replies per elicitation id.
    answers: HashMap<String, Value>,
    macos: bool,
    cards: HashMap<(String, String), String>,
    next_id: usize,
}

impl FakeState {
    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("acc_{}", self.next_id)
    }
}

// Accept only declared fields, correct types, absolute paths, nonblank strings
// and applicable size/line limits, matching Cingulate's decoder in
// crates/cingulate-decode/src/access.rs at 6bb5f4d. Malformed requests receive
// the same named refusal instead of a permissive test-double success.
#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FakeAccessRequest {
    ToolPermission {
        call_key: String,
        tool: String,
        operation: String,
        paths: Vec<String>,
        #[serde(default)]
        command: Option<String>,
        reason: String,
        args_hash: String,
        #[serde(default)]
        schema_pin: Option<String>,
        pattern: String,
        #[serde(default)]
        head_policy: Option<String>,
    },
    MacosPermission {
        folder: String,
        program: String,
        steps: String,
    },
}

fn fake_refusal(code: &str, message: &str) -> ConsentError {
    ConsentError::Refused {
        code: code.into(),
        message: message.into(),
    }
}

impl FakeAccessRequest {
    fn decode(params: &Value) -> Result<Self, ConsentError> {
        let request: Self = serde_json::from_value(params.clone())
            .map_err(|error| fake_refusal("elicitation_invalid_request", &error.to_string()))?;
        let invalid = |message| fake_refusal("elicitation_invalid_request", message);
        match &request {
            Self::MacosPermission {
                folder,
                program,
                steps,
            } => {
                if !folder.starts_with('/')
                    || folder.contains('\0')
                    || program.trim().is_empty()
                    || steps.trim().is_empty()
                {
                    return Err(invalid(
                        "folder must be absolute; program and steps must be nonblank",
                    ));
                }
            }
            Self::ToolPermission {
                call_key,
                tool,
                operation,
                paths,
                command,
                reason,
                args_hash,
                schema_pin,
                pattern,
                head_policy,
            } => {
                if tool.chars().count() > 64
                    || [call_key, tool, args_hash, pattern]
                        .iter()
                        .any(|s| s.trim().is_empty())
                    || schema_pin.as_ref().is_some_and(|s| s.trim().is_empty())
                {
                    return Err(invalid(
                        "call_key, tool, args_hash, pattern and any schema_pin must be nonblank",
                    ));
                }
                if !matches!(operation.as_str(), "read" | "write" | "exec") {
                    return Err(invalid("operation must be read, write or exec"));
                }
                if paths.len() > 16
                    || paths
                        .iter()
                        .any(|p| !p.starts_with('/') || p.contains('\0'))
                {
                    return Err(invalid("paths must contain at most 16 absolute paths"));
                }
                if (operation == "exec") != command.is_some()
                    || command
                        .as_ref()
                        .is_some_and(|c| c.trim().is_empty() || c.chars().count() > 600)
                {
                    return Err(invalid(
                        "command is required for exec only and limited to 600 characters",
                    ));
                }
                if reason.trim().is_empty()
                    || reason.chars().count() > 160
                    || reason.contains(['\r', '\n', '\0'])
                {
                    return Err(invalid(
                        "reason must be a nonblank single line of at most 160 characters",
                    ));
                }
                if head_policy
                    .as_ref()
                    .is_some_and(|p| !matches!(p.as_str(), "allow" | "not_held"))
                {
                    return Err(invalid("head_policy must be allow or not_held"));
                }
            }
        }
        Ok(request)
    }
}

/// A stand-in for Cingulate that keeps one ask per call key and long-polls.
#[derive(Default)]
struct FakeCingulate {
    state: StdMutex<FakeState>,
}

impl FakeCingulate {
    fn up() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        {
            let mut state = fake.state.lock().unwrap();
            state.up = true;
            // The fake service models a macOS Cingulate instance even when
            // the caller's permission probes are simulated on Linux.
            state.macos = true;
        }
        fake
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    #[cfg(unix)]
    fn answer(&self, call_key: &str, reply: Value) {
        let mut state = self.state.lock().unwrap();
        let id = state.asks[call_key].clone();
        state.answers.insert(id, reply);
    }

    fn reply(&self, method: &str, params: &Value) -> Result<Option<Value>, ConsentError> {
        let mut state = self.state.lock().unwrap();
        if !state.up {
            return Err(ConsentError::Unavailable("cingulate is not running".into()));
        }
        state.calls.push((method.to_string(), params.clone()));
        match method {
            "consent.request" => {
                let key = match FakeAccessRequest::decode(params)? {
                    FakeAccessRequest::ToolPermission { call_key, .. } => call_key,
                    FakeAccessRequest::MacosPermission {
                        folder, program, ..
                    } => {
                        if !state.macos {
                            return Err(fake_refusal(
                                "access_macos_only",
                                "macOS permission cards require macOS",
                            ));
                        }
                        let key = (folder, program);
                        let id = match state.cards.get(&key) {
                            Some(id) => id.clone(),
                            None => {
                                let id = state.new_id();
                                state.cards.insert(key, id.clone());
                                id
                            }
                        };
                        return Ok(Some(json!({"elicitation_id":id})));
                    }
                };
                let id = match state.asks.get(&key) {
                    Some(id) => id.clone(),
                    None => {
                        let id = state.new_id();
                        state.asks.insert(key, id.clone());
                        id
                    }
                };
                Ok(Some(json!({"elicitation_id":id})))
            }
            "consent.await" => {
                let id = params["elicitation_id"].as_str().unwrap();
                if state.cards.values().any(|card| card == id) {
                    return Err(fake_refusal(
                        "access_informational",
                        "informational cards cannot be awaited",
                    ));
                }
                assert!(params["timeout_ms"].as_u64().unwrap() <= 120_000);
                Ok(state.answers.get(id).cloned())
            }
            "consent.withdraw" => {
                if let Some(id) = params["elicitation_id"].as_str() {
                    state.cards.retain(|_, card| card != id);
                }
                Ok(Some(
                    json!({"state":"refused:access_withdrawn","outcome":"access_withdrawn","code":"access_withdrawn"}),
                ))
            }
            _ => Ok(Some(
                json!({"state": "refused:access_withdrawn", "outcome": "access_withdrawn", "code": "access_withdrawn"}),
            )),
        }
    }
}

struct SharedFake(Arc<FakeCingulate>);

impl ConsentTransport for SharedFake {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        let fake = Arc::clone(&self.0);
        Box::pin(async move {
            // Go through the real envelope encoder and decoder on both sides.
            let sent: Value = serde_json::from_slice(&encode(call.method, &call.params)?).unwrap();
            let params = sent["params"].clone();
            let wait = Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(0));
            let end = Instant::now() + wait.min(call.timeout);
            let result = loop {
                if let Some(reply) = fake.reply(call.method, &params)? {
                    break reply;
                }
                // An open ask long-polls until it settles or the wait ends.
                fake.state.lock().unwrap().calls.pop();
                if Instant::now() >= end {
                    break json!({"state": "pending:head"});
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            decode_result(&serde_json::to_vec(&json!({ "result": result })).unwrap())
        })
    }
}

fn stamp(name: &str, kind: &str, parent: Option<&str>) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
        "ref": name,
        "scope_epoch": 7,
        "kind": kind,
        "parent": parent.map(|parent| json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": parent,
            "scope_epoch": 7,
        })),
        "owner_authorized": true,
    }))
    .unwrap()
}

fn identity(root: &Path, scope: Option<subc_protocol::scope::ScopeStamp>) -> RouteIdentity {
    let session = scope
        .as_ref()
        .map_or("standalone".to_string(), |stamp| stamp.scope_ref.clone());
    RouteIdentity(Arc::new(RouteIdentityData {
        root: ProjectRootId::from_path(root).unwrap(),
        project_root: root.into(),
        harness: "runner".into(),
        session: session.clone(),
        role: tool_provider::RouteRole::ToolProviderV1,
        trust: BindTrust::FirstParty,
        spawn_principal: AuthenticatedPrincipal::RouteBind {
            trust: PrincipalTrust::FirstParty,
            route_channel: 41,
            route_epoch: 1,
            project_root: root.into(),
            harness: "runner".into(),
            session_id: session,
            principal_id: Some("reserved:broca".into()),
        },
        consumer_elicitation_capable: false,
        disabled_tools: Arc::new(vec![]),
        scope,
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
    }))
}

struct Fixture {
    dir: tempfile::TempDir,
    head_root: PathBuf,
    ctx: Arc<AppContext>,
    executor: Arc<Executor>,
    head: RouteIdentity,
    worker: RouteIdentity,
    active: ActiveToolCalls,
    connection_cancel: PersistentCancelSignal,
    fake: Arc<FakeCingulate>,
}

fn fixture(fake: Arc<FakeCingulate>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let head_root = crate::access_policy::policy_path(&dir.path().join("head"));
    let worker_root = crate::access_policy::policy_path(&dir.path().join("worker"));
    std::fs::create_dir_all(&head_root).unwrap();
    std::fs::create_dir_all(&worker_root).unwrap();
    std::fs::write(head_root.join("shared.txt"), "head-owned contents\n").unwrap();
    std::fs::write(dir.path().join("elsewhere.txt"), "unheld contents\n").unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(worker_root.clone());
    config.storage_dir = Some(dir.path().join("store"));
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    ctx.set_db(Arc::new(StdMutex::new(
        crate::db::open(&dir.path().join("store").join("aft.db")).unwrap(),
    )));
    let head = identity(&head_root, Some(stamp("head", "head", None)));
    let worker = identity(&worker_root, Some(stamp("worker", "worker", Some("head"))));
    access_grants::register(&head, &ctx).unwrap();
    access_grants::register(&worker, &ctx).unwrap();
    // These permission-request cases exercise an explicitly selected ask mode,
    // not the primary agent's temporary full-access default.
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"ask"}}),
        &head,
        &ctx,
    )
    .unwrap();
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(worker.root.clone(), Arc::clone(&ctx)));
    install_test_transport(Some(Arc::new(SharedFake(Arc::clone(&fake)))));
    Fixture {
        dir,
        head_root,
        ctx,
        executor,
        head,
        worker,
        active: Arc::default(),
        connection_cancel: PersistentCancelSignal::new(),
        fake,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        install_test_transport(None);
        set_hold_budget_for_test(None);
    }
}

fn read_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::read::handle_read(&request, ctx)
}

#[cfg(unix)]
fn scope_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    match request.command.as_str() {
        "read" => read_dispatch(request, ctx),
        "grep" => crate::commands::grep::handle_grep(&request, ctx),
        "write" => crate::commands::write::handle_write(&request, ctx),
        other => panic!("unexpected scope test command: {other}"),
    }
}

impl Fixture {
    /// Send one `read` tool call on `identity`'s route and return the reply body.
    async fn read(&self, identity: &RouteIdentity, path: &Path, corr: u64) -> Value {
        let mut replies = self.send_read(identity, path, corr).await;
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .expect("the call ends with one terminal frame")
            .expect("writer stays open");
        serde_json::from_slice(&reply.body).unwrap()
    }

    async fn send_read(
        &self,
        identity: &RouteIdentity,
        path: &Path,
        corr: u64,
    ) -> mpsc::Receiver<WriterFrame> {
        self.send_with(identity, path, corr, read_dispatch).await
    }

    async fn send_with(
        &self,
        identity: &RouteIdentity,
        path: &Path,
        corr: u64,
        dispatch: DispatchFn,
    ) -> mpsc::Receiver<WriterFrame> {
        self.send_tool(identity, "read", json!({"filePath": path}), corr, dispatch)
            .await
    }

    #[cfg(unix)]
    async fn tool(&self, name: &str, arguments: Value, corr: u64) -> Value {
        let mut replies = self
            .send_tool(&self.worker, name, arguments, corr, scope_dispatch)
            .await;
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .expect("the scope test call terminates")
            .unwrap();
        serde_json::from_slice(&reply.body).unwrap()
    }
    async fn send_tool(
        &self,
        identity: &RouteIdentity,
        name: &str,
        arguments: Value,
        corr: u64,
        dispatch: DispatchFn,
    ) -> mpsc::Receiver<WriterFrame> {
        if self.executor.actor_context(&identity.root).is_none() {
            self.executor
                .register_actor(identity.root.clone(), Arc::clone(&self.ctx));
        }
        let routes = HashMap::from([(route_key(41, 1), identity.clone())]);
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            corr,
            serde_json::to_vec(&json!({
                "name": name,
                "preset": if identity.scope.as_ref().is_some_and(|s| s.kind == subc_protocol::scope::ScopeKind::Head) { "head" } else { "worker" },
                "arguments": arguments,
            }))
            .unwrap(),
        )
        .unwrap();
        let (writer, replies) = mpsc::channel(8);
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
            PhaseTrace::new(Instant::now()),
            &routes,
            &HashMap::new(),
            &ReclaimedRoutes::default(),
            &mut HashMap::new(),
            &self.executor,
            &self.active,
            &Arc::new(PendingDeferredSetups::default()),
            &Arc::new(Notify::new()),
            &self.connection_cancel,
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
            dispatch,
            &deferred_tx,
            false,
            1024 * 1024,
            &drain::ModuleDrainWindow::default(),
            None,
        )
        .await
        .unwrap();
        replies
    }

    fn asks(&self) -> Vec<asks::AskRecord> {
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        asks::for_scope(&conn, &access_grants::scope_key(&self.worker)).unwrap()
    }

    fn worker_grants(&self) -> Vec<AccessGrant> {
        let db = self.ctx.db().unwrap();
        let conn = db.lock().unwrap();
        crate::db::access_grants::list(&conn, &access_grants::scope_key(&self.worker)).unwrap()
    }

    #[cfg(unix)]
    async fn allow_read_scope(&self) {
        set_hold_budget_for_test(Some(Duration::from_millis(100)));
        let target = self.head_root.join("shared.txt");
        let pending = self.read(&self.worker, &target, 201).await;
        assert_eq!(code(&pending), "access_pending", "{pending}");
        let request = &self.fake.calls("consent.request")[0];
        self.fake.answer(
            request["call_key"].as_str().unwrap(),
            json!({"state":"settled", "outcome":"allow_scope", "answered_by":"user"}),
        );
        let allowed = self.read(&self.worker, &target, 202).await;
        assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    }
}

#[cfg(unix)]
#[path = "consent_rust_sources_tests.rs"]
mod rust_sources;

fn code(reply: &Value) -> &str {
    reply["structuredContent"]["code"].as_str().unwrap_or("")
}

fn text(reply: &Value) -> String {
    reply["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(unix)]
async fn wait_for(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn consent_parked_ask_only_scope_goodbye_ended_withdraws_and_refuses_retry() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let mut f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    Arc::get_mut(&mut f.worker.0).unwrap().call_mode = subc_protocol::call_mode::CallMode::Codemode;
    let stamp = f.worker.scope.clone().unwrap();
    let target = f.head_root.join("shared.txt");
    let (writer, mut controls) = mpsc::channel(8);
    let pending = super::super::readiness::PendingControlReplies::default();
    let control = Arc::new(super::super::readiness::DaemonControl::new(
        writer,
        Arc::new(DispatchPathMetrics::new()),
        PROTOCOL_VERSION,
        vec![subc_protocol::scope::SCOPE_DESCRIBE_OP.into()],
        vec![],
        pending.clone(),
        HELLO_CORR + 1,
    ));
    let scopes = Arc::new(super::super::run_scope::RunScopes::new(control.clone()));
    scopes.bind(&f.worker).unwrap();
    let request = |corr| DecodedFrame {
        frame: Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            corr,
            serde_json::to_vec(
                &json!({"name":"read", "preset":"worker", "arguments":{"filePath":target}}),
            )
            .unwrap(),
        )
        .unwrap(),
        phase_trace: PhaseTrace::new(Instant::now()),
        parsed_body: Default::default(),
    };
    let reply = |corr, status| {
        let live = status == subc_protocol::scope::ScopeStatus::Live;
        let body = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
            status,
            scope_epoch: Some(stamp.scope_epoch),
            daemon_incarnation: "consent-test".into(),
            owner_synced: true,
            owner_configured: true,
            scope: live.then(|| stamp.clone()),
        };
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    };
    let (ready, mut ready_rx) = mpsc::unbounded_channel();
    let held = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&held), "access_pending", "{held}");
    assert_eq!(f.asks().len(), 1);
    assert_eq!(scopes.parked_scopes().len(), 1);
    assert!(f.ctx.bash_background().live_scope_stamps().is_empty());

    let (describes, mut ended, worker) =
        super::super::scope_describe::start(control, f.executor.clone(), scopes.clone());
    // Goodbye closes the consumer route, but its already-parked ask needs a
    // daemon determination before the run can discard the remaining consent.
    note_route_closed(route_key(41, 1));
    describes.goodbye(stamp.clone());
    let query = tokio::time::timeout(Duration::from_secs(2), controls.recv())
        .await
        .expect("parked consent work must trigger scope.describe without a shell task")
        .unwrap();
    let body: Value = serde_json::from_slice(&query.body).unwrap();
    assert_eq!(body["op"], "scope.describe");
    assert_eq!(body["ref"], stamp.scope_ref);
    assert!(pending.resolve(reply(
        query.header.corr,
        subc_protocol::scope::ScopeStatus::Ended
    )));
    let confirmed = tokio::time::timeout(Duration::from_secs(2), ended.recv())
        .await
        .unwrap()
        .unwrap();
    scopes.revoke(&confirmed.stamp);
    wait_for(|| !f.fake.calls("consent.withdraw").is_empty() && f.asks().is_empty()).await;
    assert!(f.worker.run_scope.get().unwrap().revoked());
    assert!(scopes.parked_scopes().is_empty());

    // A retry crosses the same run admission seam as a real incoming frame.
    // It must be refused before it can reach the tool or claim an approval.
    assert!(scopes.defer(request(8), &f.worker, &ready).is_ok());
    let refusal = tokio::time::timeout(Duration::from_secs(2), controls.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refusal.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&refusal.body).unwrap();
    assert_eq!(body["code"], "run_scope_revoked");
    assert!(
        ready_rx.try_recv().is_err(),
        "a revoked retry must not be dispatched"
    );
    assert_eq!(f.fake.calls("consent.request").len(), 1);
    assert!(f.worker_grants().is_empty());
    worker.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn consent_held_call_becomes_pending_and_retry_runs_after_allow_once() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.head_root.join("shared.txt");

    let held = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&held), "access_pending", "{held}");
    assert!(text(&held).contains("end your turn") || text(&held).contains("End your turn"));
    assert!(!text(&held).contains("head-owned contents"));
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 1);
    let first = &requests[0];
    assert_eq!(first["kind"], "tool_permission");
    assert_eq!(first["operation"], "read");
    assert_eq!(first["head_policy"], "allow");
    assert_eq!(first["paths"][0], json!(target));
    let call_key = first["call_key"].as_str().unwrap().to_string();
    assert_eq!(f.asks().len(), 1, "the open ask is persisted");

    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "head"}),
    );
    // The retry arrives under a new transport call; it re-sends the original
    // consent call key, gets the same ask, and runs once.
    let allowed = f.read(&f.worker, &target, 8).await;
    assert_ne!(code(&allowed), "access_pending", "{allowed}");
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["call_key"], json!(call_key));
    assert!(f.asks().is_empty(), "allow_once is used up by the run");
    assert!(
        f.worker_grants().is_empty(),
        "allow_once installs no standing grant"
    );

    // A later identical call asks again under a new consent call key.
    let again = f.read(&f.worker, &target, 9).await;
    assert_eq!(code(&again), "access_pending", "{again}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 3);
    assert_ne!(requests[2]["call_key"], json!(call_key));
}

#[cfg(unix)]
#[tokio::test]
async fn consent_answer_within_the_budget_runs_the_held_call() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let target = f.head_root.join("shared.txt");
    let mut replies = f.send_read(&f.worker, &target, 7).await;
    let fake = Arc::clone(&f.fake);
    wait_for(|| !fake.calls("consent.request").is_empty()).await;
    let call_key = f.fake.calls("consent.request")[0]["call_key"]
        .as_str()
        .unwrap()
        .to_string();
    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_scope"}),
    );
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(text(&reply).contains("head-owned contents"), "{reply}");
    // allow_scope with no answered_by is the head's answer; the head holds the
    // path, so the scope grant is installed on the worker's scope.
    assert_eq!(f.worker_grants().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_read_admits_sibling_read_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let sibling = f.head_root.join("sibling.txt");
    std::fs::write(&sibling, "sibling contents").unwrap();
    f.allow_read_scope().await;
    let count = f.fake.calls("consent.request").len();
    let reply = f.read(&f.worker, &sibling, 203).await;
    assert!(text(&reply).contains("sibling contents"), "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), count);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_read_admits_grep_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    std::fs::write(f.head_root.join("sibling.txt"), "sibling contents").unwrap();
    f.allow_read_scope().await;
    let count = f.fake.calls("consent.request").len();
    let reply = f
        .tool(
            "grep",
            json!({"path": f.head_root, "pattern": "sibling contents"}),
            203,
        )
        .await;
    assert!(text(&reply).contains("sibling contents"), "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), count);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_read_does_not_admit_write() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.allow_read_scope().await;
    let target = f.head_root.join("shared.txt");
    let reply = f
        .tool(
            "write",
            json!({"filePath": target, "content": "changed"}),
            203,
        )
        .await;
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "head-owned contents\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_read_does_not_admit_parent() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.allow_read_scope().await;
    let reply = f.read(&f.worker, f.head_root.parent().unwrap(), 203).await;
    assert_eq!(code(&reply), "access_pending", "{reply}");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_read_preserves_floor_denial() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let secret = f.head_root.join("secret.txt");
    std::fs::write(&secret, "credential").unwrap();
    f.ctx
        .update_config(|config| config.sandbox.read_deny.push(secret.clone()));
    f.allow_read_scope().await;
    let count = f.fake.calls("consent.request").len();
    let reply = f.read(&f.worker, &secret, 203).await;
    assert_eq!(code(&reply), "denied_floor", "{reply}");
    assert!(!text(&reply).contains("credential"));
    assert_eq!(f.fake.calls("consent.request").len(), count);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_card_matches_stored_directory() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.allow_read_scope().await;
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(
        request["pattern"],
        json!(format!("{}/**", f.head_root.display()))
    );
    assert_eq!(request["paths"], json!([f.head_root.join("shared.txt")]));
    assert_eq!(request["tool"], "read");
    assert_eq!(request["operation"], "read");
    assert_eq!(request["reason"], "Allow scope: read-only access for all AFT read tools within the displayed directory tree. Protected paths remain denied.");
    assert_eq!(
        f.worker_grants(),
        vec![AccessGrant {
            path: f.head_root.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_old_pending_file_is_not_widened() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    let target = f.head_root.join("shared.txt");
    assert_eq!(
        code(&f.read(&f.worker, &target, 201).await),
        "access_pending"
    );
    let record = f.asks().pop().unwrap();
    let mut old_request = record.request.unwrap();
    old_request["pattern"] = json!(target);
    old_request["reason"] = json!("read needs to read a path outside this route's roots");
    let old_grant = record.pending.unwrap();
    {
        let db = f.ctx.db().unwrap();
        asks::open(
            &db.lock().unwrap(),
            &record.key,
            record.call_key.as_deref().unwrap(),
            &record.selector,
            &old_request,
            &old_grant,
            crate::db::call_ledger::now_ms(),
        )
        .unwrap();
    }
    f.fake.answer(
        record.call_key.as_deref().unwrap(),
        json!({"state":"settled", "outcome":"allow_scope", "answered_by":"user"}),
    );
    let allowed = f.read(&f.worker, &target, 202).await;
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    assert_eq!(f.worker_grants(), vec![old_grant]);
    assert_eq!(f.fake.calls("consent.request")[1]["pattern"], json!(target));
    let sibling = f.head_root.join("sibling.txt");
    std::fs::write(&sibling, "sibling contents").unwrap();
    assert_eq!(
        code(&f.read(&f.worker, &sibling, 203).await),
        "access_pending"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_head_file_grant_allows_once_without_delegating_directory() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    // Cards and stored grants carry canonical policy paths. Temp directories can
    // sit behind a symlink (macOS /var -> /private/var), so compare canonically.
    let target = crate::access_policy::policy_path(&f.dir.path().join("elsewhere.txt"));
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &AccessGrant {
                path: target.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    assert_eq!(
        code(&f.read(&f.worker, &target, 201).await),
        "access_pending"
    );
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["head_policy"], "allow");
    assert_eq!(request["pattern"], json!(target));
    assert_eq!(request["reason"], READ_FILE_SCOPE_REASON);
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({
            "state":"settled", "outcome":"allow_once", "answered_by":"head",
        }),
    );
    let allowed = f.read(&f.worker, &target, 202).await;
    assert!(text(&allowed).contains("unheld contents"), "{allowed}");
    assert!(f.worker_grants().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_head_file_only_scope_answer_stores_only_the_file() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    // Cards and stored grants carry canonical policy paths. Temp directories can
    // sit behind a symlink (macOS /var -> /private/var), so compare canonically.
    let target = crate::access_policy::policy_path(&f.dir.path().join("elsewhere.txt"));
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &grant,
        )
        .unwrap();
    }
    assert_eq!(
        code(&f.read(&f.worker, &target, 201).await),
        "access_pending"
    );
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["pattern"], json!(target));
    assert_eq!(request["head_policy"], "allow");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({
            "state":"settled", "outcome":"allow_scope", "answered_by":"head",
        }),
    );
    let allowed = f.read(&f.worker, &target, 202).await;
    assert!(text(&allowed).contains("unheld contents"), "{allowed}");
    assert_eq!(f.worker_grants(), vec![grant]);
    let sibling = f.dir.path().join("sibling.txt");
    std::fs::write(&sibling, "sibling contents").unwrap();
    assert_eq!(
        code(&f.read(&f.worker, &sibling, 203).await),
        "access_pending"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_head_directory_still_delegates_directory() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    let target = f.head_root.join("shared.txt");
    assert_eq!(
        code(&f.read(&f.worker, &target, 201).await),
        "access_pending"
    );
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["pattern"], format!("{}/**", f.head_root.display()));
    assert_eq!(request["head_policy"], "allow");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({
            "state":"settled", "outcome":"allow_scope", "answered_by":"head",
        }),
    );
    let allowed = f.read(&f.worker, &target, 202).await;
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    assert_eq!(
        f.worker_grants(),
        vec![AccessGrant {
            path: f.head_root.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }]
    );
}

#[cfg(unix)]
fn scope_probe_call(f: &Fixture, target: &Path) -> HeldCall {
    HeldCall {
        reply_deadline: Instant::now() + Duration::from_secs(5),
        identity: f.worker.clone(),
        tool: "read".into(),
        arguments: json!({"path":target}),
        request_id: "scope-probe".into(),
        executor: Arc::clone(&f.executor),
        route: RouteChannel {
            channel: 41,
            epoch: 1,
        },
        corr: 201,
        active: Arc::clone(&f.active),
        connection_cancel: f.connection_cancel.clone(),
        drain: drain::ModuleDrainWindow::default(),
    }
}

#[cfg(unix)]
async fn assert_probe_leaves_database_available(call: &HeldCall) {
    let count = tokio::time::timeout(
        Duration::from_millis(100),
        with_db(call, |conn, _| {
            conn.query_row("SELECT COUNT(*) FROM access_grants", [], |row| {
                row.get::<_, i64>(0)
            })
        }),
    )
    .await
    .expect("another database user proceeds while the scope probe is stalled");
    assert!(count.unwrap() >= 0);
}

#[cfg(unix)]
async fn wait_for_scope_probe_idle() {
    tokio::time::timeout(Duration::from_secs(1), async {
        while DIRECTORY_SCOPE_PROBE_LIMITER
            .in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the released filesystem helper returns its scope-probe slot");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_stalled_prepare_probe_is_bounded_and_releases_database() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.head_root.join("shared.txt");
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let call = Arc::new(scope_probe_call(&f, &target));
    let gate = crate::bounded_io::stall_helper(&f.head_root);
    let running_call = Arc::clone(&call);
    let started = Instant::now();
    let task = tokio::spawn(async move {
        let route = RouteHold::new(running_call.route);
        hold(
            &running_call,
            grant,
            Instant::now() + Duration::from_millis(100),
            &route,
            None,
        )
        .await
    });
    let _gate = tokio::task::spawn_blocking(move || {
        gate.reached
            .recv_timeout(Duration::from_secs(1))
            .expect("scope probe entered the bounded helper");
        gate
    })
    .await
    .unwrap();
    assert_probe_leaves_database_available(&call).await;
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("scope probe has a deadline")
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(result, Hold::Answer(_)));
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["pattern"], json!(target));
    assert_eq!(request["reason"], READ_FILE_SCOPE_REASON);
    assert_eq!(request["head_policy"], "allow");
    drop(_gate);
    wait_for_scope_probe_idle().await;
}

#[cfg(unix)]
#[tokio::test]
async fn consent_stalled_home_scope_probe_is_single_flight_and_file_only() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .expect("an absolute home path for the scope probe");
    let mut stalls = Vec::new();
    let mut stalled_helpers = 0;
    let started = Instant::now();
    // More requests than the shared filesystem pool can hold must still leave
    // room for unrelated reads while home canonicalization remains blocked.
    for n in 0..72 {
        let target = f.head_root.join(format!("home-probe-{n}.txt"));
        let grant = AccessGrant {
            path: target.clone(),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let call = scope_probe_call(&f, &target);
        let route = RouteHold::new(call.route);
        let gate = crate::bounded_io::stall_helper(&home);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            hold(&call, grant, Instant::now(), &route, None),
        )
        .await
        .expect("a stalled or busy optional probe falls back without waiting");
        assert!(matches!(result, Hold::Answer(_)));
        stalled_helpers += usize::from(gate.reached.try_recv().is_ok());
        stalls.push(gate);
        let requests = f.fake.calls("consent.request");
        let request = requests.last().expect("each request places a card");
        assert_eq!(requests.len(), n + 1);
        assert_eq!(request["pattern"], json!(target));
        assert_eq!(request["reason"], READ_FILE_SCOPE_REASON);
        assert_eq!(request["head_policy"], "allow");
        assert_eq!(request["operation"], "read");
    }
    assert_eq!(
        stalled_helpers, 1,
        "timed-out home probes must retain the single-flight slot"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "busy probes must not wait for another probe deadline"
    );
    let unrelated = f.head_root.join("unrelated.txt");
    std::fs::write(&unrelated, "unrelated read").unwrap();
    let content = crate::bounded_io::run(
        &unrelated,
        Some(Instant::now() + Duration::from_secs(1)),
        std::fs::read_to_string,
    )
    .expect("stalled home probes leave shared filesystem helpers available");
    assert_eq!(content, "unrelated read");
    drop(stalls);
    wait_for_scope_probe_idle().await;
    let next = AccessGrant {
        path: f.head_root.join("after-home-recovery.txt"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    assert_eq!(scope_before_db(&next).await.kind, GrantKind::Subtree);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_stalled_finish_probe_is_bounded_and_releases_database() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    let target = f.head_root.join("shared.txt");
    assert_eq!(
        code(&f.read(&f.worker, &target, 201).await),
        "access_pending"
    );
    let record = f.asks().pop().unwrap();
    let prep = Prepared {
        key: record.key,
        call_key: record.call_key.unwrap(),
        request: record.request.unwrap(),
        selector: selector(f.worker.scope.as_ref().unwrap()),
        grant: record.pending.unwrap(),
    };
    let call = Arc::new(scope_probe_call(&f, &target));
    let gate = crate::bounded_io::stall_helper(&f.head_root);
    let running_call = Arc::clone(&call);
    let task = tokio::spawn(async move {
        finish(
            &running_call,
            &prep,
            Settlement::Allow {
                scope: true,
                by_user: true,
            },
        )
        .await
    });
    let _gate = tokio::task::spawn_blocking(move || {
        gate.reached
            .recv_timeout(Duration::from_secs(1))
            .expect("settlement probe entered the bounded helper");
        gate
    })
    .await
    .unwrap();
    assert_probe_leaves_database_available(&call).await;
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("settlement probe has a deadline")
        .unwrap();
    assert!(matches!(result, Hold::Run { .. }));
    assert_eq!(
        f.worker_grants(),
        vec![AccessGrant {
            path: target,
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        }]
    );
    drop(_gate);
    wait_for_scope_probe_idle().await;
}

// Cingulate accepts only `/`-rooted paths, and native Windows drive or UNC
// paths are refused before any card is prepared (see
// consent_native_windows_drive_path_is_denied_without_sending_a_request), so a
// card built from a Windows fixture path is unreachable and never decodes.
#[cfg(unix)]
#[tokio::test]
async fn consent_scope_write_cards_keep_the_original_capability() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    for (kind, path, expected_reason, expected_pattern) in [
        (GrantKind::File, f.head_root.join("shared.txt"),
         "Allow scope: read/write access for all AFT file tools to the displayed file only. Protected paths remain denied.",
         f.head_root.join("shared.txt").to_string_lossy().into_owned()),
        (GrantKind::Subtree, f.head_root.clone(),
         "Allow scope: read/write access for all AFT file tools within the displayed directory tree. Protected paths remain denied.",
         format!("{}/**", f.head_root.display())),
    ] {
        let grant = AccessGrant { path: path.clone(), kind, operation: AccessOperation::Write };
        let Prep::Ask(prep) = prepare(&conn, &f.ctx, &f.worker, "write", &json!({"path":path}), &grant, &grant, None).unwrap() else {
            panic!("expected a fresh write card");
        };
        assert_eq!(prep.request["pattern"], expected_pattern);
        assert_eq!(prep.request["reason"], expected_reason);
        assert_eq!(prep.request["operation"], "write");
        assert_eq!(advertised_scope(&prep, grant.clone()), grant);
        FakeAccessRequest::decode(&prep.request).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn consent_scope_home_file_stays_file_only() {
    // The root-alias scan limiter is process-wide; serialize with every
    // other consent test that can reach it.
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("the test runner supplies a home directory");
    assert_file_only_read_scope(crate::access_policy::policy_path(&home).join("x"));
}

#[cfg(unix)]
#[test]
fn consent_scope_etc_hosts_stays_file_only() {
    // The root-alias scan limiter is process-wide; serialize with every
    // other consent test that can reach it.
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_read_scope(crate::access_policy::policy_path(Path::new("/etc/hosts")));
}

#[cfg(unix)]
#[test]
fn consent_scope_root_file_stays_file_only() {
    // The root-alias scan limiter is process-wide; serialize with every
    // other consent test that can reach it.
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_read_scope(PathBuf::from("/x"));
}

#[cfg(unix)]
fn assert_file_only_read_scope(path: PathBuf) {
    let original = AccessGrant {
        path,
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let scope = read_directory_scope(&original);
    assert_eq!(scope, original);
    assert_eq!(scope_reason(&scope), "Allow scope: read-only access for all AFT read tools to the displayed file only. Protected paths remain denied.");
    assert_eq!(pattern(&scope), original.path.to_string_lossy());
}

#[test]
fn consent_scope_nested_file_still_offers_directory() {
    // The root-alias scan limiter is process-wide; serialize with every
    // other consent test that can reach it.
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("nested/src");
    std::fs::create_dir_all(&parent).unwrap();
    let parent = crate::access_policy::policy_path(&parent);
    let scope = read_directory_scope(&AccessGrant {
        path: parent.join("lib.rs"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    });
    assert_eq!(scope.path, parent);
    assert_eq!(scope.kind, GrantKind::Subtree);
    assert_eq!(scope.operation, AccessOperation::Read);
    assert_eq!(scope_reason(&scope), "Allow scope: read-only access for all AFT read tools within the displayed directory tree. Protected paths remain denied.");
}

#[cfg(unix)]
#[test]
fn consent_scope_rejects_home_ancestors() {
    let home = Path::new("/srv/users/alice");
    for directory in ["/srv/users", "/srv/users/alice"] {
        assert!(
            !directory_scope_is_narrow(Path::new(directory), home, Path::new("/")),
            "{directory}"
        );
    }
}

#[cfg(unix)]
#[test]
fn consent_scope_rejects_volume_boundaries() {
    let home = Path::new("/srv/users/alice");
    let volume = Path::new("/mnt/data");
    for directory in ["/mnt/data", "/mnt/data/etc"] {
        assert!(
            !directory_scope_is_narrow(Path::new(directory), home, volume),
            "{directory}"
        );
    }
    assert!(directory_scope_is_narrow(
        Path::new("/mnt/data/crates/src"),
        home,
        volume
    ));
}

#[cfg(windows)]
#[test]
fn consent_scope_rejects_drive_and_unc_volume_boundaries() {
    for (root, home) in [
        (r"C:\", r"C:\Users\alice"),
        (r"\\server\share\", r"\\server\share\Users\alice"),
    ] {
        let root = Path::new(root);
        let home = Path::new(home);
        for directory in [
            root.to_path_buf(),
            root.join("etc"),
            home.to_path_buf(),
            home.parent().unwrap().to_path_buf(),
        ] {
            assert!(
                !directory_scope_is_narrow(&directory, home, root),
                "{}",
                directory.display()
            );
        }
        assert!(directory_scope_is_narrow(
            &root.join("crates/src"),
            home,
            root
        ));
    }
}

#[cfg(unix)]
#[test]
fn consent_stalled_root_alias_scan_is_single_flight_and_fails_closed() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("nested/src");
    std::fs::create_dir_all(&parent).unwrap();
    let grant = AccessGrant {
        path: parent.join("lib.rs"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };

    let limiter = Arc::new(ScopeProbeLimiter::new());
    let scan_count = Arc::new(AtomicUsize::new(0));
    let check_count = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let root_alias_check: Arc<dyn Fn(&Path, &Path) -> Option<bool> + Send + Sync> = {
        let limiter = Arc::clone(&limiter);
        let scan_count = Arc::clone(&scan_count);
        let check_count = Arc::clone(&check_count);
        Arc::new(move |_, _| {
            let first_check = check_count.fetch_add(1, Ordering::SeqCst) == 0;
            let scan_count = Arc::clone(&scan_count);
            let started = started_tx.clone();
            let release = Arc::clone(&release_rx);
            let result = Arc::clone(&limiter).scan(move || {
                if scan_count.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = started.send(());
                    let _ = release.lock().unwrap().recv();
                }
                Some(false)
            });
            // The first probe outlives its caller's deadline and keeps the
            // whole-probe slot until its helper returns. Keep that helper busy
            // a little past the alias scan, as a slow machine would, so the
            // recovery check below must wait for the slot instead of racing it.
            if first_check {
                std::thread::sleep(Duration::from_millis(100));
            }
            result
        })
    };

    let first_check = Arc::clone(&root_alias_check);
    let started_at = Instant::now();
    let first =
        read_directory_scope_with(&grant, move |directory, root| first_check(directory, root));
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("the dedicated root-alias helper started");
    assert_eq!(first, grant);
    assert!(started_at.elapsed() < Duration::from_secs(1));

    let fanout_started = Instant::now();
    let asks = (0..16)
        .map(|_| {
            let grant = grant.clone();
            let check = Arc::clone(&root_alias_check);
            std::thread::spawn(move || {
                read_directory_scope_with(&grant, move |directory, root| check(directory, root))
            })
        })
        .collect::<Vec<_>>();
    for ask in asks {
        assert_eq!(ask.join().unwrap(), grant);
    }
    assert!(fanout_started.elapsed() < Duration::from_secs(1));

    let unrelated = crate::bounded_io::run(
        &parent,
        Some(Instant::now() + Duration::from_secs(1)),
        |_| Ok(17),
    )
    .expect("a stalled optional scan leaves the shared helper pool available");
    assert_eq!(unrelated, 17);

    release_tx.send(()).unwrap();
    let release_deadline = Instant::now() + Duration::from_secs(1);
    while limiter.in_flight.load(Ordering::Acquire) {
        assert!(
            Instant::now() < release_deadline,
            "stalled scan did not finish"
        );
        std::thread::yield_now();
    }
    // The alias scan finishing does not end the first probe: its helper still
    // holds the process-wide probe slot until it returns. A busy slot yields a
    // file-only scope by design, so recovery is only observable once it is free.
    while DIRECTORY_SCOPE_PROBE_LIMITER
        .in_flight
        .load(Ordering::Acquire)
    {
        assert!(
            Instant::now() < release_deadline,
            "the timed-out probe did not return its slot"
        );
        std::thread::yield_now();
    }

    let next_check = Arc::clone(&root_alias_check);
    let next =
        read_directory_scope_with(&grant, move |directory, root| next_check(directory, root));
    assert_eq!(next.kind, GrantKind::Subtree);
    assert!(scan_count.load(Ordering::SeqCst) >= 2);
}

#[cfg(unix)]
#[test]
fn consent_scope_rejects_canonical_root_alias() {
    // The root-alias scan limiter is process-wide; serialize with every
    // other consent test that can reach it.
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let target = root.join("private/etc");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, root.join("etc")).unwrap();
    std::os::unix::fs::symlink(root.join("missing"), root.join("unavailable")).unwrap();
    let target = crate::access_policy::policy_path(&target);
    assert_eq!(directory_is_root_alias(&target, &root), Some(true));
    assert_eq!(
        directory_is_root_alias(&root.join("private/other"), &root),
        Some(false)
    );
}

#[cfg(unix)]
#[test]
fn consent_scope_rejects_physical_directory_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let volume = dir.path().join("volume");
    let home = volume.join("users/alice");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(volume.join("etc")).unwrap();
    for (name, target) in [
        ("volume-alias", volume.as_path()),
        ("home-alias", home.as_path()),
        ("ancestor-alias", home.parent().unwrap()),
    ] {
        let alias = dir.path().join(name);
        std::os::unix::fs::symlink(target, &alias).unwrap();
        assert_eq!(
            directory_is_protected_alias(&alias, &home, &volume),
            Some(true),
            "{name}"
        );
    }
    assert_eq!(
        directory_is_protected_alias(&dir.path().join("volume-alias/etc"), &home, &volume),
        Some(true)
    );
    let nested = volume.join("crates/src");
    std::fs::create_dir_all(&nested).unwrap();
    assert_eq!(
        directory_is_protected_alias(&nested, &home, &volume),
        Some(false)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_old_broad_card_is_restricted_to_file() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let grant = AccessGrant {
        path: PathBuf::from("/x"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(mut prep) = prepare(
        &conn,
        &f.ctx,
        &f.worker,
        "read",
        &json!({"path":"/x"}),
        &grant,
        &grant,
        None,
    )
    .unwrap() else {
        panic!("expected a file-only scope card");
    };
    assert_eq!(prep.request["pattern"], "/x");
    assert_eq!(prep.request["reason"], "Allow scope: read-only access for all AFT read tools to the displayed file only. Protected paths remain denied.");
    prep.request["pattern"] = json!("/**");
    assert_eq!(advertised_scope(&prep, grant.clone()), grant);
}

#[tokio::test]
async fn consent_floor_path_is_denied_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let secret = f.worker.project_root.join("secret.txt");
    std::fs::write(&secret, "credential").unwrap();
    f.ctx
        .update_config(|config| config.sandbox.read_deny.push(secret.clone()));
    // A standing grant on the worker's scope cannot override the read_deny
    // floor: the path stays denied even when a grant covers it.
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.worker),
            &AccessGrant {
                path: secret.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    let reply = f.read(&f.worker, &secret, 7).await;
    assert_eq!(code(&reply), "denied_floor", "{reply}");
    assert!(!text(&reply).contains("credential"));
    assert!(f.fake.calls("consent.request").is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_cingulate_down_returns_access_unavailable_never_a_host_prompt() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(Arc::new(FakeCingulate::default()));
    let reply = f.read(&f.worker, &f.head_root.join("shared.txt"), 7).await;
    assert_eq!(code(&reply), "access_unavailable", "{reply}");
    assert!(!reply.to_string().contains("permission_required"));
    assert!(!text(&reply).contains("head-owned contents"));
}

#[cfg(unix)]
#[tokio::test]
async fn consent_head_answer_is_rechecked_against_the_current_head_policy() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let directory = f.dir.path().join("delegated");
    std::fs::create_dir(&directory).unwrap();
    let target = directory.join("elsewhere.txt");
    std::fs::write(&target, "unheld contents").unwrap();
    let head_grant = AccessGrant {
        path: crate::access_policy::policy_path(&directory),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &head_grant,
        )
        .unwrap();
    }
    let pending = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["head_policy"], "allow");
    // The head's only covering grant is revoked while the ask is pending.
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::revoke(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &head_grant.path,
        )
        .unwrap();
    }
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state": "settled", "outcome": "allow_scope", "answered_by": "head"}),
    );
    let declined = f.read(&f.worker, &target, 8).await;
    assert_eq!(code(&declined), "access_declined", "{declined}");
    assert!(!text(&declined).contains("unheld contents"));
    assert!(f.worker_grants().is_empty(), "no worker grant is recorded");
    assert!(f.asks().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_user_answer_is_not_rechecked_against_the_head() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.dir.path().join("elsewhere.txt");
    let pending = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["head_policy"], "not_held");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "user"}),
    );
    let allowed = f.read(&f.worker, &target, 8).await;
    assert!(text(&allowed).contains("unheld contents"), "{allowed}");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_stamped_head_with_explicit_ask_requests_user_and_full_access_opts_out() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.dir.path().join("elsewhere.txt");
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let pending = f.read(&f.head, &target, 6).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("head_policy").is_none());
    f.fake.answer(
        requests[0]["call_key"].as_str().unwrap(),
        json!({"state":"settled","outcome":"allow_once","answered_by":"user"}),
    );
    let reply = f.read(&f.head, &target, 7).await;
    assert!(text(&reply).contains("unheld contents"), "{reply}");
    let count = f.fake.calls("consent.request").len();
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"full-access"}}),
        &f.head,
        &f.ctx,
    )
    .unwrap();
    let reply = f.read(&f.head, &target, 8).await;
    assert!(text(&reply).contains("unheld contents"), "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), count);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_cancel_and_route_close_withdraw_but_a_transport_drop_keeps_the_ask() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let target = f.head_root.join("shared.txt");
    let fake = Arc::clone(&f.fake);

    // Cancel: the held call's ask is withdrawn and forgotten.
    let _replies = f.send_read(&f.worker, &target, 7).await;
    wait_for(|| fake.calls("consent.request").len() == 1).await;
    assert!(cancel_active_tool_call(&f.active, &f.executor, route_key(41, 1), 7, "test").is_some());
    wait_for(|| fake.calls("consent.withdraw").len() == 1).await;
    wait_for(|| f.asks().is_empty()).await;

    // Route close (the consumer's Goodbye) withdraws too.
    let _replies = f.send_read(&f.worker, &target, 8).await;
    wait_for(|| fake.calls("consent.request").len() == 2).await;
    note_route_closed(route_key(41, 1));
    wait_for(|| fake.calls("consent.withdraw").len() == 2).await;
    wait_for(|| f.asks().is_empty()).await;

    // A transport drop detaches without withdrawing; the retry re-sends the
    // persisted call key.
    let _replies = f.send_read(&f.worker, &target, 9).await;
    wait_for(|| fake.calls("consent.request").len() == 3).await;
    let call_key = fake.calls("consent.request")[2]["call_key"].clone();
    // Connection teardown cancels the connection first, then drops its calls.
    f.connection_cancel.cancel();
    cancel_all_active_tool_calls(&f.active, &f.executor, "connection teardown");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(fake.calls("consent.withdraw").len(), 2);
    assert_eq!(f.asks().len(), 1);
    assert_eq!(f.asks()[0].call_key.as_deref(), call_key.as_str());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_same_key_retry_replays_the_recorded_access_pending() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let held = f.read(&f.worker, &f.head_root.join("shared.txt"), 7).await;
    assert_eq!(code(&held), "access_pending");
    // The ledger records a keyed call's terminal frame and replays it for a
    // retry under the same transport call key; it never re-dispatches.
    let db = f.ctx.db().unwrap();
    let conn = db.lock().unwrap();
    let key = crate::db::call_ledger::Key {
        carrier: "reserved:broca".into(),
        call_key: "transport-key".into(),
    };
    use crate::db::call_ledger::{admit, settle, Admission, RecordedFrame, State};
    assert!(matches!(
        admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap(),
        Admission::New
    ));
    let recorded = RecordedFrame {
        ty: FrameType::Response,
        body: serde_json::to_vec(&held).unwrap(),
    };
    assert!(settle(&conn, &key, &recorded, "access_pending", None, 1).unwrap());
    let Admission::Repeat(row) =
        admit(&conn, &key, "digest", None, State::DispatchStarted).unwrap()
    else {
        panic!("a same-key retry is a repeat");
    };
    assert_eq!(row.state, State::Settled);
    let replayed: Value = serde_json::from_slice(&row.frame.unwrap().body).unwrap();
    assert_eq!(code(&replayed), "access_pending");
}

fn normalize_access_pending_wire(wire: &str, request_id: &str, target_path: &str) -> String {
    let encoded_target_path = serde_json::to_string(target_path).unwrap();
    let escaped_target_path = encoded_target_path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap();
    wire.replace(request_id, "${REQUEST_ID}")
        .replace(escaped_target_path, "${HEAD_ROOT}/shared.txt")
}

#[test]
fn access_pending_target_normalization_handles_windows_paths() {
    let target_path = r"C:\worker\head\shared.txt";
    let encoded_target_path = serde_json::to_string(target_path).unwrap();
    let escaped_target_path = encoded_target_path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
        .unwrap();
    let wire = format!(
        r#"{{"id":"subc-41-7","message":"read {escaped_target_path}","path":"{escaped_target_path}"}}"#
    );

    assert_eq!(
        normalize_access_pending_wire(&wire, "subc-41-7", target_path),
        r#"{"id":"${REQUEST_ID}","message":"read ${HEAD_ROOT}/shared.txt","path":"${HEAD_ROOT}/shared.txt"}"#
    );
}

#[cfg(unix)]
#[tokio::test]
async fn access_pending_wire_matches_golden_vector() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let outside = f.head_root.join("shared.txt");
    assert!(!outside.starts_with(&f.worker.project_root));
    let mut replies = f.send_read(&f.worker, &outside, 7).await;
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .expect("the call ends with one terminal frame")
        .expect("writer stays open");
    let wire = String::from_utf8(reply.body.clone()).unwrap();
    let body: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["isError"], true);
    assert_eq!(code(&body), "access_pending");
    assert!(body.get("errorCode").is_none());
    assert!(body["structuredContent"].get("elicitation_id").is_none());

    // JSON escapes Windows separators, so replace the escaped path in-place to preserve every other wire byte.
    let request_id = body["structuredContent"]["id"].as_str().unwrap();
    let target_path = body["structuredContent"]["ask"]["path"].as_str().unwrap();
    assert!(body["structuredContent"]["message"]
        .as_str()
        .unwrap()
        .contains(target_path));
    let wire = normalize_access_pending_wire(&wire, request_id, target_path);
    assert_eq!(
        wire.as_bytes(),
        include_bytes!("../../../../test-vectors/access-pending-v1/worker-read-outside.json")
    );
}

#[tokio::test]
async fn access_pending_renderer_wire_matches_golden_vector_without_consent() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let outside = f.head_root.join("shared.txt");
    // The consent service cannot express native Windows drive/UNC paths.
    // Every platform still pins the exact pending reply bytes through AFT's
    // production message builder and transport serializer, without an ask.
    let call = HeldCall {
        reply_deadline: Instant::now() + Duration::from_secs(5),
        identity: f.worker.clone(),
        tool: "read".into(),
        arguments: json!({"filePath":outside}),
        request_id: "subc-41-7".into(),
        executor: Arc::clone(&f.executor),
        route: RouteChannel {
            channel: 41,
            epoch: 1,
        },
        corr: 7,
        active: Arc::clone(&f.active),
        connection_cancel: f.connection_cancel.clone(),
        drain: drain::ModuleDrainWindow::default(),
    };
    let grant = AccessGrant {
        path: outside.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let result = finish_tool_call_response(
        "read",
        &crate::subc_format::FormatContext::default(),
        pending_answer(&call, &grant),
        false,
        None,
        None,
    );
    let frame = super::super::wire::build_tool_response_frame(
        PROTOCOL_VERSION,
        call.route,
        call.corr,
        control_flags(),
        &result,
        BindTrust::FirstParty,
    )
    .unwrap();
    let wire = String::from_utf8(frame.body).unwrap();
    let body: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["isError"], true);
    assert_eq!(code(&body), "access_pending");
    assert!(body.get("errorCode").is_none());
    assert!(body["structuredContent"].get("elicitation_id").is_none());
    let request_id = body["structuredContent"]["id"].as_str().unwrap();
    let target_path = body["structuredContent"]["ask"]["path"].as_str().unwrap();
    assert!(body["structuredContent"]["message"]
        .as_str()
        .unwrap()
        .contains(target_path));
    let wire = normalize_access_pending_wire(&wire, request_id, target_path);
    assert_eq!(
        wire.as_bytes(),
        include_bytes!("../../../../test-vectors/access-pending-v1/worker-read-outside.json")
    );
    assert!(f.fake.calls("consent.request").is_empty());
}

#[test]
fn consent_routes_asks_by_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let worker = identity(dir.path(), Some(stamp("w", "worker", Some("h"))));
    let head = identity(dir.path(), Some(stamp("h", "head", None)));
    let head_run = identity(dir.path(), Some(stamp("r", "ephemeral", None)));
    let worker_run = identity(dir.path(), Some(stamp("r", "ephemeral", Some("w"))));
    let standalone = identity(dir.path(), None);
    let mut foreign = stamp("x", "worker", Some("h"));
    foreign.owner = Principal::Direct;
    let foreign = identity(dir.path(), Some(foreign));
    for (identity, asks) in [
        (&worker, true),
        (&worker_run, true),
        (&head, true),
        (&head_run, true),
        (&standalone, false),
        (&foreign, false),
    ] {
        assert_eq!(
            access_grants::ask_route(identity) != AskRoute::Lowered,
            asks
        );
    }
}

#[test]
fn consent_hold_budget_is_capped_at_100_seconds() {
    assert_eq!(declared_hold_budget(None), Duration::from_secs(20));
    assert_eq!(
        declared_hold_budget(Some(Duration::from_secs(1_800))),
        Duration::from_secs(100)
    );
}

#[cfg(unix)]
fn vector(name: &str) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/subc/fixtures/cingulate-tool-permission-v1/vectors");
    let bytes = std::fs::read(dir.join(name)).unwrap();
    // Each vector must be listed in Cingulate's SHA256SUMS with this hash.
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.split_once("  ")?;
            (file == name).then_some(hash)
        })
        .unwrap_or_else(|| panic!("{name} is not in SHA256SUMS"));
    use sha2::Digest;
    let actual: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(actual, expected, "{name} differs from Cingulate's manifest");
    bytes
}

#[cfg(unix)]
fn vector_json(name: &str) -> Value {
    serde_json::from_slice(&vector(name)).unwrap()
}

#[cfg(unix)]
#[test]
fn consent_vectors_requests_encode_byte_for_byte() {
    let open = request_params(
        "call-1",
        "read",
        "read",
        "/tmp/file",
        "Inspect file",
        "hash",
        "/tmp/file",
        Some("not_held"),
    );
    for name in ["worker-open-request.json", "replay-request.json"] {
        assert_eq!(
            encode("consent.request", &open).unwrap(),
            vector(name),
            "{name}"
        );
    }
    for (name, id) in [
        ("await-pending-request.json", "acc_2"),
        ("await-settled-request.json", "acc_1"),
    ] {
        assert_eq!(
            encode(
                "consent.await",
                &json!({"elicitation_id": id, "timeout_ms": 0})
            )
            .unwrap(),
            vector(name),
            "{name}"
        );
    }
    assert_eq!(
        encode("consent.withdraw", &json!({"elicitation_id": "acc_2"})).unwrap(),
        vector("withdrawn-request.json")
    );
}

#[cfg(unix)]
#[test]
fn consent_vectors_replies_decode() {
    for name in ["worker-open-reply.json", "replay-reply.json"] {
        let result = decode_result(&vector(name)).unwrap();
        assert_eq!(decode_elicitation_id(&result).unwrap(), "acc_1", "{name}");
    }
    assert_eq!(
        decode_record(&decode_result(&vector("await-pending-reply.json")).unwrap()).unwrap(),
        Settlement::Pending
    );
    assert_eq!(
        decode_record(&decode_result(&vector("await-settled-reply.json")).unwrap()).unwrap(),
        Settlement::Allow {
            scope: true,
            by_user: true
        }
    );
    assert_eq!(
        decode_record(&decode_result(&vector("withdrawn-reply.json")).unwrap()).unwrap(),
        Settlement::Refused("access_withdrawn".into())
    );
    // The head relay's replies embed the ask record, which is still pending
    // in both.
    for name in ["head-not-held-reply.json", "escalated-reply.json"] {
        let result = decode_result(&vector(name)).unwrap();
        assert_eq!(
            decode_record(&result["elicitation"]).unwrap(),
            Settlement::Pending,
            "{name}"
        );
    }
    // The requests these replies answer are the head relay's, not AFT's.
    for name in ["head-not-held-request.json", "escalated-request.json"] {
        assert_eq!(vector_json(name)["method"], "consent.answer", "{name}");
    }
}

#[cfg(unix)]
#[test]
fn consent_vectors_settled_allow_without_answered_by_is_the_head() {
    let mut record = decode_result(&vector("await-settled-reply.json")).unwrap();
    record.as_object_mut().unwrap().remove("answered_by");
    record["outcome"] = json!("allow_once");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Allow {
            scope: false,
            by_user: false
        }
    );
    record["outcome"] = json!("deny");
    record["note"] = json!("not this one");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Deny {
            note: Some("not this one".into())
        }
    );
    record["state"] = json!("refused:access_head_gone");
    record["outcome"] = json!("access_head_gone");
    assert_eq!(
        decode_record(&record).unwrap(),
        Settlement::Refused("access_head_gone".into())
    );
    // Shapes outside the contract are refused: a record without a state, an
    // id under another key, and a reply without the `result` envelope.
    assert!(decode_record(&json!({"outcome": "allow_once"})).is_err());
    assert!(decode_elicitation_id(&json!({"id": "acc_1"})).is_err());
    assert!(decode_result(br#"{"elicitation_id":"acc_1"}"#).is_err());
}

#[cfg(unix)]
#[test]
fn consent_vectors_bind_stamps_route_asks_under_the_worker_stamp() {
    let stamps = vector_json("bind-stamps.json");
    let worker: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["worker"]["scope"].clone()).unwrap();
    let head: subc_protocol::scope::ScopeStamp =
        serde_json::from_value(stamps["head"]["scope"].clone()).unwrap();
    assert_eq!(stamps["worker"]["principal"], "reserved:aft");
    let dir = tempfile::tempdir().unwrap();
    let worker_identity = identity(dir.path(), Some(worker.clone()));
    let head_identity = identity(dir.path(), Some(head));
    assert!(access_grants::alfonso_worker(&worker_identity));
    assert!(!access_grants::alfonso_worker(&head_identity));
    // Asks go out under the asking worker's own stamp.
    let sent = selector(&worker);
    assert_eq!(sent.scope_ref, "worker");
    assert_eq!(sent.scope_epoch, Some(3));
    assert_eq!(sent.owner, worker.owner);
}

fn jail_context(root: &Path, sandbox: bool) -> AppContext {
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.to_path_buf());
    config.storage_dir = Some(root.join(".store"));
    config.sandbox.profile.worker = crate::config::SandboxMode::Strict;
    config.sandbox.profile.head = if sandbox {
        crate::config::SandboxMode::Strict
    } else {
        crate::config::SandboxMode::Off
    };
    AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
}

#[test]
fn consent_worker_jail_never_runs_a_shell_unconfined() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("worktree");
    let task = dir.path().join("task").join("io");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&task).unwrap();
    let ctx = jail_context(&root, false);
    let principal = AuthenticatedPrincipal::RouteBind {
        trust: PrincipalTrust::FirstParty,
        route_channel: 41,
        route_epoch: 1,
        project_root: root.clone(),
        harness: "runner".into(),
        session_id: "worker".into(),
        principal_id: Some("reserved:broca".into()),
    };
    let resolve = |tier| {
        resolve_sandbox_spawn(
            &ctx,
            &principal,
            tier,
            SandboxTaskKind::BashForeground,
            &task,
            None,
        )
    };
    // A head (no jail) keeps the user's choice: sandbox off runs unsandboxed.
    assert!(matches!(
        resolve(RequestedSandboxTier::Disabled),
        SpawnPlan::Unsandboxed
    ));
    for mode in [
        crate::config::SandboxMode::Relaxed,
        crate::config::SandboxMode::Strict,
    ] {
        ctx.update_config(|config| config.sandbox.profile.worker = mode);
        with_worker_jail(true, || {
            // The worker profile is independent of the head profile. Windows
            // relaxed is explicitly unenforced; strict must still fail closed.
            let native = resolve(RequestedSandboxTier::Native);
            #[cfg(windows)]
            {
                assert!(!native.is_native_launcher(), "{native:?}");
                match mode {
                    crate::config::SandboxMode::Relaxed => {
                        assert_eq!(native, SpawnPlan::RelaxedUnenforced);
                    }
                    crate::config::SandboxMode::Strict => {
                        assert_eq!(native.refusal_code(), Some("sandbox_unavailable"));
                    }
                    crate::config::SandboxMode::Off => unreachable!(),
                }
            }
            // macOS/Linux workers must use confinement or refuse the shell.
            #[cfg(not(windows))]
            assert!(
                native.is_native_launcher() || native.refusal_code() == Some("sandbox_unavailable"),
                "{native:?}"
            );
            if let SpawnPlan::Launcher { profile, .. } = &native {
                assert_eq!(profile.mode, mode);
                if mode == crate::config::SandboxMode::Strict {
                    // Writes are confined to the worktree: the directory holding it
                    // (and the sibling worktrees in it) is not writable.
                    let worktree = root.canonicalize().unwrap();
                    let parent = worktree.parent().unwrap();
                    assert!(profile.writable_roots.contains(&worktree), "{profile:?}");
                    assert!(
                        !profile
                            .writable_roots
                            .iter()
                            .any(|writable| parent.starts_with(writable)),
                        "{profile:?}"
                    );
                }
            }
            for tier in [RequestedSandboxTier::Disabled, RequestedSandboxTier::Host] {
                assert_eq!(resolve(tier).refusal_code(), Some("sandbox_unavailable"));
            }
            let untrusted = AuthenticatedPrincipal::RouteBind {
                trust: PrincipalTrust::Untrusted,
                route_channel: 41,
                route_epoch: 1,
                project_root: root.clone(),
                harness: "runner".into(),
                session_id: "worker".into(),
                principal_id: None,
            };
            // Untrusted routes now run under strict confinement instead of being
            // rejected solely for being untrusted. They must never inherit relaxed
            // or off from the role profile.
            let untrusted_plan = resolve_sandbox_spawn(
                &ctx,
                &untrusted,
                RequestedSandboxTier::Native,
                SandboxTaskKind::BashForeground,
                &task,
                None,
            );
            match untrusted_plan {
                SpawnPlan::Launcher { profile, .. } => {
                    assert_eq!(profile.mode, crate::config::SandboxMode::Strict);
                }
                plan => assert_eq!(plan.refusal_code(), Some("sandbox_unavailable")),
            }
        });
    }
    assert!(!crate::sandbox_spawn::worker_jail_active());
}

#[cfg(unix)]
static RERUN_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static RERUN_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Reads normally, except that an approved rerun (one carrying `allow_once`
/// approvals) waits until its job is cancelled, and records whether it was.
#[cfg(unix)]
fn blocking_rerun_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    if let AskRoute::Consent { once } = crate::access_policy::current_ask_route() {
        if !once.is_empty() {
            RERUN_STARTED.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !crate::executor::current_job_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            RERUN_CANCELLED.store(crate::executor::current_job_cancelled(), Ordering::SeqCst);
            return Response::error(&request.id, "request_cancelled", "rerun stopped");
        }
    }
    read_dispatch(request, ctx)
}

#[cfg(unix)]
async fn start_blocked_rerun(f: &Fixture, corr: u64) -> mpsc::Receiver<WriterFrame> {
    RERUN_STARTED.store(false, Ordering::SeqCst);
    RERUN_CANCELLED.store(false, Ordering::SeqCst);
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let replies = f
        .send_with(
            &f.worker,
            &f.head_root.join("shared.txt"),
            corr,
            blocking_rerun_dispatch,
        )
        .await;
    let fake = Arc::clone(&f.fake);
    wait_for(|| !fake.calls("consent.request").is_empty()).await;
    let call_key = fake.calls("consent.request")[0]["call_key"]
        .as_str()
        .unwrap()
        .to_string();
    f.fake.answer(
        &call_key,
        json!({"state": "settled", "outcome": "allow_once", "answered_by": "head"}),
    );
    wait_for(|| RERUN_STARTED.load(Ordering::SeqCst)).await;
    replies
}

#[cfg(unix)]
#[tokio::test]
async fn consent_cancel_reaches_the_approved_rerun() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let _replies = start_blocked_rerun(&f, 7).await;
    // The rerun is the call's active job: a Cancel for the call stops it.
    assert!(cancel_active_tool_call(&f.active, &f.executor, route_key(41, 1), 7, "test").is_some());
    wait_for(|| RERUN_CANCELLED.load(Ordering::SeqCst)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn consent_route_close_reaches_the_approved_rerun() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let _replies = start_blocked_rerun(&f, 7).await;
    note_route_closed(route_key(41, 1));
    wait_for(|| RERUN_CANCELLED.load(Ordering::SeqCst)).await;
    wait_for(|| f.asks().is_empty()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn macos_permission_cards_deduplicate_and_withdraw_on_readable_probe() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let folder = f.dir.path().join("protected");
    let observation = |readable| crate::bounded_io::PermissionObservation {
        folder: folder.clone(),
        readable,
    };
    let db = f.ctx.db().unwrap();
    macos_permissions(Arc::clone(&db), f.head.clone(), vec![observation(false)])
        .await
        .unwrap();
    macos_permissions(Arc::clone(&db), f.worker.clone(), vec![observation(false)])
        .await
        .unwrap();
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["kind"], "macos_permission");
    assert_eq!(requests[0]["folder"], json!(folder));
    assert_eq!(requests[0]["program"], "ck-aft");
    assert!(f.fake.calls("consent.await").is_empty());
    assert!(f.fake.calls("consent.withdraw").is_empty());
    // A readable-again observation withdraws the existing card, even when a
    // different scope created it: the macOS permission covers the same folder.
    macos_permissions(Arc::clone(&db), f.worker.clone(), vec![observation(true)])
        .await
        .unwrap();
    assert_eq!(f.fake.calls("consent.withdraw").len(), 1);
    let count: u64 = db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM macos_permission_cards", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    macos_permissions(db, f.head.clone(), vec![observation(false)])
        .await
        .unwrap();
    assert_eq!(
        f.fake.calls("consent.request").len(),
        2,
        "a later denial creates a new card after withdrawal"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn macos_allowed_read_eperm_returns_named_denial_and_updates_one_card() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"full-access"}}),
        &f.head,
        &f.ctx,
    )
    .unwrap();
    let folder = crate::access_policy::policy_path(&f.dir.path().join("protected"));
    std::fs::create_dir(&folder).unwrap();
    let target = folder.join("file.txt");
    std::fs::write(&target, "protected bytes").unwrap();
    let probe = crate::bounded_io::tests::deny_macos_directory(&folder);
    for corr in [7, 8] {
        let reply = f.read(&f.worker, &target, corr).await;
        assert_eq!(code(&reply), "macos_permission_denied", "{reply}");
        assert!(!text(&reply).contains("protected bytes"));
    }
    wait_for(|| f.fake.calls("consent.request").len() == 1).await;
    assert_eq!(
        f.fake.calls("consent.request")[0]["kind"],
        "macos_permission"
    );
    assert!(f.fake.calls("consent.await").is_empty());
    probe.allow();
    let reply = f.read(&f.worker, &target, 9).await;
    assert!(text(&reply).contains("protected bytes"), "{reply}");
    wait_for(|| f.fake.calls("consent.withdraw").len() == 1).await;
}

#[cfg(unix)]
#[test]
fn consent_vectors_macos_requests_match_closed_decoder() {
    let params = macos_request_params(
        "/protected",
        "Terminal",
        "Open System Settings, Privacy & Security, Files and Folders",
    );
    for name in ["macos-open-request.json", "macos-repeat-request.json"] {
        assert_eq!(
            encode("consent.request", &params).unwrap(),
            vector(name),
            "{name}"
        );
        assert!(matches!(
            FakeAccessRequest::decode(&vector_json(name)["params"]),
            Ok(FakeAccessRequest::MacosPermission { .. })
        ));
    }
    for field in ["call_key", "options", "unknown"] {
        let mut invalid = params.clone();
        invalid[field] = json!("unexpected");
        assert!(
            matches!(FakeAccessRequest::decode(&invalid), Err(ConsentError::Refused { code, .. }) if code == "elicitation_invalid_request"),
            "{invalid}"
        );
    }
    for (field, value) in [
        ("folder", json!("relative")),
        ("folder", json!("/with\0nul")),
        ("program", json!(" ")),
        ("steps", json!(" ")),
        ("steps", Value::Null),
    ] {
        let mut invalid = params.clone();
        invalid[field] = value;
        assert!(
            matches!(FakeAccessRequest::decode(&invalid), Err(ConsentError::Refused { code, .. }) if code == "elicitation_invalid_request"),
            "{invalid}"
        );
    }
}

#[cfg(unix)]
#[test]
fn consent_vectors_macos_phone_card_is_informational() {
    let record = decode_result(&vector("phone-macos-reply.json")).unwrap();
    assert_eq!(record["kind"], "macos_permission");
    assert_eq!(record["options"], json!([]));
    assert_eq!(record["elicitationId"], "acc_3");
    assert!(
        matches!(decode_record(&record), Err(ConsentError::Refused { code, .. }) if code == "access_informational")
    );
}

#[cfg(unix)]
#[test]
fn macos_fake_closed_decoder_deduplicates_and_refuses_await() {
    let fake = FakeCingulate::up();
    let first = fake
        .reply(
            "consent.request",
            &vector_json("macos-open-request.json")["params"],
        )
        .unwrap()
        .unwrap();
    let repeat = fake
        .reply(
            "consent.request",
            &vector_json("macos-repeat-request.json")["params"],
        )
        .unwrap()
        .unwrap();
    assert_eq!(first, repeat);
    let id = decode_elicitation_id(&first).unwrap();
    assert!(
        matches!(fake.reply("consent.await", &json!({"elicitation_id":id,"timeout_ms":0})), Err(ConsentError::Refused { code, .. }) if code == "access_informational")
    );
    let mut unknown = vector_json("macos-open-request.json")["params"].clone();
    unknown["call_key"] = json!("forbidden");
    assert!(
        matches!(fake.reply("consent.request", &unknown), Err(ConsentError::Refused { code, .. }) if code == "elicitation_invalid_request")
    );
    fake.reply("consent.withdraw", &json!({"elicitation_id":id}))
        .unwrap();
    let reopened = fake
        .reply(
            "consent.request",
            &vector_json("macos-open-request.json")["params"],
        )
        .unwrap()
        .unwrap();
    assert_ne!(first, reopened);
}

#[cfg(unix)]
#[tokio::test]
async fn macos_platform_refusal_is_named_and_drops_unpublished_card() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    f.fake.state.lock().unwrap().macos = false;
    let db = f.ctx.db().unwrap();
    let error = macos_permissions(
        Arc::clone(&db),
        f.head.clone(),
        vec![crate::bounded_io::PermissionObservation {
            folder: f.dir.path().join("protected"),
            readable: false,
        }],
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ConsentError::Refused { code, .. } if code == "access_macos_only"));
    assert!(f.fake.calls("consent.await").is_empty());
    let count: u64 = db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM macos_permission_cards", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[cfg(unix)]
struct BlockedMacosCard {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[cfg(unix)]
impl ConsentTransport for BlockedMacosCard {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            assert_eq!(
                call.method, "consent.request",
                "an informational card must never be awaited"
            );
            assert!(matches!(
                FakeAccessRequest::decode(&call.params)?,
                FakeAccessRequest::MacosPermission { .. }
            ));
            started.notify_one();
            release.notified().await;
            Err(fake_refusal(
                "access_macos_only",
                "the service is not running on macOS",
            ))
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn macos_blocked_or_refused_card_does_not_block_or_replace_tool_denial() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"full-access"}}),
        &f.head,
        &f.ctx,
    )
    .unwrap();
    let folder = crate::access_policy::policy_path(&f.dir.path().join("protected"));
    std::fs::create_dir(&folder).unwrap();
    let target = folder.join("file.txt");
    std::fs::write(&target, "protected bytes").unwrap();
    let _probe = crate::bounded_io::tests::deny_macos_directory(&folder);
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    install_test_transport(Some(Arc::new(BlockedMacosCard {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    })));
    let reply = f.read(&f.worker, &target, 7).await;
    assert_eq!(code(&reply), "macos_permission_denied", "{reply}");
    assert!(!text(&reply).contains("protected bytes"));
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("card request starts independently of the completed tool call");
    let db = f.ctx.db().unwrap();
    let count = || {
        db.lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM macos_permission_cards", [], |r| {
                r.get::<_, u64>(0)
            })
            .unwrap()
    };
    assert_eq!(
        count(),
        1,
        "the tool already replied while the card request is blocked"
    );
    release.notify_one();
    wait_for(|| count() == 0).await;
    assert_eq!(code(&reply), "macos_permission_denied");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_pending_messages_pin_worker_and_head_recipients() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = crate::access_policy::policy_path(&f.dir.path().join("elsewhere.txt"));
    let worker = f.read(&f.worker, &target, 71).await;
    assert_eq!(code(&worker), "access_pending", "{worker}");
    assert_eq!(worker["structuredContent"]["message"], format!(
        "Waiting for permission to read {}. The request has gone to your head. End your turn now; you will be resumed with the answer, and then you should run this same call again.",
        target.display()
    ));
    let head = f.read(&f.head, &target, 72).await;
    assert_eq!(code(&head), "access_pending", "{head}");
    assert_eq!(head["structuredContent"]["message"], format!(
        "Waiting for permission to read {}. The request has gone to the user. End your turn now; you will be resumed with the answer, and then you should run this same call again.",
        target.display()
    ));
}

async fn assert_unrepresentable_paths_send_nothing(paths: Vec<PathBuf>) {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    #[cfg(windows)]
    {
        assert!(f
            .head_root
            .starts_with(crate::access_policy::policy_path(f.dir.path())));
        let db = f.ctx.db().unwrap();
        assert_eq!(
            crate::db::access_grants::mode(&db.lock().unwrap(), &access_grants::scope_key(&f.head))
                .unwrap(),
            crate::access_policy::ApprovalMode::Ask
        );
    }
    for (index, path) in paths.into_iter().enumerate() {
        let grant = AccessGrant {
            path: path.clone(),
            kind: crate::access_policy::GrantKind::File,
            operation: AccessOperation::Read,
        };
        let first = Response::error_with_data(
            "unrepresentable",
            crate::access_policy::ACCESS_ASK_REQUIRED,
            "test access check asks",
            json!({"ask":grant}),
        );
        let call = HeldCall {
            reply_deadline: Instant::now() + Duration::from_secs(5),
            identity: f.worker.clone(),
            tool: "read".into(),
            arguments: json!({"filePath":path}),
            request_id: "unrepresentable".into(),
            executor: Arc::clone(&f.executor),
            route: RouteChannel {
                channel: 41,
                epoch: 1,
            },
            corr: 100 + index as u64,
            active: Arc::clone(&f.active),
            connection_cancel: f.connection_cancel.clone(),
            drain: drain::ModuleDrainWindow::default(),
        };
        let (response, _) = run_with_consent(call, (first, String::new()), |_| async {
            panic!("an unrepresentable consent path must never execute");
        })
        .await
        .unwrap();
        assert_eq!(response.data["code"], "access_not_placed");
        assert_eq!(response.data["reason"], "consent_path_unrepresentable");
        assert_eq!(
            response.data["message"],
            format!(
                "Outside access denied: the request for permission to read {} could not be placed.",
                path.display()
            )
        );
    }
    for method in ["consent.request", "consent.await", "consent.withdraw"] {
        assert!(f.fake.calls(method).is_empty(), "{method} must not be sent");
    }
    assert!(f.asks().is_empty());
    assert!(f.worker_grants().is_empty());
}

#[tokio::test]
async fn consent_unrepresentable_drive_and_unc_paths_fail_closed_without_traffic() {
    assert_unrepresentable_paths_send_nothing(vec![
        PathBuf::from(r"C:\outside\file.txt"),
        PathBuf::from(r"\\server\share\file.txt"),
    ])
    .await;
}

#[cfg(windows)]
#[tokio::test]
async fn consent_native_windows_drive_path_is_denied_without_sending_a_request() {
    {
        let _lock = CONSENT_TEST_LOCK.lock().await;
        let f = fixture(FakeCingulate::up());
        let drive = f.head_root.join("shared.txt");
        assert!(drive.is_absolute());
        assert!(!drive.to_string_lossy().starts_with('/'));
        let reply = f.read(&f.worker, &drive, 81).await;
        assert_eq!(code(&reply), "access_not_placed", "{reply}");
        assert_eq!(
            reply["structuredContent"]["reason"],
            "consent_path_unrepresentable"
        );
        assert!(f.fake.calls("consent.request").is_empty());
        assert!(f.fake.calls("consent.await").is_empty());
        assert!(f.asks().is_empty());
        assert!(f.worker_grants().is_empty());
    }
    // UNC coverage enters the same guard without touching a network share.
    assert_unrepresentable_paths_send_nothing(vec![PathBuf::from(r"\\server\share\file.txt")])
        .await;
}
