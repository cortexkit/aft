use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

/// Which fake module a recorded call went to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Prefrontal,
    Plexus,
}

/// Fake prefrontal and plexus. Every call's body is recorded; replies are
/// scripted per target and default to a fresh token and a completed reply.
/// `scoped` says whether this fake can open its plexus route under a scope.
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<(Target, String, Value)>>,
    prefrontal_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    plexus_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    minted: Mutex<u64>,
    scoped: bool,
    plexus_scopes: Mutex<Vec<Option<ScopeSelector>>>,
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

    fn push_prefrontal(&self, reply: Result<Value, TransportError>) {
        self.prefrontal_replies.lock().unwrap().push_back(reply);
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

fn token_with_exp(serial: u64, exp: u64) -> Value {
    json!({
        "claims": {"agent_id": "agent-fixture", "exp": exp, "jti": format!("jti-{serial}"), "handle": "aft-alfonso[bot]"},
        "signature_hex": format!("sig-secret-{serial}"),
        "generation": 1,
    })
}

const TOKEN_EXP: u64 = 1_700_014_400;
const NOW: u64 = 1_700_000_000;

impl RelayTransport for FakeTransport {
    async fn prefrontal(
        &self,
        _project_root: &str,
        session: &str,
        body: Value,
    ) -> Result<Value, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((Target::Prefrontal, session.to_string(), body));
        let scripted = self.prefrontal_replies.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| {
            let mut minted = self.minted.lock().unwrap();
            *minted += 1;
            Ok(json!({"result": {"token": token_with_exp(*minted, TOKEN_EXP)}}))
        })
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

fn envelope() -> Value {
    golden()["request"]["request"].clone()
}

fn run(
    transport: &FakeTransport,
    cache: &TokenCache,
    operation: &str,
    params: Value,
    now: u64,
) -> (RelayReply, Vec<String>) {
    run_with_scopes(
        transport,
        cache,
        operation,
        params,
        now,
        &RouteScopes::default(),
    )
}

fn run_with_scopes<T: RelayTransport>(
    transport: &T,
    cache: &TokenCache,
    operation: &str,
    params: Value,
    now: u64,
    route_scopes: &RouteScopes,
) -> (RelayReply, Vec<String>) {
    let lines = Mutex::new(Vec::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(relay(
        transport,
        cache,
        RelayCall {
            operation,
            params: &params,
            first_party: true,
            now,
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
    let cache = TokenCache::default();
    let mut params = bot_params("0123456789abcdef0123456789abcdef", "nonce-fab");
    params["session"] = json!("ses-head");
    params["session_id"] = json!("ses-head");
    params["request"]["metadata"]["session"] = json!("ses-head");
    let (reply, _) = run(&transport, &cache, BOT_REQUEST_OPERATION, params, NOW);
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty(), "no mint and no plexus call");

    let mut absent = bot_params("", "nonce-absent");
    absent["session"] = json!("ses-head");
    let (reply, _) = run(&transport, &cache, BOT_REQUEST_OPERATION, absent, NOW);
    assert_eq!(reply.data["refusal_code"], "ticket_absent");
    assert!(transport.calls().is_empty());
}

/// The ticket alone chooses the session: a typed session in the body is
/// ignored even when the ticket is real.
#[test]
fn a_real_ticket_speaks_for_its_own_session_whatever_the_body_says() {
    let worker = crate::gh_shim_ticket::ScopedTicket::issue("ses-worker", "call-w", "/p");
    let transport = FakeTransport::default();
    let cache = TokenCache::default();
    let mut params = bot_params(worker.value().unwrap(), "nonce-typed");
    params["session"] = json!("ses-head");
    let (reply, _) = run(&transport, &cache, BOT_REQUEST_OPERATION, params, NOW);
    assert!(reply.ok);
    let calls = transport.calls();
    assert_eq!(calls[0].0, Target::Prefrontal);
    assert_eq!(calls[0].2["params"]["session"], "ses-worker");
    assert_eq!(calls[0].1, "ses-worker");
}

#[test]
fn untrusted_binds_are_refused_even_with_a_live_ticket() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-untrusted", "call-u", "/p");
    let transport = FakeTransport::default();
    let cache = TokenCache::default();
    let params = bot_params(live.value().unwrap(), "nonce-untrusted");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reply = runtime.block_on(relay(
        &transport,
        &cache,
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: false,
            now: NOW,
            route_scopes: &RouteScopes::default(),
        },
        &|_| {},
    ));
    assert_eq!(reply.data["refusal_code"], "untrusted_principal");
    assert!(transport.calls().is_empty());
}

/// A ticket issued to a background bash command spawned for an agent session
/// relays as that session. The exact mint and plexus bodies are checked, and
/// plexus's expected request and reply come from its own fixture file.
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
    config.sandbox.enabled = false;
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
    let transport = FakeTransport::default();
    transport.push_prefrontal(Ok(
        json!({"result": {"token": golden["request"]["assertion"].clone()}}),
    ));
    transport.push_plexus(Ok(golden["success"].clone()));
    let cache = TokenCache::default();
    let nonce = golden["request"]["request_nonce"].as_str().unwrap();
    let (reply, lines) = run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, nonce),
        1_700_000_000,
    );

    assert_eq!(reply, RelayReply::relayed(golden["success"].clone()));
    let calls = transport.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, Target::Prefrontal);
    assert_eq!(calls[0].1, "ses-agent");
    assert_eq!(
        calls[0].2,
        json!({
            "method": "agent.assertion_mint",
            "params": {"agent_id": "agent-fixture", "session": "ses-agent"},
        })
    );
    assert_eq!(calls[1].0, Target::Plexus);
    assert_eq!(calls[1].1, "ses-agent");
    let mut expected_arguments = golden["request"].clone();
    expected_arguments["op"] = json!("bot_request");
    assert_eq!(
        calls[1].2,
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
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, "nonce-ended"),
        NOW,
    );
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty());
}

