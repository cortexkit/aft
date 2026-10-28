use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

/// Which fake module a recorded call went to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Plexus,
}

/// Fake plexus. Every call's body is recorded; replies are scripted and
/// default to a completed reply.
/// `scoped` says whether this fake can open its plexus route under a scope.
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<(Target, String, Value)>>,
    plexus_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    scoped: bool,
    plexus_scopes: Mutex<Vec<Option<ScopeSelector>>>,
    read_binding: Mutex<Option<Result<String, String>>>,
    read_roots: Mutex<Vec<String>>,
}

impl FakeTransport {
    fn scoped() -> Self {
        Self {
            scoped: true,
            ..Self::default()
        }
    }

    /// The scope each plexus call's route was opened under, in call order.
    fn plexus_scopes(&self) -> Vec<Option<ScopeSelector>> {
        self.plexus_scopes.lock().unwrap().clone()
    }

    fn push_plexus(&self, reply: Result<Value, TransportError>) {
        self.plexus_replies.lock().unwrap().push_back(reply);
    }

    fn calls(&self) -> Vec<(Target, String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, target: Target) -> usize {
        self.calls()
            .iter()
            .filter(|(called, _, _)| *called == target)
            .count()
    }
}

impl RelayTransport for FakeTransport {
    fn read_repository(&self, ticket: &crate::gh_shim_ticket::Redeemed) -> Result<String, String> {
        let project_root = &ticket.project_root;
        self.read_roots
            .lock()
            .unwrap()
            .push(project_root.to_string());
        self.read_binding
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Ok("cortexkit/aft".into()))
    }
    async fn plexus(
        &self,
        _project_root: &str,
        session: &str,
        scope: Option<&ScopeSelector>,
        body: Value,
    ) -> Result<Value, TransportError> {
        assert!(
            scope.is_none() || self.scoped,
            "the relay asked a transport that cannot open scoped routes for one"
        );
        self.plexus_scopes.lock().unwrap().push(scope.cloned());
        self.calls
            .lock()
            .unwrap()
            .push((Target::Plexus, session.to_string(), body));
        let scripted = self.plexus_replies.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| {
            Ok(json!({"repo_binding_generation": 1, "result": {"status": "completed"}}))
        })
    }

    fn opens_scoped_routes(&self) -> bool {
        self.scoped
    }
}

fn golden() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/plexus/seen-marker-reaction.json"
    ))
    .unwrap()
}

#[test]
fn gh_shim_shared_relay_unknown_operation_is_not_bot_speech() {
    let transport = FakeTransport::scoped();
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue(
        "unknown-operation-session",
        "unknown-operation-call",
        ".",
    );
    let (reply, _) = run(
        &transport,
        "gh_shim.unknown",
        json!({"ticket": ticket.value().unwrap(), "request": envelope()}),
    );
    assert_eq!(reply.data["refusal_code"], "operation_unknown");
    assert!(transport.calls().is_empty());
}

fn envelope() -> Value {
    golden()["request"]["request"].clone()
}

fn run(transport: &FakeTransport, operation: &str, params: Value) -> (RelayReply, Vec<String>) {
    run_with_scopes(transport, operation, params, &RouteScopes::default())
}

fn run_with_scopes<T: RelayTransport>(
    transport: &T,
    operation: &str,
    params: Value,
    route_scopes: &RouteScopes,
) -> (RelayReply, Vec<String>) {
    let lines = Mutex::new(Vec::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(relay_transport_contract(
        transport,
        RelayCall {
            operation,
            params: &params,
            first_party: true,
            route_scopes,
        },
        &|line| lines.lock().unwrap().push(line.to_string()),
    ));
    (reply, lines.into_inner().unwrap())
}

fn bot_params(ticket: &str, nonce: &str) -> Value {
    json!({"ticket": ticket, "request_nonce": nonce, "request": envelope()})
}

/// A worker's request that names the head agent's session in every
/// caller-controlled field, with a fabricated ticket, is refused before any
/// outbound call.
#[test]
fn fabricated_ticket_with_a_typed_head_session_is_refused_before_any_call() {
    let head = crate::gh_shim_ticket::ScopedTicket::issue("ses-head", "call-head", "/p");
    assert!(
        head.value().is_some(),
        "the head has a live ticket of its own"
    );
    let transport = FakeTransport::default();
    let mut params = bot_params("0123456789abcdef0123456789abcdef", "nonce-fab");
    params["session"] = json!("ses-head");
    params["session_id"] = json!("ses-head");
    params["request"]["metadata"]["session"] = json!("ses-head");
    let (reply, _) = run(&transport, BOT_REQUEST_OPERATION, params);
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty(), "no mint and no plexus call");

    let mut absent = bot_params("", "nonce-absent");
    absent["session"] = json!("ses-head");
    let (reply, _) = run(&transport, BOT_REQUEST_OPERATION, absent);
    assert_eq!(reply.data["refusal_code"], "ticket_absent");
    assert!(transport.calls().is_empty());
}

/// The ticket alone chooses the session: a typed session in the body is
/// ignored even when the ticket is real.
#[test]
fn a_real_ticket_speaks_for_its_own_session_whatever_the_body_says() {
    let worker = crate::gh_shim_ticket::ScopedTicket::issue("ses-worker", "call-w", "/p");
    let transport = FakeTransport::scoped();
    let mut params = bot_params(worker.value().unwrap(), "nonce-typed");
    params["session"] = json!("ses-head");
    let mut scopes = scopes_for("ses-worker", Some(delegating_stamp()));
    scopes.push("ses-head", Some(stamp(Some("agent-head"), true, true)));
    let (reply, _) = run_with_scopes(&transport, BOT_REQUEST_OPERATION, params, &scopes);
    assert!(reply.ok, "{:?}", reply.data);
    let calls = transport.calls();
    assert_eq!(calls[0].0, Target::Plexus);
    assert_eq!(calls[0].1, "ses-worker");
    assert_eq!(
        transport.plexus_scopes(),
        vec![Some(scope_selector(&delegating_stamp()))],
        "the ticket's session's scope, not the typed head's"
    );
}

#[test]
fn untrusted_binds_are_refused_even_with_a_live_ticket() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-untrusted", "call-u", "/p");
    let transport = FakeTransport::default();
    let params = bot_params(live.value().unwrap(), "nonce-untrusted");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(relay_transport_contract(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: false,
            route_scopes: &RouteScopes::default(),
        },
        &|_| {},
    ));
    assert_eq!(reply.data["refusal_code"], "untrusted_principal");
    assert!(transport.calls().is_empty());
}

#[test]
fn relay_log_escapes_untrusted_fields_into_one_record() {
    let transport = FakeTransport::default();
    let mut params = bot_params("", "nonce\nforged record");
    params["request"]["action"] = json!("action\rforged");
    params["request"]["repository"] = json!("owner/repo\u{001b}[31m");
    let (_, lines) = run(&transport, BOT_REQUEST_OPERATION, params);
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].chars().any(char::is_control), "{:?}", lines[0]);
    assert!(transport.calls().is_empty());
}

#[tokio::test]
async fn closing_one_route_blocks_its_key_but_not_other_route_keys() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let leases = Arc::new(RouteLeases::default());
    leases.acquire("closing").await;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let closing_leases = leases.clone();
    let closing = tokio::spawn(async move {
        closing_leases
            .release_with("closing", |_| async move {
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
            })
            .await;
    });
    started_rx.await.unwrap();
    let mut same_key = std::pin::pin!(leases.acquire("closing"));
    let mut other_key = std::pin::pin!(leases.acquire("other"));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        same_key.as_mut().poll(&mut context),
        Poll::Pending
    ));
    assert!(
        matches!(other_key.as_mut().poll(&mut context), Poll::Ready(())),
        "closing a route must not hold the global lease mutex"
    );
    release_tx.send(()).unwrap();
    closing.await.unwrap();
    same_key.await;
}

/// A ticket issued to a background bash command spawned for an agent session
/// relays as that session. The exact plexus body is checked, and plexus's
/// expected request and reply come from its own fixture file.
#[cfg(unix)]
#[test]
fn agent_session_bash_ticket_relays_to_that_session_with_exact_bodies() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config {
        project_root: Some(project.path().to_path_buf()),
        storage_dir: Some(storage.path().to_path_buf()),
        experimental_bash_background: true,
        ..crate::config::Config::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    config.github.shim = true;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let seen = project.path().join("seen-ticket");
    let command = format!(
        "printf %s \"$AFT_GH_SHIM_TICKET\" > '{}'; sleep 2",
        seen.display()
    );
    let response = crate::bash_background::spawn(
        "req-relay",
        "ses-agent",
        &command,
        crate::bash_background::BashShell::Bash,
        std::path::PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(std::time::Duration::from_millis(30_000)),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(response.success, "spawn failed: {:?}", response.data);
    let task_id = response.data["task_id"].as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let ticket = loop {
        if let Ok(text) = std::fs::read_to_string(&seen) {
            if !text.is_empty() {
                break text;
            }
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(20));
    };

    let golden = golden();
    let transport = FakeTransport::scoped();
    transport.push_plexus(Ok(golden["success"].clone()));
    let nonce = golden["request"]["request_nonce"].as_str().unwrap();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, nonce),
        &scopes_for("ses-agent", Some(delegating_stamp())),
    );

    assert_eq!(reply, RelayReply::relayed(golden["success"].clone()));
    let calls = transport.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Target::Plexus);
    assert_eq!(calls[0].1, "ses-agent");
    // The fixture request still carries an `assertion` field. AFT's write is
    // that request with `assertion` removed and `op` set to `bot_request`.
    let mut expected_arguments = golden["request"].clone();
    expected_arguments["op"] = json!("bot_request");
    expected_arguments
        .as_object_mut()
        .unwrap()
        .remove("assertion");
    assert_eq!(
        calls[0].2,
        json!({"name": "github", "arguments": expected_arguments})
    );
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains(&format!("task={task_id}")),
        "{}",
        lines[0]
    );

    // A ticket from the shared default session is never issued.
    let default = crate::bash_background::spawn(
        "req-default",
        crate::protocol::DEFAULT_SESSION_ID,
        &format!(
            "printf %s \"${{AFT_GH_SHIM_TICKET:-none}}\" > '{}'",
            project.path().join("default-ticket").display()
        ),
        crate::bash_background::BashShell::Bash,
        std::path::PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(std::time::Duration::from_millis(30_000)),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(default.success);
    let default_path = project.path().join("default-ticket");
    let started = std::time::Instant::now();
    while !default_path.exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(std::fs::read_to_string(&default_path).unwrap(), "none");

    let _ = ctx.bash_background().kill(&task_id, "ses-agent");
}

