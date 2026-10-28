use super::*;
use crate::db::access_grants::ScopeKey;
use crate::gh_shim_relay::{
    RelayCall, RelayReply, RelayTransport, RouteScopes, TransportError, BOT_REQUEST_OPERATION,
};
use crate::gh_shim_speech::model::Binding;
use crate::gh_shim_speech::partner::{QueuedBotResponse, ReservedBotRequest};
use crate::gh_shim_speech::service::{LaunchSeed, Service, TestPartners};
use crate::gh_shim_speech::{Code, Refusal};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use subc_protocol::scope::{ScopeAttributes, ScopeKind, ScopeStamp, ScopeStatus};

#[path = "admission_tests.rs"]
mod admission;
#[path = "audit_proof_tests.rs"]
mod audit_proofs;
#[cfg(unix)]
#[path = "cold_redemption_tests.rs"]
mod cold;
#[path = "crash_dispatch_tests.rs"]
mod crash;
#[path = "pty_dispatch_tests.rs"]
mod pty;

struct Env(&'static str, Option<OsString>);
impl Env {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self(key, previous)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        match self.1.take() {
            Some(value) => unsafe { std::env::set_var(self.0, value) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

#[derive(Clone)]
struct FakePlex {
    head: ScopeStamp,
    state: Arc<Mutex<PlexState>>,
    slots: Arc<tokio::sync::Semaphore>,
}
struct PlexState {
    repo: String,
    agent: String,
    handle: String,
    generation: u64,
    scopes: HashMap<ScopeKey, ScopeStamp>,
    unknown: HashSet<ScopeKey>,
    ended: HashSet<ScopeKey>,
    describes: usize,
    route_open_ended: bool,
    post_log: Option<PathBuf>,
    posts: Vec<Value>,
    sends: usize,
    fingerprints: HashMap<String, Value>,
    nonces: HashMap<String, (Value, Value)>,
}

fn refused(code: &str, role: &str) -> Value {
    json!({"status":"refused","code":code,"scope_role":role,"transmission":"not_sent","legs_sent":[]})
}

fn wire_owner(owner: &subc_protocol::Principal) -> String {
    match owner {
        subc_protocol::Principal::Direct => "direct".into(),
        subc_protocol::Principal::Unverified => "unverified".into(),
        subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
    }
}

impl FakePlex {
    fn bindings(&self) -> Value {
        let state = self.state.lock().unwrap();
        json!({"repo_binding_generation":state.generation,"bindings":[{
            "repository":state.repo,"agent_id":state.agent,"app_handle_id":state.handle
        }]})
    }

    fn describe(
        &self,
        expected: ScopeStamp,
    ) -> subc_protocol::session::ModuleControlResponseToModule {
        let mut state = self.state.lock().unwrap();
        state.describes += 1;
        let key = ScopeKey::from_stamp(&expected);
        let scope = state.scopes.get(&key).cloned();
        let unknown = state.unknown.contains(&key);
        subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
            status: if unknown || scope.is_some() {
                ScopeStatus::Live
            } else {
                ScopeStatus::Ended
            },
            scope_epoch: Some(expected.scope_epoch),
            daemon_incarnation: "fake-scope-daemon".into(),
            owner_synced: !unknown,
            owner_configured: true,
            scope,
        }
    }

    fn send(&self, head: &ScopeStamp, args: Value, guarded: bool) -> Value {
        let mut state = self.state.lock().unwrap();
        state.sends += 1;
        let Some(nonce) = args
            .get("request_nonce")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return refused("invalid_request", "route");
        };
        let fingerprint = json!({"request":args["request"],"expect":args.get("expect")});
        if state
            .fingerprints
            .get(&nonce)
            .is_some_and(|old| old != &fingerprint)
        {
            return refused("nonce_payload_mismatch", "route");
        }
        state
            .fingerprints
            .entry(nonce.clone())
            .or_insert_with(|| fingerprint.clone());
        if let Some((old, reply)) = state.nonces.get(&nonce) {
            return if old == &fingerprint {
                reply.clone()
            } else {
                refused("nonce_payload_mismatch", "route")
            };
        }
        let key = ScopeKey::from_stamp(head);
        if state.ended.contains(&key) {
            return refused("scope_ended", "route");
        }
        if state.unknown.contains(&key) {
            return refused("scope_not_synced", "route");
        }
        let Some(current) = state.scopes.get(&key) else {
            return refused("scope_ended", "route");
        };
        if current.kind != ScopeKind::Head || !current.owner_authorized {
            return refused("scope_unverifiable", "route");
        }
        if crate::gh_shim_relay::delegating_agent(current) != Some(state.agent.as_str()) {
            return refused("delegation_withdrawn", "route");
        }
        if args["request"]["repository"] != state.repo
            || args["request"]["metadata"]["agent_id"] != state.agent
            || args["request"]["action"] != "issue comment"
        {
            return refused("invalid_request", "route");
        }
        if guarded {
            let Ok(expect) = serde_json::from_value::<Binding>(args["expect"].clone()) else {
                return refused("invalid_request", "route");
            };
            if expect.app_handle_id != state.handle {
                let mut reply = refused("stale_authority", "route");
                reply["dimension"] = json!("expected_handle");
                return reply;
            }
            if expect.repo_binding_generation != state.generation {
                let mut reply = refused("stale_authority", "route");
                reply["dimension"] = json!("expected_binding");
                return reply;
            }
            if let Some(run) = args.get("run_scope") {
                if serde_json::from_value::<crate::gh_shim_speech::partner::RunSelector>(
                    run.clone(),
                )
                .is_err()
                {
                    return refused("invalid_request", "run");
                }
                if state.ended.iter().any(|key| {
                    let owner: subc_protocol::Principal = serde_json::from_str(&key.owner).unwrap();
                    run == &json!({"owner":wire_owner(&owner),"ref":key.scope_ref,"epoch":key.epoch})
                }) {
                    return refused("scope_ended", "run");
                }
                let current = state.scopes.values().find(|stamp| {
                    run == &json!({"owner":wire_owner(&stamp.owner),"ref":stamp.scope_ref,"epoch":stamp.scope_epoch})
                });
                let Some(current) = current else {
                    return refused("scope_ended", "run");
                };
                let key = ScopeKey::from_stamp(current);
                if state.unknown.contains(&key) {
                    return refused("scope_not_synced", "run");
                }
                if current.parent.as_ref()
                    != Some(&subc_protocol::scope::ScopeParent::new(
                        head.owner.clone(),
                        head.scope_ref.clone(),
                        head.scope_epoch,
                    ))
                    || current.parent_state != Some(subc_protocol::scope::ParentState::Linked)
                    || current.kind != ScopeKind::Ephemeral
                    || !current.owner_authorized
                    || current.attributes.agent_id.as_deref() != Some(state.agent.as_str())
                    || current.attributes.run_id.is_none()
                    || current
                        .expires_at_ms
                        .is_some_and(|expiry| crate::db::call_ledger::now_ms() as u64 >= expiry)
                {
                    return refused("scope_unverifiable", "run");
                }
            }
        }
        if let Some(path) = &state.post_log {
            fs::write(path, nonce.as_bytes()).unwrap();
        }
        state.posts.push(args["request"].clone());
        let reply = json!({"status":"completed","result":{"posted":true}});
        state.nonces.insert(nonce, (fingerprint, reply.clone()));
        reply
    }

    fn reserve(&self, scope: &ScopeStamp, body: Value) -> Result<ReservedBotRequest, Refusal> {
        if self.state.lock().unwrap().route_open_ended {
            return Err(Refusal::new(Code::ScopeEnded, "ended"));
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refusal::new(Code::AuthorityUnavailable, "dispatch_queue_full"))?;
        let plex = self.clone();
        let scope = scope.clone();
        Ok(ReservedBotRequest::for_test(move || {
            let reply = plex.send(&scope, body["arguments"].clone(), true);
            drop(slot);
            Ok(QueuedBotResponse::for_test(reply))
        }))
    }
}

impl RelayTransport for FakePlex {
    fn opens_scoped_routes(&self) -> bool {
        true
    }
    async fn plexus(
        &self,
        _project: &str,
        _session: &str,
        scope: Option<&subc_client_rs::ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        assert_eq!(
            scope,
            Some(&subc_client_rs::ScopeSelector::new(
                self.head.owner.clone(),
                self.head.scope_ref.clone(),
                Some(self.head.scope_epoch)
            ))
        );
        if self.state.lock().unwrap().route_open_ended {
            return Err(TransportError::ScopeEnded {
                selector: scope.unwrap().clone(),
                message: "the selected epoch ended".into(),
            });
        }
        Ok(self.send(&self.head, body["arguments"].clone(), false))
    }
}

struct Fixture {
    _runtime: crate::gh_shim_speech::service::RuntimeGuard,
    _dir: tempfile::TempDir,
    _env: Vec<Env>,
    ctx: Arc<crate::context::AppContext>,
    service: Arc<Service>,
    plex: FakePlex,
    head: ScopeStamp,
    run: Option<ScopeStamp>,
    launch: crate::gh_shim_speech::model::LaunchAuthority,
    task: String,
    session: String,
    params: Value,
    incarnation: String,
    startup_error: Option<String>,
    _relay: Arc<crate::gh_shim_relay::GhRelay>,
    sdk: Option<crate::gh_shim_relay::tests::FakeDaemon>,
}

#[derive(Default)]
struct LaunchOptions {
    foreground: bool,
    sdk_daemon: bool,
    pty: bool,
    command: Option<String>,
    seal_failure: Option<Refusal>,
    fail_revocation: bool,
    pause: Option<(
        std::sync::mpsc::Sender<String>,
        std::sync::mpsc::Receiver<()>,
    )>,
}

impl Fixture {
    async fn new(codemode: bool) -> Self {
        let fixture = Self::new_with_options(codemode, LaunchOptions::default()).await;
        assert!(
            fixture.startup_error.is_none(),
            "{:?}",
            fixture.startup_error
        );
        fixture
    }

    async fn new_with_options(codemode: bool, mut options: LaunchOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let state_dir = dir.path().join("shim");
        let project = dir.path().join("project");
        // Production opens its storage lease before registering speech. The
        // directory must already exist so symlinked temp ancestors have the
        // same canonical identity during registration and later lookup.
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        let old_storage = std::env::var_os("AFT_STORAGE_DIR");
        unsafe { std::env::remove_var("AFT_STORAGE_DIR") };
        let env = vec![
            Env("AFT_STORAGE_DIR", old_storage),
            Env::set("AFT_GH_SHIM_STATE_DIR", state_dir.as_os_str()),
        ];
        let paths = StatePaths::from_root(state_dir);
        let manifest = super::tests::fixture_manifest();
        super::tests::write_signed_manifest(&paths, manifest.clone(), unix_seconds());
        load_manifest(&paths, unix_seconds()).unwrap();
        let (repo, agent) = manifest.bindings.iter().next().unwrap();
        let head = ScopeStamp::new(
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            "head-posting",
            7,
            ScopeKind::Head,
        )
        .with_attributes(
            ScopeAttributes::new()
                .with_agent_id(Some(agent.clone()))
                .with_delegates(true),
        )
        .with_owner_authorized(true);
        let run = codemode.then(|| {
            ScopeStamp::new(head.owner.clone(), "run-posting", 9, ScopeKind::Ephemeral)
                .with_parent(Some(subc_protocol::scope::ScopeParent::new(
                    head.owner.clone(),
                    head.scope_ref.clone(),
                    head.scope_epoch,
                )))
                .with_parent_state(Some(subc_protocol::scope::ParentState::Linked))
                .with_attributes(
                    ScopeAttributes::new()
                        .with_agent_id(Some(agent.clone()))
                        .with_run_id(Some("program-posting".into())),
                )
                .with_owner_authorized(true)
                .with_expires_at_ms(Some(crate::db::call_ledger::now_ms() as u64 + 60_000))
        });
        let mut scopes = HashMap::new();
        scopes.insert(ScopeKey::from_stamp(&head), head.clone());
        if let Some(run) = &run {
            scopes.insert(ScopeKey::from_stamp(run), run.clone());
        }
        let plex = FakePlex {
            head: head.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(16)),
            state: Arc::new(Mutex::new(PlexState {
                repo: repo.clone(),
                agent: agent.clone(),
                handle: "app-original".into(),
                generation: 4,
                scopes,
                unknown: HashSet::new(),
                ended: HashSet::new(),
                describes: 0,
                route_open_ended: false,
                post_log: None,
                posts: Vec::new(),
                sends: 0,
                fingerprints: HashMap::new(),
                nonces: HashMap::new(),
            })),
        };
        let sdk = if options.sdk_daemon {
            let server = plex.clone();
            Some(
                crate::gh_shim_relay::tests::fake_daemon_with_reply(Arc::new(
                    move |body, scope| {
                        let args = &body["arguments"];
                        if args["op"] == "bindings.read" {
                            return server.bindings();
                        }
                        let expected = serde_json::to_value(subc_client_rs::ScopeSelector::new(
                            server.head.owner.clone(),
                            server.head.scope_ref.clone(),
                            Some(server.head.scope_epoch),
                        ))
                        .unwrap();
                        if args["op"] != "bot_request" || scope != Some(&expected) {
                            return refused("scope_unverifiable", "route");
                        }
                        server.send(&server.head, args.clone(), true)
                    },
                ))
                .await,
            )
        } else {
            None
        };
        let relay = if let Some(daemon) = &sdk {
            Arc::new(crate::gh_shim_relay::GhRelay {
                transport: daemon.transport().await,
            })
        } else {
            crate::gh_shim_relay::GhRelay::new(dir.path().join("unused-connection.json"))
        };
        let describe = plex.clone();
        let service = crate::gh_shim_speech::service::start(
            &root,
            &relay,
            Arc::new(move |scope| {
                let reply = describe.describe(scope);
                Box::pin(async move { Ok(reply) })
            }),
            "fixture-control".into(),
            true,
        );
        assert!(
            crate::gh_shim_speech::service::lookup(&root)
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, &service)),
            "fixture speech registration must be found through its supplied storage path: {root:?}"
        );
        let runtime = service.runtime_guard();
        let read = plex.clone();
        let reserve = plex.clone();
        if !options.sdk_daemon {
            service.install_test_partners(Arc::new(TestPartners {
                seal_failure: options.seal_failure.clone(),
                expect_placed: Some("fake-atomic-expect"),
                run_placed: Some("fake-atomic-run"),
                reserved_send_placed: true,
                bindings: Arc::new(move |_, _| Ok(read.bindings())),
                reserve: Arc::new(move |_, _, scope, body| reserve.reserve(scope, body)),
                before_final_gate: None,
            }));
        }
        let mut config = crate::config::Config::default();
        config.project_root = Some(project.clone());
        config.storage_dir = Some(root.clone());
        let ctx = Arc::new(crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        ));
        let pool = Arc::new(Mutex::new(crate::db::open(&root.join("aft.db")).unwrap()));
        ctx.set_db(pool.clone());
        ctx.bash_background().set_db_pool(pool);
        ctx.bash_background()
            .set_harness(crate::harness::Harness::Opencode);
        if let Some((arrived, release)) = options.pause.take() {
            ctx.bash_background()
                .install_pty_seal_pause_for_test(arrived, release);
        }
        if options.fail_revocation {
            service.store.connect().unwrap().execute_batch("CREATE TRIGGER reject_pty_revocation BEFORE INSERT ON gh_speech_task_revocations BEGIN SELECT RAISE(ABORT,'simulated revocation failure'); END;").unwrap();
        }
        let origin = crate::gh_shim_speech::model::RouteOrigin {
            run_nonce: crate::gh_shim_speech::service::run_nonce().unwrap().into(),
            connection_generation: "fixture-control".into(),
            channel: 1,
            epoch: 1,
        };
        let launch = if let Some(run) = &run {
            let mut head_origin = origin.clone();
            head_origin.channel = 2;
            crate::gh_shim_speech::model::LaunchAuthority::codemode(
                head.clone(),
                run.clone(),
                Some(origin),
                Some(head_origin),
            )
            .unwrap()
        } else {
            crate::gh_shim_speech::model::LaunchAuthority::classic(head.clone(), Some(origin))
                .unwrap()
        };
        let session = "dispatch-session".to_string();
        let seed = LaunchSeed {
            launch: Ok(launch.clone()),
            lifecycle: Some(run.as_ref().unwrap_or(&head).clone()),
            session: session.clone(),
            harness: "opencode".into(),
            project_root: project.display().to_string(),
        };
        let spawn_ctx = ctx.clone();
        let spawn_root = root.clone();
        let spawn_session = session.clone();
        let lifecycle = seed.lifecycle.clone();
        let pty = options.pty;
        let background = !options.foreground;
        let command = options.command.take().unwrap_or_else(|| "sleep 120".into());
        let (ticket, started, incarnation) = tokio::task::spawn_blocking(move || {
            std::thread::spawn(move || {
                crate::gh_shim_speech::service::with_launch(Some(seed), &spawn_root, || {
                    let ticket = crate::bash_background::registry::with_scope_spawn(
                        lifecycle.clone(),
                        || {
                            crate::gh_shim_ticket::PendingTicket::issue(
                                &spawn_session,
                                &project.display().to_string(),
                            )
                        },
                    );
                    let raw = ticket.value().unwrap().to_string();
                    let incarnation = crate::gh_shim_ticket::speech_for_ticket(&raw)
                        .unwrap()
                        .incarnation
                        .clone();
                    let _guard = crate::gh_shim_ticket::spawn_ticket_guard(Some(&raw));
                    crate::gh_shim_ticket::mark_spawn_background(background);
                    let mut environment = HashMap::new();
                    environment.insert(
                        crate::gh_shim_ticket::GH_SHIM_TICKET_ENV.into(),
                        raw.clone(),
                    );
                    let task =
                        crate::bash_background::registry::with_scope_spawn(lifecycle, || {
                            if pty {
                                spawn_ctx.bash_background().spawn_pty_with_shell(
                                    crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                                    &command,
                                    crate::bash_background::BashShell::Bash,
                                    crate::bash_background::registry::resolve_posix_shell(),
                                    spawn_session,
                                    project.clone(),
                                    environment,
                                    crate::bash_background::HardKill::After(Duration::from_secs(
                                        120,
                                    )),
                                    None,
                                    spawn_root.join("opencode"),
                                    10,
                                    true,
                                    false,
                                    Some(project),
                                    24,
                                    80,
                                )
                            } else {
                                spawn_ctx
                                    .bash_background()
                                    .spawn_with_shell_in_slot(
                                        crate::sandbox_spawn::SpawnPlan::Unsandboxed,
                                        &command,
                                        crate::bash_background::BashShell::Bash,
                                        crate::bash_background::registry::resolve_posix_shell(),
                                        spawn_session,
                                        project.clone(),
                                        environment,
                                        crate::bash_background::HardKill::After(
                                            Duration::from_secs(120),
                                        ),
                                        None,
                                        spawn_root.join("opencode"),
                                        crate::bash_background::TaskSlot::Background { max: 10 },
                                        true,
                                        false,
                                        Some(project),
                                    )
                                    .map_err(|error| error.to_string())
                            }
                        });
                    if let Ok(task) = &task {
                        ticket.bind_task(task);
                    }
                    (raw, task, incarnation)
                })
            })
            .join()
            .unwrap()
        })
        .await
        .unwrap();
        let (task, startup_error) = match started {
            Ok(task) => (task, None),
            Err(error) => {
                let conn = service.store.connect().unwrap();
                let task: String = conn
                    .query_row(
                        "SELECT task_id FROM gh_speech_authorities WHERE incarnation=?1",
                        [&incarnation],
                        |row| row.get(0),
                    )
                    .unwrap();
                (task, Some(error))
            }
        };
        if startup_error.is_none() && options.seal_failure.is_none() && !options.foreground {
            let warm = crate::gh_shim_ticket::speech_for_ticket(&ticket).unwrap();
            assert!(
                warm.active.load(std::sync::atomic::Ordering::SeqCst),
                "fixture must acquire genuine durable authority: {:?}",
                warm.failure.lock().unwrap()
            );
        }
        let args: Vec<OsString> = [
            "issue",
            "comment",
            "7",
            "--repo",
            repo.as_str(),
            "--body",
            "frozen dispatch body",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let Classification::Governed { tuple, canonical } =
            classify(&args, &manifest, current_platform())
        else {
            panic!("fixture must govern issue comments")
        };
        let request =
            canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version).unwrap();
        let invocation = request.invocation.clone().unwrap();
        let rung = RungDetermination::r3(
            0,
            manifest.manifest_version,
            &RungRecordProvenance {
                image_path: "/aft".into(),
                version: "test".into(),
                repo_key: repo.clone(),
            },
        );
        let request = governed_wire_request(&rung.record, agent, request);
        let params = json!({"ticket":ticket,"request_nonce":"dispatch-post","request":request,"invocation":invocation});
        Self {
            _runtime: runtime,
            _dir: dir,
            _env: env,
            ctx,
            service,
            plex,
            head,
            run,
            launch,
            task,
            session,
            params,
            incarnation,
            startup_error,
            _relay: relay,
            sdk,
        }
    }

    fn routes(&self, bound: bool) -> RouteScopes {
        let mut routes = RouteScopes::default();
        routes.speech = Some(self.service.clone());
        routes.push(&self.session, Some(self.head.clone()));
        if bound {
            for (role, scope) in self.launch.required_scopes() {
                let origin = if role == crate::gh_shim_speech::ScopeRole::Head {
                    &self.launch.head_origin
                } else {
                    &self.launch.origin
                };
                routes.push_origin(origin.clone().unwrap(), Some(scope.clone()));
            }
        }
        routes
    }

    async fn post(&self, bound: bool, nonce: &str) -> RelayReply {
        let mut params = self.params.clone();
        params["request_nonce"] = json!(nonce);
        crate::gh_shim_relay::relay(
            &self.plex,
            RelayCall {
                operation: BOT_REQUEST_OPERATION,
                params: &params,
                first_party: true,
                route_scopes: &self.routes(bound),
            },
            &|_| {},
        )
        .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.ctx.bash_background().kill(&self.task, &self.session);
        self.ctx.bash_background().detach();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !self._runtime.retire() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Fixture {
    fn partners(&self, change: impl FnOnce(&mut TestPartners)) {
        let mut partners = (*self.service.test_partners().unwrap()).clone();
        change(&mut partners);
        self.service.install_test_partners(Arc::new(partners));
    }
}

#[tokio::test]
async fn captured_dispatch_reaches_the_real_gate_with_a_precommitted_pinned_nonce() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    assert!(crate::gh_shim_speech::partner::require_reserved_send().is_ok());
    let path = fixture.service.store.path().to_path_buf();
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, scope, body| {
            let nonce = body["arguments"]["request_nonce"]
                .as_str()
                .unwrap()
                .to_string();
            let slot = reserve(project, session, scope, body)?;
            let path = path.clone();
            Ok(ReservedBotRequest::for_test(move || {
                let conn = crate::db::gh_speech::Store::new(path).connect().unwrap();
                let expect: String = conn
                    .query_row(
                        "SELECT expect_json FROM gh_speech_requests WHERE request_nonce=?1",
                        [&nonce],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&expect).unwrap(),
                    json!({"app_handle_id":"app-original","repo_binding_generation":4})
                );
                slot.send()
            }))
        });
    });
    let answer = fixture.post(false, "captured-live").await;
    assert!(answer.ok, "{:?}", answer.data);
    let state = fixture.plex.state.lock().unwrap();
    assert_eq!(state.posts, vec![fixture.params["request"].clone()]);
    assert_eq!(state.sends, 1);
}

