#![cfg(unix)]

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aft::bash_background::persistence::{session_tasks_dir, task_paths, write_task, PersistedTask};
use aft::bash_background::{BgTaskRegistry, BgTaskStatus};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use super::helpers::AftProcess;

const SESSION: &str = "persist-session";

fn spawn_storage_dir(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(name)).unwrap();
    dir
}

fn configure_background(aft: &mut AftProcess, project: &Path, storage: &Path, session: &str) {
    let response = aft.send(
        &json!({
            "id": format!("cfg-{session}"),
            "session_id": session,
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "storage_dir": storage,
            "experimental_bash_background": true,
            "max_background_bash_tasks": 32,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn configure_background_without_storage(aft: &mut AftProcess, project: &Path, session: &str) {
    let response = aft.send(
        &json!({
            "id": format!("cfg-no-storage-{session}"),
            "session_id": session,
            "command": "configure",
            "harness": "opencode",
            "project_root": project,
            "experimental_bash_background": true,
            "max_background_bash_tasks": 32,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
}

fn spawn_bg(aft: &mut AftProcess, session: &str, command: &str, timeout: Option<u64>) -> String {
    let mut params = json!({ "command": command, "background": true });
    if let Some(timeout) = timeout {
        params["timeout"] = json!(timeout);
    }
    let response = aft.send(
        &json!({
            "id": "spawn-persist-bg",
            "session_id": session,
            "command": "bash",
            "params": params,
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "spawn failed: {response:?}");
    response["task_id"].as_str().unwrap().to_string()
}

fn status(aft: &mut AftProcess, session: &str, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": format!("status-{task_id}"),
            "session_id": session,
            "command": "bash_status",
            "params": { "task_id": task_id }
        })
        .to_string(),
    )
}

fn drain(aft: &mut AftProcess, session: &str) -> Value {
    aft.send(
        &json!({
            "id": "drain-persist-bg",
            "session_id": session,
            "command": "bash_drain_completions"
        })
        .to_string(),
    )
}

fn ack(aft: &mut AftProcess, session: &str, task_id: &str) -> Value {
    aft.send(
        &json!({
            "id": "ack-persist-bg",
            "session_id": session,
            "command": "bash_ack_completions",
            "params": { "task_ids": [task_id] }
        })
        .to_string(),
    )
}

fn wait_for_status(aft: &mut AftProcess, session: &str, task_id: &str, expected: &str) -> Value {
    let started = Instant::now();
    loop {
        let response = status(aft, session, task_id);
        assert_eq!(response["success"], true, "status failed: {response:?}");
        if response["status"] == expected {
            return response;
        }
        // 30s budget instead of 8s so shared CI hardware (GitHub macOS runners
        // in particular) doesn't flake when 200 iterations of `sleep 0.01` plus
        // I/O exceed the previous tighter window. Tasks finish in ~2-3s
        // locally; the budget is just a backstop against a hung registry.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for {expected}: {response:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn task_file(storage: &Path, session: &str, task_id: &str, suffix: &str) -> PathBuf {
    session_tasks_dir(storage, session).join(format!("{task_id}.{suffix}"))
}

fn read_json(storage: &Path, session: &str, task_id: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(task_file(storage, session, task_id, "json")).unwrap())
        .unwrap()
}

fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

fn wait_for_path(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_mtime(path: &Path, age: Duration) {
    let target = SystemTime::now().checked_sub(age).unwrap_or(UNIX_EPOCH);
    let secs = target
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let times = [
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        },
    ];
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "failed to set mtime for {}", path.display());
}

fn chmod(path: &Path, mode: u32) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).unwrap();
}

fn fake_task(
    storage: &Path,
    project: &Path,
    session: &str,
    task_id: &str,
    status: BgTaskStatus,
    completion_delivered: bool,
) -> aft::bash_background::persistence::TaskPaths {
    let paths = task_paths(storage, session, task_id);
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        session.to_string(),
        "true".to_string(),
        project.to_path_buf(),
        Some(project.to_path_buf()),
        None,
        true,
        true,
    );
    if status.is_terminal() {
        metadata.mark_terminal(status, Some(0), None);
    } else {
        metadata.status = status;
    }
    metadata.completion_delivered = completion_delivered;
    write_task(&paths.json, &metadata).unwrap();
    fs::write(&paths.stdout, "stdout").unwrap();
    fs::write(&paths.stderr, "stderr").unwrap();
    fs::write(&paths.exit, "0").unwrap();
    paths
}

fn write_legacy_task_json(storage: &Path, session: &str, task_id: &str, project: &Path) -> PathBuf {
    let path = task_file(storage, session, task_id, "json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "task_id": task_id,
            "session_id": session,
            "command": "echo legacy",
            "workdir": project,
            "status": "completed",
            "started_at": 1,
            "finished_at": 2,
            "duration_ms": 1,
            "timeout_ms": null,
            "exit_code": 0,
            "child_pid": null,
            "pgid": null,
            "completion_delivered": true,
            "notify_on_completion": true,
            "compressed": false,
            "status_reason": null
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(task_file(storage, session, task_id, "stdout"), "legacy\n").unwrap();
    fs::write(task_file(storage, session, task_id, "stderr"), "").unwrap();
    path
}

#[test]
fn bash_status_same_session_cold_bridge_replays_from_disk() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let first = registry();
    let task_id = first
        .spawn(
            "true",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    wait_for_path(&task_file(storage.path(), SESSION, &task_id, "exit"));

    let fresh = registry();
    let snapshot = fresh
        .status(
            &task_id,
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .expect("same-session status should replay from disk");

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert_eq!(snapshot.exit_code, Some(0));
}

#[test]
fn bash_status_cross_session_same_project_finds_task_by_id() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let first = registry();
    let task_id = first
        .spawn(
            "true",
            "session-a".to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    wait_for_path(&task_file(storage.path(), "session-a", &task_id, "exit"));

    let fresh = registry();
    let snapshot = fresh
        .status(
            &task_id,
            "session-b",
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .expect("cross-session status should find same-project task");

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
}

#[test]
fn bash_status_cross_session_different_project_returns_not_found() {
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let first = registry();
    let task_id = first
        .spawn(
            "true",
            "session-a".to_string(),
            project_a.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project_a.path().to_path_buf()),
        )
        .unwrap();
    wait_for_path(&task_file(storage.path(), "session-a", &task_id, "exit"));

    let fresh = registry();
    assert!(fresh
        .status(
            &task_id,
            "session-b",
            Some(project_b.path()),
            Some(storage.path()),
            1024,
        )
        .is_none());
}

#[test]
fn bash_status_legacy_persisted_task_is_quarantined_on_replay() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-legacy1";
    let legacy_path = write_legacy_task_json(storage.path(), "session-a", task_id, project.path());

    let same = registry();
    same.replay_session(storage.path(), "session-a").unwrap();

    assert!(!legacy_path.exists());
    let quarantine_session = storage
        .path()
        .join("bash-tasks-quarantine")
        .join(aft::backup::hash_session("session-a"));
    let quarantined = fs::read_dir(quarantine_session)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(quarantined
        .iter()
        .any(|name| name.starts_with("bash-legacy1.invalid.") && name.ends_with(".json")));
}

#[test]
fn bash_kill_cross_session_still_returns_not_found() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 5",
            "session-a".to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();

    let error = registry.kill(&task_id, "session-b").unwrap_err();
    assert!(error.contains("not found"));
    let _ = registry.kill(&task_id, "session-a");
}

#[test]
fn bash_kill_command_cross_session_same_project_finds_task_by_id() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), "session-a");
    let task_id = spawn_bg(&mut aft, "session-a", "sleep 5", None);

    let killed = aft.send(
        &json!({
            "id": "kill-cross-session",
            "session_id": "session-b",
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );

    assert_eq!(killed["success"], true, "kill failed: {killed:?}");
    assert_eq!(killed["status"], "killed");
    assert!(aft.shutdown().success());
}

#[test]
fn bash_promote_cross_session_still_returns_not_found() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 5",
            "session-a".to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            false,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();

    let error = registry.promote(&task_id, "session-b").unwrap_err();
    assert!(error.contains("not found"));
    let _ = registry.kill(&task_id, "session-a");
}