#[test]
fn an_ended_commands_ticket_no_longer_redeems() {
    let pending = crate::gh_shim_ticket::PendingTicket::issue("ses-ended", "/p");
    let ticket = pending.value().unwrap().to_string();
    pending.bind_task("task-ended-relay");
    crate::gh_shim_ticket::revoke_task("task-ended-relay");
    let transport = FakeTransport::default();
    let (reply, _) = run(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, "nonce-ended"),
    );
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty());
}

#[test]
fn a_sent_request_without_a_reply_is_outcome_unknown() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-unknown", "call-o", "/p");
    let transport = FakeTransport::scoped();
    transport.push_plexus(Err(TransportError::OutcomeUnknown("timed out".to_string())));
    let (reply, _) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-unknown"),
        &scopes_for("ses-unknown", Some(delegating_stamp())),
    );
    assert_eq!(reply.data["refusal_code"], "outcome_unknown");
}

#[test]
fn bindings_read_is_relayed_under_the_same_ticket_gate() {
    let transport = FakeTransport::default();
    let (reply, _) = run(
        &transport,
        BINDINGS_READ_OPERATION,
        json!({"ticket": "ffffffffffffffffffffffffffffffff", "agent_id": "agent-fixture"}),
    );
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty());

    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-bindings", "call-b", "/p");
    let bindings = json!({
        "repo_binding_generation": 3,
        "bindings": [{"repository": "org/repo", "app_handle_id": "aft-alfonso", "agent_id": "agent-fixture"}],
    });
    transport.push_plexus(Ok(bindings.clone()));
    let (reply, _) = run(
        &transport,
        BINDINGS_READ_OPERATION,
        json!({
            "ticket": live.value().unwrap(),
            "agent_id": "agent-fixture",
            // A connection id from the caller is ignored; the daemon always
            // names its own handle connection.
            "connection_id": "github-handle-someone-else",
        }),
    );
    assert_eq!(reply, RelayReply::relayed(bindings));
    let calls = transport.calls();
    assert_eq!(calls.len(), 1, "a single plexus call, nothing minted");
    assert_eq!(transport.plexus_scopes(), vec![None]);
    assert_eq!(
        calls[0].2,
        json!({
            "name": "github",
            "arguments": {"op": "bindings.read", "connection_id": "github-handle-aft-alfonso"},
        })
    );
}

#[test]
fn relay_log_line_carries_session_task_and_nonce_but_no_ticket_or_scope() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-log", "call-log-7", "/p");
    let ticket = live.value().unwrap().to_string();
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, "nonce-log-42"),
        &scopes_for("ses-log", Some(delegating_stamp())),
    );
    assert!(reply.ok);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(
        line,
        "gh_shim relay: op=gh_shim.bot_request session=ses-log task=call-log-7 nonce=nonce-log-42 action=issue reaction repository=org/repo path=stamp outcome=completed"
    );
    assert!(!line.contains(&ticket));
    assert!(!line.contains("head-ref-1"));
}

#[test]
fn facade_reply_unwraps_a_tool_call_result() {
    let direct = json!({"repo_binding_generation": 1, "result": {"status": "completed"}});
    assert_eq!(facade_reply(&direct), direct);
    let wrapped = json!({
        "content": [{"type": "text", "text": direct.to_string()}],
        "isError": false,
    });
    assert_eq!(facade_reply(&wrapped), direct);
}

#[test]
fn facade_reply_prefers_direct_fields_over_inner_result_content() {
    let inner = json!({"status": "completed", "result": {"url": "https://example.test/comment"}});
    let direct = json!({
        "repo_binding_generation": 1,
        "result": inner,
        "content": [{"type": "text", "text": inner.to_string()}],
    });
    assert_eq!(facade_reply(&direct), direct);
    for field in ["result", "error", "repo_binding_generation"] {
        let value = json!({field: null, "content": [{"text": inner.to_string()}]});
        assert_eq!(facade_reply(&value), value, "direct {field} must win");
    }
}

/// A stamp as the daemon puts it on a head's tool route, owned by
/// prefrontal-core.
fn stamp(agent: Option<&str>, delegates: bool, owner_authorized: bool) -> ScopeStamp {
    ScopeStamp::new(
        subc_protocol::Principal::Reserved {
            module_id: "prefrontal-core".to_string(),
        },
        "head-ref-1",
        7,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(agent.map(str::to_string))
            .with_delegates(delegates),
    )
    .with_owner_authorized(owner_authorized)
}

fn delegating_stamp() -> ScopeStamp {
    stamp(Some("agent-fixture"), true, true)
}

#[test]
fn authority_never_widens_across_reconnect() {
    let original = delegating_stamp();
    let mut replacement = original.clone();
    replacement.scope_ref = "new-work-scope".into();
    replacement.scope_epoch = 99;
    let ticket = crate::gh_shim_ticket::Redeemed {
        session_id: "same-head-session".into(),
        task_id: "bash-original".into(),
        project_root: "/project".into(),
        read_repository: None,
        launch: Some(
            crate::gh_shim_speech::model::LaunchAuthority::classic(original.clone(), None).unwrap(),
        ),
        lifecycle_scope: Some(original.clone()),
    };
    let routes = scopes_for("same-head-session", Some(replacement));
    assert_eq!(bot_authority(&ticket, &routes), Some(original));
}

// Exercise relay admission rather than only the immutable-authority helper:
// a captured Call ticket must not adopt the session's replacement scope.
#[tokio::test]
async fn call_ticket_must_not_adopt_a_rebound_agent() {
    let original = delegating_stamp();
    let live = crate::bash_background::registry::with_scope_spawn(Some(original.clone()), || {
        crate::gh_shim_ticket::ScopedTicket::issue("audit-call-rebind", "call", "/p")
    });
    let redeemed = crate::gh_shim_ticket::redeem(live.value().unwrap()).unwrap();
    assert_eq!(redeemed.launch.unwrap().authority, original);
    let mut replacement = delegating_stamp();
    replacement.scope_ref = "different-head".into();
    replacement.scope_epoch = 99;
    replacement.attributes.agent_id = Some("agent-replacement".into());
    let routes = scopes_for("audit-call-rebind", Some(replacement));
    let mut params = bot_params(live.value().unwrap(), "audit-call-rebound-write");
    params["request"]["metadata"]["agent_id"] = json!("agent-replacement");
    let transport = FakeTransport::scoped();
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(
        transport.calls().is_empty(),
        "the Call ticket captured another head/agent, but relay forwarded under the replacement: {:?}",
        answer.data
    );
}

fn background_ticket(
    root: &std::path::Path,
    session: &str,
    stamp: &ScopeStamp,
) -> crate::gh_shim_ticket::PendingTicket {
    let launch = crate::gh_shim_speech::model::LaunchAuthority::classic(
        stamp.clone(),
        Some(launch_origin()),
    )
    .unwrap();
    let seed = crate::gh_shim_speech::service::LaunchSeed {
        launch: Ok(launch),
        lifecycle: Some(stamp.clone()),
        session: session.into(),
        harness: "fixture".into(),
        project_root: "/project".into(),
    };
    let ticket = crate::gh_shim_speech::service::with_launch(Some(seed), root, || {
        crate::bash_background::registry::with_scope_spawn(Some(stamp.clone()), || {
            crate::gh_shim_ticket::PendingTicket::issue(session, "/project")
        })
    });
    crate::gh_shim_ticket::speech_for_ticket(ticket.value().unwrap())
        .unwrap()
        .background
        .store(true, std::sync::atomic::Ordering::SeqCst);
    ticket
}

fn launch_origin() -> crate::gh_shim_speech::model::RouteOrigin {
    crate::gh_shim_speech::model::RouteOrigin {
        run_nonce: crate::gh_shim_speech::service::run_nonce().unwrap().into(),
        connection_generation: "control-fixture".into(),
        channel: 1,
        epoch: 1,
    }
}

#[tokio::test]
async fn production_foreground_without_capture_and_background_without_policy_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let relay_state = GhRelay::new(root.path().join("unused-connection.json"));
    let service = crate::gh_shim_speech::service::start(
        root.path(),
        &relay_state,
        std::sync::Arc::new(|_| {
            Box::pin(async { panic!("unconfirmed posting must not probe scopes") })
        }),
        "control-fixture".into(),
        true,
    );
    let stamp = delegating_stamp();
    let foreground =
        crate::gh_shim_ticket::ScopedTicket::issue("unconfirmed-post", "call", "/project");
    let mut params = bot_params(foreground.value().unwrap(), "foreground-write");
    params["request"]["metadata"]["agent_id"] = json!(stamp.attributes.agent_id);
    let mut routes = scopes_for("unconfirmed-post", Some(stamp.clone()));
    routes.speech = Some(service);
    let transport = FakeTransport::scoped();
    assert!(crate::gh_shim_speech::partner::require_reserved_send().is_ok());
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!answer.ok);
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(answer.data["reason"], "owner_unresolved");
    assert!(transport.calls().is_empty());

    let background = background_ticket(root.path(), "unconfirmed-post", &stamp);
    *crate::gh_shim_ticket::speech_for_ticket(background.value().unwrap())
        .unwrap()
        .failure
        .lock()
        .unwrap() = None;
    params["ticket"] = json!(background.value().unwrap());
    routes.push_origin(launch_origin(), Some(stamp.clone()));
    let bound = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!bound.ok);
    assert_eq!(
        bound.data["reason"], "start_policy_missing",
        "a bound route cannot authorize an uncaptured background ticket"
    );
    assert!(transport.calls().is_empty());
    routes.origins.clear();
    let mut reconnected = stamp;
    reconnected.scope_epoch += 1;
    routes.routes.clear();
    routes.push("unconfirmed-post", Some(reconnected));
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!answer.ok);
    assert_eq!(answer.data["reason"], "start_policy_missing");
    assert_eq!(answer.data["refusal_code"], "task_gh_authority_unavailable");
    assert_eq!(answer.data["message"], "this command's bot-write permission cannot be verified (start_policy_missing); start the command again from a live session, and tell the operator if it repeats.");
    assert!(transport.calls().is_empty());
}

