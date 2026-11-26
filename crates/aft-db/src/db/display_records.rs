//! Frozen display.read replies committed before their bus wakes.
use rusqlite::{params, Connection, OptionalExtension};

/// Records stay protected for ten minutes after their wake is acknowledged,
/// and a busy computer writes about 30 a minute, so the count cap must hold
/// several hundred at once. Pull bytes average under 2 KiB, so the byte cap is
/// what bounds disk use.
pub(crate) const MAX_RECORDS: usize = 4096;
pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
const SWEEP_LIMIT: usize = 64;

/// Wakes deferred since the cache last admitted a record. Only the first
/// deferral of a full stretch is logged, and the total when admission resumes.
static DEFERRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) const CACHE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS display_records (
  record_id TEXT NOT NULL PRIMARY KEY,
  pull_bytes BLOB NOT NULL,
  announced_at_ms INTEGER NOT NULL,
  wake_acked_at_ms INTEGER
);
CREATE INDEX IF NOT EXISTS idx_display_records_expiry ON display_records (
  CASE WHEN wake_acked_at_ms IS NULL THEN announced_at_ms + 86400000
       ELSE wake_acked_at_ms + 600000 END
);
"#;

const EXPIRY: &str = "CASE WHEN wake_acked_at_ms IS NULL THEN announced_at_ms + 86400000 ELSE wake_acked_at_ms + 600000 END";
pub const AWAITED_NOTICE_PREFIX: &str = "notice:awaited:";

pub(super) fn install(conn: &Connection) -> rusqlite::Result<()> {
    // This optional delivery cache is outside the numbered schema: no older
    // reader touches it, and losing it costs only a display pull. Keep these
    // statements idempotent if a later numbered migration adopts the table.
    conn.execute_batch(CACHE_DDL)?;
    sweep(conn, now_ms())?;
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn available(conn: &Connection) -> rusqlite::Result<bool> {
    if conn.is_readonly(rusqlite::DatabaseName::Main)? {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='display_records')",
        [],
        |row| row.get(0),
    )
}