#[test]
fn gc_persisted_deletes_delivered_terminals_older_than_grace() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    for idx in 0..5 {
        let paths = fake_task(
            storage.path(),
            project.path(),
            SESSION,
            &format!("bash-gc{idx}"),
            BgTaskStatus::Completed,
            true,
        );
        set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));
    }

    let deleted = registry().maybe_gc_persisted(storage.path()).unwrap();

    assert_eq!(deleted, 5);
    assert!(!session_tasks_dir(storage.path(), SESSION).exists());
}

#[test]
fn gc_persisted_keeps_undelivered_terminals() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-keep-undelivered",
        BgTaskStatus::Completed,
        false,
    );
    set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);
    assert!(paths.json.exists());
}

#[test]
fn gc_persisted_keeps_recent_files() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-keep-recent",
        BgTaskStatus::Completed,
        true,
    );
    set_mtime(&paths.json, Duration::from_secs(60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);
    assert!(paths.json.exists());
}

#[test]
fn gc_persisted_quarantines_corrupt_json() {
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "bash-corrupt");
    fs::create_dir_all(&paths.dir).unwrap();
    fs::write(&paths.json, "not-json").unwrap();
    fs::write(&paths.stdout, "stdout").unwrap();
    fs::write(&paths.stderr, "stderr").unwrap();
    fs::write(&paths.exit, "0").unwrap();
    set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);

    assert!(!paths.json.exists());
    assert!(!paths.stdout.exists());
    assert!(!paths.stderr.exists());
    assert!(!paths.exit.exists());
    let quarantine_session = storage
        .path()
        .join("bash-tasks-quarantine")
        .join(aft::backup::hash_session(SESSION));
    let quarantined = fs::read_dir(quarantine_session)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(quarantined.len(), 4);
    assert!(quarantined
        .iter()
        .any(|name| name.starts_with("bash-corrupt.json.corrupt-")));
}

