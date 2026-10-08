use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

use super::super::{
    classify, dispatch_r3_with_relay_at, AgentBinding, Classification, Manifest, RefusalCode,
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
fn issue_and_pr_creation_replies_print_only_the_url_like_gh() {
    for url in [
        "https://github.com/org/repo/issues/42",
        "https://github.com/org/repo/pull/43",
    ] {
        assert_eq!(
            rendered(completed(Some(
                json!({"url": url, "id": 7, "number": 42, "title": "Title"})
            ))),
            format!("{url}\n")
        );
    }
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
    let manifest = BTreeMap::from([
        ("cortexkit/aft".to_string(), "alfonso-aft".to_string()),
        ("cortexkit/commons".to_string(), "alfonso-subc".to_string()),
    ]);
    // plexus's real reply shape wraps the body in `result`; the manifest may
    // govern repositories (commons) that plexus does not bind.
    let matching = json!({"result": {
        "repo_binding_generation": 5,
        "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"}],
    }});
    assert_eq!(compare_bindings(&manifest, &matching), Ok(5));
    let bare = matching["result"].clone();
    assert_eq!(compare_bindings(&manifest, &bare), Ok(5));
    assert!(
        compare_bindings(&manifest, &json!({"error": {"code": "connection_unknown"}}))
            .unwrap_err()
            .contains("connection_unknown")
    );
    let wrong_agent = json!({"result": {
        "repo_binding_generation": 7,
        "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "someone-else"}],
    }});
    assert!(compare_bindings(&manifest, &wrong_agent)
        .unwrap_err()
        .contains("for cortexkit/aft->someone-else"));
    let extra = json!({"result": {
        "repo_binding_generation": 6,
        "bindings": [
            {"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"},
            {"repository": "cortexkit/plexus", "app_handle_id": "p", "agent_id": "alfonso-plexus"},
        ],
    }});
    let error = compare_bindings(&manifest, &extra).unwrap_err();
    assert!(
        error.contains("for cortexkit/plexus->alfonso-plexus"),
        "{error}"
    );
    assert!(
        error.contains("manifest: cortexkit/aft->alfonso-aft, cortexkit/commons->alfonso-subc"),
        "{error}"
    );
    assert!(
        error.contains("plexus: cortexkit/aft->alfonso-aft, cortexkit/plexus->alfonso-plexus"),
        "{error}"
    );
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
                                                machine_id: None,
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
                                                    // A null reply means "stay silent".
                                                    match handler(&body) {
                                                        Value::Null => continue,
                                                        reply => reply,
                                                    }
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
    json!({"op": BINDINGS_READ_OPERATION, "status": "ok", "data": {"result": {
        "repo_binding_generation": 1,
        "bindings": [{"repository": "cortexkit/aft", "app_handle_id": "h", "agent_id": "alfonso-aft"}],
    }}})
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
    let status = dispatch_r3_with_relay_at(
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
            relay_budget: Duration::from_secs(5),
        },
        &harness._temp.path().join("storage"),
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
    assert!(
        harness._temp.path().join("storage/aft.db").is_file(),
        "successful relay invalidates reads in the fixture storage"
    );
    let requests = harness.daemon.requests.lock().unwrap().clone();
    // The version check runs first at activation, then the write.
    assert_eq!(requests[0]["op"], BINDINGS_READ_OPERATION);
    assert_eq!(requests[0]["params"]["agent_id"], "alfonso-aft");
    assert!(requests[0]["params"].get("connection_id").is_none());
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

