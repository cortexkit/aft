#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aft::bash_background::persistence::{
    resolve_task_layout, session_tasks_dir, task_bundle_files, task_paths, write_task,
    PersistedTask,
};
use aft::bash_background::{BgTaskRegistry, BgTaskStatus};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use super::helpers::{user_config, AftProcess, ReleaseOnDrop};

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
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
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
            "config": user_config(serde_json::json!({
                "experimental": { "bash": { "background": true } }
            })),
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

fn notify_once(aft: &mut AftProcess, session: &str, task_id: &str, pattern: &str) -> Value {
    aft.send(
        &json!({
            "id": "notify-persist-bg",
            "session_id": session,
            "command": "bash_notify",
            "params": { "task_id": task_id, "pattern": pattern, "once": true }
        })
        .to_string(),
    )
}

fn wait_for_pattern_frame(aft: &mut AftProcess, task_id: &str) -> Value {
    let started = Instant::now();
    loop {
        if let Some(frame) = aft.try_read_next_timeout(Duration::from_millis(200)) {
            if frame["type"] == "bash_pattern_match" && frame["task_id"] == task_id {
                return frame;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timed out waiting for pattern frame for {task_id}"
        );
    }
}

fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn sigkill_aft(aft: AftProcess) {
    let pid = aft.pid();
    let result = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    assert_eq!(result, 0, "failed to SIGKILL aft process {pid}");
    assert!(
        !aft.shutdown().success(),
        "SIGKILLed aft exited successfully"
    );
}

fn process_is_alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("query child process state");
    output.status.success() && !String::from_utf8_lossy(&output.stdout).contains('Z')
}

fn persisted_watch_rows(
    storage: &Path,
    session: &str,
    task_id: &str,
) -> Vec<aft::db::bash_watches::BashPatternWatchRow> {
    let conn = rusqlite::Connection::open(storage.join("aft.db")).expect("open watch database");
    aft::db::bash_watches::list_bash_pattern_watches_for_task(&conn, "opencode", session, task_id)
        .expect("read persisted watch rows")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistedOnceMatch {
    watch_id: String,
    match_text: String,
    match_offset: i64,
}

fn persisted_once_match(storage: &Path, session: &str, task_id: &str) -> PersistedOnceMatch {
    let rows = persisted_watch_rows(storage, session, task_id);
    assert_eq!(rows.len(), 1, "expected one durable watch row: {rows:?}");
    let row = &rows[0];
    assert!(row.once, "expected a once-watch row: {row:?}");
    assert!(
        !row.scanning,
        "matched once-watch must stop scanning: {row:?}"
    );
    assert!(
        row.pending_match,
        "matched watch must remain pending: {row:?}"
    );
    PersistedOnceMatch {
        watch_id: row.watch_id.clone(),
        match_text: row.match_text.clone().expect("persisted match text"),
        match_offset: row.match_offset.expect("persisted match offset"),
    }
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
    let session_dir = session_tasks_dir(storage, session);
    if let Ok(task) = resolve_task_layout(&session_dir, task_id) {
        return match suffix {
            "json" => task.paths.json,
            "stdout" => task.paths.stdout,
            "stderr" => task.paths.stderr,
            "exit" => task.paths.exit,
            "pty" => task.paths.pty,
            "sandbox-unavailable" => task.paths.sandbox_unavailable,
            other => task.paths.io_dir.join(other),
        };
    }
    session_dir.join(format!("{task_id}.{suffix}"))
}

fn read_json(storage: &Path, session: &str, task_id: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(task_file(storage, session, task_id, "json")).unwrap())
        .unwrap()
}

fn registry() -> BgTaskRegistry {
    BgTaskRegistry::new(Arc::new(Mutex::new(None)))
}

