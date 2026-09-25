use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

use super::super::{
    classify, dispatch_r3_with_relay, AgentBinding, Classification, Manifest, RefusalCode,
    RungDetermination, RungRecordProvenance, StatePaths, OUTCOME_UNKNOWN_EXIT_STATUS,
    REFUSAL_EXIT_STATUS,
};
use super::*;

/// Plexus's closed refusal taxonomy, copied from plexus-core's
/// `tests/github_acceptance/main.rs` (see tests/fixtures/plexus/SOURCE.md).
const CLOSED_CODES: &[&str] = &[
    "assertion_absent",
    "assertion_malformed",
    "assertion_signature_invalid",
    "assertion_expired",
    "assertion_not_yet_valid",
    "assertion_wrong_surface",
    "assertion_key_unavailable",
    "assertion_scope_absent",
    "agent_handle_unknown",
    "agent_handle_mismatch",
    "assertion_binding_generation_stale",
    "agent_repository_conflict",
    "repository_unbound",
    "request_kind_unmapped",
    "request_field_forbidden",
    "nonce_payload_mismatch",
    "nonce_in_flight",
    "handle_disabled",
    "probe_unproven",
    "module_grant_absent",
    "agent_grant_absent",
    "agent_grant_excludes_request",
    "assertion_scope_excludes_request",
    "policy_blocked",
    "approval_agent_mismatch",
    "approval_expired",
    "approval_denied",
    "comment_author_mismatch",
    "issue_author_mismatch",
    "stale_authority",
    "pipeline_denied",
    "handle_credential_unresolvable",
    "handle_credential_rejected",
    "vendor_rejected",
    "pre_dispatch_read_incomplete",
    "not_present",
    "issue_edit_not_own",
    "store_failure",
];

/// One row per class: the codes the shim treats that way. Every closed code
/// not listed here must classify as unknown (surfaced, never retried).
const CLASS_TABLE: &[(CodeClass, &[&str])] = &[
    (
        CodeClass::Transient,
        &[
            "nonce_in_flight",
            "assertion_key_unavailable",
            "store_failure",
            "pre_dispatch_read_incomplete",
            "handle_credential_unresolvable",
        ],
    ),
    (
        CodeClass::Remint,
        &[
            "assertion_expired",
            "assertion_not_yet_valid",
            "assertion_binding_generation_stale",
        ],
    ),
    (CodeClass::OutcomeUnknown, &["outcome_unknown"]),
    (
        CodeClass::Terminal,
        &[
            "agent_repository_conflict",
            "repository_unbound",
            "request_kind_unmapped",
            "request_field_forbidden",
            "comment_author_mismatch",
            "issue_author_mismatch",
            "issue_edit_not_own",
            "not_present",
            "vendor_rejected",
            "nonce_payload_mismatch",
            "policy_blocked",
            "agent_grant_absent",
            "agent_grant_excludes_request",
        ],
    ),
    (
        CodeClass::PlexusSetup,
        &[
            "module_grant_absent",
            "agent_handle_unknown",
            "agent_handle_mismatch",
            "handle_disabled",
            "probe_unproven",
            "assertion_scope_absent",
            "assertion_signature_invalid",
            "assertion_malformed",
            "assertion_wrong_surface",
        ],
    ),
];

#[test]
fn retry_classes_cover_plexus_closed_codes_one_row_per_class() {
    for (class, codes) in CLASS_TABLE {
        for code in *codes {
            assert_eq!(classify_code(code), *class, "{code}");
            // Codes can carry `{...}` detail; the prefix decides the class.
            assert_eq!(
                classify_code(&format!("{code}{{detail: x}}")),
                *class,
                "{code} with detail"
            );
        }
    }
    for code in CLOSED_CODES {
        let listed = CLASS_TABLE.iter().any(|(_, codes)| codes.contains(code));
        if !listed {
            assert_eq!(classify_code(code), CodeClass::Unknown, "{code}");
        }
    }
    for (_, codes) in CLASS_TABLE {
        for code in *codes {
            assert!(
                CLOSED_CODES.contains(code) || *code == "outcome_unknown",
                "{code} is not in plexus's closed taxonomy"
            );
        }
    }
    assert_eq!(classify_code("brand_new_code"), CodeClass::Unknown);
    assert_eq!(
        classify_code("handle_credential_unresolvable{class: transient}"),
        CodeClass::Transient
    );
    assert_eq!(
        classify_code("handle_credential_unresolvable{class: permanent}"),
        CodeClass::Unknown
    );
}