#[test]
fn maybe_gc_persisted_cleans_quarantine_older_than_30_days() {
    let storage = tempfile::tempdir().unwrap();
    let quarantine_session = storage
        .path()
        .join("bash-tasks-quarantine")
        .join(aft::backup::hash_session(SESSION));
    fs::create_dir_all(&quarantine_session).unwrap();
    let old = quarantine_session.join("bash-old.json.corrupt-1");
    let recent = quarantine_session.join("bash-recent.json.corrupt-2");
    fs::write(&old, "old").unwrap();
    fs::write(&recent, "recent").unwrap();
    set_mtime(&old, Duration::from_secs(31 * 24 * 60 * 60));
    set_mtime(&recent, Duration::from_secs(24 * 60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);

    assert!(!old.exists());
    assert!(recent.exists());
}

#[test]
fn maybe_gc_persisted_continues_after_per_task_deletion_failure() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let failing = fake_task(
        storage.path(),
        project.path(),
        "session-fail",
        "bash-delete-fails",
        BgTaskStatus::Completed,
        true,
    );
    let succeeding = fake_task(
        storage.path(),
        project.path(),
        "session-ok",
        "bash-delete-succeeds",
        BgTaskStatus::Completed,
        true,
    );
    set_mtime(&failing.json, Duration::from_secs(25 * 60 * 60));
    set_mtime(&succeeding.json, Duration::from_secs(25 * 60 * 60));
    chmod(&failing.dir, 0o555);

    let deleted = registry().maybe_gc_persisted(storage.path()).unwrap();

    chmod(&failing.dir, 0o755);
    assert_eq!(deleted, 1);
    assert!(failing.json.exists());
    assert!(!succeeding.json.exists());
}

#[test]
fn quarantine_corrupt_json_moves_siblings_too() {
    let storage = tempfile::tempdir().unwrap();
    let paths = task_paths(storage.path(), SESSION, "bash-corrupt-siblings");
    fs::create_dir_all(&paths.dir).unwrap();
    fs::write(&paths.json, "not-json").unwrap();
    for extension in ["stdout", "stderr", "exit", "ps1", "bat", "sh"] {
        fs::write(
            paths.dir.join(format!("bash-corrupt-siblings.{extension}")),
            extension,
        )
        .unwrap();
    }
    set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);

    assert!(!paths.dir.exists());
    let quarantine_session = storage
        .path()
        .join("bash-tasks-quarantine")
        .join(aft::backup::hash_session(SESSION));
    let quarantined = fs::read_dir(quarantine_session)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for extension in ["json", "stdout", "stderr", "exit", "ps1", "bat", "sh"] {
        assert!(
            quarantined.iter().any(
                |name| name.starts_with(&format!("bash-corrupt-siblings.{extension}.corrupt-"))
            ),
            "missing quarantined {extension} sibling in {quarantined:?}"
        );
    }
}