/// Reclaim one bounded page without loading any tool output into memory.
pub(crate) fn sweep(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    if !available(conn)? {
        return Ok(0);
    }
    let rows = conn
        .prepare(&format!(
            "SELECT record_id, length(pull_bytes) FROM display_records
             WHERE {EXPIRY} < ?1 ORDER BY {EXPIRY}, record_id LIMIT ?2"
        ))?
        .query_map(params![now_ms, SWEEP_LIMIT], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let (mut count, mut bytes) = (0, 0);
    for (id, size) in &rows {
        let deleted = conn.execute(
            &format!("DELETE FROM display_records WHERE record_id=?1 AND {EXPIRY} < ?2"),
            params![id, now_ms],
        )?;
        count += deleted;
        bytes += deleted * size;
    }
    if count > 0 {
        log::info!("display store eviction: reason=expired count={count} bytes={bytes}");
    }
    Ok(count)
}

/// False means protected capacity is full: the caller must defer the wake,
/// without interfering with the independent display.write push.
pub fn persist(conn: &Connection, id: &str, bytes: &[u8], now_ms: i64) -> rusqlite::Result<bool> {
    if !available(conn)? {
        log::warn!(
            "display store deferral: reason=unavailable count=1 bytes={}",
            bytes.len()
        );
        return Ok(false);
    }
    let tx = conn.unchecked_transaction()?;
    sweep(&tx, now_ms)?;
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT pull_bytes FROM display_records WHERE record_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != bytes {
            log::warn!(
                "display store deferral: reason=conflicting_bytes count=1 bytes={}",
                bytes.len()
            );
            tx.commit()?;
            return Ok(false);
        }
    } else {
        let (mut count, mut used): (usize, usize) = tx.query_row(
            "SELECT count(*), coalesce(sum(length(pull_bytes)),0) FROM display_records",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // Expired pages may remain after the first bounded sweep. Reclaim only
        // as many pages as admission needs; protected rows are never victims.
        while (count >= MAX_RECORDS || used.saturating_add(bytes.len()) > MAX_BYTES)
            && sweep(&tx, now_ms)? > 0
        {
            (count, used) = tx.query_row(
                "SELECT count(*), coalesce(sum(length(pull_bytes)),0) FROM display_records",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        }
        if count >= MAX_RECORDS || used.saturating_add(bytes.len()) > MAX_BYTES {
            if DEFERRED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                log::warn!("display store deferral: reason=protected_capacity count={count} bytes={used} incoming_bytes={}; later deferrals are counted until a record is admitted", bytes.len());
            }
            tx.commit()?;
            return Ok(false);
        }
    }
    // Each publish attempt gets a fresh unacknowledged window. An uncertain
    // publish result must not shorten protection for a wake the bus accepted.
    tx.execute(
        "INSERT INTO display_records(record_id,pull_bytes,announced_at_ms,wake_acked_at_ms)
         VALUES (?1,?2,?3,NULL)
         ON CONFLICT(record_id) DO UPDATE SET announced_at_ms=excluded.announced_at_ms,
           wake_acked_at_ms=NULL",
        params![id, bytes, now_ms],
    )?;
    tx.commit()?;
    let deferred = DEFERRED.swap(0, std::sync::atomic::Ordering::Relaxed);
    if deferred > 0 {
        log::info!("display store admitting again after {deferred} deferred wake(s)");
    }
    Ok(true)
}

pub fn acknowledged(conn: &Connection, id: &str, now_ms: i64) -> rusqlite::Result<()> {
    let updated = conn.execute(
        "UPDATE display_records SET wake_acked_at_ms=?2 WHERE record_id=?1",
        params![id, now_ms],
    )?;
    if updated != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

pub fn read(conn: &Connection, id: &str, now_ms: i64) -> rusqlite::Result<Option<Vec<u8>>> {
    if !available(conn)? {
        return Ok(None);
    }
    conn.query_row(
        &format!("SELECT pull_bytes FROM display_records WHERE record_id=?1 AND {EXPIRY} >= ?2"),
        params![id, now_ms],
        |row| row.get(0),
    )
    .optional()
}

/// Keep the original record and its publication receipt on a replay of the
/// same shell result. The record key is already fixed for that event.
pub fn persist_awaited_notice(
    conn: &Connection,
    id: &str,
    bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    if read(conn, id, now_ms)?.is_some() {
        return Ok(true);
    }
    persist(conn, id, bytes, now_ms)
}

pub fn prepare_awaited_notice(
    conn: &Connection,
    id: &str,
    bytes: &[u8],
    now_ms: i64,
) -> rusqlite::Result<bool> {
    if read(conn, id, now_ms)?.is_some() {
        return Ok(true);
    }
    // A late database job may commit after the caller's card deadline. Store
    // it under a draft ID so the publisher cannot announce it unless the
    // orchestrator request links to the card.
    persist_awaited_notice(conn, &format!("draft:{id}"), bytes, now_ms)
}

pub fn activate_awaited_notice(conn: &Connection, id: &str, now_ms: i64) -> rusqlite::Result<()> {
    if available(conn)? {
        conn.execute(
            &format!(
                "UPDATE display_records SET record_id=?1 WHERE record_id=?2 AND {EXPIRY} >= ?3
             AND NOT EXISTS (SELECT 1 FROM display_records WHERE record_id=?1)"
            ),
            params![id, format!("draft:{id}"), now_ms],
        )?;
    }
    Ok(())
}

/// Only linked notices use the persistent queue. Selecting one row by its key
/// prefix avoids loading display bodies or replaying legacy announcements.
pub fn pending_awaited_notice(
    conn: &Connection,
    now_ms: i64,
    deferred: &[String],
) -> rusqlite::Result<Option<String>> {
    if !available(conn)? {
        return Ok(None);
    }
    conn.query_row(
        "SELECT record_id FROM display_records
         WHERE record_id >= ?1 AND record_id < 'notice:awaited;'
           AND wake_acked_at_ms IS NULL AND announced_at_ms + 86400000 >= ?2
           AND record_id NOT IN (SELECT value FROM json_each(?3))
         ORDER BY record_id LIMIT 1",
        params![
            AWAITED_NOTICE_PREFIX,
            now_ms,
            serde_json::to_string(deferred).expect("record ids are serializable")
        ],
        |row| row.get(0),
    )
    .optional()
}

pub fn count_pending_awaited_notices(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    if !available(conn)? {
        return Ok(0);
    }
    conn.query_row(
        "SELECT count(*) FROM display_records
         WHERE record_id >= ?1 AND record_id < 'notice:awaited;'
           AND wake_acked_at_ms IS NULL AND announced_at_ms + 86400000 >= ?2",
        params![AWAITED_NOTICE_PREFIX, now_ms],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests;

#[cfg(feature = "test-support")]
pub mod test_support {
    pub const MAX_RECORDS: usize = super::MAX_RECORDS;

    pub fn current_schema_version(conn: &rusqlite::Connection) -> rusqlite::Result<u32> {
        super::super::current_schema_version(conn)
    }

    pub fn available(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
        super::available(conn)
    }

    pub fn run_migrations_supported(
        conn: &mut rusqlite::Connection,
        supported: u32,
    ) -> Result<u32, super::super::OpenError> {
        super::super::run_migrations_supported(conn, supported)
    }
}
