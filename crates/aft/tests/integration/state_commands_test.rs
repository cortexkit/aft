use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::helpers::AftProcess;

fn configure(aft: &mut AftProcess, project: &Path, storage: &Path, harness: &str) {
    let response = aft.send(
        &json!({
            "id": format!("cfg-{harness}"),
            "command": "configure",
            "harness": harness,
            "project_root": project,
            "storage_dir": storage,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn configured_aft(project: &Path, storage: &Path, harness: &str) -> AftProcess {
    let mut aft = AftProcess::spawn();
    configure(&mut aft, project, storage, harness);
    aft
}

fn set_state(aft: &mut AftProcess, key: &str, value: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("set-{key}"),
            "command": "db_set_state",
            "params": { "key": key, "value": value },
        })
        .to_string(),
    )
}

fn get_state(aft: &mut AftProcess, key: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("get-{key}"),
            "command": "db_get_state",
            "params": { "key": key },
        })
        .to_string(),
    )
}

fn set_host_state(aft: &mut AftProcess, key: &str, value: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("set-host-{key}"),
            "command": "db_set_host_state",
            "params": { "key": key, "value": value },
        })
        .to_string(),
    )
}

fn get_host_state(aft: &mut AftProcess, key: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("get-host-{key}"),
            "command": "db_get_host_state",
            "params": { "key": key },
        })
        .to_string(),
    )
}

fn db(storage: &Path) -> Connection {
    aft::db::open(&storage.join("aft.db")).expect("open aft.db")
}

fn harness_state_value(storage: &Path, harness: &str, key: &str) -> Option<String> {
    db(storage)
        .query_row(
            "SELECT value FROM harness_state WHERE harness = ?1 AND key = ?2",
            params![harness, key],
            |row| row.get(0),
        )
        .optional()
        .expect("query harness_state")
}

fn host_state_value(storage: &Path, key: &str) -> Option<String> {
    db(storage)
        .query_row(
            "SELECT value FROM host_state WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .expect("query host_state")
}

#[test]
fn db_set_state_writes_to_db() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = set_state(&mut aft, "foo", "bar");

    assert_eq!(response["success"], true, "set_state failed: {response:?}");
    assert_eq!(
        harness_state_value(storage.path(), "opencode", "foo"),
        Some("bar".into())
    );
    assert!(aft.shutdown().success());
}

#[test]
fn db_get_state_reads_from_db() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    assert_eq!(set_state(&mut aft, "foo", "bar")["success"], true);
    let response = get_state(&mut aft, "foo");

    assert_eq!(response["success"], true, "get_state failed: {response:?}");
    assert_eq!(response["value"], "bar");
    assert!(aft.shutdown().success());
}

#[test]
fn db_get_state_missing_returns_null() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = get_state(&mut aft, "missing_key");

    assert_eq!(response["success"], true, "get_state failed: {response:?}");
    assert!(response["value"].is_null());
    assert!(aft.shutdown().success());
}

#[test]
fn db_set_state_dual_writes_known_keys_to_legacy_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = set_state(&mut aft, "last_announced_version", "0.27.0");

    assert_eq!(response["success"], true, "set_state failed: {response:?}");
    let legacy = storage.path().join("opencode/last_announced_version");
    assert_eq!(fs::read_to_string(legacy).unwrap(), "0.27.0");
    assert!(aft.shutdown().success());
}

#[test]
fn db_set_state_unknown_key_no_legacy_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = set_state(&mut aft, "random_test_key", "test_value");

    assert_eq!(response["success"], true, "set_state failed: {response:?}");
    assert_eq!(
        harness_state_value(storage.path(), "opencode", "random_test_key"),
        Some("test_value".into())
    );
    assert!(!storage.path().join("opencode/random_test_key").exists());
    assert!(aft.shutdown().success());
}

#[test]
fn db_get_state_fallback_to_legacy_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    fs::create_dir_all(storage.path().join("opencode")).unwrap();
    fs::write(
        storage.path().join("opencode/last_announced_version"),
        "0.26.5",
    )
    .unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = get_state(&mut aft, "last_announced_version");

    assert_eq!(response["success"], true, "get_state failed: {response:?}");
    assert_eq!(response["value"], "0.26.5");
    assert!(aft.shutdown().success());
}

