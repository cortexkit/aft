//! Process-wide attribution of AFT-owned writes.
//!
//! Writers register a `(domain, root)` counter once and retain the returned
//! [`Counter`]. Crediting that counter is only two relaxed atomic additions;
//! registry locking and string allocation stay off the write path.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::process_io::Bytes;

pub const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
pub const DEFAULT_WINDOW_MS: u64 = 10 * 60 * 1_000;
const MINUTE_MS: u64 = 60_000;
const PROCESS_ROOT: &str = "<process>";

/// Closed attribution vocabulary used by persistence and management clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    CallgraphCold,
    CallgraphColdStaging,
    CallgraphRefresh,
    CallgraphCheckpoint,
    SearchIndexBuild,
    SearchIndexDelta,
    SemanticCold,
    SemanticDelta,
    SemanticCompaction,
    SymbolCache,
    InspectCache,
    ViewsBlob,
    ViewsDerived,
    ViewsClosure,
    AftDb,
    BashTaskIo,
    Backups,
    Checkpoints,
    Logs,
    Other,
}

impl Domain {
    pub const ALL: [Self; 20] = [
        Self::CallgraphCold,
        Self::CallgraphColdStaging,
        Self::CallgraphRefresh,
        Self::CallgraphCheckpoint,
        Self::SearchIndexBuild,
        Self::SearchIndexDelta,
        Self::SemanticCold,
        Self::SemanticDelta,
        Self::SemanticCompaction,
        Self::SymbolCache,
        Self::InspectCache,
        Self::ViewsBlob,
        Self::ViewsDerived,
        Self::ViewsClosure,
        Self::AftDb,
        Self::BashTaskIo,
        Self::Backups,
        Self::Checkpoints,
        Self::Logs,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CallgraphCold => "callgraph_cold",
            Self::CallgraphColdStaging => "callgraph_cold_staging",
            Self::CallgraphRefresh => "callgraph_refresh",
            Self::CallgraphCheckpoint => "callgraph_checkpoint",
            Self::SearchIndexBuild => "search_index_build",
            Self::SearchIndexDelta => "search_index_delta",
            Self::SemanticCold => "semantic_cold",
            Self::SemanticDelta => "semantic_delta",
            Self::SemanticCompaction => "semantic_compaction",
            Self::SymbolCache => "symbol_cache",
            Self::InspectCache => "inspect_cache",
            Self::ViewsBlob => "views_blob",
            Self::ViewsDerived => "views_derived",
            Self::ViewsClosure => "views_closure",
            Self::AftDb => "aft_db",
            Self::BashTaskIo => "bash_task_io",
            Self::Backups => "backups",
            Self::Checkpoints => "checkpoints",
            Self::Logs => "logs",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|domain| domain.as_str() == value)
    }
}

#[derive(Debug)]
struct Entry {
    domain: Domain,
    root_id: String,
    logical: AtomicU64,
    physical: AtomicU64,
    folded_logical: AtomicU64,
    folded_physical: AtomicU64,
    seam_labels: Mutex<BTreeSet<String>>,
}

#[derive(Debug)]
struct UnmeasurableEntry {
    root_id: String,
    seam: String,
    reason: String,
    estimate_basis: Option<String>,
    observations: AtomicU64,
    estimated_physical_bytes: AtomicU64,
    folded_observations: AtomicU64,
    folded_estimated_physical_bytes: AtomicU64,
}

impl UnmeasurableEntry {
    fn pending(&self) -> (u64, u64) {
        (
            self.observations
                .load(Ordering::Relaxed)
                .saturating_sub(self.folded_observations.load(Ordering::Relaxed)),
            self.estimated_physical_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(self.folded_estimated_physical_bytes.load(Ordering::Relaxed)),
        )
    }
}

impl Entry {
    fn pending(&self) -> (u64, u64) {
        (
            self.logical
                .load(Ordering::Relaxed)
                .saturating_sub(self.folded_logical.load(Ordering::Relaxed)),
            self.physical
                .load(Ordering::Relaxed)
                .saturating_sub(self.folded_physical.load(Ordering::Relaxed)),
        )
    }
}

/// Pre-registered, allocation-free write-path counter.
#[derive(Clone, Debug)]
pub struct Counter(Arc<Entry>);

impl Counter {
    pub fn credit(&self, logical_bytes: u64, physical_bytes: u64) {
        saturating_add(&self.0.logical, logical_bytes);
        saturating_add(&self.0.physical, physical_bytes);
    }

    pub fn credit_logical(&self, logical_bytes: u64) {
        self.credit(logical_bytes, 0);
    }

    pub(crate) fn domain(&self) -> Domain {
        self.0.domain
    }

    pub(crate) fn root_id(&self) -> String {
        self.0.root_id.clone()
    }

    pub fn note_seam_label(&self, label: &str) {
        self.0
            .seam_labels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(label.to_owned());
    }

