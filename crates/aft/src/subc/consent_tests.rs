use super::*;

// Consent workflow/vector cases use the consent service's '/'-rooted path
// contract on Unix, pinned to Cingulate 6bb5f4d. Windows drive and UNC paths
// have no agreed representation; fail-closed tests below check zero traffic.

use crate::sandbox_spawn::{
    resolve_sandbox_spawn, with_worker_jail, RequestedSandboxTier, SandboxTaskKind, SpawnPlan,
};

/// Serializes the consent tests: they share the process-wide test transport
/// and test overrides for the answer-hold and directory-probe budgets.
static CONSENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(unix)]
struct DirectoryScopeProbeBudgetOverride(u64);

#[cfg(unix)]
impl DirectoryScopeProbeBudgetOverride {
    fn generous() -> Self {
        Self(DIRECTORY_SCOPE_PROBE_OVERRIDE_MS.swap(10_000, Ordering::SeqCst))
    }
}

#[cfg(unix)]
impl Drop for DirectoryScopeProbeBudgetOverride {
    fn drop(&mut self) {
        DIRECTORY_SCOPE_PROBE_OVERRIDE_MS.store(self.0, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct FakeState {
    up: bool,
    calls: Vec<(String, Value)>,
    /// Elicitation id per consent call key: the same key returns the same ask.
    asks: HashMap<String, String>,
    /// Settled replies per elicitation id.
    answers: HashMap<String, Value>,
    /// Asks withdrawn through `consent.withdraw`: a later `consent.await`
    /// for the same id reports `access_withdrawn`, like Cingulate, so a
    /// sibling covered by a folder grant resumes without a second answer.
    withdrawn: std::collections::HashSet<String>,
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
                if let Some(answer) = state.answers.get(id) {
                    return Ok(Some(answer.clone()));
                }
                if state.withdrawn.contains(id) {
                    return Ok(Some(
                        json!({"state":"refused:access_withdrawn","outcome":"access_withdrawn","code":"access_withdrawn"}),
                    ));
                }
                Ok(None)
            }
            "consent.withdraw" => {
                if let Some(id) = params["elicitation_id"].as_str() {
                    state.cards.retain(|_, card| card != id);
                    // Withdrawing an already-settled ask returns its recorded
                    // outcome (the race guard); only a still-open ask settles
                    // as withdrawn, matching Cingulate's settled-withdraw reply.
                    if let Some(answer) = state.answers.get(id).cloned() {
                        return Ok(Some(answer));
                    }
                    state.withdrawn.insert(id.to_string());
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

// Delay only terminal answers, after receipt by the fake and before application.
const ANSWER_APPLICATION_DELAY: Duration = Duration::ZERO;

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
            if call.method == "consent.await"
                && matches!(decode_record(&result), Ok(settled) if settled != Settlement::Pending)
            {
                tokio::time::sleep(ANSWER_APPLICATION_DELAY).await;
            }
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
        tool_result_body: false,
        disabled_tools: Arc::new(vec![]),
        scope,
        call_mode: subc_protocol::call_mode::CallMode::Classic,
        run_scope: OnceLock::new(),
        made_tool_call: AtomicBool::new(false),
        run_owner: None,
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
    fixture_in(fake, tempfile::tempdir().unwrap())
}

fn fixture_in(fake: Arc<FakeCingulate>, dir: tempfile::TempDir) -> Fixture {
    fixture_with_executor(fake, dir, Arc::new(Executor::new()))
}

fn fixture_with_executor(
    fake: Arc<FakeCingulate>,
    dir: tempfile::TempDir,
    executor: Arc<Executor>,
) -> Fixture {
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
        SETTLED_WAIT_OVERRIDE_MS.store(0, Ordering::SeqCst);
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
        "glob" => crate::commands::glob::handle_glob(&request, ctx),
        "outline" => crate::commands::outline::handle_outline(&request, ctx),
        "zoom" => crate::commands::zoom::handle_zoom(&request, ctx),
        "write" => crate::commands::write::handle_write(&request, ctx),
        other => panic!("unexpected scope test command: {other}"),
    }
}
fn glob_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::glob::handle_glob(&request, ctx)
}

fn grep_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::grep::handle_grep(&request, ctx)
}

fn aft_search_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::semantic_search::handle_semantic_search(&request, ctx)
}

fn head_root_search_preflight(request: RawRequest, ctx: &AppContext) -> Response {
    // Exercise the shared path gate without scanning the host's entire root.
    match ctx.validate_search_path(
        &request.id,
        "head",
        Path::new("/"),
        crate::context::WorkerSearchTool::AftSearch,
    ) {
        Ok(_) => Response::success(&request.id, json!({})),
        Err(response) => response,
    }
}

fn ast_search_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::ast_search::handle_ast_search(&request, ctx)
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
    async fn checkout_tool(
        &self,
        identity: &RouteIdentity,
        name: &str,
        arguments: Value,
        corr: u64,
        dispatch: DispatchFn,
    ) -> Value {
        let mut replies = self
            .send_tool(identity, name, arguments, corr, dispatch)
            .await;
        let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .expect("the call ends with one terminal frame")
            .expect("writer stays open");
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
        self.send_tool_with_writer(identity, name, arguments, corr, dispatch)
            .await
            .1
    }

    async fn send_tool_with_writer(
        &self,
        identity: &RouteIdentity,
        name: &str,
        arguments: Value,
        corr: u64,
        dispatch: DispatchFn,
    ) -> (WriterSender, mpsc::Receiver<WriterFrame>) {
        self.send_tool_options(identity, name, arguments, corr, dispatch, false)
            .await
    }

    async fn send_tool_options(
        &self,
        identity: &RouteIdentity,
        name: &str,
        arguments: Value,
        corr: u64,
        dispatch: DispatchFn,
        preview: bool,
    ) -> (WriterSender, mpsc::Receiver<WriterFrame>) {
        if self.executor.actor_context(&identity.root).is_none() {
            self.executor
                .register_actor(identity.root.clone(), Arc::clone(&self.ctx));
        }
        let routes = HashMap::from([(route_key(41, 1), identity.clone())]);
        let mut body = json!({
            "name": name,
            "preset": if identity.scope.as_ref().is_some_and(|s| s.kind == subc_protocol::scope::ScopeKind::Head) { "head" } else { "worker" },
            "arguments": arguments,
        });
        if preview {
            body["preview"] = json!(true);
        }
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            41,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let (writer, replies) = mpsc::channel(8);
        let (bash_tx, mut bash_rx) = mpsc::channel(8);
        let (touch_tx, _touch_rx) = mpsc::channel(8);
        let (entries, _deferred_rx) = mpsc::unbounded_channel();
        let deferred_tx = DeferredResponseSender {
            entries,
            wake: crate::response_finalize::DeferredResponseWake::default(),
        };
        // As on the frame loop, a scoped worker's saved confinement is read
        // off the loop first when memory cannot answer for it.
        let decoded = DecodedFrame {
            parsed_body: Default::default(),
            confinement: None,
            launch_restore: None,
            codemode_policies: None,
            frame,
            phase_trace: PhaseTrace::new(Instant::now()),
        };
        let mut waits = crate::subc::persistence::DatabaseWaits::default();
        let decoded = match waits.read_confinement(decoded, &routes, &self.executor) {
            Ok(()) => waits.next().await,
            Err(decoded) => decoded,
        };
        let frame = decoded.frame;
        let metrics = Arc::new(DispatchPathMetrics::new());
        let shutdown = Arc::new(Notify::new());
        let mut live_roots = HashMap::new();
        let mut route_bash_cancels = HashMap::new();
        handle_tool_call(
            &writer,
            &frame,
            None,
            decoded.confinement,
            decoded.phase_trace,
            &routes,
            &HashMap::new(),
            &ReclaimedRoutes::default(),
            &mut live_roots,
            &self.executor,
            &self.active,
            &Arc::new(PendingDeferredSetups::default()),
            &shutdown,
            &self.connection_cancel,
            &bash_tx,
            &touch_tx,
            &metrics,
            &mut route_bash_cancels,
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
        let bash_writer = writer.clone();
        tokio::spawn(async move {
            while let Some(completion) = bash_rx.recv().await {
                bash::handle_bash_deferred_completion(
                    &bash_writer,
                    completion,
                    &routes,
                    &mut live_roots,
                    &mut route_bash_cancels,
                    &shutdown,
                    &metrics,
                )
                .await
                .unwrap();
            }
        });
        (writer, replies)
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
        let allowed = self.settled_read(&self.worker, &target, 202).await;
        assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    }

    #[cfg(unix)]
    async fn settled_read(&self, identity: &RouteIdentity, path: &Path, corr: u64) -> Value {
        self.settled_reply(self.send_read(identity, path, corr))
            .await
    }

    #[cfg(unix)]
    async fn settled_reply(
        &self,
        send: impl Future<Output = mpsc::Receiver<WriterFrame>>,
    ) -> Value {
        // The short budget parks unanswered calls, not local answer application.
        set_hold_budget_for_test(None);
        let received = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        install_test_transport(Some(Arc::new(ReceiptGate {
            fake: Arc::clone(&self.fake),
            received: Arc::clone(&received),
            release: Arc::clone(&release),
        })));
        let mut replies = send.await;
        // The receipt callback applies grants or discards declined asks before
        // this signal. Merely storing an answer in the fake is not sufficient.
        let applied = tokio::time::timeout(Duration::from_secs(10), received.notified()).await;
        release.notify_one();
        applied.expect("the settled answer is applied before continuing");
        let allowed = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .expect("the approved read ends with one terminal frame")
            .expect("writer stays open");
        let allowed = serde_json::from_slice(&allowed.body).unwrap();
        install_test_transport(Some(Arc::new(SharedFake(Arc::clone(&self.fake)))));
        set_hold_budget_for_test(Some(Duration::from_millis(100)));
        allowed
    }
}

#[cfg(unix)]
#[path = "consent_rust_sources_tests.rs"]
mod rust_sources;

fn code(reply: &Value) -> &str {
    reply["structuredContent"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn occipital_outside_read_refuses_without_any_permission_ask() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let carrier = |role| {
        let mut carrier = identity(&f.worker.project_root, f.worker.scope.clone());
        let data = Arc::get_mut(&mut carrier.0).unwrap();
        data.role = role;
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
            *principal_id = Some("reserved:occipital".into());
        }
        data.consumer_elicitation_capable = true;
        carrier
    };
    let target = f.head_root.join("shared.txt");
    let v1 = carrier(tool_provider::RouteRole::ToolProviderV1);
    let missing_operation = f.read(&v1, &target, 800).await;
    assert_eq!(
        missing_operation,
        json!({
            "content":[{"type":"text", "text":"rich_ui_op_malformed"}], "isError":true,
            "structuredContent":{"success":false,"code":"rich_ui_op_malformed", "message":"A versioned rich UI operation is required."}
        })
    );

    // V1 requests missing an operation marker never reach filesystem policy.
    // Use the older request format to exercise the read's outside-root refusal
    // and in-root success independently of operation admission.
    let carrier = carrier(tool_provider::RouteRole::Legacy);
    assert!(matches!(
        crate::subc::access_grants::ask_route(&carrier),
        crate::access_policy::AskRoute::Deny
    ));
    let denied = f.read(&carrier, &target, 801).await;
    assert_eq!(denied["isError"], true, "{denied}");
    assert!(!text(&denied).contains("head-owned contents"), "{denied}");
    assert_eq!(code(&denied), "path_outside_root", "{denied}");
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.asks().is_empty());
    assert!(f.worker_grants().is_empty());
    assert!(!text(&denied).contains("permission_required"), "{denied}");

    std::fs::write(carrier.project_root.join("local.txt"), "local-read-marker").unwrap();
    let local = f
        .read(&carrier, &carrier.project_root.join("local.txt"), 802)
        .await;
    assert_eq!(local["isError"], false, "{local}");
    assert!(text(&local).contains("local-read-marker"), "{local}");
}

