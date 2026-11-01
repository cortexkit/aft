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
    views: bool,
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
    async fn start(root: &Path, state: &Path, views: bool) -> Self {
        Self::start_with_attach_gate(root, state, views, None).await
    }

    async fn start_with_attach_gate(
        root: &Path,
        state: &Path,
        views: bool,
        gate: Option<&Path>,
    ) -> Self {
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
                "views": {"enabled": views},
                "git": {"co_author": "off"},
                "bash": {"shell": "/bin/bash"},
            })
            .to_string(),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_aft"));
        command
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
            .stderr(std::fs::File::create(state.join("daemon.stderr")).unwrap());
        if let Some(gate) = gate {
            command.env("AFT_TEST_WATCHER_ATTACH_GATE_FILE", gate);
        }
        let child = command.spawn().unwrap();
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
            identity: BindIdentity::new(root.to_path_buf(), "opencode", "watcher-search"),
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
            views,
        }
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let body = self.request(name, arguments).await;
        assert_ne!(body["isError"], true, "{name}: {body}");
        body
    }

    async fn request(&mut self, name: &str, arguments: Value) -> Value {
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
    wait_visible(daemon, relative, marker, Duration::from_secs(15)).await;
}

async fn wait_visible(daemon: &mut Daemon, relative: &str, marker: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let grep = daemon
            .request("grep", json!({"pattern":marker, "path":"crates"}))
            .await;
        let data = &grep["structuredContent"];
        let search = daemon
            .request("search", json!({"pattern":marker, "includeTests":true}))
            .await;
        let search_found = search["structuredContent"]["results"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter()
                    .any(|r| r["file"].as_str().is_some_and(|p| p.ends_with(relative)))
            });
        if data["index_status"] == "Ready"
            && data["total_matches"] == 1
            && search_found
            && (!daemon.views || data["complete"] == true)
        {
            if daemon.views {
                assert_eq!(
                    data["files_read_directly"], 0,
                    "views must serve the index: {grep}"
                );
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "external file {relative} missing from indexed directory grep or search:\ngrep: {grep}\nsearch: {search}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn bound_daemon_external_changes_reach_indexed_grep_and_search() {
    external_changes_reach_indexed_grep_and_search(false).await;
}

#[tokio::test]
async fn bound_daemon_views_on_external_changes_reach_indexed_grep_and_search() {
    external_changes_reach_indexed_grep_and_search(true).await;
}

async fn external_changes_reach_indexed_grep_and_search(views: bool) {
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
    std::fs::create_dir_all(root.join("crates/git-added/test-vectors/empty-claim")).unwrap();
    std::fs::write(
        root.join("crates/git-added/test-vectors/empty-claim/README.md"),
        "git_readme_marker\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "add crate"]);
    git(root, &["checkout", "-q", "-"]);
    let mut daemon = Daemon::start(root, state.path(), views).await;
    assert_visible(
        &mut daemon,
        "crates/old/src/lib.rs",
        "initial_daemon_marker",
    )
    .await;
    wait_watcher_intake(&mut daemon, root).await;
    assert_empty_arrival(&mut daemon, &root.join("crates/empty-native")).await;
    let before_changes = watcher_counts(&mut daemon).await;

    // Search while each marker is absent so the test also detects whether the
    // watcher invalidates cached absence after the files arrive.
    for marker in [
        "new_directory_marker",
        "git_checkout_marker",
        "copied_file_marker",
        "bash_file_marker",
        "moved_directory_marker",
        "git_readme_marker",
        "changed_file_marker",
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
    let bash = daemon.call("bash", json!({"command":"printf '// bash_file_marker\\n' > crates/old/src/bash.rs; printf '// changed_file_marker\\n' > crates/old/src/lib.rs", "wait":true})).await;
    assert_ne!(bash["isError"], true, "{bash}");
    let moved = state.path().join("moved");
    std::fs::create_dir_all(moved.join("src")).unwrap();
    std::fs::write(moved.join("src/lib.rs"), "// moved_directory_marker\n").unwrap();
    std::fs::rename(&moved, root.join("crates/moved")).unwrap();
    for (file, marker) in [
        ("crates/new/src/lib.rs", "new_directory_marker"),
        ("crates/git-added/src/lib.rs", "git_checkout_marker"),
        ("crates/copied/src/lib.rs", "copied_file_marker"),
        ("crates/old/src/bash.rs", "bash_file_marker"),
        ("crates/moved/src/lib.rs", "moved_directory_marker"),
        (
            "crates/git-added/test-vectors/empty-claim/README.md",
            "git_readme_marker",
        ),
        ("crates/old/src/lib.rs", "changed_file_marker"),
    ] {
        assert_visible(&mut daemon, file, marker).await;
    }
    let after_small = watcher_counts(&mut daemon).await;
    assert_eq!(
        rescan_total(&after_small),
        rescan_total(&before_changes),
        "small arrivals must remain incremental: {after_small}"
    );
    assert_eq!(
        after_small["directory_walk_fallbacks_total"],
        before_changes["directory_walk_fallbacks_total"]
    );

    // Exceed the 1,024-entry discovery limit and require a complete indexed
    // answer after the rescan, not just the files visited before the limit.
    let large = state.path().join("large");
    std::fs::create_dir(&large).unwrap();
    for i in 0..2049 {
        std::fs::write(
            large.join(format!("file-{i:04}.rs")),
            "// bounded_large_common_marker\n",
        )
        .unwrap();
    }
    std::fs::write(
        large.join("target.rs"),
        "// bounded_large_common_marker bounded_large_last_marker\n",
    )
    .unwrap();
    std::fs::rename(&large, root.join("crates/large")).unwrap();
    wait_visible(
        &mut daemon,
        "crates/large/target.rs",
        "bounded_large_last_marker",
        Duration::from_secs(60),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = daemon.request("grep", json!({"pattern":"bounded_large_common_marker", "path":"crates/large", "offset":2049})).await;
        let data = &result["structuredContent"];
        if data["index_status"] == "Ready"
            && data["complete"] == true
            && data["total_matches"] == 2050
        {
            if views {
                assert_eq!(data["files_read_directly"], 0, "{result}");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "large arrival did not become complete: {result}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after_large = watcher_counts(&mut daemon).await;
    assert!(
        after_large["directory_walk_fallbacks_total"]
            .as_u64()
            .unwrap()
            > after_small["directory_walk_fallbacks_total"]
                .as_u64()
                .unwrap(),
        "{after_large}"
    );
    assert!(
        rescan_total(&after_large) > rescan_total(&after_small),
        "{after_large}"
    );
    println!(
        "daemon_arrivals views={views} small_rescans=0 large_rescans={} large_walk_fallbacks={}",
        rescan_total(&after_large) - rescan_total(&after_small),
        after_large["directory_walk_fallbacks_total"]
            .as_u64()
            .unwrap()
            - after_small["directory_walk_fallbacks_total"]
                .as_u64()
                .unwrap()
    );
}

async fn watcher_counts(daemon: &mut Daemon) -> Value {
    let response = daemon.call("status", json!({})).await;
    response["structuredContent"]["watcher"].clone()
}

fn rescan_total(counts: &Value) -> u64 {
    [
        "rescans_buffer_overflow_total",
        "rescans_kernel_dropped_total",
        "rescans_user_dropped_total",
        "rescans_unknown_total",
    ]
    .into_iter()
    .map(|key| counts[key].as_u64().unwrap())
    .sum()
}

async fn wait_watcher_intake(daemon: &mut Daemon, root: &Path) {
    // A view may be ready before deferred native watcher startup. Cookie removal
    // proves intake is running, not that every earlier write has been indexed.
    let cookie = root.join(format!(
        ".aft-cookie-{}-daemon-readiness",
        std::process::id()
    ));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match std::fs::remove_file(&cookie) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("reset watcher-readiness cookie: {error}"),
        }
        std::fs::write(&cookie, b"").unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        let counts = watcher_counts(daemon).await;
        if !cookie.exists() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native watcher intake did not start: {counts}"
        );
    }
}

async fn assert_empty_arrival(daemon: &mut Daemon, directory: &Path) {
    let before = watcher_counts(daemon).await;
    std::fs::create_dir(directory).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let after = watcher_counts(daemon).await;
        if empty_arrival_observed(&before, &after) {
            assert_eq!(
                after["directory_subtree_walks_total"],
                before["directory_subtree_walks_total"]
            );
            assert_eq!(rescan_total(&after), rescan_total(&before));
            return;
        }
        assert!(
            Instant::now() < deadline,
            "empty arrival not observed for {directory:?}: before={before} after={after}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn empty_arrival_observed(before: &Value, after: &Value) -> bool {
    after["directory_empty_total"].as_u64().unwrap()
        > before["directory_empty_total"].as_u64().unwrap()
}

#[cfg(all(target_os = "linux", feature = "test-timing-hooks"))]
#[tokio::test]
async fn bound_daemon_index_readiness_does_not_imply_watcher_readiness() {
    let _serial = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.join("crates/old/src")).unwrap();
    std::fs::write(
        root.join("crates/old/src/lib.rs"),
        "// watcher_startup_seed\n",
    )
    .unwrap();
    let gate = state.path().join("watcher-attach.hold");
    std::fs::write(&gate, b"hold").unwrap();
    let mut daemon = Daemon::start_with_attach_gate(root, state.path(), true, Some(&gate)).await;
    assert_visible(&mut daemon, "crates/old/src/lib.rs", "watcher_startup_seed").await;
    let held_deadline = Instant::now() + Duration::from_secs(15);
    while !gate.with_extension("waiting").exists() {
        assert!(
            Instant::now() < held_deadline,
            "watcher did not reach the attach gate"
        );
        let _ = watcher_counts(&mut daemon).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let before = watcher_counts(&mut daemon).await;
    assert_eq!(
        before["raw_events_total"], 0,
        "registration is held: {before}"
    );
    // A search-ready snapshot can precede its parent's native watch. Initial
    // registration finds directories created in this interval but cannot replay
    // a past CREATE notification as a new empty arrival.
    let prewatch = root.join("crates/before-watcher");
    std::fs::create_dir(&prewatch).unwrap();
    std::fs::remove_file(&gate).unwrap();
    wait_watcher_intake(&mut daemon, root).await;
    let registered = watcher_counts(&mut daemon).await;
    println!("watcher_startup_window before={before} registered={registered}");
    assert!(registered["raw_events_total"].as_u64().unwrap() > 0);
    assert_eq!(
        registered["directory_empty_total"], 0,
        "pre-registration mkdir has no CREATE event: {registered}"
    );
    assert!(
        !empty_arrival_observed(&before, &registered),
        "watcher readiness alone must not satisfy the empty-arrival assertion"
    );
    // Initial registration still watches that directory: later file events work.
    std::fs::write(prewatch.join("file.rs"), "// watched_after_startup\n").unwrap();
    assert_visible(
        &mut daemon,
        "crates/before-watcher/file.rs",
        "watched_after_startup",
    )
    .await;
    assert_empty_arrival(&mut daemon, &root.join("crates/after-watcher")).await;
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn bound_daemon_views_on_file_changes_before_registration_are_not_complete_misses() {
    let _serial = crate::helpers::watcher_serial_lock();
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    let state = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.join("crates/old/src")).unwrap();
    let indexed = root.join("crates/old/src/lib.rs");
    std::fs::write(&indexed, "// indexed_before_attach_seed\n").unwrap();
    let gate = state.path().join("file-window-attach.hold");
    std::fs::write(&gate, b"hold").unwrap();
    let mut daemon = Daemon::start_with_attach_gate(root, state.path(), true, Some(&gate)).await;
    assert_visible(
        &mut daemon,
        "crates/old/src/lib.rs",
        "indexed_before_attach_seed",
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !gate.with_extension("waiting").exists() {
        assert!(
            Instant::now() < deadline,
            "watcher did not reach the file-window attach gate"
        );
        let _ = watcher_counts(&mut daemon).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let held = watcher_counts(&mut daemon).await;
    assert_eq!(
        held["raw_events_total"], 0,
        "native attachment is held: {held}"
    );
    let tokens = [
        ("crates/old/src/new.rs", "startup_new_file_7d9a2"),
        ("crates/old/src/lib.rs", "startup_edited_file_8e1b3"),
    ];
    std::fs::write(root.join(tokens[0].0), format!("// {}\n", tokens[0].1)).unwrap();
    std::fs::write(&indexed, format!("// {}\n", tokens[1].1)).unwrap();
    assert!(gate.exists());
    std::fs::remove_file(&gate).unwrap();
    wait_watcher_intake(&mut daemon, root).await;

    // Do not hide a complete false negative by polling until it repairs itself.
    // Inspect each first post-registration answer before requiring convergence.
    let mut evidence = Vec::new();
    for (relative, token) in tokens {
        for tool in ["grep", "search"] {
            let arguments = if tool == "grep" {
                json!({"pattern":token, "path":"crates/old/src"})
            } else {
                json!({"pattern":token, "includeTests":true})
            };
            let reply = daemon.request(tool, arguments).await;
            let found = file_window_token_found(&reply, tool, relative, token);
            let incomplete =
                reply["structuredContent"]["complete"] == false || reply["isError"] == true;
            evidence.push(json!({"tool":tool, "token":token, "found":found, "incomplete":incomplete, "reply":reply}));
        }
    }
    println!("startup_file_window {}", json!(evidence));
    assert!(
        evidence
            .iter()
            .all(|row| row["found"] == true || row["incomplete"] == true),
        "complete post-registration answer missed pre-registration file changes: {}",
        json!(evidence)
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    for (relative, token) in tokens {
        loop {
            let grep = daemon
                .request("grep", json!({"pattern":token, "path":"crates/old/src"}))
                .await;
            let search = daemon
                .request("search", json!({"pattern":token, "includeTests":true}))
                .await;
            if file_window_token_found(&grep, "grep", relative, token)
                && grep["structuredContent"]["index_status"] == "Ready"
                && grep["structuredContent"]["complete"] == true
                && file_window_token_found(&search, "search", relative, token)
            {
                assert_eq!(
                    grep["structuredContent"]["files_read_directly"], 0,
                    "{grep}"
                );
                break;
            }
            assert!(Instant::now() < deadline,
                "pre-registration token did not reach the view: {token}\ngrep: {grep}\nsearch: {search}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(feature = "test-timing-hooks")]
fn file_window_token_found(reply: &Value, tool: &str, relative: &str, token: &str) -> bool {
    let data = &reply["structuredContent"];
    let rows = data[if tool == "grep" { "matches" } else { "results" }].as_array();
    rows.is_some_and(|rows| {
        rows.iter().any(|row| {
            row["file"]
                .as_str()
                .is_some_and(|path| path.ends_with(relative))
                && if tool == "grep" {
                    data["total_matches"] == 1
                        && data["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(token))
                } else {
                    row["snippet"]
                        .as_str()
                        .is_some_and(|snippet| snippet.contains(token))
                }
        })
    })
}