#[test]
fn bash_status_cross_session_canonicalizes_paths() {
    let canonical_project = tempfile::tempdir().unwrap();
    let alias_parent = tempfile::tempdir().unwrap();
    let alias_project = alias_parent.path().join("project-link");
    std::os::unix::fs::symlink(canonical_project.path(), &alias_project).unwrap();
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-canonical";
    fake_task(
        storage.path(),
        &alias_project,
        "session-a",
        task_id,
        BgTaskStatus::Completed,
        true,
    );

    let snapshot = registry()
        .status(
            task_id,
            "session-b",
            Some(canonical_project.path()),
            Some(storage.path()),
            1024,
        )
        .expect("cross-session status should match canonical project paths");

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
}

#[test]
fn cleanup_finished_deletes_disk_bundle_of_delivered_terminal() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 5",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    let paths = task_paths(storage.path(), SESSION, &task_id);
    registry.kill(&task_id, SESSION).unwrap();
    assert_eq!(
        registry.drain_completions_for_session(Some(SESSION)).len(),
        1
    );
    assert_eq!(
        registry
            .ack_completions_for_session(Some(SESSION), std::slice::from_ref(&task_id))
            .len(),
        1
    );

    registry.cleanup_finished(Duration::ZERO);

    assert!(registry
        .status(&task_id, SESSION, None, None, 1024)
        .is_none());
    assert!(!paths.json.exists());
    assert!(!paths.stdout.exists());
    assert!(!paths.stderr.exists());
    assert!(!paths.exit.exists());
}

#[test]
fn cleanup_finished_does_not_block_other_registry_operations_during_delete() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let removable = registry
        .spawn(
            "sleep 5",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    let live = registry
        .spawn(
            "sleep 5",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    registry.kill(&removable, SESSION).unwrap();
    assert_eq!(
        registry.drain_completions_for_session(Some(SESSION)).len(),
        1
    );
    assert_eq!(
        registry
            .ack_completions_for_session(Some(SESSION), std::slice::from_ref(&removable))
            .len(),
        1
    );
    fs::write(
        task_file(storage.path(), SESSION, &removable, "sh"),
        vec![b'x'; 8 * 1024 * 1024],
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicU64::new(0));
    let max_call_ms = Arc::new(AtomicU64::new(0));
    let status_registry = registry.clone();
    let status_storage = storage.path().to_path_buf();
    let status_live = live.clone();
    let status_stop = Arc::clone(&stop);
    let status_calls = Arc::clone(&calls);
    let status_max_call_ms = Arc::clone(&max_call_ms);
    let status_thread = std::thread::spawn(move || {
        while !status_stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            let snapshot = status_registry.status(
                &status_live,
                SESSION,
                None,
                Some(status_storage.as_path()),
                1024,
            );
            let elapsed_ms = started.elapsed().as_millis() as u64;
            status_max_call_ms.fetch_max(elapsed_ms, Ordering::SeqCst);
            status_calls.fetch_add(1, Ordering::SeqCst);
            assert!(snapshot.is_some(), "live task status disappeared");
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    while calls.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    registry.cleanup_finished(Duration::ZERO);
    stop.store(true, Ordering::SeqCst);
    status_thread.join().unwrap();

    assert!(
        max_call_ms.load(Ordering::SeqCst) < 100,
        "status calls were blocked for {}ms",
        max_call_ms.load(Ordering::SeqCst)
    );
    let _ = registry.kill(&live, SESSION);
}

#[test]
fn cleanup_finished_retains_undelivered_terminals() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 5",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    registry.kill(&task_id, SESSION).unwrap();

    registry.cleanup_finished(Duration::ZERO);

    assert!(registry
        .status(&task_id, SESSION, None, None, 1024)
        .is_some());
}

#[test]
fn replay_session_recovers_killing_state() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-killing",
        BgTaskStatus::Killing,
        false,
    );
    fs::write(&paths.exit, "0").unwrap();

    registry().replay_session(storage.path(), SESSION).unwrap();
    let replayed = read_json(storage.path(), SESSION, "bash-killing");

    assert_eq!(replayed["status"], "completed");
    assert_eq!(replayed["exit_code"], 0);
    assert_eq!(
        replayed["status_reason"],
        "recovered from inconsistent killing state on replay"
    );
}

