#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, read_frame, write_frame};
use tokio::net::{TcpListener, TcpStream};

struct Daemon {
    child: Child,
    stream: TcpStream,
    corr: u64,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn receive(stream: &mut TcpStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
        .await
        .expect("daemon response deadline")
        .expect("daemon frame")
        .expect("daemon connection closed")
}

impl Daemon {
    async fn start(root: &Path, state: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = state.join("connection.json");
        connection_file::write_atomic(
            &connection,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: vec![0x42; subc_transport::KEY_LEN],
                daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
                pid: std::process::id(),
                daemon_ver: "watcher-search-test".into(),
            },
        )
        .unwrap();
        let config = state.join("config/cortexkit");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("aft.jsonc"),
            json!({
                "storage_dir": state.join("storage"),
                "indexes": {"trigram": true, "semantic": false, "callgraph": false},
                "views": {"enabled": false},
                "git": {"co_author": "off"},
                "bash": {"shell": "/bin/bash"},
            })
            .to_string(),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_aft"))
            .args(["--subc"])
            .arg(&connection)
            .env("HOME", state)
            .env("USERPROFILE", state)
            .env("XDG_CONFIG_HOME", state.join("config"))
            .env("XDG_DATA_HOME", state.join("data"))
            .env("AFT_STORAGE_DIR", state.join("storage"))
            .env("AFT_TEST_DISABLE_FILE_WATCHER", "0")
            .env("AFT_TEST_SYNC_FILE_WATCHER_START", "1")
            .env_remove("AFT_DATA_HOME")
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(state.join("daemon.stderr")).unwrap())
            .spawn()
            .unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
            .await
            .unwrap()
            .unwrap();
        authenticate_server(
            &mut stream,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "watcher-search-test",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let hello = receive(&mut stream).await;
        assert_eq!(hello.header.ty, FrameType::Hello);
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ModuleHelloAckBody {
                    negotiated_ver: PROTOCOL_VERSION,
                    subc_ops: vec![],
                    subc_capabilities: vec![],
                    storage: None,
                    machine_id: None,
                })
                .unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let bind = ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            identity: BindIdentity::new(
                root.to_path_buf(),
                "opencode".into(),
                "watcher-search".into(),
            ),
            principal: Some(Principal::Direct),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
            call_mode: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                1,
                serde_json::to_vec(&bind).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ack = receive(&mut stream).await;
        assert_eq!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body).unwrap(),
            ModuleControlResponse::RouteBindAck {}
        );
        Self {
            child,
            stream,
            corr: 10,
        }
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.corr += 1;
        write_frame(
            &mut self.stream,
            &Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                1,
                1,
                self.corr,
                serde_json::to_vec(&json!({"name":name,"arguments":arguments})).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        loop {
            let frame = receive(&mut self.stream).await;
            if frame.header.ty == FrameType::Response && frame.header.corr == self.corr {
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                assert_ne!(body["isError"], true, "{name}: {body}");
                return body;
            }
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn assert_visible(daemon: &mut Daemon, relative: &str, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let grep = daemon
            .call("grep", json!({"pattern":marker, "path":"crates"}))
            .await;
        let data = &grep["structuredContent"];
        if data["index_status"] == "Ready" && data["total_matches"] == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "indexed directory grep missed {relative}: {grep}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let search = daemon
        .call("search", json!({"pattern":marker, "includeTests":true}))
        .await;
    assert!(
        search["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["file"].as_str().is_some_and(|p| p.ends_with(relative))),
        "search missed {relative}: {search}"
    );
}

#[tokio::test]
async fn bound_daemon_external_changes_reach_indexed_grep_and_search() {
    let _serial = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.join("crates/old/src")).unwrap();
    std::fs::write(
        root.join("crates/old/src/lib.rs"),
        "// initial_daemon_marker\n",
    )
    .unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "watcher@example.invalid"]);
    git(root, &["config", "user.name", "Watcher Test"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    git(root, &["checkout", "-qb", "new-crate"]);
    std::fs::create_dir_all(root.join("crates/git-added/src")).unwrap();
    std::fs::write(
        root.join("crates/git-added/Cargo.toml"),
        "[package]\nname = \"git-added\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/git-added/src/lib.rs"),
        "// git_checkout_marker\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "add crate"]);
    git(root, &["checkout", "-q", "-"]);
    let mut daemon = Daemon::start(root, state.path()).await;
    assert_visible(
        &mut daemon,
        "crates/old/src/lib.rs",
        "initial_daemon_marker",
    )
    .await;

    // Search while each marker is absent so the test also detects whether the
    // watcher invalidates cached absence after the files arrive.
    for marker in [
        "new_directory_marker",
        "git_checkout_marker",
        "copied_file_marker",
        "bash_file_marker",
    ] {
        daemon
            .call("search", json!({"pattern":marker,"includeTests":true}))
            .await;
    }
    std::fs::create_dir_all(root.join("crates/new/src")).unwrap();
    std::fs::write(
        root.join("crates/new/src/lib.rs"),
        "// new_directory_marker\n",
    )
    .unwrap();
    git(root, &["checkout", "-q", "new-crate"]);
    let incoming = state.path().join("incoming");
    std::fs::create_dir_all(incoming.join("src")).unwrap();
    std::fs::write(incoming.join("src/lib.rs"), "// copied_file_marker\n").unwrap();
    let copied = Command::new("cp")
        .arg("-R")
        .arg(&incoming)
        .arg(root.join("crates/copied"))
        .status()
        .unwrap();
    assert!(copied.success());
    let bash = daemon.call("bash", json!({"command":"printf '// bash_file_marker\\n' > crates/old/src/bash.rs", "wait":true})).await;
    assert_ne!(bash["isError"], true, "{bash}");
    for (file, marker) in [
        ("crates/new/src/lib.rs", "new_directory_marker"),
        ("crates/git-added/src/lib.rs", "git_checkout_marker"),
        ("crates/copied/src/lib.rs", "copied_file_marker"),
        ("crates/old/src/bash.rs", "bash_file_marker"),
    ] {
        assert_visible(&mut daemon, file, marker).await;
    }
}
