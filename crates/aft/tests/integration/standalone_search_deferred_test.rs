use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use filetime::{set_file_mtime, FileTime};
use serde_json::{json, Value};

use super::helpers::{user_config, AftProcess};

const SEARCH_QUERY: &str = "where does standalone deferred cancellation stop remote embedding";

fn seed_stale_storage_entries(storage: &Path, transient_root: &Path, count: usize) -> Vec<PathBuf> {
    let old_index_time =
        FileTime::from_system_time(SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60));
    let old_transient_time =
        FileTime::from_system_time(SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60));
    let mut orphan_dirs = Vec::with_capacity(count);

    for index in 0..count {
        let key = format!("{index:016x}");
        let orphan_dir = storage.join("index").join(&key);
        fs::create_dir_all(&orphan_dir).expect("create stale index entry");
        let cache_file = orphan_dir.join("cache.bin");
        fs::write(&cache_file, b"stale").expect("write stale index cache");
        set_file_mtime(&cache_file, old_index_time).expect("age stale index cache");
        orphan_dirs.push(orphan_dir);

        let transient_dir = transient_root.join(format!("aft-search-cache.{key}.{}", index + 1));
        fs::create_dir_all(&transient_dir).expect("create stale transient cache");
        fs::write(transient_dir.join("cache.bin"), b"stale").expect("write stale transient cache");
        set_file_mtime(&transient_dir, old_transient_time).expect("age transient cache directory");
    }

    orphan_dirs
}

fn configure_without_indexes(project: &Path) -> String {
    serde_json::to_string(&json!({
        "id": "configure-maintenance",
        "command": "configure",
        "harness": "opencode",
        "project_root": project.display().to_string(),
        "config": user_config(json!({
            "indexes": { "trigram": false, "semantic": false, "callgraph": false }
        }))
    }))
    .expect("serialize configure request")
}

fn read_response(aft: &mut AftProcess, request_id: &str, timeout: Duration) -> (Value, Instant) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = aft
            .try_read_next_timeout(remaining.min(Duration::from_millis(100)))
            .unwrap_or_else(|| {
                assert!(Instant::now() < deadline, "response {request_id} timed out");
                Value::Null
            });
        if frame["id"] == request_id {
            return (frame, Instant::now());
        }
    }
}

#[test]
fn standalone_configure_yields_to_back_to_back_ping_before_seeded_storage_sweeps() {
    let project = tempfile::tempdir().expect("create queued-ping project");
    let storage = tempfile::tempdir().expect("create shared storage fixture");
    let transient_root = tempfile::tempdir().expect("create transient cache root");
    seed_stale_storage_entries(storage.path(), transient_root.path(), 200);

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("TMPDIR", transient_root.path().as_os_str()),
        ("TMP", transient_root.path().as_os_str()),
        ("TEMP", transient_root.path().as_os_str()),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_DELAY_MS",
            std::ffi::OsStr::new("3000"),
        ),
    ]);
    aft.send_silent(&configure_without_indexes(project.path()));
    aft.send_silent(r#"{"id":"queued-ping","command":"ping"}"#);

    let (configure, configure_received_at) =
        read_response(&mut aft, "configure-maintenance", Duration::from_secs(5));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let (ping, ping_received_at) = read_response(&mut aft, "queued-ping", Duration::from_secs(2));
    assert_eq!(ping["command"], "pong", "ping failed: {ping:?}");
    let latency = ping_received_at.duration_since(configure_received_at);
    // The sweep is held for 3 s, so answering within 1.5 s proves the ping did
    // not queue behind it, with room for a loaded Windows runner (568 ms was
    // seen against the old 500 ms bound with a 750 ms sweep).
    assert!(
        latency < Duration::from_millis(1_500),
        "queued ping waited {latency:?} after configure acknowledgement"
    );

    let (status, stderr) = aft.stderr_output();
    assert!(status.success());
    assert!(
        stderr.contains("configure maintenance yielded to 1 queued request(s)"),
        "queued maintenance yield was not logged: {stderr}"
    );
}

