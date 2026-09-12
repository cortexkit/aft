use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use rusqlite::{params, Connection};

pub struct CompressionEventRow<'a> {
    pub harness: &'a str,
    pub session_id: Option<&'a str>,
    pub project_key: &'a str,
    pub tool: &'a str,
    pub task_id: Option<&'a str>,
    pub command: Option<&'a str>,
    pub compressor: &'a str,
    pub original_bytes: i64,
    pub compressed_bytes: i64,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CompressionAggregate {
    pub events: u64,
    pub original_tokens: u64,
    pub compressed_tokens: u64,
}

impl CompressionAggregate {
    pub fn savings_tokens(&self) -> u64 {
        self.original_tokens.saturating_sub(self.compressed_tokens)
    }

    fn add_event(&mut self, row: &CompressionEventRow<'_>) {
        self.events = self.events.saturating_add(1);
        self.original_tokens = self
            .original_tokens
            .saturating_add(u64::from(row.original_tokens));
        self.compressed_tokens = self
            .compressed_tokens
            .saturating_add(u64::from(row.compressed_tokens));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectAggregateKey {
    harness: String,
    project_key: String,
}

impl ProjectAggregateKey {
    fn new(harness: &str, project_key: &str) -> Self {
        Self {
            harness: harness.to_string(),
            project_key: project_key.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionAggregateKey {
    project: ProjectAggregateKey,
    session_id: String,
}

impl SessionAggregateKey {
    fn new(harness: &str, project_key: &str, session_id: &str) -> Self {
        Self {
            project: ProjectAggregateKey::new(harness, project_key),
            session_id: session_id.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CachedAggregate {
    aggregate: CompressionAggregate,
    watermark: i64,
}

#[derive(Debug, Default)]
struct CompressionAggregateCacheInner {
    connection_identity: Option<usize>,
    projects: HashMap<ProjectAggregateKey, CachedAggregate>,
    sessions: HashMap<SessionAggregateKey, CachedAggregate>,
}

/// Process-local compression totals backed by the durable event table.
///
/// Status reads validate entries with the table's maximum row id, an indexed
/// lookup that detects writes from other AFT processes. Full aggregate scans run
/// only for a cold or stale key. Successful local inserts advance warm entries
/// directly while the caller still owns the database connection mutex.
#[derive(Debug, Default)]
pub struct CompressionAggregateCache {
    inner: Mutex<CompressionAggregateCacheInner>,
    #[cfg(test)]
    aggregate_scan_count: AtomicUsize,
}

impl CompressionAggregateCache {
    pub fn aggregates_for_session(
        &self,
        conn: &Connection,
        harness: &str,
        project_key: &str,
        session_id: &str,
    ) -> rusqlite::Result<(CompressionAggregate, CompressionAggregate)> {
        let watermark = compression_event_watermark(conn)?;
        let project_key = ProjectAggregateKey::new(harness, project_key);
        let session_key = SessionAggregateKey::new(harness, &project_key.project_key, session_id);
        let mut inner = self.inner.lock();
        reset_for_connection_change(&mut inner, conn);

        let project = match inner.projects.get(&project_key) {
            Some(cached) if cached.watermark == watermark => cached.aggregate,
            _ => {
                self.note_aggregate_scan();
                let aggregate = aggregate_for_project(conn, harness, &project_key.project_key)?;
                inner.projects.insert(
                    project_key.clone(),
                    CachedAggregate {
                        aggregate,
                        watermark,
                    },
                );
                aggregate
            }
        };

        let session = match inner.sessions.get(&session_key) {
            Some(cached) if cached.watermark == watermark => cached.aggregate,
            _ => {
                self.note_aggregate_scan();
                let aggregate =
                    aggregate_for_session(conn, harness, &project_key.project_key, session_id)?;
                inner.sessions.insert(
                    session_key,
                    CachedAggregate {
                        aggregate,
                        watermark,
                    },
                );
                aggregate
            }
        };

        Ok((project, session))
    }

    /// Apply a row that was inserted successfully on `conn`.
    ///
    /// A warm entry is advanced only when its watermark matches the row that
    /// immediately preceded `inserted_row_id`. If another process wrote first,
    /// the entry remains stale and the next status read rebuilds it from SQL.
    pub fn record_successful_insert(
        &self,
        conn: &Connection,
        row: &CompressionEventRow<'_>,
        inserted_row_id: i64,
    ) {
        let previous_watermark = compression_event_watermark_before(conn, inserted_row_id);
        let project_key = ProjectAggregateKey::new(row.harness, row.project_key);
        let session_key = row
            .session_id
            .map(|session_id| SessionAggregateKey::new(row.harness, row.project_key, session_id));
        let mut inner = self.inner.lock();
        reset_for_connection_change(&mut inner, conn);

        let Ok(previous_watermark) = previous_watermark else {
            *inner = CompressionAggregateCacheInner {
                connection_identity: inner.connection_identity,
                ..CompressionAggregateCacheInner::default()
            };
            return;
        };

        for (key, cached) in &mut inner.projects {
            if cached.watermark != previous_watermark {
                continue;
            }
            if key == &project_key {
                cached.aggregate.add_event(row);
            }
            cached.watermark = inserted_row_id;
        }
        for (key, cached) in &mut inner.sessions {
            if cached.watermark != previous_watermark {
                continue;
            }
            if session_key.as_ref() == Some(key) {
                cached.aggregate.add_event(row);
            }
            cached.watermark = inserted_row_id;
        }
    }

    pub fn clear(&self) {
        *self.inner.lock() = CompressionAggregateCacheInner::default();
    }

    #[cfg(test)]
    fn aggregate_scan_count_for_test(&self) -> usize {
        self.aggregate_scan_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn note_aggregate_scan(&self) {
        self.aggregate_scan_count.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(not(test))]
    fn note_aggregate_scan(&self) {}
}

/// Insert one event and return its row id. Duplicate identities are ignored and
/// return `None`, allowing in-process aggregates to advance only for durable rows.
pub fn insert_compression_event(
    conn: &Connection,
    row: &CompressionEventRow<'_>,
) -> rusqlite::Result<Option<i64>> {
    let inserted = conn.execute(
        r#"
        INSERT OR IGNORE INTO compression_events (
            harness, session_id, project_key, tool, task_id, command, compressor,
            original_bytes, compressed_bytes, original_tokens, compressed_tokens, created_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            row.harness,
            row.session_id,
            row.project_key,
            row.tool,
            row.task_id,
            row.command,
            row.compressor,
            row.original_bytes,
            row.compressed_bytes,
            row.original_tokens,
            row.compressed_tokens,
            row.created_at,
        ],
    )?;
    Ok((inserted > 0).then(|| conn.last_insert_rowid()))
}

pub fn aggregate_for_project(
    conn: &Connection,
    harness: &str,
    project_key: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT SUM(events), SUM(original), SUM(compressed) FROM (
            SELECT COUNT(*) AS events,
                   COALESCE(SUM(original_tokens), 0) AS original,
                   COALESCE(SUM(compressed_tokens), 0) AS compressed
            FROM compression_events WHERE harness = ?1 AND project_key = ?2
            UNION ALL
            SELECT events, original_tokens, compressed_tokens
            FROM compression_event_rollups WHERE harness = ?1 AND project_key = ?2
        )
        "#,
        params![harness, project_key],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}

pub fn aggregate_for_session(
    conn: &Connection,
    harness: &str,
    project_key: &str,
    session_id: &str,
) -> rusqlite::Result<CompressionAggregate> {
    conn.query_row(
        r#"
        SELECT SUM(events), SUM(original), SUM(compressed) FROM (
            SELECT COUNT(*) AS events,
                   COALESCE(SUM(original_tokens), 0) AS original,
                   COALESCE(SUM(compressed_tokens), 0) AS compressed
            FROM compression_events
            WHERE harness = ?1 AND project_key = ?2 AND session_id = ?3
            UNION ALL
            SELECT events, original_tokens, compressed_tokens
            FROM compression_event_rollups
            WHERE harness = ?1 AND project_key = ?2 AND session_is_null = 0 AND session_id = ?3
        )
        "#,
        params![harness, project_key, session_id],
        |row| {
            Ok(CompressionAggregate {
                events: row.get::<_, i64>(0)? as u64,
                original_tokens: row.get::<_, i64>(1)? as u64,
                compressed_tokens: row.get::<_, i64>(2)? as u64,
            })
        },
    )
}

fn reset_for_connection_change(inner: &mut CompressionAggregateCacheInner, conn: &Connection) {
    let identity = conn as *const Connection as usize;
    if inner.connection_identity != Some(identity) {
        *inner = CompressionAggregateCacheInner {
            connection_identity: Some(identity),
            ..CompressionAggregateCacheInner::default()
        };
    }
}

fn compression_event_watermark(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM compression_events",
        [],
        |row| row.get(0),
    )
}

fn compression_event_watermark_before(
    conn: &Connection,
    inserted_row_id: i64,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM compression_events WHERE id < ?1",
        [inserted_row_id],
        |row| row.get(0),
    )
}

/// Raw history is kept for thirty days; lifetime counters survive in rollups.
pub const RETENTION_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const RETENTION_BATCH: i64 = 500;

const RETENTION_CANDIDATES: &str = "
    SELECT id, created_at, harness, project_key, session_id, original_tokens, compressed_tokens,
           task_id IS NOT NULL AND EXISTS (
               SELECT 1 FROM bash_tasks b
               WHERE b.harness = e.harness AND b.session_id IS e.session_id AND b.task_id = e.task_id
                 AND b.status NOT IN ('completed', 'failed', 'killed', 'timed_out', 'fate_unknown')
           )
    FROM compression_events e
    WHERE (created_at, id) > (?1, ?2) AND created_at < ?3
    ORDER BY created_at, id LIMIT ?4";

/// Fold and remove at most 500 old events in one atomic transaction.
///
/// The cursor bounds rows examined as well as rows deleted, so long-running
/// tasks cannot make every sweep rescan the same protected history. It wraps
/// after the last old row. The highest event ID stays raw to preserve the warm
/// aggregate cache's insertion watermark. Live tasks retain their identities
/// because they can still emit compression events; completed history outside
/// the retention window no longer participates in duplicate suppression.
pub fn prune_compression_events(conn: &mut Connection, now_ms: i64) -> rusqlite::Result<usize> {
    use rusqlite::{OptionalExtension, TransactionBehavior};
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (created_at, event_id) = tx
        .query_row(
            "SELECT created_at, event_id FROM compression_retention_cursor WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or((i64::MIN, 0));
    let max_id = compression_event_watermark(&tx)?;
    let candidates = tx
        .prepare(RETENTION_CANDIDATES)?
        .query_map(
            params![
                created_at,
                event_id,
                now_ms.saturating_sub(RETENTION_AGE_MS),
                RETENTION_BATCH
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut folded: HashMap<(String, String, Option<String>), (i64, i64, i64)> = HashMap::new();
    let mut deleted = 0;
    for (id, _, harness, project, session, original, compressed, live) in &candidates {
        if *live || *id == max_id {
            continue;
        }
        let totals = folded
            .entry((harness.clone(), project.clone(), session.clone()))
            .or_default();
        totals.0 += 1;
        totals.1 += original;
        totals.2 += compressed;
        deleted += tx.execute("DELETE FROM compression_events WHERE id = ?1", [id])?;
    }
    for ((harness, project, session), (events, original, compressed)) in folded {
        tx.execute(
            "INSERT INTO compression_event_rollups
             (harness, project_key, session_is_null, session_id, events, original_tokens, compressed_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(harness, project_key, session_is_null, session_id) DO UPDATE SET
               events = events + excluded.events,
               original_tokens = original_tokens + excluded.original_tokens,
               compressed_tokens = compressed_tokens + excluded.compressed_tokens",
            params![harness, project, session.is_none(), session.unwrap_or_default(), events, original, compressed],
        )?;
    }
    let (next_created, next_id) = candidates
        .last()
        .map(|row| (row.1, row.0))
        .unwrap_or((i64::MIN, 0));
    tx.execute(
        "INSERT INTO compression_retention_cursor VALUES (1, ?1, ?2)
         ON CONFLICT(singleton) DO UPDATE SET created_at = excluded.created_at, event_id = excluded.event_id",
        params![next_created, next_id],
    )?;
    tx.commit()?;
    Ok(deleted)
}

/// Schedule bounded retention away from the daemon and standalone request loops.
/// A process can have only one pass in flight and attempts at most once a minute.
pub fn maybe_spawn_retention(
    db: Option<std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>>,
) {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    };
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    let Some(db) = db else {
        return;
    };
    let mut last = LAST.get_or_init(|| Mutex::new(None)).lock();
    if last.is_some_and(|value| value.elapsed() < Duration::from_secs(60))
        || IN_FLIGHT.swap(true, Ordering::AcqRel)
    {
        return;
    }
    *last = Some(Instant::now());
    if let Err(error) = std::thread::Builder::new()
        .name("aft-compression-retention".into())
        .spawn(move || {
            if let Ok(mut conn) = db.try_lock() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                match prune_compression_events(&mut conn, i64::try_from(now).unwrap_or(i64::MAX)) {
                    Ok(0) => {}
                    Ok(rows) => {
                        crate::slog_info!("compression retention: folded {} raw events", rows)
                    }
                    Err(error) => crate::slog_warn!("compression retention failed: {}", error),
                }
            }
            IN_FLIGHT.store(false, Ordering::Release);
        })
    {
        IN_FLIGHT.store(false, Ordering::Release);
        crate::slog_warn!("compression retention worker failed: {}", error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn retention_preserves_lifetime_totals_and_live_task_identity() {
        let dir = tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let now = RETENTION_AGE_MS + 100;
        for index in 0..510 {
            let task = format!("task-{index}");
            let mut event = row(
                if index % 2 == 0 {
                    "project-a"
                } else {
                    "project-b"
                },
                &task,
                100,
                40,
                1,
            );
            event.session_id = match index % 3 {
                0 => None,
                1 => Some(""),
                _ => Some("session-1"),
            };
            insert_compression_event(&conn, &event).unwrap();
        }
        conn.execute("INSERT INTO bash_tasks (harness, session_id, task_id, project_key, command, cwd, status, started_at)
            VALUES ('opencode', 'session-1', 'task-2', 'project-a', 'sleep', '.', 'running', 1)", []).unwrap();
        let recent = row("project-a", "recent", 17, 9, 100);
        insert_compression_event(&conn, &recent).unwrap();
        let cache = CompressionAggregateCache::default();
        let before = ["project-a", "project-b"].map(|project| {
            (
                aggregate_for_project(&conn, "opencode", project).unwrap(),
                aggregate_for_session(&conn, "opencode", project, "session-1").unwrap(),
                aggregate_for_session(&conn, "opencode", project, "").unwrap(),
            )
        });
        let warm = cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-1")
            .unwrap();
        assert_eq!(prune_compression_events(&mut conn, now).unwrap(), 499);
        assert_eq!(prune_compression_events(&mut conn, now).unwrap(), 10);
        assert_eq!(prune_compression_events(&mut conn, now).unwrap(), 0);
        for (index, project) in ["project-a", "project-b"].iter().enumerate() {
            assert_eq!(
                aggregate_for_project(&conn, "opencode", project).unwrap(),
                before[index].0
            );
            assert_eq!(
                aggregate_for_session(&conn, "opencode", project, "session-1").unwrap(),
                before[index].1
            );
            assert_eq!(
                aggregate_for_session(&conn, "opencode", project, "").unwrap(),
                before[index].2
            );
        }
        assert_eq!(
            cache
                .aggregates_for_session(&conn, "opencode", "project-a", "session-1")
                .unwrap(),
            warm
        );
        assert!(insert_compression_event(&conn, &recent).unwrap().is_none());
        assert!(
            insert_compression_event(&conn, &row("project-a", "task-2", 100, 40, 1))
                .unwrap()
                .is_none()
        );
        conn.execute("UPDATE bash_tasks SET status = 'completed'", [])
            .unwrap();
        assert_eq!(prune_compression_events(&mut conn, now).unwrap(), 1);
        assert_eq!(
            aggregate_for_project(&conn, "opencode", "project-a").unwrap(),
            before[0].0
        );
        let next = row("project-a", "next", 20, 10, now);
        let id = insert_compression_event(&conn, &next).unwrap().unwrap();
        cache.record_successful_insert(&conn, &next, id);
        let totals = cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-1")
            .unwrap();
        assert_eq!(
            totals.0,
            aggregate_for_project(&conn, "opencode", "project-a").unwrap()
        );
        assert_eq!(
            totals.1,
            aggregate_for_session(&conn, "opencode", "project-a", "session-1").unwrap()
        );
    }

    #[test]
    fn retention_rollup_failure_rolls_back_raw_deletes_and_cursor() {
        let dir = tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_compression_event(&conn, &row("project-a", "old", 100, 40, 1)).unwrap();
        insert_compression_event(&conn, &row("project-a", "watermark", 100, 40, 1)).unwrap();
        conn.execute_batch("CREATE TRIGGER reject_fold BEFORE INSERT ON compression_event_rollups BEGIN SELECT RAISE(ABORT, 'fold failure'); END;").unwrap();
        assert!(prune_compression_events(&mut conn, RETENTION_AGE_MS + 100).is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM compression_events", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM compression_retention_cursor",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn retention_selection_is_indexed_and_keeps_the_watermark() {
        let dir = tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_compression_event(&conn, &row("project-a", "watermark", 100, 40, 1)).unwrap();
        let plan = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {RETENTION_CANDIDATES}"))
            .unwrap()
            .query_map(
                params![i64::MIN, 0, RETENTION_AGE_MS, RETENTION_BATCH],
                |r| r.get::<_, String>(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_compression_created"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
        assert_eq!(
            prune_compression_events(&mut conn, RETENTION_AGE_MS + 100).unwrap(),
            0
        );
        assert_eq!(compression_event_watermark(&conn).unwrap(), 1);
    }

    #[test]
    fn duplicate_identity_is_ignored_without_cross_project_suppression() {
        let dir = tempdir().expect("tempdir");
        let conn = crate::db::open(&dir.path().join("aft.db")).expect("open db");

        assert!(
            insert_compression_event(&conn, &row("project-a", "task-1", 100, 40, 1))
                .expect("insert first")
                .is_some()
        );
        assert!(
            insert_compression_event(&conn, &row("project-a", "task-1", 900, 10, 2))
                .expect("ignore duplicate")
                .is_none()
        );
        assert!(
            insert_compression_event(&conn, &row("project-b", "task-1", 200, 80, 3))
                .expect("insert same task id for other project")
                .is_some()
        );

        let project_a = aggregate_for_project(&conn, "opencode", "project-a").unwrap();
        assert_eq!(project_a.events, 1);
        assert_eq!(project_a.original_tokens, 100);
        assert_eq!(project_a.compressed_tokens, 40);

        let project_b = aggregate_for_project(&conn, "opencode", "project-b").unwrap();
        assert_eq!(project_b.events, 1);
        assert_eq!(project_b.original_tokens, 200);
        assert_eq!(project_b.compressed_tokens, 80);
    }

    #[test]
    fn cached_aggregates_match_sql_after_generated_inserts_and_duplicates() {
        let dir = tempdir().expect("tempdir");
        let conn = crate::db::open(&dir.path().join("aft.db")).expect("open db");
        let cache = CompressionAggregateCache::default();
        let (project, session) = cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-1")
            .expect("warm cache");
        assert_eq!(project, CompressionAggregate::default());
        assert_eq!(session, CompressionAggregate::default());
        cache
            .aggregates_for_session(&conn, "opencode", "project-a", "session-2")
            .expect("warm sibling session");
        cache
            .aggregates_for_session(&conn, "opencode", "project-b", "session-1")
            .expect("warm sibling project");
        assert_eq!(cache.aggregate_scan_count_for_test(), 5);

        let mut previous_task = String::new();
        for index in 0..64u32 {
            let task_id = if index % 5 == 4 {
                previous_task.clone()
            } else {
                let task_id = format!("task-{index}");
                previous_task = task_id.clone();
                task_id
            };
            let row = row(
                "project-a",
                &task_id,
                100 + index,
                40 + (index % 17),
                i64::from(index),
            );
            if let Some(row_id) = insert_compression_event(&conn, &row).expect("insert event") {
                cache.record_successful_insert(&conn, &row, row_id);
            }

            for (project_key, session_id) in [
                ("project-a", "session-1"),
                ("project-a", "session-2"),
                ("project-b", "session-1"),
            ] {
                let cached = cache
                    .aggregates_for_session(&conn, "opencode", project_key, session_id)
                    .expect("read cache");
                let scanned = (
                    aggregate_for_project(&conn, "opencode", project_key).expect("scan project"),
                    aggregate_for_session(&conn, "opencode", project_key, session_id)
                        .expect("scan session"),
                );
                assert_eq!(cached, scanned, "aggregate mismatch after step {index}");
            }
            assert_eq!(
                cache.aggregate_scan_count_for_test(),
                5,
                "local inserts must advance warm entries without rescanning"
            );
        }
    }

    fn row<'a>(
        project_key: &'a str,
        task_id: &'a str,
        original_tokens: u32,
        compressed_tokens: u32,
        created_at: i64,
    ) -> CompressionEventRow<'a> {
        CompressionEventRow {
            harness: "opencode",
            session_id: Some("session-1"),
            project_key,
            tool: "bash",
            task_id: Some(task_id),
            command: Some("echo ok"),
            compressor: "zstd",
            original_bytes: i64::from(original_tokens) * 4,
            compressed_bytes: i64::from(compressed_tokens) * 4,
            original_tokens,
            compressed_tokens,
            created_at,
        }
    }
}
