//! Wire goldens for the governed `gh.route` exchange.
//!
//! Every case drives the real `aft gh-shim` binary against a fake route holder
//! that speaks the subc protocol on loopback, records the bytes the shim writes
//! to the route, and answers with a response file read verbatim from
//! `docs/investigations/gh-shim-wire-goldens/responses/`. The captured request
//! bytes and the shim's exit code, stdout and stderr are compared against the
//! checked-in goldens, so a change to the request serializer or to how a
//! response is rendered fails here by name instead of drifting silently.
//!
//! Regenerate the captured files (never the hand-written responses) with:
//!
//! ```sh
//! AFT_GH_SHIM_WIRE_GOLDENS_REGEN=1 cargo test -p agent-file-tools \
//!     --test rest gh_shim_wire_goldens_test:: -- --test-threads=1
//! ```
//!
//! Isolation: every run gets its own HOME, XDG config/state directories,
//! `AFT_GH_SHIM_STATE_DIR` and `AFT_STORAGE_DIR` under a temporary directory,
//! the subc connection file points at the loopback fake holder, and PATH puts a
//! recording stand-in for upstream `gh` first. Nothing reaches GitHub, a real
//! route holder, or the operator's shim state.
#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use ring::signature::Ed25519KeyPair;
use serde_json::{json, Value};
use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, Priority, PROTOCOL_VERSION};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{DAEMON_ID_LEN, KEY_LEN};

#[path = "../src/gh_shim/plexus_bindings_contract.rs"]
mod plexus_bindings_contract;
/// The stand-in holder's copy of Plexus's published `gh.route` request
/// contract (`crates/aft/src/gh_shim/plexus_contract_judge.rs`, vendored
/// fixtures under `crates/aft/tests/fixtures/gh_shim/plexus-contract/`).
/// Judging every incoming `gh.route` request against it is what keeps a test
/// stand-in from ever again being more lenient than Plexus's real route.
#[path = "../src/gh_shim/plexus_contract_judge.rs"]
mod plexus_contract_judge;

/// Seed of the dev manifest-signing key compiled into debug builds; see
/// `crates/aft/tests/fixtures/gh_shim/README.md`.
const DEV_MANIFEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
const REGEN_ENV: &str = "AFT_GH_SHIM_WIRE_GOLDENS_REGEN";
const REPOSITORY: &str = "cortexkit/aft";
/// Bound in the test manifest next to `REPOSITORY`, and never the origin of
/// any checkout a test runs in.
const OTHER_BOUND_REPOSITORY: &str = "cortexkit/magic-context";
const OTHER_BOUND_AGENT: &str = "alfonso-magic-context";
/// Named by no binding in the test manifest.
const UNBOUND_REPOSITORY: &str = "someone/unbound";
/// Route channel the fake holder hands out on `route.open`; a request frame on
/// this channel is the governed `gh.route` payload.
const ROUTE_CHANNEL: u16 = 42;

fn goldens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/investigations/gh-shim-wire-goldens")
}

fn regenerating() -> bool {
    std::env::var_os(REGEN_ENV).is_some_and(|value| value == "1")
}

fn aft_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_secs()
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// One governed verb: the golden file stem and the argv after `gh`.
struct Verb {
    name: &'static str,
    argv: &'static [&'static str],
}

const VERBS: &[Verb] = &[
    Verb {
        name: "v1-issue-comment",
        argv: &[
            "issue",
            "comment",
            "42",
            "--body",
            "A governed issue comment.",
        ],
    },
    Verb {
        name: "v1-pr-comment",
        argv: &[
            "pr",
            "comment",
            "7",
            "--body",
            "A governed pull-request comment.",
        ],
    },
    Verb {
        name: "v1-pr-review",
        argv: &[
            "pr",
            "review",
            "7",
            "--comment",
            "--body",
            "A governed review comment.",
        ],
    },
    Verb {
        name: "v1-issue-reaction",
        argv: &["issue", "reaction", "42", "--reaction", "+1"],
    },
    Verb {
        name: "v10-issue-comment-edit-last",
        argv: &[
            "issue",
            "comment",
            "42",
            "--edit-last",
            "--body",
            "Replacement text for the bot's last comment.",
        ],
    },
    Verb {
        name: "v10-pr-comment-edit-last",
        argv: &[
            "pr",
            "comment",
            "7",
            "--edit-last",
            "--body",
            "Replacement text for the bot's last comment.",
        ],
    },
    Verb {
        name: "v12-issue-close",
        argv: &[
            "issue",
            "close",
            "42",
            "--reason",
            "not planned",
            "--comment",
            "Closing as not planned.",
        ],
    },
    Verb {
        name: "v12-issue-reopen",
        argv: &[
            "issue",
            "reopen",
            "42",
            "--comment",
            "Reopening for another pass.",
        ],
    },
    Verb {
        name: "v12-pr-close",
        argv: &[
            "pr",
            "close",
            "7",
            "--comment",
            "Superseded by a newer pull request.",
        ],
    },
    // Deliberately without --comment: the optional comment key is then absent
    // from the wire rather than null.
    Verb {
        name: "v12-pr-reopen",
        argv: &["pr", "reopen", "7"],
    },
    Verb {
        name: "v14-issue-create",
        argv: &[
            "issue",
            "create",
            "--title",
            "A governed issue",
            "--body",
            "Filed by the bot.",
            "--label",
            "bug",
            "--label",
            "p1",
        ],
    },
    // Own-issue edit: the holder checks the issue was opened by the calling
    // seat's bot, so the request carries `author_scope: "own"`.
    Verb {
        name: "v14-issue-edit-title-body",
        argv: &[
            "issue",
            "edit",
            "42",
            "--title",
            "A retitled issue",
            "--body",
            "Edited by the bot.",
        ],
    },
    Verb {
        name: "v14-issue-edit-labels",
        argv: &[
            "issue",
            "edit",
            "42",
            "--add-label",
            "triaged",
            "--remove-label",
            "needs-triage",
        ],
    },
    Verb {
        name: "v14-api-patch-issue-comment",
        argv: &[
            "api",
            "-X",
            "PATCH",
            "/repos/cortexkit/aft/issues/comments/123",
            "-f",
            "body=Edited comment text.",
        ],
    },
];

fn verb(name: &str) -> &'static Verb {
    VERBS
        .iter()
        .find(|verb| verb.name == name)
        .unwrap_or_else(|| panic!("unknown verb {name}"))
}

/// What the fake holder does with the governed request.
#[derive(Clone)]
enum HolderReply {
    /// Answer with the exact bytes of a file under `responses/`.
    Respond(&'static str),
    /// Read the request and never answer, so the shim's 5 s call timeout
    /// fires after the request was written: the outcome-unknown exchange.
    Silent,
}

/// One recorded exchange: which verb, which holder reply, and the file stem
/// under `exchanges/` that stores the shim's exit code, stdout and stderr.
struct Exchange {
    name: &'static str,
    verb: &'static str,
    reply: HolderReply,
}

fn success_response_for(verb: &str) -> &'static str {
    match verb {
        "v12-issue-close" | "v12-pr-close" => "success-applied-closed.response.json",
        "v12-issue-reopen" | "v12-pr-reopen" => "success-applied-open.response.json",
        _ => "success-result.response.json",
    }
}

fn success_exchanges() -> Vec<Exchange> {
    VERBS
        .iter()
        .map(|verb| Exchange {
            name: leak(format!("{}-success", verb.name)),
            verb: verb.name,
            reply: HolderReply::Respond(success_response_for(verb.name)),
        })
        .collect()
}

fn refusal_exchanges() -> Vec<Exchange> {
    vec![
        Exchange {
            name: "v1-issue-comment-refusal",
            verb: "v1-issue-comment",
            reply: HolderReply::Respond("refusal-identity_mismatch.response.json"),
        },
        Exchange {
            name: "v10-issue-comment-edit-last-refusal",
            verb: "v10-issue-comment-edit-last",
            reply: HolderReply::Respond("refusal-identity_mismatch.response.json"),
        },
        Exchange {
            name: "v12-issue-close-refusal",
            verb: "v12-issue-close",
            reply: HolderReply::Respond("refusal-identity_mismatch.response.json"),
        },
        Exchange {
            name: "v14-issue-create-refusal",
            verb: "v14-issue-create",
            reply: HolderReply::Respond("refusal-identity_mismatch.response.json"),
        },
        Exchange {
            name: "v14-issue-edit-title-body-refusal",
            verb: "v14-issue-edit-title-body",
            reply: HolderReply::Respond("refusal-identity_mismatch.response.json"),
        },
        // The route holder (prefrontal-core) refuses with this code when the
        // issue was not opened by the calling seat's bot.
        Exchange {
            name: "v14-issue-edit-title-body-refusal-issue_edit_not_own",
            verb: "v14-issue-edit-title-body",
            reply: HolderReply::Respond("refusal-issue_edit_not_own.response.json"),
        },
        Exchange {
            name: "v14-issue-edit-labels-refusal-issue_edit_not_own",
            verb: "v14-issue-edit-labels",
            reply: HolderReply::Respond("refusal-issue_edit_not_own.response.json"),
        },
        Exchange {
            name: "v14-api-patch-issue-comment-refusal-custody_unreachable",
            verb: "v14-api-patch-issue-comment",
            reply: HolderReply::Respond("refusal-custody_unreachable.response.json"),
        },
        // The gh.route unbound-identity, upstream-error and "state applied,
        // comment failed" replies have no counterpart on the AFT relay: plexus
        // answers with a completed result or a refusal code, and posts a close
        // comment before changing state, so those exchanges are not recorded.
    ]
}

fn outcome_unknown_exchanges() -> Vec<Exchange> {
    [
        "v1-issue-comment",
        "v10-pr-comment-edit-last",
        "v12-pr-close",
        "v14-api-patch-issue-comment",
    ]
    .into_iter()
    .map(|verb| Exchange {
        name: leak(format!("{verb}-outcome-unknown")),
        verb,
        reply: HolderReply::Silent,
    })
    .collect()
}

fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

// ---------------------------------------------------------------------------
// Fake route holder
// ---------------------------------------------------------------------------

/// A request frame the holder read, in arrival order. For the relayed bot
/// write, `on_route` is set and `body` holds just the governed `gh.route`
/// envelope the shim put in `params.request`, so the request goldens keep
/// pinning exactly those bytes.
#[derive(Clone, Debug)]
struct CapturedFrame {
    on_route: bool,
    body: Vec<u8>,
}