#[test]
fn standalone_configure_replays_completed_task_before_queued_completion_drain() {
    const SESSION: &str = "standalone-replay-session";

    let project = tempfile::tempdir().expect("create replay project");
    let storage = tempfile::tempdir().expect("create replay storage");
    let configure = |id: &str| {
        json!({
            "id": id,
            "session_id": SESSION,
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "experimental": { "bash": { "background": true } }
            })),
            "max_background_bash_tasks": 4
        })
        .to_string()
    };

    let task_id = {
        let mut aft = AftProcess::spawn();
        let configured = aft.send(&configure("seed-configure"));
        assert_eq!(
            configured["success"], true,
            "seed configure failed: {configured:?}"
        );
        let spawned = aft.send(
            &json!({
                "id": "seed-background-task",
                "session_id": SESSION,
                "command": "bash",
                "params": { "command": "printf replay-before-drain", "background": true }
            })
            .to_string(),
        );
        assert_eq!(spawned["success"], true, "task spawn failed: {spawned:?}");
        let task_id = spawned["task_id"]
            .as_str()
            .expect("spawned task id")
            .to_string();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = aft.send(
                &json!({
                    "id": "seed-task-status",
                    "session_id": SESSION,
                    "command": "bash_status",
                    "params": { "task_id": task_id }
                })
                .to_string(),
            );
            assert_eq!(status["success"], true, "task status failed: {status:?}");
            if status["status"] == "completed" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "task did not complete: {status:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(aft.shutdown().success());
        task_id
    };

    let mut aft = AftProcess::spawn();
    let queued_drain = json!({
        "id": "queued-completion-drain",
        "session_id": SESSION,
        "command": "bash_drain_completions"
    })
    .to_string();
    aft.send_silent(&format!(
        "{}\n{}",
        configure("restart-configure"),
        queued_drain
    ));

    let (configured, _) = read_response(&mut aft, "restart-configure", Duration::from_secs(5));
    assert_eq!(
        configured["success"], true,
        "restart configure failed: {configured:?}"
    );
    let (drained, _) = read_response(&mut aft, "queued-completion-drain", Duration::from_secs(5));
    assert_eq!(
        drained["success"], true,
        "completion drain failed: {drained:?}"
    );
    assert!(
        drained["bg_completions"]
            .as_array()
            .expect("completion array")
            .iter()
            .any(|completion| completion["task_id"] == task_id),
        "queued drain missed replayed completion: {drained:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_storage_sweeps_run_detached_without_blocking_ping() {
    let project = tempfile::tempdir().expect("create detached-sweep project");
    let storage = tempfile::tempdir().expect("create shared storage fixture");
    let transient_root = tempfile::tempdir().expect("create transient cache root");
    let sweep_signal = project.path().join("sweep-started");
    seed_stale_storage_entries(storage.path(), transient_root.path(), 1);
    // Payload age no longer proves an abandoned checkout: durable retention
    // gives unknown roots an observation grace. Transient caches still have an
    // age-based sweep and exercise this detached configure maintenance worker.
    let orphan = transient_root
        .path()
        .join("aft-search-cache.0000000000000000.1");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("TMPDIR", transient_root.path().as_os_str()),
        ("TMP", transient_root.path().as_os_str()),
        ("TEMP", transient_root.path().as_os_str()),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_DELAY_MS",
            std::ffi::OsStr::new("750"),
        ),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_START_FILE",
            sweep_signal.as_os_str(),
        ),
    ]);
    let configure = aft.send(&configure_without_indexes(project.path()));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    let signal_deadline = Instant::now() + Duration::from_secs(5);
    while !sweep_signal.exists() {
        assert!(
            Instant::now() < signal_deadline,
            "detached storage sweep did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let ping_started = Instant::now();
    let ping = aft.send_with_timeout(
        r#"{"id":"sweep-ping","command":"ping"}"#,
        Duration::from_millis(500),
    );
    let ping_latency = ping_started.elapsed();
    assert_eq!(ping["command"], "pong", "ping failed: {ping:?}");
    assert!(
        ping_latency < Duration::from_millis(500),
        "ping waited for detached storage sweep for {ping_latency:?}"
    );

    let sweep_deadline = Instant::now() + Duration::from_secs(10);
    while orphan.exists() {
        assert!(
            Instant::now() < sweep_deadline,
            "transient cache sweep did not complete within ten seconds"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let (status, stderr) = aft.stderr_output();
    assert!(status.success());
    assert!(
        stderr.contains("transient search cache sweep") && stderr.contains("removed=1"),
        "detached transient cache sweep did not log its bounded effect: {stderr}"
    );
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set embedding read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = stream.read(&mut chunk).expect("read embedding request");
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
            break;
        }
    }
    String::from_utf8(bytes).expect("embedding request is utf-8")
}

fn write_embedding_response(stream: &mut TcpStream) {
    let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

/// A mock embedding backend that holds the query embedding open until the
/// test releases it. The hold is a gate rather than a sleep so the test proves
/// an ordering (sibling requests answered while the search is provably still
/// pending) instead of racing a fixed delay against runner load.
fn start_embedding_server() -> (
    String,
    mpsc::Receiver<()>,
    mpsc::SyncSender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
    let address = listener.local_addr().expect("embedding server address");
    let (query_started_tx, query_started_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let request = read_http_request(&mut stream);
            if request.contains(SEARCH_QUERY) {
                query_started_tx
                    .send(())
                    .expect("signal query embedding start");
                // Held until released, or until the test is gone; a dropped
                // sender releases too, so a panicking test cannot wedge this.
                let _ = release_rx.recv_timeout(Duration::from_secs(60));
                write_embedding_response(&mut stream);
                return;
            }
            write_embedding_response(&mut stream);
        }
        panic!("semantic query embedding did not reach the mock server");
    });
    (
        format!("http://{address}"),
        query_started_rx,
        release_tx,
        handle,
    )
}