#[test]
fn session_agent_mismatch_names_manifest_repository_and_known_bot_without_lookup() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-mismatch", "call-mm", "/p");
    for known_handle in [false, true] {
        let refusal = json!({"op": BOT_REQUEST_OPERATION, "status": "error", "data": {
            "refusal_code": "assertion_session_agent_mismatch", "stage": "mint",
            "message": "residence session ses-mismatch belongs to agent agent_session, not the requested agent agent_bound",
        }});
        let harness = harness(Arc::new(move |body| {
            if known_handle && body["op"] == BINDINGS_READ_OPERATION {
                json!({"op": BINDINGS_READ_OPERATION, "status": "ok", "data": {"result": {
                    "repo_binding_generation": 1, "bindings": [{
                        "repository": "cortexkit/aft", "agent_id": "agent_bound", "app_handle_id": "aft-alfonso[bot]",
                    }],
                }}})
            } else {
                refusal.clone()
            }
        }));
        let mut manifest = v12_manifest();
        manifest
            .bindings
            .insert("cortexkit/aft".into(), "agent_bound".into());
        let binding = AgentBinding {
            repo: "cortexkit/aft".into(),
            agent_id: "agent_bound".into(),
        };
        let args = comment_args("hi");
        let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
        else {
            panic!("not governed");
        };
        let request = super::super::canonicalize_governed(
            &args,
            &tuple,
            &canonical,
            manifest.manifest_version,
        )
        .unwrap();
        let rung = RungDetermination::r3(
            TEST_NOW,
            manifest.manifest_version,
            &RungRecordProvenance {
                image_path: "/shim".into(),
                version: "test".into(),
                repo_key: "cortexkit/aft".into(),
            },
        )
        .record;
        let outcome = route(
            &harness.paths,
            &rung,
            &binding,
            request,
            TEST_NOW,
            &manifest,
            &RelayContext {
                connection_file: Some(harness.connection_file.clone()),
                ticket: live.value().map(str::to_string),
                transient_delays: [Duration::from_millis(1); 2],
                relay_budget: Duration::from_secs(5),
            },
        );
        let RouteOutcome::RelayRefusal { code, text } = outcome else {
            panic!("not refused: {outcome:?}");
        };
        let identity = if known_handle {
            "aft-alfonso[bot] (agent_bound)"
        } else {
            "agent_bound"
        };
        assert!(
            text.starts_with(&format!(
                "cortexkit/aft is bound to {identity}; this session is agent_session; "
            )),
            "{text}"
        );
        assert!(text.contains("residence session ses-mismatch belongs to agent agent_session"));
        assert_eq!(code, "assertion_session_agent_mismatch");
        assert_eq!(
            harness.daemon.requests.lock().unwrap().len(),
            if known_handle { 2 } else { 1 },
            "no lookup or retry"
        );
    }
}

#[test]
fn governed_thread_state_confirmations_match_gh_sentences_on_stderr() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-state-output", "call-so", "/p");
    for (verb, noun, action, state) in [
        ("issue", "issue", "close", "closed"),
        ("issue", "issue", "reopen", "open"),
        ("pr", "pull request", "close", "closed"),
        ("pr", "pull request", "reopen", "open"),
    ] {
        for title in [None, Some("A real title")] {
            let mut result = json!({"state": state, "state_reason": "not_planned", "url": "https://github.com/cortexkit/aft/issues/42", "comment": {"url": "https://github.com/cortexkit/aft/issues/42#issuecomment-9"}});
            if let Some(title) = title {
                result["title"] = json!(title);
            }
            let harness = harness(ticket_gated(vec![
                json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": completed(Some(result))}),
            ]));
            let manifest = v12_manifest();
            let mut args = vec![
                OsString::from(verb),
                OsString::from(action),
                OsString::from("42"),
                OsString::from("-R"),
                OsString::from("cortexkit/aft"),
            ];
            if verb == "issue" && action == "close" {
                args.extend([OsString::from("--reason"), OsString::from("not planned")]);
            }
            let Classification::Governed { tuple, canonical } = classify(&args, &manifest, "macos")
            else {
                panic!("not governed");
            };
            let request = super::super::canonicalize_governed(
                &args,
                &tuple,
                &canonical,
                manifest.manifest_version,
            )
            .unwrap();
            let rung = RungDetermination::r3(
                TEST_NOW,
                manifest.manifest_version,
                &RungRecordProvenance {
                    image_path: "/shim".into(),
                    version: "test".into(),
                    repo_key: "cortexkit/aft".into(),
                },
            )
            .record;
            let binding = AgentBinding {
                repo: "cortexkit/aft".into(),
                agent_id: "alfonso-aft".into(),
            };
            let outcome = route(
                &harness.paths,
                &rung,
                &binding,
                request,
                TEST_NOW,
                &manifest,
                &RelayContext {
                    connection_file: Some(harness.connection_file.clone()),
                    ticket: live.value().map(str::to_string),
                    transient_delays: [Duration::from_millis(1); 2],
                    relay_budget: Duration::from_secs(5),
                },
            );
            let RouteOutcome::ResultStderr(text) = outcome else {
                panic!("expected gh stderr confirmation, got {outcome:?}");
            };
            let past = if action == "close" {
                "Closed"
            } else {
                "Reopened"
            };
            let suffix = title.map(|title| format!(" ({title})")).unwrap_or_default();
            assert_eq!(text, format!("✓ {past} {noun} cortexkit/aft#42{suffix}\n"));
            assert_eq!(
                harness.daemon.requests.lock().unwrap().len(),
                2,
                "no additional title lookup"
            );
        }
    }
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
fn unknown_result_status_is_outcome_undetermined_and_never_resent() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-future", "call-future", "/p");
    let reply = json!({"op": BOT_REQUEST_OPERATION, "status": "ok", "data": {
        "result": {"status": "some_future_state"},
    }});
    let harness = harness(ticket_gated(vec![reply]));
    let manifest = v12_manifest();
    let args = comment_args("hi");
    let Classification::Governed {
        tuple, canonical, ..
    } = classify(&args, &manifest, "macos")
    else {
        panic!("not governed");
    };
    let request =
        super::super::canonicalize_governed(&args, &tuple, &canonical, manifest.manifest_version)
            .unwrap();
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
    let outcome = route(
        &harness.paths,
        &rung,
        &binding,
        request,
        TEST_NOW,
        &manifest,
        &RelayContext {
            connection_file: Some(harness.connection_file.clone()),
            ticket: live.value().map(str::to_string),
            transient_delays: [Duration::from_millis(1); 2],
            relay_budget: Duration::from_secs(5),
        },
    );
    assert_eq!(harness.daemon.bot_requests().len(), 1, "never resent");
    let RouteOutcome::OutcomeUndetermined(text) = &outcome else {
        panic!("expected OutcomeUndetermined, got {outcome:?}");
    };
    assert!(text.contains("some_future_state"));
    assert!(text.contains("the bot write may have executed; check before retrying"));
    assert!(text.contains("gh api repos/<owner>/<repo>/issues/<n>/comments"));
    assert_eq!(
        super::super::governed_outcome_status(&harness.paths, &binding, TEST_NOW, outcome),
        OUTCOME_UNKNOWN_EXIT_STATUS
    );
}