/// Translate a recorded `gh.route` holder response into the reply the AFT
/// daemon's relay gives for the same result: plexus's facade reply wrapped in
/// the management envelope.
fn relay_reply(gh_route_response: &[u8]) -> Vec<u8> {
    let response: Value =
        serde_json::from_slice(gh_route_response).expect("gh.route response is JSON");
    // Plexus's reply vectors already carry the facade envelope. Preserve it
    // intact instead of translating it through the retired holder protocol.
    if response.get("repo_binding_generation").is_some() {
        return serde_json::to_vec(&json!({
            "op": "gh_shim.bot_request",
            "status": "ok",
            "data": response,
        }))
        .expect("native facade relay reply");
    }
    let result = match response["outcome"].as_str() {
        Some("result") => json!({"status": "completed", "result": response["result"]}),
        Some("applied") => {
            let mut state = serde_json::Map::new();
            for key in ["state", "state_reason"] {
                if let Some(value) = response.get(key).filter(|value| !value.is_null()) {
                    state.insert(key.to_string(), value.clone());
                }
            }
            json!({"status": "completed", "result": state})
        }
        Some("refusal") => json!({"status": "refused", "refusal_code": response["refusal_code"]}),
        other => panic!("gh.route outcome {other:?} has no relay counterpart"),
    };
    serde_json::to_vec(&json!({
        "op": "gh_shim.bot_request",
        "status": "ok",
        "data": {"repo_binding_generation": 1, "result": result},
    }))
    .expect("relay reply")
}

/// The fake AFT relay returns only the selected agent's view. The handler's
/// reply shape comes from Plexus's vectors; repository identities are local fixtures.
fn relay_bindings_reply(agent: &str) -> Vec<u8> {
    let reply = match agent {
        "alfonso-aft" => plexus_bindings_contract::agent_reply(REPOSITORY, agent, "h1", 3),
        OTHER_BOUND_AGENT => {
            plexus_bindings_contract::agent_reply(OTHER_BOUND_REPOSITORY, agent, "h2", 3)
        }
        _ => plexus_bindings_contract::vector("none").1,
    };
    serde_json::to_vec(&json!({
        "op": "gh_shim.bindings_read",
        "status": "ok",
        "data": reply["result"],
    }))
    .expect("bindings reply")
}

/// Stands in for the ticket the daemon gives an agent's command; the fake
/// holder accepts any ticket.
const TEST_GH_SHIM_TICKET: &str = "00112233445566778899aabbccddeeff";

/// What the loopback stand-in holder answers a relayed `gh.route` request
/// with: the canned reply when the request meets Plexus's published contract,
/// else a refusal carrying the contract's exact code. A refused request never
/// reaches the canned reply, so a shim that builds a refused shape fails its
/// round trip here instead of against live Plexus. Silence (`None` canned) is
/// preserved for the outcome-unknown rows: an accepted request with no canned
/// reply still gets no answer.
fn stand_in_route_reply(request: &Value, canned: Option<&[u8]>) -> Option<Vec<u8>> {
    match plexus_contract_judge::judge(request) {
        Ok(_) => canned.map(relay_reply),
        Err(code) => Some(relay_reply(
            &serde_json::to_vec(&json!({
                "outcome": "refusal",
                "refusal_code": code,
            }))
            .expect("refusal reply"),
        )),
    }
}

/// Loopback subc daemon that advertises `aft` serving the gh shim relay on its
/// management surface, opens route channel 42, records every request frame
/// body, and answers the relayed write according to its `HolderReply`.
struct CapturingHolder {
    port: u16,
    key: Vec<u8>,
    daemon_id: [u8; DAEMON_ID_LEN],
    captured: Arc<Mutex<Vec<CapturedFrame>>>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<std::thread::JoinHandle<()>>,
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn response_frame(request: &Frame, body: Vec<u8>) -> Frame {
    Frame::build_with_version(
        request.header.ver,
        FrameType::Response,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        body,
    )
    .expect("build response frame")
}

impl CapturingHolder {
    fn spawn(reply: Option<Vec<u8>>) -> Self {
        Self::spawn_with_binding_override(reply, None)
    }

    /// Like `spawn`, answering the relay's bindings read with
    /// `bindings_reply`, so a manifest with other bindings passes the shim's
    /// manifest-versus-holder check.
    fn spawn_with_bindings(reply: Option<Vec<u8>>, bindings_reply: Vec<u8>) -> Self {
        Self::spawn_with_binding_override(reply, Some(bindings_reply))
    }

    fn spawn_with_binding_override(
        reply: Option<Vec<u8>>,
        bindings_reply: Option<Vec<u8>>,
    ) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake holder");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("fake holder address").port();
        let key = vec![0x42; KEY_LEN];
        let daemon_id = [0x24; DAEMON_ID_LEN];
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server_key = key.clone();
        let server_captured = Arc::clone(&captured);
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("fake holder runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            tokio::spawn(serve_connection(
                                stream,
                                server_key.clone(),
                                daemon_id,
                                Arc::clone(&server_captured),
                                reply.clone(),
                                bindings_reply.clone(),
                            ));
                        }
                    }
                }
            });
        });

        Self {
            port,
            key,
            daemon_id,
            captured,
            shutdown_tx: Some(shutdown_tx),
            server: Some(server),
        }
    }

    fn write_connection_file(&self, path: &Path) {
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_string(),
                port: self.port,
            }],
            key: self.key.clone(),
            daemon_id: self.daemon_id,
            pid: std::process::id(),
            daemon_ver: "gh-shim-wire-goldens-holder".to_string(),
        };
        connection_file::write_atomic(path, &connection).expect("write fake holder connection");
    }

    fn captured(&self) -> Vec<CapturedFrame> {
        self.captured.lock().expect("captured frames lock").clone()
    }
}

