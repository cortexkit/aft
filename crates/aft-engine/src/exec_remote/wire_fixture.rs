//! A minimal authenticated daemon fixture: no execution, local fallback,
//! snapshots, or runner framing. It can lose accepted jobs and return unknown
//! refusal reasons, just like the executor's caller contract permits.
use super::{types::*, *};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::{BindIdentity, Frame, FrameType, ModuleHelloAckBody, PROTOCOL_VERSION};

// Protocol 0.34 clients require numeric daemon versions of at least 0.39.0
// before sending project-free opens. Keep this stand-in's metadata compatible
// so its scripts exercise executor traffic rather than the version refusal.
const FAKE_DAEMON_VERSION: &str = "0.39.0";

pub fn id() -> Uuid {
    "0192a64a-1234-7000-8000-000000000001".parse().unwrap()
}

pub fn cancel_exit_status_unknown_terminal() -> TerminalRecord {
    // The captured tree hash is redacted; its value supplies no process-liveness proof.
    serde_json::from_value(json!({
        "job_id": "01a16b6f-5136-775f-a4f9-0ff89d117615",
        "outcome": {"type": "outcome_unknown"},
        "killed": "cancel",
        "wall_ms": 61620,
        "queue_wait_ms": 27,
        "bundle_bytes": 487115,
        "ran": "remote",
        "tree_hash": "f6be28c…",
        "workspace_changes": []
    }))
    .unwrap()
}

/// The terminal record of a published `crate-local-server-reports-*` vector
/// (see `fixtures/SOURCE.md`), after checking its bytes against the published
/// SHA-256. Its job ID is [`id`].
pub fn report_vector(name: &str) -> TerminalRecord {
    use sha2::{Digest, Sha256};
    let (jcs, sha): (&[u8], &str) = match name {
        "all" => (
            include_bytes!("fixtures/reports/crate-local-server-reports-all.jcs"),
            include_str!("fixtures/reports/crate-local-server-reports-all.sha256"),
        ),
        "unchanged" => (
            include_bytes!("fixtures/reports/crate-local-server-reports-unchanged.jcs"),
            include_str!("fixtures/reports/crate-local-server-reports-unchanged.sha256"),
        ),
        "older-runner" => (
            include_bytes!("fixtures/reports/crate-local-server-reports-older-runner.jcs"),
            include_str!("fixtures/reports/crate-local-server-reports-older-runner.sha256"),
        ),
        "detached-head" => (
            include_bytes!("fixtures/reports/crate-local-server-reports-detached-head.jcs"),
            include_str!("fixtures/reports/crate-local-server-reports-detached-head.sha256"),
        ),
        "truncated-untracked" => (
            include_bytes!("fixtures/reports/crate-local-server-reports-truncated-untracked.jcs"),
            include_str!("fixtures/reports/crate-local-server-reports-truncated-untracked.sha256"),
        ),
        other => panic!("no report vector {other:?}"),
    };
    assert_eq!(format!("{:x}", Sha256::digest(jcs)), sha.trim(), "{name}");
    let vector: Value = serde_json::from_slice(jcs).unwrap();
    let stream = vector["stream"].as_array().unwrap();
    assert_eq!(stream.len(), 1, "{name}");
    match serde_json::from_value(stream[0].clone()).unwrap() {
        StreamRecord::Terminal(terminal) => {
            assert_eq!(terminal.job_id, id());
            terminal
        }
        other => panic!("{name}: expected a terminal record, got {other:?}"),
    }
}

/// ck-motor's `queue_wait_exceeded` detail for a job that waited on its
/// workspace owner, in the runner's own layout: `waiting_on=<phase>; ...;
/// holders=[job=<id> workspace=<key> permits=<n> phase=<p> since_ms=<t>; ...]; ...`.
/// It lists every running job, so an unrelated job in another workspace
/// appears beside the owner.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn queue_owner_detail(holder: Uuid) -> String {
    format!(
        "waiting_on=workspace owner; requested_permits=4; held_permits=8/16; accepted_ms=1000; holders=[job={holder} workspace=/work/task permits=4 phase=running since_ms=900; job=0192a64a-1234-7000-8000-0000000000bb workspace=/work/other permits=4 phase=running since_ms=950]; reservations=[]; heads_ahead=[]"
    )
}

/// ck-motor's terminal record for a job refused because its signer was briefly
/// unavailable, byte for byte as the runner sends it.
#[cfg_attr(not(unix), allow(dead_code))]
pub const SIGNER_REFUSAL_VECTOR: &str = r#"{"bundle_bytes":0,"job_id":"01a16152-22ae-71e6-9735-1905454c9804","outcome":{"reason":"runner_authentication_failed","type":"refused_before_start"},"queue_wait_ms":0,"ran":"none","refusal_detail":"runner_sign_unavailable","retry_after_ms":2000,"tree_hash":null,"type":"terminal","wall_ms":0,"workspace_changes":null}"#;

/// [`SIGNER_REFUSAL_VECTOR`] with another refusal detail and retry hint. The
/// runner omits the hint field (it never sends null) when it gives none.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn signer_refusal(detail: &str, retry_after_ms: Option<u64>) -> TerminalRecord {
    let mut value: Value = serde_json::from_str(SIGNER_REFUSAL_VECTOR).unwrap();
    value["refusal_detail"] = json!(detail);
    match retry_after_ms {
        Some(ms) => value["retry_after_ms"] = json!(ms),
        None => {
            value.as_object_mut().unwrap().remove("retry_after_ms");
        }
    }
    match serde_json::from_value(value).unwrap() {
        StreamRecord::Terminal(terminal) => terminal,
        other => panic!("expected a terminal record, got {other:?}"),
    }
}