fn wait_for_path(path: &Path) {
    // File publication is the event; the deadline only catches a wedged child
    // or persistence worker and is deliberately outside normal scheduling cost.
    let started = Instant::now();
    let is_exit_marker = path.file_name().is_some_and(|name| name == "exit")
        || path
            .extension()
            .is_some_and(|extension| extension == "exit");
    while !path.exists()
        || (is_exit_marker && fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0))
    {
        assert!(
            started.elapsed() < Duration::from_secs(30),
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
    let paths = task_paths(storage, session, task_id).unwrap();
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

#[test]
fn configure_repairs_legacy_root_bash_tasks_into_harness_namespace() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-0000000000000100";
    fake_task(
        storage.path(),
        project.path(),
        SESSION,
        task_id,
        BgTaskStatus::Completed,
        true,
    );
    assert!(
        storage.path().join("bash-tasks").exists(),
        "test setup should create legacy root bash tasks"
    );

    let mut aft = AftProcess::spawn();
    configure_background(&mut aft, project.path(), storage.path(), SESSION);

    let harness_json = storage
        .path()
        .join("opencode")
        .join("bash-tasks")
        .join(aft::backup::hash_session(SESSION))
        .join(format!("{task_id}.json"));
    // Configure returns before the legacy-task repair now; poll for the
    // maintenance side effect before checking the migrated layout.
    wait_for_path(&harness_json);
    assert!(
        !storage.path().join("bash-tasks").exists(),
        "legacy root task directory should be removed after repair"
    );

    let response = status(&mut aft, SESSION, task_id);
    assert_eq!(
        response["success"], true,
        "status should find repaired task: {response:?}"
    );
    assert_eq!(response["status"], "completed");

    let status = aft.shutdown();
    assert!(status.success());
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
fn cross_session_project_restart_sweep_delivers_and_acks_fate_unknown() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let task_id = "bash-0000000000000199";
    let paths = task_paths(storage.path(), "session-a", task_id).unwrap();
    let mut metadata = PersistedTask::starting(
        task_id.to_string(),
        "session-a".to_string(),
        "release-command".to_string(),
        project.path().to_path_buf(),
        Some(project.path().to_path_buf()),
        None,
        true,
        true,
    );
    metadata.status = BgTaskStatus::Running;
    metadata.child_pid = Some(999_999);
    metadata.pgid = Some(999_999);
    write_task(&paths.json, &metadata).unwrap();
    fs::write(&paths.stdout, "last release output").unwrap();
    fs::write(&paths.stderr, "").unwrap();

    let registry = registry();
    registry.set_harness(aft::harness::Harness::Opencode);
    let conn = Arc::new(Mutex::new(
        aft::db::open(&storage.path().join("aft.db")).unwrap(),
    ));
    {
        let db = conn.lock().unwrap();
        aft::db::bash_tasks::upsert_bash_task(
            &db,
            &metadata.to_bash_task_row("opencode", &paths).unwrap(),
        )
        .unwrap();
    }
    registry.set_db_pool(conn);

    registry
        .replay_session_for_project(storage.path(), "session-b", project.path())
        .unwrap();
    let completions = registry.drain_completions_for_session(Some("session-b"));
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].status, BgTaskStatus::FateUnknown);
    assert!(completions[0].output_preview.contains("last output at"));
    assert_eq!(
        registry.ack_completions_for_session(Some("session-b"), &[task_id.to_string()]),
        vec![task_id.to_string()]
    );
    assert_eq!(
        read_json(storage.path(), "session-a", task_id)["completion_delivered"],
        true
    );
    registry
        .replay_session_for_project(storage.path(), "session-b", project.path())
        .unwrap();
    assert!(registry
        .drain_completions_for_session(Some("session-b"))
        .is_empty());
}

#[test]
fn bash_status_cross_session_different_project_returns_not_found() {
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let first = registry();
    let task_id = first
        .spawn(
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
        .any(|name| name.starts_with("bash-legacy1.json.invalid-")));
}

#[test]
fn bash_kill_cross_session_still_returns_not_found() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    let task_id = registry
        .spawn(
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            &format!("bash-{idx:016x}"),
            BgTaskStatus::Completed,
            true,
        );
        set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));
    }

    let deleted = registry().maybe_gc_persisted(storage.path()).unwrap();

    assert_eq!(deleted, 5);
    assert!(
        fs::read_dir(session_tasks_dir(storage.path(), SESSION))
            .unwrap()
            .next()
            .is_none(),
        "GC should leave no task entries"
    );
}