#[test]
fn standalone_tool_call_read_finishes_before_slow_inspect() {
    let temp_dir = tempfile::tempdir().expect("create standalone inspect fixture");
    let project = temp_dir.path().join("project");
    let src = project.join("src");
    let storage = temp_dir.path().join("storage");
    fs::create_dir_all(&src).expect("create project source directory");
    fs::write(src.join("main.rs"), "fn main() {}\n").expect("write fixture source");
    for file_index in 0..2_000 {
        let mut source = String::with_capacity(1_500);
        for function_index in 0..20 {
            source.push_str(&format!(
                "pub fn fixture_{file_index}_{function_index}(value: usize) -> usize {{ value + {function_index} }}\n"
            ));
        }
        fs::write(src.join(format!("fixture_{file_index}.rs")), source)
            .expect("write inspect tier-2 fixture");
    }

    let mut aft = AftProcess::spawn();
    let configure = aft.send(
        &serde_json::to_string(&json!({
            "id": "configure-inspect-liveness",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.display().to_string(),
            "storage_dir": storage.display().to_string(),
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": true },
                "inspect": {"diagnostics_timeout_ms": 15000}
            }))
        }))
        .expect("serialize configure request"),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    // Poll rather than budget: the build's duration is the runner's business,
    // and a single long wait that returns `callgraph_building` reads as a
    // product failure when it is only a slow box (a contended Windows runner
    // exceeded a 120 s budget on this 2,000-file fixture, train 132). The
    // claim under test is the 3 s read liveness below, which keeps its bound.
    let warm_deadline = Instant::now() + Duration::from_secs(300);
    let mut callgraph;
    loop {
        callgraph = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": "warm-inspect-callgraph",
                "command": "callers",
                "file": src.join("main.rs").display().to_string(),
                "symbol": "main"
            }))
            .expect("serialize callgraph warmup"),
            Duration::from_secs(120),
        );
        if callgraph["success"] == true
            || callgraph["code"] != "callgraph_building"
            || Instant::now() >= warm_deadline
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(
        callgraph["success"], true,
        "callgraph warmup failed: {callgraph:#}"
    );

    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "slow-tool-call-inspect",
            "command": "tool_call",
            "name": "aft_inspect",
            "arguments": {"sections": ["diagnostics"]}
        }))
        .expect("serialize inspect tool call"),
    );
    let read_sent = Instant::now();
    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "sibling-tool-call-read",
            "command": "tool_call",
            "name": "read",
            "arguments": {"path": "src/main.rs"}
        }))
        .expect("serialize read tool call"),
    );

    let liveness = Duration::from_secs(3);
    let read_deadline = read_sent + liveness;
    let read = loop {
        let remaining = read_deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "read did not finish within the liveness bound while inspect was pending"
        );
        let Some(frame) = aft.try_read_next_timeout(remaining.min(Duration::from_millis(100)))
        else {
            continue;
        };
        assert_ne!(
            frame["id"], "slow-tool-call-inspect",
            "inspect answered before the sibling read: {frame:#}"
        );
        if frame["id"] == "sibling-tool-call-read" {
            break frame;
        }
    };
    assert_eq!(read["success"], true, "read failed: {read:#}");
    assert!(
        read_sent.elapsed() < liveness,
        "read exceeded the standalone liveness bound"
    );

    let cancel = aft.send_with_timeout(
        &serde_json::to_string(&json!({
            "id": "cancel-slow-inspect",
            "command": "cancel_request",
            "params": {"id": "slow-tool-call-inspect"}
        }))
        .expect("serialize inspect cancellation"),
        Duration::from_secs(3),
    );
    assert_eq!(cancel["success"], true, "cancel failed: {cancel:#}");
    assert_eq!(
        cancel["cancelled"], true,
        "cancel missed inspect: {cancel:#}"
    );

    let (inspect, inspect_finished) =
        read_response(&mut aft, "slow-tool-call-inspect", Duration::from_secs(15));
    assert!(
        inspect_finished > read_sent,
        "inspect must finish after read"
    );
    assert_eq!(
        inspect["success"], false,
        "inspect was not cancelled: {inspect:#}"
    );
    assert_eq!(
        inspect["inspect_terminal"], "interrupted",
        "inspect: {inspect:#}"
    );

    assert!(aft.shutdown().success());
}