#[test]
fn completed_reply_with_inner_result_content_decodes_as_completed() {
    let mut reply = completed(None);
    reply["content"] = json!([{"type": "text", "text": reply["result"].to_string()}]);
    assert_eq!(rendered(reply), "completed\n");
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
        "scope_not_synced",
        "scope_ended",
        "delegation_withdrawn",
        "scope_unverifiable",
        "agent_identity_conflict",
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

#[test]
fn production_relay_budget_is_thirty_seconds() {
    assert_eq!(RELAY_BUDGET, Duration::from_secs(30));
    if std::env::var_os(REQUEST_TIMEOUT_TEST_ENV).is_none() {
        assert_eq!(RelayContext::from_process().relay_budget, RELAY_BUDGET);
    }
}

/// A write that gets no reply within the relay budget is an unknown
/// outcome, reported with the budget that actually applied, and never resent.
#[test]
fn a_silent_relay_reports_outcome_unknown_after_the_configured_budget() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-silent", "call-s", "/p");
    let handler: Handler = Arc::new(|body: &Value| {
        if body["op"] == BINDINGS_READ_OPERATION {
            return bindings_ok();
        }
        Value::Null
    });
    let harness = harness(handler);
    let manifest = v12_manifest();
    let args = comment_args("hi");
    let classification = classify(&args, &manifest, "macos");
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
    let status = dispatch_r3_with_relay_at(
        &args,
        classification,
        &manifest,
        &harness.paths,
        &rung,
        &binding,
        TEST_NOW,
        |_| panic!("reached upstream gh"),
        &RelayContext {
            connection_file: Some(harness.connection_file.clone()),
            ticket: live.value().map(str::to_string),
            transient_delays: [Duration::from_millis(1); 2],
            relay_budget: Duration::from_millis(400),
        },
        &harness._temp.path().join("storage"),
    );
    assert_eq!(status, OUTCOME_UNKNOWN_EXIT_STATUS);
    assert_eq!(harness.daemon.bot_requests().len(), 1, "never resent");
    let probe = super::super::read_last_probe(&harness.paths).expect("last probe");
    assert_eq!(probe.stage, "request");
    assert_eq!(probe.elapsed_ms, 400);
    assert!(super::super::outcome_unknown_text(probe.elapsed_ms).contains("within 400 ms"));
}

