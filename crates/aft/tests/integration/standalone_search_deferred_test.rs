use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use filetime::{set_file_mtime, FileTime};
use serde_json::{json, Value};

#[cfg(unix)]
use super::helpers::link_cached_executable;
use super::helpers::{link_cached_path, user_config, AftProcess};

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
    let stale_entries = seed_stale_storage_entries(storage.path(), transient_root.path(), 200);
    let sweep_release = storage.path().join("sweep-release");
    let sweep_started = storage.path().join("sweep-started");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("TMPDIR", transient_root.path().as_os_str()),
        ("TMP", transient_root.path().as_os_str()),
        ("TEMP", transient_root.path().as_os_str()),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_RELEASE_FILE",
            sweep_release.as_os_str(),
        ),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_START_FILE",
            sweep_started.as_os_str(),
        ),
        (
            "AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT",
            std::ffi::OsStr::new("2"),
        ),
    ]);
    aft.send_silent(&configure_without_indexes(project.path()));
    aft.send_silent(r#"{"id":"queued-ping","command":"ping"}"#);

    let (configure, _) = read_response(&mut aft, "configure-maintenance", Duration::from_secs(5));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let (ping, _) = read_response(&mut aft, "queued-ping", Duration::from_secs(2));
    assert_eq!(ping["command"], "pong", "ping failed: {ping:?}");
    let gate_deadline = Instant::now() + Duration::from_secs(5);
    while !sweep_started.exists() {
        assert!(
            Instant::now() < gate_deadline,
            "storage sweep did not reach its gate"
        );
        thread::sleep(Duration::from_millis(10));
    }
    aft.send_silent(r#"{"id":"gated-ping","command":"ping"}"#);
    let (gated_ping, _) = read_response(&mut aft, "gated-ping", Duration::from_secs(2));
    assert_eq!(gated_ping["command"], "pong", "ping failed: {gated_ping:?}");
    // The sweep cannot finish until this release. A pong received before it
    // proves non-blocking service regardless of how long CI deschedules us.
    assert!(!sweep_release.exists());
    assert!(stale_entries.iter().all(|entry| entry.exists()));
    fs::write(&sweep_release, "release\n").unwrap();

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
    let sweep_release = project.path().join("sweep-release");
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
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_RELEASE_FILE",
            sweep_release.as_os_str(),
        ),
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_START_FILE",
            sweep_signal.as_os_str(),
        ),
        (
            "AFT_TEST_STANDALONE_INITIAL_REQUEST_COUNT",
            std::ffi::OsStr::new("2"),
        ),
    ]);
    let _release_on_drop = super::helpers::ReleaseOnDrop::new(&sweep_release);
    // Keep unrelated view-store loading out of the storage-sweep readiness
    // condition, and queue input before configure can start its maintenance.
    aft.send_silent(
        &serde_json::to_string(&json!({
            "id": "configure-maintenance",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(json!({
                "indexes": { "trigram": false, "semantic": false, "callgraph": false },
                "views": { "enabled": false }
            }))
        }))
        .unwrap(),
    );
    aft.send_silent(r#"{"id":"sweep-queued-ping","command":"ping"}"#);
    let (configure, _) = read_response(&mut aft, "configure-maintenance", Duration::from_secs(60));
    assert_eq!(
        configure["success"], true,
        "configure failed: {configure:?}"
    );
    let (queued_ping, _) = read_response(&mut aft, "sweep-queued-ping", Duration::from_secs(60));
    assert_eq!(queued_ping["command"], "pong", "{queued_ping:?}");

    let signal_deadline = Instant::now() + Duration::from_secs(5);
    while !sweep_signal.exists() {
        assert!(
            Instant::now() < signal_deadline,
            "detached storage sweep did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let ping = aft.send(r#"{"id":"sweep-ping","command":"ping"}"#);
    assert_eq!(ping["command"], "pong", "ping failed: {ping:?}");
    // A pong before the sweep can finish proves detached service, independent
    // of how long process startup or scheduling takes on the runner.
    assert!(!sweep_release.exists());
    assert!(
        orphan.exists(),
        "the held sweep must not remove its cache yet"
    );
    fs::write(&sweep_release, "release\n").unwrap();

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
    link_cached_path(&fake_server, &installed).expect("link cached fake server");
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
    assert_edit_then_queued_grep_is_fresh(true);
}

#[cfg(unix)]
#[test]
fn standalone_edit_then_queued_grep_is_fresh_without_watcher_delivery() {
    // Withhold OS delivery entirely: read-after-write must not depend on when
    // FSEvents or inotify happens to report AFT's own completed edit.
    assert_edit_then_queued_grep_is_fresh(false);
}

#[cfg(unix)]
fn assert_edit_then_queued_grep_is_fresh(watcher_enabled: bool) {
    let project = tempfile::tempdir().expect("create freshness project");
    let storage = tempfile::tempdir().expect("create freshness storage");
    let target = project.path().join("source.ts");
    fs::write(&target, "export const value = 'old_watcher_token';\n")
        .expect("write freshness fixture");

    let formatter_listener = TcpListener::bind("127.0.0.1:0").expect("bind formatter barrier");
    let formatter_port = formatter_listener.local_addr().unwrap().port().to_string();
    let (formatter_tx, formatter_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = formatter_listener.accept().expect("accept formatter");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut ready = [0];
        stream
            .read_exact(&mut ready)
            .expect("formatter ready signal");
        assert_eq!(ready, [b'R']);
        let _ = formatter_tx.send(stream);
    });
    let formatter_dir = project.path().join("bin");
    fs::create_dir_all(&formatter_dir).expect("create formatter directory");
    let formatter = formatter_dir.join("biome");
    link_cached_executable(
        &formatter,
        b"#!/bin/bash\nif [ \"$1\" = \"--version\" ]; then echo 'biome 2.0.0'; exit 0; fi\nexec 3<>/dev/tcp/127.0.0.1/$AFT_TEST_FORMATTER_PORT || exit 1\nprintf R >&3\nread -r -t 5 release <&3\n",
    )
    .expect("cache formatter shim");
    let formatter_path = std::env::join_paths(
        std::iter::once(formatter_dir.as_os_str().to_os_string()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).map(Into::into),
        ),
    )
    .expect("construct formatter PATH");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        (
            "AFT_TEST_DISABLE_FILE_WATCHER",
            std::ffi::OsStr::new(if watcher_enabled { "0" } else { "1" }),
        ),
        (
            "AFT_TEST_SYNC_FILE_WATCHER_START",
            std::ffi::OsStr::new("1"),
        ),
        (
            "AFT_TEST_FORMATTER_PORT",
            std::ffi::OsStr::new(&formatter_port),
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
    let mut formatter = formatter_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("edit did not reach formatter barrier");
    assert!(fs::read_to_string(&target)
        .unwrap()
        .contains("fresh_watcher_token"));
    // Queue the query while the edit handler is still in its formatter, then
    // allow the edit to finish. No OS notification or timing guess orders this.
    aft.send_silent(&grep);
    formatter
        .write_all(b"release\n")
        .expect("release formatter");

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
    assert_eq!(grep_response["total_matches"], 1, "{grep_response:?}");
    assert_eq!(grep_response["files_searched"], 1, "{grep_response:?}");
    if !watcher_enabled {
        assert_eq!(grep_response["complete"], true, "{grep_response:?}");
    }
    let old = aft.send(
        r#"{"id":"freshness-old","command":"grep","pattern":"old_watcher_token","max_results":20}"#,
    );
    assert_eq!(old["total_matches"], 0, "stale entry survived: {old:?}");

    assert!(aft.shutdown().success());
}

fn mutation_freshness_fixture() -> (tempfile::TempDir, tempfile::TempDir, AftProcess) {
    let mut fixture =
        mutation_freshness_fixture_with_content("export const value = 'qqqq_before_mutation';\n");
    assert_mutation_freshness(
        &mut fixture.2,
        "qqqq_before_mutation",
        "zzzz_after_mutation",
    );
    fixture
}

#[test]
fn standalone_back_to_back_write_then_grep_outline_zoom_observe_write() {
    let (_project, _storage, mut aft) =
        mutation_freshness_fixture_with_content("export function before_write() { return 1; }\n");
    // Populate parser caches before replacing the file through the protocol.
    let warm = aft.send(r#"{"id":"warm","command":"outline","file":"src/source.ts"}"#);
    assert_eq!(warm["success"], true, "{warm:?}");
    for wrapped in [false, true] {
        let symbol = if wrapped {
            "after_wrapped_write"
        } else {
            "after_bare_write"
        };
        aft.send_silent(&json!({
            "id":"write", "command":"write", "session_id":"ordered-session",
            "file":"src/source.ts", "content":format!("export function {symbol}() {{ return 2; }}\n"),
            "validate":"off"
        }).to_string());
        for (command, params) in [
            ("grep", json!({"path":"src/source.ts", "pattern":symbol})),
            (
                "outline",
                if wrapped {
                    json!({"target":"src/source.ts"})
                } else {
                    json!({"file":"src/source.ts"})
                },
            ),
            (
                "zoom",
                if wrapped {
                    json!({"batch":[{"target":"src/source.ts", "symbols":symbol}]})
                } else {
                    json!({"file":"src/source.ts", "symbol":symbol})
                },
            ),
        ] {
            let request = if wrapped {
                json!({"id":command, "command":"tool_call", "session_id":"ordered-session", "name":command, "arguments":params})
            } else {
                let mut request = params;
                request["id"] = json!(command);
                request["command"] = json!(command);
                request["session_id"] = json!("ordered-session");
                request
            };
            aft.send_silent(&request.to_string());
        }
        let mut remaining = std::collections::HashSet::from(["write", "grep", "outline", "zoom"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !remaining.is_empty() {
            let frame = aft.try_read_next_timeout(Duration::from_millis(100));
            assert!(Instant::now() < deadline, "missing replies: {remaining:?}");
            let Some(frame) = frame else { continue };
            let Some(id) = frame["id"].as_str() else {
                continue;
            };
            if remaining.remove(id) {
                assert_eq!(frame["success"], true, "{frame:?}");
                if id != "write" {
                    assert!(frame.to_string().contains(symbol), "stale {id}: {frame:?}");
                }
            }
        }
    }
    assert!(aft.shutdown().success());
}

fn mutation_freshness_fixture_with_content(
    content: &str,
) -> (tempfile::TempDir, tempfile::TempDir, AftProcess) {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/source.ts"), content).unwrap();
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("AFT_TEST_DISABLE_FILE_WATCHER", std::ffi::OsStr::new("1")),
    ]);
    let configured = aft.send(
        &json!({
            "id": "configure", "command": "configure", "harness": "opencode",
            "project_root": project.path(),
            "config": user_config(json!({
                "indexes": { "trigram": true, "semantic": false, "callgraph": false },
                "format_on_edit": false
            }))
        })
        .to_string(),
    );
    assert_eq!(configured["success"], true, "{configured:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = aft.send(r#"{"id":"status","command":"status"}"#);
        if status["search_index"]["status"] == "ready" {
            break;
        }
        assert!(Instant::now() < deadline, "index not ready: {status:?}");
    }
    (project, storage, aft)
}

#[test]
fn standalone_edit_then_grep_regex_does_not_return_replaced_text() {
    let before = "if unsafe { libc::ioctl(fd, 0xc0045878 as libc::c_ulong, 0) } != 0 {\n";
    let (project, _storage, mut aft) = mutation_freshness_fixture_with_content(before);
    let target = project.path().join("src/source.ts");
    let after = before.replace("libc::c_ulong", "libc::Ioctl");
    let grep = |aft: &mut AftProcess, pattern: &str| {
        aft.send(
            &json!({
                "id": "regex-grep", "command": "tool_call", "session_id": "regex-freshness",
                "name": "grep", "arguments": { "pattern": pattern, "path": "src/" }
            })
            .to_string(),
        )
    };
    let pattern = r"ioctl\([^;]*as libc::c_ulong";
    let initial = grep(&mut aft, pattern);
    assert_eq!(initial["total_matches"], 1, "{initial:?}");
    assert_eq!(initial["index_status"], "Ready", "{initial:?}");
    assert_eq!(
        initial["matches"][0]["line_text"],
        before.trim_end(),
        "{initial:?}"
    );
    let edit = aft.send(
        &json!({
            "id": "regex-edit", "command": "tool_call", "session_id": "regex-freshness",
            "name": "edit", "arguments": {
                "path": "src/source.ts",
                "edits": [{ "oldString": "libc::c_ulong", "newString": "libc::Ioctl" }]
            }
        })
        .to_string(),
    );
    assert_eq!(edit["success"], true, "{edit:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), after);
    let old = grep(&mut aft, pattern);
    assert_eq!(old["success"], true, "{old:?}");
    assert_eq!(
        old["total_matches"], 0,
        "replaced regex match survived: {old:?}"
    );
    let literal_old = grep(&mut aft, "libc::c_ulong");
    assert_eq!(literal_old["total_matches"], 0, "{literal_old:?}");
    let fresh = grep(&mut aft, r"ioctl\([^;]*as libc::Ioctl");
    assert_eq!(fresh["total_matches"], 1, "{fresh:?}");
    assert_eq!(
        fresh["matches"][0]["line_text"],
        after.trim_end(),
        "{fresh:?}"
    );
    assert!(aft.shutdown().success());
}

fn assert_mutation_freshness(aft: &mut AftProcess, present: &str, absent: &str) -> Value {
    let grep = |aft: &mut AftProcess, pattern| {
        aft.send(
            &json!({
                "id": "freshness", "command": "grep", "pattern": pattern, "max_results": 20
            })
            .to_string(),
        )
    };
    let fresh = grep(aft, present);
    assert_eq!(fresh["success"], true, "{fresh:?}");
    assert_eq!(fresh["complete"], true, "{fresh:?}");
    assert!(
        fresh["total_matches"].as_u64().unwrap() > 0,
        "new token missing: {fresh:?}"
    );
    assert!(
        fresh["files_searched"].as_u64().unwrap() > 0,
        "no candidates searched: {fresh:?}"
    );
    let old = grep(aft, absent);
    assert_eq!(old["success"], true, "{old:?}");
    assert_eq!(old["total_matches"], 0, "old token survived: {old:?}");
    // Disjoint tokens also detect outdated candidates after a content rewrite.
    // Deleted-path membership is checked directly by the mutation-guard unit test.
    assert_eq!(
        old["files_searched"], 0,
        "stale index candidate survived: {old:?}"
    );
    fresh
}

#[test]
fn standalone_glob_edit_is_fresh_without_watcher_delivery() {
    let (project, _storage, mut aft) = mutation_freshness_fixture();
    let seeded = aft.send(
        &json!({
            "id": "write-second", "command": "write", "file": project.path().join("src/second.ts"),
            "content": "export const value = 'qqqq_before_mutation';\n"
        })
        .to_string(),
    );
    assert_eq!(seeded["success"], true, "{seeded:?}");
    let edited = aft.send(
        &json!({
            "id": "glob-edit", "command": "edit_match", "file": "src/*.ts",
            "match": "qqqq_before_mutation", "replacement": "zzzz_after_mutation"
        })
        .to_string(),
    );
    assert_eq!(edited["success"], true, "{edited:?}");
    let fresh = assert_mutation_freshness(&mut aft, "zzzz_after_mutation", "qqqq_before_mutation");
    assert_eq!(fresh["total_matches"], 2, "{fresh:?}");
    assert_eq!(fresh["files_searched"], 2, "{fresh:?}");
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_patch_move_delete_is_fresh_without_watcher_delivery() {
    let (project, _storage, mut aft) = mutation_freshness_fixture();
    let moved = aft.send(&json!({
        "id": "patch-move", "command": "apply_patch",
        "patch_text": "*** Begin Patch\n*** Update File: src/source.ts\n*** Move to: src/moved.ts\n@@\n-export const value = 'qqqq_before_mutation';\n+export const value = 'zzzz_after_mutation';\n*** End Patch"
    }).to_string());
    assert_eq!(moved["success"], true, "{moved:?}");
    assert!(!project.path().join("src/source.ts").exists());
    let fresh = assert_mutation_freshness(&mut aft, "zzzz_after_mutation", "qqqq_before_mutation");
    assert!(
        fresh["matches"][0]["file"]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("src/moved.ts"),
        "{fresh:?}"
    );
    let deleted = aft.send(&json!({
        "id": "patch-delete", "command": "apply_patch",
        "patch_text": "*** Begin Patch\n*** Delete File: src/moved.ts\n*** Add File: src/added.ts\n+export const value = 'yyyy_after_delete';\n*** End Patch"
    }).to_string());
    assert_eq!(deleted["success"], true, "{deleted:?}");
    assert_mutation_freshness(&mut aft, "yyyy_after_delete", "zzzz_after_mutation");
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_patch_delete_is_fresh_without_watcher_delivery() {
    let (project, _storage, mut aft) = mutation_freshness_fixture();
    let written = aft.send(&json!({
        "id": "write-survivor", "command": "write", "file": project.path().join("src/survivor.ts"),
        "content": "export const value = 'zzzz_after_mutation';\n"
    }).to_string());
    assert_eq!(written["success"], true, "{written:?}");
    let deleted = aft.send(
        &json!({
            "id": "delete-only", "command": "apply_patch",
            "patch_text": "*** Begin Patch\n*** Delete File: src/source.ts\n*** End Patch"
        })
        .to_string(),
    );
    assert_eq!(deleted["success"], true, "{deleted:?}");
    assert_mutation_freshness(&mut aft, "zzzz_after_mutation", "qqqq_before_mutation");
    let glob = aft.send(r#"{"id":"removed-path","command":"glob","pattern":"**/source.ts"}"#);
    assert_eq!(
        glob["files"],
        json!([]),
        "deleted index entry survived: {glob:?}"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_undo_is_fresh_without_watcher_delivery() {
    let (project, _storage, mut aft) = mutation_freshness_fixture();
    let written = aft.send(
        &json!({
            "id": "write", "command": "write", "file": project.path().join("src/source.ts"),
            "content": "export const value = 'zzzz_after_mutation';\n"
        })
        .to_string(),
    );
    assert_eq!(written["success"], true, "{written:?}");
    assert_mutation_freshness(&mut aft, "zzzz_after_mutation", "qqqq_before_mutation");
    let undone = aft.send(r#"{"id":"undo","command":"undo","file":"src/source.ts"}"#);
    assert_eq!(undone["success"], true, "{undone:?}");
    assert_mutation_freshness(&mut aft, "qqqq_before_mutation", "zzzz_after_mutation");
    assert!(aft.shutdown().success());
}

/// How long the warming-analyzer tests allow inspect to wait for diagnostics.
const WARMING_DIAGNOSTICS_BUDGET_MS: u64 = 10_000;

/// A warming analyzer must not make inspect wait out its diagnostics budget.
/// The bound is that budget itself, so a loaded runner's slower reply still
/// passes while a reply that waited the full budget fails.
fn assert_returned_without_waiting_for_diagnostics(elapsed: Duration) {
    assert!(
        elapsed < Duration::from_millis(WARMING_DIAGNOSTICS_BUDGET_MS),
        "inspect waited {elapsed:?}, as long as its {WARMING_DIAGNOSTICS_BUDGET_MS} ms diagnostics budget"
    );
}

fn warming_rust_inspect_response(
    cargo_owned: bool,
    diagnostics_timeout_ms: u64,
    mut rust_env: Value,
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
    let check_marker = temp.path().join("cargo-checked");
    if rust_env.get("AFT_FAKE_CARGO_CHECK").is_some() {
        // This fixture checks completed-check/LSP overlap, not compiler speed.
        // Real compiler diagnostics are covered by the cargo-check tests.
        rust_env["CARGO"] = json!(binary);
        rust_env["RUSTC"] = json!(binary);
        rust_env["AFT_FAKE_CARGO_CHECK_MARKER"] = json!(check_marker);
    }
    let scanners_ready = temp.path().join("scanners-ready");
    fs::create_dir(&scanners_ready).unwrap();
    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_FAKE_LSP_SERVER_STATUS", std::ffi::OsStr::new("1")),
        (
            "AFT_TEST_INSPECT_SCANNER_READY_DIR",
            scanners_ready.as_os_str(),
        ),
    ]);
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
    // Metrics and TODO parsing have independent cold-start work. Wait for their
    // actual producers before timing diagnostics; a completed Cargo check does
    // not make those scanners ready. This pass does not start rust-analyzer.
    let warm_scanners = aft.send_with_timeout(
        &json!({
            "id": "warm-scanners", "command": "inspect", "scope": "crates",
            "sections": ["metrics", "todos"]
        })
        .to_string(),
        Duration::from_millis(diagnostics_timeout_ms) + Duration::from_secs(5),
    );
    assert_eq!(warm_scanners["success"], true, "{warm_scanners:#}");
    let readiness_deadline = Instant::now() + Duration::from_secs(60);
    for category in ["metrics", "todos"] {
        let signal = scanners_ready.join(category);
        while !signal.exists() {
            assert!(
                Instant::now() < readiness_deadline,
                "{category} scanner did not finish: {warm_scanners:#}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
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
    if rust_env.get("AFT_FAKE_CARGO_CHECK").is_some() {
        assert_eq!(fs::read_to_string(check_marker).unwrap(), "checked\n");
    }
    assert!(aft.shutdown().success());
    (response, elapsed)
}

#[test]
fn standalone_inspect_preserves_partial_results_when_rust_keeps_indexing() {
    // Without Cargo.toml, an analyzer still indexing cannot confirm complete diagnostics.
    let (response, elapsed) =
        warming_rust_inspect_response(false, WARMING_DIAGNOSTICS_BUDGET_MS, json!({}));
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
    assert_returned_without_waiting_for_diagnostics(elapsed);
}

#[test]
fn standalone_inspect_completed_cargo_check_is_fresh_while_analyzer_is_warming() {
    // A completed explicit Cargo check supplies diagnostics while the analyzer is still warming.
    let (response, elapsed) = warming_rust_inspect_response(
        true,
        WARMING_DIAGNOSTICS_BUDGET_MS,
        json!({"AFT_FAKE_CARGO_CHECK": "1"}),
    );
    assert_eq!(response["success"], true, "{response:#}");
    assert_eq!(response["complete"], true, "{response:#}");
    assert_eq!(response["summary"]["metrics"]["files"], 1, "{response:#}");
    assert_eq!(response["summary"]["todos"]["count"], 1, "{response:#}");

    assert_eq!(
        response["summary"]["diagnostics"]["errors"], 0,
        "{response:#}"
    );
    let text = response["text"].as_str().unwrap();
    assert!(text.starts_with("FRESH"), "{text}");
    assert!(text.contains("workspace analysis is warming"), "{text}");
    assert!(text.contains("from the last completed check"), "{text}");
    assert_returned_without_waiting_for_diagnostics(elapsed);
}

#[cfg(unix)]
#[test]
fn standalone_inspect_runs_cargo_check_while_the_analyzer_warms() {
    // Inspect's own cargo check must run while inspect waits for the analyzer
    // to settle, not after. With a 36 s budget (31 s of work time) that wait
    // lasts about 15 s for an analyzer that never settles; a check started
    // only after it would have about 7.5 s. The cargo below spends 9 s before
    // checking anything, so only a check that ran during the wait can finish.
    let temp = tempfile::tempdir().unwrap();
    let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let slow_cargo = temp.path().join("slow-cargo");
    crate::test_helpers::executable_cache::write_executable(
        &slow_cargo,
        format!(
            "#!/bin/sh\nif [ \"$1\" = check ]; then sleep 9; fi\nunset CARGO\nexec '{}' \"$@\"\n",
            Path::new(&real_cargo).display()
        )
        .as_bytes(),
    )
    .unwrap();

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