impl Drop for CapturingHolder {
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

async fn serve_connection(
    mut stream: tokio::net::TcpStream,
    key: Vec<u8>,
    daemon_id: [u8; DAEMON_ID_LEN],
    captured: Arc<Mutex<Vec<CapturedFrame>>>,
    reply: Option<Vec<u8>>,
    bindings_reply: Option<Vec<u8>>,
) {
    if subc_transport::authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "subc-test",
        Duration::from_secs(5),
    )
    .await
    .is_err()
    {
        return;
    }
    while let Ok(Some(frame)) = subc_transport::read_frame(&mut stream).await {
        let response = match frame.header.ty {
            FrameType::Hello => Some(
                Frame::build(
                    FrameType::HelloAck,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    serde_json::to_vec(&ModuleHelloAckBody {
                        negotiated_ver: PROTOCOL_VERSION,
                        subc_ops: Vec::new(),
                        subc_capabilities: Vec::new(),
                        storage: None,
                        machine_id: None,
                    })
                    .expect("hello ack body"),
                )
                .expect("hello ack frame"),
            ),
            FrameType::Request => {
                let decoded = serde_json::from_slice::<Value>(&frame.body).ok();
                let op = decoded
                    .as_ref()
                    .and_then(|value| value.get("op").and_then(Value::as_str).map(str::to_string));
                let bot_write = frame.header.channel == ROUTE_CHANNEL
                    && op.as_deref() == Some("gh_shim.bot_request");
                let body = match decoded.as_ref() {
                    Some(value) if bot_write => {
                        serde_json::to_vec(&value["params"]["request"]).expect("governed envelope")
                    }
                    _ => frame.body.clone(),
                };
                captured
                    .lock()
                    .expect("captured frames lock")
                    .push(CapturedFrame {
                        on_route: bot_write,
                        body,
                    });
                match op.as_deref() {
                    Some("catalog.list") => Some(response_frame(
                        &frame,
                        serde_json::to_vec(&json!({
                            "op": "catalog.list",
                            "generation": 1,
                            "modules": [{
                                "module_id": "aft",
                                "module_version": "0.1.0",
                                "roles": [{
                                    "role": "management_surface",
                                    "operations": [
                                        { "name": "gh_shim.bot_request", "kind": "mutate" },
                                        { "name": "gh_shim.bindings_read", "kind": "query" }
                                    ],
                                    "config_schema": {},
                                    "observability": [],
                                    "identity_scope": ["project"]
                                }],
                                "control_ops": []
                            }],
                            "subc_ops": ["catalog.list", "route.open"]
                        }))
                        .expect("catalog body"),
                    )),
                    Some("route.open") => Some(response_frame(
                        &frame,
                        serde_json::to_vec(&json!({
                            "op": "route.open",
                            "route_channel": ROUTE_CHANNEL,
                            "route_epoch": 1
                        }))
                        .expect("route open body"),
                    )),
                    Some("route.close") => Some(response_frame(
                        &frame,
                        serde_json::to_vec(&json!({ "op": "route.close" }))
                            .expect("route close body"),
                    )),
                    Some("gh_shim.bindings_read") if frame.header.channel == ROUTE_CHANNEL => {
                        let agent = decoded
                            .as_ref()
                            .and_then(|request| request["params"]["agent_id"].as_str())
                            .expect("shim binding read names its signed target agent");
                        Some(response_frame(
                            &frame,
                            bindings_reply
                                .clone()
                                .unwrap_or_else(|| relay_bindings_reply(agent)),
                        ))
                    }
                    _ if bot_write => {
                        // The captured frame body above holds these same bytes;
                        // read the request from the decoded frame because that
                        // body was moved into the capture.
                        let request = decoded
                            .as_ref()
                            .and_then(|value| value.get("params"))
                            .and_then(|params| params.get("request"))
                            .expect("relayed bot write carries params.request");
                        stand_in_route_reply(request, reply.as_deref())
                            .map(|bytes| response_frame(&frame, bytes))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(response) = response {
            if subc_transport::write_frame(&mut stream, &response)
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Isolated shim environment
// ---------------------------------------------------------------------------

/// The v12 manifest fixture plus the three v14 speech rows (`issue create`,
/// own-issue `issue edit`, the own-comment PATCH) and the two v14 admin-tier
/// operator rows (label-only `pr edit`, `label create`), signed with the dev
/// key and published as version 14, so one manifest admits every governed
/// family exercised here (v1, v10 `--edit-last`, v12, v14).
fn v14_manifest(now: u64) -> Value {
    let mut manifest: Value =
        serde_json::from_str(include_str!("fixtures/gh_shim/v12-manifest.json"))
            .expect("parse v12 manifest fixture");
    manifest["manifest_version"] = json!(14);
    manifest["issued_at_unix_secs"] = json!(now);
    manifest["tiers"]["governed"]
        .as_array_mut()
        .expect("governed tier")
        .push(json!({"tuple": "issue create", "platform": ["macos", "linux"]}));
    manifest["canonicalization"]["issue create"] = json!({
        "argv_forms": ["fields-only"],
        "target_fields": [],
        "body_fields": ["title", "body", "labels"]
    });
    manifest["tiers"]["governed"]
        .as_array_mut()
        .expect("governed tier")
        .push(json!({"tuple": "issue edit", "platform": ["macos", "linux"]}));
    manifest["canonicalization"]["issue edit"] = json!({
        "argv_forms": ["target-and-fields"],
        "target_fields": ["number"],
        "body_fields": [
            "title",
            "body",
            "add_labels",
            "remove_labels",
            "add_assignees",
            "remove_assignees"
        ]
    });
    manifest["api_rules"]
        .as_array_mut()
        .expect("api rules")
        .push(json!({
            "method": "PATCH",
            "path_glob": "/repos/*/*/issues/comments/*",
            "tier": "governed",
            "platform": ["macos", "linux"],
            "rationale": "Own-comment edit: body-only speech; the holder verifies authorship"
        }));
    for tuple in ["pr edit", "label create"] {
        manifest["tiers"]["admin"]
            .as_array_mut()
            .expect("admin tier")
            .push(json!({"tuple": tuple, "platform": ["macos", "linux"]}));
    }
    // A second bound repository, so a command can target a bound repository
    // other than the checkout it runs in.
    manifest["bindings"][OTHER_BOUND_REPOSITORY] = json!(OTHER_BOUND_AGENT);
    manifest
}

fn write_signed_manifest(state_dir: &Path, now: u64) {
    let manifest_bytes = serde_json::to_vec(&v14_manifest(now)).expect("serialize manifest");
    let key = Ed25519KeyPair::from_seed_unchecked(&DEV_MANIFEST_SEED).expect("dev signing key");
    let envelope = json!({
        "artifact_id": "gh-routing-manifest",
        "envelope_version": 2,
        "key_id": "gh-routing-dev-test-key-v1",
        "fetched_at_unix_secs": now,
        "signature": base64::engine::general_purpose::STANDARD.encode(key.sign(&manifest_bytes).as_ref()),
        "manifest_bytes": String::from_utf8(manifest_bytes).expect("manifest is UTF-8"),
    });
    fs::create_dir_all(state_dir).expect("create shim state directory");
    fs::write(
        state_dir.join("gh-routing-manifest.json"),
        serde_json::to_vec(&envelope).expect("serialize envelope"),
    )
    .expect("write manifest envelope");
}

/// A fresh R3 rung record lets the shim skip discovery and go straight to the
/// governed route; `as_of_unix_secs` becomes the request's
/// `rung_as_of_unix_secs`.
fn write_fresh_r3_cache(state_dir: &Path, now: u64) {
    fs::write(
        state_dir.join("rung-cache.json"),
        serde_json::to_vec(&json!({
            "rung": "R3",
            "as_of_unix_secs": now,
            "last_reachable_unix_secs": now,
            "inputs": {
                "connection_file": "ready",
                "catalog_gh_route": "ready",
                "agent_binding": "ready",
                "manifest": "ready",
                "agent_credentials_present": "absent"
            },
            "manifest_version": 14
        }))
        .expect("serialize rung cache"),
    )
    .expect("write rung cache");
}

fn write_project_repo(root: &Path) -> PathBuf {
    let project = root.join("project");
    fs::create_dir_all(&project).expect("create project directory");
    for args in [
        vec!["init".to_string(), "--quiet".to_string()],
        vec![
            "remote".to_string(),
            "add".to_string(),
            "origin".to_string(),
            format!("https://github.com/{REPOSITORY}.git"),
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&project)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed: {status}");
    }
    project
}

fn write_upstream_gh(bin: &Path) {
    fs::create_dir_all(bin).expect("create upstream bin");
    let gh = bin.join("gh");
    crate::test_helpers::executable_cache::link_cached_executable(
        &gh,
        b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$GH_SHIM_TEST_RECORD\"\nexit 73\n",
    )
    .expect("cache upstream gh stand-in");
}

fn write_user_config(config_home: &Path, connection_file: &Path) {
    let dir = config_home.join("cortexkit");
    fs::create_dir_all(&dir).expect("create config dir");
    fs::write(
        dir.join("aft.jsonc"),
        serde_json::to_vec_pretty(&json!({ "subc": { "connection_file": connection_file } }))
            .expect("serialize config"),
    )
    .expect("write config");
}

/// Everything one shim run produced.
struct Run {
    pid: u32,
    rung_written_at: u64,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    /// The bytes the shim wrote on the governed route, if it got that far.
    route_request: Option<Vec<u8>>,
    /// Every request frame body in arrival order (catalog.list, route.open,
    /// the governed request, route.close), for the session record.
    frames: Vec<CapturedFrame>,
    project_root: PathBuf,
}

fn run_shim(verb: &Verb, reply: &HolderReply) -> Run {
    let reply_bytes = match reply {
        HolderReply::Respond(file) => Some(
            fs::read(goldens_dir().join("responses").join(file))
                .unwrap_or_else(|error| panic!("read response {file}: {error}")),
        ),
        HolderReply::Silent => None,
    };
    let holder = CapturingHolder::spawn(reply_bytes);
    let temp = tempfile::tempdir().expect("temp root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let state_dir = state_home.join("cortexkit/aft/gh-shim");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");
    let project = write_project_repo(temp.path());
    let connection_file = temp.path().join("subc-connection.json");
    holder.write_connection_file(&connection_file);
    write_user_config(&config_home, &connection_file);
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_signed_manifest(&state_dir, now);
    write_fresh_r3_cache(&state_dir, now);

    let inherited_path = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        std::iter::once(upstream_bin.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("build PATH");
    let child = Command::new(aft_binary())
        .arg("gh-shim")
        .args(verb.argv)
        .current_dir(&project)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("AFT_GH_SHIM_STATE_DIR", &state_dir)
        .env("AFT_STORAGE_DIR", state_home.join("aft-test-storage"))
        .env("PATH", path)
        .env("GH_SHIM_TEST_RECORD", &recorder)
        .env("AFT_GH_SHIM_TICKET", TEST_GH_SHIM_TICKET)
        // Debug-build seam: the silent-holder exchanges pin a 5000 ms
        // outcome-unknown message instead of waiting the production 30 s.
        .env("AFT_GH_SHIM_RELAY_TIMEOUT_MS", "5000")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GH_SHIM_BYPASS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gh shim");
    let pid = child.id();
    let output = child.wait_with_output().expect("wait for gh shim");
    assert!(
        !recorder.exists(),
        "{}: a governed invocation must never reach upstream gh",
        verb.name
    );

    let frames = holder.captured();
    let route_requests = frames
        .iter()
        .filter(|frame| frame.on_route)
        .map(|frame| frame.body.clone())
        .collect::<Vec<_>>();
    assert!(
        route_requests.len() <= 1,
        "{}: the shim sent {} governed requests",
        verb.name,
        route_requests.len()
    );
    Run {
        pid,
        rung_written_at: now,
        exit_code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        route_request: route_requests.into_iter().next(),
        frames,
        // Resolved while the temporary directory still exists: macOS temp
        // directories sit behind a symlink (/var -> /private/var) and the shim
        // sends the resolved path.
        project_root: fs::canonicalize(&project).unwrap_or(project),
    }
}

// ---------------------------------------------------------------------------
// Golden comparison
// ---------------------------------------------------------------------------

/// Replace the digits after `"<key>":` with a placeholder. The request carries
/// two values that legitimately change on every run: the shim's own process
/// id and the rung record's timestamp.
fn mask_number(text: &str, key: &str) -> String {
    let needle = format!("\"{key}\":");
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(&needle) {
        let (before, after) = rest.split_at(index + needle.len());
        output.push_str(before);
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        assert!(digits > 0, "{key} must be an unsigned integer in {text}");
        output.push_str(&format!("<{key}>"));
        rest = &after[digits..];
    }
    output.push_str(rest);
    output
}

fn mask_volatile(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).expect("request is UTF-8");
    mask_number(&mask_number(text, "pid"), "rung_as_of_unix_secs")
}

fn request_path(verb: &str) -> PathBuf {
    goldens_dir()
        .join("requests")
        .join(format!("{verb}.request.json"))
}

/// Check the volatile fields carry the values they claim to carry, then
/// compare everything else byte for byte against the golden.
fn assert_request_matches_golden(verb: &Verb, run: &Run) {
    let captured = run
        .route_request
        .as_ref()
        .unwrap_or_else(|| panic!("{}: no governed request reached the holder", verb.name));
    let parsed: Value = serde_json::from_slice(captured).expect("request is JSON");
    assert_eq!(
        parsed["metadata"]["pid"],
        json!(run.pid),
        "{}: metadata.pid must be the shim process id",
        verb.name
    );
    assert!(
        parsed["rung_as_of_unix_secs"]
            .as_u64()
            .is_some_and(|value| value >= run.rung_written_at),
        "{}: rung_as_of_unix_secs must come from the R3 rung record",
        verb.name
    );
    let golden = fs::read(request_path(verb.name))
        .unwrap_or_else(|error| panic!("{}: read request golden: {error}", verb.name));
    assert_eq!(
        mask_volatile(captured),
        mask_volatile(&golden),
        "{}: the shim's gh.route request bytes drifted from requests/{}.request.json",
        verb.name,
        verb.name
    );
}

fn exchange_path(name: &str) -> PathBuf {
    goldens_dir()
        .join("exchanges")
        .join(format!("{name}.exchange.json"))
}

fn exchange_record(exchange: &Exchange, run: &Run) -> Value {
    let verb = verb(exchange.verb);
    json!({
        "argv": std::iter::once("gh").chain(verb.argv.iter().copied()).collect::<Vec<_>>(),
        "request": format!("requests/{}.request.json", verb.name),
        "response": match &exchange.reply {
            HolderReply::Respond(file) => Value::String(format!("responses/{file}")),
            HolderReply::Silent => Value::Null,
        },
        "holder_behavior": match &exchange.reply {
            HolderReply::Respond(_) => "replied with the response file bytes",
            HolderReply::Silent => "read the request and never replied",
        },
        "exit_code": run.exit_code,
        "stdout": run.stdout,
        "stderr": run.stderr,
    })
}

fn write_or_check_exchange(exchange: &Exchange, run: &Run) {
    let record = exchange_record(exchange, run);
    let path = exchange_path(exchange.name);
    if regenerating() {
        fs::create_dir_all(path.parent().unwrap()).expect("create exchanges dir");
        let mut text = serde_json::to_string_pretty(&record).expect("serialize exchange");
        text.push('\n');
        fs::write(&path, text).expect("write exchange");
        return;
    }
    let golden: Value = serde_json::from_slice(
        &fs::read(&path)
            .unwrap_or_else(|error| panic!("{}: read exchange: {error}", exchange.name)),
    )
    .expect("exchange golden is JSON");
    assert_eq!(
        record, golden,
        "{}: the shim's exit code, stdout or stderr drifted from exchanges/{}.exchange.json",
        exchange.name, exchange.name
    );
}

fn run_exchange(exchange: &Exchange) {
    let verb = verb(exchange.verb);
    let run = run_shim(verb, &exchange.reply);
    if !regenerating() {
        assert_request_matches_golden(verb, &run);
    }
    write_or_check_exchange(exchange, &run);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every governed verb's request bytes, captured from a successful exchange.
#[test]
fn gh_route_request_bytes_match_the_captured_goldens_for_every_governed_verb() {
    for exchange in success_exchanges() {
        let verb = verb(exchange.verb);
        let run = run_shim(verb, &exchange.reply);
        if regenerating() {
            let captured = run
                .route_request
                .as_ref()
                .unwrap_or_else(|| panic!("{}: no governed request captured", verb.name));
            let path = request_path(verb.name);
            fs::create_dir_all(path.parent().unwrap()).expect("create requests dir");
            fs::write(&path, captured).expect("write request golden");
            if verb.name == "v1-issue-comment" {
                write_session_record(&run);
            }
        } else {
            assert_request_matches_golden(verb, &run);
        }
        write_or_check_exchange(&exchange, &run);
    }
}

#[test]
fn holder_refusal_and_failure_responses_map_to_the_recorded_exit_and_stderr() {
    for exchange in refusal_exchanges() {
        run_exchange(&exchange);
    }
}

/// The holder never answers, so each run waits out the shim's 5 s call
/// timeout; the four families run concurrently to keep the test short.
#[test]
fn a_silent_holder_after_the_request_is_outcome_unknown_exit_87() {
    let handles = outcome_unknown_exchanges()
        .into_iter()
        .map(|exchange| std::thread::spawn(move || run_exchange(&exchange)))
        .collect::<Vec<_>>();
    for handle in handles {
        if let Err(panic) = handle.join() {
            std::panic::resume_unwind(panic);
        }
    }
}

/// The frames around the governed request (catalog discovery, route open and
/// close) for one verb, with the temporary project path replaced by a
/// placeholder so the record is stable. Written only when regenerating; it is
/// documentation of the session, not a golden the tests compare.
fn write_session_record(run: &Run) {
    let project_root = run.project_root.to_string_lossy().into_owned();
    let frames = run
        .frames
        .iter()
        .map(|frame| {
            let text = String::from_utf8_lossy(&frame.body)
                .replace(&project_root, "<project_root>")
                .replace(&format!("\"pid\":{}", run.pid), "\"pid\":<pid>");
            json!({
                "on_route_channel": frame.on_route,
                "body": text,
            })
        })
        .collect::<Vec<_>>();
    let mut text = serde_json::to_string_pretty(&json!({
        "verb": "v1-issue-comment",
        "note": "request frame bodies the fake holder read, in arrival order; <project_root> and <pid> replace per-run values",
        "frames": frames,
    }))
    .expect("serialize session record");
    text.push('\n');
    fs::write(
        goldens_dir().join("requests/session-v1-issue-comment.frames.json"),
        text,
    )
    .expect("write session record");
}

// ---------------------------------------------------------------------------
// Target repository
// ---------------------------------------------------------------------------
//
// The binding and the routed repository come from the repository a command
// targets (`-R`/`--repo`, a thread URL, `GH_REPO`, then the working
// directory's origin), not from the working directory alone. Each run records
// every upstream `gh` invocation through the stand-in, so "delegated" is an
// observed call rather than an inference from the exit code.

/// Where the shim runs.
#[derive(Clone, Copy)]
enum RunFrom {
    /// A checkout whose origin is `REPOSITORY` (bound).
    BoundCheckout,
    /// A directory in no git repository at all.
    ScratchDirectory,
    /// A checkout whose origin is `UNBOUND_REPOSITORY`.
    UnboundCheckout,
}

#[derive(Clone, Copy, PartialEq)]
enum Governance {
    /// The fake holder answers every governed request with success.
    Available,
    /// The connection file names a holder that is gone, and no rung record is
    /// cached, so discovery finds the daemon unreachable.
    Unavailable,
}

struct TargetRun {
    exit_code: Option<i32>,
    stderr: String,
    /// The governed request the holder received, if any.
    route_request: Option<Value>,
    /// One line per upstream `gh` invocation, its argv joined by spaces.
    upstream_calls: Vec<String>,
    bypass_audit: Vec<Value>,
}

fn write_checkout(root: &Path, repository: &str) -> PathBuf {
    let checkout = root.join(repository.replace('/', "-"));
    fs::create_dir_all(&checkout).expect("create checkout");
    for args in [
        vec!["init".to_string(), "--quiet".to_string()],
        vec![
            "remote".to_string(),
            "add".to_string(),
            "origin".to_string(),
            format!("https://github.com/{repository}.git"),
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&checkout)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed: {status}");
    }
    checkout
}

fn run_targeting(
    argv: &[&str],
    from: RunFrom,
    env: &[(&str, &str)],
    governance: Governance,
) -> TargetRun {
    let reply = fs::read(goldens_dir().join("responses/success-result.response.json"))
        .expect("read success response");
    let mut holder = Some(CapturingHolder::spawn(Some(reply)));
    let temp = tempfile::tempdir().expect("temp root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let state_dir = state_home.join("cortexkit/aft/gh-shim");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");
    let cwd = match from {
        RunFrom::BoundCheckout => write_checkout(temp.path(), REPOSITORY),
        RunFrom::UnboundCheckout => write_checkout(temp.path(), UNBOUND_REPOSITORY),
        RunFrom::ScratchDirectory => {
            let directory = temp.path().join("scratch");
            fs::create_dir_all(&directory).expect("create scratch directory");
            directory
        }
    };
    let connection_file = temp.path().join("subc-connection.json");
    holder
        .as_ref()
        .expect("holder")
        .write_connection_file(&connection_file);
    write_user_config(&config_home, &connection_file);
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_signed_manifest(&state_dir, now);
    match governance {
        Governance::Available => write_fresh_r3_cache(&state_dir, now),
        Governance::Unavailable => drop(holder.take()),
    }

    let inherited_path = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        std::iter::once(upstream_bin.clone()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("build PATH");
    let mut command = Command::new(aft_binary());
    command
        .arg("gh-shim")
        .args(argv)
        .current_dir(&cwd)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("AFT_GH_SHIM_STATE_DIR", &state_dir)
        .env("AFT_STORAGE_DIR", state_home.join("aft-test-storage"))
        .env("PATH", path)
        .env("GH_SHIM_TEST_RECORD", &recorder)
        // Keep git from finding a repository above the temporary directory.
        .env("GIT_CEILING_DIRECTORIES", temp.path())
        .env("AFT_GH_SHIM_TICKET", TEST_GH_SHIM_TICKET)
        .env_remove("GIT_DIR")
        .env_remove("GH_REPO")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GH_SHIM_BYPASS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("run gh shim");

    let route_requests = holder
        .as_ref()
        .map(|holder| holder.captured())
        .unwrap_or_default()
        .into_iter()
        .filter(|frame| frame.on_route)
        .map(|frame| serde_json::from_slice::<Value>(&frame.body).expect("request is JSON"))
        .collect::<Vec<_>>();
    assert!(route_requests.len() <= 1, "more than one governed request");
    let upstream_calls = fs::read_to_string(&recorder)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let bypass_audit = fs::read_to_string(state_dir.join("operator-bypass.jsonl"))
        .map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).expect("audit line is JSON"))
                .collect()
        })
        .unwrap_or_default();
    TargetRun {
        exit_code: output.status.code(),
        stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        route_request: route_requests.into_iter().next(),
        upstream_calls,
        bypass_audit,
    }
}

/// The run spoke through the governed route as `agent` in `repository` and
/// never reached upstream `gh`.
fn assert_routed_as(run: &TargetRun, repository: &str, agent: &str) {
    assert!(
        run.upstream_calls.is_empty(),
        "a governed call reached upstream gh as the operator: {:?}; stderr: {}",
        run.upstream_calls,
        run.stderr
    );
    let request = run.route_request.as_ref().unwrap_or_else(|| {
        panic!(
            "no governed request reached the holder; stderr: {}",
            run.stderr
        )
    });
    assert_eq!(
        request["repository"],
        json!(repository),
        "routed repository"
    );
    assert_eq!(
        request["metadata"]["agent_id"],
        json!(agent),
        "routed agent"
    );
    assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
}

/// The run passed straight through to upstream `gh` with its argv untouched.
fn assert_delegated(run: &TargetRun, argv: &[&str]) {
    assert!(
        run.route_request.is_none(),
        "an unbound target was routed: {:?}",
        run.route_request
    );
    assert_eq!(
        run.upstream_calls,
        vec![argv.join(" ")],
        "upstream gh calls"
    );
    assert_eq!(run.exit_code, Some(73), "the stand-in gh's exit status");
}

/// The run refused with `code` and neither routed nor reached upstream `gh`.
fn assert_refused(run: &TargetRun, code: &str) {
    assert!(
        run.upstream_calls.is_empty(),
        "a refused call reached upstream gh: {:?}",
        run.upstream_calls
    );
    assert!(run.route_request.is_none(), "a refused call was routed");
    assert!(
        run.stderr.contains(code),
        "expected refusal {code}; stderr: {}",
        run.stderr
    );
    assert_ne!(run.exit_code, Some(0));
}

#[test]
fn speech_with_repo_flag_naming_a_bound_repository_routes_as_its_bot_from_outside_any_checkout() {
    let run = run_targeting(
        &[
            "issue",
            "comment",
            "513",
            "-R",
            OTHER_BOUND_REPOSITORY,
            "--body",
            "hello",
        ],
        RunFrom::ScratchDirectory,
        &[],
        Governance::Available,
    );
    assert_routed_as(&run, OTHER_BOUND_REPOSITORY, OTHER_BOUND_AGENT);
}

#[test]
fn speech_with_repo_equals_naming_a_bound_repository_routes_as_its_bot_from_outside_any_checkout() {
    for repo_flag in [
        format!("--repo={OTHER_BOUND_REPOSITORY}"),
        format!("-R={OTHER_BOUND_REPOSITORY}"),
    ] {
        let run = run_targeting(
            &["issue", "comment", "513", &repo_flag, "--body", "hello"],
            RunFrom::ScratchDirectory,
            &[],
            Governance::Available,
        );
        assert_routed_as(&run, OTHER_BOUND_REPOSITORY, OTHER_BOUND_AGENT);
    }
}

#[test]
fn speech_with_a_thread_url_naming_a_bound_repository_routes_as_its_bot_from_outside_any_checkout()
{
    let url = format!("https://github.com/{OTHER_BOUND_REPOSITORY}/issues/513");
    let run = run_targeting(
        &["issue", "comment", &url, "--body", "hello"],
        RunFrom::ScratchDirectory,
        &[],
        Governance::Available,
    );
    assert_routed_as(&run, OTHER_BOUND_REPOSITORY, OTHER_BOUND_AGENT);
    // The route holder routes on the bare thread number: the shim sends the
    // URL's digits, not the URL.
    assert_eq!(
        run.route_request.as_ref().unwrap()["target"]["number"],
        json!("513")
    );
}

#[test]
fn speech_with_gh_repo_naming_a_bound_repository_routes_as_its_bot_from_an_unbound_checkout() {
    let run = run_targeting(
        &["issue", "comment", "513", "--body", "hello"],
        RunFrom::UnboundCheckout,
        &[("GH_REPO", OTHER_BOUND_REPOSITORY)],
        Governance::Available,
    );
    assert_routed_as(&run, OTHER_BOUND_REPOSITORY, OTHER_BOUND_AGENT);
}

#[test]
fn speech_aimed_at_a_bound_repository_refuses_when_governance_is_unavailable_and_never_delegates() {
    let run = run_targeting(
        &[
            "issue",
            "comment",
            "513",
            "-R",
            OTHER_BOUND_REPOSITORY,
            "--body",
            "hello",
        ],
        RunFrom::ScratchDirectory,
        &[],
        Governance::Unavailable,
    );
    assert_refused(&run, "gh_shim_governance_unavailable");
}

/// No bot is bound to the named repository, so the comment cannot be bot
/// speech. It is refused rather than run under the operator's own gh login,
/// and runs as the operator, with an audit line, only under the bypass.
fn assert_unbound_speech_needs_the_bypass(argv: &[&str], from: RunFrom) {
    let refused = run_targeting(argv, from, &[], Governance::Available);
    assert_refused(&refused, "gh_shim_unbound_target");
    assert!(refused.bypass_audit.is_empty());

    let bypassed = run_targeting(
        argv,
        from,
        &[("GH_SHIM_BYPASS", "operator")],
        Governance::Available,
    );
    assert_delegated(&bypassed, argv);
    assert_eq!(bypassed.bypass_audit.len(), 1, "one audit line");
    assert_eq!(
        bypassed.bypass_audit[0]["repository"],
        json!(UNBOUND_REPOSITORY)
    );
    assert_eq!(bypassed.bypass_audit[0]["tuple"], json!("issue comment"));
}

#[test]
fn speech_with_repo_flag_naming_an_unbound_repository_from_an_unbound_directory_needs_the_bypass() {
    let argv = [
        "issue",
        "comment",
        "513",
        "-R",
        UNBOUND_REPOSITORY,
        "--body",
        "hello",
    ];
    assert_unbound_speech_needs_the_bypass(&argv, RunFrom::ScratchDirectory);
}

#[test]
fn speech_from_one_bound_checkout_aimed_at_another_bound_repository_uses_the_target_binding() {
    let run = run_targeting(
        &[
            "issue",
            "comment",
            "513",
            "-R",
            OTHER_BOUND_REPOSITORY,
            "--body",
            "hello",
        ],
        RunFrom::BoundCheckout,
        &[],
        Governance::Available,
    );
    assert_routed_as(&run, OTHER_BOUND_REPOSITORY, OTHER_BOUND_AGENT);
}

#[test]
fn speech_from_a_bound_checkout_aimed_at_an_unbound_repository_needs_the_bypass() {
    let argv = [
        "issue",
        "comment",
        "513",
        "-R",
        UNBOUND_REPOSITORY,
        "--body",
        "hello",
    ];
    assert_unbound_speech_needs_the_bypass(&argv, RunFrom::BoundCheckout);
}

#[test]
fn speech_whose_url_and_repo_flag_disagree_refuses_instead_of_guessing() {
    let url = format!("https://github.com/{OTHER_BOUND_REPOSITORY}/issues/513");
    let run = run_targeting(
        &[
            "issue", "comment", &url, "-R", REPOSITORY, "--body", "hello",
        ],
        RunFrom::ScratchDirectory,
        &[],
        Governance::Available,
    );
    assert_refused(&run, "gh_shim_unclassified");
}

/// A bound checkout keeps administration behind the audited operator bypass
/// even when the command targets an unbound repository.
#[test]
fn admin_from_a_bound_checkout_aimed_at_an_unbound_repository_still_needs_the_bypass() {
    let argv = ["pr", "merge", "7", "-R", UNBOUND_REPOSITORY];
    let refused = run_targeting(&argv, RunFrom::BoundCheckout, &[], Governance::Available);
    assert_refused(&refused, "gh_shim_admin_tier");
    assert!(refused.bypass_audit.is_empty());

    let bypassed = run_targeting(
        &argv,
        RunFrom::BoundCheckout,
        &[("GH_SHIM_BYPASS", "operator")],
        Governance::Available,
    );
    assert_delegated(&bypassed, &argv);
    assert_eq!(bypassed.bypass_audit.len(), 1, "one audit line");
    assert_eq!(
        bypassed.bypass_audit[0]["repository"],
        json!(UNBOUND_REPOSITORY)
    );
    assert_eq!(bypassed.bypass_audit[0]["tuple"], json!("pr merge"));
}

// ---------------------------------------------------------------------------
// Manifest v22 rows: the routed request each bot-speech row sends
// ---------------------------------------------------------------------------

/// Environment variables that make an AFT worker shell route governed writes
/// through the live daemon's worker broker. The v22 runs remove them so the
/// governed route can only reach the loopback stand-in holder.
const WORKER_BROKER_ENV: &[&str] = &[
    "AFT_GH_SHIM_WORKER",
    "AFT_GH_READ_SOCKET",
    "AFT_GH_READ_CAPABILITY_FILE",
    "AFT_GH_SHIMS_DIR",
];

const V22_RESOLVE: &str = r#"query=mutation { resolveReviewThread(input: {threadId: "PRRT_v22probe"}) { thread { isResolved } } }"#;
const V22_UNRESOLVE: &str = r#"query=mutation($id: ID!) { unresolveReviewThread(input: {threadId: $id}) { thread { isResolved } } }"#;

/// What a v22 row must do.
enum V22Expect {
    /// Reach the holder with exactly these routed fields (besides the fixed
    /// envelope fields checked for every request), then exit 0.
    Routed(Value),
    /// Refuse with this code before anything reaches the holder.
    Refused(&'static str),
}

struct V22Row {
    name: &'static str,
    argv: Vec<&'static str>,
    expect: V22Expect,
}

fn routed(action: &str, target: Value, body: Value, own: bool) -> V22Expect {
    let mut fields = json!({
        "action": action,
        "target": target,
        "body": body,
        "repository": REPOSITORY,
    });
    if own {
        fields["author_scope"] = json!("own");
    }
    V22Expect::Routed(fields)
}

/// The GraphQL documents that name a review-thread mutation but are not the
/// one admitted shape.
const V22_LOOKALIKES: &[(&str, &str)] = &[
    (
        "refuse-graphql-second-mutation",
        r#"query=mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } addComment(input: {subjectId: "X", body: "y"}) { clientMutationId } }"#,
    ),
    (
        "refuse-graphql-input-extra-field",
        r#"query=mutation { resolveReviewThread(input: {threadId: "PRRT_a", clientMutationId: "c"}) { thread { id } } }"#,
    ),
    (
        "refuse-graphql-query-fragment",
        r#"query=query { ...F } fragment F on Mutation { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "refuse-graphql-query-alias",
        r#"query=query { x: resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "refuse-graphql-mutation-alias",
        r#"query=mutation { x: resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
    (
        "refuse-graphql-two-operations",
        r#"query=mutation A { resolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } } mutation B { unresolveReviewThread(input: {threadId: "PRRT_a"}) { thread { id } } }"#,
    ),
];

fn v22_rows() -> Vec<V22Row> {
    let reply_tuple = "api:POST:/repos/*/*/pulls/*/comments/*/replies";
    let mut rows = vec![
        V22Row {
            name: "01-pr-edit-title",
            argv: vec!["pr", "edit", "1", "--title", "v22 round-trip probe"],
            expect: routed(
                "pr edit",
                json!({"number": "1"}),
                json!({"title": "v22 round-trip probe"}),
                true,
            ),
        },
        V22Row {
            name: "02-pr-edit-title-and-body",
            argv: vec!["pr", "edit", "1", "-t", "probe", "--body", "v22 body"],
            expect: routed(
                "pr edit",
                json!({"number": "1"}),
                json!({"title": "probe", "body": "v22 body"}),
                true,
            ),
        },
        V22Row {
            name: "03-review-reply",
            argv: vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/pulls/1/comments/1/replies",
                "-f",
                "body=v22 round-trip probe",
            ],
            expect: routed(
                reply_tuple,
                json!({"number": "1", "comment_id": "1"}),
                json!({"body": "v22 round-trip probe"}),
                false,
            ),
        },
        V22Row {
            name: "04-review-reply-implicit-post",
            argv: vec![
                "api",
                "repos/cortexkit/aft/pulls/1/comments/1/replies",
                "-f",
                "body=v22 round-trip probe",
            ],
            expect: routed(
                reply_tuple,
                json!({"number": "1", "comment_id": "1"}),
                json!({"body": "v22 round-trip probe"}),
                false,
            ),
        },
        V22Row {
            name: "05-pull-patch",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-f",
                "title=v22 round-trip probe",
            ],
            expect: routed(
                "pr edit",
                json!({"number": "1"}),
                json!({"title": "v22 round-trip probe"}),
                true,
            ),
        },
        V22Row {
            name: "06-resolve-thread",
            argv: vec!["api", "graphql", "-f", V22_RESOLVE],
            expect: routed(
                "graphql resolvereviewthread",
                json!({"thread_id": "PRRT_v22probe"}),
                json!({}),
                false,
            ),
        },
        V22Row {
            name: "07-unresolve-thread",
            argv: vec![
                "api",
                "graphql",
                "-f",
                V22_UNRESOLVE,
                "-f",
                "id=PRRT_v22probe",
            ],
            expect: routed(
                "graphql unresolvereviewthread",
                json!({"thread_id": "PRRT_v22probe"}),
                json!({}),
                false,
            ),
        },
        V22Row {
            name: "08-stack-create",
            argv: vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks",
                "-F",
                "pull_requests[]=1",
                "-F",
                "pull_requests[]=2",
            ],
            expect: routed(
                "api:POST:/repos/*/*/stacks",
                json!({}),
                json!({"pull_requests": [1, 2]}),
                false,
            ),
        },
        V22Row {
            name: "09-stack-add",
            argv: vec![
                "api",
                "-X",
                "POST",
                "repos/cortexkit/aft/stacks/1/add",
                "-F",
                "pull_requests[]=3",
            ],
            expect: routed(
                "api:POST:/repos/*/*/stacks/*/add",
                json!({"stack_number": "1"}),
                json!({"pull_requests": [3]}),
                false,
            ),
        },
        V22Row {
            name: "10-stack-unstack",
            argv: vec!["api", "-X", "POST", "repos/cortexkit/aft/stacks/1/unstack"],
            expect: routed(
                "api:POST:/repos/*/*/stacks/*/unstack",
                json!({"stack_number": "1"}),
                json!({}),
                false,
            ),
        },
        V22Row {
            name: "refuse-pull-patch-base",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-f",
                "title=x",
                "-f",
                "base=x",
            ],
            expect: V22Expect::Refused("gh_shim_unclassified"),
        },
        V22Row {
            name: "refuse-pr-edit-base",
            argv: vec!["pr", "edit", "1", "--title", "x", "--base", "main"],
            expect: V22Expect::Refused("gh_shim_unsupported_flag"),
        },
        V22Row {
            name: "refuse-pr-edit-label",
            argv: vec!["pr", "edit", "1", "--add-label", "probe"],
            expect: V22Expect::Refused("gh_shim_unsupported_flag"),
        },
    ];
    for (name, query) in V22_LOOKALIKES {
        rows.push(V22Row {
            name,
            argv: vec!["api", "graphql", "-f", query],
            // Every GraphQL mutation other than the admitted shape is an
            // unbound write (it names no /repos/ endpoint), refused before
            // any governance contact, exactly as before v22.
            expect: V22Expect::Refused("gh_shim_unbound_target"),
        });
    }
    rows
}

/// The tracked v20 manifest fixture with the v22 additions applied the way
/// `scripts/prepare-gh-shim-v22.py` applies them, bound only to this test's
/// repository.
fn synthetic_v22_manifest(now: u64) -> Vec<u8> {
    let mut manifest: Value =
        serde_json::from_str(include_str!("fixtures/gh_shim/v20-manifest.json"))
            .expect("parse v20 manifest fixture");
    let additions: Value =
        serde_json::from_str(include_str!("fixtures/gh_shim/v22-additions.json"))
            .expect("parse v22 additions");
    for removed in additions["removed_from_admin"].as_array().unwrap() {
        manifest["tiers"]["admin"]
            .as_array_mut()
            .unwrap()
            .retain(|row| &row["tuple"] != removed);
    }
    for (tier, rows) in additions["tiers"].as_object().unwrap() {
        manifest["tiers"][tier]
            .as_array_mut()
            .unwrap()
            .extend(rows.as_array().unwrap().iter().cloned());
    }
    manifest["api_rules"]
        .as_array_mut()
        .unwrap()
        .extend(additions["api_rules"].as_array().unwrap().iter().cloned());
    for (tuple, canonical) in additions["canonicalization"].as_object().unwrap() {
        manifest["canonicalization"][tuple] = canonical.clone();
    }
    manifest["manifest_version"] = json!(22);
    manifest["issued_at_unix_secs"] = json!(now);
    manifest["bindings"] = json!({ REPOSITORY: "alfonso-aft" });
    serde_json::to_vec(&manifest).expect("serialize synthetic v22 manifest")
}

fn write_signed_manifest_bytes(state_dir: &Path, manifest_bytes: &[u8], now: u64) {
    let key = Ed25519KeyPair::from_seed_unchecked(&DEV_MANIFEST_SEED).expect("dev signing key");
    let envelope = json!({
        "artifact_id": "gh-routing-manifest",
        "envelope_version": 2,
        "key_id": "gh-routing-dev-test-key-v1",
        "fetched_at_unix_secs": now,
        "signature": base64::engine::general_purpose::STANDARD.encode(key.sign(manifest_bytes).as_ref()),
        "manifest_bytes": std::str::from_utf8(manifest_bytes).expect("manifest is UTF-8"),
    });
    fs::create_dir_all(state_dir).expect("create shim state directory");
    fs::write(
        state_dir.join("gh-routing-manifest.json"),
        serde_json::to_vec(&envelope).expect("serialize envelope"),
    )
    .expect("write manifest envelope");
}

/// Everything one v22 row produced.
struct V22Run {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    upstream_invocations: Option<String>,
    routed_request: Option<Value>,
}

/// Run one row through the real shim with the given signed manifest bytes,
/// governance reachable through the loopback stand-in holder, and the
/// recording stand-in for upstream `gh` first on PATH.
fn run_v22_row(row: &V22Row, manifest_bytes: &[u8], agent_id: &str) -> V22Run {
    let reply = fs::read(goldens_dir().join("responses/success-result.response.json"))
        .expect("read success response");
    run_v22_row_with_reply(row, manifest_bytes, agent_id, reply)
}

fn run_v22_row_with_reply(
    row: &V22Row,
    manifest_bytes: &[u8],
    agent_id: &str,
    reply: Vec<u8>,
) -> V22Run {
    run_v22_row_with_input(row, manifest_bytes, agent_id, reply, "")
}

fn run_v22_row_with_input(
    row: &V22Row,
    manifest_bytes: &[u8],
    agent_id: &str,
    reply: Vec<u8>,
    input: &str,
) -> V22Run {
    let bindings = serde_json::to_vec(&json!({
        "op": "gh_shim.bindings_read",
        "status": "ok",
        "data": plexus_bindings_contract::agent_reply(REPOSITORY, agent_id, "h1", 3)["result"],
    }))
    .expect("bindings reply");
    let holder = CapturingHolder::spawn_with_bindings(Some(reply), bindings);
    let temp = tempfile::tempdir().expect("temp root");
    let config_home = temp.path().join("config");
    let state_home = temp.path().join("state");
    let state_dir = state_home.join("cortexkit/aft/gh-shim");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");
    let project = write_project_repo(temp.path());
    let connection_file = temp.path().join("subc-connection.json");
    holder.write_connection_file(&connection_file);
    write_user_config(&config_home, &connection_file);
    let upstream_bin = temp.path().join("upstream-bin");
    let recorder = temp.path().join("upstream-invocations.txt");
    write_upstream_gh(&upstream_bin);
    let now = unix_seconds();
    write_signed_manifest_bytes(&state_dir, manifest_bytes, now);

    let inherited_path = std::env::var_os("PATH").expect("test PATH");
    let path = std::env::join_paths(
        std::iter::once(upstream_bin).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("build PATH");
    let mut command = Command::new(aft_binary());
    command
        .arg("gh-shim")
        .args(&row.argv)
        .current_dir(&project)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("AFT_GH_SHIM_STATE_DIR", &state_dir)
        .env("AFT_STORAGE_DIR", state_home.join("aft-test-storage"))
        .env("PATH", path)
        .env("GH_SHIM_TEST_RECORD", &recorder)
        .env("AFT_GH_SHIM_TICKET", TEST_GH_SHIM_TICKET)
        .env("AFT_GH_SHIM_RELAY_TIMEOUT_MS", "5000")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GH_SHIM_BYPASS")
        .env_remove("GH_REPO")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in WORKER_BROKER_ENV {
        command.env_remove(name);
    }
    let mut child = command.spawn().expect("run gh shim");
    child
        .stdin
        .take()
        .expect("shim stdin")
        .write_all(input.as_bytes())
        .expect("write shim stdin");
    let output = child.wait_with_output().expect("wait for gh shim");
    let routed = holder
        .captured()
        .into_iter()
        .filter(|frame| frame.on_route)
        .map(|frame| serde_json::from_slice::<Value>(&frame.body).expect("routed request is JSON"))
        .collect::<Vec<_>>();
    assert!(
        routed.len() <= 1,
        "{}: the shim sent {} governed requests",
        row.name,
        routed.len()
    );
    V22Run {
        exit_code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        upstream_invocations: fs::read_to_string(&recorder).ok(),
        routed_request: routed.into_iter().next(),
    }
}

fn check_v22_row(row: &V22Row, run: &V22Run, agent_id: &str) {
    assert_eq!(
        run.upstream_invocations, None,
        "{}: a v22 speech row must never reach upstream gh",
        row.name
    );
    match &row.expect {
        V22Expect::Routed(fields) => {
            let request = run.routed_request.as_ref().unwrap_or_else(|| {
                panic!("{}: nothing reached the holder: {}", row.name, run.stderr)
            });
            assert_eq!(run.exit_code, Some(0), "{}: {}", row.name, run.stderr);
            assert_eq!(request["operation"], json!("gh.route"), "{}", row.name);
            assert_eq!(request["gh_route_schema"], json!(1), "{}", row.name);
            assert_eq!(request["manifest_version"], json!(22), "{}", row.name);
            assert_eq!(
                request["metadata"]["agent_id"],
                json!(agent_id),
                "{}",
                row.name
            );
            assert!(request["rung_as_of_unix_secs"].is_u64(), "{}", row.name);
            let mut expected_keys = vec![
                "operation",
                "gh_route_schema",
                "manifest_version",
                "metadata",
                "rung_as_of_unix_secs",
            ];
            let fields = fields.as_object().unwrap();
            expected_keys.extend(fields.keys().map(String::as_str));
            expected_keys.sort_unstable();
            let mut keys = request
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            keys.sort_unstable();
            assert_eq!(keys, expected_keys, "{}: routed request fields", row.name);
            for (key, value) in fields {
                assert_eq!(&request[key], value, "{}: routed {key}", row.name);
            }
        }
        V22Expect::Refused(code) => {
            assert!(
                run.routed_request.is_none(),
                "{}: a refused row reached the holder",
                row.name
            );
            assert_eq!(run.exit_code, Some(86), "{}", row.name);
            assert!(
                run.stderr.starts_with(&format!("gh-shim: {code}: ")),
                "{}: {}",
                row.name,
                run.stderr
            );
        }
    }
}

/// The stand-in holder's reply decision, the seam the contract pins: an
/// accepted `gh.route` request gets its canned reply (or silence when none
/// is canned, preserving the outcome-unknown rows), while a request Plexus's
/// contract refuses gets a refusal carrying the contract's exact code —
/// never the canned reply.
#[test]
fn stand_in_holder_answers_routed_requests_from_the_plexus_contract() {
    let canned = serde_json::to_vec(&json!({
        "outcome": "result",
        "gh_route_schema": 1,
        "result": {"url": "https://github.com/cortexkit/aft/issues/42"},
        "field_order": ["url"],
    }))
    .expect("canned reply");
    let vectors = plexus_contract_judge::load_vectors();
    for vector in &vectors {
        let expected = match &vector.expect {
            plexus_contract_judge::ContractExpect::Accepted => Some(relay_reply(&canned)),
            plexus_contract_judge::ContractExpect::Refused(code) => Some(relay_reply(
                &serde_json::to_vec(&json!({"outcome": "refusal", "refusal_code": code})).unwrap(),
            )),
        };
        assert_eq!(
            stand_in_route_reply(&vector.request, Some(&canned)),
            expected,
            "{}",
            vector.name,
        );
        let expected_without_canned = match vector.expect {
            plexus_contract_judge::ContractExpect::Accepted => None,
            plexus_contract_judge::ContractExpect::Refused(_) => expected,
        };
        assert_eq!(
            stand_in_route_reply(&vector.request, None),
            expected_without_canned,
            "{}: no canned reply",
            vector.name,
        );
    }
}

/// Construct argv for each reply vector's request, failing on a new family
/// rather than silently leaving a native reply without a real-shim exchange.
fn reply_contract_row(vector: &Value) -> V22Row {
    let request = &vector["request"];
    let text = |value: &Value| value.as_str().expect("request string").to_string();
    let action = request["action"]
        .as_str()
        .or(request["verb"].as_str())
        .unwrap();
    let argv: Vec<String> = match action {
        "pr ready" => {
            let mut argv = vec!["pr".into(), "ready".into(), text(&request["number"])];
            if request["undo"] == true {
                argv.push("--undo".into());
            }
            argv
        }
        "pr edit" => vec![
            "pr".into(),
            "edit".into(),
            text(&request["target"]["number"]),
            "--title".into(),
            text(&request["body"]["title"]),
        ],
        "graphql resolvereviewthread" | "graphql unresolvereviewthread" => {
            let mutation = if action == "graphql resolvereviewthread" {
                "resolveReviewThread"
            } else {
                "unresolveReviewThread"
            };
            vec!["api".into(), "graphql".into(), "-f".into(), format!(
                "query=mutation {{ {mutation}(input: {{threadId: \"{}\"}}) {{ thread {{ id }} }} }}",
                text(&request["target"]["thread_id"]),
            )]
        }
        "api:POST:/repos/*/*/stacks"
        | "api:POST:/repos/*/*/stacks/*/add"
        | "api:POST:/repos/*/*/stacks/*/unstack" => {
            let mut endpoint = format!("repos/{REPOSITORY}/stacks");
            if action.ends_with("/add") || action.ends_with("/unstack") {
                endpoint += &format!(
                    "/{}/{}",
                    text(&request["target"]["stack_number"]),
                    action.rsplit('/').next().unwrap()
                );
            }
            let mut argv = vec!["api".into(), "-X".into(), "POST".into(), endpoint];
            if let Some(numbers) = request["body"]["pull_requests"].as_array() {
                for number in numbers {
                    argv.extend(["-F".into(), format!("pull_requests[]={number}")]);
                }
            }
            argv
        }
        other => panic!("{}: untested reply request {other}", vector["name"]),
    };
    V22Row {
        name: leak(text(&vector["name"])),
        argv: argv.into_iter().map(leak).collect(),
        expect: V22Expect::Routed(request.clone()),
    }
}

#[test]
fn every_plexus_reply_vector_round_trips_through_the_real_shim() {
    let vectors = plexus_contract_judge::load_reply_vectors();
    assert_eq!(vectors.len(), 17);
    let manifest = synthetic_v22_manifest(unix_seconds());
    for vector in vectors {
        let row = reply_contract_row(&vector);
        let native = serde_json::to_vec(&vector["reply"]).unwrap();
        let wrapped: Value = serde_json::from_slice(
            &stand_in_route_reply(&vector["request"], Some(&native)).unwrap(),
        )
        .unwrap();
        assert_eq!(wrapped["data"], vector["reply"], "{}", row.name);
        let run = run_v22_row_with_reply(&row, &manifest, "alfonso-aft", native);
        assert!(
            run.upstream_invocations.is_none(),
            "{} delegated to gh",
            row.name
        );
        let mut routed = run.routed_request.expect("one request, with no retries");
        let mut expected = vector["request"].clone();
        expected["repository"] = json!(REPOSITORY);
        for request in [&mut routed, &mut expected] {
            request
                .as_object_mut()
                .unwrap()
                .remove("rung_as_of_unix_secs");
            request.as_object_mut().unwrap().remove("metadata");
        }
        assert_eq!(routed, expected, "{}", row.name);
        let result = &vector["reply"]["result"];
        if result["status"] == "completed" {
            assert_eq!(run.exit_code, Some(0), "{}: {}", row.name, run.stderr);
            assert_eq!(row.name, "resolve-thread-own", "unhandled completed vector");
            assert_eq!(run.stdout, "id: \"PRRT_kwDOAbc123\"\nresolved: true\n");
        } else {
            assert_eq!(run.exit_code, Some(86), "{}: {}", row.name, run.stderr);
            assert!(run.stdout.is_empty(), "{}", row.name);
            assert!(
                run.stderr
                    .contains(result["refusal_code"].as_str().unwrap()),
                "{}",
                row.name
            );
            assert!(!run.stderr.contains("outcome unknown"), "{}", row.name);
            if row.name == "resolve-thread-not-own" {
                assert_eq!(run.stderr, "gh-shim: gh_shim_seam_refusal: plexus refused the bot write with a code this shim does not know: review_thread_not_own; it was not retried\n");
            }
        }
    }
}

#[test]
fn v22_speech_rows_route_exactly_the_declared_requests_and_lookalikes_never_route() {
    let manifest = synthetic_v22_manifest(unix_seconds());
    for row in v22_rows() {
        let run = run_v22_row(&row, &manifest, "alfonso-aft");
        check_v22_row(&row, &run, "alfonso-aft");
    }
}

#[test]
fn api_typed_patch_body_file_reaches_the_real_relay_as_contents() {
    let temp = tempfile::tempdir().expect("body fixture");
    let path = temp.path().join("ship-note.md");
    let contents = "Ship note with **formatting**.\nSecond line.\n";
    fs::write(&path, contents).expect("write body fixture");
    let row = V22Row {
        name: "api-typed-patch-body-file",
        argv: vec![
            "api",
            "-X",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/1",
            "-F",
            leak(format!("body=@{}", path.display())),
        ],
        expect: routed(
            "api:PATCH:/repos/*/*/issues/comments/*",
            json!({"comment_id": "1"}),
            json!({"body": contents}),
            true,
        ),
    };
    let run = run_v22_row(&row, &synthetic_v22_manifest(unix_seconds()), "alfonso-aft");
    check_v22_row(&row, &run, "alfonso-aft");
}

#[test]
fn api_comment_field_wire_goldens_preserve_file_stdin_and_raw_semantics() {
    let temp = tempfile::tempdir().expect("body fixtures");
    let path = temp.path().join("body.md");
    let contents = "true\nA Unicode ship note: 🚢\n";
    fs::write(&path, contents).expect("write body fixture");
    let document = temp.path().join("body.json");
    fs::write(
        &document,
        serde_json::to_vec(&json!({"body": contents})).unwrap(),
    )
    .expect("write JSON fixture");
    let manifest = synthetic_v22_manifest(unix_seconds());
    for (method, endpoint, action, target, own) in [
        (
            "PATCH",
            "repos/cortexkit/aft/issues/comments/1",
            "api:PATCH:/repos/*/*/issues/comments/*",
            json!({"comment_id": "1"}),
            true,
        ),
        (
            "POST",
            "repos/cortexkit/aft/pulls/1/comments/1/replies",
            "api:POST:/repos/*/*/pulls/*/comments/*/replies",
            json!({"number": "1", "comment_id": "1"}),
            false,
        ),
    ] {
        for (flag, value, input, expected) in [
            ("-F", format!("body=@{}", path.display()), "", contents),
            ("--field", format!("body=@{}", path.display()), "", contents),
            ("-F", "body=@-".into(), contents, contents),
            ("-f", "body=@literal".into(), "", "@literal"),
            ("--raw-field", "body=@literal".into(), "", "@literal"),
            ("--input", document.to_str().unwrap().into(), "", contents),
            (
                "--input",
                "-".into(),
                "{\"body\":\"stdin document\"}",
                "stdin document",
            ),
        ] {
            let row = V22Row {
                name: leak(format!("{method}-{flag}-{value}")),
                argv: vec!["api", "-X", method, endpoint, flag, leak(value)],
                expect: routed(action, target.clone(), json!({"body": expected}), own),
            };
            let reply = fs::read(goldens_dir().join("responses/success-result.response.json"))
                .expect("success response");
            let run = run_v22_row_with_input(&row, &manifest, "alfonso-aft", reply, input);
            check_v22_row(&row, &run, "alfonso-aft");
        }
        for source in [temp.path().join("missing.md"), temp.path().to_path_buf()] {
            let row = V22Row {
                name: "unreadable-typed-comment-file",
                argv: vec![
                    "api",
                    "-X",
                    method,
                    endpoint,
                    "-F",
                    leak(format!("body=@{}", source.display())),
                ],
                expect: V22Expect::Refused("gh_shim_unclassified"),
            };
            let run = run_v22_row(&row, &manifest, "alfonso-aft");
            check_v22_row(&row, &run, "alfonso-aft");
            assert!(
                run.stderr.contains("could not read body file"),
                "{}",
                run.stderr
            );
        }
        for scalar in ["true", "false", "null", "123", "-2", "+03"] {
            let row = V22Row {
                name: "non-string-typed-comment-body",
                argv: vec![
                    "api",
                    "-X",
                    method,
                    endpoint,
                    "-F",
                    leak(format!("body={scalar}")),
                ],
                expect: V22Expect::Refused("gh_shim_unclassified"),
            };
            let run = run_v22_row(&row, &manifest, "alfonso-aft");
            check_v22_row(&row, &run, "alfonso-aft");
            assert!(
                run.stderr.contains("needs body as a string"),
                "{}",
                run.stderr
            );
        }
        for flags in [
            vec!["--input", document.to_str().unwrap(), "-f", "body=query"],
            vec!["-f", "body=query", "--input", document.to_str().unwrap()],
            vec![
                "--input",
                document.to_str().unwrap(),
                "--input",
                document.to_str().unwrap(),
            ],
        ] {
            let mut argv = vec!["api", "-X", method, endpoint];
            argv.extend(flags.into_iter().map(|value| leak(value.to_string())));
            let row = V22Row {
                name: "input-query-fields-not-representable",
                argv,
                expect: V22Expect::Refused("gh_shim_unclassified"),
            };
            let run = run_v22_row(&row, &manifest, "alfonso-aft");
            check_v22_row(&row, &run, "alfonso-aft");
            assert!(run.stderr.contains("--input cannot"), "{}", run.stderr);
        }
    }
}

#[test]
fn api_other_governed_rows_keep_types_or_refuse_before_relay() {
    let temp = tempfile::tempdir().expect("API fixtures");
    let path = temp.path().join("title.md");
    fs::write(&path, "true").expect("write string-looking scalar fixture");
    let manifest = synthetic_v22_manifest(unix_seconds());
    let mut rows = vec![
        V22Row {
            name: "pull-edit-typed-file-is-still-a-string",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-F",
                leak(format!("title=@{}", path.display())),
            ],
            expect: routed(
                "pr edit",
                json!({"number": "1"}),
                json!({"title": "true"}),
                true,
            ),
        },
        V22Row {
            name: "pull-edit-typed-boolean-is-not-prose",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-F",
                "title=true",
            ],
            expect: V22Expect::Refused("gh_shim_unclassified"),
        },
        V22Row {
            name: "pull-edit-raw-placeholder-is-literal",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-f",
                "title={repo}",
            ],
            expect: routed(
                "pr edit",
                json!({"number": "1"}),
                json!({"title": "{repo}"}),
                true,
            ),
        },
        V22Row {
            name: "pull-edit-typed-placeholder-is-not-guessed",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-F",
                "title={repo}",
            ],
            expect: V22Expect::Refused("gh_shim_unclassified"),
        },
        V22Row {
            name: "pull-edit-cannot-replay-stdin-twice",
            argv: vec![
                "api",
                "-X",
                "PATCH",
                "repos/cortexkit/aft/pulls/1",
                "-F",
                "title=@-",
                "--field",
                "body=@-",
            ],
            expect: V22Expect::Refused("gh_shim_unclassified"),
        },
    ];
    for (endpoint, action, target) in [
        (
            "repos/cortexkit/aft/stacks",
            "api:POST:/repos/*/*/stacks",
            json!({}),
        ),
        (
            "repos/cortexkit/aft/stacks/1/add",
            "api:POST:/repos/*/*/stacks/*/add",
            json!({"stack_number": "1"}),
        ),
    ] {
        for (integer, expected) in [("1", 1_i64), ("+02", 2), ("9223372036854775807", i64::MAX)] {
            rows.push(V22Row {
                name: "stack-typed-decimal-integer",
                argv: vec![
                    "api",
                    endpoint,
                    "-F",
                    leak(format!("pull_requests[]={integer}")),
                ],
                expect: routed(
                    action,
                    target.clone(),
                    json!({"pull_requests": [expected]}),
                    false,
                ),
            });
        }
        for (flag, value) in [
            ("-f", "1"),
            ("-F", "true"),
            ("-F", "false"),
            ("-F", "null"),
            ("-F", "1.5"),
            ("-F", "9223372036854775808"),
        ] {
            rows.push(V22Row {
                name: "stack-non-integer-is-not-coerced",
                argv: vec![
                    "api",
                    endpoint,
                    flag,
                    leak(format!("pull_requests[]={value}")),
                ],
                expect: V22Expect::Refused("gh_shim_unclassified"),
            });
        }
    }
    for field in [
        "id=true",
        "id=false",
        "id=null",
        "id=123",
        "id=@-",
        "id={repo}",
    ] {
        rows.push(V22Row {
            name: "graphql-variable-rewrite-is-refused",
            argv: vec!["api", "graphql", "-f", V22_UNRESOLVE, "-F", field],
            expect: V22Expect::Refused("gh_shim_unbound_target"),
        });
    }
    for row in rows {
        let run = run_v22_row(&row, &manifest, "alfonso-aft");
        check_v22_row(&row, &run, "alfonso-aft");
    }
}

