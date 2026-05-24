#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aft::backup::BackupStore;
use aft::bash_background::persistence::{task_paths, write_task, PersistedTask};
use aft::bash_background::{BgTaskRegistry, BgTaskStatus};
use aft::harness::Harness;
use rusqlite::Connection;

const SESSION: &str = "db-read-fallback-session";
const PROJECT_KEY: &str = "db-read-fallback-project";

fn registry_with_db(storage: &Path) -> (BgTaskRegistry, Arc<Mutex<Connection>>) {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(Harness::Opencode);
    let conn = aft::db::open(&storage.join("aft.db")).unwrap();
    let shared = Arc::new(Mutex::new(conn));
    registry.set_db_pool(shared.clone());
    (registry, shared)
}

fn fresh_registry(conn: Arc<Mutex<Connection>>) -> BgTaskRegistry {
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(Harness::Opencode);
    registry.set_db_pool(conn);
    registry
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
        .unwrap()
}

fn wait_for_task_status(
    registry: &BgTaskRegistry,
    storage: &Path,
    project: &Path,
    task_id: &str,
    expected: BgTaskStatus,
) {
    let started = Instant::now();
    loop {
        if registry
            .status(task_id, SESSION, Some(project), Some(storage), 1024)
            .is_some_and(|snapshot| snapshot.info.status == expected)
        {
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(8));
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn write_disk_task(storage: &Path, project: &Path, task_id: &str, command: &str) {
    let paths = task_paths(storage, SESSION, task_id);
    let mut task = PersistedTask::starting(
        task_id.to_string(),
        SESSION.to_string(),
        command.to_string(),
        project.to_path_buf(),
        Some(project.to_path_buf()),
        None,
        true,
        true,
    );
    task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &task).unwrap();
    fs::write(&paths.stdout, "disk-output\n").unwrap();
    fs::write(&paths.stderr, "").unwrap();
    fs::write(&paths.exit, "0").unwrap();
}

fn backup_store_with_db(storage: &Path) -> (BackupStore, Arc<Mutex<Connection>>) {
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.to_path_buf(), 72);
    store.set_db_harness(Harness::Opencode);
    store.set_db_project_key(PROJECT_KEY.to_string());
    let conn = aft::db::open(&storage.join("aft.db")).unwrap();
    let shared = Arc::new(Mutex::new(conn));
    store.set_db_pool(shared.clone());
    (store, shared)
}

fn fresh_backup_store(storage: &Path, conn: Arc<Mutex<Connection>>) -> BackupStore {
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.to_path_buf(), 72);
    store.set_db_harness(Harness::Opencode);
    store.set_db_project_key(PROJECT_KEY.to_string());
    store.set_db_pool(conn);
    store
}

fn temp_file(project: &Path, name: &str, content: &str) -> PathBuf {
    let path = project.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn db_backup_count(conn: &Arc<Mutex<Connection>>) -> i64 {
    conn.lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM backups", [], |row| row.get(0))
        .unwrap()
}

fn latest_backup_path(conn: &Arc<Mutex<Connection>>) -> PathBuf {
    let value: String = conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT backup_path FROM backups WHERE backup_path IS NOT NULL ORDER BY order_blob DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    PathBuf::from(value)
}

#[test]
fn db_read_fallback_bash_lookup_prefers_db_row_when_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path());
    let task_id = spawn_task(&registry, storage.path(), project.path(), "echo db-wins");
    wait_for_task_status(
        &registry,
        storage.path(),
        project.path(),
        &task_id,
        BgTaskStatus::Completed,
    );
    registry.detach();

    fs::write(
        &task_paths(storage.path(), SESSION, &task_id).json,
        "not json",
    )
    .unwrap();
    let fresh = fresh_registry(conn);
    let snapshot = fresh
        .status(
            &task_id,
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert_eq!(snapshot.info.command, "echo db-wins");
    fresh.detach();
}