#[tokio::test]
async fn consent_nonhost_carrier_cannot_save_agent_scope_grants() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let grant = AccessGrant {
        path: f.head_root.join("shared.txt"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let db = f.ctx.db().unwrap();
    crate::db::access_grants::insert(
        &db.lock().unwrap(),
        &access_grants::scope_key(&f.worker),
        &grant,
    )
    .unwrap();
    let arguments = json!({"path": grant.path});
    for principal in [
        "reserved:unrecognized-carrier",
        "reserved:occipital",
        "reserved:basal",
    ] {
        let mut carrier = identity(&f.worker.project_root, f.worker.scope.clone());
        let data = Arc::get_mut(&mut carrier.0).unwrap();
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut data.spawn_principal {
            *principal_id = Some(principal.into());
        }
        if principal == "reserved:basal" {
            let scope = data.scope.as_mut().unwrap();
            scope.kind = subc_protocol::scope::ScopeKind::Ephemeral;
            scope.attributes.flow_id = Some("flow-target-lease".into());
        }
        let call = HeldCall {
            reply_deadline: Instant::now() + Duration::from_secs(5),
            identity: carrier.clone(),
            tool: "read".into(),
            arguments: arguments.clone(),
            request_id: "nonhost-consent".into(),
            executor: f.executor.clone(),
            route: RouteChannel {
                channel: 41,
                epoch: 1,
            },
            corr: 803,
            active: f.active.clone(),
            connection_cancel: f.connection_cancel.clone(),
            drain: drain::ModuleDrainWindow::default(),
        };
        let mut first = Response::error(
            "nonhost-consent",
            crate::access_policy::ACCESS_ASK_REQUIRED,
            "approval required",
        );
        first.data["ask"] = serde_json::to_value(&grant).unwrap();
        let (refusal, _) = tokio::time::timeout(
            Duration::from_secs(2),
            run_with_consent(call, (first, String::new()), |_| async {
                panic!("carrier must not rerun")
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!refusal.success);
        assert_eq!(refusal.data["code"], "path_outside_root");
        assert!(prepare(&db, &f.ctx, &carrier, "read", &arguments, &grant, &grant, None).is_err());
        assert!(f.asks().is_empty());
        assert!(f.fake.calls("consent.request").is_empty());
        let Prep::Ask(prep) = prepare(
            &db, &f.ctx, &f.worker, "read", &arguments, &grant, &grant, None,
        )
        .unwrap() else {
            panic!("host must be able to prepare a consent card");
        };
        assert!(apply_allow(
            &db,
            &f.ctx,
            &carrier,
            &prep.key,
            &prep.call_key,
            "carrier",
            &grant,
            true,
            true,
        )
        .is_err());
        asks::discard(&db.lock().unwrap(), &prep.key).unwrap();
    }
    assert_eq!(f.worker_grants().len(), 1);
}

#[tokio::test]
async fn consent_qualifying_hosts_and_codemode_runs_can_save_scope_grants() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let mut direct = identity(&f.head.project_root, f.head.scope.clone());
    if let AuthenticatedPrincipal::RouteBind { principal_id, .. } =
        &mut Arc::get_mut(&mut direct.0).unwrap().spawn_principal
    {
        *principal_id = Some("direct".into());
    }
    let mut routes = vec![direct, f.worker.clone()];
    for (parent, worker) in [(&f.head, false), (&f.worker, true)] {
        let run_id = if worker { "save-worker" } else { "save-head" };
        let mut run = identity(
            &parent.project_root,
            Some(stamp(
                &format!("codemode/{run_id}"),
                "ephemeral",
                Some(&parent.scope.as_ref().unwrap().scope_ref),
            )),
        );
        let data = Arc::get_mut(&mut run.0).unwrap();
        let scope = data.scope.as_mut().unwrap();
        scope.attributes.run_id = Some(run_id.into());
        scope.attributes.agent_id = (!worker).then(|| "head".into());
        scope.attributes.on_behalf_of_agent = worker.then(|| "head".into());
        let mut run_principal = parent.spawn_principal.clone();
        if let AuthenticatedPrincipal::RouteBind { principal_id, .. } = &mut run_principal {
            *principal_id = Some("reserved:basal".into());
        }
        data.run_owner = Some(Arc::new(codemode::RunOwner {
            kind: if worker {
                codemode::RunKind::Worker {
                    on_behalf_of_agent: "head".into(),
                }
            } else {
                codemode::RunKind::Agent {
                    agent_id: "head".into(),
                }
            },
            run_id: run_id.into(),
            owner_route: parent.clone(),
            owner_route_channel: RouteChannel {
                channel: 41,
                epoch: 1,
            },
            run_route: RouteChannel {
                channel: 42,
                epoch: 1,
            },
            run_principal,
        }));
        access_grants::register(&run, &f.ctx).unwrap();
        routes.push(run);
    }
    let db = f.ctx.db().unwrap();
    for (index, route) in routes.iter().enumerate() {
        let outside = f.dir.path().join(format!("consent-outside-{index}.txt"));
        std::fs::write(&outside, "consent scope grant").unwrap();
        let grant = AccessGrant {
            path: crate::access_policy::policy_path(&outside),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let Prep::Ask(prep) = prepare(
            &db,
            &f.ctx,
            route,
            "read",
            &json!({"path": grant.path}),
            &grant,
            &grant,
            None,
        )
        .unwrap() else {
            panic!("qualifying route must prepare a card");
        };
        let applied = apply_allow(
            &db,
            &f.ctx,
            route,
            &prep.key,
            &prep.call_key,
            &asks::new_holder(),
            &grant,
            true,
            true,
        )
        .unwrap();
        assert!(matches!(applied, Applied::Run(_)));
        assert!(crate::db::access_grants::list(
            &db.lock().unwrap(),
            &access_grants::policy_scope_key(route)
        )
        .unwrap()
        .contains(&grant));
    }
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
        confinement: None,
        launch_restore: None,
        codemode_policies: None,
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
    let allowed = f.settled_read(&f.worker, &target, 8).await;
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
#[test]
fn consent_covering_withdraw_log_names_grant_without_raw_path() {
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(Path::new("/tmp/scope")),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    let ((), lines) = crate::logging::capture_log_lines(|| {
        crate::log_ctx::with_session(Some("worker".to_string()), || {
            log_covered_withdrawn("worker", "acc_718", "acc_717", &grant);
        });
    });
    let covered: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consent covering grant withdrew covered ask"))
        .collect();
    assert_eq!(covered.len(), 1, "{lines:?}");
    let line = covered[0];
    assert!(line.starts_with("INFO "), "{line}");
    assert!(line.contains("ask=acc_718"), "{line}");
    assert!(line.contains("covered_by=acc_717"), "{line}");
    assert!(line.contains("kind=subtree"), "{line}");
    let hash = line.split_once("path_hash=").unwrap().1;
    assert_eq!(hash.len(), 12, "{line}");
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{line}");
    assert!(!line.contains("/tmp/scope"), "{line}");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_folder_grant_settles_and_withdraws_covered_sibling() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let _probe = DirectoryScopeProbeBudgetOverride::generous();
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(5)));
    let sub = f.head_root.join("sharedir");
    std::fs::create_dir_all(&sub).unwrap();
    let a = sub.join("a.txt");
    let b = sub.join("b.txt");
    std::fs::write(&a, "a contents\n").unwrap();
    std::fs::write(&b, "b contents\n").unwrap();
    let mut rx1 = f.send_read(&f.worker, &a, 901).await;
    let mut rx2 = f.send_read(&f.worker, &b, 902).await;
    wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 2);
    let key1 = requests[0]["call_key"].as_str().unwrap().to_string();
    let before: std::collections::HashSet<String> = f
        .asks()
        .into_iter()
        .filter_map(|record| record.elicitation_id)
        .collect();
    assert_eq!(before.len(), 2);
    f.fake.answer(
        &key1,
        json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
    );
    let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
        .await
        .expect("covering call resumes")
        .expect("writer stays open");
    let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .expect("covered sibling resumes without a second answer")
        .expect("writer stays open");
    let v1: Value = serde_json::from_slice(&r1.body).unwrap();
    let v2: Value = serde_json::from_slice(&r2.body).unwrap();
    assert!(text(&v1).contains("a contents"), "{v1}");
    assert!(text(&v2).contains("b contents"), "{v2}");
    assert_eq!(f.worker_grants().len(), 1);
    let grant = &f.worker_grants()[0];
    assert_eq!(grant.kind, GrantKind::Subtree);
    let withdraws = f.fake.calls("consent.withdraw");
    assert_eq!(
        withdraws.len(),
        1,
        "one covered card is withdrawn {withdraws:?}"
    );
    let withdrawn = withdraws[0]["elicitation_id"].as_str().unwrap();
    assert!(
        before.contains(withdrawn),
        "withdrawn ask was pending {withdrawn:?}"
    );
}

/// The call key of the consent request for `file_name`. Parallel calls file
/// their requests in either order, so a test must not take the first one.
#[cfg(unix)]
fn call_key_for_file(requests: &[Value], file_name: &str) -> String {
    requests
        .iter()
        .find(|request| {
            request["paths"][0]
                .as_str()
                .is_some_and(|path| path.ends_with(file_name))
        })
        .unwrap_or_else(|| panic!("no consent request for {file_name}: {requests:?}"))["call_key"]
        .as_str()
        .unwrap()
        .to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn consent_folder_grant_leaves_uncovered_asks_pending() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    // Sibling folder: a grant for one directory never covers another.
    {
        let _probe = DirectoryScopeProbeBudgetOverride::generous();
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_millis(600)));
        let sub_a = f.head_root.join("sub_a");
        let sub_b = f.head_root.join("sub_b");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::create_dir_all(&sub_b).unwrap();
        let a = sub_a.join("a.txt");
        let b = sub_b.join("b.txt");
        std::fs::write(&a, "a contents\n").unwrap();
        std::fs::write(&b, "b contents\n").unwrap();
        let mut rx1 = f.send_read(&f.worker, &a, 911).await;
        let mut rx2 = f.send_read(&f.worker, &b, 912).await;
        wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
        let key1 = call_key_for_file(&f.fake.calls("consent.request"), "a.txt");
        f.fake.answer(
            &key1,
            json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
        );
        let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let v1: Value = serde_json::from_slice(&r1.body).unwrap();
        assert!(text(&v1).contains("a contents"), "{v1}");
        let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        let v2: Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(code(&v2), "access_pending", "sibling stays pending {v2}");
        assert!(
            f.fake.calls("consent.withdraw").is_empty(),
            "no withdraw for sibling"
        );
    }
    // Different operation: a read folder grant never covers a write ask.
    {
        let _probe = DirectoryScopeProbeBudgetOverride::generous();
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_millis(600)));
        let sub = f.head_root.join("opdir");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.txt");
        let b = sub.join("b.txt");
        std::fs::write(&a, "a contents\n").unwrap();
        std::fs::write(&b, "old\n").unwrap();
        let mut rx1 = f.send_read(&f.worker, &a, 913).await;
        let mut rx2 = f
            .send_tool(
                &f.worker,
                "write",
                json!({"path": b, "content": "new\n"}),
                914,
                scope_dispatch,
            )
            .await;
        wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
        let requests = f.fake.calls("consent.request");
        let read_key = requests
            .iter()
            .find(|request| request["operation"] == json!("read"))
            .unwrap()["call_key"]
            .as_str()
            .unwrap()
            .to_string();
        f.fake.answer(
            &read_key,
            json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
        );
        let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let v1: Value = serde_json::from_slice(&r1.body).unwrap();
        assert!(text(&v1).contains("a contents"), "{v1}");
        let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        let v2: Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(code(&v2), "access_pending", "write stays pending {v2}");
        assert!(
            f.fake.calls("consent.withdraw").is_empty(),
            "no withdraw for write"
        );
    }
    // Another session: same scope and path, different harness session.
    {
        let _probe = DirectoryScopeProbeBudgetOverride::generous();
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_millis(600)));
        let sub = f.head_root.join("sessdir");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.txt");
        let b = sub.join("b.txt");
        std::fs::write(&a, "a contents\n").unwrap();
        std::fs::write(&b, "b contents\n").unwrap();
        let mut other = identity(&f.worker.project_root, f.worker.scope.clone());
        let inner = Arc::get_mut(&mut other.0).unwrap();
        inner.session = "other-session".into();
        if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut inner.spawn_principal {
            *session_id = "other-session".into();
        }
        let mut rx1 = f.send_read(&f.worker, &a, 915).await;
        let mut rx2 = f.send_read(&other, &b, 916).await;
        wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
        let key1 = call_key_for_file(&f.fake.calls("consent.request"), "a.txt");
        f.fake.answer(
            &key1,
            json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
        );
        let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let v1: Value = serde_json::from_slice(&r1.body).unwrap();
        assert!(text(&v1).contains("a contents"), "{v1}");
        let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        let v2: Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(
            code(&v2),
            "access_pending",
            "other session stays pending {v2}"
        );
        assert!(
            f.fake.calls("consent.withdraw").is_empty(),
            "no withdraw across sessions"
        );
    }
    // Another epoch: same session and path shape, different scope epoch.
    {
        let _probe = DirectoryScopeProbeBudgetOverride::generous();
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_millis(600)));
        let sub = f.head_root.join("epochdir");
        std::fs::create_dir_all(&sub).unwrap();
        let a = sub.join("a.txt");
        let b = sub.join("b.txt");
        std::fs::write(&a, "a contents\n").unwrap();
        std::fs::write(&b, "b contents\n").unwrap();
        let epoch8: subc_protocol::scope::ScopeStamp = serde_json::from_value(json!({
            "owner": {"kind": "reserved", "module_id": "prefrontal-core"},
            "ref": "worker",
            "scope_epoch": 8,
            "kind": "worker",
            "parent": {"owner": {"kind": "reserved", "module_id": "prefrontal-core"}, "ref": "head", "scope_epoch": 7},
            "owner_authorized": true,
        }))
        .unwrap();
        let other = identity(&f.worker.project_root, Some(epoch8));
        let mut rx1 = f.send_read(&f.worker, &a, 917).await;
        let mut rx2 = f.send_read(&other, &b, 918).await;
        wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
        let key1 = call_key_for_file(&f.fake.calls("consent.request"), "a.txt");
        f.fake.answer(
            &key1,
            json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
        );
        let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let v1: Value = serde_json::from_slice(&r1.body).unwrap();
        assert!(text(&v1).contains("a contents"), "{v1}");
        let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        let v2: Value = serde_json::from_slice(&r2.body).unwrap();
        assert_eq!(
            code(&v2),
            "access_pending",
            "other epoch stays pending {v2}"
        );
        assert!(
            f.fake.calls("consent.withdraw").is_empty(),
            "no withdraw across epochs"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn consent_folder_grant_withdraw_race_uses_recorded_outcome() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let _probe = DirectoryScopeProbeBudgetOverride::generous();
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(5)));
    let sub = f.head_root.join("racedir");
    std::fs::create_dir_all(&sub).unwrap();
    let a = sub.join("a.txt");
    let b = sub.join("b.txt");
    std::fs::write(&a, "a contents\n").unwrap();
    std::fs::write(&b, "b contents\n").unwrap();
    let mut rx1 = f.send_read(&f.worker, &a, 921).await;
    let mut rx2 = f.send_read(&f.worker, &b, 922).await;
    wait_for(|| f.fake.calls("consent.request").len() >= 2).await;
    let requests = f.fake.calls("consent.request");
    let key1 = call_key_for_file(&requests, "a.txt");
    let key2 = call_key_for_file(&requests, "b.txt");
    // Hold the sibling's denial inside its finish so the covering grant's
    // withdraw finds it still live: the withdraw then returns the settled
    // record (like settled-withdraw-reply) and must not override it.
    let gate = FinishGateOverride::install("subc-41-922", false);
    f.fake
        .answer(&key2, json!({"state": "settled", "outcome": "deny"}));
    f.fake.answer(
        &key1,
        json!({"state": "settled", "outcome": "allow_scope", "answered_by": "user"}),
    );
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    // The covering grant is stored while the sibling is still live, so a
    // withdraw is attempted for it even though it already settled.
    wait_for(|| !f.fake.calls("consent.withdraw").is_empty()).await;
    gate.release();
    let r1 = tokio::time::timeout(Duration::from_secs(10), rx1.recv())
        .await
        .unwrap()
        .unwrap();
    let r2 = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
        .await
        .unwrap()
        .unwrap();
    let v1: Value = serde_json::from_slice(&r1.body).unwrap();
    let v2: Value = serde_json::from_slice(&r2.body).unwrap();
    assert!(text(&v1).contains("a contents"), "covering still runs {v1}");
    assert_eq!(
        code(&v2),
        "access_declined",
        "recorded denial wins over covering grant {v2}"
    );
}

struct FinishGate {
    request: String,
    after_claim: bool,
    entered: AtomicBool,
    completed: AtomicBool,
    released: StdMutex<bool>,
    wake: std::sync::Condvar,
}

static FINISH_GATE: StdMutex<Option<Arc<FinishGate>>> = StdMutex::new(None);

#[cfg(unix)]
struct FinishGateOverride(Arc<FinishGate>);

#[cfg(unix)]
impl FinishGateOverride {
    fn install(request: &str, after_claim: bool) -> Self {
        let gate = Arc::new(FinishGate {
            request: request.into(),
            after_claim,
            entered: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            released: StdMutex::new(false),
            wake: std::sync::Condvar::new(),
        });
        assert!(FINISH_GATE
            .lock()
            .unwrap()
            .replace(Arc::clone(&gate))
            .is_none());
        Self(gate)
    }

