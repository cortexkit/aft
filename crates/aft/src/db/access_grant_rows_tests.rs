use super::*;
use crate::access_policy::AccessGrant;
use crate::config::Config;
use crate::db::access_grants::{self as store, register_agent, register_scope, register_worker};

fn key(name: &str, epoch: u64) -> ScopeKey {
    ScopeKey {
        owner: "owner".into(),
        scope_ref: name.into(),
        epoch,
    }
}

fn folder(path: &std::path::Path, operation: AccessOperation) -> AccessGrant {
    AccessGrant {
        path: path.to_path_buf(),
        kind: GrantKind::Subtree,
        operation,
    }
}

/// The folder `work/<name>` under the test's temporary directory, in the
/// form aft.db stores grant paths. Saving a grant refuses a relative path,
/// and a fixed Unix path such as `/work/x` is relative on Windows because it
/// names no drive.
fn work(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    crate::access_policy::policy_path(dir.path())
        .join("work")
        .join(name)
}

/// A head `head` (agent `agent-alf`) at epoch 7 and its worker `worker`.
fn fixture() -> (tempfile::TempDir, crate::db::TrackedConnection) {
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
    let config = Config::default();
    register_scope(&conn, &key("head", 7), None, &config).unwrap();
    register_agent(&conn, &key("head", 7), "agent-alf", true).unwrap();
    register_scope(&conn, &key("worker", 3), Some(&key("head", 7)), &config).unwrap();
    register_worker(&conn, &key("worker", 3)).unwrap();
    (dir, conn)
}

#[test]
fn grant_rows_name_agent_and_worker_holders() {
    let (dir, conn) = fixture();
    store::insert_from_card(
        &conn,
        &key("head", 7),
        &folder(&work(&dir, "x"), AccessOperation::Write),
        Some("acc_1"),
    )
    .unwrap();
    store::insert_from_card(
        &conn,
        &key("worker", 3),
        &folder(&work(&dir, "y"), AccessOperation::Read),
        Some("acc_2"),
    )
    .unwrap();
    let rows = live_rows(&conn).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let head = rows
        .iter()
        .find(|row| row.scope.scope_ref == "head")
        .unwrap();
    assert_eq!(
        head.holder,
        GrantHolder::Agent {
            agent_id: Some("agent-alf".into())
        }
    );
    assert_eq!(head.card_ref.as_deref(), Some("acc_1"));
    assert_eq!(head.operation, AccessOperation::Write);
    let worker = rows
        .iter()
        .find(|row| row.scope.scope_ref == "worker")
        .unwrap();
    assert_eq!(
        worker.holder,
        GrantHolder::Worker {
            agent_id: Some("agent-alf".into()),
            scope_ref: "worker".into(),
        }
    );
    assert!(head.grant_id.starts_with("aftg_") && head.grant_id.len() == 29);
    assert_ne!(head.grant_id, worker.grant_id);
}

#[test]
fn grant_rows_widening_keeps_the_id_and_refreshes_time_and_card() {
    let (dir, conn) = fixture();
    let head = key("head", 7);
    let x = work(&dir, "x");
    store::insert_from_card(
        &conn,
        &head,
        &folder(&x, AccessOperation::Read),
        Some("acc_1"),
    )
    .unwrap();
    conn.execute("UPDATE access_grants SET granted_at_ms=5", [])
        .unwrap();
    let before = live_rows(&conn).unwrap().remove(0);
    // Answering read again changes nothing about the grant.
    store::insert_from_card(
        &conn,
        &head,
        &folder(&x, AccessOperation::Read),
        Some("acc_9"),
    )
    .unwrap();
    assert_eq!(live_rows(&conn).unwrap(), vec![before.clone()]);
    store::insert_from_card(
        &conn,
        &head,
        &folder(&x, AccessOperation::Write),
        Some("acc_2"),
    )
    .unwrap();
    let rows = live_rows(&conn).unwrap();
    assert_eq!(rows.len(), 1, "one row per folder per holder: {rows:?}");
    assert_eq!(rows[0].grant_id, before.grant_id);
    assert_eq!(rows[0].operation, AccessOperation::Write);
    assert_eq!(rows[0].card_ref.as_deref(), Some("acc_2"));
    assert!(rows[0].granted_at_ms > 5);
}

