//! Durable catalog routing state. All operations use the owning aft.db handle.
//!
//! Routing settings saved by a worker's tool.catalog fetch are session
//! configuration, not disposable cache entries. Only the daemon can establish
//! that the worker's scope has ended; idle time, a missing worktree, and row
//! count are not evidence of that. Delete settings only for an ended scope.
use super::FrozenParams;
use crate::db::remote_exec::{self as rows, PolicyRow};
pub use crate::db::remote_exec::{forget_scope, PolicyKey};
#[cfg(test)]
use rusqlite::params;
use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Save `policy` as `key`'s catalog params. When the saved row will carry the
/// root confinement fence (asked for now, or carried from an older epoch of
/// the same worker), `before_confining_commit` runs inside the transaction
/// just before it commits, so a caller can mark the key confined in memory
/// before any reader can see the fence in aft.db. If the commit then fails,
/// the key stays marked confined, which only restricts the worker.
pub fn freeze(
    conn: &Connection,
    key: &PolicyKey,
    policy: &FrozenParams,
    now: u64,
    before_confining_commit: impl FnOnce(),
) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let prior = rows::prior(&tx, key)?;
    let mut policy = policy.clone();
    if let Some(prior) = prior {
        let prior: FrozenParams = serde_json::from_str(&prior.params).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        // Merge the fence before inserting an exact-epoch row: lookup prefers
        // that row and can no longer carry confinement from the previous epoch.
        policy.confine = policy.confine.or(prior.confine);
    }
    rows::insert(
        &tx,
        key,
        &PolicyRow {
            params: serde_json::to_string(&policy).expect("routing fields serialize"),
        },
        now,
    )?;
    // Confinement is monotonic: a later catalog may tighten an existing plan,
    // but a parameterless fetch or a routing override cannot remove the fence.
    if policy.confine.is_some() {
        rows::confine(&tx, key)?;
        before_confining_commit();
    }
    tx.commit()
}

/// The only per-call policy lookup: no files or call arguments can supply it.
pub fn lookup(
    conn: &Connection,
    key: &PolicyKey,
    now: u64,
) -> rusqlite::Result<Option<FrozenParams>> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let policy = rows::lookup(&tx, key, now)?
        .map(|row| {
            let json = row.params;
            serde_json::from_str(&json).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        })
        .transpose()?;
    tx.commit()?;
    Ok(policy)
}

