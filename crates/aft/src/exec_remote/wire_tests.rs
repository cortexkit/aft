//! A minimal authenticated daemon fixture: no execution, local fallback,
//! snapshots, or runner framing. It can lose accepted jobs and return unknown
//! refusal reasons, just like the executor's caller contract permits.
use super::{tests::MemorySink, types::*, *};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::{BindIdentity, Frame, FrameType, ModuleHelloAckBody, PROTOCOL_VERSION};

pub(crate) fn id() -> Uuid {
    "0192a64a-1234-7000-8000-000000000001".parse().unwrap()
}

/// The terminal record of a published `crate-local-server-reports-*` vector
/// (see `fixtures/SOURCE.md`), after checking its bytes against the published
/// SHA-256. Its job ID is [`id`].
pub(crate) fn report_vector(name: &str) -> TerminalRecord {
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

// Most scripts drive the Unix-only remote bash tests.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy)]
pub(crate) enum Script {
    Lost,
    Refused,
    Restart,
    Cancel,
    MissingTerminal,
    KnownRefused,
    WorkspaceSetupRefused,
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
    GappedAttach(Duration),
    GappedCancel(Duration),
    AttachDisconnected,
    /// Accepted, then exited 0 with the published `all` report: changed files,
    /// changed Git state, untracked files and ignored writes.
    Reported,
    /// Accepted, then exited 0 with a terminal record that reports nothing
    /// about the workspace (no changed-file list).
    Plain,
}

pub(crate) struct Daemon {
    pub(crate) connection: std::path::PathBuf,
    pub(crate) log: Arc<Mutex<Vec<(subc_protocol::EnvelopeHeader, Value)>>>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub(crate) async fn daemon(script: Script, claim: &str) -> Daemon {
    daemon_with_clients(script, claim, 1).await
}

pub(crate) async fn daemon_with_clients(script: Script, claim: &str, clients: usize) -> Daemon {
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    let log = Arc::new(Mutex::new(Vec::new()));
    let server_log = Arc::clone(&log);
    let server_key = key.clone();
    let claim = claim.to_string();
    let server = tokio::spawn(async move {
        for _ in 0..clients {
            let (mut socket, _) = listener.accept().await.unwrap();
            subc_transport::authenticate_server(
                &mut socket,
                &server_key,
                &daemon_id,
                "exec-client-test",
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
                let header = frame.header;
                let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                server_log.lock().unwrap().push((header, body.clone()));
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
                        "modules":[{"module_id":"executor-picked-by-capability", "roles":[], "control_ops":[], "capabilities":{"provides":[claim], "requires":[]}}]}))),
                    Some("route.open") => replies.push(reply(FrameType::Response, json!({"op":"route.open", "route_channel":40, "route_epoch":1}))),
                    _ => panic!("unexpected control request {body}"),
                },
                FrameType::Request => {
                    match body["method"].as_str().unwrap() {
                        "exec.run" | "exec.attach" => {
                            let attaching = body["method"] == "exec.attach";
                            if attaching && matches!(script, Script::AttachDisconnected) {
                                // Drop the route and listener; later attach calls and connects fail.
                                return;
                            }
                            if !attaching && !matches!(script, Script::Refused | Script::KnownRefused | Script::WorkspaceSetupRefused) {
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                            }
                             let outcome = if matches!(script, Script::PersistentTerminalGap | Script::TransientTerminalGap | Script::TerminalGapOnce) { Outcome::Exit { code: 7 } }
                                 else if attaching && matches!(script,Script::AttachRefused) { Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown("future_refusal".into()) } }
                                else if attaching && cancelled { Outcome::Signal { signal: 15 } }
                                else { match script {
                                    Script::Lost => Outcome::OutcomeUnknown,
                                    Script::Refused => Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown("future_refusal".into()) },
                                    Script::KnownRefused => Outcome::RefusedBeforeStart { reason: RefusalReason::Unreachable },
                                    Script::WorkspaceSetupRefused => Outcome::RefusedBeforeStart { reason: RefusalReason::WorkspaceSetupFailed },
                                    Script::FutureOutcome => Outcome::Unknown { kind:"future_outcome".into() },
                                    Script::Expired => Outcome::HistoryExpired,
                                    _ => Outcome::Exit { code: 0 },
                                }};
                            if matches!(script, Script::Cancel | Script::Continuous | Script::GappedCancel(_)) && !attaching { /* accepted, still running */ }
                            else if attaching && matches!(script, Script::GappedAttach(_) | Script::GappedCancel(_)) { /* delayed producer below */ }
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
                                 if !matches!(script, Script::MissingTerminal) && !(attaching && matches!(script, Script::TerminalGapOnce)) && (!matches!(script, Script::Restart | Script::AttachRefused | Script::GappedAttach(_) | Script::AttachDisconnected) || attaching) {
                                    let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
                                     if cancelled && !matches!(script, Script::PersistentTerminalGap | Script::TransientTerminalGap | Script::TerminalGapOnce) { terminal = terminal.with_killed(Killed::Cancel); }
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
                        },
                        "exec.cancel" => { cancelled = true; stop.store(true,std::sync::atomic::Ordering::SeqCst); replies.push(reply(FrameType::Response, serde_json::to_value(CancelReply::new(id())).unwrap())); },
                        "exec.status" => replies.push(reply(FrameType::Response, serde_json::to_value(StatusReply::new(0, vec![], true, vec![], "rustc test")).unwrap())),
                        _ => panic!("unexpected caller operation {body}"),
                    }
                },
                FrameType::Cancel => {cancelled = true; stop.store(true,std::sync::atomic::Ordering::SeqCst);},
                _ => {},
            }
                for response in replies {
                    subc_transport::write_frame(&mut *writer.lock().await, &response)
                        .await
                        .unwrap();
                }
                if body["method"] == "exec.attach" {
                    if let Script::GappedAttach(gap) | Script::GappedCancel(gap) = script {
                        let writer = writer.clone();
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
                            tokio::time::sleep(gap).await;
                            if matches!(script, Script::GappedCancel(_)) || seq == 2 {
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
            daemon_ver: "exec-client-test".into(),
        },
    )
    .unwrap();
    Daemon {
        connection,
        log,
        server,
        _dir: dir,
    }
}