/// How long the language-server work under test is stretched. Well past the
/// 3 s read liveness bound, so a read queued behind it cannot pass by luck.
const SLOW_LSP_WORK: Duration = Duration::from_secs(6);

fn wait_for_signal_file(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never started");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Send an `aft_inspect` with `arguments`, wait until `signal` shows it is
/// inside the stretched language-server work, then require a sibling `read`
/// of `read_path` to answer within the liveness bound and before the inspect.
/// Cancels the inspect afterwards.
fn assert_read_answers_during_inspect_lsp_work(
    aft: &mut AftProcess,
    signal: &Path,
    what: &str,
    read_path: &str,
    arguments: serde_json::Value,
) {
    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "inspect-in-lsp-work",
            "command": "tool_call",
            "name": "aft_inspect",
            "arguments": arguments
        }))
        .expect("serialize inspect tool call"),
    );
    wait_for_signal_file(signal, what);

    let read_sent = Instant::now();
    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "read-during-lsp-work",
            "command": "tool_call",
            "name": "read",
            "arguments": {"path": read_path}
        }))
        .expect("serialize read tool call"),
    );
    let liveness = Duration::from_secs(3);
    let read = loop {
        let remaining = (read_sent + liveness).saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "read did not answer within {liveness:?} while the inspect was in {what}"
        );
        let Some(frame) = aft.try_read_next_timeout(remaining.min(Duration::from_millis(100)))
        else {
            continue;
        };
        assert_ne!(
            frame["id"], "inspect-in-lsp-work",
            "inspect answered before the sibling read: {frame:#}"
        );
        if frame["id"] == "read-during-lsp-work" {
            break frame;
        }
    };
    assert_eq!(read["success"], true, "read failed: {read:#}");

    // A request that needs the language-server manager itself must not wait
    // behind the inspect's language-server work either.
    let lsp_request_sent = Instant::now();
    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "lsp-request-during-lsp-work",
            "command": "lsp_diagnostics"
        }))
        .expect("serialize lsp_diagnostics request"),
    );
    let lsp_response = loop {
        let remaining = (lsp_request_sent + liveness).saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "lsp_diagnostics did not answer within {liveness:?} while the inspect was in {what}"
        );
        let Some(frame) = aft.try_read_next_timeout(remaining.min(Duration::from_millis(100)))
        else {
            continue;
        };
        assert_ne!(
            frame["id"], "inspect-in-lsp-work",
            "inspect answered before the sibling lsp_diagnostics: {frame:#}"
        );
        if frame["id"] == "lsp-request-during-lsp-work" {
            break frame;
        }
    };
    assert_eq!(
        lsp_response["success"], true,
        "lsp_diagnostics failed: {lsp_response:#}"
    );

    let cancel = aft.send_with_timeout(
        &serde_json::to_string(&json!({
            "id": "cancel-inspect-in-lsp-work",
            "command": "cancel_request",
            "params": {"id": "inspect-in-lsp-work"}
        }))
        .expect("serialize inspect cancellation"),
        Duration::from_secs(3),
    );
    assert_eq!(cancel["success"], true, "cancel failed: {cancel:#}");
    let _ = read_response(
        aft,
        "inspect-in-lsp-work",
        SLOW_LSP_WORK + Duration::from_secs(30),
    );
}

/// An inspect walks the project to decide which language servers apply. The
/// walk used to run under the language-server manager lock, and the
/// standalone loop takes that lock between requests (LSP event drains,
/// configure maintenance, the status bar), so a read sent during the walk
/// waited for all of it.
#[test]
fn standalone_read_answers_while_inspect_walks_for_language_servers() {
    let temp_dir = tempfile::tempdir().expect("create fixture");
    let project = temp_dir.path().join("project");
    fs::create_dir_all(project.join("src")).expect("create project");
    fs::write(project.join("src/main.rs"), "fn main() {}\n").expect("write source");
    let signal = temp_dir.path().join("walk-started");
    let delay_ms = SLOW_LSP_WORK.as_millis().to_string();

    let mut aft = AftProcess::spawn_with_env(&[
        (
            "AFT_TEST_APPLICABILITY_WALK_DELAY_MS",
            std::ffi::OsStr::new(&delay_ms),
        ),
        ("AFT_TEST_APPLICABILITY_WALK_SIGNAL", signal.as_os_str()),
    ]);
    let configure = aft.send(&configure_without_indexes(&project));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    assert_read_answers_during_inspect_lsp_work(
        &mut aft,
        &signal,
        "the applicability walk",
        "src/main.rs",
        json!({"sections": ["diagnostics"]}),
    );
    assert!(aft.shutdown().success());
}

fn fake_lsp_server_path() -> PathBuf {
    crate::test_helpers::fake_lsp::fake_server_binary()
}