#[test]
fn token_is_reused_then_reminted_near_expiry_and_after_an_assertion_refusal() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-cache", "call-c", "/p");
    let ticket = live.value().unwrap();
    let transport = FakeTransport::default();
    let cache = TokenCache::default();

    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n1"),
        NOW,
    );
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n2"),
        NOW,
    );
    assert_eq!(
        transport.count(Target::Prefrontal),
        1,
        "second command reuses"
    );
    let plexus_tokens: Vec<Value> = transport
        .calls()
        .iter()
        .filter(|(target, _, _)| *target == Target::Plexus)
        .map(|(_, _, body)| body["arguments"]["assertion"].clone())
        .collect();
    assert_eq!(plexus_tokens[0], plexus_tokens[1]);

    // Within the refresh margin of `exp` the token is minted again.
    let near_expiry = TOKEN_EXP - TOKEN_REFRESH_MARGIN_SECS;
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n3"),
        near_expiry,
    );
    assert_eq!(
        transport.count(Target::Prefrontal),
        2,
        "re-minted near expiry"
    );

    // Plexus refusing the assertion drops the cached token.
    transport.push_plexus(Ok(json!({
        "repo_binding_generation": 1,
        "result": {"status": "refused", "refusal_code": "assertion_binding_generation_stale"},
    })));
    let (refused, _) = run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n4"),
        NOW,
    );
    assert!(refused.ok, "plexus's refusal is relayed unchanged");
    assert_eq!(
        facade_refusal_code(&refused.data),
        Some("assertion_binding_generation_stale")
    );
    assert_eq!(
        transport.count(Target::Prefrontal),
        2,
        "the refused call reused the cached token"
    );
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n4"),
        NOW,
    );
    assert_eq!(
        transport.count(Target::Prefrontal),
        3,
        "re-minted after refusal"
    );

    // A refusal that does not name the assertion keeps the token.
    transport.push_plexus(Ok(json!({
        "repo_binding_generation": 1,
        "result": {"status": "refused", "refusal_code": "repository_unbound"},
    })));
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n5"),
        NOW,
    );
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(ticket, "n6"),
        NOW,
    );
    assert_eq!(transport.count(Target::Prefrontal), 3);
}

#[test]
fn mint_refusals_reach_the_shim_with_prefrontals_code_and_skip_plexus() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-worker-mint", "call-m", "/p");
    let transport = FakeTransport::default();
    transport.push_prefrontal(Err(TransportError::Refused {
        code: "assertion_session_unknown".to_string(),
        message: "session is not an agent session".to_string(),
    }));
    let (reply, lines) = run(
        &transport,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-mint"),
        NOW,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "assertion_session_unknown");
    assert_eq!(reply.data["stage"], "mint");
    assert_eq!(transport.count(Target::Plexus), 0);
    assert!(
        lines[0].ends_with("outcome=assertion_session_unknown"),
        "{}",
        lines[0]
    );
}