#[test]
fn replay_runs_maybe_gc_persisted_once() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();

    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-after-first-gc",
        BgTaskStatus::Completed,
        true,
    );
    set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));
    registry.replay_session(storage.path(), SESSION).unwrap();

    assert!(
        paths.json.exists(),
        "second replay must not run persisted GC again"
    );
}

#[test]
fn spawn_detached_survives_parent_restart() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");

    let task_id = {
        let mut aft = AftProcess::spawn();
        configure_background(&mut aft, project.path(), storage.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, "sleep 1", None);
        assert!(aft.shutdown().success());
        task_id
    };

    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);
    let running = status(&mut aft, SESSION, &task_id);
    assert_eq!(
        running["success"], true,
        "task was not rehydrated: {running:?}"
    );
    assert_eq!(running["status"], "running");

    let completed = wait_for_status(&mut aft, SESSION, &task_id, "completed");
    assert_eq!(completed["exit_code"], 0);
    assert!(aft.shutdown().success());
}

#[test]
fn configure_replays_background_tasks_from_default_storage_dir() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let storage = cache.path().join("aft");

    let task_id = {
        let mut aft = AftProcess::spawn_with_env(&[("AFT_CACHE_DIR", cache.path().as_os_str())]);
        configure_background_without_storage(&mut aft, project.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, "printf default-replay", None);
        wait_for_path(&task_file(&storage, SESSION, &task_id, "exit"));
        assert!(aft.shutdown().success());
        task_id
    };

    let mut aft = AftProcess::spawn_with_env(&[("AFT_CACHE_DIR", cache.path().as_os_str())]);
    configure_background_without_storage(&mut aft, project.path(), SESSION);
    let drained = drain(&mut aft, SESSION);
    assert_eq!(drained["success"], true, "drain failed: {drained:?}");
    let completion = drained["bg_completions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|completion| completion["task_id"] == task_id)
        .unwrap_or_else(|| panic!("missing replayed completion: {drained:?}"));

    assert_eq!(completion["status"], "completed");
    assert_eq!(completion["exit_code"], 0);
    assert!(completion["output_preview"]
        .as_str()
        .unwrap()
        .contains("default-replay"));
    let acked = ack(&mut aft, SESSION, &task_id);
    assert_eq!(acked["success"], true, "ack failed: {acked:?}");
    assert!(aft.shutdown().success());
}

#[test]
fn exit_file_atomicity_many_short_tasks() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);

    let task_ids = (0..12)
        .map(|_| spawn_bg(&mut aft, SESSION, "true", None))
        .collect::<Vec<_>>();

    for task_id in &task_ids {
        let exit_path = task_file(storage.path(), SESSION, task_id, "exit");
        let started = Instant::now();
        loop {
            if exit_path.exists() {
                let content = fs::read_to_string(&exit_path).unwrap();
                assert_eq!(
                    content.trim(),
                    "0",
                    "partial exit marker for {task_id}: {content:?}"
                );
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(4));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(aft.shutdown().success());
}

#[test]
fn pre_spawn_metadata_starting_replays_as_failed() {
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-starting";
    let metadata = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "true".to_string(),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        Some(tempfile::tempdir().unwrap().path().to_path_buf()),
        None,
        true,
        true,
    );
    let path = task_file(storage.path(), SESSION, task_id, "json");
    write_task(&path, &metadata).unwrap();

    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.replay_session(storage.path(), SESSION).unwrap();
    let replayed = read_json(storage.path(), SESSION, task_id);
    assert_eq!(replayed["status"], "failed");
    assert_eq!(replayed["status_reason"], "spawn aborted");
    let completions = registry.drain_completions_for_session(Some(SESSION));
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].status, BgTaskStatus::Failed);
    assert_eq!(
        registry.ack_completions_for_session(Some(SESSION), &[task_id.to_string()]),
        vec![task_id.to_string()]
    );
    assert_eq!(
        read_json(storage.path(), SESSION, task_id)["completion_delivered"],
        true
    );
}