/// Starting a language server includes its `initialize` handshake, which can
/// take seconds. It used to run under the language-server manager lock, so a
/// read sent while an inspect started a server waited for the handshake.
#[test]
fn standalone_read_answers_while_inspect_initializes_a_language_server() {
    let temp_dir = tempfile::tempdir().expect("create fixture");
    let project = temp_dir.path().join("project");
    fs::create_dir_all(&project).expect("create project");
    fs::write(project.join("fake.toml"), "[project]\n").expect("write root marker");
    fs::write(project.join("main.fake"), "hello\n").expect("write fake source");
    fs::write(project.join("notes.txt"), "notes\n").expect("write read target");

    let fake_server = fake_lsp_server_path();
    let bin_dir = temp_dir.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create fake server dir");
    let binary_name = fake_server
        .file_name()
        .expect("fake server file name")
        .to_string_lossy()
        .to_string();
    let installed = bin_dir.join(&binary_name);
    fs::copy(&fake_server, &installed).expect("install fake server");
    // The fake LSP has no CLI probe; closed stdin makes its protocol loop exit.
    super::helpers::warm_executable(&installed, &[]);

    let signal = temp_dir.path().join("initialize-started");
    let delay_ms = SLOW_LSP_WORK.as_millis().to_string();
    let mut aft = AftProcess::spawn_with_env(&[
        (
            "AFT_FAKE_LSP_INIT_DELAY_MS",
            std::ffi::OsStr::new(&delay_ms),
        ),
        ("AFT_FAKE_LSP_INIT_DELAY_SIGNAL", signal.as_os_str()),
    ]);
    let configure = aft.send(
        &serde_json::to_string(&json!({
            "id": "configure-slow-initialize",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.display().to_string(),
            "lsp_paths_extra": [bin_dir.display().to_string()],
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "inspect": {"diagnostics_timeout_ms": 30000},
                "lsp": {
                    "servers": {
                        "fake": {
                            "extensions": ["fake"],
                            "binary": binary_name,
                            "args": [],
                            "root_markers": ["fake.toml"]
                        }
                    }
                }
            }))
        }))
        .expect("serialize configure request"),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    assert_read_answers_during_inspect_lsp_work(
        &mut aft,
        &signal,
        "a language server's initialize handshake",
        "notes.txt",
        json!({"sections": ["diagnostics"]}),
    );
    assert!(aft.shutdown().success());
}