#[test]
fn grant_rows_leave_out_ended_and_superseded_scopes() {
    let (dir, conn) = fixture();
    store::insert(
        &conn,
        &key("head", 7),
        &folder(&work(&dir, "x"), AccessOperation::Read),
    )
    .unwrap();
    store::insert(
        &conn,
        &key("worker", 3),
        &folder(&work(&dir, "y"), AccessOperation::Read),
    )
    .unwrap();
    let worker_id = live_rows(&conn)
        .unwrap()
        .into_iter()
        .find(|row| row.scope.scope_ref == "worker")
        .unwrap()
        .grant_id;
    // A revived head registers a newer epoch: the old epoch's grants no
    // longer authorize its calls and are no longer listed.
    register_scope(&conn, &key("head", 8), None, &Config::default()).unwrap();
    store::end_scope_grants(&conn, &key("worker", 3)).unwrap();
    assert!(live_rows(&conn).unwrap().is_empty());
    assert_eq!(revoke_live(&conn, &worker_id).unwrap(), None);
}

#[test]
fn grant_rows_revoke_deletes_once() {
    let (dir, conn) = fixture();
    let y = work(&dir, "y");
    store::insert(&conn, &key("worker", 3), &folder(&y, AccessOperation::Read)).unwrap();
    let id = live_rows(&conn).unwrap().remove(0).grant_id;
    let revoked = revoke_live(&conn, &id).unwrap().expect("live grant");
    assert_eq!(revoked.path, y);
    assert!(store::list(&conn, &key("worker", 3)).unwrap().is_empty());
    assert_eq!(revoke_live(&conn, &id).unwrap(), None);
    assert_eq!(revoke_live(&conn, "aftg_unknown").unwrap(), None);
}

#[test]
fn grant_rows_migration_backfills_and_reruns() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL PRIMARY KEY)")
        .unwrap();
    for version in 1..SCHEMA_VERSION {
        crate::db::apply_migration(&mut conn, version).unwrap();
    }
    conn.execute(
        "INSERT INTO access_grants(owner,scope_ref,epoch,path,kind,mode) VALUES ('o','s','1','/a','subtree','read')",
        [],
    )
    .unwrap();
    let before = now_ms();
    assert_eq!(
        crate::db::run_migrations_supported(&mut conn, SCHEMA_VERSION).unwrap(),
        SCHEMA_VERSION
    );
    let (id, at): (String, i64) = conn
        .query_row(
            "SELECT grant_id,granted_at_ms FROM access_grants",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(id.starts_with("aftg_"));
    assert!(at >= before, "a pre-27 grant shows the upgrade time");
    // A rollback that keeps the columns, then an older build writing a row,
    // then the upgrade again: the migration reruns and fills the new row.
    conn.execute("UPDATE schema_version SET version=26", [])
        .unwrap();
    conn.execute(
        "INSERT INTO access_grants(owner,scope_ref,epoch,path,kind,mode) VALUES ('o','s','1','/b','subtree','read')",
        [],
    )
    .unwrap();
    assert_eq!(
        crate::db::run_migrations_supported(&mut conn, SCHEMA_VERSION).unwrap(),
        SCHEMA_VERSION
    );
    let missing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM access_grants WHERE grant_id IS NULL OR granted_at_ms IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
    let unchanged: String = conn
        .query_row(
            "SELECT grant_id FROM access_grants WHERE path='/a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, id);
}

#[test]
fn grant_sources_schema28_preserves_rows_and_refuses_schema27_reader() {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::db::run_migrations_supported(&mut conn, 27).unwrap();
    conn.execute("INSERT INTO access_grants(owner,scope_ref,epoch,path,kind,mode,grant_id,granted_at_ms,card_ref) VALUES ('o','s','1','/a','subtree','read','preserved',123,'acc_old')", []).unwrap();
    assert_eq!(
        crate::db::run_migrations_supported(&mut conn, 28).unwrap(),
        28
    );
    let row = || {
        conn.query_row(
            "SELECT grant_id,granted_at_ms,card_ref,source FROM access_grants",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .unwrap()
    };
    assert_eq!(row(), ("preserved".into(), 123, "acc_old".into(), None));
    assert!(matches!(
        crate::db::run_migrations_supported(&mut conn, 27),
        Err(crate::db::OpenError::DowngradeRefused {
            db_version: 28,
            supported: 27
        })
    ));
    assert_eq!(
        crate::db::run_migrations_supported(&mut conn, 28).unwrap(),
        28,
        "a supported retry remains readable"
    );
    let row = conn
        .query_row(
            "SELECT grant_id,granted_at_ms,card_ref,source FROM access_grants",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("preserved".into(), 123, "acc_old".into(), None));
    conn.execute("UPDATE schema_version SET version=27", [])
        .unwrap();
    assert_eq!(
        crate::db::run_migrations_supported(&mut conn, 28).unwrap(),
        28,
        "migration tolerates an additive-column rollback"
    );
    assert!(conn
        .execute("UPDATE access_grants SET source='invented'", [])
        .is_err());
}

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