async fn client(daemon: &Daemon) -> ExecRemoteClient {
    ExecRemoteClient::connect(
        &daemon.connection,
        BindIdentity::new("/workspace", "aft-test", "session"),
    )
    .await
    .unwrap()
}

async fn drain(stream: &mut RemoteStream, sink: &mut MemorySink) -> Result<Verdict, Error> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.next(sink).await? {
                StreamProgress::Record => {}
                StreamProgress::Complete(verdict) => return Ok(verdict),
            }
        }
    })
    .await
    .expect("the fixture stream must terminate")
}

#[tokio::test]
async fn daemon_route_uses_hyphenated_capability_and_dotted_operations() {
    for (script, expected) in [
        (Script::Lost, Verdict::OutcomeUnknown),
        (
            Script::Refused,
            Verdict::RunLocally {
                reason: RefusalReason::Unknown("future_refusal".into()),
            },
        ),
    ] {
        let daemon = daemon(script, client::CAPABILITY).await;
        let client = client(&daemon).await;
        let mut stream = client
            .run(&RunRequest::new(
                "/workspace/task",
                "/src/repo",
                "/workspace/task",
                "cargo test",
            ))
            .await
            .unwrap();
        assert_eq!(
            drain(&mut stream, &mut MemorySink::default())
                .await
                .unwrap(),
            expected
        );
        assert!(client.status().await.unwrap().server_reachable);
        assert_eq!(client.cancel_job(id()).await.unwrap().job_id, id());
        let log = daemon.log.lock().unwrap();
        let open = log
            .iter()
            .find(|(_, body)| body["op"] == "route.open")
            .unwrap();
        assert_eq!(
            open.1["target"],
            serde_json::to_value(subc_protocol::RouteTarget::ManagementSurface {
                module_id: "executor-picked-by-capability".into()
            })
            .unwrap()
        );
        let operations: Vec<_> = log
            .iter()
            .filter_map(|(_, body)| body["method"].as_str())
            .collect();
        assert_eq!(operations, ["exec.run", "exec.status", "exec.cancel"]);
    }
}

#[tokio::test]
async fn dotted_capability_is_not_a_discovery_claim() {
    let daemon = daemon(Script::Lost, "exec.remote/v1").await;
    let result = ExecRemoteClient::connect(
        &daemon.connection,
        BindIdentity::new("/workspace", "aft-test", "session"),
    )
    .await;
    assert!(matches!(
        result,
        Err(Error::Transport(
            subc_client_rs::CallError::CapabilityUnprovided { .. }
        ))
    ));
    assert!(daemon
        .log
        .lock()
        .unwrap()
        .iter()
        .all(|(_, body)| body["op"] != "route.open"));
}

