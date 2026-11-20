use super::display_records::test_support::available;
use super::display_records::{now_ms, persist, read};
use rusqlite::Connection;

#[test]
fn display_read_cache_fresh_upgrade_and_rollback_keep_schema29_and_reader_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let fresh = crate::db::open(&path).unwrap();
    assert_eq!(crate::db::current_schema_version(&fresh).unwrap(), 29);
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
    assert_eq!(crate::db::current_schema_version(&upgraded).unwrap(), 29);
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
