use super::*;

fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(CACHE_DDL).unwrap();
    conn
}

#[test]
fn display_read_store_ack_window_is_inclusive_and_unacked_survives_twenty_four_hours() {
    let conn = database();
    let now = 1_000_000;
    assert!(persist(&conn, "acked", b"exact bytes", now).unwrap());
    acknowledged(&conn, "acked", now + 100).unwrap();
    assert_eq!(
        read(&conn, "acked", now + 600_100).unwrap(),
        Some(b"exact bytes".to_vec())
    );
    assert!(read(&conn, "acked", now + 600_101).unwrap().is_none());
    assert!(persist(&conn, "unacked", b"pending", now).unwrap());
    assert!(read(&conn, "unacked", now + 600_101).unwrap().is_some());
    assert!(read(&conn, "unacked", now + 86_400_000).unwrap().is_some());
    assert!(read(&conn, "unacked", now + 86_400_001).unwrap().is_none());
    assert_eq!(sweep(&conn, now + 86_400_001).unwrap(), 2);
}

#[test]
fn display_read_store_defers_count_and_byte_pressure_without_evicting_protected_rows() {
    let now = 1_000_000;
    for size in [1, MAX_BYTES / 2] {
        let conn = database();
        let count = if size == 1 { MAX_RECORDS } else { 2 };
        let bytes = vec![b'x'; size];
        for index in 0..count {
            assert!(persist(&conn, &index.to_string(), &bytes, now).unwrap());
            acknowledged(&conn, &index.to_string(), now).unwrap();
        }
        assert!(!persist(&conn, "deferred", b"new", now + 1).unwrap());
        assert_eq!(read(&conn, "0", now + 1).unwrap(), Some(bytes.clone()));
        assert!(read(&conn, "deferred", now + 1).unwrap().is_none());
        assert!(persist(&conn, "deferred", b"new", now + 600_001).unwrap());
        assert_eq!(
            read(&conn, "deferred", now + 600_001).unwrap(),
            Some(b"new".to_vec())
        );
    }
}

#[test]
fn display_read_store_sweep_bounds_each_page_and_retry_preserves_first_bytes() {
    let conn = database();
    for index in 0..MAX_RECORDS {
        assert!(persist(&conn, &index.to_string(), b"first", 0).unwrap());
    }
    assert!(!persist(&conn, "0", b"replacement", 10).unwrap());
    assert!(persist(&conn, "0", b"first", 20).unwrap());
    assert_eq!(
        read(&conn, "0", 86_400_001).unwrap(),
        Some(b"first".to_vec())
    );
    assert_eq!(sweep(&conn, 86_400_001).unwrap(), SWEEP_LIMIT);
    let remaining: usize = conn
        .query_row("SELECT count(*) FROM display_records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, MAX_RECORDS - SWEEP_LIMIT);
}

#[test]
fn display_read_cache_fresh_upgrade_and_rollback_keep_schema29_and_reader_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let fresh = crate::db::open(&path).unwrap();
    assert_eq!(crate::db::schema_version(&fresh), 29);
    assert!(available(&fresh).unwrap());
    // Model an existing schema-29 database that predates the optional cache.
    fresh.execute_batch("DROP TABLE display_records").unwrap();
    fresh
        .execute(
            "INSERT INTO host_state(key,value,updated_at) VALUES ('rollback','kept',0)",
            [],
        )
        .unwrap();
    drop(fresh);
    assert!(crate::reader_floor::prepare(dir.path()).is_empty());
    let floor_path = crate::reader_floor::floor_path(dir.path());
    let floor_before = std::fs::read(&floor_path).unwrap();
    let upgraded = crate::db::open(&path).unwrap();
    let now = now_ms();
    assert!(persist(&upgraded, "retained", b"opaque pull bytes", now).unwrap());
    assert_eq!(crate::db::schema_version(&upgraded), 29);
    assert_eq!(std::fs::read(&floor_path).unwrap(), floor_before);
    drop(upgraded);
    // The rollback build uses these unchanged version checks. It never needs
    // to know or interpret the optional display cache to open the database.
    let mut rollback = Connection::open(&path).unwrap();
    assert_eq!(crate::db::peek_schema_version(&path), Some(29));
    assert_eq!(
        crate::db::run_migrations_supported(&mut rollback, 29).unwrap(),
        29
    );
    assert_eq!(
        rollback
            .query_row(
                "SELECT value FROM host_state WHERE key='rollback'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "kept"
    );
    assert_eq!(
        read(&rollback, "retained", now).unwrap(),
        Some(b"opaque pull bytes".to_vec())
    );
    assert_eq!(std::fs::read(&floor_path).unwrap(), floor_before);
}

#[test]
fn display_read_cache_absent_or_readonly_is_memory_only_without_errors() {
    let absent = Connection::open_in_memory().unwrap();
    assert!(read(&absent, "any", 0).unwrap().is_none());
    assert!(!persist(&absent, "any", b"bytes", 0).unwrap());
    assert_eq!(sweep(&absent, i64::MAX).unwrap(), 0);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(CACHE_DDL).unwrap();
    assert!(persist(&conn, "any", b"bytes", 0).unwrap());
    drop(conn);
    let readonly =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(read(&readonly, "any", 0).unwrap().is_none());
    assert!(!persist(&readonly, "new", b"bytes", 0).unwrap());
    assert_eq!(sweep(&readonly, i64::MAX).unwrap(), 0);
}

#[test]
fn display_read_cache_writable_open_sweeps_only_one_expired_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let conn = crate::db::open(&path).unwrap();
    for index in 0..MAX_RECORDS {
        assert!(persist(&conn, &index.to_string(), b"expired", 0).unwrap());
    }
    drop(conn);
    let reopened = crate::db::open(&path).unwrap();
    let remaining: usize = reopened
        .query_row("SELECT count(*) FROM display_records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, MAX_RECORDS - SWEEP_LIMIT);
}
