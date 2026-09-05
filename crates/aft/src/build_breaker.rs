//! Durable, domain-scoped admission breaker for background cold builds.
//!
//! The breaker deliberately measures only transactional extraction credit supplied
//! by its caller. It never treats database size, row count, cursor movement, or
//! SQLite page reuse as progress.

use crate::db::{SqliteStore, TrackedConnection};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ZERO_CREDIT_DEATH_LIMIT: u64 = 3;
pub const CREDITED_DEATH_LIMIT: u64 = 6;
pub const IN_BUILD_BURN_LIMIT_MS: u64 = 30 * 60 * 1_000;
pub const TRIP_TTL_MS: u64 = 24 * 60 * 60 * 1_000;
pub const ATTEMPT_MARKER_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
pub const ATTEMPT_MARKER_RECENT_HEARTBEAT_MS: u64 = 15_000;
pub const TEMP_DELETE_AGE_FLOOR_MS: u64 = 24 * 60 * 60 * 1_000;
pub const SWEEP_AMBIGUITY_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const SWEEP_STAT_CHECK_CAP: usize = 64;
pub const BREAKER_CONFIGURATION_VERSION: &str = "v1";

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

// Thread-local, not process-global: libtest runs sibling tests in parallel and
// they open breakers of their own, so a shared counter cannot support the exact
// open-count assertions the health rollup test makes about its own thread.
#[cfg(test)]
thread_local! {
    static OPEN_CALLS_FOR_TEST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
static FAIL_NEXT_ACTIVE_SUSPENSIONS_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Every expensive background build must choose one explicit domain. The enum is
/// intentionally exhaustive so new schedulers cannot silently bypass the breaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BuildDomain {
    CallgraphCold,
    SearchCold,
    SemanticSeed,
    Tier2Scan,
}

impl BuildDomain {
    pub const ALL: [Self; 4] = [
        Self::CallgraphCold,
        Self::SearchCold,
        Self::SemanticSeed,
        Self::Tier2Scan,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CallgraphCold => "callgraph_cold",
            Self::SearchCold => "search_cold",
            Self::SemanticSeed => "semantic_seed",
            Self::Tier2Scan => "tier2_scan",
        }
    }

    fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "callgraph_cold" => Some(Self::CallgraphCold),
            "search_cold" => Some(Self::SearchCold),
            "semantic_seed" => Some(Self::SemanticSeed),
            "tier2_scan" => Some(Self::Tier2Scan),
            _ => None,
        }
    }
}

/// Durable namespace. Configure generations and cache keys do not appear here:
/// they may invalidate a staging cursor but must not launder death history.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct BreakerKey {
    pub root_id: String,
    pub domain: BuildDomain,
    pub corpus_fingerprint: String,
}