    fn release(&self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

#[cfg(unix)]
impl Drop for FinishGateOverride {
    fn drop(&mut self) {
        self.release();
        FINISH_GATE.lock().unwrap().take();
    }
}

pub(super) fn wait_at_finish(request: &str, after_claim: bool) {
    let gate = FINISH_GATE.lock().unwrap().clone();
    if let Some(gate) = gate.filter(|gate| gate.request == request) {
        if gate.after_claim == after_claim {
            gate.entered.store(true, Ordering::SeqCst);
            let released = gate.released.lock().unwrap();
            let (_released, timeout) = gate
                .wake
                .wait_timeout_while(released, Duration::from_secs(10), |released| !*released)
                .unwrap();
            assert!(!timeout.timed_out(), "finish gate must be released");
        }
        if after_claim {
            gate.completed.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn consent_deadline_during_settled_application_waits_for_the_real_answer() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for after_claim in [false, true] {
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_secs(1)));
        let gate = FinishGateOverride::install("subc-41-781", after_claim);
        let target = f.head_root.join("shared.txt");
        let mut first = f.send_read(&f.worker, &target, 781).await;
        wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
        let request = &f.fake.calls("consent.request")[0];
        let original = request["call_key"].as_str().unwrap().to_owned();
        f.fake.answer(
            &original,
            json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
        );
        wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;

        SETTLED_DEADLINE_WAITING.store(false, Ordering::SeqCst);
        wait_for(|| SETTLED_DEADLINE_WAITING.load(Ordering::SeqCst) || !first.is_empty()).await;
        assert!(
            first.try_recv().is_err(),
            "a received answer must not promise another resume"
        );
        gate.release();
        let frame = tokio::time::timeout(Duration::from_secs(5), first.recv())
            .await
            .unwrap()
            .unwrap();
        let allowed: Value = serde_json::from_slice(&frame.body).unwrap();
        assert!(
            text(&allowed).contains("head-owned contents"),
            "the received answer must finish the original call: {allowed}"
        );
        assert_eq!(
            f.fake.state.lock().unwrap().asks.len(),
            1,
            "retry must reuse the original ask"
        );
        assert!(f
            .fake
            .calls("consent.request")
            .iter()
            .all(|request| request["call_key"] == original));
        assert!(f.asks().is_empty());
        assert!(f.worker_grants().is_empty());

        let mut second = f.send_read(&f.worker, &target, 783).await;
        wait_for(|| f.fake.state.lock().unwrap().asks.len() == 2).await;
        assert!(
            second.try_recv().is_err(),
            "allow_once cannot run another retry"
        );
        let requests = f.fake.calls("consent.request");
        let new_key = requests.last().unwrap()["call_key"].as_str().unwrap();
        assert_ne!(new_key, original);
        f.fake
            .answer(new_key, json!({"state":"settled", "outcome":"deny"}));
        let frame = tokio::time::timeout(Duration::from_secs(3), second.recv())
            .await
            .unwrap()
            .unwrap();
        let refused: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(code(&refused), "access_declined", "{refused}");
    }
}

static SETTLED_DEADLINE_WAITING: AtomicBool = AtomicBool::new(false);
static SETTLED_WAITER_JOINED: AtomicBool = AtomicBool::new(false);
// Regression checks can disable either synchronization path without rebuilding
// the non-test library.
pub(super) static SETTLED_DEADLINE_ENABLED: AtomicBool = AtomicBool::new(true);
pub(super) static SETTLED_JOIN_ENABLED: AtomicBool = AtomicBool::new(true);
pub(super) static SETTLED_GROUP_RETENTION_ENABLED: AtomicBool = AtomicBool::new(true);
pub(super) static SETTLED_WAIT_OVERRIDE_MS: AtomicU64 = AtomicU64::new(0);

pub(super) fn settled_deadline_waiting() {
    SETTLED_DEADLINE_WAITING.store(true, Ordering::SeqCst);
}

pub(super) fn settled_waiter_joined() {
    SETTLED_WAITER_JOINED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_retry_during_settled_claim_reuses_the_terminal_result() {
    static APPROVED_RUNS: AtomicU64 = AtomicU64::new(0);
    fn counted_read(request: RawRequest, ctx: &AppContext) -> Response {
        let response = read_dispatch(request, ctx);
        if response.success {
            APPROVED_RUNS.fetch_add(1, Ordering::SeqCst);
        }
        response
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(1)));
    let gate = FinishGateOverride::install("subc-41-791", true);
    let target = f.head_root.join("shared.txt");
    APPROVED_RUNS.store(0, Ordering::SeqCst);
    let mut first = f.send_with(&f.worker, &target, 791, counted_read).await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    assert!(f.asks()[0].holder_is_live());
    SETTLED_WAITER_JOINED.store(false, Ordering::SeqCst);
    let mut retry = f.send_with(&f.worker, &target, 792, counted_read).await;
    wait_for(|| SETTLED_WAITER_JOINED.load(Ordering::SeqCst) || !retry.is_empty()).await;
    assert!(
        retry.try_recv().is_err(),
        "a settled holder must not return access_pending"
    );
    gate.release();
    let first = tokio::time::timeout(Duration::from_secs(5), first.recv())
        .await
        .unwrap()
        .unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(5), retry.recv())
        .await
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    let retry: Value = serde_json::from_slice(&retry.body).unwrap();
    assert!(text(&first).contains("head-owned contents"), "{first}");
    assert_eq!(text(&retry), text(&first));
    assert_eq!(f.fake.calls("consent.request").len(), 1);
    assert!(f.asks().is_empty());
    assert!(f.worker_grants().is_empty());
    assert_eq!(
        APPROVED_RUNS.load(Ordering::SeqCst),
        1,
        "share the result, not the one-time authority"
    );
    assert_eq!(
        code(&f.read(&f.worker, &target, 793).await),
        "access_pending"
    );
}

#[cfg(unix)]
struct DelayedSecondConsentReceipt {
    fake: Arc<FakeCingulate>,
    awaits: AtomicU64,
    second_waiting: Arc<Notify>,
    second_received: Arc<Notify>,
    release_second: Arc<Notify>,
}

#[cfg(unix)]
impl ConsentTransport for DelayedSecondConsentReceipt {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        SharedFake(Arc::clone(&self.fake)).call(call)
    }

    fn call_with_receipt(&self, call: ConsentCall, receipt: ConsentReceipt) -> ConsentFuture {
        let second =
            call.method == "consent.await" && self.awaits.fetch_add(1, Ordering::SeqCst) == 1;
        let reply = self.call(call);
        let waiting = Arc::clone(&self.second_waiting);
        let received = Arc::clone(&self.second_received);
        let release = Arc::clone(&self.release_second);
        Box::pin(async move {
            if second {
                waiting.notify_one();
            }
            let record = reply.await?;
            if second {
                received.notify_one();
                release.notified().await;
            }
            receipt(record).await
        })
    }
}

#[cfg(unix)]
async fn complete_overlap_with_delayed_receipt() -> (Fixture, PathBuf) {
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(None);
    let waiting = Arc::new(Notify::new());
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let transport = Arc::new(DelayedSecondConsentReceipt {
        fake: Arc::clone(&f.fake),
        awaits: AtomicU64::new(0),
        second_waiting: Arc::clone(&waiting),
        second_received: Arc::clone(&received),
        release_second: Arc::clone(&release),
    });
    install_test_transport(Some(transport.clone()));
    let target = f.head_root.join("shared.txt");
    let mut first = f.send_read(&f.worker, &target, 821).await;
    wait_for(|| transport.awaits.load(Ordering::SeqCst) >= 1).await;
    let mut second = f.send_read(&f.worker, &target, 822).await;
    tokio::time::timeout(Duration::from_secs(10), waiting.notified())
        .await
        .unwrap();
    let request = &f.fake.calls("consent.request")[0];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    tokio::time::timeout(Duration::from_secs(10), received.notified())
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), first.recv())
        .await
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_slice(&first.body).unwrap();
    assert!(text(&first).contains("head-owned contents"), "{first}");
    // Sending the terminal frame happens after run_with_consent returns and its
    // SettlementOwner drops. The second call's receipt callback is still paused.
    std::fs::write(&target, "contents changed after the approved read\n").unwrap();
    release.notify_one();
    let second = tokio::time::timeout(Duration::from_secs(10), second.recv())
        .await
        .unwrap()
        .unwrap();
    let second: Value = serde_json::from_slice(&second.body).unwrap();
    assert_eq!(
        text(&second),
        text(&first),
        "the delayed overlapping call must retain the original result: {second}"
    );
    assert_eq!(
        f.fake.state.lock().unwrap().asks.len(),
        1,
        "both calls must use one permission ask"
    );
    assert_eq!(
        f.fake.calls("consent.request").len(),
        2,
        "only the two original overlapping calls requested consent"
    );
    assert!(f.asks().is_empty());
    assert!(
        f.worker_grants().is_empty(),
        "sharing a result must not install a standing grant"
    );
    (f, target)
}

#[cfg(unix)]
#[tokio::test]
async fn consent_overlapping_retry_keeps_result_after_settlement_owner_drops() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let _completed = complete_overlap_with_delayed_receipt().await;
}

#[cfg(unix)]
#[tokio::test]
async fn consent_completed_overlap_group_is_not_a_result_cache() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let (f, target) = complete_overlap_with_delayed_receipt().await;
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    let later = f.read(&f.worker, &target, 823).await;
    assert_eq!(
        code(&later),
        "access_pending",
        "a later call needs a new approval: {later}"
    );
    assert_eq!(f.fake.state.lock().unwrap().asks.len(), 2);
    let requests = f.fake.calls("consent.request");
    assert_ne!(
        requests[0]["call_key"],
        requests.last().unwrap()["call_key"]
    );
}

#[test]
fn consent_settled_wait_retry_message_is_exact() {
    assert_eq!(ACCESS_RETRY_MESSAGE, "The permission answer has been received but is still being applied. Retry this same call now; do not wait for another resume.");
}

#[cfg(unix)]
#[tokio::test]
async fn consent_settled_application_bound_says_retry_now_and_preserves_once() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(1)));
    SETTLED_WAIT_OVERRIDE_MS.store(50, Ordering::SeqCst);
    let gate = FinishGateOverride::install("subc-41-801", true);
    let target = f.head_root.join("shared.txt");
    let mut first = f.send_read(&f.worker, &target, 801).await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    let retry = f.read(&f.worker, &target, 802).await;
    assert_eq!(code(&retry), "access_retry", "{retry}");
    assert!(text(&retry).contains(ACCESS_RETRY_MESSAGE), "{retry}");
    let frame = tokio::time::timeout(Duration::from_secs(5), first.recv())
        .await
        .unwrap()
        .unwrap();
    let expired: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(code(&expired), "access_retry", "{expired}");
    assert!(text(&expired).contains(ACCESS_RETRY_MESSAGE), "{expired}");
    let cleanup_pending = f.read(&f.worker, &target, 804).await;
    assert_eq!(code(&cleanup_pending), "access_retry", "{cleanup_pending}");
    assert!(text(&cleanup_pending).contains(ACCESS_RETRY_MESSAGE));
    gate.release();
    wait_for(|| gate.0.completed.load(Ordering::SeqCst)).await;
    wait_for(|| f.asks().iter().all(|ask| !ask.holder_is_live())).await;
    let allowed = f.settled_read(&f.worker, &target, 803).await;
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    assert_eq!(f.fake.state.lock().unwrap().asks.len(), 1);
    assert!(f.asks().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_deadline_during_settled_denial_returns_the_refusal() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(1)));
    let gate = FinishGateOverride::install("subc-41-811", false);
    let target = f.head_root.join("shared.txt");
    let mut first = f.send_read(&f.worker, &target, 811).await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    SETTLED_DEADLINE_WAITING.store(false, Ordering::SeqCst);
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"deny"}),
    );
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    wait_for(|| SETTLED_DEADLINE_WAITING.load(Ordering::SeqCst) || !first.is_empty()).await;
    assert!(
        first.try_recv().is_err(),
        "a received denial must not promise another resume"
    );
    gate.release();
    let frame = tokio::time::timeout(Duration::from_secs(5), first.recv())
        .await
        .unwrap()
        .unwrap();
    let refused: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(code(&refused), "access_declined", "{refused}");
    assert!(f.asks().is_empty());
    assert!(f.worker_grants().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn consent_cancelled_finish_releases_claim_for_identical_retry() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for after_claim in [false, true] {
        cancel_finish_at(after_claim).await;
    }
}

#[cfg(unix)]
async fn cancel_finish_at(after_claim: bool) {
    let mut f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let gate = FinishGateOverride::install("subc-41-761", after_claim);
    let target = f.head_root.join("shared.txt");
    let _first = f.send_read(&f.worker, &target, 761).await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    wait_for(|| gate.0.entered.load(Ordering::SeqCst)).await;
    assert_eq!(f.asks()[0].holder_is_live(), after_claim);

    // Losing the caller drops the receipt future while its blocking database
    // work is still completing. No tool rerun can take ownership of this claim.
    f.connection_cancel.cancel();
    wait_for(|| HELD.lock().unwrap().get(&route_key(41, 1)).is_none()).await;
    gate.release();
    assert_claim_cleared_then_retry(&mut f, &target, 762).await;
}

#[cfg(unix)]
async fn assert_claim_cleared_then_retry(f: &mut Fixture, target: &Path, corr: u64) {
    let cleared = tokio::time::timeout(Duration::from_secs(3), async {
        while !f.asks().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        cleared.is_ok(),
        "cancelled finish left a live holder: {:?}",
        f.asks()
    );

    f.connection_cancel = PersistentCancelSignal::new();
    install_test_transport(Some(Arc::new(SharedFake(Arc::clone(&f.fake)))));
    let mut retry = f.send_read(&f.worker, target, corr).await;
    wait_for(|| f.fake.calls("consent.request").len() == 2).await;
    let request = &f.fake.calls("consent.request")[1];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    let frame = tokio::time::timeout(Duration::from_secs(5), retry.recv())
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&frame.body).unwrap();
    assert!(text(&reply).contains("head-owned contents"), "{reply}");
    assert!(f.asks().is_empty());
    assert!(f.worker_grants().is_empty());
}

/// Models route cleanup after a consent reply has arrived but before the
/// transport returns it to the held tool call.
#[cfg(unix)]
struct ReceiptGate {
    fake: Arc<FakeCingulate>,
    received: Arc<Notify>,
    release: Arc<Notify>,
}

#[cfg(unix)]
impl ConsentTransport for ReceiptGate {
    fn call(&self, call: ConsentCall) -> ConsentFuture {
        self.call_with_receipt(call, Box::new(|record| Box::pin(async { Ok(record) })))
    }