#[tokio::test]
async fn posting_admission_never_serves_speech_without_a_subc_service() {
    let root = tempfile::tempdir().unwrap();
    crate::gh_shim_speech::service::record_service(root.path());
    let stamp = delegating_stamp();
    let live = background_ticket(root.path(), "standalone-post", &stamp);
    let mut params = bot_params(live.value().unwrap(), "no-serving-mode");
    params["request"]["metadata"]["agent_id"] = json!(stamp.attributes.agent_id);
    let routes = scopes_for("standalone-post", Some(stamp));
    let transport = FakeTransport::scoped();
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!answer.ok);
    assert_eq!(answer.data["reason"], "not_serving_mode");
    assert!(transport.calls().is_empty());
}

#[tokio::test]
async fn posting_admission_recorded_end_wins_over_live_rebind_and_partner_seams() {
    let root = tempfile::tempdir().unwrap();
    let relay_state = GhRelay::new(root.path().join("unused.json"));
    let service = crate::gh_shim_speech::service::start(
        root.path(),
        &relay_state,
        std::sync::Arc::new(|_| Box::pin(async { panic!("recorded end must not describe again") })),
        "control-fixture".into(),
        true,
    );
    let stamp = delegating_stamp();
    let ended = subc_protocol::session::ModuleControlResponseToModule::ScopeDescribe {
        status: subc_protocol::scope::ScopeStatus::Ended,
        scope_epoch: Some(stamp.scope_epoch),
        daemon_incarnation: "daemon-a".into(),
        owner_synced: true,
        owner_configured: true,
        scope: None,
    };
    let proof = match crate::gh_shim_speech::scope::validate_scope_identity(
        &stamp,
        crate::gh_shim_speech::ScopeRole::Authority,
        &ended,
        "control-fixture",
        "control-fixture",
        0,
    )
    .unwrap()
    {
        crate::gh_shim_speech::scope::Answer::Ended(proof) => proof,
        _ => panic!("expected end"),
    };
    service.record_end(&stamp, proof).unwrap();
    let live = background_ticket(root.path(), "ended-rebind", &stamp);
    let mut params = bot_params(live.value().unwrap(), "end-wins");
    params["request"]["metadata"]["agent_id"] = json!(stamp.attributes.agent_id);
    let mut routes = scopes_for("ended-rebind", Some(stamp));
    routes.speech = Some(service);
    let transport = FakeTransport::scoped();
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert!(!answer.ok);
    assert_eq!(answer.data["reason"], "ended_previously");
    assert!(transport.calls().is_empty());
}

fn scopes_for(session: &str, stamp: Option<ScopeStamp>) -> RouteScopes {
    let mut scopes = RouteScopes::default();
    scopes.push(session, stamp);
    scopes
}

/// A delegating scope on the session's route sends the write under that scope,
/// and the body sent to plexus carries no `assertion` field.
#[test]
fn a_delegating_stamped_route_sends_the_write_under_its_scope() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-stamp", "call-stamp", "/p");
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-stamp"),
        &scopes_for("ses-stamp", Some(delegating_stamp())),
    );

    assert!(reply.ok, "{:?}", reply.data);
    let calls = transport.calls();
    assert_eq!(calls.len(), 1, "one plexus call per invocation");
    assert_eq!(calls[0].0, Target::Plexus);
    assert_eq!(calls[0].1, "ses-stamp");
    assert_eq!(
        calls[0].2,
        json!({
            "name": "github",
            "arguments": {
                "op": "bot_request",
                "request_nonce": "nonce-stamp",
                "request": envelope(),
            },
        }),
        "the write is unchanged except that it carries no assertion"
    );
    assert_eq!(
        transport.plexus_scopes(),
        vec![Some(ScopeSelector::new(
            subc_protocol::Principal::Reserved {
                module_id: "prefrontal-core".to_string(),
            },
            "head-ref-1",
            Some(7),
        ))],
        "the plexus route is opened under the session's scope at its epoch"
    );
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);
    assert!(!lines[0].contains("head-ref-1"), "{}", lines[0]);
}

#[test]
fn head_tool_and_background_routes_use_the_stamp() {
    for harness in ["opencode", "pi"] {
        let session = format!("{harness}-head");
        let ticket = crate::gh_shim_ticket::ScopedTicket::issue(&session, "head-write", "/p");
        let transport = FakeTransport::scoped();
        let mut routes = scopes_for(&session, Some(delegating_stamp()));
        // The tool-call route and the idle completion subscription's separate route must carry the same stamp.
        routes.push(&session, Some(delegating_stamp()));
        let (reply, _) = run_with_scopes(
            &transport,
            BOT_REQUEST_OPERATION,
            bot_params(ticket.value().unwrap(), "head-write-nonce"),
            &routes,
        );
        assert!(reply.ok, "{:?}", reply.data);
        assert_eq!(transport.count(Target::Plexus), 1);
        assert_eq!(
            transport.plexus_scopes(),
            vec![Some(scope_selector(&delegating_stamp()))]
        );
        assert!(transport.calls()[0].2["arguments"]
            .get("assertion")
            .is_none());
    }
}

/// Plexus's scope refusals reach the shim exactly as plexus sent them,
/// `legs_sent` included, and the write is never sent a second time.
#[test]
fn a_stamp_path_refusal_is_relayed_verbatim_with_no_retry() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-stamp-refused", "call-sr", "/p");
    for code in [
        "scope_unverifiable",
        "scope_ended",
        "delegation_withdrawn",
        "agent_identity_conflict",
    ] {
        let refusal = json!({
            "repo_binding_generation": 1,
            "result": {
                "status": "refused",
                "refusal_code": code,
                "legs_sent": ["add_reaction"],
            },
        });
        let transport = FakeTransport::scoped();
        transport.push_plexus(Ok(refusal.clone()));
        let (reply, lines) = run_with_scopes(
            &transport,
            BOT_REQUEST_OPERATION,
            bot_params(live.value().unwrap(), "nonce-stamp-refused"),
            &scopes_for("ses-stamp-refused", Some(delegating_stamp())),
        );
        assert_eq!(reply, RelayReply::relayed(refusal), "{code}");
        assert_eq!(transport.count(Target::Plexus), 1, "{code}: no second send");
        assert!(transport.calls()[0].2["arguments"]
            .get("assertion")
            .is_none());
        assert!(
            lines[0].ends_with(&format!("path=stamp outcome={code}")),
            "{}",
            lines[0]
        );
    }

    // A refusal from the subc layer on the stamp path is not retried either.
    let transport = FakeTransport::scoped();
    transport.push_plexus(Err(TransportError::Refused {
        code: "scope_ended".to_string(),
        message: "the scope ended".to_string(),
    }));
    let (reply, _) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-stamp-subc"),
        &scopes_for("ses-stamp-refused", Some(delegating_stamp())),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "scope_ended");
    assert_eq!(reply.data["stage"], "plexus");
    assert_eq!(transport.count(Target::Plexus), 1);
}

/// Plexus takes the agent from a delegating stamp, so a request naming a
/// different agent is refused before anything is sent rather than posted as
/// the stamp's bot.
#[test]
fn a_stamp_naming_another_agent_is_refused_before_any_call() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-other-agent", "call-oa", "/p");
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-other-agent"),
        &scopes_for(
            "ses-other-agent",
            Some(stamp(Some("agent-someone-else"), true, true)),
        ),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "agent_identity_conflict");
    assert_eq!(reply.data["stage"], "scope");
    assert!(transport.calls().is_empty());
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);
}

/// A session whose route carries no stamp has no agent to speak as. Its bot
/// write is refused before anything is sent: the daemon mints no assertions.
#[test]
fn an_unstamped_route_is_refused_before_any_call() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-unstamped", "call-un", "/p");
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-unstamped"),
        &scopes_for("ses-unstamped", None),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "bot_write_requires_scope");
    assert_eq!(reply.data["stage"], "scope");
    assert_eq!(reply.data["repository"], envelope()["repository"]);
    assert_eq!(reply.data["action"], envelope()["action"]);
    assert!(
        reply.data["message"]
            .as_str()
            .unwrap()
            .contains("Restart the host session"),
        "{:?}",
        reply.data
    );
    assert!(transport.calls().is_empty(), "nothing reached plexus");
    assert!(lines[0].contains(" path=- "), "{}", lines[0]);
    assert!(
        lines[0].ends_with("outcome=bot_write_requires_scope"),
        "{}",
        lines[0]
    );
}

#[test]
fn v19_pr_ready_without_a_bot_identity_is_refused_before_any_call() {
    let live =
        crate::gh_shim_ticket::ScopedTicket::issue("ses-ready-no-bot", "call-ready-no-bot", "/p");
    for scope in [None, Some(stamp(None, true, true))] {
        for undo in [false, true] {
            let transport = FakeTransport::scoped();
            let mut params = bot_params(live.value().unwrap(), "nonce-ready-no-bot");
            params["request"] = json!({
                "operation": "gh.route", "gh_route_schema": 1,
                "verb": "pr ready", "repository": "cortexkit/aft", "number": "7",
                "undo": undo, "manifest_version": 19,
                "metadata": { "agent_id": "agent-fixture" }
            });
            let (reply, _) = run_with_scopes(
                &transport,
                BOT_REQUEST_OPERATION,
                params,
                &scopes_for("ses-ready-no-bot", scope.clone()),
            );
            assert!(!reply.ok);
            assert_eq!(reply.data["refusal_code"], "bot_write_requires_scope");
            assert_eq!(reply.data["stage"], "scope");
            assert_eq!(reply.data["action"], "pr ready");
            assert!(transport.calls().is_empty());
        }
    }
}

#[test]
fn v19_pr_ready_is_forwarded_unchanged_under_the_callers_bot_scope() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-ready", "call-ready", "/p");
    for undo in [false, true] {
        let transport = FakeTransport::scoped();
        let request = json!({
            "operation": "gh.route", "gh_route_schema": 1,
            "verb": "pr ready", "repository": "cortexkit/aft", "number": "7",
            "undo": undo, "manifest_version": 19,
            "metadata": { "agent_id": "agent-fixture" }
        });
        let (reply, _) = run_with_scopes(
            &transport,
            BOT_REQUEST_OPERATION,
            json!({"ticket": live.value().unwrap(), "request_nonce": "nonce-ready", "request": request}),
            &scopes_for("ses-ready", Some(delegating_stamp())),
        );
        assert!(reply.ok, "{:?}", reply.data);
        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "ses-ready");
        assert_eq!(calls[0].2["arguments"]["request"], request);
        assert!(transport.plexus_scopes()[0].is_some());
    }
}

