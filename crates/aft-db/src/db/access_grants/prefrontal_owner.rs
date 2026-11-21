use crate::access_policy::prefrontal::MODULE_ID;
use rusqlite::{params, Connection, TransactionBehavior};

const OLD_OWNER: &str = r#"{"kind":"reserved","module_id":"prefrontal-core"}"#;

/// Move saved grants and their scope registrations on the owning connection.
/// Roll back by restoring the previous AFT release; no database changes are
/// needed. The previous AFT looks up the old key, so it won't find the
/// moved grants and affected agents get one approval card per folder again.
/// Keep tombstones under both keys so neither version can revive an ended scope.
pub(crate) fn migrate_prefrontal_owner(conn: &mut Connection) -> rusqlite::Result<()> {
    let new_owner = super::owner_key(&subc_protocol::Principal::Reserved {
        module_id: MODULE_ID.into(),
    });
    let owners = params![OLD_OWNER, new_owner];

    // After the first successful open, the legacy key is only in already
    // copied tombstones. Don't take the write lock on routine opens.
    let pending: bool = conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM access_scopes AS old WHERE old.owner=?1 AND (
                old.ended=0 OR NOT EXISTS (
                    SELECT 1 FROM access_scopes AS new WHERE new.owner=?2
                    AND new.scope_ref=old.scope_ref AND new.epoch=old.epoch AND new.ended!=0
                )
            )
            UNION ALL SELECT 1 FROM access_scopes WHERE parent_owner=?1 AND NOT (owner=?1 AND ended!=0)
            UNION ALL SELECT 1 FROM access_grants WHERE owner=?1
            UNION ALL SELECT 1 FROM access_modes WHERE owner=?1
            UNION ALL SELECT 1 FROM access_scope_agents WHERE owner=?1
            UNION ALL SELECT 1 FROM access_materialized WHERE owner=?1
        )",
        owners,
        |row| row.get(0),
    )?;
    if !pending {
        return Ok(());
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // In either direction, an existing end takes precedence over a live row.
    tx.execute(
        "UPDATE access_scopes AS old SET ended=1 WHERE old.owner=?1 AND old.ended=0
         AND EXISTS (SELECT 1 FROM access_scopes AS new WHERE new.owner=?2
             AND new.scope_ref=old.scope_ref AND new.epoch=old.epoch AND new.ended!=0)",
        owners,
    )?;
    tx.execute(
        "INSERT INTO access_scopes(owner,scope_ref,epoch,parent_owner,parent,parent_epoch,config,ended)
         SELECT ?2,scope_ref,epoch,parent_owner,parent,parent_epoch,config,ended
         FROM access_scopes WHERE owner=?1
         ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET ended=1
         WHERE excluded.ended!=0 AND access_scopes.ended=0",
        owners,
    )?;
    tx.execute(
        "UPDATE access_scopes SET parent_owner=?2 WHERE parent_owner=?1 AND NOT (owner=?1 AND ended!=0)",
        owners,
    )?;

    for table in [
        "access_grants",
        "access_modes",
        "access_scope_agents",
        "access_materialized",
    ] {
        // Don't retain authority from a live row that lost to a tombstone.
        tx.execute(
            &format!(
                "DELETE FROM {table} WHERE owner IN (?1,?2) AND EXISTS (
                SELECT 1 FROM access_scopes AS old WHERE old.owner=?1 AND old.ended!=0
                AND old.scope_ref={table}.scope_ref AND old.epoch={table}.epoch
            )"
            ),
            owners,
        )?;
        // Preserve the current key on collision, including the grant's mode and
        // opaque id. Non-duplicates retain every column except their owner.
        tx.execute(
            &format!("UPDATE OR IGNORE {table} SET owner=?2 WHERE owner=?1"),
            owners,
        )?;
        tx.execute(&format!("DELETE FROM {table} WHERE owner=?1"), [OLD_OWNER])?;
    }
    tx.execute(
        "DELETE FROM access_scopes WHERE owner=?1 AND ended=0",
        [OLD_OWNER],
    )?;
    tx.commit()
}