#[test]
fn terminal_state_monotonic_killed_wins_late_exit_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);
    let task_id = spawn_bg(&mut aft, SESSION, "sleep 5", None);

    let killed = aft.send(
        &json!({
            "id": "kill-monotonic",
            "session_id": SESSION,
            "command": "bash_kill",
            "params": { "task_id": task_id }
        })
        .to_string(),
    );
    assert_eq!(killed["status"], "killed");
    fs::write(task_file(storage.path(), SESSION, &task_id, "exit"), "0").unwrap();

    let after = status(&mut aft, SESSION, &task_id);
    assert_eq!(after["status"], "killed");
    assert_eq!(
        read_json(storage.path(), SESSION, &task_id)["status"],
        "killed"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn completion_durability_replays_undelivered_terminal_task() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let task_id = {
        let mut aft = AftProcess::spawn();
        configure_background(&mut aft, project.path(), storage.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, "echo durable", None);
        let _ = wait_for_status(&mut aft, SESSION, &task_id, "completed");
        assert_eq!(
            read_json(storage.path(), SESSION, &task_id)["completion_delivered"],
            false
        );
        assert!(aft.shutdown().success());
        task_id
    };

    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);
    let drained = drain(&mut aft, SESSION);
    assert!(drained["bg_completions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|completion| completion["task_id"] == task_id));
    assert_eq!(
        read_json(storage.path(), SESSION, &task_id)["completion_delivered"],
        false
    );
    let acked = ack(&mut aft, SESSION, &task_id);
    assert_eq!(acked["success"], true, "ack failed: {acked:?}");
    assert_eq!(
        read_json(storage.path(), SESSION, &task_id)["completion_delivered"],
        true
    );
    assert!(aft.shutdown().success());
}

#[test]
fn persistence_restore_does_not_push_completion_frame() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let task_id = {
        let mut aft = AftProcess::spawn();
        configure_background(&mut aft, project.path(), storage.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, "echo restored", None);
        let _ = wait_for_status(&mut aft, SESSION, &task_id, "completed");
        assert!(aft.shutdown().success());
        task_id
    };

    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);

    // Skip the async configure_warnings frame that fires after every configure
    // (introduced when configure stopped doing the file walk synchronously).
    // Anything other than that frame would mean restore emitted an unexpected
    // bash-completion frame, which would be the real bug this test guards.
    let mut deadline_iter = 0;
    loop {
        match aft.try_read_next_timeout(Duration::from_millis(250)) {
            None => break,
            Some(frame)
                if frame.get("type").and_then(|v| v.as_str()) == Some("configure_warnings") =>
            {
                // expected — keep looking
            }
            Some(other) => {
                panic!("restore unexpectedly emitted a push frame: {other:?}");
            }
        }
        deadline_iter += 1;
        assert!(
            deadline_iter < 4,
            "configure_warnings appeared more than once after restore"
        );
    }

    let drained = drain(&mut aft, SESSION);
    assert!(drained["bg_completions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|completion| completion["task_id"] == task_id));
    let acked = ack(&mut aft, SESSION, &task_id);
    assert_eq!(acked["success"], true, "ack failed: {acked:?}");
    assert!(aft.shutdown().success());
}

#[test]
fn kill_marker_idempotency_terminal_and_racy_exit() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);

    let done = spawn_bg(&mut aft, SESSION, "true", None);
    let completed = wait_for_status(&mut aft, SESSION, &done, "completed");
    let killed_done = aft.send(
        &json!({"id":"kill-done","session_id":SESSION,"command":"bash_kill","params":{"task_id":done}})
            .to_string(),
    );
    assert_eq!(killed_done["status"], completed["status"]);

    let racy = spawn_bg(&mut aft, SESSION, "sleep 5", None);
    fs::write(task_file(storage.path(), SESSION, &racy, "exit"), "0").unwrap();
    let killed = aft.send(
        &json!({"id":"kill-racy","session_id":SESSION,"command":"bash_kill","params":{"task_id":racy}})
            .to_string(),
    );
    assert_eq!(killed["success"], true);
    assert_eq!(
        fs::read_to_string(task_file(storage.path(), SESSION, &racy, "exit")).unwrap(),
        "0"
    );
    assert!(aft.shutdown().success());
}