/// A stamp that does not let plexus act as its agent (not delegating, an
/// owner the daemon does not trust with agent identity, or no agent) is
/// refused exactly as an unstamped route is.
#[test]
fn a_non_delegating_stamp_is_refused_before_any_call() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-nondeleg", "call-nd", "/p");
    for (label, non_delegating) in [
        ("not delegating", stamp(Some("agent-fixture"), false, true)),
        (
            "owner not authorized",
            stamp(Some("agent-fixture"), true, false),
        ),
        ("no agent", stamp(None, true, true)),
    ] {
        assert_eq!(delegating_agent(&non_delegating), None, "{label}");
        let transport = FakeTransport::scoped();
        let (reply, _) = run_with_scopes(
            &transport,
            BOT_REQUEST_OPERATION,
            bot_params(live.value().unwrap(), "nonce-nondeleg"),
            &scopes_for("ses-nondeleg", Some(non_delegating)),
        );
        assert_eq!(
            reply.data["refusal_code"], "bot_write_requires_scope",
            "{label}"
        );
        assert!(transport.calls().is_empty(), "{label}");
    }
}

/// The production transport opens scoped routes, so a write under a delegating
/// stamp is sent to plexus. With no subc connection, that plexus call fails.
/// A transport that cannot open a scoped route refuses the same write locally,
/// because the scope is the only way a write names its agent.
#[test]
fn production_transport_takes_the_stamp_path_for_a_delegating_stamp() {
    let production = SubcRelayTransport::new(std::path::PathBuf::from(
        "/nonexistent/aft-gh-relay-test/connection.json",
    ));
    assert!(production.opens_scoped_routes());
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-prod", "call-prod", "/p");
    let scopes = scopes_for("ses-prod", Some(delegating_stamp()));
    let (reply, lines) = run_with_scopes(
        &production,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-prod"),
        &scopes,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["stage"], "plexus", "{:?}", reply.data);
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);

    let unscoped = FakeTransport::default();
    let (reply, lines) = run_with_scopes(
        &unscoped,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-prod-fake"),
        &scopes,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "bot_write_requires_scope");
    assert!(unscoped.calls().is_empty());
    assert!(lines[0].contains(" path=- "), "{}", lines[0]);
}

/// A session's stamp is used only when every route bound for it carries the
/// same one; otherwise the relay cannot tell which route ran the command.
#[test]
fn route_scopes_answer_only_when_the_sessions_routes_agree() {
    let mut scopes = RouteScopes::default();
    scopes.push("ses-a", Some(delegating_stamp()));
    scopes.push("ses-a", Some(delegating_stamp()));
    scopes.push("ses-other", None);
    assert_eq!(scopes.for_session("ses-a"), Some(&delegating_stamp()));
    assert_eq!(scopes.for_session("ses-missing"), None);
    assert_eq!(scopes.for_session("ses-other"), None);

    let mut mixed = RouteScopes::default();
    mixed.push("ses-b", Some(delegating_stamp()));
    mixed.push("ses-b", None);
    assert_eq!(mixed.for_session("ses-b"), None);
    let mut unstamped_first = RouteScopes::default();
    unstamped_first.push("ses-c", None);
    unstamped_first.push("ses-c", Some(delegating_stamp()));
    assert_eq!(unstamped_first.for_session("ses-c"), None);

    let mut newer_epoch = delegating_stamp();
    newer_epoch.scope_epoch += 1;
    let mut disagreeing = RouteScopes::default();
    disagreeing.push("ses-d", Some(delegating_stamp()));
    disagreeing.push("ses-d", Some(newer_epoch));
    assert_eq!(disagreeing.for_session("ses-d"), None);
}

// ---------------------------------------------------------------------------
// The production transport against a fake subc daemon, over real subc framing.

/// What the fake daemon does beyond opening routes and echoing a completed
/// reply to every request.
#[derive(Clone, Default)]
struct DaemonScript {
    reply: Option<Arc<dyn Fn(&Value, Option<&Value>) -> Value + Send + Sync>>,
    scoped_open_pause: Option<Arc<LeaseTestPause>>,
    /// Refuse every route.open that carries a scope with this code.
    refuse_scoped_opens: Option<String>,
    /// Before writing each scoped route.open refusal, wait this long in real
    /// time, as a slow runner would. The wait runs on a blocking thread, so
    /// it neither stalls the runtime nor moves a paused test clock.
    scoped_refusal_delay: Option<std::time::Duration>,
    /// After each scoped route.open refusal, push a channel-0 notice with op
    /// `test.route_open_refused`. The client reads one connection's frames in
    /// order and hands each refusal to its waiting route open as it reads it,
    /// so a test that receives this notice from `SubcConsumer::control_pushes`
    /// knows the client already holds the refusal sent just before it.
    announce_scoped_refusals: bool,
    /// After answering the first request on a scoped route, push a
    /// `route.closed` for plexus with reason `scope_ended`, and refuse every
    /// later scoped route.open with `scope_ended`, as the daemon does once a
    /// scope is removed.
    end_scope_after_first_write: bool,
    /// Hold the reply to the first request until a second request arrives;
    /// answer the second at once and the first 200 ms later.
    hold_first_request: bool,
}

/// Everything the fake daemon was sent.
#[derive(Default)]
struct DaemonLog {
    /// Every route.open body, refused ones included.
    opens: Vec<Value>,
    /// The channel of every route the client closed.
    closes: Vec<Value>,
    /// Every request body sent on an open route.
    requests: Vec<Value>,
    connections_closed: usize,
}

struct DaemonConnection(Arc<Mutex<DaemonLog>>);

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.0.lock().unwrap().connections_closed += 1;
    }
}

pub(crate) struct FakeDaemon {
    log: Arc<Mutex<DaemonLog>>,
    connection_file: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl FakeDaemon {
    pub(crate) fn opens(&self) -> Vec<Value> {
        self.log.lock().unwrap().opens.clone()
    }

    /// The routes closed so far, waiting briefly for the client's GOODBYE,
    /// which it sends without waiting for an answer.
    async fn closes(&self, expected: usize) -> Vec<Value> {
        for _ in 0..200 {
            let closes = self.log.lock().unwrap().closes.clone();
            if closes.len() >= expected {
                return closes;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        self.log.lock().unwrap().closes.clone()
    }

    pub(crate) fn requests(&self) -> Vec<Value> {
        self.log.lock().unwrap().requests.clone()
    }

    pub(crate) async fn transport(&self) -> SubcRelayTransport {
        self.transport_with_deadline(None).await
    }

    async fn transport_with_deadline(
        &self,
        route_open_deadline: Option<std::time::Duration>,
    ) -> SubcRelayTransport {
        SubcRelayTransport::with_consumer(self.consumer().await, route_open_deadline)
    }

    async fn consumer(&self) -> subc_client_rs::SubcConsumer {
        subc_client_rs::SubcConsumer::connect(
            &self.connection_file,
            subc_client_rs::ConsumerOptions {
                call_timeout: std::time::Duration::from_secs(5),
                ..subc_client_rs::ConsumerOptions::default()
            },
        )
        .await
        .expect("the fake daemon accepts the relay's connection")
    }
}

#[tokio::test]
async fn stale_consumer_failure_does_not_clear_a_replacement_connection() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let transport = daemon.transport().await;
    let failed = transport.consumer().await.unwrap();
    let replacement_transport = daemon.transport().await;
    let replacement = replacement_transport.consumer().await.unwrap();
    assert!(!Arc::ptr_eq(&failed, &replacement));
    *transport.consumer.lock().await = Some(Arc::clone(&replacement));
    transport.clear_failed_consumer(&failed).await;
    assert!(transport
        .consumer
        .lock()
        .await
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &replacement)));
    transport.clear_failed_consumer(&replacement).await;
    assert!(transport.consumer.lock().await.is_none());
}