#[test]
fn agent_repository_conflict_names_both_agents() {
    let text = plexus_refusal_text(
        "agent_repository_conflict{claimed_agent: alfonso-aft, bound_agent: alfonso-plexus}",
        CodeClass::Terminal,
    );
    assert!(text.contains("speaks as agent alfonso-aft"), "{text}");
    assert!(text.contains("bound to agent alfonso-plexus"), "{text}");
    let setup = plexus_refusal_text("module_grant_absent", CodeClass::PlexusSetup);
    assert!(setup.starts_with("plexus configuration problem: module_grant_absent"));
}

fn ok_envelope(reply: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": reply}))
        .unwrap()
}

fn completed(result: Option<Value>) -> Value {
    let mut inner = json!({"status": "completed"});
    if let Some(result) = result {
        inner["result"] = result;
    }
    json!({"repo_binding_generation": 1, "result": inner})
}

fn rendered(reply: Value) -> String {
    let Reply::Completed { result, generation } = parse_reply(&ok_envelope(reply)) else {
        panic!("not a completed reply");
    };
    assert_eq!(generation, Some(1));
    render_completed(&result).unwrap()
}

#[test]
fn completed_replies_render_every_plexus_shape() {
    // Today's reply carries only a status.
    assert_eq!(rendered(completed(None)), "completed\n");
    // Speech verbs return the new comment or issue URL, printed like gh.
    assert_eq!(
        rendered(completed(Some(json!({
            "url": "https://github.com/org/repo/issues/42#issuecomment-7",
            "id": 7,
        })))),
        "https://github.com/org/repo/issues/42#issuecomment-7\n"
    );
    // Own-issue edits and labels return the issue's URL and id.
    assert_eq!(
        rendered(completed(Some(
            json!({"url": "https://github.com/org/repo/issues/42", "id": 42})
        ))),
        "https://github.com/org/repo/issues/42\n"
    );
    // Close and reopen return the state they applied.
    assert_eq!(
        rendered(completed(Some(
            json!({"state": "closed", "state_reason": "not_planned"})
        ))),
        "closed\nnot_planned\n"
    );
    assert_eq!(
        rendered(completed(Some(json!({"state": "open"})))),
        "open\n"
    );
    // A close or reopen that posted a comment prints the state, then the
    // comment's URL.
    assert_eq!(
        rendered(completed(Some(json!({
            "state": "closed",
            "state_reason": "completed",
            "comment": {"url": "https://github.com/org/repo/issues/42#issuecomment-9", "id": 9},
        })))),
        "closed\ncompleted\nhttps://github.com/org/repo/issues/42#issuecomment-9\n"
    );
    // Any field GitHub did not return is omitted: a comment without a URL
    // prints only the state, and a reply with only an id still renders.
    assert_eq!(
        rendered(completed(Some(
            json!({"state": "open", "comment": {"id": 3}})
        ))),
        "open\n"
    );
    assert_eq!(rendered(completed(Some(json!({"id": 5})))), "id: 5\n");
    // Reactions return their id and content, rendered in plexus's order.
    assert_eq!(
        rendered(completed(Some(json!({"id": 99, "content": "eyes"})))),
        "id: 99\ncontent: \"eyes\"\n"
    );
    // Plexus's own reaction golden, captured from a real facade run.
    let golden: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/plexus/seen-marker-reaction.json"
    ))
    .unwrap();
    assert_eq!(
        rendered(golden["success"].clone()),
        "content: \"eyes\"\nid: 701\n"
    );
    // A tool-call wrapper around the same reply is unwrapped first.
    let wrapped = json!({
        "content": [{"type": "text", "text": completed(Some(json!({"url": "https://x/1"}))).to_string()}],
    });
    assert_eq!(rendered(wrapped), "https://x/1\n");
}

