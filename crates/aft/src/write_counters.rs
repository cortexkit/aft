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
pub(crate) const MINUTE_MS: u64 = 60_000;
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
pub(crate) struct Entry {
    pub(crate) domain: Domain,
    pub(crate) root_id: String,
    pub(crate) logical: AtomicU64,
    pub(crate) physical: AtomicU64,
    pub(crate) folded_logical: AtomicU64,
    pub(crate) folded_physical: AtomicU64,
    pub(crate) seam_labels: Mutex<BTreeSet<String>>,
}

#[derive(Debug)]
pub(crate) struct UnmeasurableEntry {
    pub(crate) root_id: String,
    pub(crate) seam: String,
    pub(crate) reason: String,
    pub(crate) estimate_basis: Option<String>,
    pub(crate) observations: AtomicU64,
    pub(crate) estimated_physical_bytes: AtomicU64,
    pub(crate) folded_observations: AtomicU64,
    pub(crate) folded_estimated_physical_bytes: AtomicU64,
}

impl UnmeasurableEntry {
    pub(crate) fn pending(&self) -> (u64, u64) {
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
    pub(crate) fn pending(&self) -> (u64, u64) {
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

    /// Record a write operation whose bytes cannot be measured without changing
    /// the operation being observed. Estimates stay separate from attributed bytes.
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

#[allow(deprecated)] // fetch_update became try_update in Rust 1.99; the MSRV (1.92) lacks try_update
pub(crate) fn saturating_add(value: &AtomicU64, delta: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(delta))
    });
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessBaseline {
    pub(crate) bytes: Option<Bytes>,
    pub(crate) sampled_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RecentRow {
    pub(crate) minute_ts: u64,
    pub(crate) domain: Domain,
    pub(crate) root_id: String,
    pub(crate) physical_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct Registry {
    pub(crate) started_ms: u64,
    pub(crate) entries: Mutex<BTreeMap<(Domain, String), Arc<Entry>>>,
    pub(crate) unmeasurable_entries: Mutex<BTreeMap<(String, String), Arc<UnmeasurableEntry>>>,
    pub(crate) process_baseline: Mutex<ProcessBaseline>,
    pub(crate) fold_lock: Mutex<()>,
    pub(crate) recent: Mutex<VecDeque<RecentRow>>,
    pub(crate) recent_top: Mutex<Vec<WriterRow>>,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) fn registry() -> &'static Registry {
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