/// A scoped inspect asks the language server for each scoped file's
/// diagnostics, and a server can take seconds to answer. The request used to
/// be waited for under the language-server manager lock, so a sibling read
/// or language-server request sent meanwhile waited for the answer.
#[test]
fn standalone_read_answers_while_scoped_inspect_waits_for_a_diagnostics_pull() {
    let temp_dir = tempfile::tempdir().expect("create fixture");
    let project = temp_dir.path().join("project");
    fs::create_dir_all(project.join("src")).expect("create project");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"pull-liveness\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    fs::write(project.join("src/lib.rs"), "pub fn f() {}\n").expect("write source");
    fs::write(project.join("notes.txt"), "notes\n").expect("write read target");

    let signal = temp_dir.path().join("pull-started");
    let delay_ms = SLOW_LSP_WORK.as_millis().to_string();
    let fake_server = fake_lsp_server_path();
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_LSP_RUST_BINARY", fake_server.as_os_str()),
        ("AFT_FAKE_LSP_PULL", std::ffi::OsStr::new("1")),
        (
            "AFT_FAKE_LSP_PULL_DELAY_MS",
            std::ffi::OsStr::new(&delay_ms),
        ),
        ("AFT_FAKE_LSP_PULL_DELAY_SIGNAL", signal.as_os_str()),
    ]);
    let configure = aft.send(
        &serde_json::to_string(&json!({
            "id": "configure-slow-pull",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.display().to_string(),
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "inspect": {"diagnostics_timeout_ms": 30000}
            }))
        }))
        .expect("serialize configure request"),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    assert_read_answers_during_inspect_lsp_work(
        &mut aft,
        &signal,
        "a diagnostics pull",
        "notes.txt",
        json!({"sections": ["diagnostics"], "scope": "src"}),
    );
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_ndjson_status_and_cancel_proceed_while_search_is_pending() {
    let project = tempfile::tempdir().expect("create standalone project");
    let storage = tempfile::tempdir().expect("create standalone storage");
    let (base_url, query_started, release_query, embedding_server) = start_embedding_server();
    let mut aft = AftProcess::spawn();

    let configure = aft.send(
        &serde_json::to_string(&json!({
            "id": "configure-search",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path().display().to_string(),
            "storage_dir": storage.path().display().to_string(),
            "config": user_config(json!({
                "indexes": { "semantic": true },
                "semantic": {
                    "backend": "openai_compatible",
                    "model": "test-embedding",
                    "base_url": base_url,
                    "query_timeout_ms": 3000
                }
            }))
        }))
        .expect("serialize configure request"),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );

    // Wait for the semantic index to reach Ready before issuing the search.
    // A search sent while the index is still building takes the building-reply
    // arm and never starts a query embedding, so the query_started signal this
    // test blocks on would time out - exactly what happened on loaded CI
    // shards where the tiny fixture index was not yet built.
    let ready_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = aft.send(
            &serde_json::to_string(&json!({"id": "ready-poll", "command": "status"}))
                .expect("serialize status request"),
        );
        if status["semantic_index"]["status"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "semantic index never became ready before the search: {status:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "slow-search",
            "command": "semantic_search",
            "query": SEARCH_QUERY
        }))
        .expect("serialize search request"),
    );
    query_started
        .recv_timeout(Duration::from_secs(10))
        .expect("standalone query embedding starts");

    // The remote is gated (not released yet), so the search is provably
    // pending while the sibling requests below are answered: an answer that
    // arrives at all is an answer that did not wait behind the embedding.
    // The bounds are liveness bounds for a contended runner, not latency
    // claims; `send_with_timeout` fails the test if either is exceeded.
    let liveness = Duration::from_secs(30);
    let status = aft.send_with_timeout(
        &serde_json::to_string(&json!({"id": "sibling-status", "command": "status"}))
            .expect("serialize status request"),
        liveness,
    );
    assert_eq!(
        status["id"], "sibling-status",
        "search blocked status: {status:?}"
    );
    assert_eq!(status["success"], true);

    let cancel = aft.send_with_timeout(
        &serde_json::to_string(&json!({
            "id": "cancel-search",
            "command": "cancel_request",
            "params": {"id": "slow-search"}
        }))
        .expect("serialize cancel request"),
        liveness,
    );
    assert_eq!(cancel["success"], true, "cancel command failed: {cancel:?}");
    assert_eq!(cancel["cancelled"], true);

    // The cancelled search must resolve while the remote is still held: the
    // `request_cancelled` code is only reachable that way, because a released
    // remote would produce a real result instead.
    let deadline = Instant::now() + liveness;
    let cancelled = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "cancelled search response did not arrive within the liveness bound"
        );
        let Some(frame) = aft.try_read_next_timeout(remaining.min(Duration::from_millis(100)))
        else {
            continue;
        };
        if frame["id"] == "slow-search" {
            break frame;
        }
    };
    assert_eq!(cancelled["success"], false);
    assert_eq!(cancelled["code"], "request_cancelled");

    // Only now let the remote answer, so the mock thread can finish.
    let _ = release_query.send(());
    let status = aft.shutdown();
    assert!(status.success());
    embedding_server.join().expect("embedding server joins");
}

#[cfg(unix)]
#[test]
fn standalone_edit_then_queued_grep_observes_watcher_update() {
    let project = tempfile::tempdir().expect("create freshness project");
    let storage = tempfile::tempdir().expect("create freshness storage");
    let target = project.path().join("source.ts");
    fs::write(&target, "export const value = 'old_watcher_token';\n")
        .expect("write freshness fixture");

    let formatter_dir = project.path().join("bin");
    fs::create_dir_all(&formatter_dir).expect("create formatter directory");
    let formatter = formatter_dir.join("biome");
    fs::write(
        &formatter,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'biome 2.0.0'; exit 0; fi\nsleep 0.4\n",
    )
    .expect("write formatter shim");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&formatter, fs::Permissions::from_mode(0o755))
        .expect("make formatter executable");
    let formatter_path = std::env::join_paths(
        std::iter::once(formatter_dir.as_os_str().to_os_string()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(Into::into),
        ),
    )
    .expect("construct formatter PATH");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("0")),
        (
            "AFT_TEST_SYNC_FILE_WATCHER_START",
            std::ffi::OsStr::new("1"),
        ),
        ("PATH", formatter_path.as_os_str()),
    ]);
    let configure = serde_json::to_string(&json!({
        "id": "freshness-configure",
        "command": "configure",
        "harness": "opencode",
        "project_root": project.path().display().to_string(),
        "config": user_config(json!({
            "indexes": { "trigram": true, "semantic": false, "callgraph": false },
            "format_on_edit": true,
            "formatter": { "typescript": "biome" }
        }))
    }))
    .expect("serialize freshness configure");
    let configured = aft.send(&configure);
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );

    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        thread::sleep(Duration::from_millis(125));
        let status = aft.send(r#"{"id":"freshness-status","command":"status"}"#);
        if status["search_index"]["status"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "search index did not become ready: {status:?}"
        );
    }
    let initial = aft.send(
        r#"{"id":"freshness-initial","command":"grep","pattern":"old_watcher_token","max_results":20}"#,
    );
    assert!(
        initial["matches"]
            .as_array()
            .is_some_and(|matches| !matches.is_empty()),
        "initial search index did not include the fixture: {initial:?}"
    );
    // The search index can become ready before the file watcher is attached, so
    // wait briefly for watcher setup to finish.
    thread::sleep(Duration::from_secs(1));

    let edit = serde_json::to_string(&json!({
        "id": "freshness-edit",
        "command": "tool_call",
        "session_id": "freshness-session",
        "name": "edit",
        "arguments": {
            "filePath": "source.ts",
            "edits": [{
                "oldString": "old_watcher_token",
                "newString": "fresh_watcher_token"
            }]
        }
    }))
    .expect("serialize edit request");
    let grep = serde_json::to_string(&json!({
        "id": "freshness-grep",
        "command": "grep",
        "pattern": "fresh_watcher_token",
        "max_results": 20
    }))
    .expect("serialize grep request");
    aft.send_silent(&edit);
    aft.send_silent(&grep);

    let (edit_response, _) = read_response(&mut aft, "freshness-edit", Duration::from_secs(5));
    assert_eq!(
        edit_response["success"], true,
        "edit failed: {edit_response:?}"
    );
    let (grep_response, _) = read_response(&mut aft, "freshness-grep", Duration::from_secs(5));
    assert_eq!(
        grep_response["success"], true,
        "grep failed: {grep_response:?}"
    );
    assert!(
        grep_response["matches"]
            .as_array()
            .is_some_and(|matches| !matches.is_empty()),
        "queued grep did not observe the edit: {grep_response:?}"
    );

    assert!(aft.shutdown().success());
}

