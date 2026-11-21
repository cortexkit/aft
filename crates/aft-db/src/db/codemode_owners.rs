//! Optional restart cache. Older AFT database readers can ignore the table,
//! so installing it does not change the minimum supported reader version.

use super::access_grants::{scope_ended, ScopeKey};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};

const MAX_ROWS: i64 = 4_096;
const IDLE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

pub(super) fn install(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS codemode_owners (
             owner TEXT NOT NULL,
             scope_ref TEXT NOT NULL,
             epoch BLOB NOT NULL,
             runner TEXT NOT NULL,
             identity TEXT NOT NULL,
             used_ms INTEGER NOT NULL,
             UNIQUE (owner, scope_ref, epoch, runner)
         );
         CREATE INDEX IF NOT EXISTS idx_codemode_owners_used ON codemode_owners (used_ms);",
    )
}

pub fn save(
    conn: &Connection,
    scope: &ScopeKey,
    runner: &str,
    identity: &str,
    now: i64,
) -> rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let epoch = scope.epoch.to_be_bytes();
    // Late bind work must not restore an ended scope or replace a newer epoch.
    if scope_ended(&tx, scope)? || newer_scope_registered(&tx, scope)? || tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM codemode_owners WHERE owner=?1 AND scope_ref=?2 AND epoch>?3)",
        params![scope.owner, scope.scope_ref, epoch], |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    discard_older(&tx, scope)?;
    sweep(&tx, now)?;
    tx.execute(
        "INSERT INTO codemode_owners(owner,scope_ref,epoch,runner,identity,used_ms)
         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(owner,scope_ref,epoch,runner)
         DO UPDATE SET identity=excluded.identity, used_ms=excluded.used_ms",
        params![scope.owner, scope.scope_ref, epoch, runner, identity, now],
    )?;
    tx.execute(
        "DELETE FROM codemode_owners WHERE rowid IN
         (SELECT rowid FROM codemode_owners ORDER BY used_ms DESC, rowid DESC LIMIT -1 OFFSET ?1)",
        [MAX_ROWS],
    )?;
    tx.commit()
}

/// Scope registrations retain the daemon's newer epoch even when the bind
/// was a lease, not a runner, or its restart snapshot has already expired.
fn newer_scope_registered(conn: &Connection, scope: &ScopeKey) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_scopes WHERE owner=?1 AND scope_ref=?2
         AND (length(epoch)>length(?3) OR (length(epoch)=length(?3) AND epoch>?3)))",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
        |row| row.get(0),
    )
}

pub fn discard_older(conn: &Connection, scope: &ScopeKey) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM codemode_owners WHERE owner=?1 AND scope_ref=?2 AND epoch<?3",
        params![scope.owner, scope.scope_ref, scope.epoch.to_be_bytes()],
    )?;
    Ok(())
}

pub(super) fn sweep(conn: &Connection, now: i64) -> rusqlite::Result<()> {
    let cutoff = now.saturating_sub(IDLE_MS);
    // Read first: a DELETE takes the write lock even when nothing matches, and
    // this runs on every maintenance pass while most passes have nothing to do.
    let expired: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM codemode_owners WHERE used_ms <= ?1)",
        [cutoff],
        |row| row.get(0),
    )?;
    if expired {
        conn.execute("DELETE FROM codemode_owners WHERE used_ms <= ?1", [cutoff])?;
    }
    Ok(())
}

pub fn load(conn: &Connection, scope: &ScopeKey, now: i64) -> rusqlite::Result<Vec<String>> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    sweep(&tx, now)?;
    let key = params![scope.owner, scope.scope_ref, scope.epoch.to_be_bytes()];
    let rows = if scope_ended(&tx, scope)? || newer_scope_registered(&tx, scope)? {
        Vec::new()
    } else {
        let mut stmt = tx.prepare(
            "SELECT identity FROM codemode_owners WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
        )?;
        let rows = stmt
            .query_map(key, |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    tx.commit()?;
    Ok(rows)
}

pub fn touch(conn: &Connection, scope: &ScopeKey, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codemode_owners SET used_ms=MAX(used_ms,?4)
         WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND used_ms>?5",
        params![
            scope.owner,
            scope.scope_ref,
            scope.epoch.to_be_bytes(),
            now,
            now.saturating_sub(IDLE_MS)
        ],
    )?;
    Ok(())
}

pub fn end(conn: &Connection, scope: &ScopeKey) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM codemode_owners WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
        params![scope.owner, scope.scope_ref, scope.epoch.to_be_bytes()],
    )?;
    Ok(())
}