#[tokio::test]
async fn interrupted_call_reattaches_with_next_durable_seq_without_resubmission() {
    let daemon = daemon(Script::Restart, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("missing terminal requires recovery")
    };
    assert_eq!(point.last_seq, Some(1));
    let mut resumed = client.attach(point).await.unwrap();
    assert_eq!(
        drain(&mut resumed, &mut sink).await.unwrap(),
        Verdict::Exited { code: 0 }
    );
    assert_eq!(sink.stdout, b"ABCD");
    let log = daemon.log.lock().unwrap();
    let attach = log
        .iter()
        .find(|(_, body)| body["method"] == "exec.attach")
        .unwrap();
    assert_eq!(attach.1["params"]["from_seq"], 2);
    assert_eq!(
        log.iter()
            .filter(|(_, body)| body["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn persistent_terminal_gap_completes_after_three_attaches() {
    let daemon = daemon(Script::PersistentTerminalGap, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let verdict = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match drain(&mut stream, &mut sink).await {
                Ok(verdict) => return verdict,
                Err(Error::RecoveryRequired {
                    resume: Some(point),
                    ..
                }) => {
                    stream = client.attach(point).await.unwrap();
                }
                other => panic!("unexpected gap recovery: {other:?}"),
            }
        }
    })
    .await
    .expect("a repeated terminal gap must not reattach forever");
    assert_eq!(verdict, Verdict::Exited { code: 7 });
    assert_eq!(sink.stdout, b"BC");
    assert_eq!(sink.seqs, [1, 2]);
    assert_eq!(sink.truncations, [1]);
    let log = daemon.log.lock().unwrap();
    let from: Vec<_> = log
        .iter()
        .filter(|(_, b)| b["method"] == "exec.attach")
        .map(|(_, b)| b["params"]["from_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(from, [0, 0, 0]);
    assert_eq!(
        log.iter()
            .filter(|(_, b)| b["method"] == "exec.run")
            .count(),
        1
    );
}

#[tokio::test]
async fn transient_terminal_gap_fills_on_next_attach_without_truncation() {
    let daemon = daemon(Script::TransientTerminalGap, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut sink = MemorySink::default();
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let Err(Error::RecoveryRequired {
        resume: Some(point),
        ..
    }) = drain(&mut stream, &mut sink).await
    else {
        panic!("the first stream must have a gap");
    };
    assert!(sink.stdout.is_empty());
    let mut resumed = client.attach(point).await.unwrap();
    assert_eq!(
        drain(&mut resumed, &mut sink).await.unwrap(),
        Verdict::Exited { code: 7 }
    );
    assert_eq!(sink.stdout, b"ABC");
    assert_eq!(sink.seqs, [0, 1, 2]);
    assert!(sink.truncations.is_empty());
}

#[tokio::test]
async fn active_cancel_uses_call_key_then_attach_observes_cancel_not_deadline() {
    let daemon = daemon(Script::Cancel, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let mut sink = MemorySink::default();
    assert_eq!(
        stream.next(&mut sink).await.unwrap(),
        StreamProgress::Record
    );
    let mut cancelled = client.cancel(&stream).await.unwrap();
    assert_eq!(
        drain(&mut cancelled, &mut sink).await.unwrap(),
        Verdict::CancelKilled
    );
    let log = daemon.log.lock().unwrap();
    let run = log
        .iter()
        .find(|(_, body)| body["method"] == "exec.run")
        .unwrap()
        .0;
    let cancel = log
        .iter()
        .find(|(header, _)| header.ty == FrameType::Cancel)
        .unwrap()
        .0;
    assert_eq!(
        (cancel.channel, cancel.epoch, cancel.corr),
        (run.channel, run.epoch, run.corr)
    );
}

#[tokio::test]
async fn end_without_terminal_returns_recovery_promptly() {
    let daemon = daemon(Script::MissingTerminal, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    assert!(matches!(
        drain(&mut stream, &mut MemorySink::default()).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(client.status().await.unwrap().server_reachable);
}

#[tokio::test]
async fn empty_attach_reports_no_frames_and_keeps_accepted_resume_point() {
    let daemon = daemon(Script::MissingTerminal, client::CAPABILITY).await;
    let client = client(&daemon).await;
    let mut stream = client
        .run(&RunRequest::new(
            "/workspace/task",
            "/src/repo",
            "/workspace/task",
            "cargo test",
        ))
        .await
        .unwrap();
    let mut sink = MemorySink::default();
    assert!(!stream.received_frame());
    assert!(matches!(
        drain(&mut stream, &mut sink).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(
        stream.received_frame(),
        "acceptance is a frame even without output"
    );
    let point = stream.resume_point().unwrap();
    assert_eq!(point.job_id, id());
    let mut attached = client.attach(point.clone()).await.unwrap();
    assert!(matches!(
        drain(&mut attached, &mut sink).await,
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(
        !attached.received_frame(),
        "StreamEnd alone must not reset the empty-attach budget"
    );
    assert_eq!(attached.resume_point(), Some(point));
}