#[test]
fn a_sent_request_without_a_reply_is_outcome_unknown() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-unknown", "call-o", "/p");
    let transport = FakeTransport::default();
    transport.push_plexus(Err(TransportError::OutcomeUnknown("timed out".to_string())));
    let (reply, _) = run(
        &transport,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-unknown"),
        NOW,
    );
    assert_eq!(reply.data["refusal_code"], "outcome_unknown");
}

#[test]
fn bindings_read_is_relayed_under_the_same_ticket_gate() {
    let transport = FakeTransport::default();
    let cache = TokenCache::default();
    let (reply, _) = run(
        &transport,
        &cache,
        BINDINGS_READ_OPERATION,
        json!({"ticket": "ffffffffffffffffffffffffffffffff", "agent_id": "agent-fixture"}),
        NOW,
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
        &cache,
        BINDINGS_READ_OPERATION,
        json!({
            "ticket": live.value().unwrap(),
            "agent_id": "agent-fixture",
            // A connection id from the caller is ignored; the daemon derives
            // it from the assertion it minted.
            "connection_id": "github-handle-someone-else",
        }),
        NOW,
    );
    assert_eq!(reply, RelayReply::relayed(bindings));
    let calls = transport.calls();
    assert_eq!(calls[0].0, Target::Prefrontal);
    assert_eq!(
        calls[0].2["params"],
        json!({"agent_id": "agent-fixture", "session": "ses-bindings"})
    );
    assert_eq!(
        calls[1].2,
        json!({
            "name": "github",
            "arguments": {"op": "bindings.read", "connection_id": "github-handle-aft-alfonso"},
        })
    );

    // The minted token is cached, so the following bot write reuses it.
    run(
        &transport,
        &cache,
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "n"),
        NOW,
    );
    assert_eq!(transport.count(Target::Prefrontal), 1);
}

#[test]
fn handle_connection_id_comes_from_the_assertion_bot_login() {
    assert_eq!(
        handle_connection_id(&json!({"claims": {"handle": "aft-alfonso[bot]"}})),
        Ok("github-handle-aft-alfonso".to_string())
    );
    for token in [
        json!({"claims": {"handle": "aft-alfonso"}}),
        json!({"claims": {"handle": "[bot]"}}),
        json!({"claims": {}}),
    ] {
        assert!(handle_connection_id(&token).is_err(), "{token}");
    }
}

#[test]
fn bindings_read_fails_closed_on_a_malformed_handle_or_a_mint_refusal() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-bindings-bad", "call-bb", "/p");
    let params = json!({"ticket": live.value().unwrap(), "agent_id": "agent-fixture"});

    let transport = FakeTransport::default();
    transport.push_prefrontal(Ok(json!({"result": {"token": {
        "claims": {"agent_id": "agent-fixture", "exp": TOKEN_EXP, "handle": "aft-alfonso"},
        "signature_hex": "sig-secret-x",
        "generation": 1,
    }}})));
    let (reply, _) = run(
        &transport,
        &TokenCache::default(),
        BINDINGS_READ_OPERATION,
        params.clone(),
        NOW,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "assertion_handle_malformed");
    assert_eq!(transport.count(Target::Plexus), 0);

    let refused = FakeTransport::default();
    refused.push_prefrontal(Err(TransportError::Refused {
        code: "assertion_session_unknown".to_string(),
        message: "not an agent session".to_string(),
    }));
    let (reply, _) = run(
        &refused,
        &TokenCache::default(),
        BINDINGS_READ_OPERATION,
        params,
        NOW,
    );
    assert_eq!(reply.data["refusal_code"], "assertion_session_unknown");
    assert_eq!(reply.data["stage"], "mint");
    assert_eq!(refused.count(Target::Plexus), 0);
}

#[test]
fn relay_log_line_carries_session_task_and_nonce_but_no_ticket_or_token() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-log", "call-log-7", "/p");
    let ticket = live.value().unwrap().to_string();
    let transport = FakeTransport::default();
    let (reply, lines) = run(
        &transport,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(&ticket, "nonce-log-42"),
        NOW,
    );
    assert!(reply.ok);
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(
        line,
        "gh_shim relay: op=gh_shim.bot_request session=ses-log task=call-log-7 nonce=nonce-log-42 action=issue reaction repository=org/repo path=assertion outcome=completed"
    );
    assert!(!line.contains(&ticket));
    assert!(!line.contains("sig-secret"));
    assert!(!line.contains("jti-"));
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
    ScopeStamp {
        owner: subc_protocol::Principal::Reserved {
            module_id: PREFRONTAL_MODULE_ID.to_string(),
        },
        scope_ref: "head-ref-1".to_string(),
        scope_epoch: 7,
        kind: subc_protocol::scope::ScopeKind::Head,
        parent: None,
        parent_state: None,
        attributes: subc_protocol::scope::ScopeAttributes {
            agent_id: agent.map(str::to_string),
            delegates,
            flow_id: None,
        },
        owner_authorized,
    }
}

