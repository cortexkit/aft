use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aft::backup::BackupStore;
use aft::db::backups::{upsert_backup, BackupRow};
use aft::harness::Harness;
use rusqlite::Connection;

const SESSION: &str = "backup-dual-write-session";
const PROJECT_KEY: &str = "backup-project-key";

#[derive(Debug)]
struct DbBackupRow {
    backup_id: String,
    harness: String,
    session_id: String,
    project_key: String,
    op_id: Option<String>,
    order_blob: Vec<u8>,
    file_path: String,
    path_hash: String,
    backup_path: Option<String>,
    kind: String,
    description: Option<String>,
    created_at: i64,
    is_tombstone: bool,
}

fn store_with_db(storage: &Path, harness: Harness) -> (BackupStore, Arc<Mutex<Connection>>) {
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.to_path_buf(), 72);
    store.set_db_harness(harness);
    store.set_db_project_key(PROJECT_KEY.to_string());
    let conn = aft::db::open(&storage.join("aft.db")).expect("open test DB");
    let shared = Arc::new(Mutex::new(conn));
    store.set_db_pool(shared.clone());
    (store, shared)
}

fn temp_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

fn fetch_rows(conn: &Arc<Mutex<Connection>>, order_by: &str) -> Vec<DbBackupRow> {
    let conn = conn.lock().unwrap();
    let sql = format!(
        "SELECT backup_id, harness, session_id, project_key, op_id, order_blob, file_path,
                path_hash, backup_path, kind, description, created_at, is_tombstone
         FROM backups {order_by}"
    );
    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map([], |row| {
        Ok(DbBackupRow {
            backup_id: row.get(0)?,
            harness: row.get(1)?,
            session_id: row.get(2)?,
            project_key: row.get(3)?,
            op_id: row.get(4)?,
            order_blob: row.get(5)?,
            file_path: row.get(6)?,
            path_hash: row.get(7)?,
            backup_path: row.get(8)?,
            kind: row.get(9)?,
            description: row.get(10)?,
            created_at: row.get(11)?,
            is_tombstone: row.get::<_, i64>(12)? != 0,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn backup_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM backups", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn backups_dual_write_backup_save_writes_both_disk_and_db_row() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(project.path(), "file.txt", "original");

    let backup_id = store
        .snapshot_with_op(SESSION, &file, "before edit", Some("op-save"))
        .unwrap();

    assert_eq!(store.disk_history_count(SESSION, &file), 1);
    let row = fetch_rows(&conn, "").pop().unwrap();
    assert_eq!(row.backup_id, backup_id);
    assert_eq!(row.harness, "opencode");
    assert_eq!(row.session_id, SESSION);
    assert_eq!(row.project_key, PROJECT_KEY);
    assert_eq!(row.op_id.as_deref(), Some("op-save"));
    assert_eq!(row.order_blob.len(), 16);
    assert_eq!(
        row.file_path,
        fs::canonicalize(&file).unwrap().display().to_string()
    );
    assert_eq!(row.kind, "content");
    assert_eq!(row.description.as_deref(), Some("before edit"));
    assert!(!row.is_tombstone);
    assert!(row.created_at > 0);
    let backup_path = PathBuf::from(row.backup_path.unwrap());
    assert!(backup_path.exists());
    assert_eq!(fs::read_to_string(&backup_path).unwrap(), "original");
    assert_eq!(
        backup_path.parent().unwrap().file_name().unwrap().to_str(),
        Some(row.path_hash.as_str())
    );
}

#[test]
fn backups_dual_write_backup_order_preserves_sort_in_db() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(project.path(), "ordered.txt", "v1");

    store.snapshot(SESSION, &file, "first").unwrap();
    fs::write(&file, "v2").unwrap();
    store.snapshot(SESSION, &file, "second").unwrap();
    fs::write(&file, "v3").unwrap();
    store.snapshot(SESSION, &file, "third").unwrap();

    let ids: Vec<String> = fetch_rows(&conn, "ORDER BY order_blob DESC")
        .into_iter()
        .map(|row| row.backup_id)
        .collect();
    assert_eq!(ids, vec!["backup-2", "backup-1", "backup-0"]);
}

#[test]
fn backups_dual_write_backup_op_id_isolates_operations() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(project.path(), "op-id.txt", "v1");

    store
        .snapshot_with_op(SESSION, &file, "op first", Some("op-shared"))
        .unwrap();
    fs::write(&file, "v2").unwrap();
    store
        .snapshot_with_op(SESSION, &file, "op second", Some("op-shared"))
        .unwrap();
    fs::write(&file, "v3").unwrap();
    store.snapshot(SESSION, &file, "no op").unwrap();

    let count: i64 = conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM backups WHERE harness = 'opencode' AND session_id = ?1 AND op_id = 'op-shared'",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn backups_dual_write_backup_harness_isolation_in_db() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    upsert_backup(&conn, &direct_row("opencode", SESSION, "shared", 1)).unwrap();
    upsert_backup(&conn, &direct_row("pi", SESSION, "shared", 1)).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM backups WHERE session_id = ?1 AND path_hash = 'shared'",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn backups_dual_write_backup_db_failure_does_not_break_disk_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(project.path(), "db-failure.txt", "original");
    conn.lock()
        .unwrap()
        .execute("DROP TABLE backups", [])
        .unwrap();

    store.snapshot(SESSION, &file, "before edit").unwrap();

    assert_eq!(store.disk_history_count(SESSION, &file), 1);
}

#[test]
fn backups_dual_write_backup_disabled_db_path_skips_dual_write() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut store = BackupStore::new();
    store.set_storage_dir(storage.path().to_path_buf(), 72);
    store.set_db_harness(Harness::Opencode);
    store.set_db_project_key(PROJECT_KEY.to_string());
    let file = temp_file(project.path(), "no-db.txt", "original");

    store.snapshot(SESSION, &file, "before edit").unwrap();

    assert_eq!(store.disk_history_count(SESSION, &file), 1);
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();
    assert_eq!(backup_count(&conn), 0);
}

#[test]
fn backups_dual_write_backup_order_blob_is_16_bytes() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    upsert_backup(&conn, &direct_row("opencode", SESSION, "len", 42)).unwrap();

    let len: i64 = conn
        .query_row("SELECT length(order_blob) FROM backups", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(len, 16);
}

#[test]
fn backups_dual_write_backup_order_blob_sorts_lexicographically() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    upsert_backup(&conn, &direct_row("opencode", SESSION, "one", 1)).unwrap();
    upsert_backup(&conn, &direct_row("opencode", SESSION, "two", 2)).unwrap();
    upsert_backup(&conn, &direct_row("opencode", SESSION, "max", u128::MAX)).unwrap();

    assert_eq!(backup_ids_ordered(&conn, "ASC"), vec!["one", "two", "max"]);
    assert_eq!(backup_ids_ordered(&conn, "DESC"), vec!["max", "two", "one"]);
}