#[tokio::test]
async fn captured_dispatch_rechecks_cache_age_after_reserving_the_send_slot() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let service = Arc::downgrade(&fixture.service);
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, scope, body| {
            let slot = reserve(project, session, scope, body)?;
            service.upgrade().unwrap().expire_test_scope_cache();
            Ok(slot)
        });
    });
    let answer = fixture.post(false, "expired-send").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_unknown");
    assert_eq!(answer.data["reason"], "timeout");
    assert!(fixture.plex.state.lock().unwrap().posts.is_empty());
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn captured_dispatch_pins_handle_and_expect_generation_without_nonce_rebinding() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    fixture.plex.state.lock().unwrap().handle = "app-rotated".into();
    let rotated = fixture.post(false, "rotated-handle").await;
    assert_eq!(rotated.data["refusal_code"], "task_gh_identity_conflict");
    assert_eq!(rotated.data["reason"], "binding_changed");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
    fixture.plex.state.lock().unwrap().handle = "app-original".into();
    let plex = fixture.plex.clone();
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, scope, body| {
            let slot = reserve(project, session, scope, body)?;
            plex.state.lock().unwrap().generation += 1;
            Ok(slot)
        });
    });
    let raced = fixture.post(false, "generation-race").await;
    assert_eq!(raced.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(raced.data["reason"], "admission_raced");
    let retry = fixture.post(false, "generation-race").await;
    assert_eq!(retry.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(retry.data["reason"], "snapshot_invalid");
    let state = fixture.plex.state.lock().unwrap();
    assert!(state.posts.is_empty());
    assert_eq!(
        state.sends, 1,
        "the durable request must reject a changed expect before another provider enqueue"
    );
}

#[tokio::test]
async fn captured_codemode_dispatch_checks_both_head_and_program_before_enqueue() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(true).await;
    let first = fixture.post(false, "codemode-live").await;
    assert!(first.ok, "{:?}", first.data);
    let run = fixture.run.as_ref().unwrap();
    fixture
        .plex
        .state
        .lock()
        .unwrap()
        .unknown
        .insert(ScopeKey::from_stamp(run));
    fixture.service.invalidate(run);
    let unknown = fixture.post(false, "codemode-unknown").await;
    assert_eq!(unknown.data["refusal_code"], "task_gh_scope_unknown");
    assert_eq!(unknown.data["scope_role"], "run");
    assert_eq!(unknown.data["reason"], "not_synced");
    assert_eq!(unknown.data["message"], "AFT could not confirm that the codemode program this command started from is still active (not_synced); retry in a minute.");
    assert_eq!(fixture.plex.state.lock().unwrap().posts.len(), 1);
    fixture.plex.state.lock().unwrap().unknown.clear();
    fixture
        .plex
        .state
        .lock()
        .unwrap()
        .scopes
        .remove(&ScopeKey::from_stamp(&fixture.head));
    fixture.service.invalidate(&fixture.head);
    let ended = fixture.post(false, "codemode-head-ended").await;
    assert_eq!(ended.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(ended.data["scope_role"], "head");
    assert_eq!(ended.data["reason"], "ended");
    assert_eq!(ended.data["message"], "the head session this command started from has ended (ended), so it can no longer write as the bot; start the command again from a live session.");
    assert_eq!(fixture.plex.state.lock().unwrap().posts.len(), 1);
}

#[test]
fn cancel_child() {
    let Some(root) = std::env::var_os("AFT_TEST_DISPATCH_CANCEL_ROOT") else {
        return;
    };
    let stamp: ScopeStamp =
        serde_json::from_str(&std::env::var("AFT_TEST_DISPATCH_CANCEL_SCOPE").unwrap()).unwrap();
    assert!(crate::root_cache::acquire_speech_serving_lease(Path::new(&root)).is_err());
    let saved = saved_task_summary(Path::new(&root));
    let reply = crate::commands::bash_kill_scope::handle_bash_kill_scope_with_saved_tasks(
        json!({"scope":stamp}),
        Some(&stamp.owner),
        &[],
        Some(Path::new(&root)),
    );
    let mut receipt = serde_json::to_value(reply).unwrap();
    receipt["_saved_before_cancel"] = json!(saved);
    println!("CANCEL_REPLY {}", serde_json::to_string(&receipt).unwrap());
}

fn saved_task_summary(root: &Path) -> Vec<Value> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            found.extend(saved_task_summary(&entry.path()));
        } else if kind.is_file() && entry.path().extension() == Some(OsStr::new("json")) {
            if let Ok(value) =
                fs::read(entry.path()).map(|bytes| serde_json::from_slice::<Value>(&bytes))
            {
                if let Ok(value) = value {
                    if value.get("task_id").is_some() {
                        found.push(json!({"path":entry.path(),"task_id":value["task_id"],"status":value["status"],"scope":value["scope"],"child_pid":value["child_pid"]}));
                    }
                }
            }
        }
    }
    found
}