// Most scripts drive the Unix-only remote bash tests.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
pub enum Script {
    ReturnCapRefusal {
        code: &'static str,
        field: &'static str,
        max_bytes: Option<u64>,
        always: bool,
        after_accepted: bool,
    },
    SubmissionFreshReplyLost,
    SubmissionGapExpired {
        unknown: bool,
    },
    Submission {
        never_accepted: bool,
        mismatch: bool,
        always_unknown: bool,
        disconnect: bool,
    },
    Lost,
    Refused,
    /// Hold the first record until `acceptance_gate`, then refuse without Accepted.
    GatedPreStartRefusal {
        reason: &'static str,
        detail: Option<&'static str>,
    },
    Restart,
    Cancel,
    /// Keep an existing job's attach stream open until `exec.cancel` reaches it.
    AttachUntilCancelled,
    /// Hold the run stream open until cancellation; attach returns this terminal.
    CancelTerminal {
        terminal: fn() -> TerminalRecord,
        stderr: &'static str,
    },
    CancelRefused,
    MissingTerminal,
    KnownRefused,
    WorkspaceSetupRefused,
    RefusedWithDetail,
    Draining {
        refusals: u32,
        retry_after_ms: Option<u64>,
    },
    DiskFull {
        refusals: u32,
        retry_after_ms: Option<u64>,
    },
    /// Refuse the first `refusals` runs with [`signer_refusal`] for this detail
    /// and hint, sending no `Accepted`; later runs are accepted and exit 0.
    SignerRefused {
        refusals: u32,
        detail: &'static str,
        retry_after_ms: Option<u64>,
    },
    /// Refuse the first run as draining with hint `draining_after_ms`, the
    /// next `signer_refusals` runs with [`signer_refusal`] for
    /// `runner_sign_unavailable` with hint `signer_after_ms`, then accept
    /// later runs, which exit 0. No refusal sends `Accepted`.
    DrainingThenSigner {
        signer_refusals: u32,
        draining_after_ms: u64,
        signer_after_ms: u64,
    },
    FutureOutcome,
    Expired,
    Utf8,
    Deadline,
    AttachRefused,
    RetainedGap,
    PersistentTerminalGap,
    TransientTerminalGap,
    TerminalGapOnce,
    Continuous,
    GappedAttach,
    GappedCancel,
    AttachDisconnected,
    /// Accepted, then exited 0 with the published `all` report: changed files,
    /// changed Git state, untracked files and ignored writes.
    Reported,
    /// Accepted, then exited 0 with a terminal record that reports nothing
    /// about the workspace (no changed-file list).
    Plain,
    /// Copy one tracked file and report the bytes taken by this fake snapshot.
    CaptureCheckout {
        gated: bool,
    },
    /// A runner with a long queue: accepted at `position`, then queued for
    /// `queue` with no records, then one output record (it started), then
    /// after a further `run` it exits 0.
    Staged {
        position: u32,
        queue: Duration,
        run: Duration,
    },
    /// Accepted immediately; output and terminal each require a test release.
    GatedStages {
        position: u32,
    },
    /// Gated output and terminal with the executor's detach-only request Cancel;
    /// only exec.cancel stops the job, independently of its observation.
    DetachSafeStages {
        position: u32,
    },
    /// Accepted, then refused before start (`unreachable`): the runner took
    /// the inputs but never ran the command.
    AcceptedThenRefused,
    /// Withhold the first stream record until `Daemon::acceptance_gate` is notified.
    GatedAcceptance,
    /// Like `GatedAcceptance`, but once released the runner accepts the job and
    /// at once reports it exited 0: a job that waited long for admission and
    /// then finished.
    GatedAcceptanceThenExit,
    /// Like `GatedAcceptance`, but once released the runner accepts the inputs
    /// and then refuses before the command starts.
    GatedAcceptanceThenRefused,
    /// Hold admission until released, then complete or refuse without running.
    GatedAdmission {
        refuse: bool,
    },
    /// Release acceptance separately from placement progress, optionally carrying
    /// a job id. Only exec.cancel stops the job; attach confirms it never ran.
    PlacementWait {
        job_id: bool,
    },
    /// Delay route opening before the client can submit exec.run.
    GatedRoute,
    /// Drop the connection on the first `exec.run`, before any stream record:
    /// the runner never sends `Accepted`, so the caller cannot know the outcome.
    DropBeforeAccepted,
    /// Refuse once while draining, then gate the retry's first record.
    DrainingGatedAcceptance,
    NetworkGranted,
    NetworkUnknown,
    NetworkLost,
    NetworkUnsupported,
    Started,
    /// Keep the original run alive; restored callers may be refused before
    /// an attach finally returns output and a terminal (or an unknown outcome).
    RestartRunning {
        auth_refusals: u32,
        lost: bool,
    },
    Windows {
        code: Option<u32>,
        acknowledge: bool,
    },
    /// Accepted, then refused with `queue_wait_exceeded` while waiting on its
    /// workspace owner `holder`, with ck-motor's queue report as the detail.
    QueueWaitOwner {
        holder: Uuid,
    },
    /// Answer the run with this published chunked-return vector's records,
    /// after an acceptance when the vector has none. A replay vector starts
    /// past seq 0, so empty output stands in for the seqs before it. An
    /// attach replays the vector's records from the requested seq.
    ReturnVector(&'static str),
    /// Like `ReturnVector`, but the terminal record reports these
    /// checkout-relative paths as changed, as ck-motor's Linux runner does
    /// (its list includes untracked files; it sends no separate list).
    ReturnVectorReporting(&'static str, &'static [&'static str]),
    /// Refuse the run with ck-motor's pinned final refusal for return_paths
    /// on a runner that does not copy files back.
    ReturnPathsUnsupported,
    /// Like `ReturnVector`, but nothing is sent, not even the acceptance,
    /// until `Daemon::acceptance_gate` is notified.
    GatedReturnVector(&'static str),
    /// The published expired replay reshaped as ck-motor sends it: its
    /// 10-byte file had one chunk, so the manifest follows at seq 4.
    MotorExpiredReplay,
}

pub struct Daemon {
    pub(crate) connection: std::path::PathBuf,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) connections: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) log: Arc<Mutex<Vec<(subc_protocol::EnvelopeHeader, Value)>>>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) acceptance_gate: Arc<tokio::sync::Notify>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) output_gate: Arc<tokio::sync::Notify>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) completion_gate: Arc<tokio::sync::Notify>,
    #[cfg_attr(not(unix), allow(dead_code))]
    requests: Arc<tokio::sync::Notify>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub async fn wait_for_requests(&self, method: &str, count: usize) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let notified = self.requests.notified();
                if self
                    .log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, body)| body["method"] == method)
                    .count()
                    >= count
                {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("fake runner must receive the expected request");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub async fn daemon(script: Script, claim: &str) -> Daemon {
    daemon_with_clients(script, claim, 1).await
}

pub async fn daemon_with_clients(script: Script, claim: &str, clients: usize) -> Daemon {
    daemon_with_capabilities(script, &[claim], clients).await
}

pub async fn daemon_with_capabilities(script: Script, claims: &[&str], clients: usize) -> Daemon {
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let log = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_connections = connections.clone();
    let server_log = Arc::clone(&log);
    let acceptance_gate = Arc::new(tokio::sync::Notify::new());
    let server_acceptance_gate = acceptance_gate.clone();
    let output_gate = Arc::new(tokio::sync::Notify::new());
    let server_output_gate = output_gate.clone();
    let completion_gate = Arc::new(tokio::sync::Notify::new());
    let server_completion_gate = completion_gate.clone();
    let requests = Arc::new(tokio::sync::Notify::new());
    let server_requests = requests.clone();
    let server_key = key.clone();
    let claims: Vec<String> = claims.iter().map(|claim| (*claim).to_string()).collect();

    let server = tokio::spawn(async move {
        let mut runs = 0;
        let mut attaches = 0;
        let mut submission: Option<Value> = None;
        for _ in 0..clients {
            let (mut socket, _) = listener.accept().await.unwrap();
            server_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            subc_transport::authenticate_server(
                &mut socket,
                &server_key,
                &daemon_id,
                FAKE_DAEMON_VERSION,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            let (mut reader, writer) = tokio::io::split(socket);
            let writer = Arc::new(tokio::sync::Mutex::new(writer));
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut producers = Vec::new();
            let mut cancelled = false;
            while let Ok(Some(frame)) = subc_transport::read_frame(&mut reader).await {
                if let Some(reply) = aft_base::subc_fake::server_describe_reply(&frame) {
                    if subc_transport::write_frame(&mut *writer.lock().await, &reply)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let header = frame.header;
                let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                server_log.lock().unwrap().push((header, body.clone()));
                server_requests.notify_one();
                let reply = |ty, value: Value| {
                    Frame::build_with_version(
                        header.ver,
                        ty,
                        header.flags,
                        header.channel,
                        header.epoch,
                        header.corr,
                        serde_json::to_vec(&value).unwrap(),
                    )
                    .unwrap()
                };
                let mut replies = Vec::new();
                match header.ty {
                FrameType::Hello => replies.push(reply(FrameType::HelloAck, serde_json::to_value(ModuleHelloAckBody {
                    negotiated_ver: PROTOCOL_VERSION, subc_ops: vec!["catalog.list".into()], subc_capabilities: vec![], storage: None, machine_id: None,
                }).unwrap())),
                FrameType::Request if header.channel == 0 => match body["op"].as_str() {
                    Some("catalog.list") => replies.push(reply(FrameType::Response, json!({"op":"catalog.list", "generation":1, "subc_ops":["catalog.list"],
                        "modules":[{"module_id":"executor-picked-by-capability", "roles":[], "control_ops":[], "capabilities":{"provides":claims, "requires":[]}}]}))),
                    Some("route.open") if matches!(script, Script::GatedRoute) => {
                        let frame = reply(FrameType::Response, json!({"op":"route.open", "route_channel":40, "route_epoch":1}));
                        let writer = writer.clone();
                        let gate = server_acceptance_gate.clone();
                        producers.push(tokio::spawn(async move {
                            gate.notified().await;
                            let _ = subc_transport::write_frame(&mut *writer.lock().await, &frame).await;
                        }));
                    },
                    Some("route.open") => replies.push(reply(FrameType::Response, json!({"op":"route.open", "route_channel":40, "route_epoch":1}))),
                    _ => panic!("unexpected control request {body}"),
                },
                FrameType::Request => {
                    match body["method"].as_str().unwrap() {
                        "exec.run" if matches!(script, Script::CaptureCheckout { .. }) => {
                            if matches!(script, Script::CaptureCheckout { gated: true }) {
                                server_acceptance_gate.notified().await;
                            }
                            let root = Path::new(body["params"]["workspace_key"].as_str().unwrap());
                            let bytes = std::fs::read(root.join("tracked.txt")).unwrap();
                            for record in [
                                StreamRecord::Accepted(Accepted::new(id(), 1)),
                                StreamRecord::Output(Output::new(0, OutputStream::Stdout, BytePayload(bytes))),
                                StreamRecord::Terminal(TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 0, 0, 0)),
                            ] {
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(record).unwrap()));
                            }
                            replies.push(reply(FrameType::StreamEnd, Value::Null));
                        }
                        "exec.run" if matches!(script, Script::ReturnCapRefusal { .. }) => {
                            let Script::ReturnCapRefusal { code, field, max_bytes, always, after_accepted } = script else { unreachable!() };
                            runs += 1;
                            if runs == 1 || always {
                                if after_accepted {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                                }
                                let mut detail = json!({"field":field});
                                if let Some(maximum) = max_bytes { detail["max_bytes"] = json!(maximum); }
                                replies.push(reply(FrameType::Error, json!({"code":code,"message":"return cap refused","detail":detail})));
                            } else {
                                let records = super::return_vector_fixture::return_vector_records("two-files-one-empty");
                                if !matches!(records.first(), Some(StreamRecord::Accepted(_))) {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                                }
                                let first_seq = records.iter().find_map(|record| match record {
                                    StreamRecord::ReturnChunk(chunk) => Some(chunk.seq),
                                    StreamRecord::ReturnManifest(manifest) => Some(manifest.seq),
                                    _ => None,
                                }).unwrap_or(0);
                                for seq in 0..first_seq {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(Vec::new())))).unwrap()));
                                }
                                for record in records {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(record).unwrap()));
                                }
                                replies.push(reply(FrameType::StreamEnd, Value::Null));
                            }
                        }
                        "exec.run" if matches!(script, Script::ReturnPathsUnsupported) => {
                            let terminal: Value = serde_json::from_str(include_str!("fixtures/motor/return-paths-unsupported.json")).unwrap();
                            replies.push(reply(FrameType::StreamData, terminal));
                            replies.push(Frame::build_with_version(header.ver, FrameType::StreamEnd, header.flags, header.channel, header.epoch, header.corr, vec![]).unwrap());
                        }
                        "exec.attach" if matches!(script, Script::ReturnVector(_)) => {
                            // Replay the vector from the requested seq, as a
                            // runner that still keeps the job's files does.
                            let Script::ReturnVector(name) = script else { unreachable!() };
                            let from = body["params"]["from_seq"].as_u64().unwrap();
                            for record in super::return_vector_fixture::return_vector_records(name) {
                                let seq = match &record {
                                    StreamRecord::Accepted(_) => continue,
                                    StreamRecord::Started(started) => Some(started.seq),
                                    StreamRecord::Output(output) => Some(output.seq),
                                    StreamRecord::ReturnChunk(chunk) => Some(chunk.seq),
                                    StreamRecord::ReturnManifest(manifest) => Some(manifest.seq),
                                    _ => None,
                                };
                                if seq.is_none_or(|seq| seq >= from) {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(record).unwrap()));
                                }
                            }
                            replies.push(Frame::build_with_version(header.ver, FrameType::StreamEnd, header.flags, header.channel, header.epoch, header.corr, vec![]).unwrap());
                        }
                        "exec.run" if matches!(script, Script::ReturnVector(_) | Script::ReturnVectorReporting(..) | Script::GatedReturnVector(_) | Script::MotorExpiredReplay) => {
                            let records = match script {
                                Script::ReturnVector(name) | Script::ReturnVectorReporting(name, _) | Script::GatedReturnVector(name) => super::return_vector_fixture::return_vector_records(name),
                                _ => super::return_vector_fixture::motor_expired_replay(4, None),
                            };
                            if !matches!(records.first(), Some(StreamRecord::Accepted(_))) {
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                            }
                            // A replay vector starts after the records the caller
                            // already had; stand in for them with empty output.
                            let first_seq = records.iter().find_map(|record| match record {
                                StreamRecord::Started(started) => Some(started.seq),
                                StreamRecord::Output(output) => Some(output.seq),
                                StreamRecord::ReturnChunk(chunk) => Some(chunk.seq),
                                StreamRecord::ReturnManifest(manifest) => Some(manifest.seq),
                                _ => None,
                            }).unwrap_or(0);
                            let mut records = records.into_iter();
                            if let Some(accepted) = records.as_slice().first().filter(|record| matches!(record, StreamRecord::Accepted(_))).cloned() {
                                records.next();
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(accepted).unwrap()));
                            }
                            for seq in 0..first_seq {
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(Vec::new())))).unwrap()));
                            }
                            for mut record in records {
                                if let (Script::ReturnVectorReporting(_, reported), StreamRecord::Terminal(terminal)) = (script, &mut record) {
                                    terminal.workspace_changes = Some(reported.iter().map(|path| path.to_string()).collect());
                                }
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(record).unwrap()));
                            }
                            replies.push(Frame::build_with_version(header.ver, FrameType::StreamEnd, header.flags, header.channel, header.epoch, header.corr, vec![]).unwrap());
                            if matches!(script, Script::GatedReturnVector(_)) {
                                // Hold every record, acceptance included, until the test releases them.
                                let frames = std::mem::take(&mut replies);
                                let writer = writer.clone();
                                let gate = server_acceptance_gate.clone();
                                producers.push(tokio::spawn(async move {
                                    gate.notified().await;
                                    for frame in frames {
                                        let _ = subc_transport::write_frame(&mut *writer.lock().await, &frame).await;
                                    }
                                }));
                            }
                        }
                        "exec.attach" if matches!(script, Script::RestartRunning { .. }) => {
                            let Script::RestartRunning { auth_refusals, lost } = script else { unreachable!() };
                            attaches += 1;
                            if attaches <= auth_refusals {
                                replies.push(reply(FrameType::Error, json!({"code":"auth", "message":"host refused caller attach"})));
                            } else {
                                if !lost {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(1, OutputStream::Stdout, BytePayload(b"remote-restart-proof\n".to_vec())))).unwrap()));
                                }
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(TerminalRecord::new(id(), if lost { Outcome::OutcomeUnknown } else { Outcome::Exit { code: 0 } }, 1, 0, 0))).unwrap()));
                                replies.push(reply(FrameType::StreamEnd, Value::Null));
                            }
                        }
                        "exec.run" | "exec.attach" if matches!(script, Script::PlacementWait { .. }) => {
                            if body["method"] == "exec.run" {
                                assert_eq!(body["params"]["progress"], true);
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Progress(
                                    match script {
                                        Script::PlacementWait { job_id: true } => Progress::new(ProgressPhase::PlacementWait, 0, Some("waiting for a host with 8 free vCPUs".into())).unwrap().with_job_id(id()),
                                        _ => Progress::new(ProgressPhase::PlacementWait, 26 * 60 * 1000, Some("waiting for a host with 8 free vCPUs".into())).unwrap(),
                                    }
                                )).unwrap()));
                                let accepted = reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap());
                                let writer = writer.clone();
                                let gate = server_acceptance_gate.clone();
                                producers.push(tokio::spawn(async move {
                                    gate.notified().await;
                                    let _ = subc_transport::write_frame(&mut *writer.lock().await, &accepted).await;
                                }));
                            } else {
                                assert_eq!(body["params"]["job_id"], id().to_string());
                                let mut terminal = TerminalRecord::new(id(), if cancelled { Outcome::Cancelled } else { Outcome::OutcomeUnknown }, 0, 0, 0);
                                terminal.ran = Some(Ran::None);
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(terminal)).unwrap()));
                                replies.push(reply(FrameType::StreamEnd, Value::Null));
                            }
                        }
                        "exec.attach" if matches!(script, Script::AttachUntilCancelled) && !cancelled => {},
                        "exec.run" | "exec.attach" => {
                            let attaching = body["method"] == "exec.attach";
                            if !attaching { runs += 1; }
                            if matches!(script, Script::SubmissionFreshReplyLost) {
                                if runs == 1 {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(TerminalRecord::new(
                                        Uuid::from_bytes([0x77;16]), Outcome::RefusedBeforeStart { reason: RefusalReason::SubmissionNeverAccepted }, 0, 0, 0,
                                    ))).unwrap()));
                                    replies.push(reply(FrameType::StreamEnd, Value::Null));
                                }
                                // Later requests may be running, but provide no admission or result.
                            } else if let Script::SubmissionGapExpired { unknown } = script {
                                let initial = !attaching && runs == 1;
                                if initial {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                                    for (seq, bytes) in [(0, b"A".to_vec()), (2, b"C".to_vec())] {
                                        replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(bytes)))).unwrap()));
                                    }
                                }
                                let outcome = if initial { Outcome::Exit { code: 0 } } else if unknown { Outcome::OutcomeUnknown } else { Outcome::HistoryExpired };
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(TerminalRecord::new(id(), outcome, 0, 0, 0))).unwrap()));
                                replies.push(reply(FrameType::StreamEnd, Value::Null));
                            } else if attaching && matches!(script, Script::Submission { .. }) {
                                let from = body["params"]["from_seq"].as_u64().unwrap();
                                for seq in from..4 {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(format!("line-{seq}\n").into_bytes())))).unwrap()));
                                }
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 0, 0, 0))).unwrap()));
                                replies.push(reply(FrameType::StreamEnd, Value::Null));
                            } else if let Script::Submission { never_accepted, mismatch, always_unknown, disconnect } = script {
                                let mut request = body["params"].clone();
                                request.as_object_mut().unwrap().remove("progress");
                                let cursor = request.as_object_mut().unwrap().remove("resume_from_seq").and_then(|v| v.as_u64());
                                let keyed = request["submission_key"].is_string();
                                let same_key = keyed && submission.as_ref().is_some_and(|old| old["submission_key"] == request["submission_key"]);
                                let refusal = same_key && (mismatch || submission.as_ref() != Some(&request) || never_accepted);
                                let unknown = runs == 1 || always_unknown;
                                if !same_key { submission = Some(request); }
                                if refusal {
                                    // Refusals have their own ID, never the existing job's ID.
                                    let mut refusal_id = [0x77; 16];
                                    refusal_id[8..].copy_from_slice(&(runs as u64).to_be_bytes());
                                    let record = TerminalRecord::new(Uuid::from_bytes(refusal_id), Outcome::RefusedBeforeStart {
                                        reason: if never_accepted { RefusalReason::SubmissionNeverAccepted } else { RefusalReason::RequestRejected }
                                    }, 0, 0, 0).with_refusal_detail(if never_accepted { "submission_never_accepted" } else { "submission_key_mismatch" });
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(record)).unwrap()));
                                } else {
                                    if !(never_accepted && unknown) {
                                        replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1).with_attached(same_key))).unwrap()));
                                        // ck-motor's keyed replay cursor is exclusive. Without a cursor
                                        // it replays the whole stream, including already delivered lines.
                                        let from = cursor.map_or(0, |seq| seq + 1);
                                        let end = if unknown { 2 } else { 4 };
                                        for seq in from..end {
                                            replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(format!("line-{seq}\n").into_bytes())))).unwrap()));
                                        }
                                    }
                                    if !(disconnect && runs == 1) || never_accepted {
                                        replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(TerminalRecord::new(id(), if unknown { Outcome::OutcomeUnknown } else { Outcome::Exit { code: 0 } }, 0, 0, 0))).unwrap()));
                                    }
                                }
                                if !(disconnect && runs == 1) { replies.push(reply(FrameType::StreamEnd, Value::Null)); }
                            } else {
                            if !attaching && body["params"]["progress"] == true && claims.iter().any(|claim| claim == PROGRESS_CAPABILITY) {
                                let progress = Progress::new(ProgressPhase::Upload, 14_000, Some("plain <detail>".into())).unwrap();
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Progress(progress)).unwrap()));
                            }
                            let draining = matches!(script, Script::Draining { refusals, .. } | Script::DiskFull { refusals, .. } | Script::SignerRefused { refusals, .. } if runs <= refusals)
                                || matches!(script, Script::DrainingThenSigner { signer_refusals, .. } if runs <= 1 + signer_refusals)
                                || matches!(script, Script::DrainingGatedAcceptance) && runs == 1;
                            if attaching && matches!(script, Script::AttachDisconnected) {
                                // Drop the route and listener; later attach calls and connects fail.
                                return;
                            }
                            if !attaching && matches!(script, Script::DropBeforeAccepted) {
                                // Drop the connection and listener with the run in flight.
                                return;
                            }
                            if !attaching && !draining && !matches!(script, Script::Refused | Script::GatedPreStartRefusal { .. } | Script::KnownRefused | Script::WorkspaceSetupRefused | Script::RefusedWithDetail | Script::NetworkUnsupported | Script::GatedAcceptance | Script::GatedAcceptanceThenExit | Script::GatedAcceptanceThenRefused | Script::GatedAdmission { .. } | Script::DrainingGatedAcceptance) {
                                let mut accepted = Accepted::new(id(), if let Script::Staged { position, .. } | Script::GatedStages { position } | Script::DetachSafeStages { position } = script { position } else { 1 });
                                if matches!(script, Script::Windows { acknowledge: true, .. }) {
                                    accepted = accepted.with_platform(Platform::Windows);
                                }
                                if matches!(script, Script::NetworkGranted | Script::NetworkLost) {
                                    accepted = accepted.with_network(Network::Outbound);
                                } else if matches!(script, Script::NetworkUnknown) {
                                    accepted = accepted.with_network(Network::Unknown("future_network".into()));
                                }
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(accepted)).unwrap()));
                                if matches!(script, Script::Windows { .. }) {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Started(Started::new(0, 123, 1).with_path_mapping(PathMapping::new("/", r"C:\m")))).unwrap()));
                                }
                                if matches!(script, Script::Started | Script::RestartRunning { .. }) {
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Started(Started::new(0, 123, 1))).unwrap()));
                                }
                            }
                             let outcome = if matches!(script, Script::PersistentTerminalGap | Script::TransientTerminalGap | Script::TerminalGapOnce) { Outcome::Exit { code: 7 } }
                                 else if attaching && matches!(script,Script::AttachRefused) { Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown("future_refusal".into()) } }
                                else if draining { Outcome::RefusedBeforeStart { reason: if matches!(script, Script::DiskFull { .. }) { RefusalReason::RunnerDiskFull } else { RefusalReason::RunnerDraining } } }
                                 else if attaching && cancelled { Outcome::Signal { signal: 15 } }
                                else { match script {
                                    Script::Lost | Script::NetworkLost => Outcome::OutcomeUnknown,
                                    Script::NetworkUnsupported => Outcome::RefusedBeforeStart { reason: RefusalReason::NetworkUnsupported },
                                    Script::Refused => Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown("future_refusal".into()) },
                                    Script::KnownRefused | Script::AcceptedThenRefused => Outcome::RefusedBeforeStart { reason: RefusalReason::Unreachable },
                                    Script::WorkspaceSetupRefused => Outcome::RefusedBeforeStart { reason: RefusalReason::WorkspaceSetupFailed },
                                    Script::RefusedWithDetail => Outcome::RefusedBeforeStart { reason: RefusalReason::RunnerDraining },
                                    Script::FutureOutcome => Outcome::Unknown { kind:"future_outcome".into() },
                                    Script::Expired => Outcome::HistoryExpired,
                                    Script::QueueWaitOwner { .. } => Outcome::RefusedBeforeStart { reason: RefusalReason::QueueWaitExceeded },
                                    Script::Windows { code: Some(code), .. } => Outcome::WindowsExit { code },
                                    Script::Windows { code: None, .. } => Outcome::OutcomeUnknown,
                                    _ => Outcome::Exit { code: 0 },
                                }};
                            if matches!(script, Script::Cancel | Script::CancelTerminal { .. } | Script::CancelRefused | Script::Continuous | Script::GappedCancel | Script::Staged { .. } | Script::GatedStages { .. } | Script::DetachSafeStages { .. } | Script::Started | Script::RestartRunning { .. } | Script::GatedAcceptance | Script::GatedAcceptanceThenExit | Script::GatedAcceptanceThenRefused | Script::GatedPreStartRefusal { .. } | Script::GatedAdmission { .. } | Script::DrainingGatedAcceptance) && !attaching && !draining { /* held-open run stream */ }
                            else if attaching && matches!(script, Script::GappedAttach | Script::GappedCancel) { /* output is sent below; completion waits for the test's release */ }
                            else {
                                if matches!(script, Script::Restart) {
                                    let from = if attaching { body["params"]["from_seq"].as_u64().unwrap() } else { 0 };
                                    let end = if attaching { 4 } else { 2 };
                                    for seq in from..end {
                                        replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(vec![b'A' + seq as u8])))).unwrap()));
                                    }
                                }
                                if matches!(script, Script::Utf8) {
                                    for (seq,stream,bytes) in [(0,OutputStream::Stdout,vec![0xe2]),(1,OutputStream::Stderr,vec![0xf0,0x9f]),(2,OutputStream::Stdout,vec![0x82,0xac]),(3,OutputStream::Stderr,vec![0x98,0x80])] {
                                        replies.push(reply(FrameType::StreamData,serde_json::to_value(StreamRecord::Output(Output::new(seq,stream,BytePayload(bytes)))).unwrap()));
                                    }
                                }
                                 if matches!(script,Script::RetainedGap) {
                                    let output=Output::new(3,OutputStream::Stdout,BytePayload(b"D".to_vec())).with_truncated_before_seq(3);
                                    replies.push(reply(FrameType::StreamData,serde_json::to_value(StreamRecord::Output(output)).unwrap()));
                                 }
                                 if matches!(script, Script::PersistentTerminalGap | Script::TransientTerminalGap | Script::TerminalGapOnce) && !(attaching && matches!(script, Script::TerminalGapOnce)) {

                                     let from = if attaching { body["params"]["from_seq"].as_u64().unwrap() } else { 0 };
                                     let first = if attaching && matches!(script, Script::TransientTerminalGap) { from } else { from.max(1) };
                                     for seq in (first..3).rev() {
                                         replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(vec![b'A' + seq as u8])))).unwrap()));
                                     }
                                 }
                                 if !matches!(script, Script::MissingTerminal) && !(attaching && matches!(script, Script::TerminalGapOnce)) && (!matches!(script, Script::Restart | Script::AttachRefused | Script::GappedAttach | Script::AttachDisconnected) || attaching) {
                                    let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
                                    if matches!(script, Script::RefusedWithDetail) {
                                        let mut value = serde_json::to_value(&terminal).unwrap();
                                        value["refusal_detail"] = json!("runner says:\nmaintenance\twindow");
                                        terminal = serde_json::from_value(value).unwrap();
                                    }
                                    if let Script::QueueWaitOwner { holder } = script {
                                        terminal.refusal_detail = Some(queue_owner_detail(holder));
                                    }
                                    if matches!(script, Script::NetworkUnsupported) {
                                        terminal.refusal_detail = Some("outbound access disabled by runner policy".into());
                                    }
                                     if draining {
                                         if matches!(script, Script::DrainingGatedAcceptance) {
                                             terminal.retry_after_ms = Some(50);
                                         }
                                        if let Script::Draining { retry_after_ms, .. } | Script::DiskFull { retry_after_ms, .. } = script {
                                            terminal.retry_after_ms = retry_after_ms;
                                            terminal.refusal_detail = Some("maintenance".into());
                                        }
                                    }
                                    if let (true, Script::SignerRefused { detail, retry_after_ms, .. }) = (draining, script) {
                                        terminal = signer_refusal(detail, retry_after_ms);
                                    }
                                    if let (true, Script::DrainingThenSigner { draining_after_ms, signer_after_ms, .. }) = (draining, script) {
                                        if runs == 1 {
                                            terminal.retry_after_ms = Some(draining_after_ms);
                                            terminal.refusal_detail = Some("maintenance".into());
                                        } else {
                                            terminal = signer_refusal("runner_sign_unavailable", Some(signer_after_ms));
                                        }
                                    }
                                    if cancelled && !matches!(script, Script::PersistentTerminalGap | Script::TransientTerminalGap | Script::TerminalGapOnce) { terminal = terminal.with_killed(Killed::Cancel); }
                                    if let (true, true, Script::CancelTerminal { terminal: record, stderr }) = (attaching, cancelled, script) {
                                        terminal = record();
                                        if !stderr.is_empty() {
                                            replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(0, OutputStream::Stderr, BytePayload(stderr.as_bytes().to_vec())))).unwrap()));
                                        }
                                    }
                                    if matches!(script, Script::Deadline) { terminal = terminal.with_killed(Killed::Deadline); }
                                    if matches!(script, Script::Reported) { terminal = report_vector("all"); }
                                    if matches!(script, Script::Utf8) {
                                        terminal.pipestatus=Some(vec![3,0]);
                                        terminal.workspace_changes=Some(vec!["generated.txt".into()]);
                                    }
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(terminal)).unwrap()));
                                }
                                replies.push(Frame::build_with_version(header.ver, FrameType::StreamEnd, header.flags, header.channel, header.epoch, header.corr, vec![]).unwrap());
                            }
                            }
                        },
                        "exec.cancel" if matches!(script, Script::CancelRefused) => replies.push(reply(FrameType::Error, json!({"code":"cancel_refused","message":"runner unavailable"}))),
                        "exec.cancel" => { cancelled = true; stop.store(true,std::sync::atomic::Ordering::SeqCst); replies.push(reply(FrameType::Response, serde_json::to_value(CancelReply::new(id())).unwrap())); },
                        "exec.status" => replies.push(reply(FrameType::Response, serde_json::to_value(StatusReply::new(0, vec![], true, vec![], "rustc test")).unwrap())),
                        _ => panic!("unexpected caller operation {body}"),
                    }
                },
                FrameType::Cancel if matches!(script, Script::DetachSafeStages { .. } | Script::PlacementWait { .. } | Script::AttachUntilCancelled) => {},
                FrameType::Cancel => {cancelled = true; stop.store(true,std::sync::atomic::Ordering::SeqCst);},
                _ => {},
            }
                for response in replies {
                    subc_transport::write_frame(&mut *writer.lock().await, &response)
                        .await
                        .unwrap();
                }
                if matches!(
                    script,
                    Script::Submission {
                        disconnect: true,
                        ..
                    }
                ) && runs == 1
                    && body["method"] == "exec.run"
                {
                    break;
                }
                if let Script::GatedAdmission { refuse } = script {
                    if body["method"] == "exec.run" {
                        let writer = writer.clone();
                        let gate = server_acceptance_gate.clone();
                        let stop = stop.clone();
                        producers.push(tokio::spawn(async move {
                            gate.notified().await;
                            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                                return;
                            }
                            let records = if refuse {
                                vec![StreamRecord::Terminal(TerminalRecord::new(
                                    id(),
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::QueueWaitExceeded,
                                    },
                                    0,
                                    0,
                                    0,
                                ))]
                            } else {
                                vec![
                                    StreamRecord::Accepted(Accepted::new(id(), 7)),
                                    StreamRecord::Output(Output::new(
                                        0,
                                        OutputStream::Stdout,
                                        BytePayload(b"admitted\n".to_vec()),
                                    )),
                                    StreamRecord::Terminal(TerminalRecord::new(
                                        id(),
                                        Outcome::Exit { code: 0 },
                                        1,
                                        0,
                                        0,
                                    )),
                                ]
                            };
                            let mut writer = writer.lock().await;
                            for record in records {
                                let frame = Frame::build_with_version(
                                    header.ver,
                                    FrameType::StreamData,
                                    header.flags,
                                    header.channel,
                                    header.epoch,
                                    header.corr,
                                    serde_json::to_vec(&record).unwrap(),
                                )
                                .unwrap();
                                if subc_transport::write_frame(&mut *writer, &frame)
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            let end = Frame::build_with_version(
                                header.ver,
                                FrameType::StreamEnd,
                                header.flags,
                                header.channel,
                                header.epoch,
                                header.corr,
                                vec![],
                            )
                            .unwrap();
                            let _ = subc_transport::write_frame(&mut *writer, &end).await;
                        }));
                    }
                }
                if (matches!(
                    script,
                    Script::GatedAcceptance
                        | Script::GatedAcceptanceThenExit
                        | Script::GatedAcceptanceThenRefused
                        | Script::GatedPreStartRefusal { .. }
                ) || matches!(script, Script::DrainingGatedAcceptance) && runs > 1)
                    && body["method"] == "exec.run"
                {
                    let writer = writer.clone();
                    let gate = server_acceptance_gate.clone();
                    producers.push(tokio::spawn(async move {
                        gate.notified().await;
                        if !matches!(script, Script::GatedPreStartRefusal { .. }) {
                            let accepted = Frame::build_with_version(
                                header.ver,
                                FrameType::StreamData,
                                header.flags,
                                header.channel,
                                header.epoch,
                                header.corr,
                                serde_json::to_vec(&StreamRecord::Accepted(Accepted::new(id(), 1)))
                                    .unwrap(),
                            )
                            .unwrap();
                            let _ =
                                subc_transport::write_frame(&mut *writer.lock().await, &accepted)
                                    .await;
                        }
                        if matches!(
                            script,
                            Script::GatedAcceptanceThenExit
                                | Script::GatedAcceptanceThenRefused
                                | Script::GatedPreStartRefusal { .. }
                        ) {
                            let record = |ty, body: Vec<u8>| {
                                Frame::build_with_version(
                                    header.ver,
                                    ty,
                                    header.flags,
                                    header.channel,
                                    header.epoch,
                                    header.corr,
                                    body,
                                )
                                .unwrap()
                            };
                            let outcome =
                                if let Script::GatedPreStartRefusal { reason, .. } = script {
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::Unknown(reason.into()),
                                    }
                                } else if matches!(script, Script::GatedAcceptanceThenRefused) {
                                    Outcome::RefusedBeforeStart {
                                        reason: RefusalReason::Unreachable,
                                    }
                                } else {
                                    Outcome::Exit { code: 0 }
                                };
                            let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
                            if let Script::GatedPreStartRefusal { detail, .. } = script {
                                terminal.refusal_detail = detail.map(str::to_owned);
                            }
                            let terminal =
                                serde_json::to_vec(&StreamRecord::Terminal(terminal)).unwrap();
                            for frame in [
                                record(FrameType::StreamData, terminal),
                                record(FrameType::StreamEnd, vec![]),
                            ] {
                                let _ =
                                    subc_transport::write_frame(&mut *writer.lock().await, &frame)
                                        .await;
                            }
                        }
                    }));
                }
                if body["method"] == "exec.attach" {
                    if matches!(script, Script::GappedAttach | Script::GappedCancel) {
                        let writer = writer.clone();
                        let gate = server_completion_gate.clone();
                        let seq = body["params"]["from_seq"].as_u64().unwrap();
                        producers.push(tokio::spawn(async move {
                            let data = |record| {
                                Frame::build_with_version(
                                    header.ver,
                                    FrameType::StreamData,
                                    header.flags,
                                    header.channel,
                                    header.epoch,
                                    header.corr,
                                    serde_json::to_vec(&record).unwrap(),
                                )
                                .unwrap()
                            };
                            let output = data(StreamRecord::Output(Output::new(
                                seq,
                                OutputStream::Stdout,
                                BytePayload(vec![b'A' + seq as u8]),
                            )));
                            if subc_transport::write_frame(&mut *writer.lock().await, &output)
                                .await
                                .is_err()
                            {
                                return;
                            }
                            gate.notified().await;
                            if matches!(script, Script::GappedCancel) || seq == 2 {
                                let outcome = if cancelled {
                                    Outcome::Signal { signal: 15 }
                                } else {
                                    Outcome::Exit { code: 0 }
                                };
                                let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
                                if cancelled {
                                    terminal = terminal.with_killed(Killed::Cancel);
                                }
                                if subc_transport::write_frame(
                                    &mut *writer.lock().await,
                                    &data(StreamRecord::Terminal(terminal)),
                                )
                                .await
                                .is_err()
                                {
                                    return;
                                }
                            }
                            let end = Frame::build_with_version(
                                header.ver,
                                FrameType::StreamEnd,
                                header.flags,
                                header.channel,
                                header.epoch,
                                header.corr,
                                vec![],
                            )
                            .unwrap();
                            let _ =
                                subc_transport::write_frame(&mut *writer.lock().await, &end).await;
                        }));
                    }
                }
                if matches!(
                    script,
                    Script::Staged { .. }
                        | Script::GatedStages { .. }
                        | Script::DetachSafeStages { .. }
                ) && body["method"] == "exec.run"
                {
                    let writer = writer.clone();
                    let output_gate = server_output_gate.clone();
                    let completion_gate = server_completion_gate.clone();
                    producers.push(tokio::spawn(async move {
                        let data = |record| {
                            Frame::build_with_version(
                                header.ver,
                                FrameType::StreamData,
                                header.flags,
                                header.channel,
                                header.epoch,
                                header.corr,
                                serde_json::to_vec(&record).unwrap(),
                            )
                            .unwrap()
                        };
                        if let Script::Staged { queue, .. } = script {
                            tokio::time::sleep(queue).await;
                        } else {
                            output_gate.notified().await;
                        }
                        let output = data(StreamRecord::Output(Output::new(
                            0,
                            OutputStream::Stdout,
                            BytePayload(b"started\n".to_vec()),
                        )));
                        if subc_transport::write_frame(&mut *writer.lock().await, &output)
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if let Script::Staged { run, .. } = script {
                            tokio::time::sleep(run).await;
                        } else {
                            completion_gate.notified().await;
                        }
                        let terminal =
                            TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0);
                        let end = Frame::build_with_version(
                            header.ver,
                            FrameType::StreamEnd,
                            header.flags,
                            header.channel,
                            header.epoch,
                            header.corr,
                            vec![],
                        )
                        .unwrap();
                        let mut writer = writer.lock().await;
                        if subc_transport::write_frame(
                            &mut *writer,
                            &data(StreamRecord::Terminal(terminal)),
                        )
                        .await
                        .is_ok()
                        {
                            let _ = subc_transport::write_frame(&mut *writer, &end).await;
                        }
                    }));
                }
                if matches!(script, Script::Continuous) && body["method"] == "exec.run" {
                    let writer = writer.clone();
                    let stop = stop.clone();
                    producers.push(tokio::spawn(async move {
                        let mut seq = 0;
                        let mut tick = tokio::time::interval(Duration::from_millis(1));
                        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                            tick.tick().await;
                            let record = StreamRecord::Output(Output::new(
                                seq,
                                OutputStream::Stdout,
                                BytePayload(b"x".to_vec()),
                            ));
                            let frame = Frame::build_with_version(
                                header.ver,
                                FrameType::StreamData,
                                header.flags,
                                header.channel,
                                header.epoch,
                                header.corr,
                                serde_json::to_vec(&record).unwrap(),
                            )
                            .unwrap();
                            if subc_transport::write_frame(&mut *writer.lock().await, &frame)
                                .await
                                .is_err()
                            {
                                break;
                            }
                            seq += 1;
                        }
                    }));
                }
            }
            for producer in producers {
                producer.abort();
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    // Connection files contain authentication material, so their parent must
    // stay private even when the test runner uses a permissive umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let connection = dir.path().join("connection.json");
    connection_file::write_atomic(
        &connection,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port,
            }],
            key,
            daemon_id,
            pid: std::process::id(),
            daemon_ver: FAKE_DAEMON_VERSION.into(),
        },
    )
    .unwrap();
    Daemon {
        connection,
        connections,
        log,
        acceptance_gate,
        output_gate,
        completion_gate,
        requests,
        server,
        _dir: dir,
    }
}