#[test]
fn backups_dual_write_backup_op_id_index_is_partial() {
    let storage = tempfile::tempdir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_backups_session_op'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(sql.contains("WHERE op_id IS NOT NULL"));

    upsert_backup(
        &conn,
        &direct_row_with_op("opencode", SESSION, "with-op", 1, Some("op")),
    )
    .unwrap();
    upsert_backup(
        &conn,
        &direct_row_with_op("opencode", SESSION, "no-op", 2, None),
    )
    .unwrap();

    let plan = explain_plan(
        &conn,
        "EXPLAIN QUERY PLAN SELECT * FROM backups WHERE harness = 'opencode' AND session_id = 'backup-dual-write-session' AND op_id = 'op'",
    );
    assert!(plan.contains("idx_backups_session_op"));

    let matching: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM backups WHERE harness = 'opencode' AND session_id = ?1 AND op_id = 'op'",
            [SESSION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(matching, 1);
}

fn direct_row(harness: &str, session_id: &str, path_hash: &str, order: u128) -> BackupRow {
    direct_row_with_op(harness, session_id, path_hash, order, None)
}

fn direct_row_with_op(
    harness: &str,
    session_id: &str,
    path_hash: &str,
    order: u128,
    op_id: Option<&str>,
) -> BackupRow {
    BackupRow {
        backup_id: path_hash.to_string(),
        harness: harness.to_string(),
        session_id: session_id.to_string(),
        project_key: PROJECT_KEY.to_string(),
        op_id: op_id.map(str::to_string),
        order,
        file_path: "/tmp/file.txt".to_string(),
        path_hash: path_hash.to_string(),
        backup_path: Some("/tmp/backup.bak".to_string()),
        kind: "content".to_string(),
        description: "direct row".to_string(),
        created_at: 1,
        is_tombstone: false,
    }
}

fn backup_ids_ordered(conn: &Connection, direction: &str) -> Vec<String> {
    let sql = format!("SELECT backup_id FROM backups ORDER BY order_blob {direction}");
    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn explain_plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n")
}