    fn call_with_receipt(&self, call: ConsentCall, receipt: ConsentReceipt) -> ConsentFuture {
        let reply = SharedFake(Arc::clone(&self.fake)).call(call.clone());
        let received = Arc::clone(&self.received);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            let result = receipt(reply.await?).await?;
            if call.method == "consent.await"
                && matches!(decode_record(&result), Ok(settled) if settled != Settlement::Pending)
            {
                received.notify_one();
                release.notified().await;
            }
            Ok(result)
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn consent_cancelled_receipt_releases_claim_before_handoff() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let mut f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_secs(8)));
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    install_test_transport(Some(Arc::new(ReceiptGate {
        fake: Arc::clone(&f.fake),
        received: Arc::clone(&received),
        release: Arc::clone(&release),
    })));
    let target = f.head_root.join("shared.txt");
    let _first = f.send_read(&f.worker, &target, 771).await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_once", "answered_by":"user"}),
    );
    tokio::time::timeout(Duration::from_secs(5), received.notified())
        .await
        .unwrap();
    assert!(f.asks()[0].holder_is_live());
    f.connection_cancel.cancel();
    wait_for(|| HELD.lock().unwrap().get(&route_key(41, 1)).is_none()).await;
    release.notify_one();
    assert_claim_cleared_then_retry(&mut f, &target, 772).await;
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_outline_settlement_precedes_cleanup_and_zoom_via_symlink() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let _probe_budget = DirectoryScopeProbeBudgetOverride::generous();
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(None);
    let target = f.head_root.join("live.rs");
    std::fs::write(&target, "pub fn retained() -> bool { true }\n").unwrap();
    let alias = f.dir.path().join("target");
    std::os::unix::fs::symlink(&f.head_root, &alias).unwrap();
    let received = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    install_test_transport(Some(Arc::new(ReceiptGate {
        fake: Arc::clone(&f.fake),
        received: Arc::clone(&received),
        release: Arc::clone(&release),
    })));
    let mut outline = f
        .send_tool(
            &f.worker,
            "outline",
            json!({"target": alias.join("live.rs")}),
            701,
            scope_dispatch,
        )
        .await;
    wait_for(|| !f.fake.calls("consent.request").is_empty()).await;
    let request = &f.fake.calls("consent.request")[0];
    assert_eq!(request["paths"][0], json!(target));
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_scope"}),
    );
    tokio::time::timeout(Duration::from_secs(5), received.notified())
        .await
        .unwrap();
    let zoom = f
        .tool(
            "zoom",
            json!({"batch":[{"target": target, "symbols":"retained"}]}),
            702,
        )
        .await;
    release.notify_one();
    assert!(text(&zoom).contains("retained"), "{zoom}");
    assert_eq!(
        f.fake.calls("consent.request").len(),
        1,
        "zoom must not file another ask"
    );
    assert_eq!(f.worker_grants().len(), 1);
    let reply = tokio::time::timeout(Duration::from_secs(5), outline.recv())
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&reply.body).unwrap();
    assert!(text(&reply).contains("retained"), "{reply}");
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
async fn consent_scope_write_folder_admits_subfolders_and_refuses_sibling_and_symlink_escape() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(100)));
    let target = f.head_root.join("shared.txt");
    let args = json!({"path": target, "content": "first write\n"});
    let pending = f.tool("write", args.clone(), 301).await;
    assert_eq!(code(&pending), "access_pending", "{pending}");
    // The card offers the file (allow once) and its folder (allow scope).
    let request = f.fake.calls("consent.request")[0].clone();
    assert_eq!(request["operation"], "write");
    assert_eq!(request["paths"], json!([target]));
    assert_eq!(
        request["pattern"],
        json!(format!("{}/**", f.head_root.display()))
    );
    assert_eq!(request["reason"], "Allow scope: read/write access for all AFT file tools within the displayed directory tree. Protected paths remain denied.");
    assert_eq!(request["head_policy"], "allow");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_scope", "answered_by":"user"}),
    );
    let allowed = f
        .settled_reply(f.send_tool(&f.worker, "write", args, 302, scope_dispatch))
        .await;
    assert_ne!(code(&allowed), "access_pending", "{allowed}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first write\n");
    assert_eq!(
        f.worker_grants(),
        vec![AccessGrant {
            path: f.head_root.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        }]
    );
    let asked = f.fake.calls("consent.request").len();

    let nested = f.head_root.join("sub/deeper");
    std::fs::create_dir_all(&nested).unwrap();
    let reply = f
        .tool(
            "write",
            json!({"path": nested.join("new.txt"), "content": "nested write\n"}),
            303,
        )
        .await;
    assert_ne!(code(&reply), "access_pending", "{reply}");
    assert_eq!(
        std::fs::read_to_string(nested.join("new.txt")).unwrap(),
        "nested write\n"
    );
    assert_eq!(f.fake.calls("consent.request").len(), asked);

    let sibling = f.dir.path().join("sibling");
    std::fs::create_dir_all(&sibling).unwrap();
    let reply = f
        .tool(
            "write",
            json!({"path": sibling.join("new.txt"), "content": "sibling write\n"}),
            304,
        )
        .await;
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert!(!sibling.join("new.txt").exists());

    // A link inside the granted folder resolves to its target, which is
    // judged on its own: neither a linked directory nor a linked file
    // outside the folder is covered.
    std::os::unix::fs::symlink(&sibling, f.head_root.join("escape")).unwrap();
    let reply = f
        .tool(
            "write",
            json!({"path": f.head_root.join("escape/through.txt"), "content": "escaped\n"}),
            305,
        )
        .await;
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert!(!sibling.join("through.txt").exists());
    let elsewhere = f.dir.path().join("elsewhere.txt");
    std::os::unix::fs::symlink(&elsewhere, f.head_root.join("link.txt")).unwrap();
    let reply = f
        .tool(
            "write",
            json!({"path": f.head_root.join("link.txt"), "content": "escaped\n"}),
            306,
        )
        .await;
    assert_eq!(code(&reply), "access_pending", "{reply}");
    assert_eq!(
        std::fs::read_to_string(&elsewhere).unwrap(),
        "unheld contents\n"
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
    let allowed = f.settled_read(&f.worker, &target, 202).await;
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
async fn consent_scope_persisted_non_parent_pattern_is_refused() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    for offer_ancestor in [false, true] {
        let f = fixture(FakeCingulate::up());
        set_hold_budget_for_test(Some(Duration::from_millis(100)));
        let target = f.head_root.join("shared.txt");
        assert_eq!(
            code(&f.read(&f.worker, &target, 201).await),
            "access_pending"
        );
        let record = f.asks().pop().unwrap();
        let mut request = record.request.unwrap();
        let directory = if offer_ancestor {
            f.head_root.parent().unwrap().to_path_buf()
        } else {
            f.head_root.join("unrelated")
        };
        request["pattern"] = json!(format!("{}/**", directory.display()));
        {
            let db = f.ctx.db().unwrap();
            asks::open(
                &db.lock().unwrap(),
                &record.key,
                record.call_key.as_deref().unwrap(),
                &record.selector,
                &request,
                &record.pending.unwrap(),
                crate::db::call_ledger::now_ms(),
            )
            .unwrap();
        }
        f.fake.answer(
            record.call_key.as_deref().unwrap(),
            json!({"state":"settled", "outcome":"allow_scope", "answered_by":"head"}),
        );
        let declined = f.settled_read(&f.worker, &target, 202).await;
        assert_eq!(code(&declined), "access_declined", "{declined}");
        assert!(!text(&declined).contains("head-owned contents"));
        assert!(f.worker_grants().is_empty());
        assert!(f.asks().is_empty());
    }
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
    let allowed = f.settled_read(&f.worker, &target, 202).await;
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
    let allowed = f.settled_read(&f.worker, &target, 202).await;
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
    let allowed = f.settled_read(&f.worker, &target, 202).await;
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
        let owner = SettlementOwner::new(&running_call);
        hold(
            &running_call,
            grant,
            Instant::now() + Duration::from_millis(100),
            &route,
            None,
            &owner.0,
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
    let _probe_budget = DirectoryScopeProbeBudgetOverride::generous();
    const HANG_GUARD: Duration = Duration::from_secs(60);
    let f = fixture(FakeCingulate::up());
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .expect("an absolute home path for the scope probe");
    let gate = crate::bounded_io::stall_helper(&home);
    let mut tasks = tokio::task::JoinSet::new();
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
        tasks.spawn(async move {
            let route = RouteHold::new(call.route);
            let owner = SettlementOwner::new(&call);
            let result = tokio::time::timeout(
                HANG_GUARD,
                hold(&call, grant, Instant::now(), &route, None, &owner.0),
            )
            .await
            .expect("concurrent stalled probes must finish before the hang guard");
            assert!(matches!(result, Hold::Answer(_)));
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    let requests = f.fake.calls("consent.request");
    assert_eq!(requests.len(), 72);
    for request in requests {
        assert_eq!(request["pattern"], request["paths"][0]);
        assert_eq!(request["reason"], READ_FILE_SCOPE_REASON);
        assert_eq!(request["head_policy"], "allow");
        assert_eq!(request["operation"], "read");
    }
    gate.reached
        .try_recv()
        .expect("one stalled home probe started");
    assert!(
        gate.reached.try_recv().is_err(),
        "only one helper may probe the home"
    );
    assert!(
        started.elapsed() < HANG_GUARD,
        "concurrent probes must share their wait, not take 72 serial probe budgets"
    );
    let unrelated = f.head_root.join("unrelated.txt");
    std::fs::write(&unrelated, "unrelated read").unwrap();
    let content = crate::bounded_io::run(
        &unrelated,
        Some(Instant::now() + HANG_GUARD),
        std::fs::read_to_string,
    )
    .expect("stalled home probes leave shared filesystem helpers available");
    assert_eq!(content, "unrelated read");
    drop(gate);
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
async fn consent_scope_held_probe_at_settlement_keeps_advertised_tree_and_sibling_access() {
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
    assert_eq!(request["reason"], READ_TREE_SCOPE_REASON);
    assert_eq!(request["head_policy"], "allow");
    // A head may answer after the directory memo expires, while another
    // filesystem probe owns the only permit.
    if let Some(probe) = DIRECTORY_SCOPE_PROBES
        .lock()
        .unwrap()
        .probes
        .get(&f.head_root)
    {
        if let Some((at, _)) = probe.answer.lock().unwrap().as_mut() {
            *at -= DIRECTORY_SCOPE_MEMO_TTL;
        }
    }
    let _permit = DIRECTORY_SCOPE_PROBE_LIMITER
        .try_acquire()
        .expect("the test owns the directory probe permit");
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state":"settled", "outcome":"allow_scope", "answered_by":"head"}),
    );
    let allowed = f.settled_read(&f.worker, &target, 202).await;
    assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
    assert_eq!(
        f.worker_grants(),
        vec![AccessGrant {
            path: f.head_root.clone(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        }]
    );
    let count = f.fake.calls("consent.request").len();
    let sibling = f.head_root.join("sibling.txt");
    std::fs::write(&sibling, "sibling contents").unwrap();
    let reply = f.read(&f.worker, &sibling, 203).await;
    assert!(text(&reply).contains("sibling contents"), "{reply}");
    assert_eq!(f.fake.calls("consent.request").len(), count);
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
    for (kind, path, expected_reason, expected_pattern) in [
        (GrantKind::File, f.head_root.join("shared.txt"),
         "Allow scope: read/write access for all AFT file tools to the displayed file only. Protected paths remain denied.",
         f.head_root.join("shared.txt").to_string_lossy().into_owned()),
        (GrantKind::Subtree, f.head_root.clone(),
         "Allow scope: read/write access for all AFT file tools within the displayed directory tree. Protected paths remain denied.",
         format!("{}/**", f.head_root.display())),
    ] {
        let grant = AccessGrant { path: path.clone(), kind, operation: AccessOperation::Write };
        let Prep::Ask(prep) = prepare(&db, &f.ctx, &f.worker, "write", &json!({"path":path}), &grant, &grant, None).unwrap() else {
            panic!("expected a fresh write card");
        };
        assert_eq!(prep.request["pattern"], expected_pattern);
        assert_eq!(prep.request["reason"], expected_reason);
        assert_eq!(prep.request["operation"], "write");
        assert_eq!(advertised_scope(&prep), Some(grant));
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
    let scope = directory_scope(&original);
    assert_eq!(scope, original);
    assert_eq!(scope_reason(&scope), "Allow scope: read-only access for all AFT read tools to the displayed file only. Protected paths remain denied.");
    assert_eq!(pattern(&scope), original.path.to_string_lossy());
}

/// A write ask for `path` offers the file only. A write in an ordinary nested
/// directory is checked first and must offer its folder, so the file-only
/// answer below comes from the directory rules rather than from write asks
/// never offering a folder.
#[cfg(unix)]
fn assert_file_only_write_scope(path: PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let nested = crate::access_policy::policy_path(&dir.path().join("nested/src"));
    std::fs::create_dir_all(&nested).unwrap();
    let control = directory_scope(&AccessGrant {
        path: nested.join("lib.rs"),
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    });
    assert_eq!(
        control,
        AccessGrant {
            path: nested,
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        }
    );
    let original = AccessGrant {
        path,
        kind: GrantKind::File,
        operation: AccessOperation::Write,
    };
    let scope = directory_scope(&original);
    assert_eq!(scope, original);
    assert_eq!(scope_reason(&scope), "Allow scope: read/write access for all AFT file tools to the displayed file only. Protected paths remain denied.");
    assert_eq!(pattern(&scope), original.path.to_string_lossy());
}

#[cfg(unix)]
fn test_home() -> PathBuf {
    let home = crate::environment::non_empty_os_var("HOME")
        .or_else(|| crate::environment::non_empty_os_var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("the test runner supplies a home directory");
    crate::access_policy::policy_path(&home)
}

#[cfg(unix)]
#[test]
fn consent_scope_write_home_file_stays_file_only() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_write_scope(test_home().join("x"));
}

#[cfg(unix)]
#[test]
fn consent_scope_write_home_ancestor_file_stays_file_only() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let home = test_home();
    let ancestor = home.parent().expect("the home folder has a parent");
    assert_file_only_write_scope(ancestor.join("x"));
}

#[cfg(unix)]
#[test]
fn consent_scope_write_root_file_stays_file_only() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_write_scope(PathBuf::from("/x"));
}

#[cfg(unix)]
#[test]
fn consent_scope_write_root_level_folder_stays_file_only() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_write_scope(crate::access_policy::policy_path(Path::new("/usr")).join("x"));
}

#[cfg(unix)]
#[test]
fn consent_scope_write_system_folder_stays_file_only() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    assert_file_only_write_scope(crate::access_policy::policy_path(Path::new("/etc/hosts")));
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
    let scope = directory_scope(&AccessGrant {
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

#[test]
fn consent_scope_preserves_ordinary_system_name_prefixes() {
    #[cfg(unix)]
    let (home, root, ordinary, system) = (
        Path::new("/srv/users/alice"),
        Path::new("/"),
        vec!["/library/x", "/library/projects", "/libfoo/x"],
        vec!["/lib64/x", "/libx32/x"],
    );
    #[cfg(windows)]
    let (home, root, ordinary, system) = (
        Path::new(r"C:\Users\alice"),
        Path::new(r"C:\"),
        vec![r"C:\Program Files-backup\x", r"C:\PROGRAM FILES ARM\Vendor"],
        vec![r"C:\Program Files (x86)\x", r"C:\pRoGrAm FiLeS\x"],
    );
    for directory in ordinary {
        assert!(
            directory_scope_is_narrow(Path::new(directory), home, root),
            "ordinary folder must remain eligible: {directory}"
        );
    }
    for directory in system {
        assert!(
            !directory_scope_is_narrow(Path::new(directory), home, root),
            "system tree must stay file-only: {directory}"
        );
    }
}

#[test]
fn consent_scope_rejects_system_trees() {
    #[cfg(unix)]
    let (home, root, directories) = (
        Path::new("/srv/users/alice"),
        Path::new("/"),
        vec![
            "/usr/local",
            "/usr/local/share",
            "/etc/ssl",
            "/bin/tools",
            "/sbin/tools",
            "/lib/modules",
            "/lib32/modules",
            "/lib64/modules",
            "/libx32/modules",
            "/libexec/tools",
            "/opt/vendor",
            "/var/log",
            "/var/folders-other/user",
            "/System/Library",
            "/Library/Application Support",
            "/private/etc/ssl",
            "/private/var/db",
        ],
    );
    #[cfg(windows)]
    let (home, root, directories) = (
        Path::new(r"C:\Users\alice"),
        Path::new(r"C:\"),
        vec![
            r"C:\Windows\System32",
            r"C:\wInDoWs\Temp",
            r"C:\Program Files\Vendor",
            r"C:\Program Files (x86)\Vendor",
            r"C:\PROGRAM FILES (X86)\Vendor",
        ],
    );
    for directory in directories {
        assert!(
            !directory_scope_is_narrow(Path::new(directory), home, root),
            "system tree must stay file-only: {directory}"
        );
    }
    #[cfg(unix)]
    let ordinary = [
        "/srv/projects/src",
        "/usr-local/project",
        "/opt-project/src",
        "/var/folders/ab/user/T",
        "/private/var/folders/ab/user/T",
    ];
    #[cfg(windows)]
    let ordinary = [r"C:\Users\alice\Work\src", r"C:\Windows-project\src"];
    for directory in ordinary {
        assert!(
            directory_scope_is_narrow(Path::new(directory), home, root),
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
fn consent_scope_four_concurrent_cards_share_directory_probe() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let _probe_budget = DirectoryScopeProbeBudgetOverride::generous();
    let f = fixture(FakeCingulate::up());
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let probes = Arc::new(AtomicUsize::new(0));
    let threads = ["generate.py", "qr.json", "responder.json", "README.md"].map(|name| {
        let grant = AccessGrant {
            path: f.head_root.join(name),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let barrier = Arc::clone(&barrier);
        let probes = Arc::clone(&probes);
        std::thread::spawn(move || {
            barrier.wait();
            let scope = directory_scope_with(&grant, move |directory, root| {
                if directory.ancestors().last() == Some(root) {
                    probes.fetch_add(1, Ordering::SeqCst);
                    // Keep the first probe open while sibling requests arrive.
                    std::thread::sleep(Duration::from_millis(75));
                }
                Some(false)
            });
            (grant, scope)
        })
    });
    let db = f.ctx.db().unwrap();
    for thread in threads {
        let (grant, scope) = thread.join().unwrap();
        let Prep::Ask(prep) = prepare(
            &db,
            &f.ctx,
            &f.worker,
            "read",
            &json!({"path": grant.path}),
            &grant,
            &scope,
            None,
        )
        .unwrap() else {
            panic!("expected a read card");
        };
        assert_eq!(
            prep.request["pattern"],
            json!(format!("{}/**", f.head_root.display()))
        );
        assert_eq!(prep.request["reason"], READ_TREE_SCOPE_REASON);
    }
    assert_eq!(
        probes.load(Ordering::SeqCst),
        1,
        "all four cards share one probe"
    );
}

#[cfg(unix)]
#[test]
fn consent_scope_sibling_card_reuses_memoized_probe() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let parent = crate::access_policy::policy_path(&dir.path().join("nested"));
    std::fs::create_dir_all(&parent).unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    let mut previous_count = 0;
    for name in ["first.txt", "sibling.txt"] {
        let counter = Arc::clone(&probes);
        let scope = directory_scope_with(
            &AccessGrant {
                path: parent.join(name),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
            move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(false)
            },
        );
        assert_eq!(scope.path, parent);
        assert_eq!(scope.kind, GrantKind::Subtree);
        let count = probes.load(Ordering::SeqCst);
        if previous_count > 0 {
            assert_eq!(
                count, previous_count,
                "sibling must reuse the directory answer"
            );
        }
        previous_count = count;
    }
    assert!(previous_count > 0, "the initial card must probe");
}

#[cfg(unix)]
#[test]
fn consent_scope_unknown_alias_answer_is_not_memoized() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let parent = crate::access_policy::policy_path(&dir.path().join("nested"));
    std::fs::create_dir_all(&parent).unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    for name in ["first.txt", "sibling.txt"] {
        let grant = AccessGrant {
            path: parent.join(name),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        };
        let counter = Arc::clone(&probes);
        assert_eq!(
            directory_scope_with(&grant, move |_, _| {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            }),
            grant
        );
    }
    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "unknown answers must be retried"
    );
    let state = DIRECTORY_SCOPE_PROBES.lock().unwrap();
    assert_eq!(
        state
            .logged
            .keys()
            .filter(|(path, _)| path == &parent)
            .count(),
        1,
        "repeat unknown fallbacks log only once per directory and reason"
    );
}

#[cfg(unix)]
#[test]
fn consent_scope_memo_expires_and_unsafe_answers_keep_each_file() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let parent = crate::access_policy::policy_path(&dir.path().join("nested"));
    std::fs::create_dir_all(&parent).unwrap();
    let grant = AccessGrant {
        path: parent.join("first.txt"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    assert_eq!(
        directory_scope_with(&grant, |_, _| Some(false)).kind,
        GrantKind::Subtree
    );
    let probe = Arc::clone(&DIRECTORY_SCOPE_PROBES.lock().unwrap().probes[&parent]);
    probe.answer.lock().unwrap().as_mut().unwrap().0 -= DIRECTORY_SCOPE_MEMO_TTL;
    let sibling = AccessGrant {
        path: parent.join("sibling.txt"),
        ..grant
    };
    assert_eq!(
        directory_scope_with(&sibling, |_, _| Some(true)),
        sibling,
        "an expired safe answer must not hide a newly unsafe alias"
    );
    let other = AccessGrant {
        path: parent.join("other.txt"),
        ..sibling
    };
    assert_eq!(
        directory_scope_with(&other, |_, _| panic!("unsafe answer should be memoized")),
        other,
        "a memoized file-only answer must keep the current caller's file"
    );
}

#[cfg(unix)]
#[test]
fn consent_scope_waits_for_other_directory_probe_permit() {
    let _lock = CONSENT_TEST_LOCK.blocking_lock();
    let _probe_budget = DirectoryScopeProbeBudgetOverride::generous();
    let dir = tempfile::tempdir().unwrap();
    let parent = crate::access_policy::policy_path(&dir.path().join("nested"));
    std::fs::create_dir_all(&parent).unwrap();
    let permit = DIRECTORY_SCOPE_PROBE_LIMITER.try_acquire().unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(75));
        drop(permit);
    });
    let scope = directory_scope_with(
        &AccessGrant {
            path: parent.join("file.txt"),
            kind: GrantKind::File,
            operation: AccessOperation::Read,
        },
        |_, _| Some(false),
    );
    release.join().unwrap();
    assert_eq!(scope.path, parent);
    assert_eq!(scope.kind, GrantKind::Subtree);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_timed_out_probe_is_not_memoized() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _lock = CONSENT_TEST_LOCK.lock().await;
    assert_eq!(directory_scope_probe_budget(), Duration::from_millis(250));
    let dir = tempfile::tempdir().unwrap();
    let parent = crate::access_policy::policy_path(&dir.path().join("nested"));
    std::fs::create_dir_all(&parent).unwrap();
    let grant = AccessGrant {
        path: parent.join("first.txt"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let probes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&probes);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = std::sync::Mutex::new(release_rx);
    let first = grant.clone();
    let scope = tokio::task::spawn_blocking(move || {
        directory_scope_with(&first, move |_, _| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                release_rx.lock().unwrap().recv().unwrap();
            }
            Some(false)
        })
    })
    .await
    .unwrap();
    assert_eq!(scope, grant, "timeout must fail toward the file-only grant");
    assert_eq!(probes.load(Ordering::SeqCst), 1, "the timed-out probe ran");
    release_tx.send(()).unwrap();
    wait_for_scope_probe_idle().await;
    let count_before = probes.load(Ordering::SeqCst);
    let counter = Arc::clone(&probes);
    let sibling = AccessGrant {
        path: parent.join("sibling.txt"),
        ..grant
    };
    let scope = directory_scope_with(&sibling, move |_, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Some(false)
    });
    assert_eq!(scope.kind, GrantKind::Subtree);
    assert!(
        probes.load(Ordering::SeqCst) > count_before,
        "timeout must not be memoized, even when its helper later succeeds"
    );
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
    let first = directory_scope_with(&grant, move |directory, root| first_check(directory, root));
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
                directory_scope_with(&grant, move |directory, root| check(directory, root))
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
    // holds the process-wide probe slot until it returns. Wait for that helper
    // so the recovery check starts with its full filesystem-probe budget.
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
    let next = directory_scope_with(&grant, move |directory, root| next_check(directory, root));
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
async fn consent_scope_old_root_card_is_refused() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let db = f.ctx.db().unwrap();
    let grant = AccessGrant {
        path: PathBuf::from("/x"),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(mut prep) = prepare(
        &db,
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
    assert_eq!(advertised_scope(&prep), None);
}

#[cfg(unix)]
#[tokio::test]
async fn consent_scope_reconstruction_checks_kind_and_operation() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.head_root.join("shared.txt");
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let directory = AccessGrant {
        path: f.head_root.clone(),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(prep) = prepare(
        &f.ctx.db().unwrap(),
        &f.ctx,
        &f.worker,
        "read",
        &json!({"path": target}),
        &grant,
        &directory,
        None,
    )
    .unwrap() else {
        panic!("expected a directory scope card");
    };
    assert_eq!(advertised_scope(&prep), Some(directory.clone()));
    let mut mismatched = prep.clone();
    mismatched.request["kind"] = json!("command");
    assert_eq!(advertised_scope(&mismatched), None);
    let mut mismatched = prep.clone();
    mismatched.request["operation"] = json!("write");
    assert_eq!(advertised_scope(&mismatched), None);
    // A write file card may advertise its directory too, and settling it
    // stores a write directory grant, never a read one.
    let mut write = prep.clone();
    write.grant.operation = AccessOperation::Write;
    write.request["operation"] = json!("write");
    assert_eq!(
        advertised_scope(&write),
        Some(AccessGrant {
            operation: AccessOperation::Write,
            ..directory
        })
    );
    let mut mismatched = prep;
    mismatched.grant.kind = GrantKind::Subtree;
    assert_eq!(advertised_scope(&mismatched), None);
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

#[tokio::test]
async fn standalone_plugins_receive_in_process_two_phase_ask_without_cingulate() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    for (harness, role) in [
        ("opencode", tool_provider::RouteRole::Legacy),
        ("pi", tool_provider::RouteRole::Legacy),
        ("opencode", tool_provider::RouteRole::ToolProviderV1),
        ("pi", tool_provider::RouteRole::ToolProviderV1),
    ] {
        let mut route = identity(&f.worker.project_root, None);
        let inner = Arc::get_mut(&mut route.0).unwrap();
        inner.harness = harness.into();
        inner.role = role;
        let reply = f.read(&route, &target, 17).await;
        assert_eq!(code(&reply), "permission_required", "{harness}: {reply}");
        assert!(!text(&reply).contains("head-owned contents"));
        let response = &reply["structuredContent"];
        let grant = response["asks"][0]["grant_id"].as_str().unwrap();
        let mut replies = f
            .send_tool(
                &route,
                "read",
                json!({
                    "filePath": target, "permissions_granted": [grant]
                }),
                18,
                read_dispatch,
            )
            .await;
        let frame = tokio::time::timeout(Duration::from_secs(10), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let allowed: Value = serde_json::from_slice(&frame.body).unwrap();
        assert!(text(&allowed).contains("head-owned contents"), "{allowed}");
        assert!(f.fake.calls("consent.request").is_empty());
    }
}

#[tokio::test]
async fn standalone_plugin_exemptions_skip_outside_asks_for_both_protocol_versions() {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let scratch = tempfile::tempdir().unwrap();
    let scratch_file = scratch.path().join("scratch.txt");
    std::fs::write(&scratch_file, "scratch contents").unwrap();
    let storage = f.ctx.storage_dir();
    let paths = task_paths(&storage, "standalone", "bash-5a57a11e01234567").unwrap();
    let mut task = PersistedTask::starting(
        "bash-5a57a11e01234567".into(),
        "standalone".into(),
        "fixture".into(),
        f.worker.project_root.clone(),
        Some(f.worker.project_root.clone()),
        None,
        false,
        false,
    );
    task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &task).unwrap();
    std::fs::write(&paths.stdout, "owned task output").unwrap();
    std::fs::write(&paths.stderr, "").unwrap();
    std::fs::write(&paths.exit, "0\n").unwrap();
    f.ctx
        .bash_background()
        .replay_session(&storage, "standalone")
        .unwrap();
    assert!(!crate::bash_permissions::is_system_temp_path(&paths.stdout));
    for role in [
        tool_provider::RouteRole::Legacy,
        tool_provider::RouteRole::ToolProviderV1,
    ] {
        let mut route = identity(&f.worker.project_root, None);
        let inner = Arc::get_mut(&mut route.0).unwrap();
        inner.harness = "opencode".into();
        inner.role = role;
        for (corr, target, contents) in [
            (71, &scratch_file, "scratch contents"),
            (72, &paths.stdout, "owned task output"),
        ] {
            let reply = f.read(&route, target, corr).await;
            assert_eq!(reply["isError"], false, "{reply}");
            assert!(text(&reply).contains(contents), "{reply}");
            assert_ne!(code(&reply), "permission_required");
        }
    }
    assert!(f.fake.calls("consent.request").is_empty());
}

async fn standalone_reply(replies: &mut mpsc::Receiver<WriterFrame>) -> Value {
    let reply = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice(&reply.body).unwrap()
}

#[tokio::test]
async fn standalone_plugin_tokens_reject_forgery_replay_and_different_capabilities() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    let other = f.head_root.join("other.txt");
    std::fs::write(&other, "other protected contents").unwrap();
    for harness in ["opencode", "pi"] {
        let mut route = identity(&f.worker.project_root, None);
        let inner = Arc::get_mut(&mut route.0).unwrap();
        inner.harness = harness.into();
        inner.role = tool_provider::RouteRole::Legacy;
        let forged = format!(
            "outside:{}",
            serde_json::to_string(&AccessGrant {
                path: target.clone(),
                kind: GrantKind::File,
                operation: AccessOperation::Read
            })
            .unwrap()
        );
        let (_, mut replies) = f
            .send_tool_with_writer(
                &route,
                "read",
                json!({"path":target,"permissions_granted":[forged.clone()]}),
                81,
                read_dispatch,
            )
            .await;
        let ask = standalone_reply(&mut replies).await;
        assert_eq!(code(&ask), "permission_required", "{harness}: {ask}");
        let token = ask["structuredContent"]["asks"][0]["grant_id"]
            .as_str()
            .unwrap();
        assert_ne!(
            token, forged,
            "issued authority must not be the predictable grant encoding"
        );
        let (_, mut replies) = f
            .send_tool_with_writer(
                &route,
                "read",
                json!({"path":other,"permissions_granted":[token]}),
                82,
                read_dispatch,
            )
            .await;
        assert_eq!(
            code(&standalone_reply(&mut replies).await),
            "permission_required"
        );
        let ask = f.read(&route, &target, 83).await;
        let token = ask["structuredContent"]["asks"][0]["grant_id"]
            .as_str()
            .unwrap();
        let (_, mut replies) = f
            .send_tool_with_writer(
                &route,
                "read",
                json!({"path":target,"permissions_granted":[token]}),
                84,
                read_dispatch,
            )
            .await;
        assert!(text(&standalone_reply(&mut replies).await).contains("head-owned contents"));
        let (_, mut replies) = f
            .send_tool_with_writer(
                &route,
                "read",
                json!({"path":target,"permissions_granted":[token]}),
                85,
                read_dispatch,
            )
            .await;
        assert_eq!(
            code(&standalone_reply(&mut replies).await),
            "permission_required"
        );
        #[cfg(unix)]
        {
            let ask = f.read(&route, &target, 88).await;
            let token = ask["structuredContent"]["asks"][0]["grant_id"]
                .as_str()
                .unwrap();
            let (_, mut replies) = f
                .send_tool_with_writer(
                    &route,
                    "write",
                    json!({"path":target,"content":"not approved","permissions_granted":[token]}),
                    89,
                    scope_dispatch,
                )
                .await;
            assert_eq!(
                code(&standalone_reply(&mut replies).await),
                "permission_required"
            );
            assert!(std::fs::read_to_string(&target)
                .unwrap()
                .contains("head-owned contents"));
        }
    }
}

#[tokio::test]
async fn standalone_deferred_bash_carries_claimed_plugin_route() {
    fn routed_bash(request: RawRequest, ctx: &AppContext) -> Response {
        let target = Path::new(request.params["command"].as_str().unwrap());
        if let Err(response) = ctx.validate_read_path(&request.id, request.session(), target) {
            return response;
        }
        Response::success(
            &request.id,
            json!({"output":std::fs::read_to_string(target).unwrap(), "exit_code":0}),
        )
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    let mut route = identity(&f.worker.project_root, None);
    Arc::get_mut(&mut route.0).unwrap().harness = "opencode".into();
    let read_ask = f.read(&route, &target, 96).await;
    let read_token = read_ask["structuredContent"]["asks"][0]["grant_id"]
        .as_str()
        .unwrap();
    let (_, mut replies) = f
        .send_tool_with_writer(
            &route,
            "bash",
            json!({"command":target,"permissions_granted":[read_token]}),
            97,
            routed_bash,
        )
        .await;
    let ask = standalone_reply(&mut replies).await;
    assert_eq!(
        code(&ask),
        "permission_required",
        "a read-tool token must not authorize bash: {ask}"
    );
    let token = ask["structuredContent"]["asks"][0]["grant_id"]
        .as_str()
        .unwrap();
    let (_, mut replies) = f
        .send_tool_with_writer(
            &route,
            "bash",
            json!({"command":target,"permissions_granted":[token]}),
            98,
            routed_bash,
        )
        .await;
    let reply = standalone_reply(&mut replies).await;
    assert_eq!(reply["isError"], false, "{reply}");
    assert!(text(&reply).contains("head-owned contents"), "{reply}");
}

#[tokio::test]
async fn standalone_untrusted_arguments_cannot_replace_a_consumer_approval() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    let mut plugin = identity(&f.worker.project_root, None);
    Arc::get_mut(&mut plugin.0).unwrap().harness = "opencode".into();
    let ask = f.read(&plugin, &target, 86).await;
    let issued = ask["structuredContent"]["asks"][0]["grant_id"]
        .as_str()
        .unwrap();
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    inner.role = tool_provider::RouteRole::Legacy;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"path":target,"permissions_granted":[issued]}),
            87,
            read_dispatch,
        )
        .await;
    let reverse = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reverse.header.ty,
        FrameType::Request,
        "tool arguments cannot stand in for elicitation"
    );
    let answer = Frame::build_with_version(
        reverse.header.ver,
        FrameType::Response,
        reverse.header.flags,
        41,
        1,
        reverse.header.corr,
        serde_json::to_vec(&json!({"decision":"deny"})).unwrap(),
    )
    .unwrap();
    assert!(standalone::reply(&writer, &answer));
    let denied = standalone_reply(&mut replies).await;
    assert_eq!(denied["isError"], true);
    assert!(!text(&denied).contains("head-owned contents"));
}