#[test]
fn command_body_file_wire_goldens_read_paths_and_stdin_not_api_fields() {
    let temp = tempfile::tempdir().expect("command body fixtures");
    let path = temp.path().join("command-body.md");
    let contents = "Command body from file or stdin.\n";
    fs::write(&path, contents).expect("write command body");
    let manifest = synthetic_v22_manifest(unix_seconds());
    for (head, action, target, own, extra) in [
        (
            vec!["issue", "comment", "1"],
            "issue comment",
            json!({"number": "1"}),
            false,
            json!({}),
        ),
        (
            vec!["pr", "comment", "1"],
            "pr comment",
            json!({"number": "1"}),
            false,
            json!({}),
        ),
        (
            vec!["issue", "create", "--title", "probe"],
            "issue create",
            json!({}),
            false,
            json!({"title": "probe"}),
        ),
        (
            vec!["issue", "edit", "1"],
            "issue edit",
            json!({"number": "1"}),
            true,
            json!({}),
        ),
        (
            vec![
                "pr", "create", "--title", "probe", "--base", "main", "--head", "topic",
            ],
            "pr create",
            json!({}),
            false,
            json!({"title": "probe", "base": "main", "head": "topic", "draft": false}),
        ),
        (
            vec!["pr", "edit", "1"],
            "pr edit",
            json!({"number": "1"}),
            true,
            json!({}),
        ),
        (
            vec!["pr", "review", "1", "--comment"],
            "pr review",
            json!({"number": "1"}),
            false,
            json!({"event": "COMMENT"}),
        ),
    ] {
        for (flag, source, input) in [
            ("--body-file", path.to_str().unwrap(), ""),
            ("-F", path.to_str().unwrap(), ""),
            ("--body-file", "-", contents),
            ("-F", "-", contents),
        ] {
            let mut argv = head.clone();
            argv.extend([flag, leak(source.to_string())]);
            let mut body = extra.clone();
            body["body"] = json!(contents);
            let row = V22Row {
                name: leak(format!("{action}-{flag}-{source}")),
                argv,
                expect: routed(action, target.clone(), body, own),
            };
            let reply = fs::read(goldens_dir().join("responses/success-result.response.json"))
                .expect("success response");
            let run = run_v22_row_with_input(&row, &manifest, "alfonso-aft", reply, input);
            check_v22_row(&row, &run, "alfonso-aft");
        }
        let mut argv = head.clone();
        argv.extend([
            "--body-file",
            leak(temp.path().join("missing.md").display().to_string()),
        ]);
        let row = V22Row {
            name: "missing-command-body-file",
            argv,
            expect: V22Expect::Refused("gh_shim_unclassified"),
        };
        let run = run_v22_row(&row, &manifest, "alfonso-aft");
        check_v22_row(&row, &run, "alfonso-aft");
        assert!(
            run.stderr.contains("could not read body file"),
            "{}",
            run.stderr
        );
    }
}