/// Whether the saved params for `key` carry the root confinement fence.
///
/// Tool calls ask this on the subc frame loop, so it only reads: no write
/// transaction, no carried-forward row, no `last_used` update. It returns the
/// same answer [`lookup`] would: the exact-epoch row when present, otherwise
/// the newest older epoch's row, which is the row `lookup` would copy forward.
/// Saved params that do not parse are an error, so callers can refuse the call
/// instead of treating an unreadable fence as absent.
pub fn confined(conn: &Connection, key: &PolicyKey) -> rusqlite::Result<bool> {
    let row = rows::current(conn, key)?;
    row.map(|row| {
        serde_json::from_str::<FrozenParams>(&row.params)
            .map(|params| params.confine.is_some())
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
    })
    .transpose()
    .map(|confined| confined.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_policy_row_preserves_existing_json_bytes() {
        const SAVED: &str =
            r#"{"confine":"root","remote_exec":null,"siblings":["/sibling"],"host":"runner"}"#;
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let original = key(dir.path());
        rows::insert(
            &conn,
            &original,
            &PolicyRow {
                params: SAVED.into(),
            },
            100,
        )
        .unwrap();
        let policy = lookup(&conn, &original, 200).unwrap().unwrap();
        assert!(policy.confine.is_some());
        let mut rewritten = original.clone();
        rewritten.session = "rewritten".into();
        freeze(&conn, &rewritten, &policy, 300, || {}).unwrap();
        let saved = rows::current(&conn, &rewritten).unwrap().unwrap();
        assert_eq!(saved.params.as_bytes(), SAVED.as_bytes());
    }

    fn key(root: &std::path::Path) -> PolicyKey {
        PolicyKey {
            project_root: root.display().to_string(),
            harness: "broca".into(),
            session: "one".into(),
            principal: "reserved:broca".into(),
            owner: "prefrontal-core".into(),
            scope_ref: "work".into(),
            epoch: 7,
            preset: "worker".into(),
        }
    }
    fn policy(enabled: bool) -> FrozenParams {
        FrozenParams {
            remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                enabled,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    #[test]
    fn exec_remote_catalog_policy_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let key = key(dir.path());
        {
            let conn = crate::db::open(&path).unwrap();
            freeze(&conn, &key, &policy(true), 100, || {}).unwrap();
            assert!(
                lookup(&conn, &key, 100).unwrap().is_some(),
                "the first process routed before restart"
            );
        }
        let conn = crate::db::open(&path).unwrap();
        assert!(
            lookup(&conn, &key, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        forget_scope(&conn, &key.owner, &key.scope_ref, key.epoch).unwrap();
        assert!(lookup(&conn, &key, 200).unwrap().is_none());
    }
    #[test]
    fn exec_remote_policy_key_isolates_lower_epoch_principal_and_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = key(dir.path());
        freeze(&conn, &first, &policy(true), 100, || {}).unwrap();
        let mut second = first.clone();
        second.session = "two".into();
        freeze(&conn, &second, &policy(false), 100, || {}).unwrap();
        assert!(
            lookup(&conn, &first, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        assert!(
            !lookup(&conn, &second, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        for different in 0..7 {
            let mut other = first.clone();
            match different {
                0 => other.epoch -= 1,
                1 => other.principal = "direct".into(),
                2 => other.owner = "other".into(),
                3 => other.scope_ref = "other".into(),
                4 => other.harness = "other".into(),
                5 => other.project_root = "/other".into(),
                _ => other.preset = "head".into(),
            }
            assert!(
                lookup(&conn, &other, 200).unwrap().is_none(),
                "key dimension {different}"
            );
        }
    }

    #[test]
    fn exec_remote_revived_worker_carries_params_and_then_hits_exact_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let mut first = key(dir.path());
        first.epoch = 1;
        let frozen = FrozenParams {
            siblings: vec!["/sibling".into()],
            host: Some("runner".into()),
            ..policy(true)
        };
        freeze(&conn, &first, &frozen, 100, || {}).unwrap();
        let mut revived = first.clone();
        revived.epoch = 2;
        let carried = lookup(&conn, &revived, 200).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(carried).unwrap(),
            serde_json::to_value(&frozen).unwrap()
        );
        let (json, last_used): (String, i64) = conn
            .query_row(
                "SELECT params,last_used FROM remote_exec_policies WHERE epoch='2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(json, serde_json::to_string(&frozen).unwrap());
        assert_eq!(last_used, 200);
        forget_scope(&conn, &first.owner, &first.scope_ref, first.epoch).unwrap();
        let exact = lookup(&conn, &revived, 300).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(exact).unwrap(),
            serde_json::to_value(frozen).unwrap()
        );
        let (count, last_used): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*),last_used FROM remote_exec_policies",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((count, last_used), (1, 300));
    }

    #[test]
    fn exec_remote_revived_worker_uses_highest_older_epoch_only() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let mut first = key(dir.path());
        first.epoch = 1;
        freeze(&conn, &first, &policy(true), 100, || {}).unwrap();
        let mut second = first.clone();
        second.epoch = 2;
        freeze(&conn, &second, &policy(false), 100, || {}).unwrap();
        let mut third = first.clone();
        third.epoch = 3;
        assert!(
            !lookup(&conn, &third, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        for different in 0..8 {
            let mut other = third.clone();
            match different {
                0 => other.epoch = 0,
                1 => other.principal = "direct".into(),
                2 => other.owner = "other".into(),
                3 => other.scope_ref = "other".into(),
                4 => other.harness = "other".into(),
                5 => other.project_root = "/other".into(),
                6 => other.preset = "head".into(),
                _ => other.session = "two".into(),
            }
            assert!(
                lookup(&conn, &other, 200).unwrap().is_none(),
                "key dimension {different}"
            );
        }
        let mut newer = first.clone();
        newer.epoch = u64::MAX - 1;
        freeze(&conn, &newer, &policy(true), 100, || {}).unwrap();
        newer.epoch = u64::MAX;
        assert!(
            lookup(&conn, &newer, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn confined_freeze_carries_only_fence_across_epochs_and_preserves_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let mut first = key(dir.path());
        first.epoch = 9;
        freeze(
            &conn,
            &first,
            &FrozenParams {
                confine: Some(crate::exec_remote::RootConfinement::Root),
                siblings: vec!["/old-sibling".into()],
                ..policy(false)
            },
            100,
            || {},
        )
        .unwrap();
        for different in 0..7 {
            let mut other = first.clone();
            other.epoch = 10;
            match different {
                0 => other.principal = "direct".into(),
                1 => other.owner = "other".into(),
                2 => other.scope_ref = "other".into(),
                3 => other.harness = "other".into(),
                4 => other.project_root = "/other".into(),
                5 => other.preset = "head".into(),
                _ => other.session = "two".into(),
            }
            freeze(&conn, &other, &policy(true), 200, || {}).unwrap();
            assert!(lookup(&conn, &other, 200)
                .unwrap()
                .unwrap()
                .confine
                .is_none());
        }
        for epoch in [10, u64::MAX - 1, u64::MAX] {
            let mut rebound = first.clone();
            rebound.epoch = epoch;
            freeze(&conn, &rebound, &policy(true), 300, || {}).unwrap();
            // Inspect the inserted row before lookup can copy an older policy.
            let v = rebound.values();
            let json: String = conn
                .query_row(
                    "SELECT params FROM remote_exec_policies
                     WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
                     AND owner=?5 AND scope_ref=?6 AND preset=?7 AND epoch=?8",
                    params![v[0], v[1], v[2], v[3], v[4], v[5], v[6], epoch.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            let saved: FrozenParams = serde_json::from_str(&json).unwrap();
            assert!(saved.confine.is_some(), "epoch {epoch}: {json}");
            assert!(saved.remote_exec.unwrap().enabled);
            assert!(saved.siblings.is_empty());
        }
    }

    #[test]
    fn exec_remote_migration_from_previous_version_preserves_existing_data() {
        for previous in [12, 13] {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            conn.execute_batch("DROP TABLE remote_exec_policies; CREATE TABLE unrelated (v TEXT); INSERT INTO unrelated VALUES ('keep');").unwrap();
            conn.execute("UPDATE schema_version SET version=?1", [previous])
                .unwrap();
            assert_eq!(
                crate::db::run_migrations(&mut conn).unwrap(),
                crate::db::CURRENT_SCHEMA_VERSION
            );
            assert_eq!(
                conn.query_row("SELECT v FROM unrelated", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "keep"
            );
            let dir = tempfile::tempdir().unwrap();
            let key = key(dir.path());
            freeze(&conn, &key, &policy(true), 100, || {}).unwrap();
            assert!(lookup(&conn, &key, 200).unwrap().is_some());
        }
    }
}