impl BreakerKey {
    pub fn new(
        root_id: impl Into<String>,
        domain: BuildDomain,
        corpus_fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            root_id: root_id.into(),
            domain,
            corpus_fingerprint: corpus_fingerprint.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildAttempt {
    pub attempt_id: String,
    pub start_committed_extracted_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildSuspension {
    pub domain: BuildDomain,
    pub reason: String,
    pub death_count: u64,
    pub suspended_since_unix_ms: u64,
}

impl BuildSuspension {
    pub fn age_millis_at(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.suspended_since_unix_ms)
    }

    pub fn age_seconds_at(&self, now_ms: u64) -> u64 {
        self.age_millis_at(now_ms) / 1_000
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BreakerAdmission {
    Admitted(BuildAttempt),
    Suspended(BuildSuspension),
}

#[derive(Debug)]
pub enum BuildBreakerError {
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for BuildBreakerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl std::fmt::Display for BuildBreakerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "build breaker database error: {error}"),
        }
    }
}

impl std::error::Error for BuildBreakerError {}

pub type Result<T> = std::result::Result<T, BuildBreakerError>;

/// SQLite-backed, root/domain/fingerprint-isolated death history.
pub struct BuildDeathBreaker {
    path: PathBuf,
    /// One connection retains SQLite's initialized schema/WAL state for callers
    /// that repeatedly inspect a breaker, while the mutex keeps its non-Sync
    /// connection safe when a health thread and a build overlap. Build paths may
    /// open the same WAL file independently: readers do not block writers, and
    /// each connection has a five-second busy timeout for contested operations.
    connection: Mutex<TrackedConnection>,
}

impl BuildDeathBreaker {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        #[cfg(test)]
        OPEN_CALLS_FOR_TEST.with(|calls| calls.set(calls.get() + 1));
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                BuildBreakerError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            })?;
        }
        let connection = TrackedConnection::open(&path, SqliteStore::BreakerFile)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 CREATE TABLE IF NOT EXISTS breaker_records (
                    root_id TEXT NOT NULL,
                    domain TEXT NOT NULL,
                    corpus_fingerprint TEXT NOT NULL,
                    configuration_version TEXT NOT NULL,
                    zero_credit_deaths INTEGER NOT NULL DEFAULT 0,
                    credited_deaths INTEGER NOT NULL DEFAULT 0,
                    in_build_burn_ms INTEGER NOT NULL DEFAULT 0,
                    suspended_reason TEXT,
                    suspended_since_ms INTEGER,
                    suspended_until_ms INTEGER,
                    PRIMARY KEY(root_id, domain, corpus_fingerprint)
                 );
                 CREATE TABLE IF NOT EXISTS breaker_attempts (
                    root_id TEXT NOT NULL,
                    domain TEXT NOT NULL,
                    corpus_fingerprint TEXT NOT NULL,
                    attempt_id TEXT NOT NULL,
                    start_committed_extracted_bytes INTEGER NOT NULL,
                    death_charged INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY(root_id, domain, corpus_fingerprint, attempt_id)
                 );",
        )?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn admit(
        &self,
        key: &BreakerKey,
        committed_extracted_bytes: u64,
    ) -> Result<BreakerAdmission> {
        self.admit_at(key, committed_extracted_bytes, unix_millis_now())
    }

    pub fn admit_at(
        &self,
        key: &BreakerKey,
        committed_extracted_bytes: u64,
        now_ms: u64,
    ) -> Result<BreakerAdmission> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            ensure_record(&tx, key)?;
            let suspension = suspension_in_tx(&tx, key, now_ms)?;
            if let Some(suspension) = suspension {
                tx.commit()?;
                return Ok(BreakerAdmission::Suspended(suspension));
            }
            let attempt = BuildAttempt {
                attempt_id: format!(
                    "{}-{}-{}",
                    std::process::id(),
                    now_ms,
                    NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)
                ),
                start_committed_extracted_bytes: committed_extracted_bytes,
            };
            tx.execute(
                "INSERT INTO breaker_attempts(
                    root_id, domain, corpus_fingerprint, attempt_id, start_committed_extracted_bytes
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    key.root_id,
                    key.domain.as_str(),
                    key.corpus_fingerprint,
                    attempt.attempt_id,
                    committed_extracted_bytes as i64,
                ],
            )?;
            tx.commit()?;
            Ok(BreakerAdmission::Admitted(attempt))
        })
    }

    /// Charge one *already attributed* exact-process death. Callers must only use
    /// this after validating a durable expensive-phase marker and reading the
    /// staging metadata counter. Repeating the same attempt is idempotent.
    pub fn record_attributed_death(
        &self,
        key: &BreakerKey,
        attempt_id: &str,
        committed_extracted_bytes_at_death: u64,
        durable_burn_ms: u64,
    ) -> Result<Option<BuildSuspension>> {
        self.record_attributed_death_at(
            key,
            attempt_id,
            committed_extracted_bytes_at_death,
            durable_burn_ms,
            unix_millis_now(),
        )
    }

    pub fn record_attributed_death_at(
        &self,
        key: &BreakerKey,
        attempt_id: &str,
        committed_extracted_bytes_at_death: u64,
        durable_burn_ms: u64,
        now_ms: u64,
    ) -> Result<Option<BuildSuspension>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            ensure_record(&tx, key)?;
            let attempt = tx
                .query_row(
                    "SELECT start_committed_extracted_bytes, death_charged
                     FROM breaker_attempts
                     WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3 AND attempt_id = ?4",
                    params![key.root_id, key.domain.as_str(), key.corpus_fingerprint, attempt_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0)),
                )
                .optional()?;
            let Some((start, already_charged)) = attempt else {
                tx.commit()?;
                return Ok(None);
            };
            if already_charged {
                let current = suspension_in_tx(&tx, key, now_ms)?;
                tx.commit()?;
                return Ok(current);
            }

            // A backward counter is integrity ambiguity, not free credit. It is
            // deliberately charged as zero credit so it cannot erase history.
            let credited = committed_extracted_bytes_at_death > start.max(0) as u64;
            tx.execute(
                "UPDATE breaker_attempts SET death_charged = 1
                 WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3 AND attempt_id = ?4",
                params![key.root_id, key.domain.as_str(), key.corpus_fingerprint, attempt_id],
            )?;
            tx.execute(
                "UPDATE breaker_records
                 SET zero_credit_deaths = zero_credit_deaths + ?4,
                     credited_deaths = credited_deaths + ?5,
                     in_build_burn_ms = in_build_burn_ms + ?6
                 WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
                params![
                    key.root_id,
                    key.domain.as_str(),
                    key.corpus_fingerprint,
                    i64::from(!credited),
                    i64::from(credited),
                    durable_burn_ms.min(i64::MAX as u64) as i64,
                ],
            )?;
            let suspension = trip_if_needed(&tx, key, now_ms)?;
            tx.commit()?;
            Ok(suspension)
        })
    }

    /// Credit cannot reset the burn ceiling. This is used for live heartbeat
    /// checkpoints whose elapsed interval has already been durably bounded.
    pub fn record_durable_burn_at(
        &self,
        key: &BreakerKey,
        durable_burn_ms: u64,
        now_ms: u64,
    ) -> Result<Option<BuildSuspension>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            ensure_record(&tx, key)?;
            tx.execute(
                "UPDATE breaker_records SET in_build_burn_ms = in_build_burn_ms + ?4
                 WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
                params![
                    key.root_id,
                    key.domain.as_str(),
                    key.corpus_fingerprint,
                    durable_burn_ms.min(i64::MAX as u64) as i64,
                ],
            )?;
            let suspension = trip_if_needed(&tx, key, now_ms)?;
            tx.commit()?;
            Ok(suspension)
        })
    }

    /// A ready pointer flip is the only automatic full reset. Staged commits do
    /// not call this method and therefore cannot launder a death loop.
    pub fn record_ready_publication(&self, key: &BreakerKey) -> Result<()> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            ensure_record(&tx, key)?;
            reset_record(&tx, key)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Doctor and force-rebuild use this explicit, tuple-scoped full reset.
    pub fn explicit_reset(&self, key: &BreakerKey) -> Result<()> {
        self.record_ready_publication(key)
    }

    pub fn suspension(&self, key: &BreakerKey) -> Result<Option<BuildSuspension>> {
        self.suspension_at(key, unix_millis_now())
    }

    pub fn suspension_at(&self, key: &BreakerKey, now_ms: u64) -> Result<Option<BuildSuspension>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            ensure_record(&tx, key)?;
            let suspension = suspension_in_tx(&tx, key, now_ms)?;
            tx.commit()?;
            Ok(suspension)
        })
    }

    pub fn active_suspensions_for_root(&self, root_id: &str) -> Result<Vec<BuildSuspension>> {
        self.active_suspensions_for_root_at(root_id, unix_millis_now())
    }

    /// Read every still-active domain suspension for one root from the durable
    /// breaker rows. Health and doctor snapshots use this instead of inferring a
    /// suspension from transient worker state, so every surface reports the same
    /// persisted reason and counters.
    pub fn active_suspensions_for_root_at(
        &self,
        root_id: &str,
        now_ms: u64,
    ) -> Result<Vec<BuildSuspension>> {
        #[cfg(test)]
        if FAIL_NEXT_ACTIVE_SUSPENSIONS_FOR_TEST.swap(false, Ordering::SeqCst) {
            return Err(BuildBreakerError::Sqlite(rusqlite::Error::InvalidQuery));
        }
        self.with_connection(|conn| {
            let mut statement = conn.prepare(
                "SELECT domain, zero_credit_deaths, credited_deaths, suspended_reason, suspended_since_ms
                 FROM breaker_records
                 WHERE root_id = ?1
                   AND configuration_version = ?2
                   AND suspended_reason IS NOT NULL
                   AND suspended_since_ms IS NOT NULL
                   AND suspended_until_ms > ?3
                 ORDER BY domain",
            )?;
            let rows = statement.query_map(
                params![root_id, BREAKER_CONFIGURATION_VERSION, now_ms.min(i64::MAX as u64) as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?;
            let mut suspensions = Vec::new();
            for row in rows {
                let (domain, zero_credit_deaths, credited_deaths, reason, since) = row?;
                let Some(domain) = BuildDomain::from_persisted(&domain) else {
                    continue;
                };
                suspensions.push(BuildSuspension {
                    domain,
                    reason,
                    death_count: (zero_credit_deaths.max(0) as u64)
                        .saturating_add(credited_deaths.max(0) as u64),
                    suspended_since_unix_ms: since.max(0) as u64,
                });
            }
            Ok(suspensions)
        })
    }

    #[cfg(test)]
    pub(crate) fn reset_open_calls_for_test() {
        OPEN_CALLS_FOR_TEST.with(|calls| calls.set(0));
    }

    #[cfg(test)]
    pub(crate) fn open_calls_for_test() -> u64 {
        OPEN_CALLS_FOR_TEST.with(|calls| calls.get())
    }

    #[cfg(test)]
    pub(crate) fn fail_next_active_suspensions_for_test() {
        FAIL_NEXT_ACTIVE_SUSPENSIONS_FOR_TEST.store(true, Ordering::SeqCst);
    }

    fn with_connection<T>(&self, work: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        work(&mut connection)
    }
}

fn ensure_record(tx: &rusqlite::Transaction<'_>, key: &BreakerKey) -> Result<()> {
    tx.execute(
        "INSERT INTO breaker_records(root_id, domain, corpus_fingerprint, configuration_version)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(root_id, domain, corpus_fingerprint) DO UPDATE SET
           configuration_version = excluded.configuration_version,
           zero_credit_deaths = 0,
           credited_deaths = 0,
           in_build_burn_ms = 0,
           suspended_reason = NULL,
           suspended_since_ms = NULL,
           suspended_until_ms = NULL
         WHERE breaker_records.configuration_version != excluded.configuration_version",
        params![
            key.root_id,
            key.domain.as_str(),
            key.corpus_fingerprint,
            BREAKER_CONFIGURATION_VERSION,
        ],
    )?;
    Ok(())
}

fn reset_record(tx: &rusqlite::Transaction<'_>, key: &BreakerKey) -> Result<()> {
    tx.execute(
        "UPDATE breaker_records
         SET zero_credit_deaths = 0, credited_deaths = 0, in_build_burn_ms = 0,
             suspended_reason = NULL, suspended_since_ms = NULL, suspended_until_ms = NULL
         WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
        params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
    )?;
    Ok(())
}

fn suspension_in_tx(
    tx: &rusqlite::Transaction<'_>,
    key: &BreakerKey,
    now_ms: u64,
) -> Result<Option<BuildSuspension>> {
    let record = tx.query_row(
        "SELECT zero_credit_deaths, credited_deaths, in_build_burn_ms,
                    suspended_reason, suspended_since_ms, suspended_until_ms
             FROM breaker_records
             WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
        params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        },
    )?;
    let (zero_credit_deaths, credited_deaths, _burn, reason, since, until) = record;
    let (Some(reason), Some(since), Some(until)) = (reason, since, until) else {
        return Ok(None);
    };
    if until.max(0) as u64 <= now_ms {
        // TTL permits another probe while retaining all historical tallies.
        tx.execute(
            "UPDATE breaker_records
             SET suspended_reason = NULL, suspended_since_ms = NULL, suspended_until_ms = NULL
             WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
            params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
        )?;
        return Ok(None);
    }
    Ok(Some(BuildSuspension {
        domain: key.domain,
        reason,
        death_count: zero_credit_deaths.saturating_add(credited_deaths),
        suspended_since_unix_ms: since.max(0) as u64,
    }))
}

