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
//!     --test gh_shim_wire_goldens_test -- --test-threads=1
//! ```
//!
//! Isolation: every run gets its own HOME, XDG config/state directories,
//! `AFT_GH_SHIM_STATE_DIR` and `AFT_STORAGE_DIR` under a temporary directory,
//! the subc connection file points at the loopback fake holder, and PATH puts a
//! recording stand-in for upstream `gh` first. Nothing reaches GitHub, a real
//! route holder, or the operator's shim state.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

/// Seed of the dev manifest-signing key compiled into debug builds; see
/// `crates/aft/tests/fixtures/gh_shim/README.md`.
const DEV_MANIFEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
const REGEN_ENV: &str = "AFT_GH_SHIM_WIRE_GOLDENS_REGEN";
const REPOSITORY: &str = "cortexkit/aft";
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
        Exchange {
            name: "v1-issue-comment-unbound-identity",
            verb: "v1-issue-comment",
            reply: HolderReply::Respond("unbound-identity.response.json"),
        },
        Exchange {
            name: "v1-issue-comment-upstream-error",
            verb: "v1-issue-comment",
            reply: HolderReply::Respond("upstream-error.response.json"),
        },
        Exchange {
            name: "v12-issue-close-state-applied-comment-failed",
            verb: "v12-issue-close",
            reply: HolderReply::Respond("state-applied-comment-failed.response.json"),
        },
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

/// A request frame the holder read, in arrival order.
#[derive(Clone, Debug)]
struct CapturedFrame {
    on_route: bool,
    body: Vec<u8>,
}

/// Loopback subc daemon that advertises `prefrontal-core` as the `gh.route`
/// management surface, opens route channel 42, records every request frame
/// body, and answers the governed request according to its `HolderReply`.
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
                    })
                    .expect("hello ack body"),
                )
                .expect("hello ack frame"),
            ),
            FrameType::Request => {
                let on_route = frame.header.channel == ROUTE_CHANNEL;
                captured
                    .lock()
                    .expect("captured frames lock")
                    .push(CapturedFrame {
                        on_route,
                        body: frame.body.clone(),
                    });
                let op = serde_json::from_slice::<Value>(&frame.body)
                    .ok()
                    .and_then(|value| value.get("op").and_then(Value::as_str).map(str::to_string));
                match op.as_deref() {
                    Some("catalog.list") => Some(response_frame(
                        &frame,
                        serde_json::to_vec(&json!({
                            "op": "catalog.list",
                            "generation": 1,
                            "modules": [{
                                "module_id": "prefrontal-core",
                                "module_version": "0.1.0",
                                "roles": [{
                                    "role": "management_surface",
                                    "operations": [{ "name": "gh.route", "kind": "query" }],
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
                    _ if on_route => reply.clone().map(|bytes| response_frame(&frame, bytes)),
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
/// own-issue `issue edit`, the own-comment PATCH), signed with the dev key and
/// published as version 14, so one manifest admits every governed family
/// exercised here (v1, v10 `--edit-last`, v12, v14).
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
    fs::write(
        &gh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$GH_SHIM_TEST_RECORD\"\nexit 73\n",
    )
    .expect("write upstream gh stand-in");
    let mut permissions = fs::metadata(&gh).expect("gh metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gh, permissions).expect("make gh stand-in executable");
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
