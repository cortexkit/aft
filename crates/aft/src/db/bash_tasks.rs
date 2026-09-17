use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::bash_background::persistence::{
    resolve_task_layout, session_tasks_dir, uninitialized_layout_is_recent,
};

pub const TERMINAL_ROW_RETENTION_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_TERMINAL_PRUNE_ROWS: usize = 500;
const LAYOUT_CREATION_GRACE: Duration = Duration::from_secs(5 * 60);

const TERMINAL_PRUNE_PREDICATE: &str = "
    status IN ('completed', 'failed', 'killed', 'timed_out')
    AND completed_at IS NOT NULL
    AND completed_at < ?1
    AND completion_delivered = 1
    AND NOT EXISTS (
        SELECT 1 FROM bash_pattern_watches AS watch
        WHERE watch.harness = bash_tasks.harness
          AND watch.session_id = bash_tasks.session_id
          AND watch.task_id = bash_tasks.task_id
    )";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalRowsPrune {
    pub removed: usize,
    pub remaining_candidates: usize,
}

#[derive(Debug)]
struct TerminalPruneCandidate {
    harness: String,
    session_id: String,
    task_id: String,
}

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

pub fn delete_delivered_terminal_bash_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    reason: &str,
) -> rusqlite::Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3
           AND completion_delivered = 1
           AND status IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown')",
        params![harness, session_id, task_id],
    )?;
    // A row can produce this warning only once: retries affect zero rows after
    // the first successful DELETE, preventing a cleanup loop from flooding logs.
    if deleted > 0 {
        crate::slog_warn!("bash task row deleted: task_id={task_id} reason={reason}");
    }
    Ok(deleted)
}

pub fn delete_bash_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        params![harness, session_id, task_id],
    )
}

