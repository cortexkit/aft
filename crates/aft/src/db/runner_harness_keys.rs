pub use aft_db::db::runner_harness_keys::*;

#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

#[cfg(test)]
mod tests {
    use aft_db::db::runner_harness_keys::tests::*;

    use super::*;
    use crate::exec_remote::persistence::PolicyKey;

    fn key(harness: &str) -> PolicyKey {
        PolicyKey {
            project_root: "/work".into(),
            harness: harness.into(),
            session: "session".into(),
            principal: "reserved:broca".into(),
            owner: "reserved:prefrontal-core".into(),
            scope_ref: "worker-7".into(),
            epoch: 3,
            preset: "worker".into(),
        }
    }

    fn seed(conn: &Connection, harness: &str, host: &str, hashline: bool, timestamp: u64) {
        let frozen = crate::exec_remote::FrozenParams {
            host: Some(host.into()),
            confine: Some(crate::exec_remote::RootConfinement::Root),
            ..Default::default()
        };
        crate::exec_remote::persistence::freeze(conn, &key(harness), &frozen, timestamp, || {})
            .unwrap();
        crate::db::catalog_edit_modes::record(conn, &key(harness), hashline, timestamp as i64)
            .unwrap();
    }

    fn assert_read_only_fallback(conn: &Connection, host: &str, hashline: bool) {
        let changes = conn.total_changes();
        conn.pragma_update(None, "query_only", true).unwrap();
        for epoch in [3, 4] {
            let mut canonical = key("runner");
            canonical.epoch = epoch;
            assert_eq!(
                crate::exec_remote::persistence::lookup(conn, &canonical, 100)
                    .unwrap()
                    .unwrap()
                    .host
                    .as_deref(),
                Some(host)
            );
            assert!(crate::exec_remote::persistence::confined(conn, &canonical).unwrap());
            assert_eq!(
                crate::db::catalog_edit_modes::lookup(conn, &canonical).unwrap(),
                Some(hashline)
            );
        }
        let mut foreign = key("runner");
        foreign.principal = "reserved:another-runner".into();
        assert!(crate::exec_remote::persistence::lookup(conn, &foreign, 100)
            .unwrap()
            .is_none());
        assert_eq!(
            crate::db::catalog_edit_modes::lookup(conn, &foreign).unwrap(),
            None
        );
        assert_eq!(conn.total_changes(), changes);
        conn.pragma_update(None, "query_only", false).unwrap();
    }

    #[test]
    fn runner_harness_copy_overflow_does_not_fail_startup_or_raw_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        seed(&conn, "opencode2", "old-host", true, 17);
        conn.execute("WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x+1 FROM n WHERE x<?1)
            INSERT INTO catalog_edit_modes (project_root,harness,session,principal,owner,scope_ref,epoch,preset,hashline,recorded_at)
            SELECT '/work','claude-code','overflow-' || x,'reserved:broca','reserved:prefrontal-core','worker-7','3','worker',0,17 FROM n", [SOURCE_LIMIT as i64]).unwrap();
        drop(conn);
        let conn = crate::db::open(&path).expect("opening a database must not copy aliases");
        assert_eq!(canonical_rows(&conn), 0);
        initialize(&conn);
        assert!(
            conn.is_autocommit(),
            "a failed copy must roll back its transaction"
        );
        assert_eq!(
            canonical_rows(&conn),
            0,
            "the earlier table's copy must also roll back"
        );
        assert_read_only_fallback(&conn, "old-host", true);
    }