fn warming_rust_inspect_response(
    cargo_owned: bool,
    diagnostics_timeout_ms: u64,
    rust_env: Value,
) -> (Value, Duration) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    fs::create_dir_all(project.join("crates")).unwrap();
    let root_marker = if cargo_owned {
        "Cargo.toml"
    } else {
        "fake.toml"
    };
    if cargo_owned {
        fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"partial-inspect\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"crates/lib.rs\"\n",
    )
    .unwrap();
        fs::write(
            project.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"partial-inspect\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
    } else {
        fs::write(project.join("fake.toml"), "").unwrap();
    }
    fs::write(
        project.join("crates/lib.rs"),
        "// TODO: check indexing\npub fn value() {}\n",
    )
    .unwrap();
    fs::write(project.join("package.json"), "{}").unwrap();
    fs::write(project.join("outside.ts"), "export const value = 1;\n").unwrap();
    let binary = fake_lsp_server_path();
    let mut aft =
        AftProcess::spawn_with_env(&[("AFT_FAKE_LSP_SERVER_STATUS", std::ffi::OsStr::new("1"))]);
    let configured = aft.send(
        &json!({
            "id": "configure-partial", "command": "configure", "harness": "opencode",
            "project_root": project, "storage_dir": temp.path().join("storage"),
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "inspect": {"diagnostics_timeout_ms": diagnostics_timeout_ms},
                "lsp": {"servers": {
                    "rust": {"binary": binary, "args": [], "root_markers": [root_marker], "env": rust_env},
                    "typescript": {"binary": binary, "args": []}
                }}
            }))
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:#}");
    let started = Instant::now();
    let response = aft.send_with_timeout(
        &json!({
            "id": "partial-inspect", "command": "inspect", "scope": "crates"
        })
        .to_string(),
        Duration::from_millis(diagnostics_timeout_ms) + Duration::from_secs(5),
    );
    eprintln!(
        "partial inspect elapsed={:?} response={response:#}",
        started.elapsed()
    );
    let elapsed = started.elapsed();
    assert!(aft.shutdown().success());
    (response, elapsed)
}