/// A daemon whose exec-remote provider (ck-motor) goes away mid-job, the way a
/// module restart closes its routes, and stays away until the test brings it
/// back with [`OutageDaemon::restore`]. While it is away the daemon still
/// accepts connections, but its catalog lists no provider, so a caller can
/// neither open a route nor attach. Once restored, `exec.attach` replays the
/// job's records from the requested `from_seq` and ends with its terminal.
///
/// Every frame is an `exec-remote/v1` type from the published types crate, and
/// the fake does no more than ck-motor does: attach replays from `from_seq`
/// without repeating `accepted`, and nothing else reports a job's state.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct OutageDaemon {
    pub(crate) connection: std::path::PathBuf,
    pub(crate) log: Arc<Mutex<Vec<Value>>>,
    /// Connections that asked for the catalog while the provider was away.
    pub(crate) refused_connects: Arc<tokio::sync::watch::Sender<usize>>,
    /// Every authenticated connection, whether or not the provider was there.
    pub(crate) connects: Arc<tokio::sync::watch::Sender<usize>>,
    /// Catalog requests currently held by [`OutageDaemon::hold_next_connect`].
    held_connects: Arc<tokio::sync::watch::Sender<usize>>,
    hold_next: Arc<std::sync::atomic::AtomicBool>,
    release_held: Arc<tokio::sync::Notify>,
    up: Arc<std::sync::atomic::AtomicBool>,
    finish_job: Arc<tokio::sync::Notify>,
    interrupt_submission: Arc<tokio::sync::Notify>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl OutageDaemon {
    /// Bring the provider back, as when ck-motor finishes restarting.
    pub fn restore(&self) {
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Complete the running job, even if no caller is attached yet.
    pub fn finish_job(&self) {
        self.finish_job.notify_one();
    }

    #[cfg(unix)]
    pub fn interrupt_submission(&self) {
        self.interrupt_submission.notify_one();
    }

    /// Hold the next connection's catalog request unanswered until
    /// [`OutageDaemon::release_held_connect`], so a test can act while the
    /// reconnecting client waits for its catalog reply. The held reply reports
    /// the provider as it is at release time.
    pub fn hold_next_connect(&self) {
        self.hold_next
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Wait until a reconnect is held by [`OutageDaemon::hold_next_connect`].
    pub async fn after_held_connect(&self) {
        let mut held = self.held_connects.subscribe();
        tokio::time::timeout(Duration::from_secs(30), held.wait_for(|n| *n >= 1))
            .await
            .expect("the caller must reconnect into the held catalog request")
            .unwrap();
    }

    /// Answer the catalog request held by [`OutageDaemon::hold_next_connect`].
    pub fn release_held_connect(&self) {
        self.release_held.notify_one();
    }

    /// Wait until at least `count` connections found the provider away.
    pub async fn after_refused_connects(&self, count: usize) {
        let mut seen = self.refused_connects.subscribe();
        tokio::time::timeout(Duration::from_secs(30), seen.wait_for(|n| *n >= count))
            .await
            .expect("the caller must keep reconnecting while the runner is away")
            .unwrap();
    }

    pub fn methods(&self, method: &str) -> Vec<Value> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|body| body["method"] == method)
            .cloned()
            .collect()
    }
}