#[test]
fn disk_read_tail_does_not_truncate_live_file() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);
    let command = "for i in $(seq 1 200); do printf '%01024d' 0; sleep 0.01; done";
    let task_id = spawn_bg(&mut aft, SESSION, command, None);
    let stdout_path = task_file(storage.path(), SESSION, &task_id, "stdout");

    std::thread::sleep(Duration::from_millis(600));
    let before = fs::metadata(&stdout_path).unwrap().len();
    let snapshot = status(&mut aft, SESSION, &task_id);
    assert!(!snapshot["output_preview"].as_str().unwrap().is_empty());
    std::thread::sleep(Duration::from_millis(600));
    let after = fs::metadata(&stdout_path).unwrap().len();
    assert!(
        after > before,
        "live stdout did not keep growing after tail read: {before}->{after}"
    );
    let _ = wait_for_status(&mut aft, SESSION, &task_id, "completed");
    assert!(aft.shutdown().success());
}

#[test]
fn watchdog_deadline_enforcement_without_status_query() {
    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);
    let task_id = spawn_bg(&mut aft, SESSION, "sleep 5", Some(1000));
    std::thread::sleep(Duration::from_millis(1800));
    let timed_out = status(&mut aft, SESSION, &task_id);
    assert_eq!(
        timed_out["status"], "timed_out",
        "watchdog did not time out task: {timed_out:?}"
    );
    assert_eq!(timed_out["exit_code"], 124);
    assert!(aft.shutdown().success());
}

#[test]
fn session_isolation_on_replay() {
    let project = tempfile::tempdir().unwrap();
    let other_project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let mut aft_a = AftProcess::spawn();
    configure_background(&mut aft_a, project.path(), storage.path(), "session-a");
    let task_id = spawn_bg(&mut aft_a, "session-a", "sleep 1", None);
    assert!(aft_a.shutdown().success());

    let mut aft_b = AftProcess::spawn();
    configure_background(
        &mut aft_b,
        other_project.path(),
        storage.path(),
        "session-b",
    );
    let missing = status(&mut aft_b, "session-b", &task_id);
    assert_eq!(missing["success"], false);
    assert!(aft_b.shutdown().success());

    let mut aft_a2 = AftProcess::spawn();
    configure_background(&mut aft_a2, project.path(), storage.path(), "session-a");
    assert_eq!(status(&mut aft_a2, "session-a", &task_id)["success"], true);
    let _ = wait_for_status(&mut aft_a2, "session-a", &task_id, "completed");
    assert!(aft_a2.shutdown().success());
}

#[test]
fn replay_stale_running_task_marks_killed_orphaned() {
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-stale";
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "sleep 99".to_string(),
        tempfile::tempdir().unwrap().path().to_path_buf(),
        Some(tempfile::tempdir().unwrap().path().to_path_buf()),
        None,
        true,
        true,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.started_at = metadata.started_at.saturating_sub(25 * 60 * 60 * 1000);
    metadata.child_pid = Some(999_999);
    metadata.pgid = Some(999_999);
    write_task(
        &task_file(storage.path(), SESSION, task_id, "json"),
        &metadata,
    )
    .unwrap();

    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.replay_session(storage.path(), SESSION).unwrap();
    let replayed = read_json(storage.path(), SESSION, task_id);
    assert_eq!(replayed["status"], "killed");
    assert_eq!(replayed["status_reason"], "orphaned (>24h)");
}