#[test]
fn db_read_fallback_bash_lookup_falls_back_to_disk_when_db_row_absent() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (_registry, conn) = registry_with_db(storage.path());
    write_disk_task(
        storage.path(),
        project.path(),
        "disk-only",
        "echo disk-only",
    );

    let fresh = fresh_registry(conn);
    let snapshot = fresh
        .status(
            "disk-only",
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    assert_eq!(snapshot.info.command, "echo disk-only");
    fresh.detach();
}

#[test]
fn db_read_fallback_bash_lookup_returns_not_found_when_both_missing() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (_registry, conn) = registry_with_db(storage.path());
    let fresh = fresh_registry(conn);

    assert!(fresh
        .status(
            "missing-task",
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .is_none());
    fresh.detach();
}

#[test]
fn db_read_fallback_bash_replay_session_prefers_db_when_any_rows_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path());
    let task_ids: Vec<_> = (0..3)
        .map(|idx| {
            spawn_task(
                &registry,
                storage.path(),
                project.path(),
                &format!("echo {idx}"),
            )
        })
        .collect();
    for task_id in &task_ids {
        wait_for_task_status(
            &registry,
            storage.path(),
            project.path(),
            task_id,
            BgTaskStatus::Completed,
        );
    }
    registry.detach();
    fs::remove_file(&task_paths(storage.path(), SESSION, &task_ids[0]).json).unwrap();
    fs::remove_file(&task_paths(storage.path(), SESSION, &task_ids[1]).json).unwrap();

    let fresh = fresh_registry(conn);
    fresh.replay_session(storage.path(), SESSION).unwrap();
    assert_eq!(fresh.list(0).len(), 3);
    fresh.detach();
}

#[test]
fn db_read_fallback_bash_replay_session_falls_back_to_disk_when_db_empty() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (_registry, conn) = registry_with_db(storage.path());
    for idx in 0..3 {
        write_disk_task(
            storage.path(),
            project.path(),
            &format!("disk-only-{idx}"),
            &format!("echo disk {idx}"),
        );
    }

    let fresh = fresh_registry(conn);
    fresh.replay_session(storage.path(), SESSION).unwrap();
    assert_eq!(fresh.list(0).len(), 3);
    fresh.detach();
}

#[test]
fn db_read_fallback_bash_replay_session_no_double_count_when_both_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (registry, conn) = registry_with_db(storage.path());
    let task_ids: Vec<_> = (0..3)
        .map(|idx| {
            spawn_task(
                &registry,
                storage.path(),
                project.path(),
                &format!("echo {idx}"),
            )
        })
        .collect();
    for task_id in &task_ids {
        wait_for_task_status(
            &registry,
            storage.path(),
            project.path(),
            task_id,
            BgTaskStatus::Completed,
        );
    }
    registry.detach();

    let fresh = fresh_registry(conn);
    fresh.replay_session(storage.path(), SESSION).unwrap();
    let ids: Vec<_> = fresh
        .list(0)
        .into_iter()
        .map(|snapshot| snapshot.info.task_id)
        .collect();

    assert_eq!(ids.len(), 3);
    for task_id in task_ids {
        assert_eq!(ids.iter().filter(|id| *id == &task_id).count(), 1);
    }
    fresh.detach();
}

#[test]
fn db_read_fallback_backup_iter_prefers_db_order_by_order_blob() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = backup_store_with_db(storage.path());
    let file = temp_file(project.path(), "ordered.txt", "v1");
    store.snapshot(SESSION, &file, "first").unwrap();
    fs::write(&file, "v2").unwrap();
    store.snapshot(SESSION, &file, "second").unwrap();
    fs::write(&file, "v3").unwrap();
    store.snapshot(SESSION, &file, "third").unwrap();

    let history = fresh_backup_store(storage.path(), conn).history(SESSION, &file);

    assert_eq!(
        history
            .into_iter()
            .map(|entry| entry.description)
            .collect::<Vec<_>>(),
        vec!["first", "second", "third"]
    );
}

