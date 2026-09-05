use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aft::backup::{hash_session, BackupStore};
use aft::db::backups::{upsert_backup, BackupRow};
use aft::db::TrackedConnection as Connection;
use aft::harness::Harness;

#[cfg(unix)]
use super::helpers::{warm_executable, AftProcess};

const SESSION: &str = "backup-dual-write-session";
const PROJECT_KEY: &str = "backup-project-key";

fn non_temp_dir() -> std::io::Result<tempfile::TempDir> {
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/aft-backup-tests");
    fs::create_dir_all(&parent)?;
    tempfile::Builder::new().prefix("case-").tempdir_in(parent)
}

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
    restore_meta: Option<String>,
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
                path_hash, backup_path, kind, description, created_at, is_tombstone, restore_meta
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
            restore_meta: row.get(13)?,
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
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(project.path(), "file.txt", "original");

    let backup_id = store
        .snapshot_with_op(SESSION, &file, "before edit", Some("op-save"))
        .unwrap()
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
    assert!(row.restore_meta.is_some());
    let backup_path = row.backup_path.unwrap();
    assert!(!Path::new(&backup_path).is_absolute());
    let stack_dir = storage
        .path()
        .join("backups")
        .join(hash_session(SESSION))
        .join(&row.path_hash);
    let resolved_backup_path = stack_dir.join(&backup_path);
    assert!(resolved_backup_path.exists());
    assert_eq!(
        fs::read_to_string(&resolved_backup_path).unwrap(),
        "original"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(stack_dir.join("meta.json")).unwrap()).unwrap();
    let meta_content_path = meta
        .get("entries")
        .and_then(|entries| entries.as_array())
        .and_then(|entries| entries.first())
        .and_then(|entry| entry.get("content_path"))
        .and_then(|value| value.as_str());
    assert_eq!(meta_content_path, Some(backup_path.as_str()));
}