#[test]
fn daemon_and_plexus_refusals_decode_with_their_stage() {
    let plexus = parse_reply(&ok_envelope(json!({
        "repo_binding_generation": 4,
        "result": {"status": "refused", "refusal_code": "repository_unbound"},
    })));
    assert_eq!(
        plexus,
        Reply::Refused {
            code: "repository_unbound".to_string(),
            stage: PLEXUS_REPLY_STAGE.to_string(),
            message: String::new(),
            generation: Some(4),
        }
    );
    let daemon = serde_json::to_vec(&json!({
        "op": BOT_REQUEST_OPERATION,
        "status": "error",
        "data": {"refusal_code": "assertion_session_unknown", "stage": "mint", "message": "m"},
    }))
    .unwrap();
    assert!(matches!(
        parse_reply(&daemon),
        Reply::Refused { ref code, ref stage, .. } if code == "assertion_session_unknown" && stage == "mint"
    ));
    assert!(matches!(
        daemon_refusal("assertion_session_unknown", "mint", ""),
        RouteOutcome::RelayRefusal { ref text, .. } if text.contains("prefrontal refused to mint")
    ));
    assert!(matches!(
        daemon_refusal("ticket_absent", "ticket", ""),
        RouteOutcome::NoAgentSession
    ));
}

#[test]
fn bindings_comparison_accepts_only_an_exact_match_and_shows_both_views() {
    let manifest = BTreeMap::from([("cortexkit/aft".to_string(), "alfonso-aft".to_string())]);
    let matching = json!({
        "repo_binding_generation": 5,
        "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"}],
    });
    assert_eq!(compare_bindings(&manifest, &matching), Ok(5));
    let extra = json!({
        "repo_binding_generation": 6,
        "bindings": [
            {"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"},
            {"repository": "cortexkit/plexus", "app_handle_id": "p", "agent_id": "alfonso-plexus"},
        ],
    });
    let error = compare_bindings(&manifest, &extra).unwrap_err();
    assert!(
        error.contains("manifest: cortexkit/aft->alfonso-aft"),
        "{error}"
    );
    assert!(
        error.contains("plexus: cortexkit/aft->alfonso-aft, cortexkit/plexus->alfonso-plexus"),
        "{error}"
    );
    assert_eq!(connection_id("alfonso-aft"), "github-handle-alfonso-aft");
}

// ---------------------------------------------------------------------------
// A fake AFT daemon that serves the relay operations over real subc framing.

type Handler = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

pub(in crate::gh_shim) struct FakeRelayDaemon {
    port: u16,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<std::thread::JoinHandle<()>>,
    /// Every relay request body the daemon received, in order.
    pub(in crate::gh_shim) requests: Arc<Mutex<Vec<Value>>>,
    pub(in crate::gh_shim) connections: Arc<Mutex<usize>>,
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

impl FakeRelayDaemon {
    /// `handler` answers each relay request (the whole `{op, params}` body)
    /// with the management reply envelope.
    pub(in crate::gh_shim) fn spawn(handler: Handler) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0));
        let server_requests = Arc::clone(&requests);
        let server_connections = Arc::clone(&connections);
        let server_key = key.clone();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            let Ok((mut stream, _)) = accepted else { break };
                            *server_connections.lock().unwrap() += 1;
                            let key = server_key.clone();
                            let handler = Arc::clone(&handler);
                            let requests = Arc::clone(&server_requests);
                            tokio::spawn(async move {
                                if subc_transport::authenticate_server(
                                    &mut stream, &key, &daemon_id, "subc-test", Duration::from_secs(5),
                                ).await.is_err() {
                                    return;
                                }
                                while let Ok(Some(frame)) = subc_transport::read_frame(&mut stream).await {
                                    let reply = match frame.header.ty {
                                        FrameType::Hello => Frame::build(
                                            FrameType::HelloAck, control_flags(), 0, 0, frame.header.corr,
                                            serde_json::to_vec(&ModuleHelloAckBody {
                                                negotiated_ver: PROTOCOL_VERSION,
                                                subc_ops: Vec::new(),
                                                subc_capabilities: Vec::new(),
                                                storage: None,
                                            }).unwrap(),
                                        ).unwrap(),
                                        FrameType::Request => {
                                            let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                                            let response = match body.get("op").and_then(Value::as_str) {
                                                Some("catalog.list") => json!({
                                                    "op": "catalog.list",
                                                    "generation": 1,
                                                    "modules": [{
                                                        "module_id": "aft",
                                                        "module_version": "0.0.0",
                                                        "roles": [{
                                                            "role": "management_surface",
                                                            "operations": [
                                                                {"name": BOT_REQUEST_OPERATION, "kind": "mutate"},
                                                                {"name": BINDINGS_READ_OPERATION, "kind": "query"},
                                                            ],
                                                            "config_schema": {},
                                                            "observability": [],
                                                            "identity_scope": []
                                                        }],
                                                        "control_ops": []
                                                    }],
                                                    "subc_ops": ["catalog.list", "route.open"]
                                                }),
                                                Some("route.open") => json!({"op": "route.open", "route_channel": 42, "route_epoch": 1}),
                                                Some("route.close") => json!({"op": "route.close"}),
                                                _ if frame.header.channel == 42 => {
                                                    requests.lock().unwrap().push(body.clone());
                                                    handler(&body)
                                                }
                                                _ => continue,
                                            };
                                            Frame::build_with_version(
                                                frame.header.ver, FrameType::Response, frame.header.flags,
                                                frame.header.channel, frame.header.epoch, frame.header.corr,
                                                serde_json::to_vec(&response).unwrap(),
                                            ).unwrap()
                                        }
                                        _ => continue,
                                    };
                                    if subc_transport::write_frame(&mut stream, &reply).await.is_err() {
                                        break;
                                    }
                                }
                            });
                        }
                    }
                }
            });
        });
        Self {
            port,
            key,
            daemon_id,
            shutdown_tx: Some(shutdown_tx),
            server: Some(server),
            requests,
            connections,
        }
    }

    pub(in crate::gh_shim) fn write_connection_file(&self, path: &Path) {
        connection_file::write_atomic(
            path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".to_string(),
                    port: self.port,
                }],
                key: self.key.clone(),
                daemon_id: self.daemon_id,
                pid: std::process::id(),
                daemon_ver: "gh-shim-relay-test".to_string(),
            },
        )
        .unwrap();
    }

    fn bot_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body["op"] == BOT_REQUEST_OPERATION)
            .cloned()
            .collect()
    }
}