/// Serve one subc connection per accept on the current runtime, as `script`
/// says, recording what arrives.
async fn fake_daemon(script: DaemonScript) -> FakeDaemon {
    use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let log = Arc::new(Mutex::new(DaemonLog::default()));
    let server_log = Arc::clone(&log);
    let server_key = key.clone();
    tokio::spawn(async move {
        let flags = Flags::new(false, Priority::Passive, false);
        while let Ok((mut stream, _)) = listener.accept().await {
            let key = server_key.clone();
            let log = Arc::clone(&server_log);
            let script = script.clone();
            tokio::spawn(async move {
                let _connection = DaemonConnection(log.clone());
                if subc_transport::authenticate_server(
                    &mut stream,
                    &key,
                    &daemon_id,
                    "subc-test",
                    std::time::Duration::from_secs(5),
                )
                .await
                .is_err()
                {
                    return;
                }
                let mut next_channel: u16 = 40;
                let mut scoped_channels = Vec::new();
                let mut route_scopes = std::collections::HashMap::new();
                let mut scope_ended = false;
                let mut held = None;
                let mut deferred = Vec::new();
                while let Ok(Some(frame)) = subc_transport::read_frame(&mut stream).await {
                    let header = frame.header;
                    let mut replies = Vec::new();
                    match header.ty {
                        FrameType::Hello => replies.push(
                            Frame::build(
                                FrameType::HelloAck,
                                flags,
                                0,
                                0,
                                header.corr,
                                serde_json::to_vec(&ModuleHelloAckBody {
                                    negotiated_ver: PROTOCOL_VERSION,
                                    subc_ops: Vec::new(),
                                    subc_capabilities: Vec::new(),
                                    storage: None,
                                    machine_id: None,
                                })
                                .unwrap(),
                            )
                            .unwrap(),
                        ),
                        FrameType::Request => {
                            let body: Value =
                                serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                            let respond = |response: Value| {
                                Frame::build_with_version(
                                    header.ver,
                                    FrameType::Response,
                                    header.flags,
                                    header.channel,
                                    header.epoch,
                                    header.corr,
                                    serde_json::to_vec(&response).unwrap(),
                                )
                                .unwrap()
                            };
                            if header.channel != 0 {
                                let scoped = scoped_channels.contains(&header.channel);
                                let seen = {
                                    let mut log = log.lock().unwrap();
                                    log.requests.push(body.clone());
                                    log.requests.len()
                                };
                                let response = script.reply.as_ref().map(|reply| reply(&body,route_scopes.get(&header.channel)))
                                    .unwrap_or_else(|| json!({"repo_binding_generation":1,"result":{"status":"completed"}}));
                                let reply = respond(response);
                                if script.hold_first_request && seen == 1 {
                                    held = Some(reply);
                                    continue;
                                }
                                replies.push(reply);
                                if let Some(first) = held.take() {
                                    deferred.push(first);
                                }
                                if script.end_scope_after_first_write && scoped && !scope_ended {
                                    scope_ended = true;
                                    replies.push(
                                        Frame::build(
                                            FrameType::Push,
                                            flags,
                                            0,
                                            0,
                                            0,
                                            serde_json::to_vec(&json!({
                                                "op": "route.closed",
                                                "module_id": PLEXUS_MODULE_ID,
                                                "reason": "scope_ended",
                                                "drained": false,
                                                "abandoned": 0,
                                                "excluded_subscriptions": 0,
                                            }))
                                            .unwrap(),
                                        )
                                        .unwrap(),
                                    );
                                }
                            } else {
                                match body.get("op").and_then(Value::as_str) {
                                    Some("route.open") => {
                                        log.lock().unwrap().opens.push(body.clone());
                                        let scoped =
                                            body.get("scope").is_some_and(|s| !s.is_null());
                                        let refusal =
                                            script.refuse_scoped_opens.clone().or_else(|| {
                                                scope_ended.then(|| "scope_ended".to_string())
                                            });
                                        match refusal.as_deref() {
                                            Some(code) if scoped => {
                                                if let Some(delay) = script.scoped_refusal_delay {
                                                    let _ =
                                                        tokio::task::spawn_blocking(move || {
                                                            std::thread::sleep(delay)
                                                        })
                                                        .await;
                                                }
                                                replies.push(
                                                    Frame::build(
                                                        FrameType::Error,
                                                        flags,
                                                        0,
                                                        0,
                                                        header.corr,
                                                        serde_json::to_vec(
                                                            &subc_protocol::ErrorBody::new(
                                                                code,
                                                                "refused by the fake daemon",
                                                            ),
                                                        )
                                                        .unwrap(),
                                                    )
                                                    .unwrap(),
                                                );
                                                if script.announce_scoped_refusals {
                                                    replies.push(
                                                        Frame::build(
                                                            FrameType::Push,
                                                            flags,
                                                            0,
                                                            0,
                                                            0,
                                                            serde_json::to_vec(&json!({
                                                                "op": "test.route_open_refused",
                                                                "code": code,
                                                            }))
                                                            .unwrap(),
                                                        )
                                                        .unwrap(),
                                                    );
                                                }
                                            }
                                            _ => {
                                                if scoped {
                                                    if let Some(pause) = &script.scoped_open_pause {
                                                        pause.acquired.notify_one();
                                                        pause.resume.notified().await;
                                                    }
                                                }
                                                next_channel += 1;
                                                if scoped {
                                                    scoped_channels.push(next_channel);
                                                    route_scopes.insert(
                                                        next_channel,
                                                        body["scope"].clone(),
                                                    );
                                                }
                                                replies.push(respond(json!({
                                                    "op": "route.open",
                                                    "route_channel": next_channel,
                                                    "route_epoch": 1,
                                                })));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // The client closes a route by sending a GOODBYE frame on the route's own
                        // channel.
                        FrameType::Goodbye if header.channel != 0 => {
                            log.lock()
                                .unwrap()
                                .closes
                                .push(json!({"channel": header.channel}));
                        }
                        _ => {}
                    }
                    for reply in replies {
                        if subc_transport::write_frame(&mut stream, &reply)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    // A held reply goes out a little after the one that
                    // released it, so the second call finishes first.
                    for reply in std::mem::take(&mut deferred) {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        if subc_transport::write_frame(&mut stream, &reply)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let connection_file = dir.path().join("connection.json");
    connection_file::write_atomic(
        &connection_file,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_string(),
                port,
            }],
            key,
            daemon_id,
            pid: std::process::id(),
            daemon_ver: "gh-shim-relay-scope-test".to_string(),
        },
    )
    .unwrap();
    FakeDaemon {
        log,
        connection_file,
        _dir: dir,
    }
}

fn is_scoped_open(open: &Value) -> bool {
    open.get("scope").is_some_and(|scope| !scope.is_null())
}

async fn relay_stamped(
    transport: &SubcRelayTransport,
    session: &str,
    nonce: &str,
) -> (RelayReply, Vec<String>) {
    let live = crate::gh_shim_ticket::ScopedTicket::issue(session, "call-scoped", "/p");
    let params = bot_params(live.value().unwrap(), nonce);
    let scopes = scopes_for(session, Some(delegating_stamp()));
    let lines = Mutex::new(Vec::new());
    let reply = relay_transport_contract(
        transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &scopes,
        },
        &|line| lines.lock().unwrap().push(line.to_string()),
    )
    .await;
    (reply, lines.into_inner().unwrap())
}

/// A delegating stamp on the production transport opens plexus's route under
/// the stamp's scope, pinned to its epoch, sends the write with no assertion,
/// mints nothing, and closes the route through its handle.
#[tokio::test]
async fn production_transport_opens_a_delegating_stamps_route_under_its_scope() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let transport = daemon.transport().await;
    let (reply, lines) = relay_stamped(&transport, "ses-scoped-open", "nonce-scoped-open").await;
    assert!(reply.ok, "{:?}", reply.data);
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);

    let opens = daemon.opens();
    assert_eq!(
        opens.len(),
        1,
        "one route, to plexus, and no mint: {opens:?}"
    );
    assert_eq!(
        opens[0]["scope"],
        serde_json::to_value(scope_selector(&delegating_stamp())).unwrap()
    );
    assert_eq!(opens[0]["scope"]["scope_epoch"], 7);
    assert!(
        opens[0].to_string().contains(PLEXUS_MODULE_ID),
        "{}",
        opens[0]
    );
    let requests = daemon.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]["arguments"].get("assertion").is_none());
    let closes = daemon.closes(1).await;
    assert_eq!(
        closes.len(),
        1,
        "the scoped route is closed after the write"
    );
    assert_eq!(
        closes[0]["channel"], 41,
        "closed through its handle, on its own channel"
    );
}

/// Each terminal scope refusal of the route open reaches the shim under its
/// own code. Nothing else is opened afterwards: no unscoped route to plexus,
/// and no route to prefrontal for an assertion.
#[tokio::test]
async fn a_scope_refusal_on_route_open_is_relayed_typed_and_never_reopened_unscoped() {
    for code in [
        "scope_not_live",
        "scope_ended",
        "scope_epoch_required",
        "scope_not_carrier",
    ] {
        let daemon = fake_daemon(DaemonScript {
            refuse_scoped_opens: Some(code.to_string()),
            ..DaemonScript::default()
        })
        .await;
        let transport = daemon.transport().await;
        let (reply, lines) = relay_stamped(&transport, "ses-scope-refused", "nonce-refused").await;
        assert!(!reply.ok, "{code}");
        assert_eq!(reply.data["refusal_code"], code);
        assert_eq!(reply.data["stage"], "plexus", "{code}");
        assert!(
            lines[0].ends_with(&format!("path=stamp outcome={code}")),
            "{}",
            lines[0]
        );
        let opens = daemon.opens();
        assert_eq!(opens.len(), 1, "{code}: exactly one route.open: {opens:?}");
        assert!(is_scoped_open(&opens[0]), "{code}");
        assert!(daemon.requests().is_empty(), "{code}: nothing was sent");
    }
}

/// After the daemon closes a scoped route because its scope was removed, the
/// next write for that session opens a fresh route under the same scope, and
/// the daemon's typed refusal of that open reaches the shim as it is: no
/// unscoped route, no assertion mint, nothing sent.
#[tokio::test]
async fn a_write_after_a_scope_close_relays_the_daemons_typed_refusal() {
    let daemon = fake_daemon(DaemonScript {
        end_scope_after_first_write: true,
        ..DaemonScript::default()
    })
    .await;
    let transport = daemon.transport().await;
    let (first, _) = relay_stamped(&transport, "ses-scope-closed", "nonce-closed-1").await;
    assert!(first.ok, "{:?}", first.data);

    let (again, lines) = relay_stamped(&transport, "ses-scope-closed", "nonce-closed-2").await;
    assert!(!again.ok);
    assert_eq!(again.data["refusal_code"], "scope_ended");
    assert_eq!(again.data["stage"], "plexus");
    assert!(again.data.get("retryable").is_none(), "{:?}", again.data);
    assert!(
        lines[0].ends_with("path=stamp outcome=scope_ended"),
        "{}",
        lines[0]
    );
    let opens = daemon.opens();
    assert_eq!(opens.len(), 2, "one open per write: {opens:?}");
    assert!(opens.iter().all(is_scoped_open), "{opens:?}");
    assert_eq!(
        daemon.requests().len(),
        1,
        "the refused write was never sent"
    );
}

/// Two overlapping writes for one session share the client's cached route.
/// The one that finishes first leaves the route open for the other, and the
/// route is closed once, after both.
#[tokio::test]
async fn overlapping_writes_for_one_session_keep_their_shared_route_until_both_finish() {
    let daemon = fake_daemon(DaemonScript {
        hold_first_request: true,
        ..DaemonScript::default()
    })
    .await;
    let transport = daemon.transport().await;
    let selector = scope_selector(&delegating_stamp());
    let body = || json!({"name": "github", "arguments": {"op": "bot_request"}});

    let first = transport.plexus("/p", "ses-overlap", Some(&selector), body());
    let second = async {
        while daemon.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        transport
            .plexus("/p", "ses-overlap", Some(&selector), body())
            .await
    };
    let (first, second) = tokio::join!(first, second);
    assert!(second.is_ok(), "{second:?}");
    assert!(first.is_ok(), "the first write lost its route: {first:?}");
    assert_eq!(daemon.opens().len(), 1, "both writes used one route");
    assert_eq!(daemon.requests().len(), 2);
    assert_eq!(daemon.closes(1).await.len(), 1);
}

/// When the daemon keeps refusing a scoped open for a reason the client
/// retries (`scope_not_synced`, `scope_changed`) until the deadline runs out,
/// the shim gets that code, marked retryable.
///
/// The subc client measures that deadline on tokio's clock, so the test
/// pauses it and moves it by hand. The deadline then passes only after the
/// refusal has reached the client, while the client waits to retry, which is
/// the case this test is about. On the real clock a slow runner can let the
/// deadline pass while a route open is still waiting for its answer; the
/// client then reports that open's outcome as unknown, and the relay answers
/// `relay_unavailable`, which is right for that case but is not the outcome
/// asserted here. The fake daemon delays each refusal by more than the
/// deadline in real time, so the test also shows that real elapsed time no
/// longer decides the outcome.
#[tokio::test(start_paused = true)]
async fn a_retried_scope_refusal_that_outlasts_the_deadline_keeps_its_code() {
    const DEADLINE: std::time::Duration = std::time::Duration::from_millis(300);
    // A paused clock still jumps to the next timer whenever the runtime has
    // nothing to run, which includes waiting on a socket. A running blocking
    // task stops those jumps (see `tokio::time::pause`), so the clock moves
    // only through `advance` below. Dropping `_release`, also on a panic, ends
    // the blocking task.
    let (_release, held) = std::sync::mpsc::channel::<()>();
    let _hold_clock = tokio::task::spawn_blocking(move || {
        let _ = held.recv();
    });
    for code in ["scope_not_synced", "scope_changed"] {
        let daemon = fake_daemon(DaemonScript {
            refuse_scoped_opens: Some(code.to_string()),
            scoped_refusal_delay: Some(DEADLINE + std::time::Duration::from_millis(100)),
            announce_scoped_refusals: true,
            ..DaemonScript::default()
        })
        .await;
        let consumer = daemon.consumer().await;
        let mut announced = consumer.control_pushes(8);
        let transport = SubcRelayTransport::with_consumer(consumer, Some(DEADLINE));
        let expire = async {
            // The channel closes without a notice only when a failed call
            // has dropped the client's connection; the assertions on the
            // reply below then name the failure.
            if let Some(push) = announced.recv().await {
                assert_eq!(push.op, "test.route_open_refused", "{code}");
                tokio::time::advance(DEADLINE).await;
            }
        };
        let ((reply, _), ()) = tokio::join!(
            relay_stamped(&transport, "ses-scope-retried", "nonce-retried"),
            expire
        );
        assert!(!reply.ok, "{code}");
        assert_eq!(reply.data["refusal_code"], code, "{:?}", reply.data);
        assert_eq!(reply.data["retryable"], true, "{code}");
        assert_eq!(reply.data["stage"], "plexus", "{code}");
        let opens = daemon.opens();
        // One open reached the daemon: the retry after its refusal came due
        // only once the deadline had passed, so the client never sent it.
        assert_eq!(opens.len(), 1, "{code}: {opens:?}");
        assert!(is_scoped_open(&opens[0]), "{code}: {opens:?}");
        assert!(daemon.requests().is_empty(), "{code}: nothing was sent");
    }
}

/// AFT makes its plexus calls as itself, never as a relay of another
/// principal's tool call, so the body names no `origin` and keeps the plain
/// two-field shape plexus reads.
#[test]
fn plexus_calls_name_no_origin() {
    let body = github_call(json!({"op": "bindings.read"}));
    assert_eq!(
        body,
        json!({"name": "github", "arguments": {"op": "bindings.read"}})
    );
    let call: subc_protocol::tool_call::ToolCallRequest = serde_json::from_value(body).unwrap();
    assert_eq!(call.origin, None);
}

/// The relay never asks prefrontal to mint an agent assertion: agent identity
/// reaches plexus only through a session's scope stamp.
#[test]
fn the_relay_has_no_assertion_mint_call() {
    let source = include_str!("mod.rs");
    assert!(!source.contains("assertion_mint"));
    assert!(!source.contains("ManagementSurface"));
}

#[test]
fn session_context_uses_the_redeemed_ticket_and_authenticated_worker_stamp() {
    use subc_protocol::scope::ScopeKind;
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("ses-context", "context-call", "/p");
    let transport = FakeTransport::scoped();
    for (kind, parented, expected) in [
        (ScopeKind::Head, false, false),
        (ScopeKind::Worker, false, true),
        (ScopeKind::Ephemeral, false, false),
        (ScopeKind::Ephemeral, true, true),
    ] {
        let mut scope = serde_json::to_value(delegating_stamp()).unwrap();
        scope["kind"] = serde_json::to_value(kind).unwrap();
        if parented {
            scope["parent"] = json!({"owner":scope["owner"],"ref":"head-parent","scope_epoch":7});
        }
        let scope: ScopeStamp = serde_json::from_value(scope).unwrap();
        let (reply, _) = run_with_scopes(
            &transport,
            SESSION_CONTEXT_OPERATION,
            json!({"ticket":ticket.value().unwrap(),"worker":!expected,"session_id":"ses-forged"}),
            &scopes_for("ses-context", Some(scope)),
        );
        assert!(reply.ok, "{:?}", reply.data);
        assert_eq!(reply.data["worker"], expected);
        assert_eq!(reply.data["head"], !expected);
    }
    for mut scope in [
        delegating_stamp(),
        stamp(Some("agent-fixture"), true, false),
    ] {
        scope.kind = ScopeKind::Worker;
        if scope.owner_authorized {
            scope.owner = subc_protocol::Principal::Reserved {
                module_id: "other-owner".into(),
            };
        }
        let (reply, _) = run_with_scopes(
            &transport,
            SESSION_CONTEXT_OPERATION,
            json!({"ticket":ticket.value().unwrap()}),
            &scopes_for("ses-context", Some(scope)),
        );
        assert!(!reply.ok);
        assert_eq!(reply.data["refusal_code"], "session_unscoped");
    }
    let (reply, _) = run(
        &transport,
        SESSION_CONTEXT_OPERATION,
        json!({"ticket":"0123456789abcdef0123456789abcdef","worker":true}),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(
        transport.calls().is_empty(),
        "session classification never contacts Plexus"
    );
}

#[test]
fn session_context_refuses_ambiguous_scopes_instead_of_guessing_head() {
    let ticket =
        crate::gh_shim_ticket::ScopedTicket::issue("ses-ambiguous-context", "context-call", "/p");
    let transport = FakeTransport::scoped();
    let mut scopes = scopes_for("ses-ambiguous-context", Some(delegating_stamp()));
    let mut worker = delegating_stamp();
    worker.kind = subc_protocol::scope::ScopeKind::Worker;
    scopes.push("ses-ambiguous-context", Some(worker));
    let (reply, _) = run_with_scopes(
        &transport,
        SESSION_CONTEXT_OPERATION,
        json!({"ticket":ticket.value().unwrap()}),
        &scopes,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "session_unscoped");
    assert!(transport.calls().is_empty());
}

#[test]
fn session_context_replayed_ticket_cannot_restore_worker_routing() {
    let ticket =
        crate::gh_shim_ticket::ScopedTicket::issue("ses-replay-context", "context-call", "/p");
    let value = ticket.value().unwrap().to_string();
    let transport = FakeTransport::scoped();
    let mut worker = delegating_stamp();
    worker.kind = subc_protocol::scope::ScopeKind::Worker;
    let scopes = scopes_for("ses-replay-context", Some(worker));
    let (live, _) = run_with_scopes(
        &transport,
        SESSION_CONTEXT_OPERATION,
        json!({"ticket":value}),
        &scopes,
    );
    assert_eq!(live.data["worker"], true);
    drop(ticket);
    let (replay, _) = run_with_scopes(
        &transport,
        SESSION_CONTEXT_OPERATION,
        json!({"ticket":value}),
        &scopes,
    );
    assert!(!replay.ok);
    assert_eq!(replay.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty());
}

fn read_params(ticket: &str) -> Value {
    json!({"ticket":ticket,"request":{"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"pulls/7/files","accept":"application/vnd.github+json"}}})
}

fn read_worker_stamp() -> ScopeStamp {
    let mut scope = delegating_stamp();
    scope.kind = subc_protocol::scope::ScopeKind::Worker;
    scope
}

#[test]
fn read_relay_uses_github_read_and_the_ticket_scope_without_identity_fields() {
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("ses-read", "read-call", "/p");
    for worker in [true] {
        let transport = FakeTransport::scoped();
        let vector: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/gh_shim/v20-read-vectors.json"
        ))
        .unwrap();
        transport.push_plexus(Ok(vector["get_success"].clone()));
        let mut scope = stamp(None, false, true);
        if worker {
            scope.kind = subc_protocol::scope::ScopeKind::Worker;
        }
        let mut params = read_params(ticket.value().unwrap());
        params["agent_id"] = json!("forged-agent");
        params["session_id"] = json!("forged-head");
        let (reply, _) = run_with_scopes(
            &transport,
            READ_REQUEST_OPERATION,
            params.clone(),
            &scopes_for("ses-read", Some(scope.clone())),
        );
        assert!(reply.ok);
        assert_eq!(reply.data, vector["get_success"]);
        assert_eq!(transport.calls()[0].1, "ses-read");
        assert_eq!(
            transport.calls()[0].2,
            json!({"name":"github_read","arguments":params["request"]})
        );
        assert_eq!(
            transport.plexus_scopes(),
            vec![Some(scope_selector(&scope))]
        );
    }
}

#[test]
fn read_relay_refuses_unknown_actions_unsafe_paths_and_caller_identity() {
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("ses-read-invalid", "read-call", "/p");
    let transport = FakeTransport::scoped();
    let scopes = scopes_for("ses-read-invalid", Some(read_worker_stamp()));
    let base = read_params(ticket.value().unwrap());
    let mut cases = Vec::new();
    for path in [
        "/",
        "https://evil.example",
        "../issues",
        "pulls//7",
        "a%2fb",
    ] {
        let mut params = base.clone();
        params["request"]["args"]["path"] = json!(path);
        cases.push(params);
    }
    let mut params = base.clone();
    params["request"]["action"] = json!("github.repo.delete");
    cases.push(params);
    let mut params = base.clone();
    params["request"]["metadata"] = json!({"agent_id":"forged"});
    cases.push(params);
    let mut params = base.clone();
    params["request"]["args"]["method"] = json!("POST");
    cases.push(params);
    let mut params = base;
    params["request"] = json!({"action":"github.search.code","repository":"cortexkit/aft","args":{"q":"repo:other/repo","per_page":100}});
    cases.push(params);
    for params in cases {
        let (reply, _) = run_with_scopes(&transport, READ_REQUEST_OPERATION, params, &scopes);
        assert!(!reply.ok);
        assert_eq!(reply.data["refusal_code"], "request_malformed");
    }
    assert!(transport.calls().is_empty());
}

#[test]
fn read_relay_requires_live_ticket_and_scoped_transport_without_fallback() {
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("ses-read-scoped", "read-call", "/p");
    let transport = FakeTransport::scoped();
    let params = read_params(ticket.value().unwrap());
    let (reply, _) = run(&transport, READ_REQUEST_OPERATION, params.clone());
    assert_eq!(reply.data["refusal_code"], "agent_identity_required");
    assert!(transport.calls().is_empty());
    let unscoped = FakeTransport::default();
    let (reply, _) = run_with_scopes(
        &unscoped,
        READ_REQUEST_OPERATION,
        params,
        &scopes_for("ses-read-scoped", Some(read_worker_stamp())),
    );
    assert_eq!(reply.data["refusal_code"], "agent_identity_required");
    assert!(unscoped.calls().is_empty());
    let expired = ticket.value().unwrap().to_string();
    drop(ticket);
    for value in [expired, "0123456789abcdef0123456789abcdef".into()] {
        let (reply, _) = run(&transport, READ_REQUEST_OPERATION, read_params(&value));
        assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    }
    assert!(transport.calls().is_empty());
}

#[test]
fn read_relay_preserves_tool_refusals_and_treats_error_frames_as_transport_errors() {
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("ses-read-errors", "read-call", "/p");
    let scopes = scopes_for("ses-read-errors", Some(read_worker_stamp()));
    let vector: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-read-vectors.json"
    ))
    .unwrap();
    for code in [
        "read_budget_exhausted",
        "repository_not_visible",
        "endpoint_not_allowed",
        "response_too_large",
        "agent_identity_required",
        "vendor_error",
    ] {
        let transport = FakeTransport::scoped();
        transport.push_plexus(Ok(vector[code].clone()));
        let (reply, _) = run_with_scopes(
            &transport,
            READ_REQUEST_OPERATION,
            read_params(ticket.value().unwrap()),
            &scopes,
        );
        assert!(reply.ok, "a tool refusal is still a subc Response");
        assert_eq!(reply.data, vector[code]);
        assert_eq!(transport.calls().len(), 1);
    }
    let transport = FakeTransport::scoped();
    transport.push_plexus(Err(TransportError::Refused {
        code: "decode_error".into(),
        message: "Error frame".into(),
    }));
    let (reply, _) = run_with_scopes(
        &transport,
        READ_REQUEST_OPERATION,
        read_params(ticket.value().unwrap()),
        &scopes,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "decode_error");
    assert_eq!(reply.data["stage"], "plexus");
    assert_eq!(transport.calls().len(), 1);
}

#[test]
fn audit_daemon_read_rejects_head_and_unparented_run_scopes() {
    let ticket =
        crate::gh_shim_ticket::ScopedTicket::issue("ses-read-head-audit", "read-call", "/p");
    let vector: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-read-vectors.json"
    ))
    .unwrap();
    for kind in [
        subc_protocol::scope::ScopeKind::Head,
        subc_protocol::scope::ScopeKind::Ephemeral,
    ] {
        let transport = FakeTransport::scoped();
        transport.push_plexus(Ok(vector["get_success"].clone()));
        let mut scope = delegating_stamp();
        scope.kind = kind;
        let (reply, _) = run_with_scopes(
            &transport,
            READ_REQUEST_OPERATION,
            read_params(ticket.value().unwrap()),
            &scopes_for("ses-read-head-audit", Some(scope)),
        );
        assert!(!reply.ok);
        assert_eq!(reply.data["refusal_code"], "agent_identity_required");
        assert!(transport.calls().is_empty());
        assert!(transport.read_roots.lock().unwrap().is_empty());
    }
}

#[test]
fn audit_daemon_read_binds_repository_to_the_redeemed_checkout() {
    let manifest: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    assert!(manifest["bindings"].get("cortexkit/aft").is_some());
    assert!(manifest["bindings"].get("cortexkit/common-auth").is_some());
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue(
        "ses-read-repo-audit",
        "read-call",
        "/trusted/session-checkout",
    );
    let transport = FakeTransport::scoped();
    let mut params = read_params(ticket.value().unwrap());
    params["project_root"] = json!("/forged/checkout");
    params["request"]["repository"] = json!("cortexkit/common-auth");
    let (reply, _) = run_with_scopes(
        &transport,
        READ_REQUEST_OPERATION,
        params,
        &scopes_for("ses-read-repo-audit", Some(read_worker_stamp())),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "repository_not_visible");
    assert_eq!(
        *transport.read_roots.lock().unwrap(),
        vec!["/trusted/session-checkout"]
    );
    assert!(transport.calls().is_empty());
}

#[test]
fn audit_daemon_read_refuses_an_unverifiable_checkout_binding() {
    let ticket =
        crate::gh_shim_ticket::ScopedTicket::issue("ses-read-no-binding-audit", "read-call", "/p");
    let transport = FakeTransport::scoped();
    *transport.read_binding.lock().unwrap() = Some(Err("no verified manifest".into()));
    let (reply, _) = run_with_scopes(
        &transport,
        READ_REQUEST_OPERATION,
        read_params(ticket.value().unwrap()),
        &scopes_for("ses-read-no-binding-audit", Some(read_worker_stamp())),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "repository_not_visible");
    assert!(transport.calls().is_empty());
}

#[test]
fn audit_session_context_never_calls_untrusted_scopes_a_head() {
    let ticket = crate::gh_shim_ticket::ScopedTicket::issue("untrusted-head-scope", "query", "/p");
    let transport = FakeTransport::scoped();
    for mut scope in [stamp(None, false, false), stamp(None, false, true)] {
        if scope.owner_authorized {
            scope.owner = subc_protocol::Principal::Reserved {
                module_id: "other-owner".into(),
            };
        }
        let (reply, _) = run_with_scopes(
            &transport,
            SESSION_CONTEXT_OPERATION,
            json!({"ticket":ticket.value().unwrap()}),
            &scopes_for("untrusted-head-scope", Some(scope)),
        );
        assert!(!reply.ok);
        assert_eq!(reply.data["refusal_code"], "session_unscoped");
    }
    assert!(transport.calls().is_empty());
}

#[tokio::test]
async fn gh_shim_relay_cancellation_after_acquisition_releases_lease() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let transport = daemon.transport().await;
    let consumer = transport.consumer().await.unwrap();
    let leases = Arc::new(RouteLeases::default());
    let pause = Arc::new(LeaseTestPause::default());
    *leases.pause.lock().unwrap() = Some(pause.clone());
    let task_leases = leases.clone();
    let caller = tokio::spawn(async move {
        route_request(
            &consumer,
            &task_leases,
            subc_protocol::RouteTarget::ToolProvider {
                module_id: "plexus".into(),
            },
            subc_protocol::BindIdentity::new("/project", "lease-test", "cancel-acquire"),
            None,
            Default::default(),
            &json!({"name":"github_read"}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), pause.acquired.notified())
        .await
        .unwrap();
    assert_eq!(leases.holders.lock().await.len(), 1);
    caller.abort();
    let _ = caller.await;
    pause.resume.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if leases.holders.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation after acquire must remove its lease entry");
    assert!(daemon.opens().is_empty());
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn gh_shim_relay_cancellation_after_forwarding_closes_route() {
    let daemon = fake_daemon(DaemonScript {
        hold_first_request: true,
        ..Default::default()
    })
    .await;
    let transport = Arc::new(daemon.transport().await);
    let caller_transport = transport.clone();
    let caller = tokio::spawn(async move {
        caller_transport
            .plexus(
                "/project",
                "cancel-forward",
                Some(&scope_selector(&delegating_stamp())),
                json!({"name":"github_read"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while daemon.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        daemon.requests().len(),
        1,
        "the upstream must actually be reached"
    );
    assert_eq!(transport.leases.holders.lock().await.len(), 1);
    caller.abort();
    let _ = caller.await;
    assert_eq!(
        daemon.closes(1).await.len(),
        1,
        "canceled forwarding must close the scoped handle"
    );
    assert!(
        transport.leases.holders.lock().await.is_empty(),
        "no canceled lease/map entry may remain"
    );
}

#[tokio::test]
async fn worker_and_parented_ephemeral_writes_have_the_same_specific_refusal() {
    use subc_protocol::scope::{ParentState, ScopeKind, ScopeParent};
    let head = delegating_stamp();
    for kind in [ScopeKind::Worker, ScopeKind::Ephemeral] {
        let mut worker = head.clone();
        worker.kind = kind;
        if kind == ScopeKind::Ephemeral {
            worker.scope_ref = "worker-run".into();
            worker.parent = Some(ScopeParent::new(
                head.owner.clone(),
                head.scope_ref.clone(),
                head.scope_epoch,
            ));
            worker.parent_state = Some(ParentState::Linked);
        }
        let ticket =
            crate::bash_background::registry::with_scope_spawn(Some(worker.clone()), || {
                crate::gh_shim_ticket::PendingTicket::issue("worker-run-post", "/project")
            });
        let mut params = bot_params(ticket.value().unwrap(), "worker-run-write");
        params["request"]["metadata"]["agent_id"] = json!("agent-fixture");
        let transport = FakeTransport::scoped();
        let routes = scopes_for("worker-run-post", Some(worker));
        let answer = relay(
            &transport,
            RelayCall {
                operation: BOT_REQUEST_OPERATION,
                params: &params,
                first_party: true,
                route_scopes: &routes,
            },
            &|_| {},
        )
        .await;
        assert_eq!(
            answer.data["refusal_code"],
            crate::gh_shim_speech::WORKER_WRITE_CODE,
            "{kind:?}"
        );
        assert_eq!(
            answer.data["message"],
            crate::gh_shim_speech::WORKER_WRITE_REFUSED
        );
        assert!(transport.calls().is_empty());
    }
}

#[tokio::test]
async fn validated_head_codemode_is_not_a_worker_write_but_invalid_head_is() {
    use subc_protocol::scope::{ParentState, ScopeKind, ScopeParent};
    let head = delegating_stamp();
    let run = ScopeStamp::new(head.owner.clone(), "codemode-run", 3, ScopeKind::Ephemeral)
        .with_parent(Some(ScopeParent::new(
            head.owner.clone(),
            head.scope_ref.clone(),
            head.scope_epoch,
        )))
        .with_parent_state(Some(ParentState::Linked))
        .with_attributes(
            subc_protocol::scope::ScopeAttributes::new()
                .with_agent_id(Some("agent-fixture".into()))
                .with_run_id(Some("program-a".into())),
        )
        .with_owner_authorized(true)
        .with_expires_at_ms(Some(crate::db::call_ledger::now_ms() as u64 + 60_000));
    let launch = crate::gh_shim_speech::model::LaunchAuthority::codemode(
        head.clone(),
        run.clone(),
        None,
        None,
    )
    .unwrap();
    for valid in [true, false] {
        let mut launch = launch.clone();
        if !valid {
            launch.authority.owner_authorized = false;
        }
        let redeemed = crate::gh_shim_ticket::Redeemed {
            session_id: "head-codemode".into(),
            task_id: String::new(),
            project_root: "/project".into(),
            read_repository: None,
            lifecycle_scope: Some(run.clone()),
            launch: Some(launch),
        };
        let mut params = bot_params("not-used-by-this-admission-unit", "codemode-write");
        params["request"]["metadata"]["agent_id"] = json!("agent-fixture");
        let transport = FakeTransport::scoped();
        let (answer, _) = bot_request(
            &transport,
            &redeemed,
            Some(&head),
            &params,
            &RouteScopes::default(),
            true,
        )
        .await;
        if valid {
            assert!(answer.ok);
            assert_eq!(transport.calls().len(), 1);
        } else {
            assert_eq!(
                answer.data["refusal_code"],
                crate::gh_shim_speech::WORKER_WRITE_CODE
            );
            assert!(transport.calls().is_empty());
        }
    }
}

#[tokio::test]
async fn published_sdk_reservation_does_not_send_until_committed_and_releases_on_drop() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let body = github_call(
        json!({"op":"bot_request","request_nonce":"reserved-before-send","request":{"action":"issue comment"}}),
    );
    let reservation = relay
        .reserve_bot(
            "/project",
            "reserved-session",
            &delegating_stamp(),
            body,
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .expect("published SUBC 0.32.7 must reserve rather than return guard_unsupported");
    assert!(
        daemon.requests().is_empty(),
        "reserving a writer slot must not enqueue the mutation"
    );
    assert_eq!(daemon.opens()[0]["scope"]["scope_epoch"], 7);
    drop(reservation);
    assert_eq!(daemon.closes(1).await.len(), 1);
    assert!(daemon.requests().is_empty());
    let sent = relay
        .reserve_bot(
            "/project",
            "reserved-session",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"reserved-committed"})),
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .unwrap();
    assert!(daemon.requests().is_empty());
    let queued = sent.send().unwrap();
    let reply = queued.response().await.unwrap();
    assert_eq!(facade_reply(&reply)["result"]["status"], "completed");
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(
        daemon.requests()[0]["arguments"]["request_nonce"],
        "reserved-committed"
    );
    assert_eq!(daemon.closes(2).await.len(), 2);
}

#[tokio::test]
async fn published_sdk_reservation_names_scoped_route_end_before_any_send() {
    let daemon = fake_daemon(DaemonScript {
        refuse_scoped_opens: Some(subc_protocol::error_codes::SCOPE_ENDED.into()),
        ..Default::default()
    })
    .await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let error = relay
        .reserve_bot(
            "/project",
            "reserved-ended",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"reserved-ended"})),
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .err()
        .expect("an ended epoch-pinned route cannot reserve a mutation");
    assert_eq!(
        (error.code, error.reason.as_str()),
        (crate::gh_shim_speech::Code::ScopeEnded, "ended")
    );
    assert!(daemon.requests().is_empty());
    assert!(daemon.opens().iter().all(|open| !open["scope"].is_null()));
}

#[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub(crate) async fn fake_daemon_with_reply(
    reply: Arc<dyn Fn(&Value, Option<&Value>) -> Value + Send + Sync>,
) -> FakeDaemon {
    fake_daemon(DaemonScript {
        reply: Some(reply),
        ..Default::default()
    })
    .await
}

#[tokio::test]
async fn published_sdk_reservation_expired_budget_names_timeout_without_enqueuing() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let error = relay
        .reserve_bot(
            "/project",
            "reserved-timeout",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"expired"})),
            tokio::time::Instant::now() - Duration::from_millis(1),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.reason, "reservation_timeout");
    assert_eq!(error.text(),"bot writes from background commands are briefly unavailable (reservation_timeout); retry in a few seconds.");
    assert!(daemon.requests().is_empty());
    assert!(daemon.opens().is_empty());
}

#[tokio::test]
async fn published_sdk_reservation_reply_drop_releases_its_scoped_lease_after_enqueue() {
    let daemon = fake_daemon(DaemonScript {
        hold_first_request: true,
        ..Default::default()
    })
    .await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let slot = relay
        .reserve_bot(
            "/project",
            "reserved-cancel",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"cancel-after-send"})),
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .unwrap();
    let queued = slot.send().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while daemon.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    drop(queued);
    assert_eq!(daemon.closes(1).await.len(), 1);
    assert_eq!(
        daemon.requests().len(),
        1,
        "dropping a reply cannot turn an already-dequeued mutation into a zero-send claim"
    );
}

