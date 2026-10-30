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
    pub each_line: bool,
    pub match_batch: Option<String>,
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
            pending_match, match_text, match_offset, match_context, each_line, match_batch
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7,
            ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18
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
            match_context = excluded.match_context, each_line = excluded.each_line, match_batch = excluded.match_batch",
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
            row.each_line,
            row.match_batch,
        ],
    )?;
    refresh_watch_gap_health(conn);
    Ok(())
}

pub fn delete_bash_pattern_watch(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
    watch_id: &str,
) -> rusqlite::Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3 AND watch_id = ?4",
        params![harness, session_id, task_id, watch_id],
    )?;
    refresh_watch_gap_health(conn);
    Ok(deleted)
}

pub fn delete_bash_pattern_watches_for_task(
    conn: &Connection,
    harness: &str,
    session_id: &str,
    task_id: &str,
) -> rusqlite::Result<usize> {
    let deleted = conn.execute(
        "DELETE FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND task_id = ?3",
        params![harness, session_id, task_id],
    )?;
    refresh_watch_gap_health(conn);
    Ok(deleted)
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
                pending_match, match_text, match_offset, match_context, each_line, match_batch
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
                pending_match, match_text, match_offset, match_context, each_line, match_batch
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
                pending_match, match_text, match_offset, match_context, each_line, match_batch
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
                pending_match, match_text, match_offset, match_context, each_line, match_batch
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
        "SELECT (SELECT COUNT(*)
         FROM bash_pattern_watches
         WHERE harness = ?1 AND session_id = ?2 AND pending_match = 1) + (SELECT COUNT(*) FROM bash_watch_delivery_gaps WHERE harness=?1 AND session_id=?2)",
        params![harness, session_id],
        |row| row.get(0),
    )
}

pub fn count_pending_bash_pattern_watches(
    conn: &Connection,
    harness: &str,
) -> rusqlite::Result<usize> {
    conn.query_row(
        "SELECT (SELECT COUNT(*)
         FROM bash_pattern_watches
         WHERE harness = ?1 AND pending_match = 1) + (SELECT COUNT(*) FROM bash_watch_delivery_gaps WHERE harness=?1)",
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
                pending_match, match_text, match_offset, match_context, each_line, match_batch
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
        each_line: row.get(16)?,
        match_batch: row.get(17)?,
    })
}

// Keep the last census for every opened database, even after its actor retires.
// Health reads only this compact cache, never SQLite on the frame loop.
type GapCensus = (u64, u64, bool);
static WATCH_GAP_CENSUS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, GapCensus>>,
> = std::sync::OnceLock::new();

/// The census entry for one database: SQLite's own file name for it, so every
/// connection to the same file shares an entry, or a per-connection key for an
/// in-memory database.
fn census_key(conn: &Connection) -> String {
    conn.path()
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("memory:{:p}", conn))
}

pub(crate) fn refresh_watch_gap_health(conn: &Connection) {
    let key = census_key(conn);
    let census = (|| -> rusqlite::Result<GapCensus> {
        let mut stmt = conn.prepare("SELECT match_batch FROM bash_pattern_watches WHERE pending_match = 1 AND match_batch IS NOT NULL UNION ALL SELECT batch FROM bash_watch_delivery_gaps")?;
        let batches = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let (mut outstanding, mut missing) = (0u64, 0u64);
        let mut uncertain = false;
        for batch in batches {
            match serde_json::from_str::<crate::bash_background::watch_lines::LineBatch>(&batch?) {
                Ok(batch) => {
                    if let Some(gap) = batch.retry_gap {
                        outstanding = outstanding.saturating_add(1);
                        missing = missing.saturating_add(gap.known_missing_occurrences);
                    }
                }
                Err(_) => uncertain = true,
            }
        }
        Ok((outstanding, missing, uncertain))
    })();
    let mut cache = WATCH_GAP_CENSUS
        .get_or_init(Default::default)
        .lock()
        .unwrap();
    match census {
        Ok((0, 0, false)) => {
            cache.remove(&key);
        }
        Ok(value) => {
            cache.insert(key, value);
        }
        Err(_) => {
            cache.entry(key).or_default().2 = true;
        }
    }
}

pub(crate) fn watch_gap_health() -> GapCensus {
    WATCH_GAP_CENSUS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .values()
        .fold((0u64, 0u64, false), |(gaps, missing, uncertain), next| {
            (
                gaps.saturating_add(next.0),
                missing.saturating_add(next.1),
                uncertain || next.2,
            )
        })
}

pub(crate) fn delivery_ack_version(
    conn: &Connection,
    harness: &str,
    session: &str,
    task: &str,
    watch: &str,
) -> rusqlite::Result<Option<(u64, String, u64)>> {
    conn.query_row("SELECT version, eviction_epoch, evicted_through FROM bash_watch_delivery_acks WHERE harness=?1 AND session_id=?2 AND task_id=?3 AND watch_id=?4", params![harness, session, task, watch], |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get(1)?, r.get::<_, i64>(2)?.max(0) as u64))).optional()
}