#[test]
fn replay_session_preserves_started_at_relative_offset() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let task_id = "bash-timeout-offset";
    let paths = task_paths(storage.path(), SESSION, task_id);
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "sleep 99".to_string(),
        project.path().to_path_buf(),
        Some(project.path().to_path_buf()),
        Some(60_000),
        true,
        true,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.started_at = metadata.started_at.saturating_sub(61_000);
    write_task(&paths.json, &metadata).unwrap();
    fs::write(&paths.stdout, "").unwrap();
    fs::write(&paths.stderr, "").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();

    let started = Instant::now();
    loop {
        let snapshot = registry
            .status(
                task_id,
                SESSION,
                Some(project.path()),
                Some(storage.path()),
                1024,
            )
            .expect("rehydrated task should be present");
        if snapshot.info.status == BgTaskStatus::TimedOut {
            assert_eq!(snapshot.exit_code, Some(124));
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "watchdog did not preserve elapsed timeout offset: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn watchdog_marks_rehydrated_detached_task_failed_when_pid_dies_without_marker() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let task_id = "bash-detached-dead-no-marker";
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stand-in child process");
    let child_pid = child.id();
    let paths = task_paths(storage.path(), SESSION, task_id);
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        "sleep 60".to_string(),
        project.path().to_path_buf(),
        Some(project.path().to_path_buf()),
        Some(60_000),
        true,
        true,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.child_pid = Some(child_pid);
    metadata.pgid = Some(child_pid as i32);
    write_task(&paths.json, &metadata).unwrap();
    fs::write(&paths.stdout, "").unwrap();
    fs::write(&paths.stderr, "").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let running = registry
        .status(
            task_id,
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .expect("rehydrated task should be present");
    assert_eq!(running.info.status, BgTaskStatus::Running);

    child.kill().expect("kill stand-in child process");
    child.wait().expect("reap stand-in child process");

    let started = Instant::now();
    loop {
        let snapshot = registry
            .status(
                task_id,
                SESSION,
                Some(project.path()),
                Some(storage.path()),
                1024,
            )
            .expect("rehydrated task should remain present");
        if snapshot.info.status == BgTaskStatus::Failed {
            let replayed = read_json(storage.path(), SESSION, task_id);
            assert_eq!(
                replayed["status_reason"],
                "process exited without exit marker"
            );
            assert_eq!(snapshot.exit_code, None);
            let completions = registry.drain_completions_for_session(Some(SESSION));
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].status, BgTaskStatus::Failed);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "watchdog did not fail detached dead task: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn bash_kill_preserves_real_exit_code_when_marker_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "sleep 5",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    fs::write(task_file(storage.path(), SESSION, &task_id, "exit"), "7").unwrap();

    let snapshot = registry.kill(&task_id, SESSION).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Failed);
    assert_eq!(snapshot.exit_code, Some(7));
}

#[test]
fn failed_spawn_cleans_up_bundle() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let missing_workdir = project.path().join("does-not-exist");
    let registry = registry();

    let err = registry
        .spawn(
            "true",
            SESSION.to_string(),
            missing_workdir,
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap_err();

    assert!(err.contains("failed to spawn background bash command"));
    assert!(!session_tasks_dir(storage.path(), SESSION).exists());
}

#[test]
fn replay_completion_carries_preview() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-preview",
        BgTaskStatus::Completed,
        false,
    );
    fs::write(&paths.stdout, "preview survives replay\n").unwrap();
    fs::write(&paths.stderr, "").unwrap();

    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    let completions = registry.drain_completions_for_session(Some(SESSION));

    assert_eq!(completions.len(), 1);
    assert!(completions[0]
        .output_preview
        .contains("preview survives replay"));
}

#[cfg(unix)]
#[test]
fn background_bash_uses_bash_syntax_when_available() {
    if which::which("bash").is_err() {
        eprintln!("skipping: bash not available on PATH");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            "[[ 1 -eq 1 ]] && echo ok",
            SESSION.to_string(),
            project.path().to_path_buf(),
            Default::default(),
            Some(Duration::from_secs(30)),
            storage.path().to_path_buf(),
            10,
            true,
            false,
            Some(project.path().to_path_buf()),
        )
        .unwrap();
    wait_for_path(&task_file(storage.path(), SESSION, &task_id, "exit"));

    let snapshot = registry
        .status(
            &task_id,
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert_eq!(snapshot.exit_code, Some(0));
    assert!(snapshot.output_preview.contains("ok"));
}