#[test]
fn db_set_state_atomic_legacy_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = set_state(&mut aft, "last_announced_version", "0.27.0");

    assert_eq!(response["success"], true, "set_state failed: {response:?}");
    let harness_dir = storage.path().join("opencode");
    let tmp_files = fs::read_dir(harness_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count();
    assert_eq!(tmp_files, 0);
    assert!(aft.shutdown().success());
}

#[test]
fn db_get_state_harness_isolation() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut opencode = configured_aft(project.path(), storage.path(), "opencode");
    assert_eq!(set_state(&mut opencode, "foo", "bar")["success"], true);
    assert!(opencode.shutdown().success());

    let mut pi = configured_aft(project.path(), storage.path(), "pi");
    let response = get_state(&mut pi, "foo");

    assert_eq!(response["success"], true, "get_state failed: {response:?}");
    assert!(response["value"].is_null());
    assert!(pi.shutdown().success());
}

#[test]
fn db_set_host_state_writes_to_db_and_legacy_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = set_host_state(&mut aft, "trusted_filter_projects", "[\"X\"]");

    assert_eq!(
        response["success"], true,
        "set_host_state failed: {response:?}"
    );
    assert_eq!(
        host_state_value(storage.path(), "trusted_filter_projects"),
        Some("[\"X\"]".into())
    );
    assert_eq!(
        fs::read_to_string(storage.path().join("trusted-filter-projects.json")).unwrap(),
        "[\"X\"]"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn db_set_host_state_concurrent_insert() {
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft_a = configured_aft(project_a.path(), storage.path(), "opencode");
    let mut aft_b = configured_aft(project_b.path(), storage.path(), "pi");
    let barrier = Arc::new(Barrier::new(2));

    let barrier_a = barrier.clone();
    let thread_a = std::thread::spawn(move || {
        barrier_a.wait();
        let response = set_host_state(&mut aft_a, "alpha", "one");
        let ok = response["success"] == true;
        assert!(aft_a.shutdown().success());
        ok
    });
    let barrier_b = barrier.clone();
    let thread_b = std::thread::spawn(move || {
        barrier_b.wait();
        let response = set_host_state(&mut aft_b, "beta", "two");
        let ok = response["success"] == true;
        assert!(aft_b.shutdown().success());
        ok
    });

    assert!(thread_a.join().unwrap());
    assert!(thread_b.join().unwrap());
    assert_eq!(
        host_state_value(storage.path(), "alpha"),
        Some("one".into())
    );
    assert_eq!(host_state_value(storage.path(), "beta"), Some("two".into()));
}

#[test]
fn db_get_host_state_no_harness_scoping() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut opencode = configured_aft(project.path(), storage.path(), "opencode");
    assert_eq!(
        set_host_state(&mut opencode, "alpha", "shared")["success"],
        true
    );
    assert!(opencode.shutdown().success());

    let mut pi = configured_aft(project.path(), storage.path(), "pi");
    let response = get_host_state(&mut pi, "alpha");

    assert_eq!(
        response["success"], true,
        "get_host_state failed: {response:?}"
    );
    assert_eq!(response["value"], "shared");
    assert!(pi.shutdown().success());
}

#[cfg(unix)]
#[test]
fn db_set_state_legacy_write_failure_does_not_fail_db_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let harness_dir = storage.path().join("opencode");
    fs::create_dir_all(&harness_dir).unwrap();
    let original_perms = fs::metadata(&harness_dir).unwrap().permissions();
    fs::set_permissions(&harness_dir, fs::Permissions::from_mode(0o500)).unwrap();

    let mut aft = configured_aft(project.path(), storage.path(), "opencode");
    let response = set_state(&mut aft, "last_announced_version", "0.27.0");

    fs::set_permissions(&harness_dir, original_perms).unwrap();
    assert_eq!(response["success"], true, "set_state failed: {response:?}");
    assert_eq!(
        harness_state_value(storage.path(), "opencode", "last_announced_version"),
        Some("0.27.0".into())
    );
    assert!(aft.shutdown().success());
}