/// Run a governed comment through dispatch with explicit relay timing and
/// report the exit status and how long the whole invocation took.
fn dispatch_timed(
    harness: &Harness,
    ticket: Option<&str>,
    transient_delays: [Duration; 2],
    relay_budget: Duration,
) -> (i32, Duration) {
    let manifest = v12_manifest();
    let args = comment_args("hi");
    let classification = classify(&args, &manifest, "macos");
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
    let started = std::time::Instant::now();
    let status = dispatch_r3_with_relay_at(
        &args,
        classification,
        &manifest,
        &harness.paths,
        &rung,
        &binding,
        TEST_NOW,
        |_| panic!("reached upstream gh"),
        &RelayContext {
            connection_file: Some(harness.connection_file.clone()),
            ticket: ticket.map(str::to_string),
            transient_delays,
            relay_budget,
        },
        &harness._temp.path().join("storage"),
    );
    (status, started.elapsed())
}

/// Transient refusals whose retries would outlast the relay budget stop at
/// the budget instead of sleeping past it. Each refusal proves nothing was
/// written, so the command exits with the named refusal (86).
#[test]
fn transient_retries_stop_inside_the_relay_budget_with_a_named_refusal() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-budget", "call-b", "/p");
    // Every write attempt takes 400 ms and is refused as transient, and each
    // retry sleeps 700 ms. After the second attempt ends (about 1.5 s) the
    // next sleep would end past the 2 s budget, so the shim stops there;
    // all three attempts would need 2.6 s.
    let handler: Handler = Arc::new(|body: &Value| {
        if body["op"] == BINDINGS_READ_OPERATION {
            return bindings_ok();
        }
        std::thread::sleep(Duration::from_millis(400));
        plexus_refused("store_failure")
    });
    let harness = harness(handler);
    let budget = Duration::from_secs(2);
    let (status, elapsed) = dispatch_timed(
        &harness,
        live.value(),
        [Duration::from_millis(700); 2],
        budget,
    );
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(
        elapsed < budget,
        "took {elapsed:?} against a {budget:?} budget"
    );
    let sent = nonces(&harness);
    assert_eq!(sent.len(), 2, "the second retry would pass the budget");
    assert_eq!(sent[0], sent[1], "a retry reuses the request nonce");
    let seam = super::super::seam_state(&harness.paths);
    assert_eq!(seam.last_seam_refusal.unwrap().code, "store_failure");
}

/// A retried attempt only gets what is left of the relay budget, so a silent
/// reply to it is reported as an unknown outcome (87) at the budget, not a
/// fresh full budget after the retry sleep.
#[test]
fn a_silent_retry_is_an_unknown_outcome_at_the_relay_budget() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-budget-silent", "call-bs", "/p");
    let attempts = Arc::new(Mutex::new(0_usize));
    let handler_attempts = Arc::clone(&attempts);
    let handler: Handler = Arc::new(move |body: &Value| {
        if body["op"] == BINDINGS_READ_OPERATION {
            return bindings_ok();
        }
        let mut attempts = handler_attempts.lock().unwrap();
        *attempts += 1;
        if *attempts == 1 {
            plexus_refused("nonce_in_flight")
        } else {
            Value::Null
        }
    });
    let harness = harness(handler);
    let budget = Duration::from_millis(1_000);
    ATTEMPT_DEADLINES.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
    let (status, _elapsed) = dispatch_timed(
        &harness,
        live.value(),
        [Duration::from_millis(400); 2],
        budget,
    );
    assert_eq!(status, OUTCOME_UNKNOWN_EXIT_STATUS);
    // Setup and scheduling do not consume the exchange budget. Observe the
    // actual CallOptions passed to the consumer: resetting the timeout for the
    // retry would give it a different deadline even if this thread ran late.
    let deadlines = ATTEMPT_DEADLINES.with(|slot| slot.borrow_mut().take().unwrap());
    assert_eq!(deadlines.len(), 2);
    assert_eq!(deadlines[0], deadlines[1], "retry received a fresh budget");
    assert_eq!(
        nonces(&harness).len(),
        2,
        "the silent retry is never resent"
    );
    let probe = super::super::read_last_probe(&harness.paths).expect("last probe");
    assert_eq!(probe.elapsed_ms, 1_000);
}

fn v16_manifest() -> Manifest {
    serde_json::from_str(include_str!("../tests/fixtures/gh_shim/v16-manifest.json")).unwrap()
}