impl Drop for OutageDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The job's records, in sequence order, as ck-motor retains them for replay.
#[cfg_attr(not(unix), allow(dead_code))]
fn outage_job_records() -> Vec<StreamRecord> {
    vec![
        StreamRecord::Started(Started::new(0, 0, 1)),
        StreamRecord::Output(Output::new(
            1,
            OutputStream::Stdout,
            BytePayload(b"A".to_vec()),
        )),
        StreamRecord::Output(Output::new(
            2,
            OutputStream::Stdout,
            BytePayload(b"B".to_vec()),
        )),
    ]
}

#[cfg_attr(not(unix), allow(dead_code))]
pub async fn outage_daemon() -> OutageDaemon {
    outage_daemon_with(true, OutageReplay::Finished, None).await
}

/// The outage daemon whose accepted job is still queued (no started or output
/// records yet) when the provider restarts.
#[cfg(unix)]
pub async fn queued_outage_daemon() -> OutageDaemon {
    outage_daemon_with(false, OutageReplay::Finished, None).await
}

/// The only keyed recovery answers used here are published exec-remote records.
/// A never-accepted key stays refused; only a new key can admit a fresh job.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
pub enum SubmissionOutage {
    Accepted,
    NeverAccepted,
    Ambiguous,
    Repeated,
}

