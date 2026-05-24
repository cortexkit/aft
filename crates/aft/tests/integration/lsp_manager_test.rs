use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aft::config::{Config, UserServerDef};
use aft::lsp::child_registry::LspChildRegistry;
use aft::lsp::client::{LspEvent, ServerState};
use aft::lsp::manager::{LspManager, ServerAttemptResult};
use aft::lsp::registry::ServerKind;
use lsp_types::FileChangeType;
use serde_json::{json, Value};
use tempfile::tempdir;

fn fake_server_path() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let mut path = std::env::current_exe().ok()?;
            path.pop();
            path.pop();
            path.push("fake-lsp-server");
            Some(path)
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

fn rust_fixture_files() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("workspace");
    let src_dir = root.join("src");
    let main_rs = src_dir.join("main.rs");
    let lib_rs = src_dir.join("lib.rs");

    fs::create_dir_all(&src_dir).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(&main_rs, "fn main() {}\n").unwrap();
    fs::write(&lib_rs, "pub fn answer() -> u32 { 42 }\n").unwrap();

    (temp_dir, main_rs, lib_rs)
}

fn collect_notification(manager: &mut LspManager, method: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for event in manager.drain_events() {
            if let LspEvent::Notification {
                method: event_method,
                params,
                ..
            } = event
            {
                if event_method == method {
                    return params.expect("notification params");
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {method}");
}

fn collect_event<F>(manager: &mut LspManager, predicate: F, timeout: Duration) -> Option<LspEvent>
where
    F: Fn(&LspEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for event in manager.drain_events() {
            if predicate(&event) {
                return Some(event);
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    None
}

fn collect_optional_notification(
    manager: &mut LspManager,
    method: &str,
    timeout: Duration,
) -> Option<Value> {
    collect_event(
        manager,
        |event| matches!(event, LspEvent::Notification { method: event_method, .. } if event_method == method),
        timeout,
    )
    .and_then(|event| match event {
        LspEvent::Notification { params, .. } => params,
        _ => None,
    })
}

fn executable_protocol_server_script() -> PathBuf {
    let temp_dir = tempdir().expect("tempdir for protocol server");
    let script = temp_dir.keep().join("protocol_lsp_server.py");
    fs::write(&script, PROTOCOL_SERVER).expect("write protocol server");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("chmod protocol server");
    }
    script
}

const PROTOCOL_SERVER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys


def read_message():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line == b"\r\n":
            break
        key, value = line.decode("ascii").split(":", 1)
        headers[key.lower()] = value.strip()
    length = int(headers["content-length"])
    return json.loads(sys.stdin.buffer.read(length).decode("utf-8"))


def write_message(value):
    payload = json.dumps(value, separators=(",", ":")).encode("utf-8")
    sys.stdout.buffer.write(b"Content-Length: " + str(len(payload)).encode("ascii") + b"\r\n\r\n" + payload)
    sys.stdout.buffer.flush()


def response(msg_id, result):
    write_message({"jsonrpc": "2.0", "id": msg_id, "result": result})


def request(msg_id, method, params):
    write_message({"jsonrpc": "2.0", "id": msg_id, "method": method, "params": params})


def notification(method, params):
    write_message({"jsonrpc": "2.0", "method": method, "params": params})


mode = os.environ.get("AFT_PROTOCOL_LSP_MODE", "watch")
next_request_id = 100

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method")
    if method == "initialize":
        capabilities = {"textDocumentSync": 1}
        if mode == "workspace-timeout":
            capabilities["diagnosticProvider"] = {
                "interFileDependencies": True,
                "workspaceDiagnostics": True,
                "identifier": "protocol-test",
            }
        response(message["id"], {"capabilities": capabilities})
    elif method == "initialized":
        if mode == "watch":
            request(next_request_id, "client/registerCapability", {
                "registrations": [{
                    "id": "watch-1",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": {"watchers": [{"globPattern": "**/*"}]},
                }]
            })
            next_request_id += 1
    elif method == "workspace/didChangeWatchedFiles":
        notification("custom/watchedFilesChanged", message.get("params"))
        request(next_request_id, "client/unregisterCapability", {
            "unregisterations": [{
                "id": "watch-1",
                "method": "workspace/didChangeWatchedFiles",
            }]
        })
        next_request_id += 1
    elif method == "workspace/diagnostic":
        pass
    elif method == "$/cancelRequest":
        notification("custom/cancelReceived", message.get("params"))
    elif method == "shutdown":
        response(message["id"], None)
    elif method == "exit":
        break
"#;

#[test]
fn test_manager_spawns_server_on_first_touch() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    let keys = manager.ensure_server_for_file_default(&main_rs);

    assert_eq!(keys.len(), 1);
    assert_eq!(manager.active_client_count(), 1);
    let client = manager
        .client_for_file_default(&main_rs)
        .expect("missing client");
    assert_eq!(client.kind(), ServerKind::Rust);
    assert_eq!(client.state(), ServerState::Ready);
}

#[test]
fn test_manager_reuses_existing_server() {
    let (_temp_dir, main_rs, lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    let first = manager.ensure_server_for_file_default(&main_rs);
    let second = manager.ensure_server_for_file_default(&lib_rs);

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0], second[0]);
    assert_eq!(manager.active_client_count(), 1);
}

#[test]
fn test_manager_shutdown_all() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    manager.ensure_server_for_file_default(&main_rs);
    assert_eq!(manager.active_client_count(), 1);

    manager.shutdown_all();

    assert_eq!(manager.active_client_count(), 0);
    assert!(!manager.has_active_servers());
}

#[test]
fn test_server_lifecycle_states() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());

    manager.ensure_server_for_file_default(&main_rs);

    let client = manager
        .client_for_file_default(&main_rs)
        .expect("missing client");
    assert_eq!(client.state(), ServerState::Ready);
}

#[test]
fn test_manager_handles_missing_binary() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server"),
    );

    let keys = manager.ensure_server_for_file_default(&main_rs);

    assert!(keys.is_empty());
    assert_eq!(manager.active_client_count(), 0);
    assert!(manager.client_for_file_default(&main_rs).is_none());
}

#[test]
fn test_custom_server_env_and_initialization_options_reach_spawned_server() {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("workspace");
    let main_typ = root.join("main.typ");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("typst.toml"), "[package]\nname = \"demo\"\n").unwrap();
    fs::write(&main_typ, "= Demo\n").unwrap();

    // NOTE: id must NOT match a built-in server name. Earlier versions of this
    // test used "tinymist" — that is a built-in id, so after #56 (user entries
    // merge with matching built-ins) the registered ServerKind would be
    // ServerKind::Tinymist, not ServerKind::Custom("tinymist"). Use a clearly
    // user-only id so this test verifies the Custom code path.
    let mut env = HashMap::new();
    env.insert("AFT_TEST_LSP_ENV".to_string(), "from-config".to_string());
    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "my-typst".to_string(),
            extensions: vec!["typ".to_string()],
            binary: "tinymist".to_string(),
            args: Vec::new(),
            root_markers: vec!["typst.toml".to_string()],
            env,
            initialization_options: Some(json!({
                "exportPdf": "never",
                "nested": { "enabled": true }
            })),
            disabled: false,
        }],
        ..Config::default()
    };

    let mut manager = LspManager::new();
    manager.override_binary(
        ServerKind::Custom(Arc::from("my-typst")),
        fake_server_path(),
    );

    let keys = manager.ensure_server_for_file(&main_typ, &config);
    assert_eq!(keys.len(), 1);
    assert_eq!(manager.active_client_count(), 1);

    let initialized = collect_notification(&mut manager, "custom/initialized");
    assert_eq!(initialized["env"]["AFT_TEST_LSP_ENV"], "from-config");
    assert_eq!(initialized["initializationOptions"]["exportPdf"], "never");
    assert_eq!(
        initialized["initializationOptions"]["nested"]["enabled"],
        true
    );
}

#[test]
fn watched_file_capability_defaults_false_when_initialize_has_no_field() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let config = Config::default();
    let mut manager = LspManager::new();
    manager.override_binary(ServerKind::Rust, fake_server_path());
    // Drive the fake server to OMIT workspace.didChangeWatchedFiles from its
    // initialize result so the client's capability tracker defaults to false.
    manager.set_extra_env("AFT_FAKE_LSP_NO_WATCHED_FILES", "1");

    let keys = manager.ensure_server_for_file(&main_rs, &config);
    assert_eq!(keys.len(), 1);

    let client = manager.client_for_file(&main_rs, &config).expect("client");
    assert!(
        !client.supports_watched_files(),
        "missing explicit didChangeWatchedFiles capability should default to false"
    );
}

#[test]
fn watched_file_notifications_require_dynamic_registration_and_stop_after_unregister() {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("workspace");
    let source = root.join("main.watchtest");
    let changed = root.join("config.json");
    let created = root.join("created.json");
    let deleted = root.join("deleted.json");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("marker.txt"), "marker\n").unwrap();
    fs::write(&source, "content\n").unwrap();
    fs::write(&changed, "{}\n").unwrap();

    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "protocol-watch".to_string(),
            extensions: vec!["watchtest".to_string()],
            binary: "protocol-watch".to_string(),
            args: Vec::new(),
            root_markers: vec!["marker.txt".to_string()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let server_kind = ServerKind::Custom(Arc::from("protocol-watch"));
    let mut manager = LspManager::new();
    manager.override_binary(server_kind, executable_protocol_server_script());
    manager.set_extra_env("AFT_PROTOCOL_LSP_MODE", "watch");

    let keys = manager.ensure_server_for_file(&source, &config);
    assert_eq!(keys.len(), 1);

    let registered = collect_event(
        &mut manager,
        |event| {
            matches!(
                event,
                LspEvent::ServerRequest { method, .. }
                    if method == "client/registerCapability"
            )
        },
        Duration::from_secs(2),
    );
    assert!(
        registered.is_some(),
        "server did not register watched files"
    );

    manager
        .notify_files_watched_changed(
            &[
                (changed.clone(), FileChangeType::CHANGED),
                (created, FileChangeType::CREATED),
                (deleted, FileChangeType::DELETED),
            ],
            &config,
        )
        .expect("send watched-file change");
    let watched = collect_optional_notification(
        &mut manager,
        "custom/watchedFilesChanged",
        Duration::from_secs(2),
    )
    .expect("registered server should receive watched-file notification");
    let event_types: Vec<i64> = watched["changes"]
        .as_array()
        .expect("changes array")
        .iter()
        .map(|change| change["type"].as_i64().expect("type number"))
        .collect();
    assert_eq!(event_types, vec![2, 1, 3]);

    thread::sleep(Duration::from_millis(100));
    let _ = manager.drain_events();

    manager
        .notify_files_watched_changed(&[(changed, FileChangeType::CHANGED)], &config)
        .expect("skip after unregister");
    assert!(
        collect_optional_notification(
            &mut manager,
            "custom/watchedFilesChanged",
            Duration::from_millis(250),
        )
        .is_none(),
        "unregistered server must not receive watched-file notification"
    );
}

#[test]
fn workspace_pull_timeout_sends_cancel_request() {
    let temp_dir = tempdir().unwrap();
    let root = temp_dir.path().join("workspace");
    let source = root.join("main.wspull");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("marker.txt"), "marker\n").unwrap();
    fs::write(&source, "content\n").unwrap();

    let config = Config {
        lsp_servers: vec![UserServerDef {
            id: "protocol-workspace".to_string(),
            extensions: vec!["wspull".to_string()],
            binary: "protocol-workspace".to_string(),
            args: Vec::new(),
            root_markers: vec!["marker.txt".to_string()],
            env: HashMap::new(),
            initialization_options: None,
            disabled: false,
        }],
        ..Config::default()
    };
    let server_kind = ServerKind::Custom(Arc::from("protocol-workspace"));
    let mut manager = LspManager::new();
    manager.override_binary(server_kind, executable_protocol_server_script());
    manager.set_extra_env("AFT_PROTOCOL_LSP_MODE", "workspace-timeout");

    let keys = manager.ensure_server_for_file(&source, &config);
    assert_eq!(keys.len(), 1);

    let started = Instant::now();
    let result = manager
        .pull_workspace_diagnostics(&keys[0], Some(Duration::from_millis(200)))
        .expect("workspace pull result");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.cancelled);
    assert!(!result.complete);

    let cancel = collect_optional_notification(
        &mut manager,
        "custom/cancelReceived",
        Duration::from_secs(2),
    )
    .expect("server should receive $/cancelRequest");
    assert!(
        cancel.get("id").is_some(),
        "cancel params should include request id"
    );
}

// ---------------------------------------------------------------------------
// Failed-spawn dedup tests
//
// Regression: before v0.19.1, every file open / didChange / lsp_diagnostics
// call retried `spawn_server` for a (kind, root) pair that had already failed
// once. typescript-language-server failing on "Could not find a valid
// TypeScript installation" produced a fresh ERROR log per request.
//
// The fix caches the classified spawn-failure result per `(kind, root)` and
// skips re-spawn attempts. These tests pin that contract.
// ---------------------------------------------------------------------------

#[test]
fn failed_spawn_is_cached_and_not_retried_on_second_call() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    // Override with a binary that doesn't exist → spawn classifies as
    // BinaryNotInstalled.
    manager.override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server-XYZZY"),
    );

    // First call — must produce a BinaryNotInstalled attempt.
    let first = manager.ensure_server_for_file_detailed(&main_rs, &Config::default());
    assert_eq!(first.attempts.len(), 1);
    let first_result = &first.attempts[0].result;
    assert!(
        matches!(first_result, ServerAttemptResult::BinaryNotInstalled { .. })
            || matches!(first_result, ServerAttemptResult::SpawnFailed { .. }),
        "first call should classify as BinaryNotInstalled or SpawnFailed, got {first_result:?}"
    );
    assert_eq!(manager.active_client_count(), 0);

    // Second call — must return the SAME cached classification, no new spawn.
    let second = manager.ensure_server_for_file_detailed(&main_rs, &Config::default());
    assert_eq!(second.attempts.len(), 1);
    let second_result = &second.attempts[0].result;
    // The cached result is cloned, so the variant must match the first call's.
    assert_eq!(
        std::mem::discriminant(first_result),
        std::mem::discriminant(second_result),
        "cached failure must replay with the same variant"
    );
    assert_eq!(manager.active_client_count(), 0);
}

#[test]
fn failed_spawn_dedup_persists_across_many_calls() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let mut manager = LspManager::new();
    manager.override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server-XYZZY"),
    );

    // Simulate the production case: many file events in a row. Without dedup,
    // each one would log a fresh ERROR and try to spawn the missing binary.
    for _ in 0..10 {
        let outcome = manager.ensure_server_for_file_detailed(&main_rs, &Config::default());
        assert_eq!(outcome.attempts.len(), 1);
        assert_eq!(manager.active_client_count(), 0);
    }
}

#[test]
fn failed_spawn_for_one_root_does_not_block_a_different_root() {
    // The cache key is (ServerKind, workspace_root). A failed spawn for
    // workspace A must NOT prevent spawn attempts for an unrelated workspace
    // B — they're independent server instances.
    let (_temp_dir_a, main_rs_a, _lib_rs_a) = rust_fixture_files();
    let (_temp_dir_b, main_rs_b, _lib_rs_b) = rust_fixture_files();
    let mut manager = LspManager::new();

    // First override: missing binary → workspace A fails.
    manager.override_binary(
        ServerKind::Rust,
        PathBuf::from("/definitely/missing/fake-lsp-server-XYZZY"),
    );
    let outcome_a = manager.ensure_server_for_file_detailed(&main_rs_a, &Config::default());
    assert_eq!(outcome_a.successful.len(), 0);
    assert_eq!(manager.active_client_count(), 0);

    // Now point to a working binary. Workspace A is still cached as failed
    // (we don't auto-recover at runtime — the user has to fix env + restart),
    // but workspace B should spawn cleanly.
    manager.override_binary(ServerKind::Rust, fake_server_path());

    // Workspace A still returns the cached failure (no retry on a different
    // binary path — the cache deliberately survives override changes mid-session
    // because runtime overrides are a test-only feature).
    let outcome_a_again = manager.ensure_server_for_file_detailed(&main_rs_a, &Config::default());
    assert_eq!(outcome_a_again.successful.len(), 0);

    // Workspace B is a fresh (kind, root) pair → not in the failed cache → spawns.
    let outcome_b = manager.ensure_server_for_file_detailed(&main_rs_b, &Config::default());
    assert_eq!(outcome_b.successful.len(), 1);
    assert_eq!(manager.active_client_count(), 1);
}

// ---------------------------------------------------------------------------
// Child-PID registry tests
//
// Regression: before v0.19.3, LSP child processes were orphaned when aft
// received SIGTERM/SIGINT (e.g. during e2e test cleanup or plugin restart),
// because the signal handler called `process::exit` directly without killing
// child processes. The shared `LspChildRegistry` exposes spawned PIDs to the
// signal handler so it can SIGKILL them before exiting.
// ---------------------------------------------------------------------------

#[test]
fn spawned_lsp_child_is_tracked_in_registry() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let registry = LspChildRegistry::new();
    let mut manager = LspManager::new();
    manager.set_child_registry(registry.clone());
    manager.override_binary(ServerKind::Rust, fake_server_path());

    assert!(
        registry.pids().is_empty(),
        "registry should start empty before any spawn"
    );

    let keys = manager.ensure_server_for_file_default(&main_rs);
    assert_eq!(keys.len(), 1);
    assert_eq!(manager.active_client_count(), 1);

    let tracked = registry.pids();
    assert_eq!(
        tracked.len(),
        1,
        "registry should contain exactly one PID after one spawn, got {tracked:?}"
    );
}

#[test]
fn shutdown_all_untracks_pids_from_registry() {
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    let registry = LspChildRegistry::new();
    let mut manager = LspManager::new();
    manager.set_child_registry(registry.clone());
    manager.override_binary(ServerKind::Rust, fake_server_path());

    manager.ensure_server_for_file_default(&main_rs);
    assert_eq!(registry.pids().len(), 1);

    manager.shutdown_all();
    assert_eq!(manager.active_client_count(), 0);
    assert!(
        registry.pids().is_empty(),
        "graceful shutdown_all should untrack all PIDs"
    );
}

#[test]
fn dropping_manager_untracks_pids_from_registry() {
    // The Drop impl on LspClient must untrack its PID. This guards against
    // a subtle leak where the registry would grow unbounded across LspManager
    // recreations even though the actual child processes died correctly.
    let registry = LspChildRegistry::new();
    let (_temp_dir, main_rs, _lib_rs) = rust_fixture_files();
    {
        let mut manager = LspManager::new();
        manager.set_child_registry(registry.clone());
        manager.override_binary(ServerKind::Rust, fake_server_path());
        manager.ensure_server_for_file_default(&main_rs);
        assert_eq!(registry.pids().len(), 1);
    } // manager and its LspClients drop here

    assert!(
        registry.pids().is_empty(),
        "Drop on LspClient must untrack the PID even without graceful shutdown"
    );
}
