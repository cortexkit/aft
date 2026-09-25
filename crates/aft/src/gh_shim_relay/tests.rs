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
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<(Target, String, Value)>>,
    prefrontal_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    plexus_replies: Mutex<VecDeque<Result<Value, TransportError>>>,
    minted: Mutex<u64>,
}

impl FakeTransport {
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
        "claims": {"agent_id": "agent-fixture", "exp": exp, "jti": format!("jti-{serial}")},
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
        body: Value,
    ) -> Result<Value, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((Target::Plexus, session.to_string(), body));
        let scripted = self.plexus_replies.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| {
            Ok(json!({"repo_binding_generation": 1, "result": {"status": "completed"}}))
        })
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
    assert!(head.value().is_some(), "the head has a live ticket of its own");
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
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    );
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
        Some(30_000),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
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
    assert!(lines[0].contains(&format!("task={task_id}")), "{}", lines[0]);

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
        Some(30_000),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
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

    let _ = ctx
        .bash_background()
        .kill(&task_id, "ses-agent");
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

    run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n1"), NOW);
    run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n2"), NOW);
    assert_eq!(transport.count(Target::Prefrontal), 1, "second command reuses");
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
    assert_eq!(transport.count(Target::Prefrontal), 2, "re-minted near expiry");

    // Plexus refusing the assertion drops the cached token.
    transport.push_plexus(Ok(json!({
        "repo_binding_generation": 1,
        "result": {"status": "refused", "refusal_code": "assertion_binding_generation_stale"},
    })));
    let (refused, _) = run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n4"), NOW);
    assert!(refused.ok, "plexus's refusal is relayed unchanged");
    assert_eq!(
        facade_refusal_code(&refused.data),
        Some("assertion_binding_generation_stale")
    );
    assert_eq!(transport.count(Target::Prefrontal), 2, "the refused call reused the cached token");
    run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n4"), NOW);
    assert_eq!(transport.count(Target::Prefrontal), 3, "re-minted after refusal");

    // A refusal that does not name the assertion keeps the token.
    transport.push_plexus(Ok(json!({
        "repo_binding_generation": 1,
        "result": {"status": "refused", "refusal_code": "repository_unbound"},
    })));
    run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n5"), NOW);
    run(&transport, &cache, BOT_REQUEST_OPERATION, bot_params(ticket, "n6"), NOW);
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
    assert!(lines[0].ends_with("outcome=assertion_session_unknown"), "{}", lines[0]);
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
        json!({"ticket": "ffffffffffffffffffffffffffffffff", "connection_id": "github-handle-a"}),
        NOW,
    );
    assert_eq!(reply.data["refusal_code"], "ticket_unknown");
    assert!(transport.calls().is_empty());

    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-bindings", "call-b", "/p");
    let bindings = json!({
        "repo_binding_generation": 3,
        "bindings": [{"repository": "org/repo", "app_handle_id": "h", "agent_id": "agent-fixture"}],
    });
    transport.push_plexus(Ok(bindings.clone()));
    let (reply, _) = run(
        &transport,
        &cache,
        BINDINGS_READ_OPERATION,
        json!({"ticket": live.value().unwrap(), "connection_id": "github-handle-agent-fixture"}),
        NOW,
    );
    assert_eq!(reply, RelayReply::relayed(bindings));
    assert_eq!(
        transport.calls()[0].2,
        json!({
            "name": "github",
            "arguments": {"op": "bindings.read", "connection_id": "github-handle-agent-fixture"},
        })
    );
    assert_eq!(transport.count(Target::Prefrontal), 0);
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
        "gh_shim relay: op=gh_shim.bot_request session=ses-log task=call-log-7 nonce=nonce-log-42 action=issue reaction repository=org/repo outcome=completed"
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
