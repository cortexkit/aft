//! Cursor-signing keys and records of which worker/reader catalogs a caller
//! fetched for an exact daemon-stamped session and scope, stored in aft.db.

use rusqlite::{params, Connection};

pub use super::remote_exec::PolicyKey as PresetKey;

pub const SCHEMA_VERSION: u32 = 20;

// Historical migration SQL is immutable. The legacy display tables remain
// for rollback compatibility, not as a storage backend for current views.
pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS bash_view_records (
  harness TEXT NOT NULL, session_id TEXT NOT NULL, task_id TEXT NOT NULL,
  record TEXT NOT NULL,
  row_template_digest TEXT NOT NULL, page_template_digest TEXT NOT NULL,
  PRIMARY KEY (harness, session_id, task_id),
  FOREIGN KEY (harness, session_id, task_id)
    REFERENCES bash_tasks(harness, session_id, task_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS rich_ui_templates (
  digest TEXT NOT NULL PRIMARY KEY, bytes BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS rich_ui_cursor_key (
  id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
  key BLOB NOT NULL CHECK (length(key) = 32)
);
INSERT OR IGNORE INTO rich_ui_cursor_key(id, key) VALUES (1, randomblob(32));
CREATE TABLE IF NOT EXISTS rich_ui_session_presets (
  project_root TEXT NOT NULL, harness TEXT NOT NULL, session TEXT NOT NULL,
  principal TEXT NOT NULL, owner TEXT NOT NULL, scope_ref TEXT NOT NULL,
  epoch TEXT NOT NULL, preset TEXT NOT NULL CHECK (preset IN ('worker', 'reader')),
  recorded_at INTEGER NOT NULL,
  PRIMARY KEY (project_root, harness, session, principal, owner, scope_ref, epoch, preset)
);
CREATE INDEX IF NOT EXISTS idx_rich_ui_session_presets_recorded
  ON rich_ui_session_presets(recorded_at);
"#;

/// Each successful fetch refreshes only its exact identity, epoch and preset.
/// Unlike routing policies, attestations never carry forward from an older epoch.
pub fn record_preset(conn: &Connection, key: &PresetKey, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO rich_ui_session_presets
         (project_root,harness,session,principal,owner,scope_ref,epoch,preset,recorded_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(project_root,harness,session,principal,owner,scope_ref,epoch,preset)
         DO UPDATE SET recorded_at=excluded.recorded_at",
        params![
            key.project_root,
            key.harness,
            key.session,
            key.principal,
            key.owner,
            key.scope_ref,
            key.epoch.to_string(),
            key.preset,
            now_ms
        ],
    )?;
    Ok(())
}

/// Read all presets for the exact route and scope without refreshing retention.
/// The key's preset is ignored so a worker/reader disagreement remains visible.
pub fn session_presets(conn: &Connection, key: &PresetKey) -> rusqlite::Result<Vec<String>> {
    conn.prepare(
        "SELECT preset FROM rich_ui_session_presets
         WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
           AND owner=?5 AND scope_ref=?6 AND epoch=?7 ORDER BY preset",
    )?
    .query_map(
        params![
            key.project_root,
            key.harness,
            key.session,
            key.principal,
            key.owner,
            key.scope_ref,
            key.epoch.to_string()
        ],
        |row| row.get(0),
    )?
    .collect()
}

pub(crate) fn prune_presets(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
    conn.execute(
        "DELETE FROM rich_ui_session_presets WHERE recorded_at < ?1",
        [now_ms.saturating_sub(RETENTION_MS)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn assert_tables(conn: &Connection) {
        for table in [
            "bash_view_records",
            "rich_ui_templates",
            "rich_ui_cursor_key",
            "rich_ui_session_presets",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing {table}");
        }
    }

    #[test]
    fn rich_ui_migration_fresh_database_has_four_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        assert_tables(&conn);
        assert_eq!(cursor_key(&conn).len(), 32);
    }

    fn cursor_key(conn: &Connection) -> Vec<u8> {
        conn.query_row("SELECT key FROM rich_ui_cursor_key WHERE id=1", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn rich_ui_migration_reapplication_keeps_cursor_key() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        let key = cursor_key(&conn);
        conn.execute_batch(MIGRATION).unwrap();
        assert_eq!(cursor_key(&conn), key);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM rich_ui_cursor_key", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn rich_ui_migration_upgrade_reopen_and_downgrade_fence() {
        use super::super::{run_migrations, run_migrations_supported, CURRENT_SCHEMA_VERSION};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let previous = CURRENT_SCHEMA_VERSION - 1;
        let mut conn = Connection::open(&path).unwrap();
        run_migrations_supported(&mut conn, previous).unwrap();
        conn.execute(
            "INSERT INTO host_state(key,value,updated_at) VALUES ('upgrade-canary','preserved',1)",
            [],
        )
        .unwrap();
        assert_eq!(run_migrations(&mut conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_tables(&conn);
        let key = cursor_key(&conn);
        assert_eq!(key.len(), 32);
        assert_ne!(key, vec![0; 32]);
        drop(conn);

        let mut conn = super::super::open(&path).unwrap();
        let changes = conn.total_changes();
        assert_eq!(run_migrations(&mut conn).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(
            conn.total_changes(),
            changes,
            "current-version open must not write"
        );
        assert_eq!(cursor_key(&conn), key);
        let canary: String = conn
            .query_row(
                "SELECT value FROM host_state WHERE key='upgrade-canary'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canary, "preserved");
        assert!(matches!(
            run_migrations_supported(&mut conn, previous),
            Err(super::super::OpenError::DowngradeRefused { db_version, supported })
                if db_version == CURRENT_SCHEMA_VERSION && supported == previous
        ));
        let mut fresh = Connection::open_in_memory().unwrap();
        run_migrations(&mut fresh).unwrap();
        assert_ne!(
            cursor_key(&fresh),
            key,
            "databases must not share a fixed key"
        );
    }

    #[test]
    fn rich_ui_migration_upgrades_schema_18_without_losing_ledger_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations_supported(&mut conn, 18).unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        assert_tables(&conn);
        conn.prepare("SELECT tool_result_source FROM call_ledger")
            .unwrap();
    }

    fn key(epoch: u64) -> PresetKey {
        PresetKey {
            project_root: "/project".into(),
            harness: "Raw Harness".into(),
            session: "session".into(),
            principal: "reserved:broca".into(),
            owner: r#"{"kind":"reserved","module_id":"prefrontal-core"}"#.into(),
            scope_ref: "worker".into(),
            epoch,
            preset: "worker".into(),
        }
    }

    #[test]
    fn rich_ui_presets_epochs_are_exact_text_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = super::super::open(&path).unwrap();
        let epochs = [i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX];
        for epoch in epochs {
            record_preset(&conn, &key(epoch), 1).unwrap();
        }
        drop(conn);
        let conn = super::super::open(&path).unwrap();
        for epoch in epochs {
            let k = key(epoch);
            let stored: (String, String) = conn
                .query_row(
                    "SELECT epoch,typeof(epoch) FROM rich_ui_session_presets WHERE epoch=?1",
                    [epoch.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(stored, (epoch.to_string(), "text".into()));
            assert_eq!(session_presets(&conn, &k).unwrap(), ["worker"]);
            // Other requested boundary epochs are also present. Remove them
            // temporarily to prove each boundary misses both of its neighbours.
            for other in epochs.into_iter().filter(|&other| other != epoch) {
                conn.execute(
                    "DELETE FROM rich_ui_session_presets WHERE epoch=?1",
                    [other.to_string()],
                )
                .unwrap();
            }
            for neighbour in [epoch.checked_sub(1), epoch.checked_add(1)]
                .into_iter()
                .flatten()
            {
                assert!(session_presets(&conn, &key(neighbour)).unwrap().is_empty());
            }
            for other in epochs.into_iter().filter(|&other| other != epoch) {
                record_preset(&conn, &key(other), 1).unwrap();
            }
        }
    }

    #[test]
    fn rich_ui_presets_exact_identity_and_distinct_presets_are_read_only() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        let k = key(7);
        record_preset(&conn, &k, 10).unwrap();
        record_preset(
            &conn,
            &PresetKey {
                preset: "reader".into(),
                ..k.clone()
            },
            20,
        )
        .unwrap();
        let changes = conn.total_changes();
        assert_eq!(session_presets(&conn, &k).unwrap(), ["reader", "worker"]);
        for field in 0..7 {
            let mut wrong = k.clone();
            match field {
                0 => wrong.project_root.push('x'),
                1 => wrong.harness.push('x'),
                2 => wrong.session.push('x'),
                3 => wrong.principal.push('x'),
                4 => wrong.owner.push('x'),
                5 => wrong.scope_ref.push('x'),
                _ => wrong.epoch += 1,
            }
            assert!(
                session_presets(&conn, &wrong).unwrap().is_empty(),
                "field {field}"
            );
        }
        assert_eq!(conn.total_changes(), changes);
    }

    #[test]
    fn rich_ui_presets_retention_tick_removes_only_older_than_thirty_days() {
        let mut conn = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&mut conn).unwrap();
        let now = 40 * 24 * 60 * 60 * 1000;
        let cutoff = now - 30 * 24 * 60 * 60 * 1000;
        record_preset(&conn, &key(1), cutoff - 1).unwrap();
        record_preset(&conn, &key(2), cutoff).unwrap();
        record_preset(&conn, &key(3), now).unwrap();
        crate::bash_background::persistence::retention::prune_retention_tick(&mut conn, now)
            .unwrap();
        assert!(session_presets(&conn, &key(1)).unwrap().is_empty());
        assert_eq!(session_presets(&conn, &key(2)).unwrap(), ["worker"]);
        assert_eq!(session_presets(&conn, &key(3)).unwrap(), ["worker"]);
    }
}