fn delegating_stamp() -> ScopeStamp {
    stamp(Some("agent-fixture"), true, true)
}

fn scopes_for(session: &str, stamp: Option<ScopeStamp>) -> RouteScopes {
    let mut scopes = RouteScopes::default();
    scopes.push(session, stamp);
    scopes
}

/// A delegating scope on the session's route sends the write under that scope
/// with no assertion, and prefrontal is never asked to mint one.
#[test]
fn a_delegating_stamped_route_sends_no_assertion_and_mints_nothing() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-stamp", "call-stamp", "/p");
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-stamp"),
        NOW,
        &scopes_for("ses-stamp", Some(delegating_stamp())),
    );

    assert!(reply.ok, "{:?}", reply.data);
    assert_eq!(transport.count(Target::Prefrontal), 0, "nothing was minted");
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
        vec![Some(ScopeSelector {
            owner: subc_protocol::Principal::Reserved {
                module_id: PREFRONTAL_MODULE_ID.to_string(),
            },
            scope_ref: "head-ref-1".to_string(),
            scope_epoch: Some(7),
        })],
        "the plexus route is opened under the session's scope at its epoch"
    );
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);
    assert!(!lines[0].contains("head-ref-1"), "{}", lines[0]);
}

/// A session whose route carries no stamp keeps minting and sending the
/// assertion on an unscoped route, even when the transport could scope it.
#[test]
fn an_unstamped_route_keeps_minting_and_sending_the_assertion() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-unstamped", "call-un", "/p");
    let transport = FakeTransport::scoped();
    let (reply, lines) = run_with_scopes(
        &transport,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-unstamped"),
        NOW,
        &scopes_for("ses-unstamped", None),
    );
    assert!(reply.ok);
    assert_eq!(transport.count(Target::Prefrontal), 1);
    let calls = transport.calls();
    assert_eq!(calls[1].0, Target::Plexus);
    assert_eq!(
        calls[1].2["arguments"]["assertion"],
        token_with_exp(1, TOKEN_EXP)
    );
    assert_eq!(transport.plexus_scopes(), vec![None]);
    assert!(lines[0].contains(" path=assertion "), "{}", lines[0]);
}

/// A stamp that does not let plexus act as its agent (not delegating, an
/// owner the daemon does not trust with agent identity, or no agent) uses the
/// assertion path exactly as an unstamped route does.
#[test]
fn a_non_delegating_stamp_uses_the_assertion_path() {
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
        let (reply, lines) = run_with_scopes(
            &transport,
            &TokenCache::default(),
            BOT_REQUEST_OPERATION,
            bot_params(live.value().unwrap(), "nonce-nondeleg"),
            NOW,
            &scopes_for("ses-nondeleg", Some(non_delegating)),
        );
        assert!(reply.ok, "{label}");
        assert_eq!(transport.count(Target::Prefrontal), 1, "{label}");
        let calls = transport.calls();
        assert!(calls[1].2["arguments"]["assertion"].is_object(), "{label}");
        assert_eq!(transport.plexus_scopes(), vec![None], "{label}");
        assert!(
            lines[0].contains(" path=assertion "),
            "{label}: {}",
            lines[0]
        );
    }
}

/// Plexus's scope refusals on the stamp path reach the shim exactly as plexus
/// sent them, `legs_sent` included, and the write is never retried with an
/// assertion: that retry would get past the scope decision plexus just made.
#[test]
fn a_stamp_path_refusal_is_relayed_verbatim_with_no_assertion_retry() {
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
        let cache = TokenCache::default();
        let (reply, lines) = run_with_scopes(
            &transport,
            &cache,
            BOT_REQUEST_OPERATION,
            bot_params(live.value().unwrap(), "nonce-stamp-refused"),
            NOW,
            &scopes_for("ses-stamp-refused", Some(delegating_stamp())),
        );
        assert_eq!(reply, RelayReply::relayed(refusal), "{code}");
        assert_eq!(transport.count(Target::Prefrontal), 0, "{code}: no mint");
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
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-stamp-subc"),
        NOW,
        &scopes_for("ses-stamp-refused", Some(delegating_stamp())),
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["refusal_code"], "scope_ended");
    assert_eq!(reply.data["stage"], "plexus");
    assert_eq!(transport.count(Target::Prefrontal), 0);
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
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-other-agent"),
        NOW,
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

