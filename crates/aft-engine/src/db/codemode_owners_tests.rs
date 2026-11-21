use super::access_grants::ScopeKey;
use super::codemode_owners::{load, save, touch};
use rusqlite::Connection;

fn scope(scope_ref: &str, epoch: u64) -> ScopeKey {
    ScopeKey {
        owner: "prefrontal".into(),
        scope_ref: scope_ref.into(),
        epoch,
    }
}

#[test]
fn codemode_owners_install_on_open_and_allow_schema_29_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let fresh = super::open(&path).unwrap();
    assert_eq!(super::current_schema_version(&fresh).unwrap(), 29);
    // Model a database created before the optional table existed.
    fresh
        .execute_batch(
            "DROP TABLE codemode_owners;
        INSERT INTO host_state(key,value,updated_at) VALUES ('rollback','kept',0);",
        )
        .unwrap();
    drop(fresh);
    assert!(crate::reader_floor::prepare(dir.path()).is_empty());
    let floor_path = crate::reader_floor::floor_path(dir.path());
    let before = std::fs::read(&floor_path).unwrap();
    let upgraded = super::open(&path).unwrap();
    let scope = scope("head", 1);
    save(&upgraded, &scope, "runner", "identity", 1).unwrap();
    drop(upgraded);

    // The prior reader uses the same version check and knows nothing of the cache.
    let mut rollback = Connection::open(&path).unwrap();
    assert_eq!(super::peek_schema_version(&path), Some(29));
    assert_eq!(
        super::run_migrations_supported(&mut rollback, 29).unwrap(),
        29
    );
    let preserved: String = rollback
        .query_row(
            "SELECT value FROM host_state WHERE key='rollback'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(preserved, "kept");
    assert_eq!(load(&rollback, &scope, 1).unwrap(), ["identity"]);
    assert_eq!(std::fs::read(floor_path).unwrap(), before);
}

#[test]
fn codemode_owners_newer_scope_epoch_evicts_old_snapshots_without_reviving_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = super::open(&dir.path().join("aft.db")).unwrap();
    let old = scope("head", i64::MAX as u64);
    let new = scope("head", i64::MAX as u64 + 1);
    save(&db, &old, "runner-1", "old snap", 1).unwrap();
    save(&db, &new, "runner-2", "new snap", 2).unwrap();
    save(&db, &old, "runner-1", "late old snap", 3).unwrap();
    assert!(load(&db, &old, 4).unwrap().is_empty());
    assert_eq!(load(&db, &new, 4).unwrap(), ["new snap"]);
}

#[test]
fn codemode_owners_expire_after_seven_days_unused_and_cap_evicts_lru() {
    let dir = tempfile::tempdir().unwrap();
    let db = super::open(&dir.path().join("aft.db")).unwrap();
    let frequently_used = scope("recently-used", 1);
    let idle = scope("idle", 1);
    save(&db, &frequently_used, "runner", "frequently-used", 0).unwrap();
    save(&db, &idle, "runner", "idle", 0).unwrap();
    let week = 7 * 24 * 60 * 60 * 1_000;
    touch(&db, &frequently_used, week - 1).unwrap();
    assert!(load(&db, &idle, week).unwrap().is_empty());
    assert_eq!(
        load(&db, &frequently_used, week).unwrap(),
        ["frequently-used"]
    );

    let other = scope("other", 1);
    let oldest = scope("least-recently-used", 1);
    save(&db, &oldest, "runner", "oldest", week - 2).unwrap();
    for i in 0..4095 {
        save(&db, &other, &i.to_string(), "other", week).unwrap();
    }
    assert!(load(&db, &oldest, week).unwrap().is_empty());
    assert_eq!(
        load(&db, &frequently_used, week).unwrap(),
        ["frequently-used"]
    );
    assert_eq!(load(&db, &other, week).unwrap().len(), 4095);
}
