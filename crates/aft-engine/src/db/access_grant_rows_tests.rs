#[test]
fn grant_sources_reader_floor_refusal_then_retry() {
    use crate::persisted_format::{self, PersistedStore};
    let supported = crate::db::CURRENT_SCHEMA_VERSION;
    let dir = tempfile::tempdir().unwrap();
    let floor = crate::reader_floor::floor_path(dir.path());
    std::fs::write(
        &floor,
        format!(
            r#"{{"floor_schema":1,"stores":{{"aft.db":{}}}}}"#,
            supported + 1
        ),
    )
    .unwrap();
    assert_eq!(crate::reader_floor::prepare(dir.path()).len(), 1);
    let path = dir.path().join("aft.db");
    assert!(
        persisted_format::gate(PersistedStore::AftDb, &path, &path, Some(supported.into()))
            .is_err()
    );
    std::fs::write(
        &floor,
        format!(r#"{{"floor_schema":1,"stores":{{"aft.db":{supported}}}}}"#),
    )
    .unwrap();
    filetime::set_file_mtime(
        &floor,
        filetime::FileTime::from_system_time(
            std::time::SystemTime::now() + std::time::Duration::from_secs(2),
        ),
    )
    .unwrap();
    crate::reader_floor::refresh_refused(dir.path()).unwrap();
    assert!(
        persisted_format::gate(PersistedStore::AftDb, &path, &path, Some(supported.into())).is_ok()
    );
    let conn = crate::db::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT version FROM schema_version", [], |row| row
            .get::<_, u32>(0))
            .unwrap(),
        supported
    );
}