impl Drop for FakeRelayDaemon {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

const TEST_NOW: u64 = 1_790_000_000;

fn v12_manifest() -> Manifest {
    serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v12-manifest.json")).unwrap()
}

fn bindings_ok() -> Value {
    json!({"op": BINDINGS_READ_OPERATION, "status": "ok", "data": {
        "repo_binding_generation": 1,
        "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"}],
    }})
}

fn plexus_refused(code: &str) -> Value {
    json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": {
        "repo_binding_generation": 1,
        "result": {"status": "refused", "refusal_code": code},
    }})
}

fn plexus_completed() -> Value {
    json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": {
        "repo_binding_generation": 1,
        "result": {"status": "completed", "result": {"url": "https://github.com/cortexkit/aft/issues/42#issuecomment-1", "id": 1}},
    }})
}

/// A handler that behaves like the real daemon for the ticket gate: it
/// redeems the ticket against this process's ticket registry and refuses an
/// unknown one, then answers bot requests from `script` in order.
fn ticket_gated(script: Vec<Value>) -> Handler {
    let script = Mutex::new(VecDeque::from(script));
    Arc::new(move |body: &Value| {
        let operation = body["op"].as_str().unwrap_or_default();
        let ticket = body["params"]["ticket"].as_str().unwrap_or_default();
        if crate::gh_shim_ticket::redeem(ticket).is_none() {
            return json!({"op": operation, "status": "error", "data": {
                "refusal_code": "ticket_unknown", "stage": "ticket", "message": "not live",
            }});
        }
        if operation == BINDINGS_READ_OPERATION {
            return bindings_ok();
        }
        script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(plexus_completed)
    })
}

struct Harness {
    _temp: tempfile::TempDir,
    daemon: FakeRelayDaemon,
    paths: StatePaths,
    connection_file: std::path::PathBuf,
}

fn harness(handler: Handler) -> Harness {
    let temp = tempfile::tempdir().unwrap();
    let daemon = FakeRelayDaemon::spawn(handler);
    let connection_file = temp.path().join("subc-connection.json");
    daemon.write_connection_file(&connection_file);
    Harness {
        paths: StatePaths::from_root(temp.path().join("state")),
        _temp: temp,
        daemon,
        connection_file,
    }
}

