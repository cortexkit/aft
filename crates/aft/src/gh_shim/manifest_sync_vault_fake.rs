//! Only the three AFT operations, using the pinned vault response shapes.

use super::*;
use serde_json::json;
use std::sync::Arc;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

#[derive(Default)]
struct State {
    methods: Vec<String>,
    reports: Vec<Value>,
    watch_rounds: usize,
    unknown_method: bool,
    report_unknown_once: bool,
    digest_mismatch: bool,
}

struct Vault {
    _directory: tempfile::TempDir,
    path: PathBuf,
    state: Arc<Mutex<State>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Vault {
    fn start(state: State) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "0.39.0".into(),
            },
        )
        .unwrap();
        let state = Arc::new(Mutex::new(state));
        let served = Arc::clone(&state);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        Some(result) = tasks.join_next(), if !tasks.is_empty() => result.unwrap(),
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            tasks.spawn(serve(stream, key.clone(), daemon_id, Arc::clone(&served)));
                        }
                    }
                }
                tasks.abort_all();
            });
        });
        Self {
            _directory: directory,
            path,
            state,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    state: Arc<Mutex<State>>,
) {
    if subc_transport::authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "0.39.0",
        Duration::from_secs(2),
    )
    .await
    .is_err()
    {
        return;
    }
    let samples = samples();
    while let Ok(Some(request)) = subc_transport::read_frame(&mut stream).await {
        if request.header.ty == FrameType::Ping {
            let pong = Frame::build(
                FrameType::Pong,
                Flags::new(false, Priority::Interactive, false),
                0,
                0,
                request.header.corr,
                Vec::new(),
            )
            .unwrap();
            if subc_transport::write_frame(&mut stream, &pong)
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        if request.header.ty != FrameType::Request {
            continue;
        }
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        let response = |ty, body: Value| {
            Frame::build(
                ty,
                request.header.flags,
                request.header.channel,
                request.header.epoch,
                request.header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap()
        };
        let replies = {
            let mut state = state.lock().unwrap();
            if value["op"] == "route.open" {
                assert_eq!(
                    value["target"],
                    json!({"kind":"management_surface","module_id":"claustrum"})
                );
                assert_eq!(value["consumer_identity"]["module_id"], "aft");
                assert_eq!(
                    value["consumer_identity"]["launch_nonce"],
                    "manifest-sync-test-nonce"
                );
                assert!(value.get("scope").is_none_or(Value::is_null));
                vec![response(
                    FrameType::Response,
                    json!({"op":"route.open","route_channel":7,"route_epoch":1}),
                )]
            } else if value["op"] == "route.close" {
                vec![response(FrameType::Response, json!({"op":"route.close"}))]
            } else {
                let method = value["method"].as_str().unwrap();
                state.methods.push(method.into());
                if state.unknown_method {
                    vec![response(
                        FrameType::Error,
                        json!({"code":"unknown_method","message":"unknown method"}),
                    )]
                } else {
                    match method {
                        "manifest.latest" => {
                            assert_eq!(value["params"], json!({}));
                            vec![response(
                                FrameType::Response,
                                samples["latest.reply"].clone(),
                            )]
                        }
                        "manifest.report_active" => {
                            let report: Report =
                                serde_json::from_value(value["params"].clone()).unwrap();
                            assert_eq!(report.version, 29);
                            assert_eq!(report.payload_sha256.len(), 64);
                            assert!(report
                                .reason
                                .as_ref()
                                .is_none_or(|reason| reason.chars().count() <= 512));
                            state.reports.push(value["params"].clone());
                            let reply = if state.report_unknown_once && state.reports.len() == 1 {
                                samples["refusal.home_unavailable"].clone()
                            } else {
                                samples["report.reply"].clone()
                            };
                            vec![response(FrameType::Response, reply)]
                        }
                        "manifest.watch" => {
                            assert_eq!(value["params"], json!({}));
                            state.watch_rounds += 1;
                            let mut notice = samples["watch.stored"].clone();
                            if state.digest_mismatch {
                                notice["manifest_sha256"] = json!("0".repeat(64));
                            }
                            let mut replies = vec![response(FrameType::StreamData, notice.clone())];
                            if state.watch_rounds == 1 && !state.digest_mismatch {
                                notice["seq"] = json!(3);
                                replies.push(response(FrameType::StreamData, notice.clone()));
                                notice["seq"] = json!(4);
                                notice["epoch"] = json!("ffeeddccbbaa99887766554433221100");
                                replies.push(response(FrameType::StreamData, notice));
                                replies.push(
                                    Frame::build(
                                        FrameType::StreamEnd,
                                        request.header.flags,
                                        request.header.channel,
                                        request.header.epoch,
                                        request.header.corr,
                                        vec![],
                                    )
                                    .unwrap(),
                                );
                            }
                            replies
                        }
                        _ => panic!("fake received operation AFT is not admitted to call"),
                    }
                }
            }
        };
        for reply in replies {
            if subc_transport::write_frame(&mut stream, &reply)
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

async fn until(state: &Mutex<State>, predicate: impl Fn(&State) -> bool) {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if predicate(&state.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("sync did not reach expected state");
}

#[tokio::test]
async fn manifest_sync_watch_seq_gap_epoch_change_and_stream_end_read_latest_again() {
    let vault = Vault::start(State::default());
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    tokio::select! {
        _ = run_with(paths, || ManagementSession::for_test(&vault.path)) => panic!("sync stopped"),
        _ = until(&vault.state, |state| state.watch_rounds == 2) => {}
    }
    let state = vault.state.lock().unwrap();
    assert_eq!(
        state
            .methods
            .iter()
            .filter(|method| *method == "manifest.latest")
            .count(),
        4
    );
    assert_eq!(state.watch_rounds, 2);
    assert!(!state.reports.is_empty());
}

#[tokio::test]
async fn manifest_sync_unknown_method_stops_once_without_watch_or_retry_loop() {
    let vault = Vault::start(State {
        unknown_method: true,
        ..State::default()
    });
    let directory = tempfile::tempdir().unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        run_with(paths(&directory), || {
            ManagementSession::for_test(&vault.path)
        }),
    )
    .await
    .expect("unknown method must stop the worker");
    assert_eq!(vault.state.lock().unwrap().methods, ["manifest.latest"]);
}

#[tokio::test]
async fn manifest_sync_report_unknown_outcome_retries_the_same_tuple() {
    let vault = Vault::start(State {
        report_unknown_once: true,
        ..State::default()
    });
    let directory = tempfile::tempdir().unwrap();
    tokio::select! {
        _ = run_with(paths(&directory), || ManagementSession::for_test(&vault.path)) => panic!("sync stopped"),
        _ = until(&vault.state, |state| state.watch_rounds >= 1) => {}
    }
    let state = vault.state.lock().unwrap();
    assert!(state.reports.len() >= 2);
    assert_eq!(state.reports[0], state.reports[1]);
}

#[tokio::test]
async fn manifest_sync_notice_digest_disagreement_rereads_once_then_reports_invalid() {
    let vault = Vault::start(State {
        digest_mismatch: true,
        ..State::default()
    });
    let directory = tempfile::tempdir().unwrap();
    let local = paths(&directory);
    let manifest_path = local.manifest.clone();
    tokio::select! {
        _ = run_with(local, || ManagementSession::for_test(&vault.path)) => panic!("sync stopped"),
        _ = until(&vault.state, |state| state.reports.iter().any(|report| report["reason"] == "digest differs from the vault's notice")) => {}
    }
    let state = vault.state.lock().unwrap();
    assert_eq!(
        state
            .methods
            .iter()
            .filter(|method| *method == "manifest.latest")
            .count(),
        3
    );
    assert!(!manifest_path.exists());
}

#[tokio::test]
async fn manifest_sync_newer_activation_reports_the_shims_valid_verdict() {
    let vault = Vault::start(State::default());
    let directory = tempfile::tempdir().unwrap();
    let paths = paths(&directory);
    let now = unix_seconds();
    let activated = activate(&paths, &candidate(29, now), now).unwrap();
    let session = ManagementSession::for_test(&vault.path).await.unwrap();
    report(&session, &activated).await.unwrap();
    session.close().await;
    let state = vault.state.lock().unwrap();
    assert_eq!(state.reports.len(), 1);
    assert_eq!(state.reports[0]["verdict"], "valid");
    assert_eq!(state.reports[0]["reason"], Value::Null);
    assert_eq!(cached_manifest_report_at(&paths, now).version, Some(29));
}
