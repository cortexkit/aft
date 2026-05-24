#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::bash_background::persistence::{read_task, task_paths, PersistedTask};
use aft::bash_background::{BgTaskRegistry, BgTaskStatus};
use aft::db::bash_tasks::{upsert_bash_task, BashTaskRow};
use aft::harness::Harness;
use rusqlite::Connection;

const SESSION: &str = "dual-write-session";

#[derive(Debug)]
struct DbTaskRow {
    harness: String,
    session_id: String,
    task_id: String,
    project_key: String,
    command: String,
    cwd: String,
    status: String,
    exit_code: Option<i32>,
    pid: Option<i64>,
    pgid: Option<i64>,
    started_at: i64,
    completed_at: Option<i64>,
    stdout_path: Option<String>,
    stderr_path: Option<String>,
    compressed: bool,
    timeout_ms: Option<i64>,
    completion_delivered: bool,
    output_bytes: Option<i64>,
    metadata: String,
}

fn registry_with_db(storage: &Path, harness: Harness) -> (BgTaskRegistry, Arc<Mutex<Connection>>) {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(harness);
    let conn = aft::db::open(&storage.join("aft.db")).expect("open test DB");
    let shared = Arc::new(Mutex::new(conn));
    registry.set_db_pool(shared.clone());
    (registry, shared)
}

fn spawn_task(registry: &BgTaskRegistry, storage: &Path, project: &Path, command: &str) -> String {
    registry
        .spawn(
            command,
            SESSION.to_string(),
            project.to_path_buf(),
            HashMap::new(),
            Some(Duration::from_secs(30)),
            storage.to_path_buf(),
            16,
            true,
            true,
            Some(project.to_path_buf()),
        )
        .expect("spawn background task")
}

fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn wait_for_file(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn hold_until_release_command(marker: &Path, release: &Path) -> String {
    format!(
        "printf ready > {}; while [ ! -f {} ]; do sleep 0.05; done",
        shell_quote_path(marker),
        shell_quote_path(release)
    )
}

fn wait_for_status(
    conn: &Arc<Mutex<Connection>>,
    harness: &str,
    session_id: &str,
    task_id: &str,
    expected: &str,
) -> DbTaskRow {
    let started = Instant::now();
    loop {
        if let Some(row) = fetch_row(conn, harness, session_id, task_id) {
            if row.status == expected {
                return row;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for DB status {expected}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_json_status(storage: &Path, session_id: &str, task_id: &str, expected: BgTaskStatus) {
    let paths = task_paths(storage, session_id, task_id);
    let started = Instant::now();
    loop {
        if let Ok(task) = read_task(&paths.json) {
            if task.status == expected {
                return;
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "timed out waiting for JSON status {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn fetch_row(
    conn: &Arc<Mutex<Connection>>,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> Option<DbTaskRow> {
    let conn = conn.lock().expect("DB lock");
    conn.query_row(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        rusqlite::params![harness, session_id, task_id],
        |row| {
            Ok(DbTaskRow {
                harness: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                project_key: row.get(3)?,
                command: row.get(4)?,
                cwd: row.get(5)?,
                status: row.get(6)?,
                exit_code: row.get(7)?,
                pid: row.get(8)?,
                pgid: row.get(9)?,
                started_at: row.get(10)?,
                completed_at: row.get(11)?,
                stdout_path: row.get(12)?,
                stderr_path: row.get(13)?,
                compressed: row.get::<_, i64>(14)? != 0,
                timeout_ms: row.get(15)?,
                completion_delivered: row.get::<_, i64>(16)? != 0,
                output_bytes: row.get(17)?,
                metadata: row.get(18)?,
            })
        },
    )
    .ok()
}

fn assert_row_matches_task(row: &DbTaskRow, task: &PersistedTask, project: &Path, storage: &Path) {
    let paths = task_paths(storage, &task.session_id, &task.task_id);
    assert_eq!(row.harness, "opencode");
    assert_eq!(row.session_id, task.session_id);
    assert_eq!(row.task_id, task.task_id);
    assert_eq!(
        row.project_key,
        aft::search_index::project_cache_key(project)
    );
    assert_eq!(row.command, task.command);
    assert_eq!(row.cwd, task.workdir.display().to_string());
    assert_eq!(row.exit_code, task.exit_code);
    assert_eq!(row.pid, task.child_pid.map(i64::from));
    assert_eq!(row.pgid, task.pgid.map(i64::from));
    assert_eq!(row.started_at, task.started_at as i64);
    assert_eq!(row.completed_at, task.finished_at.map(|value| value as i64));
    assert_eq!(
        row.stdout_path.as_deref(),
        Some(paths.stdout.to_str().unwrap())
    );
    assert_eq!(
        row.stderr_path.as_deref(),
        Some(paths.stderr.to_str().unwrap())
    );
    assert_eq!(row.compressed, task.compressed);
    assert_eq!(row.timeout_ms, task.timeout_ms.map(|value| value as i64));
    assert_eq!(row.completion_delivered, task.completion_delivered);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.metadata).unwrap(),
        serde_json::to_value(task).unwrap()
    );
}

#[test]
fn bash_tasks_dual_write_spawn_writes_both_json_and_db_row() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(&registry, storage.path(), project.path(), "echo dual-write");
    let row = wait_for_status(&conn, "opencode", SESSION, &task_id, "completed");
    let task = read_task(&task_paths(storage.path(), SESSION, &task_id).json).unwrap();

    assert_row_matches_task(&row, &task, project.path(), storage.path());
    registry.detach();
}

#[test]
fn bash_tasks_dual_write_status_transitions_update_db_row() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let marker = project.path().join("dual-write-running.alive");
    let release = project.path().join("dual-write-running.release");
    let command = hold_until_release_command(&marker, &release);
    let task_id = spawn_task(&registry, storage.path(), project.path(), &command);
    wait_for_file(&marker);
    let running = wait_for_status(&conn, "opencode", SESSION, &task_id, "running");
    assert!(running.pid.is_some());
    assert!(running.completed_at.is_none());

    fs::write(&release, "").expect("release background task");
    let completed = wait_for_status(&conn, "opencode", SESSION, &task_id, "completed");
    assert_eq!(completed.exit_code, Some(0));
    assert!(completed.pid.is_none());
    assert!(completed.completed_at.is_some());
    registry.detach();
}

#[test]
fn bash_tasks_dual_write_background_task_updates_under_watchdog() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo watchdog-done",
    );
    let completed = wait_for_status(&conn, "opencode", SESSION, &task_id, "completed");

    assert_eq!(completed.exit_code, Some(0));
    assert!(completed.output_bytes.unwrap_or_default() > 0);
    registry.detach();
}

#[test]
fn bash_tasks_dual_write_db_failure_does_not_break_json_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);
    conn.lock()
        .unwrap()
        .execute("DROP TABLE bash_tasks", [])
        .unwrap();

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "echo json-survives",
    );
    wait_for_json_status(storage.path(), SESSION, &task_id, BgTaskStatus::Completed);

    assert!(task_paths(storage.path(), SESSION, &task_id).json.exists());
    registry.detach();
}