#[test]
fn gc_persisted_keeps_undelivered_terminals() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-0000000000000110",
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
        "bash-0000000000000111",
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
    let paths = task_paths(storage.path(), SESSION, "bash-0000000000000112").unwrap();
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
        .any(|name| { name.starts_with("bash-0000000000000112.json.invalid-") }));
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
        "bash-0000000000000113",
        BgTaskStatus::Completed,
        true,
    );
    let succeeding = fake_task(
        storage.path(),
        project.path(),
        "session-ok",
        "bash-0000000000000114",
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
    let paths = task_paths(storage.path(), SESSION, "bash-0000000000000115").unwrap();
    fs::create_dir_all(&paths.dir).unwrap();
    fs::write(&paths.json, "not-json").unwrap();
    for extension in ["stdout", "stderr", "exit", "ps1", "bat", "sh"] {
        fs::write(
            paths.dir.join(format!("bash-0000000000000115.{extension}")),
            extension,
        )
        .unwrap();
    }
    set_mtime(&paths.json, Duration::from_secs(25 * 60 * 60));

    assert_eq!(registry().maybe_gc_persisted(storage.path()).unwrap(), 0);

    assert!(
        task_bundle_files(&paths).iter().all(|path| !path.exists()),
        "corrupt flat bundle siblings remained"
    );
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
                |name| name.starts_with(&format!("bash-0000000000000115.{extension}.invalid-"))
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
    let task_id = "bash-0000000000000116";
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
    let paths = resolve_task_layout(&session_tasks_dir(storage.path(), SESSION), &task_id)
        .unwrap()
        .paths;
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
        "bash-0000000000000117",
        BgTaskStatus::Killing,
        false,
    );
    fs::write(&paths.exit, "0").unwrap();

    registry().replay_session(storage.path(), SESSION).unwrap();
    let replayed = read_json(storage.path(), SESSION, "bash-0000000000000117");

    assert_eq!(replayed["status"], "completed");
    assert_eq!(replayed["exit_code"], 0);
    assert_eq!(
        replayed["status_reason"],
        "recovered from inconsistent killing state on replay"
    );
}