    #[test]
    fn runner_harness_copy_locked_database_does_not_fail_startup_or_raw_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        seed(&conn, "claude-code", "old-host", false, 17);
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        initialize(&conn);
        assert!(conn.is_autocommit());
        assert_eq!(canonical_rows(&conn), 0);
        assert_read_only_fallback(&conn, "old-host", false);
        writer.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn runner_harness_ordinary_database_opens_do_not_copy_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let conn = crate::db::open(&path).unwrap();
        seed(&conn, "opencode2", "old-host", true, 17);
        drop(conn);
        let conn = crate::db::open(&path).unwrap();
        assert_eq!(canonical_rows(&conn), 0);
        assert_read_only_fallback(&conn, "old-host", true);
    }

    #[test]
    fn runner_harness_alias_copy_matches_read_only_fallback_precedence() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        seed(&conn, "opencode2", "first", false, 17);
        seed(&conn, "claude-code", "second", true, 18);
        seed(&conn, "broca", "third", false, 18);
        assert_eq!(copy_aliases(&conn).unwrap(), 2);
        assert_eq!(
            crate::exec_remote::persistence::lookup(&conn, &key("runner"), 100)
                .unwrap()
                .unwrap()
                .host
                .as_deref(),
            Some("second")
        );
        assert_eq!(
            crate::db::catalog_edit_modes::lookup(&conn, &key("runner")).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn runner_harness_new_catalog_keeps_preserved_alias_confinement_when_copy_was_skipped() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        seed(&conn, "opencode2", "old-host", true, 17);
        let mut canonical = key("runner");
        canonical.epoch = 4;
        let mut confined_before_commit = false;
        crate::exec_remote::persistence::freeze(&conn, &canonical, &Default::default(), 18, || {
            confined_before_commit = true
        })
        .unwrap();
        assert!(confined_before_commit);
        assert!(crate::exec_remote::persistence::confined(&conn, &canonical).unwrap());
    }

    #[test]
    fn runner_harness_alias_fallback_uses_recency_then_fixed_order_and_canonical_wins() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        seed(&conn, "opencode2", "first", false, 17);
        seed(&conn, "claude-code", "second", true, 18);
        seed(&conn, "broca", "third", false, 18);
        assert_read_only_fallback(&conn, "second", true);
        seed(&conn, "runner", "canonical", false, 1);
        assert_eq!(
            crate::exec_remote::persistence::lookup(&conn, &key("runner"), 100)
                .unwrap()
                .unwrap()
                .host
                .as_deref(),
            Some("canonical")
        );
        assert_eq!(
            crate::db::catalog_edit_modes::lookup(&conn, &key("runner")).unwrap(),
            Some(false)
        );
        copy_aliases(&conn).unwrap();
        assert_eq!(
            crate::exec_remote::persistence::lookup(&conn, &key("runner"), 100)
                .unwrap()
                .unwrap()
                .host
                .as_deref(),
            Some("canonical")
        );
    }

    #[test]
    fn runner_harness_value_copy_preserves_original_rows_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::run_migrations(&mut conn).unwrap();
        let frozen = crate::exec_remote::FrozenParams {
            host: Some("broca".into()),
            siblings: vec!["/sibling".into()],
            ..Default::default()
        };
        for (index, alias) in ["opencode2", "claude-code", "broca"]
            .into_iter()
            .enumerate()
        {
            let session = format!("session-{index}");
            let key = PolicyKey {
                project_root: "/work".into(),
                harness: alias.into(),
                session,
                principal: "reserved:broca".into(),
                owner: "reserved:prefrontal-core".into(),
                scope_ref: "worker-7".into(),
                epoch: 3,
                preset: "worker".into(),
            };
            crate::exec_remote::persistence::freeze(&conn, &key, &frozen, 17, || {}).unwrap();
            crate::db::catalog_edit_modes::record(&conn, &key, true, 17).unwrap();
        }
        assert_eq!(copy_aliases(&conn).unwrap(), 6);
        assert_eq!(copy_aliases(&conn).unwrap(), 0);
        for (index, alias) in ["opencode2", "claude-code", "broca"]
            .into_iter()
            .enumerate()
        {
            let key = PolicyKey {
                project_root: "/work".into(),
                harness: "runner".into(),
                session: format!("session-{index}"),
                principal: "reserved:broca".into(),
                owner: "reserved:prefrontal-core".into(),
                scope_ref: "worker-7".into(),
                epoch: 3,
                preset: "worker".into(),
            };
            assert_eq!(
                crate::db::catalog_edit_modes::lookup(&conn, &key).unwrap(),
                Some(true)
            );
            let params = crate::exec_remote::persistence::lookup(&conn, &key, 18)
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::to_value(params).unwrap(),
                serde_json::to_value(&frozen).unwrap()
            );
            for table in ["catalog_edit_modes", "remote_exec_policies"] {
                let raw: i64 = conn
                    .query_row(
                        &format!("SELECT count(*) FROM {table} WHERE harness=?1 AND session=?2"),
                        rusqlite::params![alias, key.session],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(raw, 1);
            }
        }
    }
}
