//! Durable state summarized when a user is considering removing AFT.
//!
//! These counts intentionally read the state AFT already maintains instead of
//! adding runtime telemetry. At removal time, a user can still connect the
//! numbers to their recent work; a delayed TTL cleanup or an orphaned task is
//! much harder to recognize as an AFT consequence.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::Serialize;

/// The usage period shown by `aft doctor` when it explains removal costs.
pub const USAGE_WINDOW_DAYS: u8 = 7;
const USAGE_WINDOW_MILLIS: i64 = (USAGE_WINDOW_DAYS as i64) * 24 * 60 * 60 * 1_000;

/// Durable removal-time counts reported through the status payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemovalHealth {
    pub usage_window_days: u8,
    pub project_roots_served: u64,
    pub sessions_served: u64,
    /// AFT keeps stable per-root keys, not a historical root-path ledger. The
    /// count is therefore a durable approximation of distinct roots served.
    pub project_roots_source: &'static str,
    pub running_background_tasks: u64,
    pub undo_history_sessions: u64,
}

impl RemovalHealth {
    fn empty() -> Self {
        Self {
            usage_window_days: USAGE_WINDOW_DAYS,
            project_roots_source: "durable_project_keys_approximation",
            ..Self::default()
        }
    }
}

/// Read removal-time health from an already-open AFT database.
///
/// `now_millis` is an input so boundary behavior stays deterministic in tests.
pub fn removal_health_from_connection(
    conn: &Connection,
    now_millis: i64,
) -> rusqlite::Result<RemovalHealth> {
    let mut health = RemovalHealth::empty();
    let since_millis = now_millis.saturating_sub(USAGE_WINDOW_MILLIS);

    // The two activity tables already retain both a project scope key and a
    // timestamp. Project keys are deliberately one-way identifiers, so this
    // reports a count rather than pretending durable state can recover paths.
    // Keep this allowlist aligned with `BgTaskStatus::is_terminal`: new
    // non-terminal statuses should be visible as removal risks rather than
    // silently treated as safe. The partial index created in migration V6 keeps
    // this part of the single query bounded to non-terminal rows.
    let (project_roots_served, sessions_served, undo_history_sessions, running_background_tasks) =
        conn.query_row(
            "WITH activity AS (
                SELECT project_key, harness, session_id
                FROM bash_tasks
                WHERE started_at >= ?1
                UNION ALL
                SELECT project_key, harness, session_id
                FROM backups
                WHERE created_at >= ?1
            )
            SELECT
                (SELECT COUNT(*) FROM (
                    SELECT project_key FROM activity GROUP BY project_key
                )),
                (SELECT COUNT(*) FROM (
                    SELECT harness, session_id FROM activity GROUP BY harness, session_id
                )),
                (SELECT COUNT(*) FROM (
                    SELECT harness, session_id FROM backups GROUP BY harness, session_id
                )),
                (SELECT COUNT(*) FROM bash_tasks
                    WHERE status NOT IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown'))",
            [since_millis],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        )?;
    health.project_roots_served = project_roots_served;
    health.sessions_served = sessions_served;
    health.undo_history_sessions = undo_history_sessions;
    health.running_background_tasks = running_background_tasks;

    Ok(health)
}