fn cancel_from_sibling(root: &Path, scope: &ScopeStamp) -> Value {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gh_shim::speech_dispatch_tests::cancel_child",
            "--nocapture",
        ])
        .env("AFT_TEST_DISPATCH_CANCEL_ROOT", root)
        .env(
            "AFT_TEST_DISPATCH_CANCEL_SCOPE",
            serde_json::to_string(scope).unwrap(),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "sibling cancel test did not execute"
    );
    let reply = stdout
        .lines()
        .find_map(|line| line.split_once("CANCEL_REPLY ").map(|(_, reply)| reply))
        .expect("missing sibling cancel receipt");
    serde_json::from_str(reply).unwrap()
}

#[tokio::test]
async fn bound_background_cancel_committed_before_sdk_send_is_refused() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let root = fixture.service.store.path().parent().unwrap().to_path_buf();
    let scope = fixture.head.clone();
    let incarnation = fixture.incarnation.clone();
    fixture.partners(|partners| {
        partners.before_final_gate = Some(Arc::new(move || {
            let receipt = cancel_from_sibling(&root, &scope);
            assert_eq!(receipt["success"], true);
            assert_eq!(
                receipt["killed"].as_array().unwrap().len() as u64
                    + receipt["already_finished"].as_u64().unwrap(),
                1,
                "{receipt:?}"
            );
            let marker = crate::db::gh_speech::Store::new(root.join("aft.db"))
                .get(&incarnation)
                .err()
                .map(|error| error.reason);
            assert_eq!(
                marker,
                Some("revoked".into()),
                "cancellation must commit before acknowledging: {receipt:?}"
            );
        }));
    });
    let answer = fixture.post(true, "cancel-before").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(answer.data["reason"], "revoked");
    assert_eq!(
        fixture.plex.state.lock().unwrap().sends,
        0,
        "no SDK mutation may be sent after an earlier cancellation commit"
    );
}