fn comment_args(body: &str) -> Vec<OsString> {
    [
        "issue",
        "comment",
        "42",
        "-R",
        "cortexkit/aft",
        "--body",
        body,
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// Run a governed `gh issue comment` through dispatch with `ticket`, and
/// report the exit status and whether upstream `gh` was reached.
fn dispatch_comment(harness: &Harness, ticket: Option<&str>, body: &str) -> (i32, bool) {
    let manifest = v12_manifest();
    let args = comment_args(body);
    let classification = classify(&args, &manifest, "macos");
    assert!(matches!(classification, Classification::Governed { .. }));
    let rung = RungDetermination::r3(
        TEST_NOW,
        manifest.manifest_version,
        &RungRecordProvenance {
            image_path: "/opt/cortexkit/aft-gh-shim".to_string(),
            version: "test".to_string(),
            repo_key: "cortexkit/aft".to_string(),
        },
    )
    .record;
    let binding = AgentBinding {
        repo: "cortexkit/aft".to_string(),
        agent_id: "alfonso-aft".to_string(),
    };
    let mut upstream_reached = false;
    let status = dispatch_r3_with_relay(
        &args,
        classification,
        &manifest,
        &harness.paths,
        &rung,
        &binding,
        TEST_NOW,
        |_| {
            upstream_reached = true;
            0
        },
        &RelayContext {
            connection_file: Some(harness.connection_file.clone()),
            ticket: ticket.map(str::to_string),
            transient_delays: [Duration::from_millis(1), Duration::from_millis(1)],
        },
    );
    (status, upstream_reached)
}

/// From a worker's session, naming the head agent's session id on the command
/// line and presenting a fabricated ticket (or none), a governed write is
/// refused and upstream `gh` is never reached.
#[test]
fn worker_session_with_typed_head_session_and_fabricated_ticket_is_refused() {
    let head = crate::gh_shim_ticket::ScopedTicket::issue("ses-head", "call-head", "/p");
    assert!(head.value().is_some());
    let harness = harness(ticket_gated(Vec::new()));

    let (status, upstream) = dispatch_comment(
        &harness,
        Some("fedcba9876543210fedcba9876543210"),
        "AFT_SESSION=ses-head --session ses-head",
    );
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(!upstream, "a governed write reached upstream gh");
    assert!(
        harness.daemon.bot_requests().is_empty(),
        "no bot request was relayed"
    );
    let seam = super::super::seam_state(&harness.paths);
    assert_eq!(seam.last_seam_refusal.unwrap().code, "ticket_unknown");

    // With no ticket at all the daemon is never contacted.
    let connections_before = *harness.daemon.connections.lock().unwrap();
    let (status, upstream) = dispatch_comment(&harness, None, "--session ses-head");
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(!upstream);
    assert_eq!(
        *harness.daemon.connections.lock().unwrap(),
        connections_before
    );
    assert_eq!(
        RefusalCode::UnboundIdentity.as_str(),
        "gh_shim_unbound_identity"
    );
}

#[test]
fn a_live_ticket_relays_the_governed_envelope_and_prints_the_url() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-worker-live", "call-live", "/p");
    let harness = harness(ticket_gated(Vec::new()));
    let (status, upstream) = dispatch_comment(&harness, live.value(), "hello");
    assert_eq!(status, 0);
    assert!(!upstream);
    let requests = harness.daemon.requests.lock().unwrap().clone();
    // The version check runs first at activation, then the write.
    assert_eq!(requests[0]["op"], BINDINGS_READ_OPERATION);
    assert_eq!(
        requests[0]["params"]["connection_id"],
        "github-handle-alfonso-aft"
    );
    let write = &requests[1];
    assert_eq!(write["op"], BOT_REQUEST_OPERATION);
    assert_eq!(write["params"]["ticket"], live.value().unwrap());
    assert_eq!(write["params"]["request"]["operation"], "gh.route");
    assert_eq!(write["params"]["request"]["action"], "issue comment");
    assert_eq!(
        write["params"]["request"]["metadata"]["agent_id"],
        "alfonso-aft"
    );
    assert!(write["params"].get("session").is_none());
}

fn nonces(harness: &Harness) -> Vec<String> {
    harness
        .daemon
        .bot_requests()
        .iter()
        .map(|body| {
            body["params"]["request_nonce"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn transient_refusals_retry_twice_with_the_same_nonce() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-transient", "call-t", "/p");
    let harness = harness(ticket_gated(vec![
        plexus_refused("nonce_in_flight"),
        plexus_refused("store_failure"),
    ]));
    let (status, _) = dispatch_comment(&harness, live.value(), "hi");
    assert_eq!(status, 0);
    let sent = nonces(&harness);
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|nonce| nonce == &sent[0]), "{sent:?}");

    let exhausted = self::harness(ticket_gated(vec![
        plexus_refused("store_failure"),
        plexus_refused("store_failure"),
        plexus_refused("store_failure"),
        plexus_completed(),
    ]));
    let (status, _) = dispatch_comment(&exhausted, live.value(), "hi");
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert_eq!(nonces(&exhausted).len(), 3, "two retries, then refuse");
}

#[test]
fn assertion_refusals_retry_once_for_a_fresh_mint() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-remint", "call-r", "/p");
    let harness = harness(ticket_gated(vec![plexus_refused("assertion_expired")]));
    let (status, _) = dispatch_comment(&harness, live.value(), "hi");
    assert_eq!(status, 0);
    let sent = nonces(&harness);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);

    let twice = self::harness(ticket_gated(vec![
        plexus_refused("assertion_not_yet_valid"),
        plexus_refused("assertion_binding_generation_stale"),
    ]));
    let (status, _) = dispatch_comment(&twice, live.value(), "hi");
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert_eq!(nonces(&twice).len(), 2);
}

