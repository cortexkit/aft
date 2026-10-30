use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};

use aft::db::TrackedConnection as Connection;
use rusqlite::{params, OptionalExtension};
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
fn db_get_state_repairs_root_scoped_notification_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    fs::write(storage.path().join("last_announced_version"), "0.26.5").unwrap();
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");

    let response = get_state(&mut aft, "last_announced_version");

    assert_eq!(response["success"], true, "get_state failed: {response:?}");
    assert_eq!(response["value"], "0.26.5");
    assert!(!storage.path().join("last_announced_version").exists());
    assert_eq!(
        fs::read_to_string(storage.path().join("opencode/last_announced_version")).unwrap(),
        "0.26.5"
    );
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
        assert!(aft_a.shutdown().success());
        response
    });
    let barrier_b = barrier.clone();
    let thread_b = std::thread::spawn(move || {
        barrier_b.wait();
        let response = set_host_state(&mut aft_b, "beta", "two");
        assert!(aft_b.shutdown().success());
        response
    });

    let response_a = thread_a.join().unwrap();
    let response_b = thread_b.join().unwrap();
    assert_eq!(response_a["success"], true, "alpha: {response_a}");
    assert_eq!(response_b["success"], true, "beta: {response_b}");
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

// An exclusive rollback-journal lock prevents the deferred opener from switching to WAL.
// Use SQLite for the lock; touching database files directly can drop POSIX locks.
fn journal_lock(storage: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(storage.join("aft.db")).unwrap();
    conn.execute_batch("CREATE TABLE lock_fixture (id INTEGER); BEGIN EXCLUSIVE;")
        .unwrap();
    conn
}

#[test]
fn deferred_database_open_retries_journal_lock_until_release() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let lock = journal_lock(storage.path());
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(6));
        lock.execute_batch("ROLLBACK").unwrap();
    });
    let response = set_host_state(&mut aft, "after-lock", "yes");
    release.join().unwrap();
    assert_eq!(response["success"], true, "{response}");
    assert!(aft.shutdown().success());
}

#[test]
fn deferred_database_open_busy_exhaustion_retries_without_rebind() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let lock = journal_lock(storage.path());
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");
    let start = std::time::Instant::now();
    let response = set_host_state(&mut aft, "after-lock", "yes");
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(response["retryable"], true, "{response}");
    assert_eq!(response["code"], "database_unavailable", "{response}");
    assert!(start.elapsed() >= std::time::Duration::from_secs(9));
    assert!(start.elapsed() < std::time::Duration::from_secs(15));
    lock.execute_batch("ROLLBACK").unwrap();
    let response = set_host_state(&mut aft, "after-lock", "yes");
    assert_eq!(response["success"], true, "{response}");
    assert!(aft.shutdown().success());
}

/// Saved root identity records under `storage`, parsed.
#[cfg(unix)]
fn saved_root_identities(storage: &Path) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(storage.join("root-identities")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .map(|path| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
        .collect()
}

// Unix only: the pin row must use the root spelling configure resolves, and
// fs::canonicalize returns a verbatim path on Windows.
#[cfg(unix)]
#[test]
fn locked_database_legacy_path_pin_binds_without_saving_then_honours_the_pin() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(project.path()).unwrap();
    // A repository with a commit: without the pin, a bind chooses its Git
    // identity, so honouring the path pin is observable.
    let git = |args: &[&str]| {
        let mut command = std::process::Command::new("git");
        crate::test_helpers::apply_hermetic_git_env(command.current_dir(&root));
        let output = command.args(args).output().expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=Identity Tests",
        "-c",
        "user.email=identity@example.invalid",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    {
        let mut database = db(storage.path());
        let spelling = root.display().to_string();
        aft::db::standing_roots::ensure_standing_root(
            &mut database,
            &aft::db::standing_roots::StandingRootRecord {
                literal_path: spelling.clone(),
                resolved_target: spelling,
                resolved_git_toplevel: None,
                scoped_relative_path: None,
            },
            &[aft::config::IndexKind::Search],
        )
        .unwrap();
    }
    // Rollback-journal mode, so an exclusive transaction also blocks readers.
    let lock = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
    lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();

    let aft = configured_aft(&root, storage.path(), "opencode");
    assert!(aft.shutdown().success());
    assert_eq!(
        saved_root_identities(storage.path()),
        Vec::<Value>::new(),
        "an identity chosen while the pin is unreadable must not be saved"
    );
    assert!(
        !storage.path().join("cache-keys.json").exists(),
        "nor may it record a repository memo that would conflict with the pin"
    );

    lock.execute_batch("ROLLBACK").unwrap();
    drop(lock);
    let aft = configured_aft(&root, storage.path(), "opencode");
    assert!(aft.shutdown().success());
    let saved = saved_root_identities(storage.path());
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0]["identity"]["kind"], "path", "{saved:?}");
}