/// Read a storage root without creating, migrating, or writing its database.
pub fn removal_health_from_storage_root(storage_root: &Path) -> Result<RemovalHealth, String> {
    let db_path = storage_root.join("aft.db");
    if !db_path.is_file() {
        return Ok(RemovalHealth::empty());
    }

    let conn = crate::db::open_readonly(&db_path)
        .map_err(|error| format!("could not open {} read-only: {error}", db_path.display()))?;
    removal_health_from_connection(&conn, unix_millis())
        .map_err(|error| format!("could not read {}: {error}", db_path.display()))
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{removal_health_from_connection, USAGE_WINDOW_MILLIS};
    use crate::db::backups::{insert_backup, BackupRow};
    use crate::db::bash_tasks::{upsert_bash_task, BashTaskRow};

    fn fixture_db() -> (tempfile::TempDir, crate::db::TrackedConnection) {
        let dir = tempfile::tempdir().expect("create fixture directory");
        let connection = crate::db::open(&dir.path().join("aft.db")).expect("open fixture db");
        (dir, connection)
    }

    fn task(
        task_id: &str,
        project_key: &str,
        session_id: &str,
        status: &str,
        started_at: i64,
    ) -> BashTaskRow {
        BashTaskRow {
            harness: "opencode".to_string(),
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            project_key: project_key.to_string(),
            command: "sleep 1".to_string(),
            cwd: "/project".to_string(),
            status: status.to_string(),
            exit_code: None,
            pid: None,
            pgid: None,
            started_at,
            completed_at: None,
            stdout_path: None,
            stderr_path: None,
            compressed: true,
            timeout_ms: None,
            completion_delivered: false,
            output_bytes: None,
            metadata: String::new(),
        }
    }

    fn backup(
        backup_id: &str,
        harness: &str,
        project_key: &str,
        session_id: &str,
        created_at: i64,
        order: u128,
    ) -> BackupRow {
        BackupRow {
            backup_id: backup_id.to_string(),
            harness: harness.to_string(),
            session_id: session_id.to_string(),
            project_key: project_key.to_string(),
            op_id: None,
            order,
            file_path: format!("/project/{backup_id}.txt"),
            path_hash: format!("path-{backup_id}"),
            backup_path: Some(format!("/backups/{backup_id}")),
            kind: "snapshot".to_string(),
            description: "fixture".to_string(),
            created_at,
            is_tombstone: false,
            restore_meta: None,
        }
    }

    #[test]
    fn usage_counts_rows_inside_the_seven_day_window_and_excludes_rows_outside_it() {
        let (_dir, conn) = fixture_db();
        let now = USAGE_WINDOW_MILLIS * 10;
        upsert_bash_task(
            &conn,
            &task(
                "inside-task",
                "project-task",
                "session-task",
                "completed",
                now - USAGE_WINDOW_MILLIS + 1,
            ),
        )
        .expect("seed inside task");
        upsert_bash_task(
            &conn,
            &task(
                "outside-task",
                "project-old",
                "session-old",
                "completed",
                now - USAGE_WINDOW_MILLIS - 1,
            ),
        )
        .expect("seed outside task");
        insert_backup(
            &conn,
            &backup(
                "inside-backup",
                "opencode",
                "project-backup",
                "session-backup",
                now - USAGE_WINDOW_MILLIS + 1,
                1,
            ),
        )
        .expect("seed inside backup");
        insert_backup(
            &conn,
            &backup(
                "outside-backup",
                "opencode",
                "project-old-backup",
                "session-old-backup",
                now - USAGE_WINDOW_MILLIS - 1,
                2,
            ),
        )
        .expect("seed outside backup");

        let health = removal_health_from_connection(&conn, now).expect("read removal health");

        assert_eq!(health.project_roots_served, 2);
        assert_eq!(health.sessions_served, 2);
    }

    #[test]
    fn running_task_count_excludes_every_terminal_status_from_the_shared_allowlist() {
        let (_dir, conn) = fixture_db();
        let now = USAGE_WINDOW_MILLIS * 10;
        for (index, status) in ["completed", "failed", "killed", "timed_out", "fate_unknown"]
            .iter()
            .enumerate()
        {
            upsert_bash_task(
                &conn,
                &task(
                    &format!("terminal-{index}"),
                    "project",
                    "session",
                    status,
                    now,
                ),
            )
            .expect("seed terminal task");
        }
        upsert_bash_task(
            &conn,
            &task("running", "project", "session", "running", now),
        )
        .expect("seed running task");
        upsert_bash_task(
            &conn,
            &task("future-state", "project", "session", "pausing", now),
        )
        .expect("seed future non-terminal task");

        let health = removal_health_from_connection(&conn, now).expect("read removal health");

        assert_eq!(health.running_background_tasks, 2);
    }

    #[test]
    fn undo_history_counts_distinct_harness_and_session_pairs() {
        let (_dir, conn) = fixture_db();
        let now = USAGE_WINDOW_MILLIS * 10;
        insert_backup(
            &conn,
            &backup("first", "opencode", "project", "same-id", now, 1),
        )
        .expect("seed first backup");
        insert_backup(
            &conn,
            &backup("second", "opencode", "project", "same-id", now, 2),
        )
        .expect("seed second backup");
        insert_backup(&conn, &backup("third", "pi", "project", "same-id", now, 3))
            .expect("seed third backup");

        let health = removal_health_from_connection(&conn, now).expect("read removal health");

        assert_eq!(health.undo_history_sessions, 2);
    }
}
