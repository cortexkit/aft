#![cfg_attr(test, allow(hidden_glob_reexports))]
pub use aft_db::db::*;

#[cfg(test)]
pub(crate) use display_records::test_support::{current_schema_version, run_migrations_supported};
#[cfg(test)]
mod display_records_tests;

#[cfg(test)]
#[allow(unused_imports)]
pub use crate::live_db_files as file_identity;
#[cfg(test)]
#[allow(unused_imports)]
pub use lifecycle::{
    connection_snapshot, SqliteConnectionSnapshot, SqliteStore, SqliteStoreCount, TrackedConnection,
};
#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;

#[cfg(test)]
mod tests {
    use aft_db::db::tests::*;

    use super::*;

    use tempfile::tempdir;

    #[test]
    fn migration_v21_opens_v20_with_default_first_match_and_new_reader_floor() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        for version in 1..=20 {
            apply_migration_statements(&conn, version).unwrap();
        }
        conn.execute("INSERT INTO schema_version(version) VALUES (20)", [])
            .unwrap();
        drop(conn);
        crate::reader_floor::raise(
            dir.path(),
            &[(crate::persisted_format::PersistedStore::AftDb, 20)],
        )
        .unwrap();
        assert!(crate::reader_floor::prepare(dir.path()).is_empty());
        let conn = open(&path).unwrap();
        // Opening migrates through every later version too, so the result is
        // the current schema; the v21 columns below must still be present.
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        let columns = conn
            .prepare("PRAGMA table_info(bash_pattern_watches)")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(columns.contains(&("each_line".into(), "INTEGER".into(), true, Some("0".into()))));
        assert!(columns.contains(&("match_batch".into(), "TEXT".into(), false, None)));
        let result_source_type: String = conn.query_row(
            "SELECT type FROM pragma_table_info('call_ledger') WHERE name = 'tool_result_source'",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(result_source_type, "BLOB");
        assert_eq!(
            crate::reader_floor::read(dir.path())
                .unwrap()
                .unwrap()
                .get(crate::persisted_format::PersistedStore::AftDb),
            Some(u64::from(CURRENT_SCHEMA_VERSION))
        );
    }

    #[test]
    fn additive_task_owner_index_keeps_v18_schema_and_reader_floor() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        for version in 1..=18 {
            apply_migration_statements(&conn, version).unwrap();
        }
        conn.execute("INSERT INTO schema_version(version) VALUES (18)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO host_state(key, value, updated_at) VALUES ('compatibility', 'v18', 0)",
            [],
        )
        .unwrap();
        let table_schema = |conn: &Connection| {
            conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let v18_tables = table_schema(&conn);
        assert!(!sqlite_names(&conn, "index").contains(&"idx_bash_tasks_task_owner".to_string()));
        drop(conn);

        // Today's startup baseline includes newer table formats; this fixture
        // intentionally models only the optional index on the historical schema.
        std::fs::write(
            crate::reader_floor::floor_path(dir.path()),
            br#"{"floor_schema":1,"stores":{"aft.db":18}}"#,
        )
        .unwrap();
        let floor_before = std::fs::read(crate::reader_floor::floor_path(dir.path())).unwrap();
        // Isolate the optional index at its historical table version; writable
        // opens now also perform later table-format migrations.
        let conn = Connection::open(&path).unwrap();
        ensure_additive_indexes(&conn).unwrap();
        assert_eq!(schema_version(&conn), 18);
        assert_eq!(
            crate::reader_floor::read(dir.path())
                .unwrap()
                .unwrap()
                .get(crate::persisted_format::PersistedStore::AftDb),
            Some(18)
        );
        assert_eq!(
            std::fs::read(crate::reader_floor::floor_path(dir.path())).unwrap(),
            floor_before
        );
        assert!(sqlite_names(&conn, "index").contains(&"idx_bash_tasks_task_owner".to_string()));
        assert_eq!(table_schema(&conn), v18_tables);
        assert_eq!(
            conn.query_row(
                "SELECT value FROM host_state WHERE key = 'compatibility'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "v18"
        );
        drop(conn);

        // A v18 reader accepts these version gates and can still query every
        // table it knew before the optional performance index was installed.
        let legacy = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(schema_version(&legacy), 18);
        assert_eq!(table_schema(&legacy), v18_tables);
        for (table, _) in v18_tables {
            let mut statement = legacy
                .prepare(&format!("SELECT * FROM \"{table}\" LIMIT 0"))
                .unwrap();
            assert!(statement.query([]).unwrap().next().unwrap().is_none());
        }
    }

    #[test]
    fn result_source_migration_preserves_legacy_frames_and_sets_reader_floor() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = Connection::open(&path).unwrap();
        for version in 1..=18 {
            apply_migration_statements(&conn, version).unwrap();
        }
        conn.execute("INSERT INTO schema_version(version) VALUES (18)", [])
            .unwrap();
        let legacy = br#"{"content":[{"type":"text","text":"legacy"}],"isError":true}"#;
        conn.execute("INSERT INTO call_ledger(carrier,call_key,digest,state,event_id,frame_type,frame_body) VALUES ('carrier','key','digest','Settled','event','Response',?1)", [legacy.as_slice()]).unwrap();
        drop(conn);
        assert!(crate::reader_floor::prepare(dir.path()).is_empty());
        let conn = open(&path).unwrap();
        assert_eq!(schema_version(&conn), CURRENT_SCHEMA_VERSION);
        let row = call_ledger::get_for_replay(
            &conn,
            &call_ledger::Key {
                carrier: "carrier".into(),
                call_key: "key".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(row.frame.unwrap().body, legacy);
        assert!(row.tool_result_source.is_none());
        assert_eq!(
            crate::reader_floor::read(dir.path())
                .unwrap()
                .unwrap()
                .get(crate::persisted_format::PersistedStore::AftDb),
            Some(u64::from(CURRENT_SCHEMA_VERSION))
        );
    }
}
#[cfg(test)]
pub mod compression_events;

#[cfg(test)]
pub mod call_ledger;

#[cfg(test)]
pub mod runner_harness_keys;

#[cfg(test)]
pub mod access_grant_rows;

#[cfg(test)]
pub mod rich_ui;

#[cfg(test)]
pub mod bash_tasks;