/// The persisted GC walks the whole shared storage root, so it must not run
/// on the thread that called replay: in standalone mode that caller is the
/// request loop, and the first request after configure waited on it (2.4 s on
/// a warm box; past the plugin's 5 s timeout under load). The GC thread name
/// is recorded by the run itself, so the assertion is about where it ran, not
/// how long it took.
#[test]
fn replay_runs_persisted_gc_off_the_calling_thread() {
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let thread = loop {
        if let Some(name) = registry.persisted_gc_thread() {
            break name;
        }
        assert!(
            Instant::now() < deadline,
            "persisted GC never ran after replay"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(thread, "aft-bash-task-gc");
    assert_ne!(
        Some(thread.as_str()),
        std::thread::current().name(),
        "persisted GC ran on the replay caller's thread"
    );
}

#[test]
fn replay_runs_maybe_gc_persisted_once() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = registry();
    registry.replay_session(storage.path(), SESSION).unwrap();
    // The first replay's GC runs detached; the fixture below is aged past the
    // GC threshold on purpose, so it must be planted only after that sweep has
    // finished or the first run (not a second) deletes it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while registry.persisted_gc_thread().is_none() {
        assert!(
            Instant::now() < deadline,
            "first persisted GC never finished after replay"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-0000000000000118",
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

    // The task must still be running when the restarted process rehydrates
    // it. A fixed-duration sleep races the restart under CI load, so gate the
    // task's exit on a sentinel file the test controls instead.
    let stop_file = project.path().join("stop-detached-task");
    // Declare after both TempDirs: Rust drops locals in reverse declaration order,
    // so this guard writes the sentinel before either TempDir removes its directory.
    let _stop_guard = ReleaseOnDrop::new(stop_file.clone());
    let command = format!(
        "polls=0; while [ ! -e '{}' ] && [ \"$polls\" -lt 6000 ]; do sleep 0.1; polls=$((polls + 1)); done; if [ ! -e '{}' ]; then printf 'gate-timeout\\n'; fi",
        stop_file.display(),
        stop_file.display(),
    );

    let task_id = {
        let mut aft = AftProcess::spawn();
        configure_background(&mut aft, project.path(), storage.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, &command, None);
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

    drop(_stop_guard);
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
                if !content.trim().is_empty() {
                    assert_eq!(
                        content.trim(),
                        "0",
                        "partial exit marker for {task_id}: {content:?}"
                    );
                    break;
                }
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
    let task_id = "bash-0000000000000119";
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
fn unacked_once_watch_replays_after_unread_rearm_crash_until_ack() {
    const MATCH_TEXT: &str = "PENDING-ONCE-MATCH";

    let project = tempfile::tempdir().unwrap();
    let storage = spawn_storage_dir("storage");
    let release = project.path().join("release-once-watch");
    let stop = project.path().join("stop-once-watch");
    // Declare both guards after the TempDirs: Rust drops locals in reverse
    // declaration order, so they write their sentinels before either TempDir
    // removes the directory during unwinding.
    let _release_guard = ReleaseOnDrop::new(release.clone());
    let _stop_guard = ReleaseOnDrop::new(stop.clone());
    let command = format!(
        "polls=0; while [ ! -e {} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ ! -e {} ]; then printf 'gate-timeout\\n'; exit 0; fi; printf '%s\\n' '{}'; polls=0; while [ ! -e {} ] && [ \"$polls\" -lt 6000 ]; do sleep 0.05; polls=$((polls + 1)); done; if [ ! -e {} ]; then printf 'gate-timeout\\n'; fi",
        shell_quote_path(&release),
        shell_quote_path(&release),
        MATCH_TEXT,
        shell_quote_path(&stop),
        shell_quote_path(&stop),
    );

    let (task_id, child_pid, persisted_match) = {
        let mut aft = AftProcess::spawn();
        configure_background(&mut aft, project.path(), storage.path(), SESSION);
        let task_id = spawn_bg(&mut aft, SESSION, &command, Some(120_000));
        let registered = notify_once(&mut aft, SESSION, &task_id, MATCH_TEXT);
        assert_eq!(
            registered["success"], true,
            "watch registration failed: {registered:?}"
        );

        drop(_release_guard);
        let frame = wait_for_pattern_frame(&mut aft, &task_id);
        assert_eq!(frame["match_text"], MATCH_TEXT);
        assert_eq!(frame["once"], true);

        let running = status(&mut aft, SESSION, &task_id);
        assert_eq!(
            running["status"], "running",
            "task exited early: {running:?}"
        );
        let child_pid = running["child_pid"].as_u64().expect("running child PID") as u32;
        assert!(process_is_alive(child_pid), "background child is not alive");

        let persisted_match = persisted_once_match(storage.path(), SESSION, &task_id);
        assert_eq!(persisted_match.match_text, MATCH_TEXT);
        assert_eq!(
            frame["match_offset"].as_u64(),
            Some(persisted_match.match_offset as u64)
        );

        sigkill_aft(aft);
        (task_id, child_pid, persisted_match)
    };

    let phase_two_cache = tempfile::tempdir().unwrap();
    let phase_two_ready = project.path().join("phase-two-configured");
    let mut phase_two = Command::new(env!("CARGO_BIN_EXE_aft"))
        .env("AFT_CACHE_DIR", phase_two_cache.path())
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env("AFT_TEST_RAW_PATH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn unread re-arm process");
    {
        let stdin = phase_two.stdin.as_mut().expect("phase-two stdin");
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "cfg-unread-rearm",
                "session_id": SESSION,
                "command": "configure",
                "harness": "opencode",
                "project_root": project.path(),
                "storage_dir": storage.path(),
                "config": user_config(serde_json::json!({
                    "experimental": { "bash": { "background": true } }
                })),
                "max_background_bash_tasks": 32,
            })
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({
                "id": "phase-two-probe",
                "session_id": SESSION,
                "command": "bash",
                "params": {
                    "command": format!(
                        "printf ready > {}",
                        shell_quote_path(&phase_two_ready)
                    )
                }
            })
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    // Drain stdout from a thread while waiting: configure-time frames (status
    // snapshot, rearm replay, ack) exceed the OS pipe buffer, and an undrained
    // pipe deadlocks the child inside its response flush before the sentinel
    // command ever runs (observed live: main thread parked in write(2) on
    // stdout while this test waited for the marker).
    let phase_two_stdout = phase_two.stdout.take().expect("phase-two stdout");
    let phase_two_reader = std::thread::spawn(move || {
        let mut output = String::new();
        let mut stdout = phase_two_stdout;
        let _ = stdout.read_to_string(&mut output);
        output
    });
    wait_for_path(&phase_two_ready);
    phase_two.kill().expect("SIGKILL unread re-arm process");
    let phase_two_status = phase_two.wait().expect("reap unread re-arm process");
    assert!(!phase_two_status.success());
    let phase_two_output = phase_two_reader
        .join()
        .expect("join phase-two stdout reader");
    let phase_two_frame = phase_two_output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|frame| frame["type"] == "bash_pattern_match" && frame["task_id"] == task_id)
        .unwrap_or_else(|| panic!("phase two did not emit the pending match: {phase_two_output}"));
    assert_eq!(phase_two_frame["match_text"], persisted_match.match_text);
    assert_eq!(
        phase_two_frame["match_offset"].as_u64(),
        Some(persisted_match.match_offset as u64)
    );

    let mut phase_three = AftProcess::spawn();
    configure_background(&mut phase_three, project.path(), storage.path(), SESSION);
    let phase_three_frame = wait_for_pattern_frame(&mut phase_three, &task_id);
    assert_eq!(phase_three_frame["match_text"], persisted_match.match_text);
    assert_eq!(
        phase_three_frame["match_offset"].as_u64(),
        Some(persisted_match.match_offset as u64)
    );
    let running = status(&mut phase_three, SESSION, &task_id);
    assert_eq!(
        running["status"], "running",
        "task exited early: {running:?}"
    );
    assert_eq!(running["child_pid"].as_u64(), Some(child_pid as u64));
    assert!(
        process_is_alive(child_pid),
        "background child died across re-arm"
    );
    assert_eq!(
        persisted_once_match(storage.path(), SESSION, &task_id),
        persisted_match,
        "restarts must preserve the one durable match row"
    );

    let acked = ack(&mut phase_three, SESSION, &task_id);
    assert_eq!(acked["success"], true, "ack failed: {acked:?}");
    assert!(acked["acked_task_ids"]
        .as_array()
        .expect("acked task IDs")
        .iter()
        .any(|acked_task_id| acked_task_id == &task_id));
    assert!(
        persisted_watch_rows(storage.path(), SESSION, &task_id).is_empty(),
        "ack must delete the re-armed once-watch row"
    );
    assert!(phase_three.shutdown().success());

    let mut phase_four = AftProcess::spawn();
    configure_background(&mut phase_four, project.path(), storage.path(), SESSION);
    // Replay is synchronous with session restore. A following status response is
    // a protocol barrier: AftProcess queues every push frame that preceded that
    // response, so no observation window or scheduler assumption is required.
    let running = status(&mut phase_four, SESSION, &task_id);
    while let Some(frame) = phase_four.try_read_next_timeout(Duration::ZERO) {
        assert!(
            frame["type"] != "bash_pattern_match" || frame["task_id"] != task_id,
            "acked watch re-delivered after restart: {frame:?}"
        );
    }
    // Positive control: the phase-two and phase-three restarts replayed this
    // same row before ack, while the durable row is absent here.
    assert!(persisted_watch_rows(storage.path(), SESSION, &task_id).is_empty());
    assert_eq!(
        running["status"], "running",
        "task exited early: {running:?}"
    );
    assert_eq!(running["child_pid"].as_u64(), Some(child_pid as u64));
    assert!(
        process_is_alive(child_pid),
        "background child died before release"
    );

    drop(_stop_guard);
    let completed = wait_for_status(&mut phase_four, SESSION, &task_id, "completed");
    assert_eq!(completed["exit_code"], 0);
    let final_ack = ack(&mut phase_four, SESSION, &task_id);
    assert_eq!(
        final_ack["success"], true,
        "final ack failed: {final_ack:?}"
    );
    assert!(phase_four.shutdown().success());
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
    let command = "for i in $(seq 1 80); do printf '%0512d\\n' 0; sleep 0.01; done";
    let task_id = spawn_bg(&mut aft, SESSION, command, None);
    let stdout_path = task_file(storage.path(), SESSION, &task_id, "stdout");

    // Poll for the first output instead of a fixed sleep: on a loaded runner
    // the detached child can take well over half a second to start writing.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let before = loop {
        let len = fs::metadata(&stdout_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if len > 0 || std::time::Instant::now() >= deadline {
            break len;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
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
fn restart_sweep_marks_dead_pid_fate_unknown_once() {
    let storage = tempfile::tempdir().unwrap();
    let task_id = "bash-0000000000000120";
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
    assert_eq!(replayed["status"], "fate_unknown");
    assert!(replayed["status_reason"]
        .as_str()
        .unwrap()
        .contains("daemon restarted, process fate unknown"));
    registry.replay_session(storage.path(), SESSION).unwrap();
    let completions = registry.drain_completions_for_session(Some(SESSION));
    assert_eq!(completions.len(), 1, "restart sweep must enqueue once");
    assert_eq!(completions[0].status, BgTaskStatus::FateUnknown);
    assert!(completions[0]
        .output_preview
        .contains("daemon restarted, process fate unknown"));
    assert!(completions[0].output_preview.contains("last output at"));
}

#[test]
fn restart_sweep_marks_missing_pid_fate_unknown() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let task_id = "bash-0000000000000121";
    let paths = task_paths(storage.path(), SESSION, task_id).unwrap();
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

    let snapshot = registry
        .status(
            task_id,
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .expect("rehydrated task should be present");
    assert_eq!(snapshot.info.status, BgTaskStatus::FateUnknown);
    assert_eq!(snapshot.exit_code, None);
    assert!(snapshot
        .info
        .status_reason
        .as_deref()
        .is_some_and(|reason| reason.contains(paths.io_dir.to_string_lossy().as_ref())));
}

#[test]
fn watchdog_marks_rehydrated_detached_task_fate_unknown_when_pid_dies_without_marker() {
    let storage = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let task_id = "bash-0000000000000122";
    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stand-in child process");
    let child_pid = child.id();
    let paths = task_paths(storage.path(), SESSION, task_id).unwrap();
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
        if snapshot.info.status == BgTaskStatus::FateUnknown {
            let replayed = read_json(storage.path(), SESSION, task_id);
            assert!(replayed["status_reason"]
                .as_str()
                .unwrap()
                .contains("daemon restarted, process fate unknown"));
            assert_eq!(snapshot.exit_code, None);
            // The watchdog marks the task terminal under the state lock but
            // enqueues the completion afterwards (the enqueue does heavy I/O
            // off-lock), so a status poll can observe Failed a beat before the
            // completion is drainable. Settle: accumulate drains until it arrives.
            let mut completions = Vec::new();
            let settle_deadline = Instant::now() + Duration::from_secs(3);
            loop {
                completions.extend(registry.drain_completions_for_session(Some(SESSION)));
                if !completions.is_empty() {
                    break;
                }
                assert!(
                    Instant::now() < settle_deadline,
                    "watchdog marked task Failed but never enqueued its completion"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(completions.len(), 1);
            assert_eq!(completions[0].status, BgTaskStatus::FateUnknown);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "watchdog did not mark detached dead task fate-unknown: {snapshot:?}"
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
    let session_dir = session_tasks_dir(storage.path(), SESSION);
    assert!(
        !session_dir.exists() || fs::read_dir(session_dir).unwrap().next().is_none(),
        "failed spawn left a partial task bundle"
    );
}

#[test]
fn replay_completion_carries_preview() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let paths = fake_task(
        storage.path(),
        project.path(),
        SESSION,
        "bash-0000000000000123",
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
            aft::sandbox_spawn::SpawnPlan::Unsandboxed,
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