#[cfg(unix)]
#[tokio::test]
async fn standalone_plugin_preview_renews_approval_once_for_apply() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.harness = "opencode".into();
    inner.role = tool_provider::RouteRole::Legacy;
    let args = json!({"path":target, "content":"approved mutation"});
    let (_, mut replies) = f
        .send_tool_options(&route, "write", args.clone(), 91, scope_dispatch, true)
        .await;
    let ask = standalone_reply(&mut replies).await;
    let token = ask["structuredContent"]["asks"][0]["grant_id"]
        .as_str()
        .unwrap();
    let mut approved = args.clone();
    approved["permissions_granted"] = json!([token]);
    let (_, mut replies) = f
        .send_tool_options(&route, "write", approved, 92, scope_dispatch, true)
        .await;
    let preview = standalone_reply(&mut replies).await;
    assert_eq!(preview["isError"], false, "{preview}");
    assert!(std::fs::read_to_string(&target)
        .unwrap()
        .contains("head-owned contents"));
    let renewal = preview["structuredContent"]["permissions_continuation"][0]
        .as_str()
        .unwrap();
    assert_ne!(renewal, token);
    let mut apply = args;
    apply["permissions_granted"] = json!([renewal]);
    let (_, mut replies) = f
        .send_tool_options(&route, "write", apply.clone(), 93, scope_dispatch, false)
        .await;
    let result = standalone_reply(&mut replies).await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "approved mutation"
    );
    let (_, mut replies) = f
        .send_tool_options(&route, "write", apply, 94, scope_dispatch, false)
        .await;
    assert_eq!(
        code(&standalone_reply(&mut replies).await),
        "permission_required"
    );
}

