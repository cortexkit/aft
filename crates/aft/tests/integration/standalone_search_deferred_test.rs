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
            "search_index": false,
            "semantic_search": false,
            "callgraph_store": false
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
        (
            "AFT_TEST_CONFIGURE_STORAGE_SWEEP_DELAY_MS",
            std::ffi::OsStr::new("750"),
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
    assert!(
        latency < Duration::from_millis(500),
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
                "search_index": false,
                "semantic_search": false,
                "callgraph_store": false,
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
    let orphan = seed_stale_storage_entries(storage.path(), transient_root.path(), 1)
        .pop()
        .expect("seeded orphan path");

    let mut aft = AftProcess::spawn_with_env(&[
        ("AFT_STORAGE_DIR", storage.path().as_os_str()),
        ("TMPDIR", transient_root.path().as_os_str()),
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
            "orphan sweep did not complete within ten seconds"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let (status, stderr) = aft.stderr_output();
    assert!(status.success());
    assert!(
        stderr.contains("search index orphan sweep") && stderr.contains("scanned=1"),
        "detached orphan sweep did not log its bounded effect: {stderr}"
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
                "semantic_search": true,
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
            "search_index": true,
            "semantic_search": false,
            "callgraph_store": false,
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
