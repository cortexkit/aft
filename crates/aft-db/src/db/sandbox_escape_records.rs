//! Additive audit records for person-approved, one-command sandbox escapes.
//!
//! This table is deliberately outside the numbered schema. Older binaries can
//! open the same database and simply ignore it, making a binary swap a safe
//! rollback. Command output is not part of the record shape.

use rusqlite::{params, Connection, OptionalExtension};

pub const RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
pub const MAX_ROWS: usize = 10_000;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS sandbox_escape_records (
    call_key TEXT NOT NULL PRIMARY KEY,
    requested_at_ms INTEGER NOT NULL,
    caller_session TEXT NOT NULL,
    caller_scope TEXT NOT NULL,
    agent_id TEXT,
    command TEXT NOT NULL,
    cwd TEXT NOT NULL,
    shell TEXT NOT NULL,
    escapes_level TEXT NOT NULL CHECK (escapes_level IN ('credentials', 'standard', 'strict')),
    background INTEGER NOT NULL CHECK (background IN (0, 1)),
    elicitation_id TEXT,
    task_id TEXT,
    decision TEXT NOT NULL,
    exit_status TEXT,
    exit_code INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sandbox_escape_records_retention
    ON sandbox_escape_records(requested_at_ms);
CREATE INDEX IF NOT EXISTS idx_sandbox_escape_records_task
    ON sandbox_escape_records(task_id);
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRecord {
    pub call_key: String,
    pub requested_at_ms: i64,
    pub caller_session: String,
    pub caller_scope: String,
    pub agent_id: Option<String>,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub escapes_level: String,
    pub background: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub call_key: String,
    pub requested_at_ms: i64,
    pub caller_session: String,
    pub caller_scope: String,
    pub agent_id: Option<String>,
    pub command: String,
    pub cwd: String,
    pub shell: String,
    pub escapes_level: String,
    pub background: bool,
    pub elicitation_id: Option<String>,
    pub task_id: Option<String>,
    pub decision: String,
    pub exit_status: Option<String>,
    pub exit_code: Option<i64>,
}

pub fn install(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(DDL)
}

#[cfg(test)]
#[path = "sandbox_escape_records_tests.rs"]
mod tests;

pub fn insert(conn: &Connection, record: &NewRecord) -> rusqlite::Result<()> {
    install(conn)?;
    conn.execute(
        "INSERT OR IGNORE INTO sandbox_escape_records (
            call_key, requested_at_ms, caller_session, caller_scope, agent_id,
            command, cwd, shell, escapes_level, background, decision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'requested')",
        params![
            record.call_key,
            record.requested_at_ms,
            record.caller_session,
            record.caller_scope,
            record.agent_id,
            record.command,
            record.cwd,
            record.shell,
            record.escapes_level,
            record.background,
        ],
    )?;
    Ok(())
}

pub fn note_elicitation(conn: &Connection, call_key: &str, id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sandbox_escape_records SET elicitation_id=?2 WHERE call_key=?1",
        params![call_key, id],
    )?;
    Ok(())
}

pub fn note_decision(conn: &Connection, call_key: &str, decision: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sandbox_escape_records SET decision=?2 WHERE call_key=?1",
        params![call_key, decision],
    )?;
    Ok(())
}

pub fn finish(
    conn: &Connection,
    call_key: &str,
    decision: &str,
    exit_status: Option<&str>,
    exit_code: Option<i64>,
    task_id: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sandbox_escape_records
         SET decision=?2, exit_status=?3, exit_code=?4, task_id=COALESCE(?5, task_id)
         WHERE call_key=?1",
        params![call_key, decision, exit_status, exit_code, task_id],
    )?;
    Ok(())
}

/// Update the terminal status of an approved background command. The task id is
/// recorded at launch; output remains in the ordinary bounded task capture.
pub fn finish_task(
    conn: &Connection,
    task_id: &str,
    exit_status: &str,
    exit_code: Option<i64>,
) -> rusqlite::Result<()> {
    install(conn)?;
    conn.execute(
        "UPDATE sandbox_escape_records SET exit_status=?2, exit_code=?3 WHERE task_id=?1",
        params![task_id, exit_status, exit_code],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, call_key: &str) -> rusqlite::Result<Option<Record>> {
    install(conn)?;
    conn.query_row(
        "SELECT call_key, requested_at_ms, caller_session, caller_scope, agent_id,
                command, cwd, shell, escapes_level, background, elicitation_id,
                task_id, decision, exit_status, exit_code
         FROM sandbox_escape_records WHERE call_key=?1",
        [call_key],
        |row| {
            Ok(Record {
                call_key: row.get(0)?,
                requested_at_ms: row.get(1)?,
                caller_session: row.get(2)?,
                caller_scope: row.get(3)?,
                agent_id: row.get(4)?,
                command: row.get(5)?,
                cwd: row.get(6)?,
                shell: row.get(7)?,
                escapes_level: row.get(8)?,
                background: row.get(9)?,
                elicitation_id: row.get(10)?,
                task_id: row.get(11)?,
                decision: row.get(12)?,
                exit_status: row.get(13)?,
                exit_code: row.get(14)?,
            })
        },
    )
    .optional()
}

/// Apply both retention bounds in one maintenance transaction. The row cap
/// keeps the newest records, independently of their timestamps.
pub fn sweep(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    install(conn)?;
    let tx = conn.unchecked_transaction()?;
    let cutoff = now_ms.saturating_sub(RETENTION_MS);
    let expired = tx.execute(
        "DELETE FROM sandbox_escape_records WHERE requested_at_ms < ?1",
        [cutoff],
    )?;
    let excess = tx.execute(
        "DELETE FROM sandbox_escape_records WHERE rowid IN (
            SELECT rowid FROM sandbox_escape_records
            ORDER BY requested_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?1
         )",
        [MAX_ROWS],
    )?;
    tx.commit()?;
    Ok(expired + excess)
}
