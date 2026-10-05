//! A minimal authenticated daemon fixture: no execution, local fallback,
//! snapshots, or runner framing. It can lose accepted jobs and return unknown
//! refusal reasons, just like the executor's caller contract permits.
use super::{tests::MemorySink, types::*, *};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::{BindIdentity, Frame, FrameType, ModuleHelloAckBody, PROTOCOL_VERSION};

fn id() -> Uuid {
    "0192a64a-1234-7000-8000-000000000001".parse().unwrap()
}

#[derive(Clone, Copy)]
enum Script {
    Lost,
    Refused,
    Restart,
    Cancel,
    MissingTerminal,
}

struct Daemon {
    connection: std::path::PathBuf,
    log: Arc<Mutex<Vec<(subc_protocol::EnvelopeHeader, Value)>>>,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn daemon(script: Script, claim: &str) -> Daemon {
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
        let mut cancelled = false;
        while let Ok(Some(frame)) = subc_transport::read_frame(&mut socket).await {
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
                    Some("catalog.list") => replies.push(reply(FrameType::Response, json!({"op":"catalog.list", "generation":1,
                        "modules":[{"module_id":"executor-picked-by-capability", "roles":[], "control_ops":[], "capabilities":{"provides":[claim], "requires":[]}}]}))),
                    Some("route.open") => replies.push(reply(FrameType::Response, json!({"op":"route.open", "route_channel":40, "route_epoch":1}))),
                    _ => panic!("unexpected control request {body}"),
                },
                FrameType::Request => {
                    match body["method"].as_str().unwrap() {
                        "exec.run" | "exec.attach" => {
                            let attaching = body["method"] == "exec.attach";
                            if !attaching && !matches!(script, Script::Refused) {
                                replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Accepted(Accepted::new(id(), 1))).unwrap()));
                            }
                            let outcome = if attaching && cancelled { Outcome::Signal { signal: 15 } }
                                else { match script {
                                    Script::Lost => Outcome::OutcomeUnknown,
                                    Script::Refused => Outcome::RefusedBeforeStart { reason: RefusalReason::Unknown("future_refusal".into()) },
                                    _ => Outcome::Exit { code: 0 },
                                }};
                            if matches!(script, Script::Cancel) && !attaching { /* accepted, still running */ }
                            else {
                                if matches!(script, Script::Restart) {
                                    let from = if attaching { body["params"]["from_seq"].as_u64().unwrap() } else { 0 };
                                    let end = if attaching { 4 } else { 2 };
                                    for seq in from..end {
                                        replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Output(Output::new(seq, OutputStream::Stdout, BytePayload(vec![b'A' + seq as u8])))).unwrap()));
                                    }
                                }
                                if !matches!(script, Script::MissingTerminal) && (!matches!(script, Script::Restart) || attaching) {
                                    let mut terminal = TerminalRecord::new(id(), outcome, 1, 0, 0);
                                    if cancelled { terminal = terminal.with_killed(Killed::Cancel); }
                                    replies.push(reply(FrameType::StreamData, serde_json::to_value(StreamRecord::Terminal(terminal)).unwrap()));
                                }
                                replies.push(Frame::build_with_version(header.ver, FrameType::StreamEnd, header.flags, header.channel, header.epoch, header.corr, vec![]).unwrap());
                            }
                        },
                        "exec.cancel" => { cancelled = true; replies.push(reply(FrameType::Response, serde_json::to_value(CancelReply::new(id())).unwrap())); },
                        "exec.status" => replies.push(reply(FrameType::Response, serde_json::to_value(StatusReply::new(0, vec![], true, vec![], "rustc test")).unwrap())),
                        _ => panic!("unexpected caller operation {body}"),
                    }
                },
                FrameType::Cancel => cancelled = true,
                _ => {},
            }
            for response in replies {
                subc_transport::write_frame(&mut socket, &response)
                    .await
                    .unwrap();
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
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
