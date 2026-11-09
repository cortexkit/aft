//! Deletion of revoked speech authorities.
//!
//! Every shell launch in a bot-enabled session stores one authority row, and
//! revocation only marks that row `revoked`, so without this sweep the table
//! grows with every launch. Once revoked, a row grants nothing: every lookup that
//! could authorize a write checks `gh_speech_task_revocations` first, and that
//! table is kept, so the incarnation stays refused after its authority row is
//! gone. A ticket whose row was deleted is refused as unknown instead of as
//! revoked. Rows still named by a dispatch from a build that queued writes in the
//! database (`gh_speech_legacy_dispatch`) are kept: the relay reads them to
//! answer a retried request as "already queued, outcome unknown", and task kill
//! reports count them.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, TransactionBehavior};

use crate::db::{MaintenanceWriteError, TrackedConnection};

/// How long a revoked authority is kept after its revocation was recorded.
/// A revoked row authorizes nothing; the day is a margin that keeps recent rows
/// available for diagnosis after the launch that owned them ended.
pub(crate) const REVOKED_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Rows examined and deleted per transaction. Each transaction holds the
/// process's shared connection mutex; at most this many rows are read or
/// deleted while it is held.
pub(crate) const BATCH_ROWS: i64 = 64;

/// Wall-clock budget for all batches of one retention tick. A batch starts only
/// while the budget lasts, so one tick can overrun it by at most one batch.
pub(crate) const TICK_BUDGET: Duration = Duration::from_millis(100);

/// Pages released per tick when the database file uses incremental auto-vacuum.
pub(crate) const INCREMENTAL_VACUUM_PAGES: i64 = 4096;

/// How long one batch waits for another process's write lock before the tick
/// stops and leaves the rest to the next tick.
const BUSY_WAIT: Duration = Duration::from_millis(50);

/// Revoked authorities old enough to delete, in incarnation order after `?1`.
///
/// The scan must not read `phase` or `record_json`: those columns sit in each
/// row's overflow pages (a record is tens of kilobytes), so reading them for every
/// row would read the whole table. The authority side is therefore read only
/// through its primary-key index, which covers `incarnation`, and a row counts as
/// revoked when its revocation is recorded. The outer statements still require
/// `phase = 'revoked'` before deleting, for the few rows they touch.
///
/// The CROSS JOIN fixes the authority index as the outer loop, so one batch
/// examines at most the authority rows that still exist, not every revocation
/// ever recorded (those are kept forever and keep growing).
macro_rules! candidates {
    () => {
        "SELECT a.incarnation AS incarnation
         FROM gh_speech_authorities AS a
         CROSS JOIN gh_speech_task_revocations AS r ON r.incarnation = a.incarnation
         WHERE a.incarnation > ?1
           AND r.recorded_at_ms <= ?2
           AND NOT EXISTS (
               SELECT 1 FROM gh_speech_legacy_dispatch AS d WHERE d.incarnation = a.incarnation
           )
         ORDER BY a.incarnation
         LIMIT ?3"
    };
}

pub(crate) const COUNT_CANDIDATES: &str = concat!(
    "SELECT COUNT(*), MAX(incarnation) FROM (",
    candidates!(),
    ")"
);

pub(crate) const DELETE_CANDIDATES: &str = concat!(
    "DELETE FROM gh_speech_authorities WHERE phase = 'revoked' AND incarnation IN (",
    candidates!(),
    ")"
);

/// Where the retention tick's next batch resumes (see [`sweep`]).
static CURSOR: Mutex<String> = Mutex::new(String::new());

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Batch {
    /// Candidates the batch selected (at most its limit).
    pub(crate) examined: usize,
    /// Candidates it deleted; the rest were not in phase `revoked`.
    pub(crate) deleted: usize,
    /// The last selected incarnation, where the next batch resumes.
    pub(crate) last: Option<String>,
}

