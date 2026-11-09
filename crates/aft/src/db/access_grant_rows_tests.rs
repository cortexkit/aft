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

fn folder(path: &str, operation: AccessOperation) -> AccessGrant {
    AccessGrant {
        path: PathBuf::from(path),
        kind: GrantKind::Subtree,
        operation,
    }
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
    let (_dir, conn) = fixture();
    store::insert_from_card(
        &conn,
        &key("head", 7),
        &folder("/work/x", AccessOperation::Write),
        Some("acc_1"),
    )
    .unwrap();
    store::insert_from_card(
        &conn,
        &key("worker", 3),
        &folder("/work/y", AccessOperation::Read),
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
    let (_dir, conn) = fixture();
    let head = key("head", 7);
    store::insert_from_card(
        &conn,
        &head,
        &folder("/work/x", AccessOperation::Read),
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
        &folder("/work/x", AccessOperation::Read),
        Some("acc_9"),
    )
    .unwrap();
    assert_eq!(live_rows(&conn).unwrap(), vec![before.clone()]);
    store::insert_from_card(
        &conn,
        &head,
        &folder("/work/x", AccessOperation::Write),
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
    let (_dir, conn) = fixture();
    store::insert(
        &conn,
        &key("head", 7),
        &folder("/work/x", AccessOperation::Read),
    )
    .unwrap();
    store::insert(
        &conn,
        &key("worker", 3),
        &folder("/work/y", AccessOperation::Read),
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
    let (_dir, conn) = fixture();
    store::insert(
        &conn,
        &key("worker", 3),
        &folder("/work/y", AccessOperation::Read),
    )
    .unwrap();
    let id = live_rows(&conn).unwrap().remove(0).grant_id;
    let revoked = revoke_live(&conn, &id).unwrap().expect("live grant");
    assert_eq!(revoked.path, PathBuf::from("/work/y"));
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
        crate::db::run_migrations(&mut conn).unwrap(),
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
        crate::db::run_migrations(&mut conn).unwrap(),
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
