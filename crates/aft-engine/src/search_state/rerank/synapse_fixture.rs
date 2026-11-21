use super::*;
use std::time::Duration;
pub fn config(connection_file: std::path::PathBuf) -> SemanticBackendConfig {
    SemanticBackendConfig {
        local_cache_dir: None,
        backend: crate::config::SemanticBackend::Synapse,
        model: "embedding".into(),
        base_url: None,
        api_key_env: None,
        timeout_ms: 1500,
        query_timeout_ms: 1500,
        query_instruction: "off".into(),
        max_batch_size: 20,
        max_input_tokens: None,
        max_files: 100,
        subc_connection_file: Some(connection_file),
        route_project_root: None,
        route_harness: None,
    }
}

pub fn fake_daemon(
    responses: Vec<Option<Value>>,
    registered: bool,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::thread::JoinHandle<Vec<Value>>,
) {
    fake_daemon_gated(responses, registered, None)
}

pub type ResponseGate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

pub(super) fn connection_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::net::TcpListener,
    Vec<u8>,
    [u8; subc_transport::DAEMON_ID_LEN],
) {
    use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
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
            // The SDK requires daemon 0.39+ to open a management route without
            // specifying a project.
            daemon_ver: "0.39.0".into(),
        },
    )
    .unwrap();
    (directory, path, listener, key, daemon_id)
}

pub fn fake_daemon_gated(
    responses: Vec<Option<Value>>,
    registered: bool,
    gate: Option<ResponseGate>,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::thread::JoinHandle<Vec<Value>>,
) {
    let (directory, path, listener, key, daemon_id) = connection_fixture();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            tokio::time::timeout(Duration::from_secs(30), async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "0.39.0", Duration::from_secs(30)).await.unwrap();
                let catalog = aft_base::subc_fake::read_frame(&mut stream).await.unwrap().unwrap();
                let modules = if registered { json!([{"module_id":"synapse","module_version":"1","roles":[{"role":"management_surface","operations":[{"name":"models.list","kind":"query"},{"name":"rerank.score","kind":"query"}],"config_schema":{"type":"object"},"observability":[],"identity_scope":[]}],"control_ops":[]}]) } else { json!([]) };
                respond(&mut stream, &catalog, json!({"op":"catalog.list","generation":1,"modules":modules,"subc_ops":["catalog.list","route.open"]})).await;
                if !registered { return Vec::new(); }
                let route = aft_base::subc_fake::read_frame(&mut stream).await.unwrap().unwrap();
                let route_body: Value = serde_json::from_slice(&route.body).unwrap();
                assert_eq!(route_body["target"], json!({"kind":"management_surface","module_id":"synapse"}));
                respond(&mut stream, &route, json!({"op":"route.open","route_channel":7,"route_epoch":1})).await;
                let mut requests = Vec::new();
                let mut gate = gate;
                for response in responses {
                    let request = aft_base::subc_fake::read_frame(&mut stream).await.unwrap().unwrap();
                    requests.push(serde_json::from_slice(&request.body).unwrap());
                    if let Some((entered, release)) = gate.take() {
                        entered.send(()).unwrap();
                        release.recv_timeout(Duration::from_secs(2)).unwrap();
                    }
                    if let Some(response) = response { respond(&mut stream, &request, response).await; }
                    else {
                        // Withhold the reply until the client has timed out, then detect any retry.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        if let Ok(has_byte) = tokio::time::timeout(Duration::from_millis(50), crate::synapse_embed::pause_test_support::has_next_frame_byte(&stream)).await {
                            assert!(!has_byte, "unexpected retry on route");
                        }
                        assert!(tokio::time::timeout(Duration::from_millis(50), listener.accept()).await.is_err());
                    }
                }
                requests
            }).await.expect("fake Synapse watchdog")
        })
    });
    (directory, path, server)
}

async fn respond(stream: &mut tokio::net::TcpStream, request: &subc_protocol::Frame, value: Value) {
    use subc_protocol::{Flags, Frame, FrameType, Priority};
    let frame = Frame::build_with_version(
        request.header.ver,
        FrameType::Response,
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(stream, &frame).await.unwrap();
}
