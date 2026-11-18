//! Additive audit storage for inbound computer tool calls.

use rusqlite::{params, Connection};

const DDL: &str = "
CREATE TABLE IF NOT EXISTS remote_tool_audit (
    received_at_ms INTEGER NOT NULL,
    peer_box_id TEXT,
    pairing_id TEXT,
    caller_id TEXT,
    tool TEXT,
    folder TEXT,
    mode TEXT,
    decision TEXT,
    outcome TEXT
);";

pub(crate) fn insert(
    conn: &Connection,
    received_at_ms: i64,
    peer_box_id: Option<&str>,
    pairing_id: Option<&str>,
    caller_id: Option<&str>,
    tool: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute_batch(DDL)?;
    conn.execute(
        "INSERT INTO remote_tool_audit (received_at_ms, peer_box_id, pairing_id, caller_id, tool)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![received_at_ms, peer_box_id, pairing_id, caller_id, tool],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn resolve(
    conn: &Connection,
    row: i64,
    folder: Option<&str>,
    mode: Option<&str>,
) -> rusqlite::Result<()> {
    require_row(conn.execute(
        "UPDATE remote_tool_audit SET folder=?2, mode=?3 WHERE rowid=?1",
        params![row, folder, mode],
    )?)
}

pub(crate) fn mark(
    conn: &Connection,
    row: i64,
    decision: &str,
    outcome: Option<&str>,
) -> rusqlite::Result<()> {
    require_row(conn.execute(
        "UPDATE remote_tool_audit SET decision=?2, outcome=?3 WHERE rowid=?1",
        params![row, decision, outcome],
    )?)
}

fn require_row(changed: usize) -> rusqlite::Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(rusqlite::Error::QueryReturnedNoRows)
    }
}