#[tokio::test]
async fn bound_sdk_send_precedes_cancel_without_a_legacy_mailbox() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let root = fixture.service.store.path().parent().unwrap().to_path_buf();
    let scope = fixture.head.clone();
    let receipt = Arc::new(Mutex::new(None));
    let captured = receipt.clone();
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, head, body| {
            let slot = reserve(project, session, head, body)?;
            let root = root.clone();
            let scope = scope.clone();
            let captured = captured.clone();
            Ok(ReservedBotRequest::new(move || {
                let queued = slot.send()?;
                Ok(QueuedBotResponse::new(async move {
                    *captured.lock().unwrap() = Some(cancel_from_sibling(&root, &scope));
                    queued.response().await
                }))
            }))
        });
    });
    let answer = fixture.post(true, "cancel-after").await;
    assert!(answer.ok, "{:?}", answer.data);
    assert_eq!(receipt.lock().unwrap().as_ref().unwrap()["success"], true);
    assert_eq!(fixture.plex.state.lock().unwrap().posts.len(), 1);
    assert_eq!(
        fixture
            .service
            .store
            .pending_legacy_for_identity(&fixture.session, &fixture.task)
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn bound_sdk_reservation_queue_full_does_not_prepare_or_send() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    fixture.partners(|partners| {
        partners.reserve = Arc::new(|_, _, _, _| {
            Err(Refusal::new(
                Code::AuthorityUnavailable,
                "dispatch_queue_full",
            ))
        });
    });
    let answer = fixture.post(true, "full-bound").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(answer.data["reason"], "dispatch_queue_full");
    assert_eq!(answer.data["message"], "bot writes from background commands are briefly unavailable (dispatch_queue_full); retry in a few seconds.");
    let conn = fixture.service.store.connect().unwrap();
    let prepared: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gh_speech_requests WHERE request_nonce='full-bound'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(prepared, 0);
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn historical_legacy_dispatch_cannot_be_resent_by_the_relay() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    seed_historical_dispatch(&fixture, "lost-consumer");
    let recovered = crate::db::gh_speech::Store::new(fixture.service.store.path());
    assert_eq!(
        recovered
            .pending_legacy_for_identity(&fixture.session, &fixture.task)
            .unwrap(),
        1
    );
    let retry = fixture.post(true, "lost-consumer").await;
    assert_eq!(retry.data["refusal_code"], "outcome_unknown");
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

fn seed_historical_dispatch(fixture: &Fixture, nonce: &str) {
    let store = &fixture.service.store;
    let stored = store.get(&fixture.incarnation).unwrap();
    let (current, _) = fixture.service.register_accepted_policy().unwrap().unwrap();
    let policy_generation = store.advance_policy(&current).unwrap();
    let payload = blake3::derive_key(
        "aft-gh-bound-request-v1",
        &serde_json::to_vec(&fixture.params["request"]).unwrap(),
    );
    store
        .prepare_request(nonce, &fixture.incarnation, &payload, "legacy-bound")
        .unwrap();
    store
        .final_legacy_gate(
            &fixture.incarnation,
            nonce,
            stored.generation,
            policy_generation,
            &current,
            "historical-forwarder",
            || Ok(()),
        )
        .unwrap();
}

#[tokio::test]
async fn qualified_provider_end_is_fenced_without_a_followup_describe() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let plex = fixture.plex.clone();
    let key = ScopeKey::from_stamp(&fixture.head);
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, scope, body| {
            let slot = reserve(project, session, scope, body)?;
            let mut state = plex.state.lock().unwrap();
            state.scopes.remove(&key);
            state.ended.insert(key.clone());
            state.unknown.insert(key.clone());
            Ok(slot)
        });
    });
    let answer = fixture.post(false, "provider-end").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(answer.data["reason"], "ended");
    assert_eq!(answer.data["scope_role"], "authority");
    assert!(fixture.service.store.scope_ended(&fixture.head).unwrap());
    assert_eq!(
        fixture.plex.state.lock().unwrap().describes,
        1,
        "typed end evidence must not depend on another describe"
    );
    assert!(fixture.plex.state.lock().unwrap().posts.is_empty());
}