#[tokio::test]
async fn published_sdk_reservation_old_connection_cleanup_cannot_close_a_replacement_slot() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let old = relay
        .reserve_bot(
            "/project",
            "same-session",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"old-unsent"})),
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .unwrap();
    // Keep the old socket alive so its GOODBYE proves lease cleanup rather
    // than disappearing with the last consumer when the reservation drops.
    let _old_consumer = relay.transport.consumer.lock().await.clone().unwrap();
    let replacement = daemon.transport().await;
    let consumer = replacement.consumer.lock().await.clone().unwrap();
    *relay.transport.consumer.lock().await = Some(consumer);
    let current = relay
        .reserve_bot(
            "/project",
            "same-session",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request","request_nonce":"new-send"})),
            tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
        )
        .await
        .unwrap();
    drop(old);
    assert_eq!(
        daemon.closes(1).await.len(),
        1,
        "the old generation is released without waiting for the new one"
    );
    let reply = current.send().unwrap().response().await.unwrap();
    assert_eq!(facade_reply(&reply)["result"]["status"], "completed");
    assert_eq!(daemon.requests().len(), 1);
    assert_eq!(
        daemon.requests()[0]["arguments"]["request_nonce"],
        "new-send"
    );
    assert_eq!(daemon.closes(2).await.len(), 2);
}