/// A git repository with one commit and one searchable file, optionally
/// pinned to its path identity by an older standing-root row, with aft.db held
/// by an exclusive transaction that also blocks readers.
#[cfg(unix)]
struct ProvisionalFixture {
    _project: tempfile::TempDir,
    storage: tempfile::TempDir,
    root: std::path::PathBuf,
    lock: rusqlite::Connection,
}

#[cfg(unix)]
fn provisional_fixture(path_pin: bool) -> ProvisionalFixture {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(project.path()).unwrap();
    fs::write(
        root.join("marker.rs"),
        "fn provisional_identity_marker() {}\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let mut command = std::process::Command::new("git");
        crate::test_helpers::apply_hermetic_git_env(command.current_dir(&root));
        let output = command.args(args).output().expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&["init", "--quiet"]);
    git(&["add", "marker.rs"]);
    git(&[
        "-c",
        "user.name=Identity Tests",
        "-c",
        "user.email=identity@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "initial",
    ]);
    {
        let mut database = db(storage.path());
        if path_pin {
            let spelling = root.display().to_string();
            aft::db::standing_roots::ensure_standing_root(
                &mut database,
                &aft::db::standing_roots::StandingRootRecord {
                    literal_path: spelling.clone(),
                    resolved_target: spelling,
                    resolved_git_toplevel: None,
                    scoped_relative_path: None,
                },
                &[aft::config::IndexKind::Search],
            )
            .unwrap();
        }
    }
    let lock = rusqlite::Connection::open(storage.path().join("aft.db")).unwrap();
    lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    ProvisionalFixture {
        _project: project,
        storage,
        root,
        lock,
    }
}