/// The production transport opens scoped routes, so a delegating stamp takes
/// the stamp path there: with no subc connection the plexus call fails, and
/// prefrontal is never asked for a mint. A transport that cannot open a scoped
/// route still sends the same session down the assertion path, since plexus
/// would refuse a write with neither a stamp nor an assertion.
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
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-prod"),
        NOW,
        &scopes,
    );
    assert!(!reply.ok);
    assert_eq!(reply.data["stage"], "plexus", "{:?}", reply.data);
    assert!(lines[0].contains(" path=stamp "), "{}", lines[0]);

    let unscoped = FakeTransport::default();
    let (reply, lines) = run_with_scopes(
        &unscoped,
        &TokenCache::default(),
        BOT_REQUEST_OPERATION,
        bot_params(live.value().unwrap(), "nonce-prod-fake"),
        NOW,
        &scopes,
    );
    assert!(reply.ok);
    assert_eq!(unscoped.count(Target::Prefrontal), 1);
    assert!(unscoped.calls()[1].2["arguments"]["assertion"].is_object());
    assert_eq!(unscoped.plexus_scopes(), vec![None]);
    assert!(lines[0].contains(" path=assertion "), "{}", lines[0]);
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
    /// Refuse every route.open that carries a scope with this code.
    refuse_scoped_opens: Option<String>,
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
}

struct FakeDaemon {
    log: Arc<Mutex<DaemonLog>>,
    connection_file: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl FakeDaemon {
    fn opens(&self) -> Vec<Value> {
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

    fn requests(&self) -> Vec<Value> {
        self.log.lock().unwrap().requests.clone()
    }

    async fn transport(&self) -> SubcRelayTransport {
        self.transport_with_deadline(None).await
    }

    async fn transport_with_deadline(
        &self,
        route_open_deadline: Option<std::time::Duration>,
    ) -> SubcRelayTransport {
        let consumer = subc_client_rs::SubcConsumer::connect(
            &self.connection_file,
            subc_client_rs::ConsumerOptions {
                call_timeout: std::time::Duration::from_secs(5),
                ..subc_client_rs::ConsumerOptions::default()
            },
        )
        .await
        .expect("the fake daemon accepts the relay's connection");
        SubcRelayTransport::with_consumer(consumer, route_open_deadline)
    }
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
                                    log.requests.push(body);
                                    log.requests.len()
                                };
                                let reply = respond(json!({
                                    "repo_binding_generation": 1,
                                    "result": {"status": "completed"},
                                }));
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
                                            Some(code) if scoped => replies.push(
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
                                            ),
                                            _ => {
                                                next_channel += 1;
                                                if scoped {
                                                    scoped_channels.push(next_channel);
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
    let reply = relay(
        transport,
        &TokenCache::default(),
        RelayCall {
            operation: BOT_REQUEST_OPERATION,
            params: &params,
            first_party: true,
            now: NOW,
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
#[tokio::test]
async fn a_retried_scope_refusal_that_outlasts_the_deadline_keeps_its_code() {
    for code in ["scope_not_synced", "scope_changed"] {
        let daemon = fake_daemon(DaemonScript {
            refuse_scoped_opens: Some(code.to_string()),
            ..DaemonScript::default()
        })
        .await;
        let transport = daemon
            .transport_with_deadline(Some(std::time::Duration::from_millis(300)))
            .await;
        let (reply, _) = relay_stamped(&transport, "ses-scope-retried", "nonce-retried").await;
        assert!(!reply.ok, "{code}");
        assert_eq!(reply.data["refusal_code"], code, "{:?}", reply.data);
        assert_eq!(reply.data["retryable"], true, "{code}");
        assert_eq!(reply.data["stage"], "plexus", "{code}");
        let opens = daemon.opens();
        assert!(
            !opens.is_empty() && opens.iter().all(is_scoped_open),
            "{code}: {opens:?}"
        );
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