#[test]
fn outcome_unknown_is_never_resent_and_exits_87() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-outcome", "call-o", "/p");
    let harness = harness(ticket_gated(vec![plexus_refused("outcome_unknown")]));
    let (status, upstream) = dispatch_comment(&harness, live.value(), "hi");
    assert_eq!(status, OUTCOME_UNKNOWN_EXIT_STATUS);
    assert!(!upstream);
    assert_eq!(nonces(&harness).len(), 1);
}

#[test]
fn terminal_setup_and_unknown_codes_are_not_retried() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-terminal", "call-x", "/p");
    for code in [
        "repository_unbound",
        "module_grant_absent",
        "agent_repository_conflict{claimed_agent: a, bound_agent: b}",
        "quota_exhausted_v2",
    ] {
        let harness = harness(ticket_gated(vec![plexus_refused(code)]));
        let (status, upstream) = dispatch_comment(&harness, live.value(), "hi");
        assert_eq!(status, REFUSAL_EXIT_STATUS, "{code}");
        assert!(!upstream);
        assert_eq!(nonces(&harness).len(), 1, "{code} was retried");
        let seam = super::super::seam_state(&harness.paths);
        assert_eq!(seam.last_seam_refusal.unwrap().code, code);
    }
}

#[test]
fn a_bindings_mismatch_at_activation_refuses_before_any_write() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-mismatch", "call-mm", "/p");
    let handler: Handler = Arc::new(|body: &Value| {
        if body["op"] == BINDINGS_READ_OPERATION {
            return json!({"op": BINDINGS_READ_OPERATION, "status": "ok", "data": {
                "repo_binding_generation": 2,
                "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-other"}],
            }});
        }
        plexus_completed()
    });
    let harness = harness(handler);
    let (status, upstream) = dispatch_comment(&harness, live.value(), "hi");
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(!upstream);
    assert!(harness.daemon.bot_requests().is_empty());
    let seam = super::super::seam_state(&harness.paths);
    assert_eq!(
        seam.last_seam_refusal.unwrap().code,
        "binding_view_mismatch"
    );
}

#[test]
fn a_changed_binding_generation_triggers_a_new_check() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-generation", "call-g", "/p");
    let generation = Arc::new(Mutex::new(1_u64));
    let reads = Arc::new(Mutex::new(0_usize));
    let handler_generation = Arc::clone(&generation);
    let handler_reads = Arc::clone(&reads);
    let handler: Handler = Arc::new(move |body: &Value| {
        let current = *handler_generation.lock().unwrap();
        if body["op"] == BINDINGS_READ_OPERATION {
            *handler_reads.lock().unwrap() += 1;
            return json!({"op": BINDINGS_READ_OPERATION, "status": "ok", "data": {
                "repo_binding_generation": current,
                "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"}],
            }});
        }
        json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": {
            "repo_binding_generation": current,
            "result": {"status": "completed"},
        }})
    });
    let harness = harness(handler);
    assert_eq!(dispatch_comment(&harness, live.value(), "one").0, 0);
    assert_eq!(dispatch_comment(&harness, live.value(), "two").0, 0);
    assert_eq!(
        *reads.lock().unwrap(),
        1,
        "an unchanged generation is not re-read"
    );
    *generation.lock().unwrap() = 2;
    assert_eq!(dispatch_comment(&harness, live.value(), "three").0, 0);
    assert_eq!(
        *reads.lock().unwrap(),
        2,
        "a new generation is checked again"
    );
}