#[test]
fn standalone_inspect_preserves_partial_results_when_rust_keeps_indexing() {
    // Without Cargo.toml, an analyzer still indexing cannot confirm complete diagnostics.
    let (response, elapsed) = warming_rust_inspect_response(false, 10_000, json!({}));
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], false);
    assert!(
        !response.to_string().contains("\"producer\":\"typescript\""),
        "{response:#}"
    );
    assert!(response["text"]
        .as_str()
        .unwrap()
        .contains("still indexing after"));
    assert!(response["summary"]["diagnostics"]["errors"].is_null());
    // A scoped inspect no longer writes its scoped counts into the project
    // status bar, so on a fresh project with nothing known project-wide the
    // bar may be absent. When it is shown, diagnostics must read as unknown,
    // never as a made-up numeric error count.
    let text = response["text"].as_str().unwrap();
    assert!(
        !text.contains("[AFT ") || text.contains("[AFT E? W?"),
        "{response:#}"
    );
    assert!(!text.contains("[AFT E0"), "{response:#}");
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn standalone_inspect_completed_cargo_check_is_fresh_while_analyzer_is_warming() {
    // A completed explicit Cargo check supplies diagnostics while the analyzer is still warming.
    let (response, elapsed) = warming_rust_inspect_response(true, 10_000, json!({}));
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], true, "{response:#}");

    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 0,
        "{response:#}"
    );
    let text = response["text"].as_str().unwrap();
    assert!(text.starts_with("FRESH"), "{text}");
    assert!(text.contains("workspace analysis is warming"), "{text}");
    assert!(text.contains("from the last completed check"), "{text}");
    assert!(elapsed < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn standalone_inspect_runs_cargo_check_while_the_analyzer_warms() {
    use std::os::unix::fs::PermissionsExt;

    // Inspect's own cargo check must run while inspect waits for the analyzer
    // to settle, not after. With a 36 s budget (31 s of work time) that wait
    // lasts about 15 s for an analyzer that never settles; a check started
    // only after it would have about 7.5 s. The cargo below spends 9 s before
    // checking anything, so only a check that ran during the wait can finish.
    let temp = tempfile::tempdir().unwrap();
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let slow_cargo = temp.path().join("slow-cargo");
    fs::write(
        &slow_cargo,
        format!(
            "#!/bin/sh\nif [ \"$1\" = check ]; then sleep 9; fi\nunset CARGO\nexec '{}' \"$@\"\n",
            Path::new(&real_cargo).display()
        ),
    )
    .unwrap();
    fs::set_permissions(&slow_cargo, fs::Permissions::from_mode(0o755)).unwrap();

    let (response, _) = warming_rust_inspect_response(true, 36_000, json!({ "CARGO": slow_cargo }));
    assert_eq!(response["complete"], true, "{response:#}");
    let text = response["text"].as_str().unwrap();
    assert!(text.starts_with("FRESH"), "{text}");
    assert!(text.contains("from the last completed check"), "{text}");
}

/// A search against an already-ready index is answered as soon as its worker
/// finishes, not at the standalone loop's next periodic poll of pending
/// responses. The poll interval is stretched to a minute here, so a response
/// that waited for the poll instead of being woken by its worker would miss
/// the ten-second bound on every search below.
#[test]
fn standalone_search_on_ready_index_answers_without_waiting_for_the_pending_poll() {
    let temp_dir = tempfile::tempdir().expect("create fixture");
    let project = temp_dir.path().join("project");
    let storage = temp_dir.path().join("storage");
    fs::create_dir_all(project.join("src")).expect("create project");
    fs::write(
        project.join("src/lib.rs"),
        "/// Rebuilds the trigram index after a watched file changes.\n\
         pub fn rebuild_trigram_index(changed: &str) -> usize { changed.len() }\n\
         pub struct PendingReplyQueue { pub depth: usize }\n",
    )
    .expect("write source");
    fs::write(
        project.join("src/render.rs"),
        "/// Formats ranked search rows for the agent.\n\
         pub fn render_ranked_rows(rows: &[String]) -> String { rows.join(\"\\n\") }\n",
    )
    .expect("write source");

    let mut aft = AftProcess::spawn_with_env(&[(
        "AFT_TEST_PENDING_POLL_INTERVAL_MS",
        std::ffi::OsStr::new("60000"),
    )]);
    let configure = aft.send(
        &serde_json::to_string(&json!({
            "id": "configure-ready-search",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.display().to_string(),
            "storage_dir": storage.display().to_string(),
            "config": user_config(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false }
            }))
        }))
        .expect("serialize configure request"),
    );
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:#}"
    );

    let ready_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = aft.send(r#"{"id":"ready-poll","command":"status"}"#);
        if status["search_index"]["status"] == "ready" {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "search index never became ready: {status:#}"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let queries = [
        "how is the trigram index rebuilt after a file changes",
        "where are ranked search rows formatted",
        "rebuild_trigram_index",
        "PendingReplyQueue",
        "render_ranked_rows",
    ];
    for (index, query) in queries.iter().enumerate() {
        let id = format!("ready-search-{index}");
        let response = aft.send_with_timeout(
            &serde_json::to_string(&json!({
                "id": id,
                "command": "semantic_search",
                "query": query,
                "top_k": 5
            }))
            .expect("serialize search request"),
            Duration::from_secs(10),
        );
        assert_eq!(response["id"], id, "unexpected frame: {response:#}");
        assert_eq!(
            response["success"], true,
            "search {query:?} failed: {response:#}"
        );
    }
    assert!(aft.shutdown().success());
}