#[test]
fn bash_tasks_dual_write_output_bytes_propagate_into_db_row() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path(), Harness::Opencode);

    let task_id = spawn_task(
        &registry,
        storage.path(),
        project.path(),
        "printf known-output",
    );
    let completed = wait_for_status(&conn, "opencode", SESSION, &task_id, "completed");

    assert_eq!(completed.output_bytes, Some("known-output".len() as i64));
    registry.detach();
}

#[test]
fn bash_tasks_dual_write_upsert_replaces_not_duplicates() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();
    let mut row = direct_row("opencode", "session", "bash-dupe", "running");
    upsert_bash_task(&conn, &row).unwrap();
    row.status = "completed".to_string();
    row.exit_code = Some(0);
    upsert_bash_task(&conn, &row).unwrap();

    let (count, status): (i64, String) = conn
        .query_row(
            "SELECT COUNT(*), MAX(status) FROM bash_tasks WHERE harness = 'opencode' AND session_id = 'session' AND task_id = 'bash-dupe'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(status, "completed");
}

#[test]
fn bash_tasks_dual_write_harness_isolation_in_db() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    upsert_bash_task(
        &conn,
        &direct_row("opencode", "session", "bash-shared", "running"),
    )
    .unwrap();
    upsert_bash_task(
        &conn,
        &direct_row("pi", "session", "bash-shared", "completed"),
    )
    .unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM bash_tasks WHERE session_id = 'session' AND task_id = 'bash-shared'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn bash_tasks_dual_write_disabled_db_pool_skips_dual_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(Harness::Opencode);

    let task_id = spawn_task(&registry, storage.path(), project.path(), "echo no-db");
    wait_for_json_status(storage.path(), SESSION, &task_id, BgTaskStatus::Completed);

    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    registry.detach();
}

fn direct_row(harness: &str, session_id: &str, task_id: &str, status: &str) -> BashTaskRow {
    BashTaskRow {
        harness: harness.to_string(),
        session_id: session_id.to_string(),
        task_id: task_id.to_string(),
        project_key: "project-key".to_string(),
        command: "echo ok".to_string(),
        cwd: PathBuf::from("/tmp").display().to_string(),
        status: status.to_string(),
        exit_code: None,
        pid: None,
        pgid: None,
        started_at: 1,
        completed_at: None,
        stdout_path: None,
        stderr_path: None,
        compressed: true,
        timeout_ms: None,
        completion_delivered: false,
        output_bytes: None,
        metadata: "{}".to_string(),
    }
}