fn trip_if_needed(
    tx: &rusqlite::Transaction<'_>,
    key: &BreakerKey,
    now_ms: u64,
) -> Result<Option<BuildSuspension>> {
    let (zero_credit_deaths, credited_deaths, burn) = tx.query_row(
        "SELECT zero_credit_deaths, credited_deaths, in_build_burn_ms
         FROM breaker_records WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
        params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))
        },
    )?;
    let reason = if zero_credit_deaths >= ZERO_CREDIT_DEATH_LIMIT {
        Some("zero_credit_death_limit")
    } else if credited_deaths >= CREDITED_DEATH_LIMIT {
        Some("credited_death_limit")
    } else if burn >= IN_BUILD_BURN_LIMIT_MS {
        Some("in_build_burn_limit")
    } else {
        None
    };
    let Some(reason) = reason else {
        return Ok(None);
    };
    let existing = tx.query_row(
        "SELECT suspended_since_ms FROM breaker_records
             WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
        params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let since = existing
        .unwrap_or(now_ms.min(i64::MAX as u64) as i64)
        .max(0) as u64;
    tx.execute(
        "UPDATE breaker_records
         SET suspended_reason = ?4, suspended_since_ms = ?5, suspended_until_ms = ?6
         WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
        params![
            key.root_id,
            key.domain.as_str(),
            key.corpus_fingerprint,
            reason,
            since as i64,
            now_ms.saturating_add(TRIP_TTL_MS).min(i64::MAX as u64) as i64,
        ],
    )?;
    Ok(Some(BuildSuspension {
        domain: key.domain,
        reason: reason.to_string(),
        death_count: zero_credit_deaths.saturating_add(credited_deaths),
        suspended_since_unix_ms: since,
    }))
}

fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
#[path = "build_breaker_audit_tests.rs"]
mod audit_matrix_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn breaker() -> BuildDeathBreaker {
        BuildDeathBreaker::open(tempdir().unwrap().keep().join("breaker.sqlite")).unwrap()
    }

    fn key(domain: BuildDomain) -> BreakerKey {
        BreakerKey::new("root-a", domain, "corpus-a")
    }

    fn admitted(breaker: &BuildDeathBreaker, key: &BreakerKey, now: u64) -> BuildAttempt {
        match breaker.admit_at(key, 10, now).unwrap() {
            BreakerAdmission::Admitted(attempt) => attempt,
            BreakerAdmission::Suspended(suspension) => {
                panic!("unexpected suspension: {suspension:?}")
            }
        }
    }

    fn durable_tallies(breaker: &BuildDeathBreaker, key: &BreakerKey) -> (u64, u64, u64) {
        Connection::open(breaker.path())
            .unwrap()
            .query_row(
                "SELECT zero_credit_deaths, credited_deaths, in_build_burn_ms
                 FROM breaker_records
                 WHERE root_id = ?1 AND domain = ?2 AND corpus_fingerprint = ?3",
                params![key.root_id, key.domain.as_str(), key.corpus_fingerprint],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, i64>(2)? as u64,
                    ))
                },
            )
            .unwrap()
    }

    #[test]
    fn three_zero_credit_deaths_trip_once_and_are_idempotent() {
        let breaker = breaker();
        let key = key(BuildDomain::CallgraphCold);
        let mut final_attempt = None;
        for now in 1..=3 {
            let attempt = admitted(&breaker, &key, now);
            let suspension = breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 10, 0, now)
                .unwrap();
            if now == 3 {
                final_attempt = Some((attempt, suspension.unwrap()));
            } else {
                assert!(suspension.is_none());
            }
        }
        let (attempt, suspension) = final_attempt.unwrap();
        assert_eq!(suspension.reason, "zero_credit_death_limit");
        assert_eq!(suspension.death_count, 3);
        assert_eq!(
            breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 10, 0, 4)
                .unwrap(),
            Some(suspension),
            "recovery may repeat marker cleanup but must not charge it twice"
        );
    }

    #[test]
    fn one_batch_per_death_still_trips_after_six_credited_attempts() {
        let breaker = breaker();
        let key = key(BuildDomain::CallgraphCold);
        for now in 1..=5 {
            let attempt = admitted(&breaker, &key, now);
            assert!(breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 11, 0, now)
                .unwrap()
                .is_none());
        }
        assert_eq!(durable_tallies(&breaker, &key), (0, 5, 0));
        assert!(breaker.suspension_at(&key, 6).unwrap().is_none());

        let sixth = admitted(&breaker, &key, 6);
        let suspension = breaker
            .record_attributed_death_at(&key, &sixth.attempt_id, 11, 0, 6)
            .unwrap()
            .unwrap();
        assert_eq!(suspension.reason, "credited_death_limit");
        assert_eq!(suspension.death_count, 6);
        assert_eq!(durable_tallies(&breaker, &key), (0, 6, 0));
    }

    #[test]
    fn ttl_lifts_only_suspension_and_retains_death_history() {
        let breaker = breaker();
        let key = key(BuildDomain::CallgraphCold);
        for now in 1..=3 {
            let attempt = admitted(&breaker, &key, now);
            breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 10, 0, now)
                .unwrap();
        }
        assert_eq!(durable_tallies(&breaker, &key), (3, 0, 0));

        let BreakerAdmission::Admitted(retry) =
            breaker.admit_at(&key, 10, TRIP_TTL_MS + 4).unwrap()
        else {
            panic!("expired suspension must admit exactly one probe");
        };
        assert_eq!(
            durable_tallies(&breaker, &key),
            (3, 0, 0),
            "TTL expiry lifts scheduling without erasing durable history"
        );

        let suspension = breaker
            .record_attributed_death_at(&key, &retry.attempt_id, 10, 0, TRIP_TTL_MS + 5)
            .unwrap()
            .unwrap();
        assert_eq!(suspension.reason, "zero_credit_death_limit");
        assert_eq!(suspension.death_count, 4);
        assert_eq!(durable_tallies(&breaker, &key), (4, 0, 0));
    }

    #[test]
    fn root_and_domain_histories_are_isolated() {
        for tripped_domain in BuildDomain::ALL {
            let breaker = breaker();
            let tripped = BreakerKey::new(
                format!("root-{}", tripped_domain.as_str()),
                tripped_domain,
                "corpus-a",
            );
            for now in 1..=3 {
                let attempt = admitted(&breaker, &tripped, now);
                breaker
                    .record_attributed_death_at(&tripped, &attempt.attempt_id, 10, 0, now)
                    .unwrap();
            }
            let report = breaker.suspension_at(&tripped, 4).unwrap().unwrap();
            assert_eq!(report.domain, tripped_domain);
            assert_eq!(report.death_count, 3);

            let mut siblings = BuildDomain::ALL
                .into_iter()
                .filter(|domain| *domain != tripped_domain)
                .map(|domain| BreakerKey::new(tripped.root_id.clone(), domain, "corpus-a"))
                .collect::<Vec<_>>();
            siblings.push(BreakerKey::new(
                format!("{}-sibling", tripped.root_id),
                tripped_domain,
                "corpus-a",
            ));

            for (index, sibling) in siblings.iter().enumerate() {
                assert!(breaker.suspension_at(sibling, 4).unwrap().is_none());
                let attempt = admitted(&breaker, sibling, 10 + index as u64);
                breaker
                    .record_attributed_death_at(
                        sibling,
                        &attempt.attempt_id,
                        10,
                        0,
                        10 + index as u64,
                    )
                    .unwrap();
                assert_eq!(durable_tallies(&breaker, sibling), (1, 0, 0));
            }

            breaker.explicit_reset(&siblings[0]).unwrap();
            assert_eq!(durable_tallies(&breaker, &siblings[0]), (0, 0, 0));
            assert!(breaker.suspension_at(&tripped, 20).unwrap().is_some());

            breaker.explicit_reset(&tripped).unwrap();
            assert!(breaker.suspension_at(&tripped, 21).unwrap().is_none());
            assert_eq!(durable_tallies(&breaker, &siblings[1]), (1, 0, 0));
        }
    }

    #[test]
    fn burn_limit_trips_without_counter_credit() {
        let breaker = breaker();
        let key = key(BuildDomain::Tier2Scan);
        let suspension = breaker
            .record_durable_burn_at(&key, IN_BUILD_BURN_LIMIT_MS, 77)
            .unwrap()
            .unwrap();
        assert_eq!(suspension.reason, "in_build_burn_limit");
        assert_eq!(suspension.domain, BuildDomain::Tier2Scan);
    }

    #[test]
    fn durable_trip_agrees_across_navigation_inspect_and_health_snapshots() {
        let storage = tempdir().unwrap();
        let root = storage.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let project_key = crate::search_index::artifact_cache_key(&root);
        let breaker_path = storage
            .path()
            .join("callgraph")
            .join(&project_key)
            .join("build-breaker.sqlite");
        let breaker = BuildDeathBreaker::open(&breaker_path).unwrap();
        let key = BreakerKey::new(
            root.display().to_string(),
            BuildDomain::CallgraphCold,
            "corpus-a",
        );
        let trip_at = 1_000_000;
        let mut trip_decision = None;
        for death_at in (trip_at - 2)..=trip_at {
            let attempt = admitted(&breaker, &key, death_at);
            trip_decision = breaker
                .record_attributed_death_at(&key, &attempt.attempt_id, 10, 0, death_at)
                .unwrap();
        }
        let trip_decision = trip_decision.expect("third zero-credit death trips the breaker");
        let snapshot_at = trip_at + 5_000;
        let durable_rows = breaker
            .active_suspensions_for_root_at(&root.display().to_string(), snapshot_at)
            .unwrap();
        assert_eq!(durable_rows, vec![trip_decision.clone()]);
        let suspension = &durable_rows[0];

        let navigation = crate::commands::callgraph_store_adapter::suspended_response_at(
            "surface-agreement",
            "callers",
            suspension,
            snapshot_at,
        );
        assert_eq!(navigation.data["code"], "build_suspended");
        assert_eq!(
            navigation.data["message"],
            "callers: build_suspended domain=callgraph_cold deaths=3 age_ms=5000 reason=zero_credit_death_limit; run doctor reset-build-breaker to resume"
        );

        let manager = crate::inspect::InspectManager::new();
        manager.record_tier2_build_suspension_for_test(
            crate::inspect::InspectCategory::DeadCode,
            suspension.clone(),
        );
        assert_eq!(
            manager.tier2_builder_state_detail_at(
                crate::inspect::InspectCategory::DeadCode,
                snapshot_at,
            ),
            "suspended domain=callgraph_cold deaths=3 age_s=5 reason=zero_credit_death_limit"
        );

        let config = crate::config::Config {
            project_root: Some(root.clone()),
            storage_dir: Some(storage.path().to_path_buf()),
            ..crate::config::Config::default()
        };
        let context = crate::context::AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            config,
        );
        // Contexts without a bound root must not start unrelated heavy work while
        // this test reads the cached health projection of the durable breaker row.
        context.set_heavy_root_work_allowed(false);
        assert_eq!(context.storage_dir(), storage.path());
        context.refresh_build_suspensions_for_health_at(&root, Some(&project_key), snapshot_at);
        let health = context.try_health_snapshot(&root);
        assert_eq!(health.suspended_domains.len(), 1);
        assert_eq!(health.suspended_domains[0].domain, "callgraph_cold");
        assert_eq!(
            health.suspended_domains[0].reason,
            "zero_credit_death_limit"
        );
        assert_eq!(health.suspended_domains[0].death_count, 3);
        assert_eq!(health.suspended_domains[0].age_s, 5);

        assert!(breaker
            .active_suspensions_for_root_at(&root.display().to_string(), trip_at + TRIP_TTL_MS + 5,)
            .unwrap()
            .is_empty());
        assert!(matches!(
            breaker
                .admit_at(&key, 10, trip_at + TRIP_TTL_MS + 5)
                .unwrap(),
            BreakerAdmission::Admitted(_)
        ));
    }
}