#[tokio::test]
async fn qualified_program_end_fences_the_program_not_its_live_head() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(true).await;
    let plex = fixture.plex.clone();
    let run = fixture.run.as_ref().unwrap().clone();
    let key = ScopeKey::from_stamp(&run);
    fixture.partners(|partners| {
        let reserve = partners.reserve.clone();
        partners.reserve = Arc::new(move |project, session, scope, body| {
            let slot = reserve(project, session, scope, body)?;
            let mut state = plex.state.lock().unwrap();
            state.scopes.remove(&key);
            state.ended.insert(key.clone());
            state.unknown.insert(key.clone());
            Ok(slot)
        });
    });
    let answer = fixture.post(false, "program-provider-end").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(answer.data["reason"], "ended");
    assert_eq!(answer.data["scope_role"], "run");
    assert!(fixture.service.store.scope_ended(&run).unwrap());
    assert!(!fixture.service.store.scope_ended(&fixture.head).unwrap());
    assert_eq!(fixture.plex.state.lock().unwrap().describes, 2);
    assert!(fixture.plex.state.lock().unwrap().posts.is_empty());
}

#[tokio::test]
async fn typed_route_open_end_fences_bound_background_before_any_provider_call() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    fixture.plex.state.lock().unwrap().route_open_ended = true;
    let answer = fixture.post(true, "route-open-end").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(answer.data["reason"], "ended");
    assert!(fixture.service.store.scope_ended(&fixture.head).unwrap());
    assert_eq!(fixture.plex.state.lock().unwrap().describes, 0);
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn typed_reservation_route_end_fences_captured_background_without_describe_retry() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    fixture.partners(|partners| {
        partners.reserve = Arc::new(|_, _, _, _| {
            Err(Refusal::scoped(
                Code::ScopeEnded,
                "ended",
                crate::gh_shim_speech::ScopeRole::Authority,
            ))
        });
    });
    let answer = fixture.post(false, "reservation-route-end").await;
    assert_eq!(answer.data["refusal_code"], "task_gh_scope_ended");
    assert_eq!(answer.data["reason"], "ended");
    assert!(fixture.service.store.scope_ended(&fixture.head).unwrap());
    assert_eq!(fixture.plex.state.lock().unwrap().describes, 1);
    assert_eq!(fixture.plex.state.lock().unwrap().sends, 0);
}