#[tokio::test]
async fn standalone_reverse_allow_rechecks_cancellation_before_rerun() {
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn counted_read(request: RawRequest, ctx: &AppContext) -> Response {
        RUNS.fetch_add(1, Ordering::SeqCst);
        read_dispatch(request, ctx)
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    RUNS.store(0, Ordering::SeqCst);
    let f = fixture(FakeCingulate::up());
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    inner.role = tool_provider::RouteRole::Legacy;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"path":f.head_root.join("shared.txt")}),
            95,
            counted_read,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let cancel = f.connection_cancel.clone();
    standalone::before_rerun_for_test(Some(Box::new(move || cancel.cancel())));
    let answer = Frame::build_with_version(
        ask.header.ver,
        FrameType::Response,
        ask.header.flags,
        41,
        1,
        ask.header.corr,
        serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
    )
    .unwrap();
    assert!(standalone::reply(&writer, &answer));
    wait_for(|| !active_tool_call_is_registered(&f.active, route_key(41, 1), 95)).await;
    assert_eq!(
        RUNS.load(Ordering::SeqCst),
        1,
        "an allow must not dispatch after cancellation"
    );
}

#[tokio::test]
async fn standalone_no_ui_denies_without_cingulate() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.head_root.join("shared.txt");
    for trust in [BindTrust::FirstParty, BindTrust::Untrusted] {
        let mut route = identity(&f.worker.project_root, None);
        Arc::get_mut(&mut route.0).unwrap().trust = trust;
        let reply = f.read(&route, &target, 19).await;
        if trust == BindTrust::FirstParty {
            assert_eq!(code(&reply), "path_outside_root", "{reply}");
        } else {
            assert_eq!(reply["isError"], true, "{reply}");
            assert!(text(&reply).contains("outside the project root"));
        }
        assert!(!text(&reply).contains("head-owned contents"));
    }
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn audit2_plugin_token_cannot_cross_routes_or_sessions() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_in(
        FakeCingulate::up(),
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
    );
    let target = f.head_root.join("shared.txt");
    let mut a = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut a.0).unwrap();
    inner.harness = "opencode".into();
    inner.role = tool_provider::RouteRole::Legacy;
    let ask = f.read(&a, &target, 181).await;
    assert_eq!(code(&ask), "permission_required");
    let token = ask["structuredContent"]["asks"][0]["grant_id"]
        .as_str()
        .unwrap();
    let mut b = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut b.0).unwrap();
    inner.harness = "opencode".into();
    inner.session = "session-B".into();
    inner.role = tool_provider::RouteRole::Legacy;
    if let AuthenticatedPrincipal::RouteBind { session_id, .. } = &mut inner.spawn_principal {
        *session_id = "session-B".into();
    }
    let (_, mut replies) = f
        .send_tool_with_writer(
            &b,
            "read",
            json!({"path":target,"permissions_granted":[token]}),
            182,
            read_dispatch,
        )
        .await;
    let response = standalone_reply(&mut replies).await;
    assert_eq!(code(&response), "permission_required", "{response}");
    let (_, mut replies) = f
        .send_tool_with_writer(
            &a,
            "read",
            json!({"path":target,"permissions_granted":[token]}),
            183,
            read_dispatch,
        )
        .await;
    assert!(text(&standalone_reply(&mut replies).await).contains("head-owned contents"));
}

#[tokio::test]
async fn standalone_untrusted_reverse_asks_then_reads_once_without_cingulate() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.head_root.join("shared.txt");
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"filePath": target}),
            20,
            read_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    let body: Value = serde_json::from_slice(&ask.body).unwrap();
    assert_eq!(body["method"], BASH_ELICITATION_CREATE_METHOD);
    assert_eq!(body["params"]["_meta"]["aft"]["tool"], "read");
    let answer = Frame::build_with_version(
        ask.header.ver,
        FrameType::Response,
        ask.header.flags,
        41,
        1,
        ask.header.corr,
        serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
    )
    .unwrap();
    let (other, _other_rx) = mpsc::channel(8);
    assert!(
        !standalone::reply(&other, &answer),
        "another connection cannot approve"
    );
    assert!(standalone::reply(&writer, &answer));
    assert!(
        !standalone::reply(&writer, &answer),
        "the approval is single use"
    );
    let result = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.header.ty, FrameType::Response);
    let result: Value = serde_json::from_slice(&result.body).unwrap();
    assert!(text(&result).contains("head-owned contents"), "{result}");
    assert!(f.fake.calls("consent.request").is_empty());
    let (_writer, mut again) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"filePath": target}),
            21,
            read_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), again.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ask.header.ty, FrameType::Request, "a later call asks again");
    f.connection_cancel.cancel();
}

fn write_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    crate::commands::write::handle_write(&request, ctx)
}

/// An MCP app's own approval dialog may approve one outside-project write.
/// The dialog offers no folder choice and the approval covers only this call:
/// nothing is saved as a standing grant, so a second write in the same folder
/// asks again.
#[tokio::test]
async fn standalone_untrusted_reverse_asks_then_writes_once_without_cingulate() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let target = f.head_root.join("written.txt");
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "write",
            json!({"path": target, "content": "approved outside write\n"}),
            30,
            write_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    let body: Value = serde_json::from_slice(&ask.body).unwrap();
    assert_eq!(body["method"], BASH_ELICITATION_CREATE_METHOD);
    assert_eq!(body["params"]["_meta"]["aft"]["tool"], "write");
    assert_eq!(
        body["params"]["_meta"]["aft"]["asks"],
        json!([{"kind": "external_directory", "path": target, "operation": "write"}])
    );
    assert!(
        body["params"]["message"]
            .as_str()
            .is_some_and(|message| message.ends_with(" once?")),
        "{body}"
    );
    assert!(!target.exists(), "nothing is written before the answer");
    assert!(standalone::reply(&writer, &allow_frame(&ask)));
    let result = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.header.ty, FrameType::Response);
    let result: Value = serde_json::from_slice(&result.body).unwrap();
    assert_ne!(result["isError"], json!(true), "{result}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "approved outside write\n"
    );
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.worker_grants().is_empty());

    let second = f.head_root.join("second.txt");
    let (_writer, mut again) = f
        .send_tool_with_writer(
            &route,
            "write",
            json!({"path": second, "content": "unapproved\n"}),
            31,
            write_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), again.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ask.header.ty,
        FrameType::Request,
        "a later write asks again"
    );
    let body: Value = serde_json::from_slice(&ask.body).unwrap();
    assert_eq!(
        body["params"]["_meta"]["aft"]["asks"][0]["path"],
        json!(second)
    );
    assert!(!second.exists());
    f.connection_cancel.cancel();
}