/// Remove acknowledged terminal rows only after the retention age has passed
/// and the task layout is absent. The filesystem check uses the same lookup and
/// initialization grace as persisted-task garbage collection, so a task being
/// created concurrently is not mistaken for a missing task.
pub fn prune_terminal_rows(
    conn: &Connection,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<TerminalRowsPrune> {
    prune_terminal_rows_guarded(conn, now_ms, limit, |_| false)
}

pub(crate) fn prune_terminal_rows_guarded(
    conn: &Connection,
    now_ms: i64,
    limit: usize,
    is_registered_in_process: impl Fn(&str) -> bool,
) -> rusqlite::Result<TerminalRowsPrune> {
    let cutoff = now_ms.saturating_sub(TERMINAL_ROW_RETENTION_AGE_MS);
    let bounded_limit = limit.min(MAX_TERMINAL_PRUNE_ROWS);
    let candidates = if bounded_limit == 0 {
        Vec::new()
    } else {
        conn.prepare(&format!(
            "SELECT harness, session_id, task_id
             FROM bash_tasks
             WHERE {TERMINAL_PRUNE_PREDICATE}
             ORDER BY completed_at, harness, session_id, task_id
             LIMIT ?2"
        ))?
        .query_map(
            params![cutoff, i64::try_from(bounded_limit).unwrap_or(500)],
            |row| {
                Ok(TerminalPruneCandidate {
                    harness: row.get(0)?,
                    session_id: row.get(1)?,
                    task_id: row.get(2)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut removed = 0;
    for candidate in candidates {
        if is_registered_in_process(&candidate.task_id)
            || !task_layout_is_gone(conn, &candidate.session_id, &candidate.task_id)
        {
            continue;
        }
        removed += conn.execute(
            &format!(
                "DELETE FROM bash_tasks
                 WHERE harness = ?2 AND session_id = ?3 AND task_id = ?4
                   AND {TERMINAL_PRUNE_PREDICATE}"
            ),
            params![cutoff, candidate.harness, candidate.session_id, candidate.task_id],
        )?;
    }

    let remaining_candidates = conn.query_row(
        &format!("SELECT COUNT(*) FROM bash_tasks WHERE {TERMINAL_PRUNE_PREDICATE}"),
        [cutoff],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(TerminalRowsPrune {
        removed,
        remaining_candidates: usize::try_from(remaining_candidates).unwrap_or(usize::MAX),
    })
}

fn task_layout_is_gone(conn: &Connection, session_id: &str, task_id: &str) -> bool {
    let Some(storage_root) = conn
        .path()
        .and_then(|path| Path::new(path).parent())
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return false;
    };
    let session_dir = session_tasks_dir(storage_root, session_id);
    match resolve_task_layout(&session_dir, task_id) {
        Ok(_) => false,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let directory_layout = session_dir.join(task_id);
            let flat_layout = session_dir.join(format!("{task_id}.json"));
            if !directory_layout.exists() && !flat_layout.exists() {
                return true;
            }
            !uninitialized_layout_is_recent(&session_dir, task_id, LAYOUT_CREATION_GRACE)
                .unwrap_or(true)
        }
        Err(_) => false,
    }
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

const SESSION_TASKS_SQL: &str =
    "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND session_id = ?2";

pub fn list_bash_tasks_for_session(
    conn: &Connection,
    harness: &str,
    session_id: &str,
) -> rusqlite::Result<Vec<BashTaskRow>> {
    let mut stmt = conn.prepare(SESSION_TASKS_SQL)?;
    let mut rows = stmt
        .query_map(params![harness, session_id], map_bash_task_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Task rows carry large command/metadata payloads. Sorting them in SQLite
    // spills entire rows to a temporary file for long-lived sessions. Keep the
    // legacy integer/BINARY order, but sort the already-required result vector.
    rows.sort_by(|a, b| (a.started_at, &a.task_id).cmp(&(b.started_at, &b.task_id)));
    Ok(rows)
}

pub fn list_bash_tasks_by_id(
    conn: &Connection,
    harness: &str,
    task_id: &str,
) -> rusqlite::Result<Vec<BashTaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND task_id = ?2
         ORDER BY started_at DESC",
    )?;
    let rows = stmt
        .query_map(params![harness, task_id], map_bash_task_row)?
        .collect();
    rows
}

pub fn list_replayable_bash_tasks_for_project(
    conn: &Connection,
    harness: &str,
    project_key: &str,
) -> rusqlite::Result<Vec<BashTaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness, session_id, task_id, project_key, command, cwd, status,
                exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
                compressed, timeout_ms, completion_delivered, output_bytes, metadata
         FROM bash_tasks
         WHERE harness = ?1 AND project_key = ?2
           AND (status NOT IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown')
                OR completion_delivered = 0)
         ORDER BY started_at ASC, task_id ASC",
    )?;
    let rows = stmt
        .query_map(params![harness, project_key], map_bash_task_row)?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_history_preserves_sqlite_order_without_a_temp_sort() {
        let temp = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&temp.path().join("aft.db")).unwrap();
        for (task, started, status) in [
            ("é", 4, "running"),
            ("a", 4, "completed"),
            ("z", -1, "failed"),
            ("A", 4, "failed"),
            ("aa", 4, "running"),
            ("first", i64::MIN, "completed"),
        ] {
            conn.execute("INSERT INTO bash_tasks
                (harness, session_id, task_id, project_key, command, cwd, status, started_at, metadata)
                VALUES ('opencode', 'session', ?1, 'project', ?2, '.', ?3, ?4, ?5)",
                params![task, "command".repeat(8192), status, started, format!("metadata-{task}")]).unwrap();
        }
        let legacy = conn
            .prepare(
                "SELECT harness, session_id, task_id, project_key, command, cwd, status,
            exit_code, pid, pgid, started_at, completed_at, stdout_path, stderr_path,
            compressed, timeout_ms, completion_delivered, output_bytes, metadata
            FROM bash_tasks WHERE harness = ?1 AND session_id = ?2
            ORDER BY started_at ASC, task_id ASC",
            )
            .unwrap()
            .query_map(params!["opencode", "session"], map_bash_task_row)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let actual = list_bash_tasks_for_session(&conn, "opencode", "session").unwrap();
        assert_eq!(format!("{actual:?}"), format!("{legacy:?}"));
        assert_eq!(
            actual
                .iter()
                .map(|r| r.task_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "z", "A", "a", "aa", "é"]
        );
        assert!(list_bash_tasks_for_session(&conn, "other", "session")
            .unwrap()
            .is_empty());
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SESSION_TASKS_SQL}"))
            .unwrap()
            .query_map(params!["opencode", "session"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "session history must not spill task rows: {plan}"
        );
    }
}