#[test]
fn db_read_fallback_backup_iter_falls_back_to_disk_when_db_empty() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.path().to_path_buf(), 72);
    let file = temp_file(project.path(), "disk-history.txt", "v1");
    store.snapshot(SESSION, &file, "disk first").unwrap();
    fs::write(&file, "v2").unwrap();
    store.snapshot(SESSION, &file, "disk second").unwrap();
    let (_db_store, conn) = backup_store_with_db(storage.path());

    let history = fresh_backup_store(storage.path(), conn).history(SESSION, &file);

    assert_eq!(history.len(), 2);
    assert_eq!(history[0].description, "disk first");
    assert_eq!(history[1].description, "disk second");
}

#[test]
fn db_read_fallback_backup_pop_prefers_db_when_present() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = backup_store_with_db(storage.path());
    let file = temp_file(project.path(), "pop.txt", "v1");
    store.snapshot(SESSION, &file, "first").unwrap();
    fs::write(&file, "v2").unwrap();
    store.snapshot(SESSION, &file, "second").unwrap();
    fs::write(&file, "v3").unwrap();
    store.snapshot(SESSION, &file, "third").unwrap();
    fs::write(&file, "current").unwrap();
    let latest_path = latest_backup_path(&conn);

    let mut fresh = fresh_backup_store(storage.path(), conn.clone());
    let (entry, _) = fresh.restore_latest(SESSION, &file).unwrap();

    assert_eq!(entry.description, "third");
    assert_eq!(db_backup_count(&conn), 2);
    assert!(!latest_path.exists());
}

#[test]
fn db_read_fallback_backup_pop_falls_back_to_disk_when_db_empty() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.path().to_path_buf(), 72);
    let file = temp_file(project.path(), "disk-pop.txt", "before");
    store.snapshot(SESSION, &file, "disk only").unwrap();
    fs::write(&file, "after").unwrap();
    let (_db_store, conn) = backup_store_with_db(storage.path());

    let mut fresh = fresh_backup_store(storage.path(), conn.clone());
    let (entry, _) = fresh.restore_latest(SESSION, &file).unwrap();

    assert_eq!(entry.description, "disk only");
    assert_eq!(fs::read_to_string(&file).unwrap(), "before");
    assert_eq!(db_backup_count(&conn), 0);
}

#[test]
fn db_read_fallback_backup_op_id_query_via_db() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = backup_store_with_db(storage.path());
    let file = temp_file(project.path(), "op.txt", "v1");
    store
        .snapshot_with_op(SESSION, &file, "op-a", Some("op-a"))
        .unwrap();
    fs::write(&file, "v2").unwrap();
    store
        .snapshot_with_op(SESSION, &file, "op-b", Some("op-b"))
        .unwrap();
    fs::write(&file, "v3").unwrap();
    store
        .snapshot_with_op(SESSION, &file, "op-a again", Some("op-a"))
        .unwrap();

    let conn = conn.lock().unwrap();
    let rows = aft::db::backups::list_backups_by_op(&conn, "opencode", SESSION, "op-a").unwrap();

    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.op_id.as_deref() == Some("op-a")));
}

#[test]
fn db_read_fallback_db_unavailable_falls_back_to_disk_for_all_ops() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    write_disk_task(storage.path(), project.path(), "no-db-task", "echo no-db");
    let registry = BgTaskRegistry::new(Arc::new(Mutex::new(None)));
    registry.set_harness(Harness::Opencode);
    let snapshot = registry
        .status(
            "no-db-task",
            SESSION,
            Some(project.path()),
            Some(storage.path()),
            1024,
        )
        .unwrap();
    assert_eq!(snapshot.info.command, "echo no-db");
    registry.detach();

    let mut disk_store = BackupStore::new();
    disk_store.set_storage_dir(storage.path().to_path_buf(), 72);
    let file = temp_file(project.path(), "no-db-backup.txt", "before");
    disk_store.snapshot(SESSION, &file, "no db backup").unwrap();
    fs::write(&file, "after").unwrap();

    let mut fresh = BackupStore::new();
    fresh.set_storage_dir(storage.path().to_path_buf(), 72);
    assert_eq!(fresh.history(SESSION, &file).len(), 1);
    let (entry, _) = fresh.restore_latest(SESSION, &file).unwrap();
    assert_eq!(entry.description, "no db backup");
    assert_eq!(fs::read_to_string(&file).unwrap(), "before");
}