#[tokio::test]
async fn standalone_untrusted_reverse_denial_keeps_path_error() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"filePath": f.head_root.join("shared.txt")}),
            22,
            read_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let answer = Frame::build_with_version(
        ask.header.ver,
        FrameType::Response,
        ask.header.flags,
        41,
        1,
        ask.header.corr,
        serde_json::to_vec(&json!({"decision":"deny"})).unwrap(),
    )
    .unwrap();
    assert!(standalone::reply(&writer, &answer));
    let result = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let result: Value = serde_json::from_slice(&result.body).unwrap();
    assert_eq!(result["isError"], true, "{result}");
    assert!(text(&result).contains("outside-path access was not approved"));
    assert!(!text(&result).contains("head-owned contents"));
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
    assert_eq!(request["pattern"], pattern(&head_grant));
    // The head's only covering grant is revoked while the ask is pending.
    {
        let db = f.ctx.db().unwrap();
        crate::db::access_grants::revoke(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &head_grant.path,
        )
        .unwrap();
        // The head still holds the file, but cannot delegate its former tree.
        crate::db::access_grants::insert(
            &db.lock().unwrap(),
            &access_grants::scope_key(&f.head),
            &AccessGrant {
                path: crate::access_policy::policy_path(&target),
                kind: GrantKind::File,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    f.fake.answer(
        request["call_key"].as_str().unwrap(),
        json!({"state": "settled", "outcome": "allow_scope", "answered_by": "head"}),
    );
    let declined = f.settled_read(&f.worker, &target, 8).await;
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
    let allowed = f.settled_read(&f.worker, &target, 8).await;
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
    let reply = f.settled_read(&f.head, &target, 7).await;
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
    let wire = String::from_utf8(frame.frame.body).unwrap();
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
            matches!(access_grants::ask_route(identity), AskRoute::Consent { .. }),
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
fn consent_vectors_folder_grant_withdraw() {
    // Owner withdraws settle as `access_withdrawn` on AFT's normal route;
    // withdrawing an already-settled ask returns its own settled record for
    // the race guard; a non-owner gets `elicitation_not_owner`.
    for (name, id) in [
        ("scope-owner-withdraw-request.json", "acc_5"),
        ("non-owner-withdraw-request.json", "acc_5"),
        ("settled-withdraw-request.json", "acc_4"),
    ] {
        assert_eq!(
            encode("consent.withdraw", &json!({"elicitation_id": id})).unwrap(),
            vector(name),
            "{name}"
        );
    }
    assert_eq!(
        decode_record(&decode_result(&vector("scope-owner-withdraw-reply.json")).unwrap()).unwrap(),
        Settlement::Refused("access_withdrawn".into())
    );
    assert_eq!(
        decode_record(&decode_result(&vector("settled-withdraw-reply.json")).unwrap()).unwrap(),
        Settlement::Allow {
            scope: false,
            by_user: true
        }
    );
    let refusal: Value =
        serde_json::from_slice(&vector("non-owner-withdraw-refusal.json")).unwrap();
    assert_eq!(refusal["code"], json!("elicitation_not_owner"));
    let notice = vector_json("notice-owner-withdrawn.json");
    assert_eq!(notice["purpose"], json!("withdrawn"));
    assert_eq!(notice["elicitation_id"], json!("acc_5"));
    assert_eq!(notice["asker"]["scope_ref"], json!("worker"));
    assert_eq!(notice["asker"]["scope_epoch"], json!(3));
    let stamps = vector_json("withdraw-bind-stamps.json");
    assert_eq!(
        stamps["scope-owner-withdraw"],
        json!("reserved:prefrontal-core")
    );
    assert_eq!(stamps["non-owner-withdraw"], json!("reserved:basal"));
    assert_eq!(
        stamps["settled-withdraw"],
        json!("reserved:prefrontal-core")
    );
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
    if let AskRoute::Consent { once, .. } = crate::access_policy::current_ask_route() {
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
static ELICITED_RERUN_STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static ELICITED_RERUN_DONE: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static ELICITED_RERUN_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Writes normally, except that a write approved by reverse elicitation (one
/// carrying `once` approvals) first waits up to five seconds for its job to
/// be cancelled. It records whether it was, and writes only if it was not.
#[cfg(unix)]
fn blocking_elicited_write(request: RawRequest, ctx: &AppContext) -> Response {
    if let AskRoute::Elicitation { once } = crate::access_policy::current_ask_route() {
        if !once.is_empty() {
            ELICITED_RERUN_STARTED.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !crate::executor::current_job_cancelled() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let cancelled = crate::executor::current_job_cancelled();
            ELICITED_RERUN_CANCELLED.store(cancelled, Ordering::SeqCst);
            let response = if cancelled {
                Response::error(&request.id, "request_cancelled", "rerun stopped")
            } else {
                crate::commands::write::handle_write(&request, ctx)
            };
            ELICITED_RERUN_DONE.store(true, Ordering::SeqCst);
            return response;
        }
    }
    crate::commands::write::handle_write(&request, ctx)
}

/// An untrusted consumer that approves an outside write by reverse
/// elicitation and then closes its route while the approved write is running
/// gets the write stopped: the job is cancelled, the file is unchanged, and
/// the call is no longer tracked for a later reply.
#[cfg(unix)]
#[tokio::test]
async fn standalone_route_close_cancels_the_approved_rerun() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    ELICITED_RERUN_STARTED.store(false, Ordering::SeqCst);
    ELICITED_RERUN_DONE.store(false, Ordering::SeqCst);
    ELICITED_RERUN_CANCELLED.store(false, Ordering::SeqCst);
    let f = fixture(FakeCingulate::up());
    let corr = 9_100_003;
    let target = f.head_root.join("shared.txt");
    let before = std::fs::read_to_string(&target).unwrap();
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    inner.role = tool_provider::RouteRole::Legacy;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "write",
            json!({"path": target, "content": "approved outside write"}),
            corr,
            blocking_elicited_write,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(standalone::reply(&writer, &allow_frame(&ask)));
    wait_for(|| ELICITED_RERUN_STARTED.load(Ordering::SeqCst)).await;
    note_route_closed(route_key(41, 1));
    wait_for(|| ELICITED_RERUN_DONE.load(Ordering::SeqCst)).await;
    assert!(
        ELICITED_RERUN_CANCELLED.load(Ordering::SeqCst),
        "closing the route must cancel the approved write"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        before,
        "the approved write must not land after the route closed"
    );
    wait_for(|| !active_tool_call_is_registered(&f.active, route_key(41, 1), corr)).await;
    f.connection_cancel.cancel();
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

/// A worker's ask and a head's allow answer both judge the head's policy,
/// which resolves paths on disk. Both must do so with aft.db unlocked, so a
/// slow or stalled filesystem never keeps other routes from the database.
#[tokio::test]
async fn consent_head_policy_resolves_with_database_unlocked() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let db = f.ctx.db().unwrap();
    let probes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let hook = {
        let (probes, db) = (probes.clone(), db.clone());
        access_grants::unlocked_hook::install(move |_| {
            probes.borrow_mut().push(db.try_lock().is_ok())
        })
    };
    let target = f.head_root.join("shared.txt");
    let grant = AccessGrant {
        path: target.clone(),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(prep) = prepare(
        &db,
        &f.ctx,
        &f.worker,
        "read",
        &json!({"path":target}),
        &grant,
        &grant,
        None,
    )
    .unwrap() else {
        panic!("expected a fresh ask");
    };
    assert_eq!(prep.request["head_policy"], "allow", "{}", prep.request);
    let asked = probes.borrow().len();
    assert!(asked > 0, "preparing the ask resolved the head's policy");

    asks::set_elicitation(
        &db.lock().unwrap(),
        &prep.key,
        &prep.call_key,
        "acc_log_granted",
    )
    .unwrap();
    let (applied, lines) = crate::logging::capture_log_lines(|| {
        apply_allow(
            &db,
            &f.ctx,
            &f.worker,
            &prep.key,
            &prep.call_key,
            &asks::new_holder(),
            &prep.grant,
            true,
            false,
        )
    });
    let applied = applied.unwrap();
    assert!(
        matches!(applied, Applied::Run(_)),
        "the head's answer applies"
    );
    let inserted: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("consent grant inserted"))
        .collect();
    assert_eq!(inserted.len(), 1, "{lines:?}");
    let line = inserted[0];
    assert!(
        line.starts_with("INFO ") && line.contains("[ses_worker]"),
        "{line}"
    );
    assert!(
        line.contains("ask=acc_log_granted kind=file scope_epoch=7"),
        "{line}"
    );
    let hash = line.split_once("path_hash=").unwrap().1;
    assert_eq!(hash.len(), 12, "{line}");
    assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{line}");
    assert!(
        !line.contains(target.to_str().unwrap()) && !line.contains("shared.txt"),
        "{line}"
    );
    assert_eq!(f.worker_grants(), vec![grant]);
    drop(hook);
    let probes = probes.borrow();
    assert!(
        probes.len() > asked,
        "applying the head's answer resolved the head's policy"
    );
    assert!(
        probes.iter().all(|unlocked| *unlocked),
        "aft.db was locked during resolution: {probes:?}"
    );
}

/// The head stops holding the asked path while its policy is being resolved
/// for a head's allow answer. The answer must be judged by the head's new
/// policy: declined as no longer held, the ask discarded and no grant stored
/// for the worker, after resolving the changed policy again.
#[tokio::test]
async fn consent_head_revocation_during_resolution_is_judged_by_the_new_policy() {
    use access_grants::unlocked_hook::Point;
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    let directory = f.dir.path().join("delegated");
    std::fs::create_dir(&directory).unwrap();
    let target = directory.join("elsewhere.txt");
    std::fs::write(&target, "unheld contents").unwrap();
    let head_grant = AccessGrant {
        path: crate::access_policy::policy_path(&directory),
        kind: GrantKind::Subtree,
        operation: AccessOperation::Read,
    };
    let db = f.ctx.db().unwrap();
    crate::db::access_grants::insert(
        &db.lock().unwrap(),
        &access_grants::scope_key(&f.head),
        &head_grant,
    )
    .unwrap();
    let grant = AccessGrant {
        path: crate::access_policy::policy_path(&target),
        kind: GrantKind::File,
        operation: AccessOperation::Read,
    };
    let Prep::Ask(prep) = prepare(
        &db,
        &f.ctx,
        &f.worker,
        "read",
        &json!({"path":target}),
        &grant,
        &grant,
        None,
    )
    .unwrap() else {
        panic!("expected a fresh ask");
    };
    // The head holds the path when the ask is prepared.
    assert_eq!(prep.request["head_policy"], "allow", "{}", prep.request);

    let resolves = std::rc::Rc::new(std::cell::Cell::new(0));
    let hook = {
        let (resolves, db) = (resolves.clone(), db.clone());
        let (key, path) = (access_grants::scope_key(&f.head), head_grant.path.clone());
        access_grants::unlocked_hook::install(move |point| {
            if point != Point::Resolve {
                return;
            }
            resolves.set(resolves.get() + 1);
            if resolves.get() == 1 {
                let revoked =
                    crate::db::access_grants::revoke(&db.lock().unwrap(), &key, &path).unwrap();
                assert_eq!(revoked, 1);
            }
        })
    };
    let applied = apply_allow(
        &db,
        &f.ctx,
        &f.worker,
        &prep.key,
        &prep.call_key,
        &asks::new_holder(),
        &prep.grant,
        true,
        false,
    )
    .unwrap();
    drop(hook);
    assert!(
        matches!(applied, Applied::HeadNotHeld),
        "the answer was judged by the stale head policy"
    );
    assert!(resolves.get() >= 2, "the changed policy was resolved again");
    assert!(f.worker_grants().is_empty(), "no worker grant is recorded");
    assert!(f.asks().is_empty(), "the declined ask is discarded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_completed_and_failed_calls_close_scoped_routes() {
    let daemon = crate::synapse_embed::route_tests::Daemon::start(1, false);
    let consumer = Arc::new(
        subc_client_rs::SubcConsumer::connect(
            &daemon.path,
            subc_client_rs::ConsumerOptions::default(),
        )
        .await
        .unwrap(),
    );
    let transport = Arc::new(SubcConsentTransport::new(daemon.path.clone()));
    *transport.consumer.lock().await = Some(Arc::clone(&consumer));
    let call = |method| ConsentCall {
        scope: selector(&stamp("worker", "worker", Some("head"))),
        project_root: PathBuf::from("/project"),
        session: "consent-close".into(),
        method,
        params: json!({}),
        timeout: Duration::from_millis(100),
    };
    assert_eq!(
        transport
            .clone()
            .send(call("consent.request"))
            .await
            .unwrap(),
        json!({"ok":true})
    );
    daemon.wait_closed();
    assert!(matches!(
        transport.clone().send(call("consent.test_error")).await,
        Err(ConsentError::Refused { .. })
    ));
    daemon.wait_closed();
    assert!(matches!(
        transport.clone().send(call("consent.test_timeout")).await,
        Err(ConsentError::Unavailable(_))
    ));
    daemon.wait_closed();
    assert_eq!(daemon.counts.0.lock().unwrap().opens, 3);
    assert!(transport.routes.lock().unwrap().is_empty());
    daemon.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_cancelled_call_releases_route_after_last_overlapping_call() {
    let daemon = crate::synapse_embed::route_tests::Daemon::start(1, false);
    let consumer = Arc::new(
        subc_client_rs::SubcConsumer::connect(
            &daemon.path,
            subc_client_rs::ConsumerOptions::default(),
        )
        .await
        .unwrap(),
    );
    let transport = Arc::new(SubcConsentTransport::new(daemon.path.clone()));
    *transport.consumer.lock().await = Some(consumer);
    let call = ConsentCall {
        scope: selector(&stamp("worker", "worker", Some("head"))),
        project_root: PathBuf::from("/project"),
        session: "consent-overlap".into(),
        method: "consent.test_timeout",
        params: json!({}),
        timeout: Duration::from_secs(5),
    };
    let first = tokio::spawn(transport.clone().send(call.clone()));
    let second = tokio::spawn(transport.clone().send(call));
    {
        let (counts, _) = daemon
            .counts
            .1
            .wait_timeout_while(
                daemon.counts.0.lock().unwrap(),
                Duration::from_secs(2),
                |counts| counts.queries < 2,
            )
            .unwrap();
        assert_eq!(counts.queries, 2);
        assert_eq!(
            counts.opens, 1,
            "overlapping calls should lease the SDK's cached route"
        );
    }
    first.abort();
    let _ = first.await;
    // A completed sibling must not close the route of the still-held await.
    let sibling = ConsentCall {
        scope: selector(&stamp("worker", "worker", Some("head"))),
        project_root: PathBuf::from("/project"),
        session: "consent-overlap".into(),
        method: "consent.withdraw",
        params: json!({}),
        timeout: Duration::from_secs(1),
    };
    transport.clone().send(sibling).await.unwrap();
    assert_eq!(daemon.counts.0.lock().unwrap().active, 1);

    assert_eq!(transport.routes.lock().unwrap().len(), 1);
    assert!(!second.is_finished());
    second.abort();
    let _ = second.await;
    daemon.wait_closed();
    assert_eq!(daemon.counts.0.lock().unwrap().closes, 1);
    daemon.finish();
}

fn transport_consent_call(worker: &str, method: &'static str) -> ConsentCall {
    ConsentCall {
        scope: selector(&stamp(worker, "worker", Some("head"))),
        project_root: PathBuf::from("/project"),
        session: "consent-isolation".into(),
        method,
        params: json!({}),
        timeout: Duration::from_secs(10),
    }
}

async fn authenticated_consent_transport(
    daemon: &crate::synapse_embed::route_tests::Daemon,
) -> Arc<SubcConsentTransport> {
    let consumer = subc_client_rs::SubcConsumer::connect(
        &daemon.path,
        subc_client_rs::ConsumerOptions::default(),
    )
    .await
    .unwrap();
    let transport = Arc::new(SubcConsentTransport::new(daemon.path.clone()));
    *transport.consumer.lock().await = Some(Arc::new(consumer));
    transport
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_real_transport_applies_receipt_before_releasing_lease() {
    let daemon = crate::synapse_embed::route_tests::Daemon::start(1, false);
    let transport = authenticated_consent_transport(&daemon).await;
    let call = transport_consent_call("receipt-worker", "consent.await");
    let bind = subc_protocol::BindIdentity::new(
        call.project_root.clone(),
        "aft-consent",
        call.session.clone(),
    );
    let route = transport.route_identity(ConsentRouteKey::new(&bind, &call.scope));
    let state = Arc::clone(&route.state);
    let observed = Arc::new(AtomicBool::new(false));
    let received = Arc::clone(&observed);
    let reply = transport
        .clone()
        .send_with_receipt(
            call,
            Some(Box::new(move |record| {
                Box::pin(async move {
                    // Inspect the production lease, not a stand-in's cleanup ordering.
                    // release_route removes it before closing the scoped daemon handle.
                    assert_eq!(
                        state.leases.lock().unwrap().len(),
                        1,
                        "receipt must precede route release"
                    );
                    received.store(true, Ordering::SeqCst);
                    Ok(record)
                })
            })),
        )
        .await
        .unwrap();
    assert_eq!(reply, json!({"ok":true}));
    assert!(observed.load(Ordering::SeqCst));
    assert!(route.state.leases.lock().unwrap().is_empty());
    drop(route);
    daemon.wait_closed();
    assert!(transport.routes.lock().unwrap().is_empty());
    daemon.finish();
}

async fn wait_consent_counts(
    daemon: &crate::synapse_embed::route_tests::Daemon,
    ready: impl Fn(&crate::synapse_embed::route_tests::Counts) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if ready(&daemon.counts.0.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("expected consent traffic did not reach the authenticated daemon");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_held_open_does_not_block_another_scope_open_and_close() {
    let held_call = transport_consent_call("worker-a", "consent.request");
    let daemon = crate::synapse_embed::route_tests::Daemon::with_held_consent_open(
        serde_json::to_value(&held_call.scope).unwrap(),
    );
    let transport = authenticated_consent_transport(&daemon).await;
    let held = tokio::spawn(transport.clone().send(held_call));
    wait_consent_counts(&daemon, |counts| counts.opens == 1).await;

    let other = tokio::time::timeout(
        Duration::from_secs(2),
        transport
            .clone()
            .send(transport_consent_call("worker-b", "consent.request")),
    )
    .await;
    // Keep the open reply gated until the independent call has finished and
    // its Goodbye has reached the daemon, not merely until it has an answer.
    assert!(other.is_ok(), "another scope stalled behind the held open");
    assert_eq!(other.unwrap().unwrap(), json!({"ok":true}));
    wait_consent_counts(&daemon, |counts| counts.closes == 1).await;
    assert!(!held.is_finished(), "the route.open reply was not held");
    assert_eq!(daemon.counts.0.lock().unwrap().opens, 2);
    assert_eq!(daemon.counts.0.lock().unwrap().active, 1);

    daemon.release_consent_open();
    assert_eq!(held.await.unwrap().unwrap(), json!({"ok":true}));
    daemon.wait_closed();
    assert_eq!(daemon.counts.0.lock().unwrap().closes, 2);
    assert!(transport.routes.lock().unwrap().is_empty());
    daemon.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_answered_call_returns_while_another_scope_open_is_held() {
    let held_call = transport_consent_call("worker-a", "consent.request");
    let daemon = crate::synapse_embed::route_tests::Daemon::with_held_consent_open(
        serde_json::to_value(&held_call.scope).unwrap(),
    );
    let transport = authenticated_consent_transport(&daemon).await;
    let answered = tokio::spawn(
        transport
            .clone()
            .send(transport_consent_call("worker-b", "consent.test_held")),
    );
    wait_consent_counts(&daemon, |counts| counts.queries == 1).await;
    let held = tokio::spawn(transport.clone().send(held_call));
    wait_consent_counts(&daemon, |counts| counts.opens == 2).await;
    daemon.release_replies();

    let result = tokio::time::timeout(Duration::from_secs(2), answered)
        .await
        .expect("an answered call could not release its route behind another scope's open");
    assert_eq!(result.unwrap().unwrap(), json!({"ok":true}));
    wait_consent_counts(&daemon, |counts| counts.closes == 1).await;
    assert!(!held.is_finished(), "the route.open reply was not held");
    assert_eq!(daemon.counts.0.lock().unwrap().active, 1);

    daemon.release_consent_open();
    held.await.unwrap().unwrap();
    daemon.wait_closed();
    assert_eq!(daemon.counts.0.lock().unwrap().closes, 2);
    assert!(transport.routes.lock().unwrap().is_empty());
    daemon.finish();
}

#[cfg(unix)]
#[tokio::test]
async fn scratch_read_worker_route_skips_cingulate_only_for_own_task() {
    use crate::bash_background::persistence::{create_task_layout, write_task_at, PersistedTask};
    use crate::bash_background::BgTaskStatus;

    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(50)));
    let storage = f.ctx.config().storage_dir.clone().unwrap();
    let mut roots = Vec::new();
    for (session, id) in [
        ("worker", "bash-0123456789abcdef"),
        ("other-session", "bash-fedcba9876543210"),
    ] {
        // Allocate the bundle through the locked production path so the
        // session directory is non-empty before the detached persisted-task
        // GC from the first replay can see it. A bare task_paths + write_task
        // leaves an empty session directory that the sweep can rmdir between
        // creation and the first file write.
        let task = create_task_layout(&storage, session, id).unwrap();
        let paths = &task.paths;
        let mut metadata = PersistedTask::starting(
            id.into(),
            session.into(),
            "echo scratch".into(),
            f.worker.project_root.clone(),
            Some(f.worker.project_root.clone()),
            None,
            false,
            false,
        );
        metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
        write_task_at(&task, &metadata).unwrap();
        std::fs::write(&paths.stdout, "task output\n").unwrap();
        std::fs::write(&paths.stderr, "").unwrap();
        std::fs::write(&paths.exit, "0\n").unwrap();
        f.ctx
            .bash_background()
            .replay_session(&storage, session)
            .unwrap();
        let scratch = f
            .dir
            .path()
            .canonicalize()
            .unwrap()
            .join(format!(".sandbox-tmp-{session}"));
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("scratch.rs"), "fn scratch_marker() {}\n").unwrap();
        f.ctx
            .bash_background()
            .task_for_test(id)
            .unwrap()
            .lock_state()
            .unwrap()
            .metadata
            .sandbox_temp_dir = Some(scratch.clone());
        roots.push(scratch);
    }
    let own = f.read(&f.worker, &roots[0].join("scratch.rs"), 701).await;
    assert!(text(&own).contains("scratch_marker"), "{own}");
    for (name, args) in [
        (
            "grep",
            json!({"path": roots[0], "pattern": "scratch_marker"}),
        ),
        ("glob", json!({"path": roots[0], "pattern": "*.rs"})),
    ] {
        let own = f.tool(name, args, 702).await;
        assert!(text(&own).contains("scratch.rs"), "{name}: {own}");
    }
    assert!(f.fake.calls("consent.request").is_empty());
    assert!(f.asks().is_empty());
    let foreign = f.read(&f.worker, &roots[1].join("scratch.rs"), 703).await;
    assert_eq!(code(&foreign), "access_pending", "{foreign}");
    for (name, args) in [
        (
            "grep",
            json!({"path": roots[1], "pattern": "scratch_marker"}),
        ),
        ("glob", json!({"path": roots[1], "pattern": "*.rs"})),
    ] {
        let foreign = f.tool(name, args, 704).await;
        assert_eq!(code(&foreign), "access_pending", "{name}: {foreign}");
    }
    assert_eq!(f.fake.calls("consent.request").len(), 3);
    let write = f
        .tool(
            "write",
            json!({"filePath": roots[0].join("scratch.rs"), "content": "overwrite"}),
            705,
        )
        .await;
    assert_eq!(code(&write), "access_pending", "{write}");
    assert_eq!(
        std::fs::read_to_string(roots[0].join("scratch.rs")).unwrap(),
        "fn scratch_marker() {}\n"
    );
}

fn worker_filesystem_root(f: &Fixture) -> &Path {
    f.head_root.ancestors().last().unwrap()
}

fn assert_worker_outside_read_denied(f: &Fixture, reply: &Value) {
    #[cfg(not(windows))]
    {
        assert_eq!(code(reply), "access_pending", "{reply}");
        assert_eq!(f.fake.calls("consent.request").len(), 1);
    }
    #[cfg(windows)]
    {
        // The consent wire contract accepts slash-rooted paths, not drive paths.
        // Native Windows requests must fail closed before any ask is placed.
        assert_eq!(code(reply), "access_not_placed", "{reply}");
        assert_eq!(
            reply["structuredContent"]["reason"],
            "consent_path_unrepresentable"
        );
        for method in ["consent.request", "consent.await", "consent.withdraw"] {
            assert!(f.fake.calls(method).is_empty(), "{method}");
        }
        assert!(f.asks().is_empty());
        assert!(f.worker_grants().is_empty());
    }
}

#[tokio::test]
async fn consent_worker_glob_suggests_existing_checkout_path_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let checkout = f.ctx.config().project_root.clone().unwrap();
    let name = format!(
        "worker-path-{}",
        f.dir.path().file_name().unwrap().to_string_lossy()
    );
    let requested = worker_filesystem_root(&f).join(&name);
    let counterpart = checkout.join(&name);
    assert!(std::fs::symlink_metadata(&requested).is_err());
    std::fs::create_dir_all(&counterpart).unwrap();
    std::fs::write(counterpart.join("match.rs"), "fn match_here() {}\n").unwrap();

    let reply = f
        .checkout_tool(
            &f.worker,
            "glob",
            json!({"pattern": "*.rs", "path": requested}),
            7,
            glob_dispatch,
        )
        .await;

    assert_eq!(code(&reply), "path_not_found_did_you_mean", "{reply}");
    assert!(!text(&reply).contains(r"\\?\"), "{reply}");
    assert_eq!(
        text(&reply),
        format!(
            "path not found: {}; did you mean {}?",
            requested.display(),
            counterpart.display()
        )
    );
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn consent_worker_existing_outside_path_still_asks_once() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let target = f.dir.path().join("elsewhere.txt");

    let reply = f.read(&f.worker, &target, 7).await;

    assert_worker_outside_read_denied(&f, &reply);
}

#[tokio::test]
async fn consent_worker_missing_path_without_checkout_counterpart_still_asks() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let name = format!(
        "missing-worker-path-{}",
        f.dir.path().file_name().unwrap().to_string_lossy()
    );
    let target = worker_filesystem_root(&f).join(name);
    assert!(std::fs::symlink_metadata(&target).is_err());

    let reply = f.read(&f.worker, &target, 7).await;

    assert_worker_outside_read_denied(&f, &reply);
    assert!(!text(&reply).contains("did you mean"));
}

#[tokio::test]
async fn consent_worker_dotdot_and_symlink_escapes_are_not_suggested() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let checkout = f.ctx.config().project_root.clone().unwrap();
    let name = format!(
        "escaped-worker-path-{}",
        f.dir.path().file_name().unwrap().to_string_lossy()
    );
    let outside = checkout.parent().unwrap().join(&name);
    std::fs::write(&outside, "outside the checkout\n").unwrap();
    let requested = worker_filesystem_root(&f).join("..").join(&name);
    let normalized_requested = worker_filesystem_root(&f).join(&name);
    assert!(std::fs::symlink_metadata(&normalized_requested).is_err());

    let dotdot_reply = f.read(&f.worker, &requested, 7).await;
    assert_worker_outside_read_denied(&f, &dotdot_reply);
    assert!(!text(&dotdot_reply).contains("did you mean"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let link_name = format!(
            "worker-path-link-{}",
            f.dir.path().file_name().unwrap().to_string_lossy()
        );
        symlink(&outside, checkout.join(&link_name)).unwrap();
        let symlink_reply = f.read(&f.worker, &Path::new("/").join(link_name), 8).await;
        assert_eq!(code(&symlink_reply), "access_pending", "{symlink_reply}");
        assert!(!text(&symlink_reply).contains("did you mean"));
        assert_eq!(f.fake.calls("consent.request").len(), 2);
    }
}

#[tokio::test]
async fn consent_worker_searches_of_filesystem_root_refuse_without_asking() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    set_hold_budget_for_test(Some(Duration::from_millis(300)));
    let checkout = f.ctx.config().project_root.clone().unwrap();
    let expected = format!("use your checkout instead: {}", checkout.display());
    let cases: [(&str, Value, DispatchFn); 4] = [
        (
            "glob",
            json!({"pattern": "*.rs", "path": "/"}),
            glob_dispatch,
        ),
        (
            "grep",
            json!({"pattern": "needle", "path": "/"}),
            grep_dispatch,
        ),
        (
            "search",
            json!({"query": "needle", "path": "/"}),
            aft_search_dispatch,
        ),
        (
            "ast_search",
            json!({"pattern": "console.log($MSG)", "lang": "javascript", "paths": ["/"]}),
            ast_search_dispatch,
        ),
    ];

    for (index, (name, arguments, dispatch)) in cases.into_iter().enumerate() {
        let reply = f
            .checkout_tool(&f.worker, name, arguments, 7 + index as u64, dispatch)
            .await;
        assert_eq!(code(&reply), "worker_root_search", "{name}: {reply}");
        assert_eq!(
            reply["structuredContent"]["message"].as_str().unwrap_or(""),
            expected,
            "{name}: {reply}"
        );
    }
    assert!(f.fake.calls("consent.request").is_empty());
}

#[tokio::test]
async fn consent_stamped_head_search_of_filesystem_root_keeps_todays_behavior() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture(FakeCingulate::up());
    // The general fixture explicitly selects ask mode. This case checks the
    // full-access head's exemption from worker-only search guidance.
    remote_policy::catalog(
        json!({"preset":"head","params":{"mode":"full-access"}}),
        &f.head,
        &f.ctx,
    )
    .unwrap();

    let reply = f
        .checkout_tool(
            &f.head,
            "search",
            json!({"query": "needle", "path": "/"}),
            7,
            head_root_search_preflight,
        )
        .await;

    assert_eq!(reply["structuredContent"]["success"], true, "{reply}");
    assert!(f.fake.calls("consent.request").is_empty());
}

/// Removes per-call test overrides when a test ends.
struct CallOverrides(u64);
impl Drop for CallOverrides {
    fn drop(&mut self) {
        crate::subc::PLAIN_RUN_IO_BUDGET_FOR_TEST
            .lock()
            .unwrap()
            .remove(&self.0);
        standalone::APPROVAL_WAIT_FOR_TEST
            .lock()
            .unwrap()
            .remove(&self.0);
    }
}

fn allow_frame(ask: &WriterFrame) -> Frame {
    Frame::build_with_version(
        ask.header.ver,
        FrameType::Response,
        ask.header.flags,
        41,
        1,
        ask.header.corr,
        serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
    )
    .unwrap()
}

/// A consumer that answers after the first run's I/O budget has run out, but
/// well within the caller's reply deadline, gets the approved work done: the
/// approved run has its own, renewed I/O budget.
#[tokio::test]
async fn standalone_late_approval_reruns_with_a_renewed_io_budget() {
    let _lock = CONSENT_TEST_LOCK.lock().await;
    let f = fixture_with_executor(
        FakeCingulate::up(),
        tempfile::tempdir().unwrap(),
        Arc::new(Executor::with_config(crate::executor::ExecutorConfig {
            pool_size: 2,
            read_cap: 1,
            actor_cap: 1,
            ..crate::executor::ExecutorConfig::default()
        })),
    );
    let corr = 9_100_001;
    let _overrides = CallOverrides(corr);
    crate::subc::PLAIN_RUN_IO_BUDGET_FOR_TEST
        .lock()
        .unwrap()
        .insert(corr, Duration::from_millis(1_500));
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    inner.role = tool_provider::RouteRole::Legacy;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"path": f.head_root.join("shared.txt")}),
            corr,
            read_dispatch,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    // Answer only after the first run's whole I/O budget has passed.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = f.executor.submit_async(
        route.root.clone(),
        Lane::PureRead,
        "approval-queue-blocker".to_string(),
        Box::new(move |_| {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(15));
            Response::success("approval-queue-blocker", json!({}))
        }),
    );
    started_rx.await.unwrap();
    let approved_at = Instant::now();
    assert!(standalone::reply(&writer, &allow_frame(&ask)));
    wait_for(|| {
        f.executor
            .try_dispatch_liveness_snapshot()
            .is_some_and(|snapshot| snapshot.interactive.queued > 0)
    })
    .await;
    // The approved run must retain its full 1500ms I/O allowance even after
    // spending twice that long queued behind another read for the same project.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    let admitted_after = Instant::now();
    release_tx.send(()).unwrap();
    blocker.await.unwrap();
    let result = standalone_reply(&mut replies).await;
    assert_eq!(result["isError"], false, "{result}");
    assert!(text(&result).contains("head-owned contents"), "{result}");
    let renewed = *standalone::RERUN_BUDGETS_FOR_TEST
        .lock()
        .unwrap()
        .last()
        .expect("the approved run was given a budget");
    assert!(
        renewed > admitted_after + Duration::from_millis(1_000),
        "approval at {approved_at:?}, admission after {admitted_after:?}, deadline {renewed:?}"
    );
    f.connection_cancel.cancel();
}

/// When the consumer does not answer within the time the reply budget allows,
/// the call ends with the typed `access_pending` outcome a consent hold gives,
/// nothing runs, and a late answer is not accepted.
#[tokio::test]
async fn standalone_unanswered_approval_ends_with_access_pending() {
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn counted_read(request: RawRequest, ctx: &AppContext) -> Response {
        RUNS.fetch_add(1, Ordering::SeqCst);
        read_dispatch(request, ctx)
    }
    let _lock = CONSENT_TEST_LOCK.lock().await;
    RUNS.store(0, Ordering::SeqCst);
    let f = fixture(FakeCingulate::up());
    let corr = 9_100_002;
    let _overrides = CallOverrides(corr);
    standalone::APPROVAL_WAIT_FOR_TEST
        .lock()
        .unwrap()
        .insert(corr, Duration::from_millis(300));
    let mut route = identity(&f.worker.project_root, None);
    let inner = Arc::get_mut(&mut route.0).unwrap();
    inner.trust = BindTrust::Untrusted;
    inner.consumer_elicitation_capable = true;
    inner.role = tool_provider::RouteRole::Legacy;
    let (writer, mut replies) = f
        .send_tool_with_writer(
            &route,
            "read",
            json!({"path": f.head_root.join("shared.txt")}),
            corr,
            counted_read,
        )
        .await;
    let ask = tokio::time::timeout(Duration::from_secs(10), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let result = standalone_reply(&mut replies).await;
    // Untrusted consumers receive errors as text; the outcome is the typed
    // pending answer, not the denial's path error.
    assert_eq!(result["isError"], true, "{result}");
    assert!(
        text(&result).contains("was not answered in time"),
        "{result}"
    );
    assert!(
        !text(&result).contains("outside the project root"),
        "{result}"
    );
    assert!(!standalone::reply(&writer, &allow_frame(&ask)));
    assert_eq!(RUNS.load(Ordering::SeqCst), 1, "nothing ran after the ask");
    f.connection_cancel.cancel();
}

/// The approval wait never runs into the time the approved run needs.
#[test]
fn standalone_approval_deadline_leaves_the_rerun_margin() {
    let now = Instant::now();
    let far = now + Duration::from_secs(600);
    assert_eq!(
        standalone::approval_deadline(now, far, Duration::from_secs(60)),
        now + Duration::from_secs(60)
    );
    let near = now + Duration::from_secs(20);
    assert_eq!(
        standalone::approval_deadline(now, near, Duration::from_secs(60)),
        near - RERUN_MARGIN
    );
    let past = now + Duration::from_secs(1);
    assert_eq!(
        standalone::approval_deadline(now, past, Duration::from_secs(60)),
        now
    );
}