    /// Record a write seam whose bytes cannot be measured without changing the
    /// operation being observed. Estimates stay separate from attributed bytes.
    pub fn note_unmeasurable(
        &self,
        seam: &str,
        reason: &str,
        estimated_physical_bytes: Option<u64>,
        estimate_basis: Option<&str>,
    ) {
        note_unmeasurable(
            self.0.root_id.clone(),
            seam,
            reason,
            estimated_physical_bytes,
            estimate_basis,
        );
    }
}

fn saturating_add(value: &AtomicU64, delta: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
    });
}

#[derive(Clone, Debug)]
struct ProcessBaseline {
    bytes: Option<Bytes>,
    sampled_at_ms: u64,
}

#[derive(Clone, Debug)]
struct RecentRow {
    minute_ts: u64,
    domain: Domain,
    root_id: String,
    physical_bytes: u64,
}

#[derive(Debug)]
struct Registry {
    started_ms: u64,
    entries: Mutex<BTreeMap<(Domain, String), Arc<Entry>>>,
    unmeasurable_entries: Mutex<BTreeMap<(String, String), Arc<UnmeasurableEntry>>>,
    process_baseline: Mutex<ProcessBaseline>,
    fold_lock: Mutex<()>,
    recent: Mutex<VecDeque<RecentRow>>,
    recent_top: Mutex<Vec<WriterRow>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let started_ms = now_ms();
        Registry {
            started_ms,
            entries: Mutex::new(BTreeMap::new()),
            unmeasurable_entries: Mutex::new(BTreeMap::new()),
            process_baseline: Mutex::new(ProcessBaseline {
                bytes: Bytes::capture(),
                sampled_at_ms: started_ms,
            }),
            fold_lock: Mutex::new(()),
            recent: Mutex::new(VecDeque::new()),
            recent_top: Mutex::new(Vec::new()),
        }
    })
}

pub fn register(domain: Domain, root_id: impl Into<String>) -> Counter {
    let root_id = root_id.into();
    let mut entries = registry()
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Counter(
        entries
            .entry((domain, root_id.clone()))
            .or_insert_with(|| {
                Arc::new(Entry {
                    domain,
                    root_id,
                    logical: AtomicU64::new(0),
                    physical: AtomicU64::new(0),
                    folded_logical: AtomicU64::new(0),
                    folded_physical: AtomicU64::new(0),
                    seam_labels: Mutex::new(BTreeSet::new()),
                })
            })
            .clone(),
    )
}

/// Convenience for operation-boundary instrumentation. Repeated/hot writers
/// should retain a [`Counter`] instead.
pub fn credit(domain: Domain, root_id: impl Into<String>, logical: u64, physical: u64) {
    register(domain, root_id).credit(logical, physical);
}

fn note_unmeasurable(
    root_id: String,
    seam: &str,
    reason: &str,
    estimated_physical_bytes: Option<u64>,
    estimate_basis: Option<&str>,
) {
    debug_assert_eq!(estimated_physical_bytes.is_some(), estimate_basis.is_some());
    let key = (root_id.clone(), seam.to_owned());
    let entry = {
        let mut entries = registry()
            .unmeasurable_entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .entry(key)
            .or_insert_with(|| {
                Arc::new(UnmeasurableEntry {
                    root_id,
                    seam: seam.to_owned(),
                    reason: reason.to_owned(),
                    estimate_basis: estimate_basis.map(str::to_owned),
                    observations: AtomicU64::new(0),
                    estimated_physical_bytes: AtomicU64::new(0),
                    folded_observations: AtomicU64::new(0),
                    folded_estimated_physical_bytes: AtomicU64::new(0),
                })
            })
            .clone()
    };
    debug_assert_eq!(entry.reason, reason);
    debug_assert_eq!(entry.estimate_basis.as_deref(), estimate_basis);
    saturating_add(&entry.observations, 1);
    if let Some(bytes) = estimated_physical_bytes {
        saturating_add(&entry.estimated_physical_bytes, bytes);
    }
}

pub fn note_process_unmeasurable(
    seam: &str,
    reason: &str,
    estimated_physical_bytes: Option<u64>,
    estimate_basis: Option<&str>,
) {
    note_unmeasurable(
        PROCESS_ROOT.to_owned(),
        seam,
        reason,
        estimated_physical_bytes,
        estimate_basis,
    );
}

