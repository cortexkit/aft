use rusqlite::{params, Connection, OptionalExtension, Row};

/// Durable async pattern-watch registration (bash_notify / bash_watch background).
///
/// Survives bridge/daemon restarts so a match that lands during the gap can still
/// be delivered. `pending_match` + match fields implement at-least-once delivery
/// until `bash_ack_completions` acks the task (same ack path as completions).
#[derive(Debug, Clone)]
pub struct BashPatternWatchRow {
    pub harness: String,
    pub session_id: String,
    pub task_id: String,
    pub watch_id: String,
    /// `"substring"` or `"regex"`.
    pub pattern_kind: String,
    pub pattern: String,
    pub once: bool,
    pub created_at: i64,
    pub stdout_offset: i64,
    pub stderr_offset: i64,
    pub pty_offset: i64,
    /// When false, a once-watch already matched and is only kept for pending delivery.
    pub scanning: bool,
    pub pending_match: bool,
    pub match_text: Option<String>,
    pub match_offset: Option<i64>,
    pub match_context: Option<String>,
}

pub fn upsert_bash_pattern_watch(
    conn: &Connection,
    row: &BashPatternWatchRow,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO bash_pattern_watches (
            harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
            created_at, stdout_offset, stderr_offset, pty_offset, scanning,
            pending_match, match_text, match_offset, match_context
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16
         )
         ON CONFLICT(harness, session_id, task_id, watch_id) DO UPDATE SET
            pattern_kind = excluded.pattern_kind,
            pattern = excluded.pattern,
            once = excluded.once,
            created_at = excluded.created_at,
            stdout_offset = excluded.stdout_offset,
            stderr_offset = excluded.stderr_offset,
            pty_offset = excluded.pty_offset,
            scanning = excluded.scanning,
            pending_match = excluded.pending_match,
            match_text = excluded.match_text,
            match_offset = excluded.match_offset,
            match_context = excluded.match_context",
        params![
            row.harness,
            row.session_id,
            row.task_id,
            row.watch_id,
            row.pattern_kind,
            row.pattern,
            row.once,
            row.created_at,
            row.stdout_offset,
            row.stderr_offset,
            row.pty_offset,
            row.scanning,
            row.pending_match,
            row.match_text,
            row.match_offset,
            row.match_context,
        ],
    )?;
    Ok(())
}

pub fn delete_bash_pattern_watch(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    watch_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3 AND watch_id = ?4",
        params![harness, session_id, task_id, watch_id],
    )
}

pub fn delete_bash_pattern_watches_for_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        params![harness, session_id, task_id],
    )
}

pub fn list_bash_pattern_watches_for_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
                created_at, stdout_offset, stderr_offset, pty_offset, scanning,
                pending_match, match_text, match_offset, match_context
         FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3
         ORDER BY created_at ASC, watch_id ASC",
    )?;
    let rows = stmt
        .query_map(params![harness, session_id, task_id], map_watch_row)?
        .collect();
    rows
}

pub fn list_bash_pattern_watches_for_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
                created_at, stdout_offset, stderr_offset, pty_offset, scanning,
                pending_match, match_text, match_offset, match_context
         FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2
         ORDER BY created_at ASC, task_id ASC, watch_id ASC",
    )?;
    let rows = stmt
        .query_map(params![harness, session_id], map_watch_row)?
        .collect();
    rows
}

pub fn list_bash_pattern_watches(
    conn: &Connection,
    harness: &str,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
                created_at, stdout_offset, stderr_offset, pty_offset, scanning,
                pending_match, match_text, match_offset, match_context
         FROM bash_pattern_watches
         WHERE harness = ?1
         ORDER BY created_at ASC, task_id ASC, watch_id ASC",
    )?;
    let rows = stmt.query_map(params![harness], map_watch_row)?.collect();
    rows
}

pub fn list_bash_pattern_watches_by_task_id(
    conn: &Connection,
    harness: &str,
    task_id: &str,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
                created_at, stdout_offset, stderr_offset, pty_offset, scanning,
                pending_match, match_text, match_offset, match_context
         FROM bash_pattern_watches
         WHERE harness = ?1 AND task_id = ?2
         ORDER BY created_at ASC, session_id ASC, watch_id ASC",
    )?;
    let rows = stmt
        .query_map(params![harness, task_id], map_watch_row)?
        .collect();
    rows
}

pub fn count_pending_bash_pattern_watches_for_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*)
         FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND pending_match = 1",
        params![harness, session_id],
        |row| row.get(0),
    )
}

pub fn count_pending_bash_pattern_watches(
    conn: &Connection,
    harness: &str,
) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT COUNT(*)
         FROM bash_pattern_watches
         WHERE harness = ?1 AND pending_match = 1",
        params![harness],
        |row| row.get(0),
    )
}

pub fn get_bash_pattern_watch(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    watch_id: &str,
) -> rusqlite::Result<Option<BashPatternWatchRow>> {
    conn.query_row(
        "SELECT harness, session_id, task_id, watch_id, pattern_kind, pattern, once,
                created_at, stdout_offset, stderr_offset, pty_offset, scanning,
                pending_match, match_text, match_offset, match_context
         FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3 AND watch_id = ?4",
        params![harness, session_id, task_id, watch_id],
        map_watch_row,
    )
    .optional()
}

pub fn update_watch_offsets_for_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    stdout_offset: i64,
    stderr_offset: i64,
    pty_offset: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE bash_pattern_watches
         SET stdout_offset = ?4, stderr_offset = ?5, pty_offset = ?6
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        params![
            harness,
            session_id,
            task_id,
            stdout_offset,
            stderr_offset,
            pty_offset
        ],
    )
}

fn map_watch_row(row: &Row<'_>) -> rusqlite::Result<BashPatternWatchRow> {
    Ok(BashPatternWatchRow {
        harness: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        watch_id: row.get(3)?,
        pattern_kind: row.get(4)?,
        pattern: row.get(5)?,
        once: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        stdout_offset: row.get(8)?,
        stderr_offset: row.get(9)?,
        pty_offset: row.get(10)?,
        scanning: row.get::<_, i64>(11)? != 0,
        pending_match: row.get::<_, i64>(12)? != 0,
        match_text: row.get(13)?,
        match_offset: row.get(14)?,
        match_context: row.get(15)?,
    })
}