/// The same rows against an unsigned v22 manifest payload (the bytes that
/// will later be signed for production), signed here with the dev key byte
/// for byte, to record each routed request before the production signing.
/// Run with
/// `AFT_GH_SHIM_V22_RIG_PAYLOAD=<unsigned payload>` and
/// `AFT_GH_SHIM_V22_RIG_EVIDENCE=<directory>`; each row's argv, exit code,
/// output and routed request are written to the evidence directory.
#[test]
#[ignore = "manual evidence run: needs an unsigned v22 payload path"]
fn v22_payload_draft_round_trip_records_each_routed_request() {
    let payload_path = std::env::var_os("AFT_GH_SHIM_V22_RIG_PAYLOAD")
        .map(PathBuf::from)
        .expect("AFT_GH_SHIM_V22_RIG_PAYLOAD");
    let evidence = std::env::var_os("AFT_GH_SHIM_V22_RIG_EVIDENCE")
        .map(PathBuf::from)
        .expect("AFT_GH_SHIM_V22_RIG_EVIDENCE");
    fs::create_dir_all(&evidence).expect("create evidence directory");
    let payload = fs::read(&payload_path).expect("read payload");
    let digest = |bytes: &[u8]| {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(bytes))
    };
    let before = digest(&payload);
    let parsed: Value = serde_json::from_slice(&payload).expect("payload is JSON");
    assert_eq!(parsed["manifest_version"], json!(22));
    let agent_id = parsed["bindings"][REPOSITORY]
        .as_str()
        .expect("the payload binds the test repository")
        .to_string();
    let mut index = Vec::new();
    for row in v22_rows() {
        let run = run_v22_row(&row, &payload, &agent_id);
        let record = json!({
            "row": row.name,
            "argv": std::iter::once("gh").chain(row.argv.iter().copied()).collect::<Vec<_>>(),
            "exit_code": run.exit_code,
            "stdout": run.stdout,
            "stderr": run.stderr,
            "upstream_gh_invoked": run.upstream_invocations.is_some(),
            "routed_request": run.routed_request,
        });
        let mut text = serde_json::to_string_pretty(&record).expect("serialize record");
        text.push('\n');
        fs::write(evidence.join(format!("{}.json", row.name)), text).expect("write record");
        check_v22_row(&row, &run, &agent_id);
        index.push(row.name);
        assert_eq!(
            digest(&fs::read(&payload_path).expect("re-read payload")),
            before,
            "{}: the payload changed",
            row.name
        );
    }
    fs::write(
        evidence.join("payload.sha256"),
        format!("{before}  {}\n", payload_path.display()),
    )
    .expect("write payload digest");
    println!(
        "V22_ROUND_TRIP rows={} payload_sha256={before}",
        index.len()
    );
}