/// Delete at most `limit` revoked authorities whose revocation was recorded at
/// or before `cutoff_ms`, starting after `after` in incarnation order, in one
/// immediate transaction.
pub(crate) fn delete_revoked_batch(
    conn: &mut Connection,
    after: &str,
    cutoff_ms: i64,
    limit: i64,
) -> rusqlite::Result<Batch> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (examined, last): (i64, Option<String>) = tx
        .prepare_cached(COUNT_CANDIDATES)?
        .query_row(params![after, cutoff_ms, limit], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let deleted = if examined == 0 {
        0
    } else {
        tx.prepare_cached(DELETE_CANDIDATES)?
            .execute(params![after, cutoff_ms, limit])?
    };
    tx.commit()?;
    Ok(Batch {
        examined: usize::try_from(examined).unwrap_or(0),
        deleted,
        last,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    pub(crate) batches: usize,
    pub(crate) examined: usize,
    pub(crate) deleted: usize,
    /// True when the last batch reached the end of the candidates.
    pub(crate) exhausted: bool,
    /// Why the tick stopped before reaching the end, if it did.
    pub(crate) stopped: Option<&'static str>,
    /// Pages `PRAGMA incremental_vacuum` was asked to release, when the database
    /// uses incremental auto-vacuum and had free pages.
    pub(crate) vacuum_pages: Option<i64>,
    pub(crate) elapsed: Duration,
}

/// Run batches until the candidates run out, `budget` has passed, or another
/// thread or process holds the database. Each batch is its own transaction on
/// the shared connection, taken with `maintenance_write`, so requests waiting
/// for the connection get it between batches.
///
/// `cursor` is the incarnation the next batch resumes after. Carrying it across
/// batches and ticks means rows that qualify by revocation time but are not
/// deleted (not in phase `revoked`) cannot fill every batch. It returns to the
/// start once a batch finds fewer candidates than its limit.
pub(crate) fn sweep(
    db: &Mutex<TrackedConnection>,
    cursor: &mut String,
    now_ms: i64,
    budget: Duration,
    batch_rows: i64,
) -> Result<Sweep, String> {
    let started = Instant::now();
    let deadline = started + budget;
    let cutoff_ms = now_ms.saturating_sub(REVOKED_MIN_AGE.as_millis() as i64);
    let mut sweep = Sweep::default();
    loop {
        if Instant::now() >= deadline {
            sweep.stopped = Some("time_budget");
            break;
        }
        let batch = crate::db::maintenance_write(db, BUSY_WAIT, |conn| {
            if conn.is_readonly(rusqlite::DatabaseName::Main)? {
                return Ok(None);
            }
            delete_revoked_batch(conn, cursor.as_str(), cutoff_ms, batch_rows).map(Some)
        });
        match batch {
            Ok(None) => {
                sweep.stopped = Some("read_only");
                break;
            }
            Ok(Some(batch)) => {
                sweep.batches += 1;
                sweep.examined += batch.examined;
                sweep.deleted += batch.deleted;
                match batch.last {
                    Some(last) if (batch.examined as i64) >= batch_rows => *cursor = last,
                    _ => {
                        sweep.exhausted = true;
                        cursor.clear();
                        break;
                    }
                }
            }
            Err(MaintenanceWriteError::MutexHeld) => {
                sweep.stopped = Some("connection_busy");
                break;
            }
            Err(MaintenanceWriteError::Busy(_)) => {
                sweep.stopped = Some("database_busy");
                break;
            }
            Err(MaintenanceWriteError::Poisoned) => {
                return Err("speech authority retention: database mutex poisoned".to_string())
            }
            Err(MaintenanceWriteError::Failed(error)) => {
                return Err(format!("speech authority retention: {error}"))
            }
        }
        std::thread::yield_now();
    }
    if sweep.stopped != Some("read_only") {
        sweep.vacuum_pages = incremental_vacuum(db, INCREMENTAL_VACUUM_PAGES)?;
    }
    sweep.elapsed = started.elapsed();
    Ok(sweep)
}

/// Deleting rows frees pages but does not shrink the file. A database created
/// with incremental auto-vacuum can return free pages to the file system a
/// bounded number at a time; any other mode keeps them for reuse by later
/// writes, and this does nothing.
fn incremental_vacuum(db: &Mutex<TrackedConnection>, pages: i64) -> Result<Option<i64>, String> {
    let outcome = crate::db::maintenance_write(db, BUSY_WAIT, |conn| {
        // 2 is INCREMENTAL; 0 (NONE) and 1 (FULL) need nothing here.
        let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
        let free: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
        if mode != 2 || free == 0 {
            return Ok(None);
        }
        let pages = pages.min(free);
        // Each step of this pragma releases one page, so it runs until done.
        let mut vacuum = conn.prepare(&format!("PRAGMA incremental_vacuum({pages})"))?;
        let mut steps = vacuum.query([])?;
        while steps.next()?.is_some() {}
        Ok(Some(pages))
    });
    match outcome {
        Ok(pages) => Ok(pages),
        Err(MaintenanceWriteError::MutexHeld | MaintenanceWriteError::Busy(_)) => Ok(None),
        Err(MaintenanceWriteError::Poisoned) => {
            Err("speech authority vacuum: database mutex poisoned".to_string())
        }
        Err(MaintenanceWriteError::Failed(error)) => {
            Err(format!("speech authority vacuum: {error}"))
        }
    }
}

/// The retention tick's entry point: one bounded sweep, logged.
pub(crate) fn run_tick(db: &Mutex<TrackedConnection>, now_ms: i64) {
    let mut cursor = CURSOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match sweep(db, &mut cursor, now_ms, TICK_BUDGET, BATCH_ROWS) {
        Ok(sweep) => {
            let message = format!(
                "speech authority retention: examined={} deleted={} batches={} exhausted={} stopped={} vacuum_pages={:?} elapsed_us={}",
                sweep.examined,
                sweep.deleted,
                sweep.batches,
                sweep.exhausted,
                sweep.stopped.unwrap_or("none"),
                sweep.vacuum_pages,
                sweep.elapsed.as_micros()
            );
            if sweep.deleted > 0 || sweep.vacuum_pages.is_some() {
                crate::slog_info!("{message}");
            } else {
                crate::slog_debug!("{message}");
            }
        }
        Err(error) => crate::slog_warn!("{error}"),
    }
}

#[cfg(test)]
mod tests;