/// Names of the entries directly under `storage/<folder>`.
#[cfg(unix)]
fn entry_names(storage: &Path, folder: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(storage.join(folder)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
const PROVISIONAL_REASON: &str =
    "identity provisional: aft.db was busy at bind; indexes start once the database is free";

#[cfg(unix)]
fn degraded_reasons(aft: &mut AftProcess) -> Vec<String> {
    let status = aft.send(&json!({ "id": "status", "command": "status" }).to_string());
    assert_eq!(status["success"], true, "{status}");
    status["degraded_reasons"]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(|reason| reason.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Nothing keyed by any artifact identity exists yet: no index family, owner
/// manifest, retention binding, saved identity or legacy memo.
#[cfg(unix)]
fn assert_no_artifact_state(storage: &Path) {
    for folder in [
        "index",
        "semantic",
        "callgraph",
        "views",
        "artifact-owners",
        "retention/roots",
    ] {
        assert_eq!(
            entry_names(storage, folder),
            Vec::<String>::new(),
            "{folder} must stay empty while the identity is provisional"
        );
    }
    assert_eq!(saved_root_identities(storage), Vec::<Value>::new());
    assert!(!storage.join("cache-keys.json").exists());
}

#[cfg(unix)]
#[test]
fn unreadable_git_bind_serves_tools_without_saving_an_identity_and_retries() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(project.path()).unwrap();
    let source = root.join("marker.rs");
    fs::write(&source, "pub fn unreadable_git_marker() {}\n").unwrap();
    let real_git = which::which("git").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "marker.rs"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "-m",
            "initial",
        ],
    ] {
        let mut command = std::process::Command::new(&real_git);
        crate::test_helpers::apply_hermetic_git_env(&mut command);
        assert!(command
            .current_dir(&root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let blocked = tools.path().join("blocked");
    fs::write(&blocked, "").unwrap();
    // A switchable Git stand-in reproduces an ownership refusal without
    // requiring the test runner to change the checkout's owner.
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" != '--version' ] && [ -f '{}' ]; then\n  echo 'fatal: detected dubious ownership in repository' >&2\n  exit 128\nfi\nexec '{}' \"$@\"\n",
        blocked.display(), real_git.display()
    );
    super::helpers::link_cached_executable(&tools.path().join("git"), script.as_bytes()).unwrap();
    let path = std::env::join_paths(
        std::iter::once(tools.path().to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    // Do not inherit the test runner checkout's safe.directory entry.
    let mut aft = AftProcess::spawn_with_env(&[
        ("PATH", path.as_os_str()),
        ("GIT_CONFIG_PARAMETERS", "".as_ref()),
    ]);
    let bind = json!({
        "id": "cfg", "command": "configure", "harness": "opencode",
        "project_root": root, "storage_dir": storage.path(),
        "config": super::helpers::user_config(json!({
            "indexes": {"trigram": true, "semantic": true, "callgraph": true},
            "views": {"enabled": true}
        }))
    });
    let response = aft.send(&bind.to_string());
    assert_eq!(
        response["success"], true,
        "unreadable Git must not prevent binding: {response}"
    );
    let reasons = degraded_reasons(&mut aft).join("; ");
    assert!(reasons.contains("git_identity_unavailable"), "{reasons}");
    assert!(reasons.contains("dubious ownership"), "{reasons}");
    assert!(
        reasons.contains("git config --global --add safe.directory"),
        "{reasons}"
    );
    assert!(reasons.contains(&root.display().to_string()), "{reasons}");
    for request in [
        json!({"id": "read", "command": "read", "file": source}),
        json!({"id": "outline", "command": "outline", "file": source}),
        json!({"id": "zoom", "command": "zoom", "file": source, "symbols": "unreadable_git_marker"}),
        json!({"id": "edit", "command": "tool_call", "name": "edit", "arguments": {"path": source, "edits": [{"oldString": "pub fn", "newString": "// edited\npub fn"}]}}),
        json!({"id": "bash", "command": "bash", "params": {"command": "printf unreadable_git_marker"}}),
    ] {
        let response = aft.send(&request.to_string());
        assert_eq!(response["success"], true, "{request}: {response}");
    }
    assert!(fs::read_to_string(&source)
        .unwrap()
        .starts_with("// edited\n"));
    let grep = aft.send(
        &json!({"id": "grep", "command": "grep", "pattern": "unreadable_git_marker"}).to_string(),
    );
    assert_eq!(grep["success"], true, "{grep}");
    assert_eq!(grep["fallback"], "filesystem", "{grep}");
    assert_eq!(
        grep["index"]["trigram"]["reason"], "identity_provisional",
        "{grep}"
    );
    let search = aft.send(
        &json!({"id": "search", "command": "semantic_search", "query": "unreadable_git_marker", "top_k": 5}).to_string(),
    );
    assert_eq!(search["success"], true, "{search}");
    assert!(
        search.to_string().contains("git_identity_unavailable"),
        "{search}"
    );
    assert!(search.to_string().contains("dubious ownership"), "{search}");
    assert_no_artifact_state(storage.path());

    fs::remove_file(blocked).unwrap();
    let response = aft.send(&bind.to_string());
    assert_eq!(response["success"], true, "{response}");
    assert!(!degraded_reasons(&mut aft)
        .join("; ")
        .contains("git_identity_unavailable"));
    assert_eq!(
        wait_for_single_built_family(&mut aft, storage.path()),
        "git"
    );
    assert!(aft.shutdown().success());
}

/// After a finalizing bind, wait until exactly one artifact family is owned
/// and built for the root, and return its saved identity kind. A views-off
/// root builds its trigram index under `index/<family>`; with views on the
/// legacy index is never written and the checkout's lexical view builds the
/// family's trigram plane under `blobs/v3/<family>` instead. That view is
/// built by the first lexical query, not during configure maintenance of the
/// stdin/stdout process these tests spawn, so each wait round sends a grep.
#[cfg(unix)]
fn wait_for_single_built_family(aft: &mut AftProcess, storage: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let owners = entry_names(storage, "artifact-owners");
        let mut built = entry_names(storage, "index");
        built.extend(
            entry_names(storage, "blobs/v3")
                .into_iter()
                .filter(|family| {
                    storage
                        .join("blobs/v3")
                        .join(family)
                        .join("trigram.sqlite")
                        .is_file()
                }),
        );
        built.sort();
        built.dedup();
        if owners.len() == 1 && built.contains(&owners[0]) {
            assert_eq!(built, owners, "one folder must build one index family");
            let saved = saved_root_identities(storage);
            assert_eq!(saved.len(), 1, "{saved:?}");
            return saved[0]["identity"]["kind"].as_str().unwrap().to_string();
        }
        assert!(
            owners.len() <= 1,
            "one folder must own one artifact family: {owners:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "no index was built after the finalizing bind: owners={owners:?} index={built:?}"
        );
        let grep = aft
            .send(&json!({"id": "build-lexical", "command": "grep", "pattern": "fn"}).to_string());
        assert_eq!(grep["success"], true, "{grep}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(unix)]
fn provisional_bind_then_finalize(path_pin: bool) -> String {
    let fixture = provisional_fixture(path_pin);
    let storage = fixture.storage.path();
    let mut aft = configured_aft(&fixture.root, storage, "opencode");
    assert!(
        degraded_reasons(&mut aft).contains(&PROVISIONAL_REASON.to_string()),
        "status must name the provisional identity"
    );
    let grep = aft.send(
        &json!({
            "id": "grep",
            "command": "grep",
            "pattern": "provisional_identity_marker",
        })
        .to_string(),
    );
    assert_eq!(grep["success"], true, "{grep}");
    assert!(
        grep.to_string().contains("marker.rs"),
        "grep must answer through the unindexed fallback: {grep}"
    );
    assert_ne!(
        grep["index_status"], "Ready",
        "grep must not claim an index: {grep}"
    );
    assert_eq!(grep["fallback"], "filesystem", "{grep}");
    assert_eq!(
        grep["index"]["trigram"]["reason"], "identity_provisional",
        "grep must disclose why its index is off: {grep}"
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_no_artifact_state(storage);

    fixture.lock.execute_batch("ROLLBACK").unwrap();
    drop(fixture.lock);
    // The same bind again, from the same session: it must not take the
    // equivalent-configure shortcut, so it reads the pin and finalizes.
    configure(&mut aft, &fixture.root, storage, "opencode");
    assert!(
        !degraded_reasons(&mut aft).contains(&PROVISIONAL_REASON.to_string()),
        "a finalizing bind clears the provisional reason"
    );
    let kind = wait_for_single_built_family(&mut aft, storage);
    assert!(aft.shutdown().success());
    kind
}

#[cfg(unix)]
#[test]
fn provisional_identity_with_a_path_pin_finalizes_to_the_path_key_at_the_next_bind() {
    assert_eq!(provisional_bind_then_finalize(true), "path");
}

#[cfg(unix)]
#[test]
fn provisional_identity_without_a_pin_finalizes_to_the_git_key_at_the_next_bind() {
    assert_eq!(provisional_bind_then_finalize(false), "git");
}

/// The bind that found aft.db busy is the only bind: once the database is
/// free, the background retry settles the identity and builds the index.
#[cfg(unix)]
#[test]
fn provisional_identity_settles_without_a_second_bind_once_aft_db_frees() {
    let fixture = provisional_fixture(false);
    let storage = fixture.storage.path();
    let mut aft = configured_aft(&fixture.root, storage, "opencode");
    assert!(
        degraded_reasons(&mut aft).contains(&PROVISIONAL_REASON.to_string()),
        "status must name the provisional identity"
    );
    fixture.lock.execute_batch("ROLLBACK").unwrap();
    drop(fixture.lock);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let reasons = degraded_reasons(&mut aft);
        if !reasons
            .iter()
            .any(|reason| reason.starts_with("identity provisional"))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the identity never settled after aft.db became free: {reasons:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(wait_for_single_built_family(&mut aft, storage), "git");
    assert!(aft.shutdown().success());
}

#[cfg(unix)]
#[test]
fn provisional_identity_search_answers_through_the_unindexed_fallback() {
    let fixture = provisional_fixture(true);
    let mut aft = configured_aft(&fixture.root, fixture.storage.path(), "opencode");
    let search = aft.send(
        &json!({
            "id": "search",
            "command": "semantic_search",
            "query": "provisional_identity_marker",
            "top_k": 5,
        })
        .to_string(),
    );
    assert_eq!(search["success"], true, "{search}");
    assert!(
        search.to_string().contains("marker.rs"),
        "search must answer through the unindexed fallback: {search}"
    );
    assert!(
        search.to_string().contains("identity provisional"),
        "search must disclose why its indexes are off: {search}"
    );
    assert_no_artifact_state(fixture.storage.path());
    drop(fixture.lock);
    assert!(aft.shutdown().success());
}

#[test]
fn standalone_database_retry_does_not_block_read_only_requests() {
    use std::time::{Duration, Instant};

    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let path = project.path().join("readable.txt");
    fs::write(&path, "available during database contention\n").unwrap();
    let lock = journal_lock(storage.path());
    let mut aft = configured_aft(project.path(), storage.path(), "opencode");
    let exhausted = set_host_state(&mut aft, "initial", "no");
    assert_eq!(exhausted["retryable"], true, "{exhausted}");
    assert_eq!(exhausted["code"], "database_unavailable", "{exhausted}");
    let busy_message = exhausted["message"]
        .as_str()
        .unwrap_or_else(|| panic!("missing refusal message: {exhausted}"));
    assert!(
        busy_message
            .starts_with("Project persistence is busy after bounded initialization retries: "),
        "{exhausted}"
    );
    assert!(
        busy_message.contains("sqlite PRAGMA journal_mode=WAL: database is locked."),
        "{exhausted}"
    );
    assert!(
        busy_message.ends_with(
            "Retry the tool shortly; no rebind is needed. No tool operation was performed."
        ),
        "{exhausted}"
    );

    // Keep the lock held through both responses. Send the read behind the retry
    // so a long synchronous retry cannot masquerade as a responsive main loop.
    let deadline = Instant::now() + Duration::from_secs(2);
    aft.send_silent(&json!({"id": "retry-held", "command": "db_set_host_state", "params": {"key": "retry", "value": "no"}}).to_string());
    aft.send_silent(&json!({"id": "read-held", "command": "read", "file": path}).to_string());
    let mut saw_read = false;
    let mut saw_retry = false;
    while !saw_read || !saw_retry {
        let response = aft
            .try_read_next_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("read-only request must answer within two seconds while SQLite remains locked");
        match response["id"].as_str() {
            Some("read-held") => {
                assert_eq!(response["success"], true, "{response}");
                saw_read = true;
            }
            Some("retry-held") => {
                assert_eq!(response["success"], false, "{response}");
                assert_eq!(response["retryable"], true, "{response}");
                assert_eq!(response["code"], "database_unavailable", "{response}");
                assert_eq!(response["message"], busy_message, "{response}");
                saw_retry = true;
            }
            _ => {}
        }
    }
    lock.execute_batch("ROLLBACK").unwrap();
    let released = set_host_state(&mut aft, "released", "yes");
    assert_eq!(released["success"], true, "{released}");
    assert!(aft.shutdown().success());
}