#[tokio::test]
async fn captured_dispatch_survives_unscoped_head_reconnect_via_sdk_without_test_partners() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new_with_options(
        false,
        LaunchOptions {
            sdk_daemon: true,
            ..Default::default()
        },
    )
    .await;
    assert!(
        fixture.startup_error.is_none(),
        "{:?}",
        fixture.startup_error
    );
    assert!(
        fixture.service.test_partners().is_none(),
        "the real service/reserve delegation must be exercised"
    );
    let mut routes = RouteScopes::default();
    routes.speech = Some(fixture.service.clone());
    routes.push(&fixture.session, None);
    let mut params = fixture.params.clone();
    params["request_nonce"] = json!("sdk-head-reconnect");
    params["for_session"] = json!("cannot-replace-the-launch-owner");
    let answer = crate::gh_shim_relay::relay(
        &fixture.plex,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(answer.ok, "{:?}", answer.data);
    let daemon = fixture.sdk.as_ref().unwrap();
    let writes = daemon
        .requests()
        .into_iter()
        .filter(|request| request["arguments"]["op"] == "bot_request")
        .collect::<Vec<_>>();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0]["arguments"]["expect"],
        json!({"app_handle_id":"app-original","repo_binding_generation":4})
    );
    assert!(writes[0]["arguments"].get("for_session").is_none());
    assert!(daemon
        .opens()
        .iter()
        .filter(|open| !open["scope"].is_null())
        .all(|open| open["scope"]["scope_epoch"] == 7));
    assert_eq!(fixture.plex.state.lock().unwrap().posts.len(), 1);
    fixture
        .plex
        .state
        .lock()
        .unwrap()
        .unknown
        .insert(ScopeKey::from_stamp(&fixture.head));
    fixture.service.invalidate(&fixture.head);
    params["request_nonce"] = json!("sdk-head-unsynced");
    let refused = crate::gh_shim_relay::relay(
        &fixture.plex,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert_eq!(refused.data["refusal_code"], "task_gh_scope_unknown");
    assert_eq!(refused.data["reason"], "not_synced");
    assert_eq!(fixture.plex.state.lock().unwrap().posts.len(), 1);
}