/// Run a governed `gh pr create` through dispatch against the v16 fixture and
/// report the exit status and whether upstream `gh` was reached.
fn dispatch_pr_create(harness: &Harness, ticket: Option<&str>) -> (i32, bool) {
    let manifest = v16_manifest();
    let args: Vec<OsString> = [
        "pr",
        "create",
        "-R",
        "cortexkit/aft",
        "--base",
        "main",
        "--head",
        "no-such-branch",
        "--title",
        "T",
        "--body",
        "B",
    ]
    .iter()
    .map(OsString::from)
    .collect();
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
    let status = dispatch_r3_with_relay_at(
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
            relay_budget: Duration::from_secs(5),
        },
        &harness._temp.path().join("storage"),
    );
    (status, upstream_reached)
}

/// Plexus's reply for a created pull request is `{number, url, state, draft}`.
/// `gh pr create` prints the URL, and the shim does the same.
#[test]
fn a_created_pull_request_prints_its_url() {
    let result = json!({
        "number": 7,
        "url": "https://github.com/cortexkit/aft/pull/7",
        "state": "open",
        "draft": false,
    });
    assert_eq!(
        rendered(completed(Some(result))),
        "https://github.com/cortexkit/aft/pull/7\n"
    );
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-pr-create", "call-pc", "/p");
    let harness = harness(ticket_gated(vec![json!({
        "op": BOT_REQUEST_OPERATION, "status": "ok", "data": {
            "repo_binding_generation": 1,
            "result": {"status": "completed", "result": {
                "number": 7, "url": "https://github.com/cortexkit/aft/pull/7", "state": "open", "draft": false,
            }},
        },
    })]));
    let (status, upstream) = dispatch_pr_create(&harness, live.value());
    assert_eq!(status, 0);
    assert!(!upstream);
    let sent = harness.daemon.bot_requests();
    assert_eq!(sent.len(), 1);
    let request = &sent[0]["params"]["request"];
    assert_eq!(request["action"], "pr create");
    assert_eq!(request["target"], json!({}));
    assert_eq!(
        request["body"],
        json!({"title": "T", "body": "B", "base": "main", "head": "no-such-branch", "draft": false})
    );
    assert_eq!(request["repository"], "cortexkit/aft");
}

/// The shim cannot know which branches exist, so a head GitHub does not have
/// is GitHub's refusal to report. It comes back as a named seam refusal with
/// plexus's code verbatim, including GitHub's status and message, and is
/// neither retried nor handed to upstream `gh`.
#[test]
fn a_github_head_refusal_is_relayed_verbatim_and_never_retried() {
    let live = crate::gh_shim_ticket::ScopedTicket::issue("ses-pr-head", "call-ph", "/p");
    // GitHub answers an unknown head with 422 "Validation Failed" and an
    // error naming the `head` field as invalid. Plexus spells a vendor
    // refusal as `vendor_rejected{...}`; both the status-only spelling it
    // uses today and one carrying GitHub's message must pass through intact.
    for code in [
        "vendor_rejected{status: 422}",
        r#"vendor_rejected{status: 422, message: "Validation Failed", errors: [{"resource": "PullRequest", "field": "head", "code": "invalid"}]}"#,
    ] {
        let harness = harness(ticket_gated(vec![plexus_refused(code)]));
        let (status, upstream) = dispatch_pr_create(&harness, live.value());
        assert_eq!(status, REFUSAL_EXIT_STATUS, "{code}");
        assert!(!upstream, "{code} fell through to upstream gh");
        assert_eq!(nonces(&harness).len(), 1, "{code} was retried");
        let seam = super::super::seam_state(&harness.paths);
        assert_eq!(seam.last_seam_refusal.unwrap().code, code);
        // The caller-facing text is the named seam refusal carrying the code,
        // not a paraphrase of it.
        let text = plexus_refusal_text(code, classify_code(code));
        assert_eq!(text, format!("plexus refused the bot write: {code}"));
        assert_eq!(RefusalCode::SeamRefusal.as_str(), "gh_shim_seam_refusal");
    }
    // Plexus's own `pr_head_cross_repository` refusal takes the same path.
    let harness = harness(ticket_gated(vec![plexus_refused(
        "pr_head_cross_repository",
    )]));
    let (status, upstream) = dispatch_pr_create(&harness, live.value());
    assert_eq!(status, REFUSAL_EXIT_STATUS);
    assert!(!upstream);
    assert_eq!(nonces(&harness).len(), 1);
    assert_eq!(
        super::super::seam_state(&harness.paths)
            .last_seam_refusal
            .unwrap()
            .code,
        "pr_head_cross_repository"
    );
}