pub(crate) fn acknowledge_delivery(
    conn: &Connection,
    harness: &str,
    session: &str,
    task: &str,
    watch: &str,
    version: u64,
    gap: Option<&crate::bash_background::watch_lines::RetryGap>,
) -> rusqlite::Result<()> {
    let epoch = gap
        .map(|gap| gap.eviction_epoch.as_str())
        .unwrap_or(crate::subc::watch_delivery::eviction_epoch());
    let through = gap.map(|gap| gap.evicted_through).unwrap_or_default();
    conn.execute("INSERT INTO bash_watch_delivery_acks(harness, session_id, task_id, watch_id, version, acknowledged_at, eviction_epoch, evicted_through) VALUES(?1,?2,?3,?4,?5,unixepoch(),?6,?7) ON CONFLICT(harness, session_id, task_id, watch_id) DO UPDATE SET version=MAX(version,excluded.version), acknowledged_at=excluded.acknowledged_at, eviction_epoch=CASE WHEN excluded.eviction_epoch != '' THEN excluded.eviction_epoch ELSE eviction_epoch END, evicted_through=CASE WHEN eviction_epoch = excluded.eviction_epoch THEN MAX(evicted_through,excluded.evicted_through) WHEN excluded.eviction_epoch != '' THEN excluded.evicted_through ELSE evicted_through END", params![harness, session, task, watch, version as i64, epoch, through as i64])?;
    // Only prune old acknowledgements from an earlier daemon: its process-local
    // retry copies no longer exist, even if the current daemon is long-lived.
    conn.execute(
        "DELETE FROM bash_watch_delivery_acks WHERE acknowledged_at < unixepoch() - 2592000 AND eviction_epoch != ?1",
        [crate::subc::watch_delivery::eviction_epoch()],
    )?;
    Ok(())
}

/// One database's census entry. Uses the connection's own key rather than a
/// path the test spells, since SQLite reports its file name in its own form
/// (symlinks resolved on macOS, no verbatim prefix on Windows).
#[cfg(test)]
pub(crate) fn watch_gap_health_for(conn: &Connection) -> GapCensus {
    WATCH_GAP_CENSUS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .get(&census_key(conn))
        .copied()
        .unwrap_or_default()
}

pub(crate) fn list_delivery_gaps(
    conn: &Connection,
    harness: &str,
    session: Option<&str>,
) -> rusqlite::Result<Vec<BashPatternWatchRow>> {
    let mut statement = conn.prepare("SELECT session_id, task_id, watch_id, batch FROM bash_watch_delivery_gaps WHERE harness=?1 AND (?2 IS NULL OR session_id=?2)")?;
    let rows = statement.query_map(params![harness, session], |row| {
        let session_id = row.get(0)?;
        let task_id: String = row.get(1)?;
        let watch_id = row.get(2)?;
        let saved: String = row.get(3)?;
        let batch = serde_json::from_str::<crate::bash_background::watch_lines::LineBatch>(&saved)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
        Ok(BashPatternWatchRow {
            harness: harness.into(),
            session_id,
            task_id: task_id.clone(),
            watch_id,
            pattern_kind: "substring".into(),
            pattern: String::new(),
            once: true,
            each_line: false,
            match_batch: Some(saved),
            created_at: 0,
            stdout_offset: 0,
            stderr_offset: 0,
            pty_offset: 0,
            scanning: false,
            pending_match: true,
            match_text: Some("watch delivery gap".into()),
            match_offset: batch.version.map(|version| version as i64),
            match_context: Some(crate::subc_format::format_watch_line_batch(
                &task_id, &batch,
            )),
        })
    })?;
    rows.collect()
}

pub(crate) fn upsert_delivery_gap(
    conn: &Connection,
    row: &BashPatternWatchRow,
) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO bash_watch_delivery_gaps(harness,session_id,task_id,watch_id,batch) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(harness,session_id,task_id,watch_id) DO UPDATE SET batch=excluded.batch", params![row.harness,row.session_id,row.task_id,row.watch_id,row.match_batch])?;
    refresh_watch_gap_health(conn);
    Ok(())
}

pub(crate) fn delete_delivery_gap(
    conn: &Connection,
    harness: &str,
    session: &str,
    task: &str,
    watch: &str,
) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM bash_watch_delivery_gaps WHERE harness=?1 AND session_id=?2 AND task_id=?3 AND watch_id=?4", params![harness,session,task,watch])?;
    refresh_watch_gap_health(conn);
    Ok(())
}

#[cfg(test)]
mod delivery_ack_tests {
    use super::*;

    #[test]
    fn retry_overflow_ack_tombstones_survive_a_long_lived_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        acknowledge_delivery(&conn, "opencode", "session", "task", "live", 5, None).unwrap();
        conn.execute(
            "UPDATE bash_watch_delivery_acks SET acknowledged_at=unixepoch()-2592001",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO bash_watch_delivery_acks VALUES('opencode','session','task','old',5,unixepoch()-2592001,'earlier-daemon',0)", []).unwrap();
        acknowledge_delivery(&conn, "opencode", "session", "task", "another", 6, None).unwrap();
        assert_eq!(
            delivery_ack_version(&conn, "opencode", "session", "task", "live")
                .unwrap()
                .unwrap()
                .0,
            5
        );
        assert!(
            delivery_ack_version(&conn, "opencode", "session", "task", "old")
                .unwrap()
                .is_none()
        );
    }
}
