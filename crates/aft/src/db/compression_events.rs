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
// Production selects, stats, and mutates at most 250 rows per transaction;
// filesystem checks still run after releasing the shared database mutex.
pub const BASH_TASK_STEADY_STATE_ROWS: usize = 250;
const RETENTION_LOCK_BUDGET_MICROS: u128 = 100_000;
// A representative one-row retention sweep measured 7 ms in the mutation lock
// (and under 1 ms in each count/selection lock). Allow 10 ms for that contention,
// still well below the 100 ms maximum hold for a retention transaction.
const RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS: u128 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionSweepSkipReason {
    OpeningCountLockBudgetExhausted,
    ClosingCountLockBudgetExhausted,
}

impl RetentionSweepSkipReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpeningCountLockBudgetExhausted => "opening_count_lock_budget_exhausted",
            Self::ClosingCountLockBudgetExhausted => "closing_count_lock_budget_exhausted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSweepSkip {
    pub reason: RetentionSweepSkipReason,
    pub attempts: usize,
    pub waited_micros: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionSweepOutcome {
    Completed(RetentionSweep),
    Skipped(RetentionSweepSkip),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionTick {
    pub bash_tasks: crate::db::bash_tasks::TerminalRowsPrune,
    pub compression_events_removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPhaseTimings {
    pub selection_lock_micros: u128,
    pub stat_micros: u128,
    pub task_delete_micros: u128,
    pub event_prune_micros: u128,
    pub commit_micros: u128,
    pub mutation_lock_micros: u128,
}

impl RetentionPhaseTimings {
    pub fn total_lock_micros(self) -> u128 {
        self.selection_lock_micros
            .saturating_add(self.mutation_lock_micros)
    }

    pub fn worst_lock_micros(self) -> u128 {
        self.selection_lock_micros.max(self.mutation_lock_micros)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPass {
    pub tick: RetentionTick,
    pub timings: RetentionPhaseTimings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSweep {
    pub initial_eligible_rows: usize,
    pub remaining_eligible_rows: Option<usize>,
    pub count_skip: Option<RetentionSweepSkip>,
    pub row_ceiling: usize,
    pub passes: usize,
    pub bash_tasks_removed: usize,
    pub compression_events_removed: usize,
    pub worst_count_lock_micros: u128,
    pub worst_selection_lock_micros: u128,
    pub worst_mutation_lock_micros: u128,
    pub worst_lock_micros: u128,
    pub elapsed_micros: u128,
}

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
    use rusqlite::TransactionBehavior;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let deleted = prune_compression_events_in_transaction(&tx, now_ms)?;
    tx.commit()?;
    Ok(deleted)
}

fn prune_compression_events_in_transaction(
    conn: &Connection,
    now_ms: i64,
) -> rusqlite::Result<usize> {
    use rusqlite::OptionalExtension;
    let (created_at, event_id) = conn
        .query_row(
            "SELECT created_at, event_id FROM compression_retention_cursor WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .unwrap_or((i64::MIN, 0));
    let max_id = compression_event_watermark(conn)?;
    let candidates = conn
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
        deleted += conn.execute("DELETE FROM compression_events WHERE id = ?1", [id])?;
    }
    for ((harness, project, session), (events, original, compressed)) in folded {
        conn.execute(
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
    conn.execute(
        "INSERT INTO compression_retention_cursor VALUES (1, ?1, ?2)
         ON CONFLICT(singleton) DO UPDATE SET created_at = excluded.created_at, event_id = excluded.event_id",
        params![next_created, next_id],
    )?;
    Ok(deleted)
}

pub fn prune_retention_tick(conn: &mut Connection, now_ms: i64) -> rusqlite::Result<RetentionTick> {
    let plan = crate::db::bash_tasks::select_terminal_prune_candidates(conn, now_ms, 500)?;
    let prepared = crate::db::bash_tasks::prepare_terminal_prune(plan, |_| false);
    apply_prepared_retention_tick(conn, now_ms, prepared)
}

struct RetentionMutation {
    tick: RetentionTick,
    task_delete_micros: u128,
    event_prune_micros: u128,
    commit_micros: u128,
}

fn apply_prepared_retention_tick(
    conn: &mut Connection,
    now_ms: i64,
    prepared: crate::db::bash_tasks::PreparedTerminalPrune,
) -> rusqlite::Result<RetentionTick> {
    apply_prepared_retention_tick_timed(conn, now_ms, prepared).map(|mutation| mutation.tick)
}

fn apply_prepared_retention_tick_timed(
    conn: &mut Connection,
    now_ms: i64,
    prepared: crate::db::bash_tasks::PreparedTerminalPrune,
) -> rusqlite::Result<RetentionMutation> {
    use rusqlite::TransactionBehavior;
    use std::time::Instant;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Task rows go first so the event pass in this transaction observes their
    // final liveness, while the commit publishes both retention decisions at once.
    let task_delete_started = Instant::now();
    let bash_tasks = crate::db::bash_tasks::delete_prepared_terminal_rows(&tx, prepared)?;
    let task_delete_micros = task_delete_started.elapsed().as_micros();
    let event_prune_started = Instant::now();
    let compression_events_removed = prune_compression_events_in_transaction(&tx, now_ms)?;
    let event_prune_micros = event_prune_started.elapsed().as_micros();
    let commit_started = Instant::now();
    tx.commit()?;
    let commit_micros = commit_started.elapsed().as_micros();
    Ok(RetentionMutation {
        tick: RetentionTick {
            bash_tasks,
            compression_events_removed,
        },
        task_delete_micros,
        event_prune_micros,
        commit_micros,
    })
}

pub fn prune_retention_once(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[crate::bash_background::BgTaskRegistry]>,
) -> Result<Option<RetentionPass>, String> {
    prune_retention_once_observed(db, now_ms, registries, || {})
}

fn prune_retention_once_observed(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[crate::bash_background::BgTaskRegistry]>,
    observe_stat_phase: impl FnOnce(),
) -> Result<Option<RetentionPass>, String> {
    use std::sync::TryLockError;
    use std::time::Instant;

    let selection_started = Instant::now();
    let (path, in_memory_plan, selection_lock_micros) = {
        let conn = match db.try_lock() {
            Ok(conn) => conn,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => {
                return Err("retention database mutex poisoned".to_string())
            }
        };
        let path = conn.path().map(std::path::PathBuf::from);
        let plan = if path.is_none() {
            Some(
                crate::db::bash_tasks::select_terminal_prune_candidates(
                    &conn,
                    now_ms,
                    BASH_TASK_STEADY_STATE_ROWS,
                )
                .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        (path, plan, selection_started.elapsed().as_micros())
    };
    let plan = if let Some(plan) = in_memory_plan {
        plan
    } else {
        let conn = crate::db::open_readonly(path.as_deref().expect("checked database path"))
            .map_err(|error| error.to_string())?;
        crate::db::bash_tasks::select_terminal_prune_candidates(
            &conn,
            now_ms,
            BASH_TASK_STEADY_STATE_ROWS,
        )
        .map_err(|error| error.to_string())?
    };

    let stat_started = Instant::now();
    let mut prepared = crate::db::bash_tasks::prepare_terminal_prune_observed(
        plan,
        |task_id| {
            registries.is_none_or(|registries| {
                registries
                    .iter()
                    .any(|registry| registry.active_watch_count(task_id) > 0)
            })
        },
        observe_stat_phase,
    );
    crate::db::bash_tasks::cap_prepared_terminal_rows(&mut prepared, BASH_TASK_STEADY_STATE_ROWS);
    let stat_micros = stat_started.elapsed().as_micros();

    let mut conn = match db.try_lock() {
        Ok(conn) => conn,
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Poisoned(_)) => {
            return Err("retention database mutex poisoned".to_string())
        }
    };
    let mutation_started = Instant::now();
    let mutation = apply_prepared_retention_tick_timed(&mut conn, now_ms, prepared)
        .map_err(|error| error.to_string())?;
    drop(conn);
    let mutation_lock_micros = mutation_started.elapsed().as_micros();

    Ok(Some(RetentionPass {
        tick: mutation.tick,
        timings: RetentionPhaseTimings {
            selection_lock_micros,
            stat_micros,
            task_delete_micros: mutation.task_delete_micros,
            event_prune_micros: mutation.event_prune_micros,
            commit_micros: mutation.commit_micros,
            mutation_lock_micros,
        },
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionCountPhase {
    Opening,
    Closing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EligibleTerminalRows {
    Count {
        rows: usize,
        lock_micros: u128,
    },
    LockBudgetExhausted {
        attempts: usize,
        waited_micros: u128,
    },
}

enum RetentionCountLock<'a> {
    Acquired(std::sync::MutexGuard<'a, crate::db::TrackedConnection>),
    BudgetExhausted {
        attempts: usize,
        waited_micros: u128,
    },
}

fn try_retention_count_lock_observed<'a>(
    db: &'a std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    phase: RetentionCountPhase,
    observe_attempt: &mut impl FnMut(RetentionCountPhase, usize),
) -> Result<RetentionCountLock<'a>, String> {
    use std::sync::TryLockError;
    use std::time::Instant;

    let started = Instant::now();
    let mut attempts = 0usize;
    loop {
        if attempts > 0 {
            let elapsed = started.elapsed();
            let budget = std::time::Duration::from_micros(RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS as u64);
            if elapsed >= budget {
                return Ok(RetentionCountLock::BudgetExhausted {
                    attempts,
                    waited_micros: elapsed.as_micros(),
                });
            }
            let backoff = std::time::Duration::from_micros((50 * attempts.min(4)) as u64);
            std::thread::sleep(backoff.min(budget - elapsed));
        }

        attempts = attempts.saturating_add(1);
        observe_attempt(phase, attempts);
        match db.try_lock() {
            Ok(conn) => return Ok(RetentionCountLock::Acquired(conn)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Poisoned(_)) => {
                return Err("retention database mutex poisoned".to_string())
            }
        }
    }
}

fn eligible_terminal_rows(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
) -> Result<EligibleTerminalRows, String> {
    eligible_terminal_rows_observed(db, now_ms, RetentionCountPhase::Opening, &mut |_, _| {})
}

fn eligible_terminal_rows_observed(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
    phase: RetentionCountPhase,
    observe_attempt: &mut impl FnMut(RetentionCountPhase, usize),
) -> Result<EligibleTerminalRows, String> {
    use std::time::Instant;

    let conn = match try_retention_count_lock_observed(db, phase, observe_attempt)? {
        RetentionCountLock::Acquired(conn) => conn,
        RetentionCountLock::BudgetExhausted {
            attempts,
            waited_micros,
        } => {
            return Ok(EligibleTerminalRows::LockBudgetExhausted {
                attempts,
                waited_micros,
            })
        }
    };
    let lock_started = Instant::now();
    let path = conn.path().map(std::path::PathBuf::from);
    let rows_without_path = if path.is_none() {
        Some(
            crate::db::bash_tasks::terminal_rows_eligible_count(&conn, now_ms)
                .map_err(|error| error.to_string())?,
        )
    } else {
        None
    };
    drop(conn);
    let lock_micros = lock_started.elapsed().as_micros();
    let rows = if let Some(rows) = rows_without_path {
        rows
    } else {
        let conn = crate::db::open_readonly(path.as_deref().expect("checked database path"))
            .map_err(|error| error.to_string())?;
        crate::db::bash_tasks::terminal_rows_eligible_count(&conn, now_ms)
            .map_err(|error| error.to_string())?
    };
    Ok(EligibleTerminalRows::Count { rows, lock_micros })
}

fn retention_db_key(db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>) -> usize {
    std::sync::Arc::as_ptr(db) as usize
}

fn retention_skip_reasons() -> &'static Mutex<HashMap<usize, RetentionSweepSkipReason>> {
    static SKIPS: std::sync::OnceLock<Mutex<HashMap<usize, RetentionSweepSkipReason>>> =
        std::sync::OnceLock::new();
    SKIPS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_retention_sweep_skip(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    reason: Option<RetentionSweepSkipReason>,
) {
    let mut skips = retention_skip_reasons().lock();
    match reason {
        Some(reason) => {
            skips.insert(retention_db_key(db), reason);
        }
        None => {
            skips.remove(&retention_db_key(db));
        }
    }
}

pub fn last_retention_sweep_skip_reason(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
) -> Option<RetentionSweepSkipReason> {
    retention_skip_reasons()
        .lock()
        .get(&retention_db_key(db))
        .copied()
}

/// Use a separate read-only connection when the shared connection has a database file path;
/// retry only up to the configured time budget for counting retention-eligible rows, and hold the
/// shared mutex only long enough to copy that path.
pub fn terminal_rows_eligible_count_nonblocking(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
) -> Result<Option<usize>, String> {
    eligible_terminal_rows(db, now_ms).map(|count| match count {
        EligibleTerminalRows::Count { rows, .. } => Some(rows),
        EligibleTerminalRows::LockBudgetExhausted { .. } => None,
    })
}

fn retention_row_ceiling(eligible_rows: usize) -> usize {
    if eligible_rows > BASH_TASK_STEADY_STATE_ROWS {
        eligible_rows
    } else {
        BASH_TASK_STEADY_STATE_ROWS
    }
}

/// Drain a retention backlog through independently committed, bounded transactions.
/// Contention between batches stops the sweep so interactive work always wins the mutex.
pub fn prune_retention_sweep(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[crate::bash_background::BgTaskRegistry]>,
) -> Result<RetentionSweepOutcome, String> {
    prune_retention_sweep_observed(db, now_ms, registries, |_, _| {})
}

fn prune_retention_sweep_observed(
    db: &std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>,
    now_ms: i64,
    registries: Option<&[crate::bash_background::BgTaskRegistry]>,
    mut observe_count_attempt: impl FnMut(RetentionCountPhase, usize),
) -> Result<RetentionSweepOutcome, String> {
    use std::time::Instant;

    let sweep_started = Instant::now();
    let (initial_eligible_rows, initial_count_lock_micros) = match eligible_terminal_rows_observed(
        db,
        now_ms,
        RetentionCountPhase::Opening,
        &mut observe_count_attempt,
    )? {
        EligibleTerminalRows::Count { rows, lock_micros } => (rows, lock_micros),
        EligibleTerminalRows::LockBudgetExhausted {
            attempts,
            waited_micros,
        } => {
            let skip = RetentionSweepSkip {
                reason: RetentionSweepSkipReason::OpeningCountLockBudgetExhausted,
                attempts,
                waited_micros,
            };
            record_retention_sweep_skip(db, Some(skip.reason));
            return Ok(RetentionSweepOutcome::Skipped(skip));
        }
    };
    let row_ceiling = retention_row_ceiling(initial_eligible_rows);
    let catch_up = initial_eligible_rows > BASH_TASK_STEADY_STATE_ROWS;
    let mut passes = 0usize;
    let mut bash_tasks_removed = 0usize;
    let mut compression_events_removed = 0usize;
    let mut worst_count_lock_micros = initial_count_lock_micros;
    let mut worst_selection_lock_micros = 0u128;
    let mut worst_mutation_lock_micros = 0u128;
    let mut worst_lock_micros = initial_count_lock_micros;

    loop {
        let Some(pass) = prune_retention_once(db, now_ms, registries)? else {
            break;
        };
        passes = passes.saturating_add(1);
        bash_tasks_removed = bash_tasks_removed.saturating_add(pass.tick.bash_tasks.removed);
        compression_events_removed =
            compression_events_removed.saturating_add(pass.tick.compression_events_removed);
        worst_selection_lock_micros =
            worst_selection_lock_micros.max(pass.timings.selection_lock_micros);
        worst_mutation_lock_micros =
            worst_mutation_lock_micros.max(pass.timings.mutation_lock_micros);
        worst_lock_micros = worst_lock_micros.max(pass.timings.worst_lock_micros());

        if !catch_up
            || bash_tasks_removed >= row_ceiling
            || pass.tick.bash_tasks.removed == 0
            || pass.tick.bash_tasks.remaining_candidates == 0
        {
            break;
        }
        // A waiter that arrived during filesystem checks gets an acquisition
        // opportunity before retention starts the next bounded transaction.
        std::thread::yield_now();
    }

    let (remaining_eligible_rows, count_skip) = match eligible_terminal_rows_observed(
        db,
        now_ms,
        RetentionCountPhase::Closing,
        &mut observe_count_attempt,
    )? {
        EligibleTerminalRows::Count { rows, lock_micros } => {
            worst_count_lock_micros = worst_count_lock_micros.max(lock_micros);
            worst_lock_micros = worst_lock_micros.max(lock_micros);
            (Some(rows), None)
        }
        EligibleTerminalRows::LockBudgetExhausted {
            attempts,
            waited_micros,
        } => (
            None,
            Some(RetentionSweepSkip {
                reason: RetentionSweepSkipReason::ClosingCountLockBudgetExhausted,
                attempts,
                waited_micros,
            }),
        ),
    };
    record_retention_sweep_skip(db, count_skip.map(|skip| skip.reason));

    Ok(RetentionSweepOutcome::Completed(RetentionSweep {
        initial_eligible_rows,
        remaining_eligible_rows,
        count_skip,
        row_ceiling,
        passes,
        bash_tasks_removed,
        compression_events_removed,
        worst_count_lock_micros,
        worst_selection_lock_micros,
        worst_mutation_lock_micros,
        worst_lock_micros,
        elapsed_micros: sweep_started.elapsed().as_micros(),
    }))
}

/// Schedule bounded retention away from the daemon and standalone request loops.
/// A process can have only one sweep in flight and attempts at most once a minute.
pub fn maybe_spawn_retention(
    db: Option<std::sync::Arc<std::sync::Mutex<crate::db::TrackedConnection>>>,
    registries: Option<Vec<crate::bash_background::BgTaskRegistry>>,
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
        .name("aft-retention".into())
        .spawn(move || {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            match prune_retention_sweep(
                &db,
                i64::try_from(now).unwrap_or(i64::MAX),
                registries.as_deref(),
            ) {
                Ok(RetentionSweepOutcome::Completed(sweep)) => {
                    crate::slog_info!(
                        "bash task retention: initial_eligible={} removed={} remaining_eligible={:?} passes={} row_ceiling={} worst_count_lock_us={} worst_selection_lock_us={} worst_mutation_lock_us={} worst_lock_us={} elapsed_us={}",
                        sweep.initial_eligible_rows,
                        sweep.bash_tasks_removed,
                        sweep.remaining_eligible_rows,
                        sweep.passes,
                        sweep.row_ceiling,
                        sweep.worst_count_lock_micros,
                        sweep.worst_selection_lock_micros,
                        sweep.worst_mutation_lock_micros,
                        sweep.worst_lock_micros,
                        sweep.elapsed_micros
                    );
                    if let Some(skip) = sweep.count_skip {
                        crate::slog_warn!(
                            "bash task retention count skipped: reason={} attempts={} waited_us={} retry_budget_us={}",
                            skip.reason.as_str(),
                            skip.attempts,
                            skip.waited_micros,
                            RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
                        );
                    }
                    if sweep.worst_lock_micros > RETENTION_LOCK_BUDGET_MICROS {
                        crate::slog_warn!(
                            "bash task retention lock budget exceeded: worst_lock_us={} budget_us={}",
                            sweep.worst_lock_micros,
                            RETENTION_LOCK_BUDGET_MICROS
                        );
                    }
                    if sweep.compression_events_removed > 0 {
                        crate::slog_info!(
                            "compression retention: folded {} raw events",
                            sweep.compression_events_removed
                        );
                    }
                }
                Ok(RetentionSweepOutcome::Skipped(skip)) => crate::slog_warn!(
                    "bash task retention skipped: reason={} attempts={} waited_us={} retry_budget_us={}",
                    skip.reason.as_str(),
                    skip.attempts,
                    skip.waited_micros,
                    RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
                ),
                Err(error) => crate::slog_warn!("retention failed: {}", error),
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

    fn insert_eligible_retention_task(conn: &crate::db::TrackedConnection, task_id: &str) {
        conn.execute(
            "INSERT INTO bash_tasks (
                harness, session_id, task_id, project_key, command, cwd, status,
                started_at, completed_at, completion_delivered
             ) VALUES ('opencode', 'session', ?1, 'project', 'true', '.',
                       'completed', 1, 1, 1)",
            [task_id],
        )
        .unwrap();
    }

    #[test]
    fn retention_sweep_retries_contended_opening_count() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_eligible_retention_task(&conn, "bash-0000000000000100");
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let mut held = Some(db.lock().unwrap());
        let mut opening_attempts = 0usize;

        let outcome = prune_retention_sweep_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Opening {
                    opening_attempts = opening_attempts.max(attempt);
                    if attempt == 2 {
                        drop(held.take());
                    }
                }
            },
        )
        .unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("opening contention abandoned the retention sweep: {outcome:?}");
        };

        assert!(opening_attempts >= 2);
        assert_eq!(sweep.bash_tasks_removed, 1);
        assert_eq!(sweep.remaining_eligible_rows, Some(0));
        assert_eq!(sweep.count_skip, None);
        // Lock hold times are reported, not asserted: with one row they measure
        // scheduler preemption on a loaded runner, not the sweep's batch size,
        // and a 100 ms bound failed CI on Linux while the retry worked.
        eprintln!(
            "retention opening retry contention: attempts={} worst_count_lock_us={} worst_lock_us={}",
            opening_attempts, sweep.worst_count_lock_micros, sweep.worst_lock_micros
        );
    }

    #[test]
    fn retention_sweep_waits_for_millisecond_mutex_holder() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        insert_eligible_retention_task(&conn, "bash-0000000000000100");
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder_db = db.clone();
        let holder = std::thread::spawn(move || {
            let guard = holder_db.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            std::thread::sleep(Duration::from_millis(2));
            drop(guard);
        });
        locked_rx.recv().unwrap();
        let started = Instant::now();
        let outcome = prune_retention_sweep_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Opening && attempt == 1 {
                    release_tx.send(()).unwrap();
                }
            },
        )
        .unwrap();
        holder.join().unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("millisecond contention skipped the sweep: {outcome:?}");
        };
        assert!(started.elapsed() >= Duration::from_micros(200), "sweep did not wait");
        assert_eq!(sweep.bash_tasks_removed, 1);
        assert_eq!(sweep.count_skip, None);
        eprintln!("millisecond holder: elapsed_us={} count_hold_us={} selection_hold_us={} mutation_hold_us={}", started.elapsed().as_micros(), sweep.worst_count_lock_micros, sweep.worst_selection_lock_micros, sweep.worst_mutation_lock_micros);
    }

    #[test]
    fn retention_sweep_reports_opening_lock_retry_exhaustion() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let _held = db.lock().unwrap();

        let outcome = prune_retention_sweep(&db, RETENTION_AGE_MS + 100, Some(&[])).unwrap();
        let RetentionSweepOutcome::Skipped(skip) = outcome else {
            panic!("exhausted opening contention was not reported: {outcome:?}");
        };

        assert_eq!(
            skip.reason,
            RetentionSweepSkipReason::OpeningCountLockBudgetExhausted
        );
        assert!(skip.attempts >= 2);
        assert!(skip.waited_micros >= RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS);
        assert!(
            RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS * 5 < RETENTION_LOCK_BUDGET_MICROS,
            "count retry budget must remain far below the mutex hold budget"
        );
        assert_eq!(last_retention_sweep_skip_reason(&db), Some(skip.reason));
        eprintln!(
            "retention opening contention: attempts={} waited_us={} configured_retry_budget_us={}",
            skip.attempts, skip.waited_micros, RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
        );
    }

    #[test]
    fn retention_sweep_reports_closing_lock_retry_exhaustion() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let mut held = None;

        let outcome = prune_retention_sweep_observed(
            &db,
            RETENTION_AGE_MS + 100,
            Some(&[]),
            |phase, attempt| {
                if phase == RetentionCountPhase::Closing && attempt == 1 {
                    held = Some(db.lock().unwrap());
                }
            },
        )
        .unwrap();
        let RetentionSweepOutcome::Completed(sweep) = outcome else {
            panic!("sweep did not reach its closing count: {outcome:?}");
        };
        let skip = sweep.count_skip.expect("closing count skip");

        assert_eq!(sweep.remaining_eligible_rows, None);
        assert_eq!(
            skip.reason,
            RetentionSweepSkipReason::ClosingCountLockBudgetExhausted
        );
        assert!(skip.attempts >= 2);
        assert!(skip.waited_micros >= RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS);
        assert_eq!(last_retention_sweep_skip_reason(&db), Some(skip.reason));
        eprintln!(
            "retention closing contention: attempts={} waited_us={} configured_retry_budget_us={}",
            skip.attempts, skip.waited_micros, RETENTION_COUNT_LOCK_RETRY_BUDGET_MICROS
        );
        drop(held.take());
    }

    #[test]
    fn retention_releases_database_mutex_before_layout_stats() {
        let dir = tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        conn.execute(
            "INSERT INTO bash_tasks (
                harness, session_id, task_id, project_key, command, cwd, status,
                started_at, completed_at, completion_delivered
             ) VALUES ('opencode', 'session', 'bash-0000000000000001', 'project',
                       'true', '.', 'completed', 1, 1, 1)",
            [],
        )
        .unwrap();
        let db = std::sync::Arc::new(std::sync::Mutex::new(conn));
        let observed = std::sync::atomic::AtomicBool::new(false);

        let pass = prune_retention_once_observed(
            &db,
            crate::db::bash_tasks::TERMINAL_ROW_RETENTION_AGE_MS + 100,
            Some(&[]),
            || {
                let _guard = db
                    .try_lock()
                    .expect("database mutex held during layout stat phase");
                observed.store(true, Ordering::SeqCst);
            },
        )
        .unwrap()
        .unwrap();

        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(pass.tick.bash_tasks.removed, 1);
    }

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