#[cfg(unix)]
pub async fn preaccepted_outage_daemon(answer: SubmissionOutage) -> OutageDaemon {
    outage_daemon_with(false, OutageReplay::Finished, Some(answer)).await
}

#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
pub enum OutageReplay {
    Running,
    Queued,
    QueuedWire(&'static [u8]),
    Silent,
    Finished,
    NotFound,
    Unknown,
    Close,
}

#[cfg_attr(not(unix), allow(dead_code))]
pub async fn outage_daemon_with_replay(replay: OutageReplay) -> OutageDaemon {
    outage_daemon_with(true, replay, None).await
}

/// `started` controls whether the job has started (and printed) before the
/// provider restarts; `replay` controls what re-attaching by job id returns.
async fn outage_daemon_with(
    started: bool,
    replay: OutageReplay,
    submission_answer: Option<SubmissionOutage>,
) -> OutageDaemon {
    use std::sync::atomic::{AtomicBool, Ordering};
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let log = Arc::new(Mutex::new(Vec::new()));
    let refused_connects = Arc::new(tokio::sync::watch::channel(0).0);
    let connects = Arc::new(tokio::sync::watch::channel(0).0);
    let held_connects = Arc::new(tokio::sync::watch::channel(0).0);
    let hold_next = Arc::new(AtomicBool::new(false));
    let release_held = Arc::new(tokio::sync::Notify::new());
    let up = Arc::new(AtomicBool::new(true));
    let finish_job = Arc::new(tokio::sync::Notify::new());
    let interrupt_submission = Arc::new(tokio::sync::Notify::new());
    let server_interrupt_submission = interrupt_submission.clone();
    let submission = Arc::new(Mutex::new((None::<Value>, false, 0)));
    let server_finish_job = finish_job.clone();
    let (server_log, server_refused, server_up, server_key) = (
        log.clone(),
        refused_connects.clone(),
        up.clone(),
        key.clone(),
    );
    let (server_connects, server_held, server_hold_next, server_release) = (
        connects.clone(),
        held_connects.clone(),
        hold_next.clone(),
        release_held.clone(),
    );
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (log, refused, up, key) = (
                server_log.clone(),
                server_refused.clone(),
                server_up.clone(),
                server_key.clone(),
            );
            let (connects, held, hold_next, release) = (
                server_connects.clone(),
                server_held.clone(),
                server_hold_next.clone(),
                server_release.clone(),
            );
            let finish_job = server_finish_job.clone();
            let interrupt_submission = server_interrupt_submission.clone();
            let submission = submission.clone();
            connections.spawn(async move {
                if subc_transport::authenticate_server(
                    &mut socket,
                    &key,
                    &daemon_id,
                    FAKE_DAEMON_VERSION,
                    Duration::from_secs(5),
                )
                .await
                .is_err()
                {
                    return;
                }
                connects.send_modify(|count| *count += 1);
                let (mut reader, mut writer) = tokio::io::split(socket);
                while let Ok(Some(frame)) = subc_transport::read_frame(&mut reader).await {
                    if let Some(reply) = aft_base::subc_fake::server_describe_reply(&frame) {
                        if subc_transport::write_frame(&mut writer, &reply).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    let header = frame.header;

                    let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                    let frame_of = |ty, bytes: Vec<u8>| {
                        Frame::build_with_version(
                            header.ver,
                            ty,
                            header.flags,
                            header.channel,
                            header.epoch,
                            header.corr,
                            bytes,
                        )
                        .unwrap()
                    };
                    let reply = |ty, value: Value| frame_of(ty, serde_json::to_vec(&value).unwrap());
                    let record = |record: &StreamRecord| {
                        frame_of(FrameType::StreamData, serde_json::to_vec(record).unwrap())
                    };
                    let mut replies = Vec::new();
                    let mut drop_route = false;
                    let mut hold_submission = false;
                    match header.ty {
                        FrameType::Hello => replies.push(reply(
                            FrameType::HelloAck,
                            serde_json::to_value(ModuleHelloAckBody {
                                negotiated_ver: PROTOCOL_VERSION,
                                subc_ops: vec!["catalog.list".into()],
                                subc_capabilities: vec![],
                                storage: None,
                                machine_id: None,
                            })
                            .unwrap(),
                        )),
                        FrameType::Request if header.channel == 0 => match body["op"].as_str() {
                            Some("catalog.list") => {
                                if hold_next.swap(false, Ordering::SeqCst) {
                                    held.send_modify(|count| *count += 1);
                                    release.notified().await;
                                    held.send_modify(|count| *count -= 1);
                                }
                                // A provider that is restarting is absent from the catalog.
                                let modules = if up.load(Ordering::SeqCst) {
                                    let mut claims = vec!["exec-remote/v1", "exec-remote-submission-key/v1"];
                                    if matches!(submission_answer, Some(SubmissionOutage::Repeated)) {
                                        claims.push(PROGRESS_CAPABILITY);
                                    }
                                    json!([{"module_id":"executor-picked-by-capability", "roles":[], "control_ops":[], "capabilities":{"provides":claims, "requires":[]}}])
                                } else {
                                    refused.send_modify(|count| *count += 1);
                                    json!([])
                                };
                                replies.push(reply(FrameType::Response, json!({"op":"catalog.list", "generation":1, "subc_ops":["catalog.list"], "modules":modules})));
                            }
                            Some("route.open") => replies.push(reply(
                                FrameType::Response,
                                json!({"op":"route.open", "route_channel":40, "route_epoch":1}),
                            )),
                            // subc-client 0.40 probes server.describe before it opens a
                    // route; this fake advertises nothing beyond the old protocol.
                    Some("server.describe") => replies.push(reply(
                        FrameType::Response,
                        json!({"op":"server.describe", "protocol_ver":2, "subc_ops":[], "capabilities":[], "connected_clients":1}),
                    )),
                    _ => panic!("unexpected control request {body}"),
                        },
                        FrameType::Request => {
                            log.lock().unwrap().push(body.clone());
                            match body["method"].as_str().unwrap() {
                                "exec.run" => {
                                    if let Some(answer) = submission_answer {
                                        let mut saved = submission.lock().unwrap();
                                        let request = &body["params"];
                                        assert!(request["submission_key"].is_string());
                                        if let Some(original) = &saved.0 {
                                            if request["submission_key"] == original["submission_key"] {
                                                assert_eq!(request, original, "keyed recovery must repeat the frozen request");
                                                saved.2 += 1;
                                                match answer {
                                                    SubmissionOutage::Accepted => {
                                                        // Recovery identifies the existing job. Its records
                                                        // are available only through exec.attach.
                                                        replies.push(record(&StreamRecord::Accepted(Accepted::new(id(), 1).with_attached(true))));
                                                    }
                                                    SubmissionOutage::NeverAccepted => {
                                                        replies.push(record(&StreamRecord::Terminal(TerminalRecord::new(
                                                            Uuid::from_bytes([0x77; 16]),
                                                            Outcome::RefusedBeforeStart { reason: RefusalReason::SubmissionNeverAccepted },
                                                            0, 0, 0,
                                                        ))));
                                                    }
                                                    SubmissionOutage::Ambiguous => {
                                                        replies.push(record(&StreamRecord::Terminal(TerminalRecord::new(id(), Outcome::OutcomeUnknown, 0, 0, 0))));
                                                    }
                                                    SubmissionOutage::Repeated if saved.2 == 1 => {
                                                        // A recovered subscription can still be waiting for
                                                        // placement, without a runner-assigned job ID.
                                                        assert_eq!(request["progress"], true);
                                                        replies.push(record(&StreamRecord::Progress(Progress::new(ProgressPhase::PlacementWait, 0, None).unwrap())));
                                                        hold_submission = true;
                                                    }
                                                    SubmissionOutage::Repeated => {
                                                        replies.push(record(&StreamRecord::Accepted(Accepted::new(id(), 1).with_attached(true))));
                                                    }
                                                }
                                            } else {
                                                assert!(matches!(answer, SubmissionOutage::NeverAccepted));
                                                assert!(!saved.1, "a never-accepted proof permits only one fresh submission");
                                                let mut original = original.clone();
                                                original["submission_key"] = request["submission_key"].clone();
                                                assert_eq!(*request, original);
                                                saved.1 = true;
                                                replies.push(record(&StreamRecord::Accepted(Accepted::new(id(), 1))));
                                                for job_record in outage_job_records() {
                                                    replies.push(record(&job_record));
                                                }
                                                replies.push(record(&StreamRecord::Terminal(TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0).with_ran(Ran::Remote))));
                                            }
                                            if !hold_submission {
                                                replies.push(frame_of(FrameType::StreamEnd, vec![]));
                                            }
                                        } else {
                                            saved.0 = Some(request.clone());
                                            // The request reached the executor provider, but its caller saw no
                                            // Accepted before the provider and its route went away.
                                            replies.push(reply(FrameType::Error, json!({"code":"outcome_unknown", "message":"route closed by subc"})));
                                            up.store(false, Ordering::SeqCst);
                                            drop_route = true;
                                        }
                                    } else {
                                    replies.push(record(&StreamRecord::Accepted(Accepted::new(
                                        id(),
                                        1,
                                    ))));
                                    if started {
                                        for job_record in &outage_job_records()[..2] {
                                            replies.push(record(job_record));
                                        }
                                    }
                                    // The provider restarts while the job is still queued or
                                    // running: its route closes, and the provider stays
                                    // unavailable until restored.
                                    replies.push(reply(FrameType::Error, json!({"code":"outcome_unknown", "message":"route closed by subc"})));
                                    up.store(false, Ordering::SeqCst);
                                    drop_route = true;
                                    }
                                }
                                "exec.attach" => {
                                    assert_eq!(body["params"]["job_id"], id().to_string());
                                    if matches!(replay, OutageReplay::Close) {
                                        return;
                                    }
                                    if matches!(replay, OutageReplay::Unknown) {
                                        let terminal = TerminalRecord::new(id(), Outcome::OutcomeUnknown, 0, 0, 0);
                                        replies.push(record(&StreamRecord::Terminal(terminal)));
                                        replies.push(frame_of(FrameType::StreamEnd, vec![]));
                                    } else if matches!(replay, OutageReplay::NotFound) {
                                        replies.push(reply(FrameType::Error, json!({"code":"job_not_found", "message":"job not found"})));
                                    } else if matches!(replay, OutageReplay::Queued | OutageReplay::QueuedWire(_) | OutageReplay::Silent) {
                                        // Attach need not replay Accepted. A queued job may send
                                        // only display progress, or stay quiet until it finishes.
                                        if matches!(replay, OutageReplay::Queued) {
                                            replies.push(record(&StreamRecord::Progress(
                                                Progress::new(ProgressPhase::Unknown("queue_wait".into()), 42_000, None).unwrap().with_job_id(id())
                                            )));
                                        }
                                        if let OutageReplay::QueuedWire(bytes) = replay {
                                            replies.push(frame_of(FrameType::StreamData, bytes.to_vec()));
                                        }
                                    } else {
                                        let from = body["params"]["from_seq"].as_u64().unwrap();
                                        for job_record in outage_job_records() {
                                            let seq = match &job_record {
                                                StreamRecord::Started(started) => started.seq,
                                                StreamRecord::Output(output) => output.seq,
                                                _ => unreachable!(),
                                            };
                                            if seq >= from {
                                                replies.push(record(&job_record));
                                            }
                                        }
                                        if !matches!(replay, OutageReplay::Running) {
                                            let terminal =
                                                TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0)
                                                    .with_ran(Ran::Remote);
                                            replies.push(record(&StreamRecord::Terminal(terminal)));
                                            replies.push(frame_of(FrameType::StreamEnd, vec![]));
                                        }
                                    }
                                }
                                "exec.cancel" => replies.push(reply(
                                    FrameType::Response,
                                    serde_json::to_value(CancelReply::new(id())).unwrap(),
                                )),
                                _ => panic!("unexpected caller operation {body}"),
                            }
                        }
                        _ => {}
                    }
                    for response in replies {
                        if subc_transport::write_frame(&mut writer, &response).await.is_err() {
                            return;
                        }
                    }
                    if hold_submission {
                        interrupt_submission.notified().await;
                        up.store(false, Ordering::SeqCst);
                        let error = reply(FrameType::Error, json!({"code":"outcome_unknown", "message":"route closed by subc"}));
                        let _ = subc_transport::write_frame(&mut writer, &error).await;
                        return;
                    }
                    if body["method"] == "exec.attach" && matches!(replay, OutageReplay::Running | OutageReplay::Queued | OutageReplay::QueuedWire(_) | OutageReplay::Silent) {
                        finish_job.notified().await;
                        let terminal = TerminalRecord::new(id(), Outcome::Exit { code: 0 }, 1, 0, 0)
                            .with_ran(Ran::Remote);
                        for frame in [record(&StreamRecord::Terminal(terminal)), frame_of(FrameType::StreamEnd, vec![])] {
                            if subc_transport::write_frame(&mut writer, &frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    if drop_route {
                        return;
                    }
                }
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    // Connection files contain authentication material, so their parent must
    // stay private even when the test runner uses a permissive umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let connection = dir.path().join("connection.json");
    connection_file::write_atomic(
        &connection,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port,
            }],
            key,
            daemon_id,
            pid: std::process::id(),
            daemon_ver: FAKE_DAEMON_VERSION.into(),
        },
    )
    .unwrap();
    OutageDaemon {
        connection,
        log,
        refused_connects,
        connects,
        held_connects,
        hold_next,
        release_held,
        up,
        finish_job,
        interrupt_submission,
        server,
        _dir: dir,
    }
}