#[tokio::test]
async fn published_sdk_reservation_deadline_bounds_waiting_for_the_consumer_slot() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let slot = relay.transport.consumer.lock().await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        relay.reserve_bot(
            "/project",
            "consumer-slot-deadline",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request"})),
            tokio::time::Instant::now() + Duration::from_millis(50),
        ),
    )
    .await;
    drop(slot);
    let refusal = result
        .expect("the reservation deadline must bound consumer-slot contention")
        .err()
        .expect("no reservation is possible while the consumer slot is locked");
    assert_eq!(
        refusal.code,
        crate::gh_shim_speech::Code::AuthorityUnavailable
    );
    assert_eq!(refusal.reason, "reservation_timeout");
    assert!(daemon.opens().is_empty());
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn published_sdk_reservation_deadline_bounds_waiting_for_a_route_lease() {
    let daemon = fake_daemon(DaemonScript::default()).await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let holders = relay.transport.leases.holders.lock().await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        relay.reserve_bot(
            "/project",
            "lease-deadline",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request"})),
            tokio::time::Instant::now() + Duration::from_millis(50),
        ),
    )
    .await;
    drop(holders);
    let refusal = result
        .expect("the reservation deadline must bound route-lease contention")
        .err()
        .expect("no reservation is possible while route leases are locked");
    assert_eq!(refusal.reason, "reservation_timeout");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if Arc::strong_count(&relay.transport.leases) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the detached owner must release its expired lease");
    assert!(relay.transport.leases.holders.lock().await.is_empty());
    assert!(daemon.opens().is_empty());
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn published_sdk_reservation_deadline_during_open_retains_cleanup_ownership() {
    let pause = Arc::new(LeaseTestPause::default());
    let daemon = fake_daemon(DaemonScript {
        scoped_open_pause: Some(pause.clone()),
        ..Default::default()
    })
    .await;
    let relay = GhRelay {
        transport: daemon.transport().await,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        relay.reserve_bot(
            "/project",
            "open-deadline",
            &delegating_stamp(),
            github_call(json!({"op":"bot_request"})),
            tokio::time::Instant::now() + Duration::from_millis(100),
        ),
    )
    .await;
    pause.resume.notify_one();
    let refusal = result
        .expect("the reservation deadline must bound a successful open still in flight")
        .err()
        .expect("the paused open must not produce a reservation before the deadline");
    assert_eq!(refusal.reason, "reservation_timeout");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let cleaned = {
                let log = daemon.log.lock().unwrap();
                !log.closes.is_empty() || log.connections_closed > 0
            };
            if cleaned {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the timed-out open must release its route or close its original connection");
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn published_sdk_reservation_canceled_open_registers_and_releases_without_sending() {
    let pause = Arc::new(LeaseTestPause::default());
    let daemon = fake_daemon(DaemonScript {
        scoped_open_pause: Some(pause.clone()),
        ..Default::default()
    })
    .await;
    let relay = Arc::new(GhRelay {
        transport: daemon.transport().await,
    });
    let owner = relay.clone();
    let request = tokio::spawn(async move {
        owner
            .reserve_bot(
                "/project",
                "open-canceled",
                &delegating_stamp(),
                github_call(json!({"op":"bot_request"})),
                tokio::time::Instant::now() + RELAY_CALL_TIMEOUT,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), pause.acquired.notified())
        .await
        .unwrap();
    request.abort();
    assert!(request.await.err().unwrap().is_cancelled());
    pause.resume.notify_one();
    assert_eq!(daemon.closes(1).await.len(), 1);
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn never_issued_bot_ticket_is_unknown_before_serving_or_authority() {
    let root = tempfile::tempdir().unwrap();
    drop(crate::db::open(&root.path().join("aft.db")).unwrap());
    let service = crate::gh_shim_speech::service::record_service(root.path());
    assert!(!service.try_serve(), "the fixture has no serving runtime");
    let mut routes = RouteScopes::default();
    routes.speech = Some(service);
    let params = bot_params("0123456789abcdef0123456789abcdef", "never-issued");
    let transport = FakeTransport::scoped();
    let answer = relay(
        &transport,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            route_scopes: &routes,
        },
        &|_| {},
    )
    .await;
    assert_eq!(answer.data["refusal_code"], "ticket_unknown");
    assert_eq!(answer.data["stage"], "ticket");
    assert!(transport.calls().is_empty());
}