#[test]
fn backups_dual_write_backup_order_preserves_sort_in_db() {
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
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
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
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
    let storage = non_temp_dir().unwrap();
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
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
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
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
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
fn backup_tombstone_file_undo_uses_same_key_for_relative_created_path() {
    let mut store = BackupStore::new();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let rel_dir = PathBuf::from("target").join(format!(
        "aft-backup-relative-tombstone-{}-{unique}",
        std::process::id()
    ));
    let rel_path = rel_dir.join("created.txt");
    let _ = fs::remove_dir_all(&rel_dir);

    store
        .snapshot_op_tombstone(SESSION, "op-relative-created", &rel_path, "created")
        .unwrap();
    fs::create_dir_all(&rel_dir).unwrap();
    fs::write(&rel_path, "created").unwrap();

    let (entry, _) = store.restore_latest(SESSION, &rel_path).unwrap();

    assert_eq!(entry.kind, aft::backup::BackupEntryKind::Tombstone);
    assert!(!rel_path.exists(), "created file should be removed");
    let _ = fs::remove_dir_all(&rel_dir);
}

#[test]
fn backup_restore_round_trips_binary_bytes() {
    let project = non_temp_dir().unwrap();
    let mut store = BackupStore::new();
    let file = project.path().join("binary.bin");
    let original = vec![0, 159, 146, 150, 255, b'\n'];
    fs::write(&file, &original).unwrap();

    store.snapshot(SESSION, &file, "binary").unwrap();
    fs::write(&file, b"changed").unwrap();
    store.restore_latest(SESSION, &file).unwrap();

    assert_eq!(fs::read(&file).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn backup_restore_preserves_unix_permissions() {
    let project = non_temp_dir().unwrap();
    let mut store = BackupStore::new();
    let file = temp_file(project.path(), "executable.sh", "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    warm_executable(&file, &["--version"]);

    store.snapshot(SESSION, &file, "executable").unwrap();
    fs::write(&file, "changed\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    store.restore_latest(SESSION, &file).unwrap();

    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[cfg(unix)]
#[test]
fn legacy_db_row_without_restore_meta_reads_mode_from_sidecar() {
    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
    let (mut store, conn) = store_with_db(storage.path(), Harness::Opencode);
    let file = temp_file(
        project.path(),
        "legacy-executable.sh",
        "#!/bin/sh\nexit 0\n",
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    warm_executable(&file, &["--version"]);
    store.snapshot(SESSION, &file, "legacy executable").unwrap();

    let summary = fetch_rows(&conn, "").pop().unwrap();
    conn.lock()
        .unwrap()
        .execute("UPDATE backups SET restore_meta = NULL", [])
        .unwrap();
    let mut row = aft::db::backups::list_backups(
        &conn.lock().unwrap(),
        "opencode",
        SESSION,
        &summary.path_hash,
    )
    .unwrap()
    .pop()
    .unwrap();
    assert_eq!(row.restore_meta, None);
    let relative_backup_path = row.backup_path.take().unwrap();
    row.backup_path = Some(
        storage
            .path()
            .join("backups")
            .join(hash_session(SESSION))
            .join(&summary.path_hash)
            .join(relative_backup_path)
            .display()
            .to_string(),
    );

    let restored = aft::backup::BackupEntry::try_from(row).unwrap();

    assert_eq!(restored.mode.map(|mode| mode & 0o777), Some(0o755));
}

#[cfg(unix)]
#[test]
fn binary_db_fallback_restores_mode_without_sidecar() {
    const BINARY_SESSION: &str = "binary-db-restore-metadata-session";

    let project = non_temp_dir().unwrap();
    let storage = non_temp_dir().unwrap();
    let file = temp_file(project.path(), "tool.sh", "#!/bin/sh\necho ORIGINAL\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    warm_executable(&file, &["--version"]);
    let configure = |id: &str| {
        serde_json::json!({
            "id": id,
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
            "session_id": BINARY_SESSION,
            "config": { "format_on_edit": false },
        })
    };

    let mut first = AftProcess::spawn();
    let configured = first.send(&configure("configure-first").to_string());
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let edited = first.send(
        &serde_json::json!({
            "id": "edit",
            "command": "edit_match",
            "session_id": BINARY_SESSION,
            "file": &file,
            "match": "ORIGINAL",
            "replacement": "EDITED",
        })
        .to_string(),
    );
    assert_eq!(edited["success"], true, "edit failed: {edited:?}");
    assert!(first.shutdown().success());

    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();
    let path_hash: String = conn
        .query_row(
            "SELECT path_hash FROM backups WHERE session_id = ?1",
            [BINARY_SESSION],
            |row| row.get(0),
        )
        .unwrap();
    let meta_path = storage
        .path()
        .join("opencode")
        .join("backups")
        .join(hash_session(BINARY_SESSION))
        .join(path_hash)
        .join("meta.json");
    fs::remove_file(&meta_path).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

    let mut second = AftProcess::spawn();
    let configured = second.send(&configure("configure-second").to_string());
    assert_eq!(
        configured["success"], true,
        "configure failed: {configured:?}"
    );
    let undone = second.send(
        &serde_json::json!({
            "id": "undo",
            "command": "undo",
            "session_id": BINARY_SESSION,
            "file": &file,
        })
        .to_string(),
    );
    assert_eq!(undone["success"], true, "undo failed: {undone:?}");
    assert!(second.shutdown().success());

    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "#!/bin/sh\necho ORIGINAL\n"
    );
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
}

#[test]
fn backup_process_maintenance_repairs_legacy_root_backups() {
    let storage = non_temp_dir().unwrap();
    let legacy = storage.path().join("backups").join("legacy-session");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("sentinel"), "legacy").unwrap();

    let mut store = BackupStore::new();
    store.set_storage_dir_for_harness(storage.path().to_path_buf(), Harness::Opencode, 72);
    store.run_process_maintenance_once();

    assert!(
        storage
            .path()
            .join("opencode")
            .join("backups")
            .join("legacy-session")
            .join("sentinel")
            .exists(),
        "legacy root backups should be moved under the configured harness"
    );
    assert!(
        !storage.path().join("backups").exists(),
        "legacy root backups directory should be removed after repair"
    );
}

#[test]
fn backups_dual_write_backup_order_blob_is_16_bytes() {
    let storage = non_temp_dir().unwrap();
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
    let storage = non_temp_dir().unwrap();
    let conn = aft::db::open(&storage.path().join("aft.db")).unwrap();

    upsert_backup(&conn, &direct_row("opencode", SESSION, "one", 1)).unwrap();
    upsert_backup(&conn, &direct_row("opencode", SESSION, "two", 2)).unwrap();
    upsert_backup(&conn, &direct_row("opencode", SESSION, "max", u128::MAX)).unwrap();

    assert_eq!(backup_ids_ordered(&conn, "ASC"), vec!["one", "two", "max"]);
    assert_eq!(backup_ids_ordered(&conn, "DESC"), vec!["max", "two", "one"]);
}

#[test]
fn backups_dual_write_backup_op_id_index_is_partial() {
    let storage = non_temp_dir().unwrap();
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
        restore_meta: None,
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
