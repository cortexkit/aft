use std::fs;
use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};
use subc_protocol::{Flags, Frame, FrameType, Priority, PROTOCOL_VERSION};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tempfile::TempDir;
use tokio::net::TcpStream;

const MIB: u64 = 1024 * 1024;
const ROUTE_CHANNEL: u16 = 42;
const ROUTE_EPOCH: u32 = 7;

fn aft_binary() -> PathBuf {
    std::env::var_os("AFT_TEST_AFT_BINARY")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_aft"))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_aft"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aft")))
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

struct FakeManagementSurfaceDaemon {
    _connection_dir: TempDir,
    connection_file: PathBuf,
    server: thread::JoinHandle<()>,
}

impl FakeManagementSurfaceDaemon {
    fn spawn(census: Value) -> Self {
        let connection_dir = tempfile::tempdir().expect("connection tempdir");
        let connection_file = connection_dir.path().join("subc-connection.json");
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
        listener.set_nonblocking(true).expect("set nonblocking");
        let port = listener.local_addr().expect("fake daemon address").port();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &connection_file,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".to_string(),
                    port,
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "profile-memory-test".to_string(),
            },
        )
        .expect("write fake daemon connection file");

        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fake daemon runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
                tokio::time::timeout(
                    Duration::from_secs(15),
                    drive_profile_memory_daemon(listener, key, daemon_id, census),
                )
                .await
                .expect("profile memory fake daemon watchdog");
            });
        });

        Self {
            _connection_dir: connection_dir,
            connection_file,
            server,
        }
    }

    fn connection_file(&self) -> &Path {
        &self.connection_file
    }

    fn finish(self) {
        self.server.join().expect("fake daemon joins");
    }
}

async fn drive_profile_memory_daemon(
    listener: tokio::net::TcpListener,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    census: Value,
) {
    let (mut stream, _) = listener.accept().await.expect("accept profile client");
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "profile-memory-test",
        Duration::from_secs(5),
    )
    .await
    .expect("authenticate profile client");

    let catalog = read_request(&mut stream, "catalog.list").await;
    assert_eq!(catalog.header.channel, 0);
    assert_eq!(request_body(&catalog), json!({ "op": "catalog.list" }));
    send_response(
        &mut stream,
        &catalog,
        json!({
            "op": "catalog.list",
            "generation": 1,
            "modules": [{
                "module_id": "aft",
                "module_version": "0.55.1",
                "roles": [{
                    "role": "management_surface",
                    "operations": [{ "name": "memory.census", "kind": "query" }],
                    "config_schema": { "type": "object" },
                    "observability": [],
                    "identity_scope": [],
                }],
                "control_ops": [],
            }],
            "subc_ops": ["catalog.list", "route.open"],
        }),
    )
    .await;

    let route_open = read_request(&mut stream, "route.open").await;
    let route_open_body = request_body(&route_open);
    assert_eq!(route_open.header.channel, 0);
    assert_eq!(route_open_body["op"], "route.open");
    assert_eq!(
        route_open_body["target"],
        json!({ "kind": "management_surface", "module_id": "aft" })
    );
    assert_eq!(route_open_body["identity"]["harness"], "aft-profile");
    assert!(route_open_body["identity"]["session"]
        .as_str()
        .is_some_and(|session| session.starts_with("aft-profile-")));
    assert_eq!(
        route_open_body["consumer_identity"],
        json!({ "module_id": "aft", "launch_nonce": "profile-test-launch" })
    );
    send_response(
        &mut stream,
        &route_open,
        json!({
            "op": "route.open",
            "route_channel": ROUTE_CHANNEL,
            "route_epoch": ROUTE_EPOCH,
        }),
    )
    .await;

    let request = read_request(&mut stream, "memory.census").await;
    assert_eq!(request.header.channel, ROUTE_CHANNEL);
    assert_eq!(request.header.epoch, ROUTE_EPOCH);
    assert_eq!(
        request_body(&request),
        json!({ "op": "memory.census", "params": {} })
    );
    send_response(
        &mut stream,
        &request,
        json!({ "op": "memory.census", "status": "ok", "data": census }),
    )
    .await;
}

async fn read_request(stream: &mut TcpStream, label: &str) -> Frame {
    let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out reading {label}"))
        .unwrap_or_else(|error| panic!("failed reading {label}: {error}"))
        .unwrap_or_else(|| panic!("connection closed before {label}"));
    assert_eq!(frame.header.ty, FrameType::Request, "{label} frame type");
    frame
}

fn request_body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).expect("request JSON")
}