pub fn process_root_counter(domain: Domain) -> Counter {
    register(domain, PROCESS_ROOT)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterRow {
    pub domain: String,
    pub root_id: String,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seam_labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnmeasurableSeam {
    pub root_id: String,
    pub seam: String,
    pub reason: String,
    pub observations: u64,
    pub estimated_physical_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_basis: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTotals {
    pub available: bool,
    pub logical_bytes: Option<u64>,
    pub physical_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub requested_since_ms: u64,
    pub available_since_ms: u64,
    pub complete: bool,
    pub gap_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Census {
    pub since_ms: u64,
    pub until_ms: u64,
    pub root: Option<String>,
    pub writers: Vec<WriterRow>,
    pub process: ProcessTotals,
    pub attributed_physical_bytes: u64,
    pub unmeasurable: Vec<UnmeasurableSeam>,
    pub unmeasurable_physical_bytes_estimate: u64,
    pub unexplained_physical_bytes: Option<i64>,
    pub coverage: Coverage,
}

fn entries_snapshot() -> Vec<Arc<Entry>> {
    registry()
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect()
}

fn unmeasurable_entries_snapshot() -> Vec<Arc<UnmeasurableEntry>> {
    registry()
        .unmeasurable_entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect()
}

fn signed_difference(left: u64, right: u64) -> i64 {
    let difference = i128::from(left) - i128::from(right);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn refresh_recent_top(minute_ts: u64, folded: &[(Arc<Entry>, u64, u64)]) {
    let cutoff = minute_ts.saturating_sub(DEFAULT_WINDOW_MS);
    let mut recent = registry()
        .recent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (entry, _, physical_bytes) in folded {
        if *physical_bytes > 0 {
            recent.push_back(RecentRow {
                minute_ts,
                domain: entry.domain,
                root_id: entry.root_id.clone(),
                physical_bytes: *physical_bytes,
            });
        }
    }
    while recent.front().is_some_and(|row| row.minute_ts < cutoff) {
        recent.pop_front();
    }
    let mut totals = BTreeMap::<(Domain, String), u64>::new();
    for row in recent.iter() {
        *totals.entry((row.domain, row.root_id.clone())).or_default() = totals
            .get(&(row.domain, row.root_id.clone()))
            .copied()
            .unwrap_or(0)
            .saturating_add(row.physical_bytes);
    }
    let mut top = totals
        .into_iter()
        .map(|((domain, root_id), physical_bytes)| WriterRow {
            domain: domain.as_str().to_owned(),
            root_id,
            logical_bytes: 0,
            physical_bytes,
            seam_labels: Vec::new(),
        })
        .collect::<Vec<_>>();
    top.sort_by(|left, right| {
        right
            .physical_bytes
            .cmp(&left.physical_bytes)
            .then_with(|| left.domain.cmp(&right.domain))
            .then_with(|| left.root_id.cmp(&right.root_id))
    });
    top.truncate(3);
    *registry()
        .recent_top
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = top;
}

pub fn recent_top_writers() -> Vec<WriterRow> {
    registry()
        .recent_top
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Fold pending counters and a same-boundary process sample into minute rows.
pub fn fold_minute(
    conn: &mut crate::db::TrackedConnection,
    sampled_at_ms: u64,
) -> rusqlite::Result<()> {
    fold_minute_with_sample(conn, sampled_at_ms, Bytes::capture())
}

fn fold_minute_with_sample(
    conn: &mut crate::db::TrackedConnection,
    sampled_at_ms: u64,
    process_sample: Option<Bytes>,
) -> rusqlite::Result<()> {
    let _fold = registry()
        .fold_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    conn.sample_write_pages();
    let minute_ts = sampled_at_ms / MINUTE_MS * MINUTE_MS;
    let entries = entries_snapshot();
    let folded = entries
        .into_iter()
        .filter_map(|entry| {
            let (logical, physical) = entry.pending();
            (logical > 0 || physical > 0).then_some((entry, logical, physical))
        })
        .collect::<Vec<_>>();
    let folded_unmeasurable = unmeasurable_entries_snapshot()
        .into_iter()
        .filter_map(|entry| {
            let (observations, estimated_physical_bytes) = entry.pending();
            (observations > 0).then_some((entry, observations, estimated_physical_bytes))
        })
        .collect::<Vec<_>>();
    let baseline = registry()
        .process_baseline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let process_delta = baseline
        .bytes
        .zip(process_sample)
        .and_then(|(before, after)| after.delta(before));

    let tx = conn.transaction()?;
    for (entry, logical, physical) in &folded {
        tx.execute(
            "INSERT INTO write_ledger_minutes
             (minute_ts, domain, root_id, logical_bytes, physical_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(minute_ts, domain, root_id) DO UPDATE SET
               logical_bytes = logical_bytes + excluded.logical_bytes,
               physical_bytes = physical_bytes + excluded.physical_bytes",
            rusqlite::params![
                minute_ts,
                entry.domain.as_str(),
                entry.root_id,
                logical,
                physical
            ],
        )?;
    }
    for (entry, observations, estimated_physical_bytes) in &folded_unmeasurable {
        tx.execute(
            "INSERT INTO write_ledger_unmeasurable_minutes
             (minute_ts, root_id, seam, reason, estimate_basis, observations,
              estimated_physical_bytes, estimate_available)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(minute_ts, root_id, seam) DO UPDATE SET
               reason = excluded.reason,
               estimate_basis = excluded.estimate_basis,
               observations = observations + excluded.observations,
               estimated_physical_bytes = estimated_physical_bytes
                 + excluded.estimated_physical_bytes,
               estimate_available = MAX(estimate_available, excluded.estimate_available)",
            rusqlite::params![
                minute_ts,
                entry.root_id,
                entry.seam,
                entry.reason,
                entry.estimate_basis,
                observations,
                estimated_physical_bytes,
                u8::from(entry.estimate_basis.is_some()),
            ],
        )?;
    }
    if let Some(delta) = process_delta {
        tx.execute(
            "INSERT INTO write_ledger_process_minutes
             (minute_ts, logical_bytes, physical_bytes)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(minute_ts) DO UPDATE SET
               logical_bytes = logical_bytes + excluded.logical_bytes,
               physical_bytes = physical_bytes + excluded.physical_bytes",
            rusqlite::params![minute_ts, delta.logical, delta.written],
        )?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO write_ledger_meta(singleton, started_ms) VALUES (1, ?1)",
        [registry().started_ms],
    )?;
    let cutoff = sampled_at_ms.saturating_sub(RETENTION_MS);
    tx.execute(
        "DELETE FROM write_ledger_minutes WHERE minute_ts < ?1",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM write_ledger_process_minutes WHERE minute_ts < ?1",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM write_ledger_unmeasurable_minutes WHERE minute_ts < ?1",
        [cutoff],
    )?;
    tx.commit()?;

    for (entry, logical, physical) in &folded {
        saturating_add(&entry.folded_logical, *logical);
        saturating_add(&entry.folded_physical, *physical);
    }
    for (entry, observations, estimated_physical_bytes) in &folded_unmeasurable {
        saturating_add(&entry.folded_observations, *observations);
        saturating_add(
            &entry.folded_estimated_physical_bytes,
            *estimated_physical_bytes,
        );
    }
    *registry()
        .process_baseline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = ProcessBaseline {
        bytes: process_sample,
        sampled_at_ms,
    };
    refresh_recent_top(minute_ts, &folded);
    conn.sample_write_pages();
    Ok(())
}

pub fn census(
    conn: &rusqlite::Connection,
    since_ms: u64,
    root: Option<&str>,
    until_ms: u64,
) -> rusqlite::Result<Census> {
    census_with_sample(conn, since_ms, root, until_ms, Bytes::capture())
}

fn census_with_sample(
    conn: &rusqlite::Connection,
    since_ms: u64,
    root: Option<&str>,
    until_ms: u64,
    process_sample: Option<Bytes>,
) -> rusqlite::Result<Census> {
    let minute_since = since_ms / MINUTE_MS * MINUTE_MS;
    let mut totals = BTreeMap::<(Domain, String), (u64, u64, BTreeSet<String>)>::new();
    {
        let mut statement = conn.prepare(
            "SELECT domain, root_id, SUM(logical_bytes), SUM(physical_bytes)
             FROM write_ledger_minutes
             WHERE minute_ts >= ?1 AND minute_ts <= ?2
               AND (?3 IS NULL OR root_id = ?3)
             GROUP BY domain, root_id",
        )?;
        let rows = statement.query_map(rusqlite::params![minute_since, until_ms, root], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        for row in rows {
            let (domain, root_id, logical, physical) = row?;
            if let Some(domain) = Domain::parse(&domain) {
                totals.insert((domain, root_id), (logical, physical, BTreeSet::new()));
            }
        }
    }

    for entry in entries_snapshot() {
        if root.is_some_and(|root| root != entry.root_id) {
            continue;
        }
        let (logical, physical) = entry.pending();
        let key = (entry.domain, entry.root_id.clone());
        let seam_labels = entry
            .seam_labels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if logical == 0 && physical == 0 && !totals.contains_key(&key) && seam_labels.is_empty() {
            continue;
        }
        let total = totals.entry(key).or_default();
        total.0 = total.0.saturating_add(logical);
        total.1 = total.1.saturating_add(physical);
        total.2.extend(seam_labels);
    }

    let mut writers = totals
        .into_iter()
        .map(
            |((domain, root_id), (logical_bytes, physical_bytes, seam_labels))| WriterRow {
                domain: domain.as_str().to_owned(),
                root_id,
                logical_bytes,
                physical_bytes,
                seam_labels: seam_labels.into_iter().collect(),
            },
        )
        .collect::<Vec<_>>();
    writers.sort_by(|left, right| {
        right
            .physical_bytes
            .cmp(&left.physical_bytes)
            .then_with(|| right.logical_bytes.cmp(&left.logical_bytes))
            .then_with(|| left.domain.cmp(&right.domain))
            .then_with(|| left.root_id.cmp(&right.root_id))
    });
    let attributed_physical_bytes = writers
        .iter()
        .fold(0_u64, |total, row| total.saturating_add(row.physical_bytes));

    let mut unmeasurable_by_seam = BTreeMap::<(String, String), UnmeasurableSeam>::new();
    {
        let mut statement = conn.prepare(
            "SELECT root_id, seam, MAX(reason), MAX(estimate_basis),
                    SUM(observations), SUM(estimated_physical_bytes), MAX(estimate_available)
             FROM write_ledger_unmeasurable_minutes
             WHERE minute_ts >= ?1 AND minute_ts <= ?2
               AND (?3 IS NULL OR root_id = ?3)
             GROUP BY root_id, seam",
        )?;
        let rows = statement.query_map(rusqlite::params![minute_since, until_ms, root], |row| {
            let estimate_available = row.get::<_, u8>(6)? != 0;
            Ok(UnmeasurableSeam {
                root_id: row.get(0)?,
                seam: row.get(1)?,
                reason: row.get(2)?,
                estimate_basis: row.get(3)?,
                observations: row.get(4)?,
                estimated_physical_bytes: estimate_available.then(|| row.get(5)).transpose()?,
            })
        })?;
        for row in rows {
            let row = row?;
            unmeasurable_by_seam.insert((row.root_id.clone(), row.seam.clone()), row);
        }
    }
    for entry in unmeasurable_entries_snapshot() {
        if root.is_some_and(|root| root != entry.root_id) {
            continue;
        }
        let (observations, estimated_physical_bytes) = entry.pending();
        if observations == 0 {
            continue;
        }
        let row = unmeasurable_by_seam
            .entry((entry.root_id.clone(), entry.seam.clone()))
            .or_insert_with(|| UnmeasurableSeam {
                root_id: entry.root_id.clone(),
                seam: entry.seam.clone(),
                reason: entry.reason.clone(),
                observations: 0,
                estimated_physical_bytes: entry.estimate_basis.as_ref().map(|_| 0),
                estimate_basis: entry.estimate_basis.clone(),
            });
        row.observations = row.observations.saturating_add(observations);
        if let Some(estimate) = &mut row.estimated_physical_bytes {
            *estimate = estimate.saturating_add(estimated_physical_bytes);
        }
    }
    let mut unmeasurable = unmeasurable_by_seam.into_values().collect::<Vec<_>>();
    unmeasurable.sort_by(|left, right| {
        right
            .estimated_physical_bytes
            .cmp(&left.estimated_physical_bytes)
            .then_with(|| left.seam.cmp(&right.seam))
            .then_with(|| left.root_id.cmp(&right.root_id))
    });
    let unmeasurable_physical_bytes_estimate = unmeasurable.iter().fold(0_u64, |total, row| {
        total.saturating_add(row.estimated_physical_bytes.unwrap_or(0))
    });

    let persisted_process = conn.query_row(
        "SELECT COALESCE(SUM(logical_bytes), 0), COALESCE(SUM(physical_bytes), 0)
         FROM write_ledger_process_minutes WHERE minute_ts >= ?1 AND minute_ts <= ?2",
        rusqlite::params![minute_since, until_ms],
        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
    )?;
    let baseline = registry()
        .process_baseline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let live_process = baseline
        .bytes
        .zip(process_sample)
        .and_then(|(before, after)| after.delta(before));
    let process = live_process.map_or(
        ProcessTotals {
            available: baseline.bytes.is_some() || persisted_process != (0, 0),
            logical_bytes: (baseline.bytes.is_some() || persisted_process.0 > 0)
                .then_some(persisted_process.0),
            physical_bytes: (baseline.bytes.is_some() || persisted_process.1 > 0)
                .then_some(persisted_process.1),
        },
        |live| ProcessTotals {
            available: true,
            logical_bytes: Some(persisted_process.0.saturating_add(live.logical)),
            physical_bytes: Some(persisted_process.1.saturating_add(live.written)),
        },
    );

    let durable_start = conn
        .query_row(
            "SELECT started_ms FROM write_ledger_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, u64>(0),
        )
        .unwrap_or(registry().started_ms);
    let available_since_ms = durable_start.min(baseline.sampled_at_ms);
    let coverage = Coverage {
        requested_since_ms: since_ms,
        available_since_ms,
        complete: since_ms >= available_since_ms,
        gap_ms: available_since_ms.saturating_sub(since_ms),
    };
    let classified_physical_bytes =
        attributed_physical_bytes.saturating_add(unmeasurable_physical_bytes_estimate);
    let unexplained_physical_bytes = process
        .physical_bytes
        .map(|physical| signed_difference(physical, classified_physical_bytes));

    Ok(Census {
        since_ms,
        until_ms,
        root: root.map(str::to_owned),
        writers,
        process,
        attributed_physical_bytes,
        unmeasurable,
        unmeasurable_physical_bytes_estimate,
        unexplained_physical_bytes,
        coverage,
    })
}

#[cfg(test)]
fn set_process_baseline_for_test(bytes: Option<Bytes>, sampled_at_ms: u64) {
    *registry()
        .process_baseline
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = ProcessBaseline {
        bytes,
        sampled_at_ms,
    };
}

#[cfg(test)]
pub(crate) fn pending_for_test(domain: Domain, root_id: &str) -> (u64, u64) {
    registry()
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(domain, root_id.to_owned()))
        .map_or((0, 0), |entry| entry.pending())
}

#[cfg(test)]
pub(crate) fn seam_labels_for_test(domain: Domain, root_id: &str) -> Vec<String> {
    registry()
        .entries
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(domain, root_id.to_owned()))
        .map(|entry| {
            entry
                .seam_labels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn unmeasurable_for_test(root_id: &str) -> Vec<UnmeasurableSeam> {
    unmeasurable_entries_snapshot()
        .into_iter()
        .filter(|entry| entry.root_id == root_id)
        .filter_map(|entry| {
            let (observations, estimated_physical_bytes) = entry.pending();
            (observations > 0).then(|| UnmeasurableSeam {
                root_id: entry.root_id.clone(),
                seam: entry.seam.clone(),
                reason: entry.reason.clone(),
                observations,
                estimated_physical_bytes: entry
                    .estimate_basis
                    .as_ref()
                    .map(|_| estimated_physical_bytes),
                estimate_basis: entry.estimate_basis.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        format!(
            "/write-ledger/{label}/{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn minute_fold_writes_only_active_roots_and_prunes_seven_day_history() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let active = test_root("active");
        let idle = test_root("idle");
        let counter = register(Domain::SemanticDelta, active.clone());
        let _idle_counter = register(Domain::SemanticDelta, idle.clone());
        let start = 10 * MINUTE_MS;
        set_process_baseline_for_test(Some(Bytes::default()), start);

        counter.credit(11, 7);
        fold_minute_with_sample(
            &mut conn,
            start,
            Some(Bytes {
                logical: 20,
                written: 10,
                read: 0,
            }),
        )
        .unwrap();
        counter.credit(13, 9);
        fold_minute_with_sample(
            &mut conn,
            start + MINUTE_MS,
            Some(Bytes {
                logical: 40,
                written: 20,
                read: 0,
            }),
        )
        .unwrap();

        let active_rows: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_ledger_minutes WHERE root_id = ?1",
                [&active],
                |row| row.get(0),
            )
            .unwrap();
        let idle_rows: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_ledger_minutes WHERE root_id = ?1",
                [&idle],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_rows, 2);
        assert_eq!(idle_rows, 0);

        conn.execute(
            "INSERT INTO write_ledger_minutes VALUES (?1, 'logs', '/expired', 1, 1)",
            [start],
        )
        .unwrap();
        let retention_tick = start + RETENTION_MS + MINUTE_MS;
        fold_minute_with_sample(
            &mut conn,
            retention_tick,
            Some(Bytes {
                logical: 41,
                written: 21,
                read: 0,
            }),
        )
        .unwrap();
        let expired: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_ledger_minutes WHERE root_id = '/expired'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(expired, 0);
    }

    #[test]
    fn tick_cost_with_fifty_active_roots() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let prefix = test_root("fifty-root-tick");
        for index in 0..50 {
            register(Domain::SearchIndexDelta, format!("{prefix}/{index}")).credit(1024, 4096);
        }
        let sampled_at_ms = now_ms();
        let process = Bytes::capture();
        set_process_baseline_for_test(process, sampled_at_ms.saturating_sub(MINUTE_MS));
        let started = std::time::Instant::now();
        fold_minute_with_sample(&mut conn, sampled_at_ms, process).unwrap();
        let elapsed = started.elapsed();
        let rows: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_ledger_minutes WHERE root_id LIKE ?1",
                [format!("{prefix}/%")],
                |row| row.get(0),
            )
            .unwrap();
        eprintln!(
            "write ledger tick: roots=50 rows={rows} elapsed_us={}",
            elapsed.as_micros()
        );
        assert_eq!(rows, 50);
        assert!(elapsed < std::time::Duration::from_secs(1));
    }

    #[test]
    fn census_reports_coverage_gap_and_same_window_unexplained_delta() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let root = test_root("unattributed");
        let counter = register(Domain::BashTaskIo, root.clone());
        let minute = now_ms() / MINUTE_MS * MINUTE_MS;
        let before = Bytes::capture().unwrap_or_default();
        set_process_baseline_for_test(Some(before), minute);
        counter.credit(25, 40);
        fold_minute_with_sample(
            &mut conn,
            minute,
            Some(Bytes {
                logical: before.logical + 75,
                written: before.written + 100,
                read: before.read,
            }),
        )
        .unwrap();

        let report = census_with_sample(&conn, 0, Some(&root), minute + MINUTE_MS, None).unwrap();
        assert!(!report.coverage.complete);
        assert!(report.coverage.gap_ms > 0);
        assert_eq!(report.attributed_physical_bytes, 40);
        assert_eq!(report.process.physical_bytes, Some(100));
        assert_eq!(report.unmeasurable_physical_bytes_estimate, 0);
        assert_eq!(report.unexplained_physical_bytes, Some(60));
    }

    #[test]
    fn census_excludes_known_unmeasurable_seam_from_unexplained() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let root = test_root("known-unmeasurable");
        let counter = register(Domain::CallgraphRefresh, root.clone());
        let minute = now_ms() / MINUTE_MS * MINUTE_MS;
        let before = Bytes::capture().unwrap_or_default();
        set_process_baseline_for_test(Some(before), minute);
        counter.credit(25, 40);
        counter.note_unmeasurable(
            "db::TrackedConnection::drop",
            "the close-time checkpoint result is unavailable without reopening the live SQLite file set",
            Some(50),
            Some("estimated from outstanding WAL frames observed by the existing hook times the database page size"),
        );
        fold_minute_with_sample(
            &mut conn,
            minute,
            Some(Bytes {
                logical: before.logical + 75,
                written: before.written + 100,
                read: before.read,
            }),
        )
        .unwrap();

        let report =
            census_with_sample(&conn, minute, Some(&root), minute + MINUTE_MS, None).unwrap();
        assert_eq!(report.attributed_physical_bytes, 40);
        assert_eq!(report.unmeasurable_physical_bytes_estimate, 50);
        assert_eq!(report.unexplained_physical_bytes, Some(10));
        assert_eq!(report.unmeasurable.len(), 1);
        assert_eq!(report.unmeasurable[0].seam, "db::TrackedConnection::drop");
        assert_eq!(report.unmeasurable[0].estimated_physical_bytes, Some(50));
    }

    #[test]
    fn callgraph_cold_staging_writes_land_in_the_named_domain_not_unexplained() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut ledger_conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let root = test_root("callgraph-cold-staging");
        // The path shape the cold builder stages under; the file itself is an
        // ordinary database, and attribution must come from the writing
        // connection's own measured pages, never from a second fd on it.
        let staging_path = dir.path().join("corpus.staging.sqlite.tmp.resume");
        let staging = crate::db::TrackedConnection::open_attributed(
            &staging_path,
            crate::db::SqliteStore::CallgraphColdGeneration,
            root.clone(),
        )
        .unwrap();
        staging
            .execute_batch("CREATE TABLE symbols(id INTEGER PRIMARY KEY, name TEXT);")
            .unwrap();
        for id in 0..32 {
            staging
                .execute(
                    "INSERT INTO symbols(id, name) VALUES (?1, ?2)",
                    rusqlite::params![id, format!("name-{id}")],
                )
                .unwrap();
        }
        // The mid-build sample is what a census window during the build sees:
        // before the connection closes, these bytes must already be attributed.
        let credited = staging.sample_write_pages();
        assert!(
            credited > 0,
            "the fixture's writes must be measured before close"
        );

        let minute = now_ms() / MINUTE_MS * MINUTE_MS;
        let before = Bytes::capture().unwrap_or_default();
        set_process_baseline_for_test(Some(before), minute);
        fold_minute_with_sample(
            &mut ledger_conn,
            minute,
            Some(Bytes {
                logical: before.logical,
                written: before.written + credited,
                read: before.read,
            }),
        )
        .unwrap();

        let report =
            census_with_sample(&ledger_conn, minute, Some(&root), minute + MINUTE_MS, None)
                .unwrap();
        let row = report
            .writers
            .iter()
            .find(|row| row.root_id == root)
            .expect("staging writes must be attributed, not left unexplained");
        assert_eq!(row.domain, Domain::CallgraphColdStaging.as_str());
        assert_eq!(report.attributed_physical_bytes, credited);
        assert_eq!(report.unexplained_physical_bytes, Some(0));
    }

    /// Fold one synthetic process sample that wrote exactly `written` bytes and
    /// return the root-scoped census for that minute. Keeping the process delta
    /// synthetic makes `unexplained` depend only on what the writer credited.
    fn census_after_process_wrote(
        ledger_conn: &mut crate::db::TrackedConnection,
        root: &str,
        written: u64,
    ) -> Census {
        let minute = now_ms() / MINUTE_MS * MINUTE_MS;
        let before = Bytes::capture().unwrap_or_default();
        set_process_baseline_for_test(Some(before), minute);
        fold_minute_with_sample(
            ledger_conn,
            minute,
            Some(Bytes {
                logical: before.logical,
                written: before.written + written,
                read: before.read,
            }),
        )
        .unwrap();
        census_with_sample(ledger_conn, minute, Some(root), minute + MINUTE_MS, None).unwrap()
    }

    #[test]
    fn trigram_cold_build_writes_land_in_search_index_build_not_unexplained() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut ledger_conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        for index in 0..16 {
            std::fs::write(
                project.join(format!("file_{index}.rs")),
                format!("pub fn function_{index}() -> u32 {{ {index} * 7 }}\n"),
            )
            .unwrap();
        }
        // The builder keys its credit by the canonical root it indexes.
        let root = std::fs::canonicalize(&project)
            .unwrap()
            .display()
            .to_string();
        let cache_dir = dir.path().join("index").join("trigram-census");
        let credited_before = pending_for_test(Domain::SearchIndexBuild, &root).1;

        let index = crate::search_index::SearchIndex::build_with_limit_to_cache_dir(
            &project,
            1024 * 1024,
            &cache_dir,
        );
        assert!(index.ready, "the fixture build must take the streaming path");

        // The expected value comes from the filesystem, not from the writer's
        // own bookkeeping: the finished cache file is what the build wrote.
        let written = std::fs::metadata(cache_dir.join("cache.bin")).unwrap().len();
        assert!(written > 0);
        let credited = pending_for_test(Domain::SearchIndexBuild, &root)
            .1
            .saturating_sub(credited_before);
        assert_eq!(
            credited, written,
            "the trigram build must credit the physical bytes of cache.bin it wrote"
        );

        let report = census_after_process_wrote(&mut ledger_conn, &root, written);
        let row = report
            .writers
            .iter()
            .find(|row| row.domain == Domain::SearchIndexBuild.as_str())
            .expect("the trigram build must appear as an attributed writer");
        assert_eq!(row.physical_bytes, written);
        assert_eq!(report.attributed_physical_bytes, written);
        assert_eq!(report.unexplained_physical_bytes, Some(0));
    }

    #[test]
    fn inspect_tier2_writes_land_in_inspect_cache_not_unexplained() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut ledger_conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let root = project.display().to_string();
        let cache =
            crate::inspect::InspectCache::open(dir.path().join("inspect"), project.clone())
                .unwrap();
        let page_size = cache.page_size_for_test();
        // WAL growth is read with a metadata stat only: opening another
        // descriptor on a live SQLite file set would drop this process's
        // advisory locks.
        let wal_path = std::path::PathBuf::from(format!("{}-wal", cache.sqlite_path().display()));
        let wal_frames = |path: &std::path::Path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.len().saturating_sub(32) / (page_size + 24))
                .unwrap_or(0)
        };
        let frames_before = wal_frames(&wal_path);
        let main_path = cache.sqlite_path().to_path_buf();
        let main_len = |path: &std::path::Path| std::fs::metadata(path).map_or(0, |m| m.len());
        let main_before = main_len(&main_path);
        let credited_before = pending_for_test(Domain::InspectCache, &root).1;
        // A fresh cache has written its main file (SQLite writes it directly
        // while setting the database up) plus the schema frames in the WAL;
        // both belong to the same census window as the Tier-2 run below.
        let opened = main_before + frames_before.saturating_mul(page_size);
        assert_eq!(
            credited_before, opened,
            "schema pages written at open must be credited"
        );

        // The same two writes a background Tier-2 run makes: per-file
        // contributions, then the aggregate over them.
        let mut upserts = Vec::new();
        for index in 0..24 {
            let source = project.join(format!("src_{index}.ts"));
            std::fs::write(&source, format!("export const value{index} = {index};\n")).unwrap();
            upserts.push(crate::inspect::job::FileContribution::new(
                crate::inspect::job::InspectCategory::DeadCode,
                source.clone(),
                crate::cache_freshness::collect(&source).unwrap(),
                serde_json::json!({
                    "file": format!("src_{index}.ts"),
                    "exports": [{ "symbol": format!("value{index}"), "kind": "const", "line": 1 }],
                    "padding": "x".repeat(2048),
                }),
            ));
        }
        let (hash, _) = cache
            .apply_contribution_updates_for_config(
                crate::inspect::job::InspectCategory::DeadCode,
                crate::inspect::cache::Tier2ContributionUpdates {
                    upserts,
                    ..Default::default()
                },
                &crate::config::Config::default(),
            )
            .unwrap();
        cache
            .store_tier2_aggregate(
                crate::inspect::job::JobKey::for_project_category(
                    crate::inspect::job::InspectCategory::DeadCode,
                ),
                &hash,
                serde_json::json!({ "count": 0, "items": [], "padding": "y".repeat(4096) }),
            )
            .unwrap();

        let written = wal_frames(&wal_path)
            .saturating_sub(frames_before)
            .saturating_mul(page_size)
            + main_len(&main_path).saturating_sub(main_before);
        assert!(written > 0, "the fixture did not write WAL frames");
        let credited = pending_for_test(Domain::InspectCache, &root)
            .1
            .saturating_sub(credited_before);
        assert_eq!(
            credited, written,
            "Tier-2 inspect writes must be credited when they commit, not when the cache closes"
        );

        let report = census_after_process_wrote(&mut ledger_conn, &root, opened + written);
        assert_eq!(report.attributed_physical_bytes, opened + written);
        assert_eq!(report.unexplained_physical_bytes, Some(0));
        drop(cache);
    }

    #[test]
    fn census_keeps_named_zero_byte_residual_rows() {
        let _guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let root = test_root("named-residual");
        let counter = register(Domain::Other, root.clone());
        counter.note_seam_label("fixture residual without a safe byte estimate");

        let report = census_with_sample(&conn, 0, Some(&root), now_ms(), None).unwrap();
        let row = report
            .writers
            .iter()
            .find(|row| row.domain == Domain::Other.as_str())
            .expect("the named residual must remain visible without guessed bytes");
        assert_eq!(row.logical_bytes, 0);
        assert_eq!(row.physical_bytes, 0);
        assert_eq!(
            row.seam_labels,
            vec!["fixture residual without a safe byte estimate".to_owned()]
        );
    }
}
