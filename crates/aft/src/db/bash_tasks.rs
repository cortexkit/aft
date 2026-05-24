use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone)]
pub struct BashTaskRow {
    pub harness: String,
    pub session_id: String,
    pub task_id: String,
    pub project_key: String,
    pub command: String,
    pub cwd: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub pid: Option<i64>,
    pub pgid: Option<i64>,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub compressed: bool,
    pub timeout_ms: Option<i64>,
    pub completion_delivered: bool,
    pub output_bytes: Option<i64>,
    pub metadata: String,
}

pub fn upsert_bash_task(conn: &Connection, row: &BashTaskRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO bash_tasks (
            harness, session_id, task_id, project_key, command, cwd, status,
            exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
            compressed, timeout_ms, completion_delivered, output_bytes, metadata
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18, ?19
         )
         ON CONFLICT(harness, session_id, task_id) DO UPDATE SET
            project_key = excluded.project_key,
            command = excluded.command,
            cwd = excluded.cwd,
            status = excluded.status,
            exit_code = excluded.exit_code,
            pid = excluded.pid,
            pgid = excluded.pgid,
            started_at = excluded.started_at,
            completed_at = excluded.completed_at,
            stdout_path = excluded.stdout_path,
            stderr_path = excluded.stderr_path,
            compressed = excluded.compressed,
            timeout_ms = excluded.timeout_ms,
            completion_delivered = excluded.completion_delivered,
            output_bytes = excluded.output_bytes,
            metadata = excluded.metadata",
        params![
            row.harness,
            row.session_id,
            row.task_id,
            row.project_key,
            row.command,
            row.cwd,
            row.status,
            row.exit_code,
            row.pid,
            row.pgid,
            row.started_at,
            row.completed_at,
            row.stdout_path,
            row.stderr_path,
            row.compressed,
            row.timeout_ms,
            row.completion_delivered,
            row.output_bytes,
            row.metadata,
        ],
    )?;
    Ok(())
}

pub fn get_bash_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> rusqlite::Result<Option<BashTaskRow>> {
    conn.query_row(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        params![harness, session_id, task_id],
        map_bash_task_row,
    )
    .optional()
}

pub fn list_bash_tasks_for_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> rusqlite::Result<Vec<BashTaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2
         ORDER BY started_at ASC, task_id ASC",
    )?;

    let rows = stmt
        .query_map(params![harness, session_id], map_bash_task_row)?
        .collect();
    rows
}

pub fn find_bash_task_for_project(
    conn: &Connection,
    harness: &str,
    project_key: &str,
    task_id: &str,
) -> rusqlite::Result<Option<BashTaskRow>> {
    conn.query_row(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND project_key = ?2 AND task_id = ?3
         ORDER BY started_at DESC
         LIMIT 1",
        params![harness, project_key, task_id],
        map_bash_task_row,
    )
    .optional()
}

fn map_bash_task_row(row: &Row<'_>) -> rusqlite::Result<BashTaskRow> {
    Ok(BashTaskRow {
        harness: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        project_key: row.get(3)?,
        command: row.get(4)?,
        cwd: row.get(5)?,
        status: row.get(6)?,
        exit_code: row.get(7)?,
        pid: row.get(8)?,
        pgid: row.get(9)?,
        started_at: row.get(10)?,
        completed_at: row.get(11)?,
        stdout_path: row.get(12)?,
        stderr_path: row.get(13)?,
        compressed: row.get::<_, i64>(14)? != 0,
        timeout_ms: row.get(15)?,
        completion_delivered: row.get::<_, i64>(16)? != 0,
        output_bytes: row.get(17)?,
        metadata: row.get::<_, Option<String>>(18)?.unwrap_or_default(),
    })
}