async fn send_response(stream: &mut TcpStream, request: &Frame, body: Value) {
    let response = Frame::build_with_version(
        request.header.ver,
        FrameType::Response,
        control_flags(),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&body).expect("response JSON"),
    )
    .expect("response frame");
    write_frame(stream, &response)
        .await
        .expect("write response frame");
}

fn write_profile_config(config_home: &Path, connection_file: &Path) {
    let path = config_home.join("cortexkit").join("aft.jsonc");
    fs::create_dir_all(path.parent().unwrap()).expect("create profile config directory");
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "subc": { "connection_file": connection_file }
        }))
        .expect("profile config JSON"),
    )
    .expect("write profile config");
}

fn run_profile_memory(
    config_home: &Path,
    current_dir: &Path,
    with_launch_identity: bool,
) -> Output {
    let mut command = Command::new(aft_binary());
    command
        .args(["profile", "--memory"])
        .current_dir(current_dir)
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", config_home)
        .env_remove("USERPROFILE")
        .env_remove("SUBC_CONNECTION_FILE");
    if with_launch_identity {
        command
            .env("SUBC_MODULE_ID", "aft")
            .env("SUBC_LAUNCH_NONCE", "profile-test-launch");
    } else {
        command
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE");
    }
    command.output().expect("run aft profile --memory")
}

fn fake_census() -> Value {
    json!({
        "process": {
            "phys_footprint_bytes": 64 * MIB,
            "rss_bytes": 48 * MIB,
            "allocator_slack_bytes": 8 * MIB,
            "sqlite_bytes": MIB,
            "total_attributed_bytes": 22 * MIB,
            "unattributed_bytes": 34 * MIB,
        },
        "roots": {
            "/fake/root-alpha": {
                "bound_routes": 2,
                "last_request_age_ms": 3456,
                "evictable_in_ms": null,
                "planes": {
                    "search": MIB,
                    "semantic": 2 * MIB,
                    "symbols": 3 * MIB,
                    "callgraph": 4 * MIB,
                    "inspect": 5 * MIB,
                },
                "attributed_bytes": 15 * MIB,
                "evictable_bytes": 0,
                "lsp_children": { "count": 1, "rss_bytes": 9 * MIB },
            },
            "/fake/worktree-pool-42": {
                "bound_routes": 0,
                "last_request_age_ms": 7000,
                "evictable_in_ms": 9000,
                "planes": {
                    "search": 2 * MIB,
                    "semantic": MIB,
                    "symbols": MIB,
                    "callgraph": 2 * MIB,
                    "inspect": MIB,
                },
                "attributed_bytes": 7 * MIB,
                "evictable_bytes": 6 * MIB,
                "lsp_children": { "count": 0, "rss_bytes": 0 },
            },
        },
    })
}

#[test]
fn profile_memory_renders_fake_management_surface_census() {
    let daemon = FakeManagementSurfaceDaemon::spawn(fake_census());
    let config_home = tempfile::tempdir().expect("config home");
    let project = tempfile::tempdir().expect("profile project");
    write_profile_config(config_home.path(), daemon.connection_file());

    let output = run_profile_memory(config_home.path(), project.path(), true);
    daemon.finish();

    assert!(output.status.success(), "profile failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("profile stdout UTF-8");
    assert!(stdout.contains("AFT memory census\n"), "{stdout}");
    assert!(stdout.contains("phys footprint: 64.0 MB\n"), "{stdout}");
    assert!(stdout.contains("rss: 48.0 MB\n"), "{stdout}");
    assert!(
        stdout.contains("allocator slack (reclaimable by relief): 8.0 MB\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("total attributed: 22.0 MB; unattributed: 34.0 MB\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "/fake/root-alpha (root-alpha) | 2 | 3456ms | 1.0 2.0 3.0 4.0 5.0 | 15.0 | 0.0 | — | 1"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "/fake/worktree-pool-42 (worktree-pool-42) | 0 | 7000ms | 2.0 1.0 1.0 2.0 1.0 | 7.0 | 6.0 | 9000ms | 0"
        ),
        "{stdout}"
    );
}

#[test]
fn profile_memory_without_connection_file_reports_unavailable() {
    let config_home = tempfile::tempdir().expect("config home");
    let project = tempfile::tempdir().expect("profile project");

    let output = run_profile_memory(config_home.path(), project.path(), false);

    assert!(output.status.success(), "profile failed: {output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).expect("profile stdout UTF-8"),
        "AFT memory census unavailable: no daemon is connected.\n"
    );
}
