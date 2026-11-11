pub mod profile;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

/// A cold-path estimate of memory AFT can attribute without allocator hooks.
///
/// `estimated_bytes` is `None` when a subsystem is busy or its resident bytes
/// are not cheaply observable. Counts remain available in those cases so the
/// status response never substitutes a fabricated byte estimate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemoryEstimate {
    pub status: &'static str,
    pub bytes_status: &'static str,
    pub estimated_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_estimated: Vec<String>,
    #[serde(flatten)]
    pub counts: BTreeMap<String, u64>,
}

impl MemoryEstimate {
    pub fn estimated(bytes: u64) -> Self {
        Self {
            status: "ready",
            bytes_status: "estimated",
            estimated_bytes: Some(bytes),
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn partial(bytes: u64) -> Self {
        Self {
            status: "ready",
            bytes_status: "partial",
            estimated_bytes: Some(bytes),
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn not_estimated() -> Self {
        Self {
            status: "ready",
            bytes_status: "not_estimated",
            estimated_bytes: None,
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn busy() -> Self {
        Self {
            status: "busy",
            bytes_status: "not_estimated",
            estimated_bytes: None,
            not_estimated: Vec::new(),
            counts: BTreeMap::new(),
        }
    }

    pub fn count(mut self, name: impl Into<String>, value: usize) -> Self {
        self.counts.insert(name.into(), usize_to_u64(value));
        self
    }

    pub fn count_u64(mut self, name: impl Into<String>, value: u64) -> Self {
        self.counts.insert(name.into(), value);
        self
    }

    pub fn gap(mut self, name: impl Into<String>) -> Self {
        self.not_estimated.push(name.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RootMemorySnapshot {
    pub status: &'static str,
    pub attributed_bytes: u64,
    pub semantic: MemoryEstimate,
    pub trigram: MemoryEstimate,
    pub symbols: MemoryEstimate,
    pub callgraph: MemoryEstimate,
    pub callgraph_projection: MemoryEstimate,
    pub inspect: MemoryEstimate,
    pub bash: MemoryEstimate,
    pub lsp: MemoryEstimate,
    pub parser_pool: MemoryEstimate,
    /// Finished artifact loads handed off by their loader but not yet
    /// installed. Zero almost always; it is a label of its own so a load that
    /// waits is never invisible.
    pub pending_install: MemoryEstimate,
    pub exact_memo: MemoryEstimate,
    pub compression_filters: MemoryEstimate,
    /// What the root's checkout in the data layer holds that its planes
    /// share: the checkout state lists, its open segments and its snapshot.
    /// The bytes each plane holds alone are in `trigram` (text), `callgraph`
    /// and `semantic` (meaning search), each as `data_layer_bytes`; the
    /// block cache every checkout shares is counted once per process.
    /// Present only while the data layer runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_layer: Option<MemoryEstimate>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct RootMemoryRollup {
    pub(crate) status: &'static str,
    pub(crate) attributed_bytes: u64,
    pub(crate) busy_subsystems: usize,
    pub(crate) not_estimated_subsystems: usize,
    /// Present only for roots declared as standing. Keeping the marker on the
    /// existing row uses the same top-eight selection and one omitted-roots
    /// rollup instead of creating a separate standing-memory table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) standing: Option<bool>,
    /// How long ago the index walks behind this row were made, when the
    /// health rollup reused an earlier walk (0 for a walk made now). Unset
    /// outside the health rollup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) measured_age_ms: Option<u64>,
}

impl RootMemoryRollup {
    pub(crate) fn from_estimates(estimates: &[&MemoryEstimate]) -> Self {
        let attributed_bytes = estimates
            .iter()
            .filter_map(|estimate| estimate.estimated_bytes)
            .fold(0u64, u64::saturating_add);
        let busy_subsystems = estimates
            .iter()
            .filter(|estimate| estimate.status == "busy")
            .count();
        let not_estimated_subsystems = estimates
            .iter()
            .filter(|estimate| estimate.estimated_bytes.is_none())
            .count();
        Self {
            status: if busy_subsystems > 0 { "busy" } else { "ready" },
            attributed_bytes,
            busy_subsystems,
            not_estimated_subsystems,
            standing: None,
            measured_age_ms: None,
        }
    }

    /// Record how old the index walks behind this row are.
    pub(crate) fn with_measured_age_ms(mut self, age_ms: u64) -> Self {
        self.measured_age_ms = Some(age_ms);
        self
    }

    /// Attribute this existing per-root rollup to a configured standing entry.
    pub(crate) fn with_standing(mut self) -> Self {
        self.standing = Some(true);
        self
    }
}

impl RootMemorySnapshot {
    pub fn new(
        semantic: MemoryEstimate,
        trigram: MemoryEstimate,
        symbols: MemoryEstimate,
        callgraph: MemoryEstimate,
        callgraph_projection: MemoryEstimate,
        inspect: MemoryEstimate,
        bash: MemoryEstimate,
        lsp: MemoryEstimate,
        parser_pool: MemoryEstimate,
    ) -> Self {
        let rollup = RootMemoryRollup::from_estimates(&[
            &semantic,
            &trigram,
            &symbols,
            &callgraph,
            &callgraph_projection,
            &inspect,
            &bash,
            &lsp,
            &parser_pool,
        ]);
        Self {
            status: rollup.status,
            attributed_bytes: rollup.attributed_bytes,
            semantic,
            trigram,
            symbols,
            callgraph,
            callgraph_projection,
            inspect,
            bash,
            lsp,
            parser_pool,
            pending_install: MemoryEstimate::estimated(0),
            exact_memo: MemoryEstimate::estimated(0),
            compression_filters: MemoryEstimate::estimated(0),
            data_layer: None,
        }
    }

    /// Attach the data layer's estimate for this root, when it runs, and
    /// fold it into the root's status and attributed total.
    pub fn with_data_layer(mut self, data_layer: Option<MemoryEstimate>) -> Self {
        self.data_layer = data_layer;
        let rollup = self.rollup();
        self.status = rollup.status;
        self.attributed_bytes = rollup.attributed_bytes;
        self
    }

    /// Attach the finished-but-uninstalled load estimate and fold it into the
    /// root's status and attributed total.
    pub fn with_pending_install(mut self, pending_install: MemoryEstimate) -> Self {
        self.pending_install = pending_install;
        let rollup = self.rollup();
        self.status = rollup.status;
        self.attributed_bytes = rollup.attributed_bytes;
        self
    }

    pub fn with_query_caches(
        mut self,
        exact_memo: MemoryEstimate,
        compression_filters: MemoryEstimate,
    ) -> Self {
        self.exact_memo = exact_memo;
        self.compression_filters = compression_filters;
        let rollup = self.rollup();
        self.status = rollup.status;
        self.attributed_bytes = rollup.attributed_bytes;
        self
    }

    pub(crate) fn rollup(&self) -> RootMemoryRollup {
        let mut estimates = vec![
            &self.semantic,
            &self.trigram,
            &self.symbols,
            &self.callgraph,
            &self.callgraph_projection,
            &self.inspect,
            &self.bash,
            &self.lsp,
            &self.parser_pool,
            &self.pending_install,
            &self.exact_memo,
            &self.compression_filters,
        ];
        estimates.extend(self.data_layer.as_ref());
        RootMemoryRollup::from_estimates(&estimates)
    }

    pub fn busy_subsystem_count(&self) -> usize {
        self.rollup().busy_subsystems
    }

    pub fn not_estimated_subsystem_count(&self) -> usize {
        self.rollup().not_estimated_subsystems
    }

    /// Bytes released when the root goes idle: retained index handles,
    /// symbol data, inspect caches, and the data layer's checkout, which the
    /// idle reaper's release drops (its planes' bytes are in `trigram`,
    /// `semantic` and `callgraph`, what they share in `data_layer`). This
    /// deliberately shares the already-collected estimates instead of taking
    /// a second estimate on the reply path.
    pub fn evictable_bytes(&self) -> u64 {
        [
            &self.trigram,
            &self.semantic,
            &self.symbols,
            &self.callgraph,
            &self.inspect,
            &self.exact_memo,
        ]
        .into_iter()
        .chain(self.data_layer.as_ref())
        .filter_map(|estimate| estimate.estimated_bytes)
        .fold(0, u64::saturating_add)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SqliteMemorySnapshot {
    pub status: &'static str,
    pub memory_used_bytes: u64,
    pub memory_highwater_bytes: u64,
}

impl SqliteMemorySnapshot {
    pub(crate) fn measure() -> Self {
        // SQLite's allocator counters are process-wide and internally synchronized.
        // They intentionally replace per-connection guesses in root estimates.
        let memory_used = unsafe { rusqlite::ffi::sqlite3_memory_used() };
        let memory_highwater = unsafe { rusqlite::ffi::sqlite3_memory_highwater(0) };
        Self {
            status: "measured",
            memory_used_bytes: nonnegative_i64_to_u64(memory_used),
            memory_highwater_bytes: nonnegative_i64_to_u64(memory_highwater),
        }
    }
}

/// What `retained_slack_bytes` / `allocator_slack_bytes` measure, shown next to
/// the number wherever it is reported so nobody reads it as resident memory.
pub const ALLOCATOR_SLACK_LABEL: &str = "address-space slack: allocator-mapped bytes minus in-use bytes; includes free pages already returned to the OS, so it is not resident memory";

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct JemallocMemorySnapshot {
    pub accounting_source: &'static str,
    pub allocated: u64,
    pub active: u64,
    pub resident: u64,
    pub retained: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AllocatorMemorySnapshot {
    pub status: &'static str,
    pub bytes_in_use: Option<u64>,
    pub size_allocated: Option<u64>,
    /// Address-space slack: `size_allocated - bytes_in_use`. This is NOT
    /// resident memory. Allocators can return free pages without releasing the
    /// address space. Slack can therefore exceed RSS.
    pub retained_slack_bytes: Option<u64>,
    /// Always `ALLOCATOR_SLACK_LABEL`; serialized so status readers see what
    /// `retained_slack_bytes` means.
    pub retained_slack_label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_estimated: Option<&'static str>,
    #[cfg(target_os = "linux")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jemalloc: Option<JemallocMemorySnapshot>,
}

impl AllocatorMemorySnapshot {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn measured(bytes_in_use: u64, size_allocated: u64) -> Self {
        Self {
            status: "measured",
            bytes_in_use: Some(bytes_in_use),
            size_allocated: Some(size_allocated),
            retained_slack_bytes: Some(size_allocated.saturating_sub(bytes_in_use)),
            retained_slack_label: ALLOCATOR_SLACK_LABEL,
            not_estimated: None,
            #[cfg(target_os = "linux")]
            jemalloc: None,
        }
    }

    // A supported allocator can also fail to provide a measurement at runtime.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn not_estimated(reason: &'static str) -> Self {
        Self {
            status: "not_estimated_on_this_platform",
            bytes_in_use: None,
            size_allocated: None,
            retained_slack_bytes: None,
            retained_slack_label: ALLOCATOR_SLACK_LABEL,
            not_estimated: Some(reason),
            #[cfg(target_os = "linux")]
            jemalloc: None,
        }
    }

    fn unavailable(reason: &'static str) -> Self {
        Self {
            status: "unavailable",
            ..Self::not_estimated(reason)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocatorSource {
    /// Use the process-wide observation published by a sampler or health refresh.
    Cached,
    /// Walk allocator statistics now and publish the result for future cached reads.
    Measure,
}

/// One allocator reading shared by all memory surfaces in a health refresh.
#[derive(Clone)]
pub(crate) struct AllocatorObservation {
    pub(crate) snapshot: AllocatorMemorySnapshot,
    pub(crate) age_ms: Option<u64>,
}

impl AllocatorObservation {
    pub(crate) fn from_source(source: AllocatorSource) -> Self {
        let (snapshot, age_ms) = allocator_observation(source);
        Self { snapshot, age_ms }
    }

    pub(crate) fn measure() -> Self {
        Self::from_source(AllocatorSource::Measure)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcessMemorySnapshot {
    pub rss_status: &'static str,
    pub rss_bytes: Option<u64>,
    /// Kernel physical footprint (macOS `phys_footprint`): dirty + compressed +
    /// IOKit pages, excluding clean/reclaimable ones. This is the number
    /// Activity Monitor's "Real Memory" and the OOM killer use. RSS counts
    /// MADV_FREE pages the allocator has already surrendered (the kernel
    /// reclaims them lazily), so RSS can read gigabytes above what the process
    /// actually holds — observed 5.1 GB RSS over a 610 MB footprint. None on
    /// non-macOS platforms (Linux RSS does not have this skew; MADV_FREE'd
    /// pages leave Linux RSS on reclaim, not on advice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phys_footprint_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_not_estimated: Option<&'static str>,
    pub sqlite: SqliteMemorySnapshot,
    /// Allocator bytes overlap the attributed subsystem totals and are an
    /// allocation envelope, not another amount to subtract from RSS.
    pub allocator: AllocatorMemorySnapshot,
    /// Whether allocator slack came from an available allocator measurement.
    pub allocator_slack_measured: bool,
    /// Age of the cached allocator observation. A direct measurement is age zero;
    /// a request-path read is null until the first process-wide observation exists.
    pub allocator_observation_age_ms: Option<u64>,
    pub total_attributed_bytes: u64,
    /// Live local ONNX embedding sessions, shared across roots and query/build workers.
    pub embedding_sessions: u64,
    /// Serialized weight bytes for those sessions, counted once per session.
    /// This estimates model storage, not ORT's transient tensors or allocator slack.
    pub embedding_model_bytes: u64,
    /// Bytes resident in the data layer's block cache, shared by every
    /// checkout and counted once here. Included in `total_attributed_bytes`.
    /// Present only once a checkout of this process has bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_layer_block_cache_bytes: Option<u64>,
    pub unattributed_bytes: Option<i64>,
    pub root_count: usize,
    pub busy_subsystems: usize,
    pub not_estimated_subsystems: usize,
}

/// Source label for `AllocatorPressureRelief::allocator_accounting_bytes` on
/// Linux: the drop in jemalloc's `stats.resident`, not the kernel's RSS.
pub const RELIEF_ACCOUNTING_SOURCE_JEMALLOC: &str = "jemalloc_stats_resident_drop";
/// Source label for `AllocatorPressureRelief::allocator_accounting_bytes` on
/// macOS: the byte count `malloc_zone_pressure_relief` returns, i.e. what
/// libmalloc says it handed back. It is the allocator's own claim, not a
/// measurement of resident memory.
pub const RELIEF_ACCOUNTING_SOURCE_MACOS: &str = "malloc_zone_pressure_relief_return";

/// Resident-memory readings taken on each side of a relief pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResidentReading {
    pub rss_bytes: Option<u64>,
    /// macOS only; `None` elsewhere.
    pub phys_footprint_bytes: Option<u64>,
}

impl ResidentReading {
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    fn read() -> Self {
        Self {
            rss_bytes: process_rss_bytes(),
            phys_footprint_bytes: process_phys_footprint_bytes(),
        }
    }
}

/// What one allocator relief pass observed. There is deliberately no single
/// "released" number: the allocator's accounting and the process's resident
/// memory measure different things and disagree in practice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorPressureRelief {
    /// Bytes the allocator's own accounting says the pass returned. What it
    /// measures depends on the platform; see `allocator_accounting_source`.
    pub allocator_accounting_bytes: u64,
    /// Names the allocator counter used, independently of the kernel readings.
    pub allocator_accounting_source: &'static str,
    /// Observed RSS drop across the pass (`rss_before - rss_after`, floored at
    /// zero). `None` when either reading is unavailable. Other threads can
    /// allocate or free during the pass, so this is an observation of the
    /// process, not an exact attribution to the allocator.
    pub rss_drop_bytes: Option<u64>,
    /// Observed physical-footprint drop across the pass (macOS only). macOS
    /// relief marks pages reusable, which lowers the footprint right away but
    /// can leave them counted in RSS until the kernel reclaims them, so on
    /// macOS this is the better observable than `rss_drop_bytes`.
    pub phys_footprint_drop_bytes: Option<u64>,
    pub rss_before_bytes: Option<u64>,
    pub rss_after_bytes: Option<u64>,
    pub phys_footprint_before_bytes: Option<u64>,
    pub phys_footprint_after_bytes: Option<u64>,
    pub allocator_before: AllocatorMemorySnapshot,
    pub allocator_after: AllocatorMemorySnapshot,
}

impl AllocatorPressureRelief {
    /// Assemble a relief record from the readings around the pass. The
    /// resident drops come only from the resident readings; the allocator
    /// figure comes only from the allocator. Keeping them apart is the point.
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    fn from_observations(
        allocator_accounting_bytes: u64,
        allocator_accounting_source: &'static str,
        resident_before: ResidentReading,
        resident_after: ResidentReading,
        allocator_before: AllocatorMemorySnapshot,
        allocator_after: AllocatorMemorySnapshot,
    ) -> Self {
        let drop = |before: Option<u64>, after: Option<u64>| {
            before
                .zip(after)
                .map(|(before, after)| before.saturating_sub(after))
        };
        Self {
            allocator_accounting_bytes,
            allocator_accounting_source,
            rss_drop_bytes: drop(resident_before.rss_bytes, resident_after.rss_bytes),
            phys_footprint_drop_bytes: drop(
                resident_before.phys_footprint_bytes,
                resident_after.phys_footprint_bytes,
            ),
            rss_before_bytes: resident_before.rss_bytes,
            rss_after_bytes: resident_after.rss_bytes,
            phys_footprint_before_bytes: resident_before.phys_footprint_bytes,
            phys_footprint_after_bytes: resident_after.phys_footprint_bytes,
            allocator_before,
            allocator_after,
        }
    }

    /// Linux form: the allocator figure is the drop in `stats.resident`.
    #[cfg(target_os = "linux")]
    fn from_jemalloc_resident_drop(
        resident_before: ResidentReading,
        resident_after: ResidentReading,
        allocator_before: AllocatorMemorySnapshot,
        allocator_after: AllocatorMemorySnapshot,
    ) -> Self {
        let accounting = allocator_before
            .jemalloc
            .as_ref()
            .map(|stats| stats.resident)
            .zip(
                allocator_after
                    .jemalloc
                    .as_ref()
                    .map(|stats| stats.resident),
            )
            .map(|(before, after)| before.saturating_sub(after))
            .unwrap_or(0);
        Self::from_observations(
            accounting,
            RELIEF_ACCOUNTING_SOURCE_JEMALLOC,
            resident_before,
            resident_after,
            allocator_before,
            allocator_after,
        )
    }
}

/// Cap on per-root detail entries in serialized snapshots. Process totals
/// always cover every root; only the per-root breakdown is capped so a
/// many-root daemon process cannot balloon the status payload past
/// downstream consumers' size limits (the daemon metrics cache truncates
/// around 27 KB, and JSON keys serialize alphabetically, so an oversized
/// `memory.roots` map pushes later sections past the cut).
pub const MEMORY_SNAPSHOT_ROOT_DETAIL_CAP: usize = 8;

/// Operations the census names by their allocation high-water mark.
pub(crate) const CENSUS_ALLOCATION_LABELS: usize = 5;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub roots_status: &'static str,
    /// Top roots by attributed bytes, capped at
    /// [`MEMORY_SNAPSHOT_ROOT_DETAIL_CAP`]; the remainder is summarized by
    /// `roots_omitted` / `roots_omitted_bytes`.
    pub roots: BTreeMap<String, RootMemorySnapshot>,
    /// Total roots attributed (including omitted ones).
    pub roots_total: usize,
    /// Roots summarized out of the detail map.
    pub roots_omitted: usize,
    /// Attributed bytes carried by the omitted roots (already included in
    /// `process.total_attributed_bytes`).
    pub roots_omitted_bytes: u64,
    /// Immutable shared semantic bases, attributed once process-wide: the
    /// snapshots borrowed loads share and the indexes owner roots froze when
    /// they installed them. Root rows count zero for a view of such a base.
    pub shared_semantic_bases: MemoryEstimate,
    /// The data layer's block cache, shared by every checkout and attributed
    /// once process-wide (`process.data_layer_block_cache_bytes`). Present
    /// only once a checkout of this process has bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_layer_block_cache: Option<MemoryEstimate>,
    pub process: ProcessMemorySnapshot,
    /// The operations whose live heap bytes peaked highest since the
    /// previous census, highest first, as the `aft` binary's allocation
    /// tracer counts them. Only the uncapped census carries them, and only
    /// when an operation allocated since the last one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allocation_labels: Vec<crate::alloc_trace::LabelPeak>,
}

/// Memory attributed once for the whole process rather than to any root:
/// shared semantic bases and the data layer's block cache.
#[derive(Debug, Clone)]
pub(crate) struct SharedProcessMemory {
    pub(crate) shared_semantic_bases: MemoryEstimate,
    pub(crate) data_layer_block_cache: Option<MemoryEstimate>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct MemoryRollupSnapshot {
    pub(crate) roots_status: &'static str,
    pub(crate) roots: BTreeMap<String, RootMemoryRollup>,
    pub(crate) roots_total: usize,
    pub(crate) roots_omitted: usize,
    pub(crate) roots_omitted_bytes: u64,
    pub(crate) process: ProcessMemorySnapshot,
}

fn cap_roots<T>(
    roots: BTreeMap<String, T>,
    cap: usize,
    attributed_bytes: impl Fn(&T) -> u64,
) -> (BTreeMap<String, T>, usize, u64) {
    if roots.len() <= cap {
        return (roots, 0, 0);
    }
    let mut entries: Vec<_> = roots.into_iter().collect();
    entries.sort_by(|a, b| {
        attributed_bytes(&b.1)
            .cmp(&attributed_bytes(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    let omitted = entries.split_off(cap);
    let omitted_bytes = omitted
        .iter()
        .map(|(_, snapshot)| attributed_bytes(snapshot))
        .fold(0u64, u64::saturating_add);
    (entries.into_iter().collect(), omitted.len(), omitted_bytes)
}

pub(crate) fn cap_root_rollups(
    roots: BTreeMap<String, RootMemoryRollup>,
    cap: usize,
) -> (BTreeMap<String, RootMemoryRollup>, usize, u64) {
    cap_roots(roots, cap, |snapshot| snapshot.attributed_bytes)
}

/// Keep the `cap` roots with the highest attributed bytes; report the rest
/// as an omitted-count + omitted-bytes rollup.
pub(crate) fn cap_root_detail(
    roots: BTreeMap<String, RootMemorySnapshot>,
    cap: usize,
) -> (BTreeMap<String, RootMemorySnapshot>, usize, u64) {
    cap_roots(roots, cap, |snapshot| snapshot.attributed_bytes)
}

#[cfg(test)]
mod snapshot_cap_tests {
    use super::*;

    fn root_with_bytes(bytes: u64) -> RootMemorySnapshot {
        let estimate = MemoryEstimate::estimated;
        RootMemorySnapshot::new(
            estimate(bytes),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
            estimate(0),
        )
    }

    #[test]
    fn detail_map_capped_but_totals_cover_all_roots() {
        let mut roots = BTreeMap::new();
        for i in 0..(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4) {
            // Distinct sizes so the kept set is deterministic: later roots larger.
            roots.insert(
                format!("/root/{i:02}"),
                root_with_bytes((i as u64 + 1) * 1000),
            );
        }
        let snapshot = crate::memory_census::snapshot("ready", roots);

        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4);
        assert_eq!(snapshot.roots_omitted, 4);
        // The four smallest (1000..=4000) are the omitted ones.
        assert_eq!(snapshot.roots_omitted_bytes, 1000 + 2000 + 3000 + 4000);
        // Largest roots are the ones kept.
        assert!(snapshot
            .roots
            .values()
            .all(|root| root.attributed_bytes > 4000));
        // Process totals include omitted roots' bytes.
        let expected_total: u64 = (1..=(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP as u64 + 4))
            .map(|i| i * 1000)
            .sum();
        assert!(snapshot.process.total_attributed_bytes >= expected_total);
    }

    #[test]
    fn rollup_cap_keeps_only_top_roots_while_totals_cover_all() {
        let mut roots = BTreeMap::new();
        for i in 0..(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4) {
            roots.insert(
                format!("/rollup/{i:02}"),
                root_with_bytes((i as u64 + 1) * 1000).rollup(),
            );
        }
        let snapshot =
            crate::memory_census::rollup_snapshot("ready", roots, AllocatorObservation::measure());

        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 4);
        assert_eq!(snapshot.roots_omitted, 4);
        assert_eq!(snapshot.roots_omitted_bytes, 1000 + 2000 + 3000 + 4000);
        let expected_total: u64 = (1..=(MEMORY_SNAPSHOT_ROOT_DETAIL_CAP as u64 + 4))
            .map(|i| i * 1000)
            .sum();
        assert!(snapshot.process.total_attributed_bytes >= expected_total);
    }

    #[test]
    fn standing_rollup_uses_the_shared_top_eight_cap() {
        let mut roots = BTreeMap::new();
        for i in 0..=MEMORY_SNAPSHOT_ROOT_DETAIL_CAP {
            let rollup = root_with_bytes((i as u64 + 1) * 1000).rollup();
            roots.insert(format!("/root/{i:02}"), rollup);
        }
        roots.insert(
            "standing-artifact-key".to_string(),
            root_with_bytes(20_000).rollup().with_standing(),
        );

        let snapshot =
            crate::memory_census::rollup_snapshot("ready", roots, AllocatorObservation::measure());
        assert_eq!(snapshot.roots.len(), MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        assert_eq!(snapshot.roots_total, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP + 2);
        assert_eq!(snapshot.roots_omitted, 2);
        assert_eq!(
            snapshot.roots["standing-artifact-key"].standing,
            Some(true),
            "standing memory stays in the ordinary per-root table"
        );
    }

    #[test]
    fn under_cap_keeps_everything_with_zero_omitted() {
        let mut roots = BTreeMap::new();
        roots.insert("/a".to_string(), root_with_bytes(10));
        roots.insert("/b".to_string(), root_with_bytes(20));
        let snapshot = crate::memory_census::snapshot("ready", roots);
        assert_eq!(snapshot.roots.len(), 2);
        assert_eq!(snapshot.roots_total, 2);
        assert_eq!(snapshot.roots_omitted, 0);
        assert_eq!(snapshot.roots_omitted_bytes, 0);
    }
}

pub fn path_bytes(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        usize_to_u64(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        usize_to_u64(path.as_os_str().encode_wide().count())
            .saturating_mul(std::mem::size_of::<u16>() as u64)
    }
    #[cfg(not(any(unix, windows)))]
    {
        usize_to_u64(path.to_string_lossy().len())
    }
}

pub fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

pub fn estimated_json_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => std::mem::size_of::<bool>() as u64,
        Value::Number(_) => std::mem::size_of::<serde_json::Number>() as u64,
        Value::String(value) => usize_to_u64(value.len()),
        Value::Array(values) => values
            .iter()
            .map(estimated_json_bytes)
            .fold(0u64, u64::saturating_add),
        Value::Object(values) => values.iter().fold(0u64, |bytes, (key, value)| {
            bytes
                .saturating_add(usize_to_u64(key.len()))
                .saturating_add(estimated_json_bytes(value))
        }),
    }
}

pub(crate) fn signed_difference(lhs: u64, rhs: u64) -> i64 {
    let difference = i128::from(lhs) - i128::from(rhs);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn nonnegative_i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn allocator_memory_snapshot_impl() -> AllocatorMemorySnapshot {
    let mut statistics = std::mem::MaybeUninit::<libc::malloc_statistics_t>::zeroed();
    unsafe {
        libc::malloc_zone_statistics(libc::malloc_default_zone(), statistics.as_mut_ptr());
    }
    let statistics = unsafe { statistics.assume_init() };
    AllocatorMemorySnapshot::measured(
        usize_to_u64(statistics.size_in_use),
        usize_to_u64(statistics.size_allocated),
    )
}

#[cfg(target_os = "linux")]
fn allocator_memory_snapshot_impl() -> AllocatorMemorySnapshot {
    let read = || -> Result<_, tikv_jemalloc_ctl::Error> {
        tikv_jemalloc_ctl::epoch::advance()?;
        Ok((
            tikv_jemalloc_ctl::stats::allocated::read()? as u64,
            tikv_jemalloc_ctl::stats::active::read()? as u64,
            tikv_jemalloc_ctl::stats::resident::read()? as u64,
            tikv_jemalloc_ctl::stats::retained::read()? as u64,
            tikv_jemalloc_ctl::stats::mapped::read()? as u64,
        ))
    };
    let Ok((allocated, active, resident, retained, mapped)) = read() else {
        return AllocatorMemorySnapshot::unavailable("jemalloc_statistics_unavailable");
    };
    // Preserve the existing envelope: bytes_in_use is stats.allocated;
    // size_allocated is mapped + retained virtual address space, not residency;
    // retained_slack_bytes is that envelope minus allocated, not stats.retained.
    let mut snapshot =
        AllocatorMemorySnapshot::measured(allocated, mapped.saturating_add(retained));
    snapshot.jemalloc = Some(JemallocMemorySnapshot {
        accounting_source: "jemalloc",
        allocated,
        active,
        resident,
        retained,
    });
    snapshot
}

/// Return unused Linux allocator pages without measuring counters or RSS.
/// Call-graph build phases use the same purge as the measured idle-relief path.
#[cfg(target_os = "linux")]
pub fn purge_allocator() -> std::io::Result<()> {
    // MALLCTL_ARENAS_ALL is 4096 in jemalloc 5.3.0. This command takes no value.
    let result = unsafe {
        tikv_jemalloc_sys::mallctl(
            c"arena.4096.purge".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(result))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn allocator_memory_snapshot_impl() -> AllocatorMemorySnapshot {
    AllocatorMemorySnapshot::not_estimated("platform_allocator_statistics_unavailable")
}

/// Read allocator-wide statistics. Callers on the dispatch path must use the
/// cached slack scheduler below instead: macOS walks allocator zones under
/// their locks while producing this snapshot.
fn allocator_memory_snapshot() -> AllocatorMemorySnapshot {
    #[cfg(test)]
    {
        THREAD_ALLOCATOR_SNAPSHOT_CALLS.with(|calls| calls.set(calls.get() + 1));
        let name = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        ALLOCATOR_SNAPSHOT_THREADS
            .lock()
            .expect("allocator snapshot test thread log")
            .push(name);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let started = std::time::Instant::now();
    let snapshot = allocator_memory_snapshot_impl();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    LAST_ALLOCATOR_WALK_MS.store(
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    snapshot
}

fn allocator_observation(source: AllocatorSource) -> (AllocatorMemorySnapshot, Option<u64>) {
    match source {
        AllocatorSource::Cached => cached_allocator_observation()
            .map(|(snapshot, age_ms)| (snapshot, Some(age_ms)))
            .unwrap_or_else(|| {
                // A cold cache is not evidence that the allocator lacks counters.
                // Keep the request path cheap while the first sample is pending.
                let snapshot = if cfg!(any(target_os = "macos", target_os = "linux")) {
                    AllocatorMemorySnapshot::unavailable("allocator_observation_unavailable")
                } else {
                    AllocatorMemorySnapshot::not_estimated("allocator_observation_unavailable")
                };
                (snapshot, None)
            }),
        AllocatorSource::Measure => measure_allocator_observation(),
    }
}

fn measure_allocator_observation() -> (AllocatorMemorySnapshot, Option<u64>) {
    // Only the platforms that publish an observation stamp its start; the
    // Windows deny-warnings lane rejects the unused binding otherwise.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let sampled_at = std::time::Instant::now();
    let snapshot = allocator_memory_snapshot();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let sampled_at_ms = allocator_slack_elapsed_ms(sampled_at);
        publish_allocator_slack(&snapshot, sampled_at_ms);
        let age_ms =
            allocator_slack_elapsed_ms(std::time::Instant::now()).saturating_sub(sampled_at_ms);
        (snapshot, Some(age_ms))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        (snapshot, None)
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut libc::malloc_zone_t, goal: usize) -> usize;
}

/// Address-space allocator slack above which opportunistic pressure relief is
/// worth the zone-lock contention it briefly causes. Slack includes free pages
/// the allocator already returned to the OS (it cannot tell which free pages
/// are resident), so it stays above this line after a successful pass and the
/// pass repeats at `ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL`. That is intended: a
/// repeat pass is cheap and returns whatever has been freed since the last one.
pub const ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;

/// Minimum spacing between opportunistic relief passes so a workload that
/// legitimately cycles through large allocations does not thrash the allocator.
pub const ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(300);

/// Periodic allocator readers wait at least this many times the last walk's
/// own duration before walking again, so walks take at most 0.5% of the time.
/// Allocator walks can contend with allocations, so their measured cost bounds
/// sampling frequency even when a fragmented heap makes a walk expensive.
const ALLOCATOR_WALK_COST_FACTOR: u64 = 200;

/// The longest an expensive walk can stretch a periodic reader's reuse window,
/// so a slow allocator still gets a fresh reading every ten minutes.
const ALLOCATOR_WALK_MAX_REUSE_MS: u64 = 10 * 60 * 1000;

/// How long a periodic reader may reuse an allocator observation whose walk
/// took `walk_ms`: `floor_ms` for a cheap walk, stretched to
/// `ALLOCATOR_WALK_COST_FACTOR` times the walk's duration for an expensive
/// one, and never past `ALLOCATOR_WALK_MAX_REUSE_MS` (or `floor_ms`, if that
/// is larger).
pub(crate) fn allocator_reuse_window_ms(floor_ms: u64, walk_ms: u64) -> u64 {
    walk_ms
        .saturating_mul(ALLOCATOR_WALK_COST_FACTOR)
        .clamp(floor_ms, ALLOCATOR_WALK_MAX_REUSE_MS.max(floor_ms))
}

#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
/// The Linux module loop ticks every 250ms. Reuse its process-wide jemalloc
/// observation for at least 60s to bound telemetry work, longer when walks
/// are expensive (see `allocator_reuse_window_ms`).
/// This bounds allocator statistics walks to one per idle minute, or two for a
/// relief pass that uses its before/after snapshots.
const ALLOCATOR_SLACK_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Wall-clock duration of the most recent allocator statistics walk, on any
/// thread, in milliseconds.
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_WALK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const UNKNOWN_ALLOCATOR_SLACK: u64 = u64::MAX;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const UNSAMPLED_AT_MS: u64 = u64::MAX;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const ALLOCATOR_OBSERVATION_UNSAMPLED: u8 = 0;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const ALLOCATOR_OBSERVATION_MEASURED: u8 = 1;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const ALLOCATOR_OBSERVATION_NOT_ESTIMATED: u8 = 2;
#[cfg(any(target_os = "macos", target_os = "linux"))]
static ALLOCATOR_SLACK_CACHE_ORIGIN: std::sync::OnceLock<std::time::Instant> =
    std::sync::OnceLock::new();
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_OBSERVED_ALLOCATOR_SLACK_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_OBSERVED_ALLOCATOR_BYTES_IN_USE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_OBSERVED_ALLOCATOR_SIZE_ALLOCATED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK);
#[cfg(target_os = "linux")]
static LAST_OBSERVED_JEMALLOC: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK) }; 3];
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_OBSERVATION_STATUS: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(ALLOCATOR_OBSERVATION_UNSAMPLED);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_OBSERVATION_AT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNSAMPLED_AT_MS);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static ALLOCATOR_OBSERVATION_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static ALLOCATOR_OBSERVATION_PUBLISH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNSAMPLED_AT_MS);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_SLACK_RELIEF_AT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNSAMPLED_AT_MS);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_SLACK_RELIEF_ACCOUNTING_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_SLACK_RELIEF_RSS_DROP_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK);
#[cfg(any(target_os = "macos", target_os = "linux"))]
static LAST_ALLOCATOR_SLACK_RELIEF_FOOTPRINT_DROP_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(UNKNOWN_ALLOCATOR_SLACK);

#[cfg(test)]
static ALLOCATOR_SNAPSHOT_THREADS: std::sync::Mutex<Vec<String>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
thread_local! {
    /// Per-thread count of allocator statistics walks. Tests that assert a
    /// synchronous render path took no walk read this: the walk, if it
    /// happened, ran on the calling thread. A process-global counter is
    /// wrong for that assertion - sibling tests and the health thread walk
    /// concurrently, and a parallel run read a delta of 1 for a render that
    /// walked nothing. Which-thread questions use the thread-name log.
    static THREAD_ALLOCATOR_SNAPSHOT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn allocator_snapshot_calls_for_test() -> u64 {
    THREAD_ALLOCATOR_SNAPSHOT_CALLS.with(|calls| calls.get())
}

#[cfg(test)]
fn allocator_observation_test_mutex() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn allocator_observation_test_lock() -> std::sync::MutexGuard<'static, ()> {
    allocator_observation_test_mutex()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
pub(crate) fn reset_allocator_observation_for_test() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        ALLOCATOR_OBSERVATION_SEQUENCE.store(0, std::sync::atomic::Ordering::Release);
        LAST_ALLOCATOR_OBSERVATION_STATUS.store(
            ALLOCATOR_OBSERVATION_UNSAMPLED,
            std::sync::atomic::Ordering::Release,
        );
        LAST_OBSERVED_ALLOCATOR_BYTES_IN_USE.store(
            UNKNOWN_ALLOCATOR_SLACK,
            std::sync::atomic::Ordering::Release,
        );
        LAST_OBSERVED_ALLOCATOR_SIZE_ALLOCATED.store(
            UNKNOWN_ALLOCATOR_SLACK,
            std::sync::atomic::Ordering::Release,
        );
        LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.store(
            UNKNOWN_ALLOCATOR_SLACK,
            std::sync::atomic::Ordering::Release,
        );
        LAST_ALLOCATOR_OBSERVATION_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
        LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn allocator_slack_elapsed_ms(now: std::time::Instant) -> u64 {
    let origin = ALLOCATOR_SLACK_CACHE_ORIGIN.get_or_init(|| now);
    now.duration_since(*origin)
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(target_os = "linux")]
fn cached_allocator_slack() -> Option<u64> {
    let slack = LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.load(std::sync::atomic::Ordering::Acquire);
    (slack != UNKNOWN_ALLOCATOR_SLACK).then_some(slack)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cached_allocator_not_estimated_reason() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "jemalloc_statistics_unavailable"
    }
    #[cfg(not(target_os = "linux"))]
    {
        "platform_allocator_statistics_unavailable"
    }
}

/// Return the last process-wide allocator observation without walking allocator
/// zones. The observation timestamp is separate from the sampler reservation so
/// a status request never mistakes an in-progress sample for a fresh reading.
pub fn cached_allocator_observation() -> Option<(AllocatorMemorySnapshot, u64)> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        loop {
            let sequence =
                ALLOCATOR_OBSERVATION_SEQUENCE.load(std::sync::atomic::Ordering::Acquire);
            if sequence % 2 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let sampled_at_ms =
                LAST_ALLOCATOR_OBSERVATION_AT_MS.load(std::sync::atomic::Ordering::Acquire);
            if sampled_at_ms == UNSAMPLED_AT_MS {
                return None;
            }
            let status =
                LAST_ALLOCATOR_OBSERVATION_STATUS.load(std::sync::atomic::Ordering::Acquire);
            let bytes_in_use =
                LAST_OBSERVED_ALLOCATOR_BYTES_IN_USE.load(std::sync::atomic::Ordering::Acquire);
            let size_allocated =
                LAST_OBSERVED_ALLOCATOR_SIZE_ALLOCATED.load(std::sync::atomic::Ordering::Acquire);
            let retained_slack =
                LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.load(std::sync::atomic::Ordering::Acquire);
            #[cfg(target_os = "linux")]
            let jemalloc_counters = LAST_OBSERVED_JEMALLOC
                .each_ref()
                .map(|value| value.load(std::sync::atomic::Ordering::Acquire));
            let sequence_after =
                ALLOCATOR_OBSERVATION_SEQUENCE.load(std::sync::atomic::Ordering::Acquire);
            if sequence != sequence_after || sequence_after % 2 != 0 {
                continue;
            }
            let snapshot = match status {
                ALLOCATOR_OBSERVATION_MEASURED
                    if bytes_in_use != UNKNOWN_ALLOCATOR_SLACK
                        && size_allocated != UNKNOWN_ALLOCATOR_SLACK
                        && retained_slack != UNKNOWN_ALLOCATOR_SLACK =>
                {
                    AllocatorMemorySnapshot {
                        status: "measured",
                        bytes_in_use: Some(bytes_in_use),
                        size_allocated: Some(size_allocated),
                        retained_slack_bytes: Some(retained_slack),
                        retained_slack_label: ALLOCATOR_SLACK_LABEL,
                        not_estimated: None,
                        #[cfg(target_os = "linux")]
                        jemalloc: (!jemalloc_counters.contains(&UNKNOWN_ALLOCATOR_SLACK))
                            .then_some(JemallocMemorySnapshot {
                                accounting_source: "jemalloc",
                                allocated: bytes_in_use,
                                active: jemalloc_counters[0],
                                resident: jemalloc_counters[1],
                                retained: jemalloc_counters[2],
                            }),
                    }
                }
                ALLOCATOR_OBSERVATION_NOT_ESTIMATED => {
                    AllocatorMemorySnapshot::not_estimated(cached_allocator_not_estimated_reason())
                }
                _ => return None,
            };
            let age_ms =
                allocator_slack_elapsed_ms(std::time::Instant::now()).saturating_sub(sampled_at_ms);
            return Some((snapshot, age_ms));
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn publish_allocator_slack(snapshot: &AllocatorMemorySnapshot, sampled_at_ms: u64) {
    let _publish_guard = ALLOCATOR_OBSERVATION_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ALLOCATOR_OBSERVATION_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    LAST_ALLOCATOR_OBSERVATION_STATUS.store(
        if snapshot.status == "measured" {
            ALLOCATOR_OBSERVATION_MEASURED
        } else {
            ALLOCATOR_OBSERVATION_NOT_ESTIMATED
        },
        std::sync::atomic::Ordering::Relaxed,
    );
    LAST_OBSERVED_ALLOCATOR_BYTES_IN_USE.store(
        snapshot.bytes_in_use.unwrap_or(UNKNOWN_ALLOCATOR_SLACK),
        std::sync::atomic::Ordering::Relaxed,
    );
    LAST_OBSERVED_ALLOCATOR_SIZE_ALLOCATED.store(
        snapshot.size_allocated.unwrap_or(UNKNOWN_ALLOCATOR_SLACK),
        std::sync::atomic::Ordering::Relaxed,
    );
    LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.store(
        snapshot
            .retained_slack_bytes
            .unwrap_or(UNKNOWN_ALLOCATOR_SLACK),
        std::sync::atomic::Ordering::Relaxed,
    );
    LAST_ALLOCATOR_OBSERVATION_AT_MS.store(sampled_at_ms, std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "linux")]
    for (counter, value) in LAST_OBSERVED_JEMALLOC.iter().zip(
        snapshot
            .jemalloc
            .as_ref()
            .map_or([UNKNOWN_ALLOCATOR_SLACK; 3], |stats| {
                [stats.active, stats.resident, stats.retained]
            }),
    ) {
        counter.store(value, std::sync::atomic::Ordering::Relaxed);
    }
    ALLOCATOR_OBSERVATION_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Release);
    LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS.store(sampled_at_ms, std::sync::atomic::Ordering::Release);
}

#[cfg(target_os = "linux")]
fn sample_is_stale(now_ms: u64) -> bool {
    sample_is_stale_after(
        LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS.load(std::sync::atomic::Ordering::Acquire),
        now_ms,
        LAST_ALLOCATOR_WALK_MS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Whether a sample taken at `sampled_at` is due for replacement at `now_ms`
/// when the last walk took `walk_ms`.
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
fn sample_is_stale_after(sampled_at: u64, now_ms: u64, walk_ms: u64) -> bool {
    sampled_at == UNSAMPLED_AT_MS
        || now_ms.saturating_sub(sampled_at)
            >= allocator_reuse_window_ms(
                ALLOCATOR_SLACK_SAMPLE_INTERVAL.as_millis() as u64,
                walk_ms,
            )
}

#[cfg(target_os = "linux")]
fn reserve_sample(now_ms: u64) -> Option<u64> {
    let previous = LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS.load(std::sync::atomic::Ordering::Acquire);
    if !sample_is_stale(now_ms) {
        return None;
    }
    LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS
        .compare_exchange(
            previous,
            now_ms,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .ok()
}

#[cfg(target_os = "linux")]
fn reserve_relief(now_ms: u64) -> Option<u64> {
    let previous = LAST_ALLOCATOR_SLACK_RELIEF_AT_MS.load(std::sync::atomic::Ordering::Acquire);
    let due = cached_allocator_slack().is_some_and(|slack| {
        slack >= ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES
            && (previous == UNSAMPLED_AT_MS
                || now_ms.saturating_sub(previous)
                    >= ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL.as_millis() as u64)
    });
    due.then(|| {
        LAST_ALLOCATOR_SLACK_RELIEF_AT_MS
            .compare_exchange(
                previous,
                now_ms,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
    })?
}

#[cfg(target_os = "linux")]
fn log_allocator_relief(relief: AllocatorPressureRelief) {
    log::info!(
        "allocator slack relief: allocator_accounting_bytes={} allocator_accounting_source={} rss_drop_bytes={:?} phys_footprint_drop_bytes={:?} rss_bytes_before={:?} rss_bytes_after={:?} address_space_slack_bytes_before={:?} address_space_slack_bytes_after={:?}",
        relief.allocator_accounting_bytes,
        relief.allocator_accounting_source,
        relief.rss_drop_bytes,
        relief.phys_footprint_drop_bytes,
        relief.rss_before_bytes,
        relief.rss_after_bytes,
        relief.allocator_before.retained_slack_bytes,
        relief.allocator_after.retained_slack_bytes,
    );
}

/// Record the latest periodic relief pass for the memory census.
#[cfg(target_os = "linux")]
fn record_last_allocator_relief(relief: &AllocatorPressureRelief) {
    let ordering = std::sync::atomic::Ordering::Release;
    LAST_ALLOCATOR_SLACK_RELIEF_ACCOUNTING_BYTES.store(relief.allocator_accounting_bytes, ordering);
    LAST_ALLOCATOR_SLACK_RELIEF_RSS_DROP_BYTES.store(
        relief.rss_drop_bytes.unwrap_or(UNKNOWN_ALLOCATOR_SLACK),
        ordering,
    );
    LAST_ALLOCATOR_SLACK_RELIEF_FOOTPRINT_DROP_BYTES.store(
        relief
            .phys_footprint_drop_bytes
            .unwrap_or(UNKNOWN_ALLOCATOR_SLACK),
        ordering,
    );
}

pub fn last_allocator_relief_at_ms() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let value = LAST_ALLOCATOR_SLACK_RELIEF_AT_MS.load(std::sync::atomic::Ordering::Acquire);
        return (value != UNSAMPLED_AT_MS).then_some(value);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Allocator-accounting figure from the latest periodic relief pass; see
/// `allocator_relief_accounting_source` for what it measures on this platform.
pub fn last_allocator_relief_accounting_bytes() -> u64 {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        return LAST_ALLOCATOR_SLACK_RELIEF_ACCOUNTING_BYTES
            .load(std::sync::atomic::Ordering::Acquire);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

/// Observed RSS drop across the latest periodic relief pass, if one ran and
/// both RSS readings were available.
pub fn last_allocator_relief_rss_drop_bytes() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let value =
            LAST_ALLOCATOR_SLACK_RELIEF_RSS_DROP_BYTES.load(std::sync::atomic::Ordering::Acquire);
        return (value != UNKNOWN_ALLOCATOR_SLACK).then_some(value);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Observed physical-footprint drop across the latest periodic relief pass
/// (macOS only).
pub fn last_allocator_relief_phys_footprint_drop_bytes() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let value = LAST_ALLOCATOR_SLACK_RELIEF_FOOTPRINT_DROP_BYTES
            .load(std::sync::atomic::Ordering::Acquire);
        return (value != UNKNOWN_ALLOCATOR_SLACK).then_some(value);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// What `last_allocator_relief_accounting_bytes` measures on this platform,
/// or `None` where no relief pass exists.
pub fn allocator_relief_accounting_source() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        Some(RELIEF_ACCOUNTING_SOURCE_MACOS)
    }
    #[cfg(target_os = "linux")]
    {
        Some(RELIEF_ACCOUNTING_SOURCE_JEMALLOC)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Decide whether an opportunistic allocator relief pass is due.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn allocator_slack_relief_due(
    retained_slack_bytes: Option<u64>,
    last_relief: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let Some(slack) = retained_slack_bytes else {
        return false;
    };
    if slack < ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES {
        return false;
    }
    match last_relief {
        None => true,
        Some(at) => now.duration_since(at) >= ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL,
    }
}

/// On Linux, opportunistically return unused allocator pages without reading allocator
/// statistics on the module loop. The loop only reads cached atomics. When the
/// cached observation is stale, one sampler takes an initial snapshot; if it
/// finds enough slack, the page-return operation reuses it and takes only one
/// additional after snapshot, limiting the pass to two statistics walks.
///
/// `phys_footprint_bytes` cannot make this decision: it includes file mappings
/// and other resident memory, so it cannot identify mapped-but-unused allocator
/// arenas without allocator-zone statistics.
#[cfg(target_os = "linux")]
pub fn spawn_allocator_slack_relief_if_due(now: std::time::Instant) -> bool {
    let now_ms = allocator_slack_elapsed_ms(now);
    if sample_is_stale(now_ms) {
        let Some(previous_sample) = reserve_sample(now_ms) else {
            return false;
        };
        let spawned = std::thread::Builder::new()
            .name("aft-mem-slack-sample".to_string())
            .spawn(move || {
                let before = allocator_memory_snapshot();
                publish_allocator_slack(&before, now_ms);
                if reserve_relief(now_ms).is_some() {
                    let relief = relieve_allocator_pressure_from_snapshot(before);
                    publish_allocator_slack(
                        &relief.allocator_after,
                        allocator_slack_elapsed_ms(std::time::Instant::now()),
                    );
                    record_last_allocator_relief(&relief);
                    log_allocator_relief(relief);
                }
            })
            .is_ok();
        if !spawned {
            let _ = LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS.compare_exchange(
                now_ms,
                previous_sample,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }
        return false;
    }

    let Some(previous_relief) = reserve_relief(now_ms) else {
        return false;
    };
    let spawned = std::thread::Builder::new()
        .name("aft-mem-relief".to_string())
        .spawn(move || {
            let relief = relieve_allocator_pressure();
            publish_allocator_slack(
                &relief.allocator_after,
                allocator_slack_elapsed_ms(std::time::Instant::now()),
            );
            record_last_allocator_relief(&relief);
            log_allocator_relief(relief);
        })
        .is_ok();
    if !spawned {
        let _ = LAST_ALLOCATOR_SLACK_RELIEF_AT_MS.compare_exchange(
            now_ms,
            previous_relief,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }
    spawned
}

/// macOS statistics take every allocator-zone lock; sampling and relief there
/// are explicit-only. Other non-Linux targets have no automatic sampler.
#[cfg(not(target_os = "linux"))]
pub fn spawn_allocator_slack_relief_if_due(_now: std::time::Instant) -> bool {
    false
}

/// Ask the platform allocator to return unused pages after a process-wide idle
/// gate. Callers own that gate because allocator pressure relief can add
/// latency. Linux purges all jemalloc arenas; macOS asks libmalloc for relief.
#[cfg(target_os = "macos")]
fn relieve_allocator_pressure_from_snapshot(
    allocator_before: AllocatorMemorySnapshot,
) -> AllocatorPressureRelief {
    let resident_before = ResidentReading::read();
    // The return value is libmalloc's own count of bytes it handed back. It is
    // reported as the allocator's claim; the footprint and RSS drops recorded
    // alongside it are what the process was actually seen to give up.
    let reported = unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    let allocator_after = allocator_memory_snapshot();
    let resident_after = ResidentReading::read();
    AllocatorPressureRelief::from_observations(
        usize_to_u64(reported),
        RELIEF_ACCOUNTING_SOURCE_MACOS,
        resident_before,
        resident_after,
        allocator_before,
        allocator_after,
    )
}

#[cfg(target_os = "macos")]
pub fn relieve_allocator_pressure() -> AllocatorPressureRelief {
    relieve_allocator_pressure_from_snapshot(allocator_memory_snapshot())
}

/// Hands the allocator's free pages back to the system without measuring
/// anything, for callers that would throw the measurement away. On macOS the
/// measured `relieve_allocator_pressure` reads zone statistics before and after
/// the relief, which takes every allocator-zone lock and stalls every thread
/// that allocates meanwhile; a call-graph build calls this between stages.
#[cfg(target_os = "macos")]
pub fn return_free_pages() {
    unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
}

#[cfg(target_os = "linux")]
pub fn return_free_pages() {
    let _ = purge_allocator();
}

#[cfg(target_os = "linux")]
fn relieve_allocator_pressure_from_snapshot(
    allocator_before: AllocatorMemorySnapshot,
) -> AllocatorPressureRelief {
    let resident_before = ResidentReading::read();
    let _ = purge_allocator();
    let allocator_after = allocator_memory_snapshot();
    let resident_after = ResidentReading::read();
    AllocatorPressureRelief::from_jemalloc_resident_drop(
        resident_before,
        resident_after,
        allocator_before,
        allocator_after,
    )
}

#[cfg(target_os = "linux")]
pub fn relieve_allocator_pressure() -> AllocatorPressureRelief {
    relieve_allocator_pressure_from_snapshot(allocator_memory_snapshot())
}

#[cfg(target_os = "macos")]
pub(crate) fn process_rss_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>();
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            i32::try_from(size).ok()?,
        )
    };
    if written != i32::try_from(size).ok()? {
        return None;
    }
    Some(unsafe { info.assume_init() }.pti_resident_size)
}

/// Kernel physical footprint via `proc_pid_rusage` (`ri_phys_footprint`).
/// See `phys_footprint_bytes` for why this, not RSS, is the headline number.
#[cfg(target_os = "macos")]
pub(crate) fn process_phys_footprint_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let rc = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            usage.as_mut_ptr().cast(),
        )
    };
    if rc != 0 {
        return None;
    }
    Some(unsafe { usage.assume_init() }.ri_phys_footprint)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn process_phys_footprint_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
pub(crate) fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    resident_pages.checked_mul(page_size as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn process_rss_bytes() -> Option<u64> {
    None
}

pub(crate) fn rss_bytes() -> Option<u64> {
    process_rss_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_preserves_negative_residuals() {
        assert_eq!(signed_difference(5, 8), -3);
    }

    #[test]
    fn allocator_reuse_window_stretches_with_walk_cost() {
        // A cheap walk keeps the floor.
        assert_eq!(allocator_reuse_window_ms(30_000, 0), 30_000);
        assert_eq!(allocator_reuse_window_ms(30_000, 150), 30_000);
        // An expensive walk waits 200 times its own duration...
        assert_eq!(allocator_reuse_window_ms(30_000, 1_000), 200_000);
        // ...but never more than ten minutes.
        assert_eq!(allocator_reuse_window_ms(30_000, 6_500), 600_000);
        assert_eq!(allocator_reuse_window_ms(30_000, u64::MAX), 600_000);
        // A floor above the cap still wins.
        assert_eq!(allocator_reuse_window_ms(900_000, 6_500), 900_000);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn slack_sample_waits_longer_after_an_expensive_walk() {
        assert!(sample_is_stale_after(UNSAMPLED_AT_MS, 0, 0));
        // A cheap walk: due once a minute.
        assert!(!sample_is_stale_after(0, 59_999, 20));
        assert!(sample_is_stale_after(0, 60_000, 20));
        // A one-second walk: due after 200 seconds, not after one minute.
        assert!(!sample_is_stale_after(0, 60_000, 1_000));
        assert!(!sample_is_stale_after(0, 199_999, 1_000));
        assert!(sample_is_stale_after(0, 200_000, 1_000));
        // A multi-second glibc walk: capped at ten minutes.
        assert!(!sample_is_stale_after(0, 599_999, 6_500));
        assert!(sample_is_stale_after(0, 600_000, 6_500));
    }

    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn cached_allocator_before_first_publication_is_unavailable_not_unsupported() {
        let _test_guard = allocator_observation_test_lock();
        let publish_guard = ALLOCATOR_OBSERVATION_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_allocator_observation_for_test();
        let calls_before = allocator_snapshot_calls_for_test();

        // Hold publication at startup: a request can arrive before the sampler
        // publishes its first reading, even on a measurable allocator.
        let (allocator, age_ms) = allocator_observation(AllocatorSource::Cached);
        assert_eq!(allocator.status, "unavailable");
        assert_eq!(
            allocator.not_estimated,
            Some("allocator_observation_unavailable")
        );
        assert_eq!(allocator.bytes_in_use, None);
        assert_eq!(allocator.size_allocated, None);
        assert_eq!(allocator.retained_slack_bytes, None);
        assert_eq!(age_ms, None);
        assert_eq!(allocator_snapshot_calls_for_test(), calls_before);

        drop(publish_guard);
        publish_allocator_slack(&measured_allocator(100, 140), 0);
        let (allocator, age_ms) = allocator_observation(AllocatorSource::Cached);
        assert_eq!(allocator.status, "measured");
        assert_eq!(allocator.not_estimated, None);
        assert_eq!(allocator.bytes_in_use, Some(100));
        assert_eq!(allocator.retained_slack_bytes, Some(40));
        assert!(age_ms.is_some());
        assert_eq!(allocator_snapshot_calls_for_test(), calls_before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn loop_slack_decision_never_reads_allocator_statistics() {
        LAST_OBSERVED_ALLOCATOR_SLACK_BYTES.store(
            UNKNOWN_ALLOCATOR_SLACK,
            std::sync::atomic::Ordering::Release,
        );
        LAST_ALLOCATOR_SLACK_SAMPLE_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
        LAST_ALLOCATOR_SLACK_RELIEF_AT_MS
            .store(UNSAMPLED_AT_MS, std::sync::atomic::Ordering::Release);
        ALLOCATOR_SNAPSHOT_THREADS
            .lock()
            .expect("clear allocator snapshot thread log")
            .clear();

        for _ in 0..1_000 {
            assert!(!spawn_allocator_slack_relief_if_due(
                std::time::Instant::now()
            ));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let sampler_ran = || {
            ALLOCATOR_SNAPSHOT_THREADS
                .lock()
                .expect("read allocator snapshot thread log")
                .iter()
                .any(|name| name == "aft-mem-slack-sample")
        };
        while !sampler_ran() {
            assert!(std::time::Instant::now() < deadline, "sampler did not run");
            std::thread::yield_now();
        }
        // The thread log is process-global and other tests in the parallel suite
        // read allocator statistics on their own threads; the property under
        // test is only that THIS thread (the loop stand-in) never did.
        let loop_thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let threads = ALLOCATOR_SNAPSHOT_THREADS
            .lock()
            .expect("read allocator snapshot thread log")
            .clone();
        assert!(
            !threads.iter().any(|name| *name == loop_thread),
            "the loop-side decision must never read allocator statistics: {threads:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_automatic_slack_sampling_and_relief_are_explicit_only() {
        let before = allocator_snapshot_calls_for_test();
        for seconds in [0, 30, 300, 3_600] {
            assert!(!spawn_allocator_slack_relief_if_due(
                std::time::Instant::now() + std::time::Duration::from_secs(seconds)
            ));
        }
        assert_eq!(allocator_snapshot_calls_for_test(), before);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn slack_relief_fires_on_large_slack_and_respects_spacing() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let big = Some(ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES);
        // Unknown slack (allocator stats unavailable) never fires.
        assert!(!allocator_slack_relief_due(None, None, now));
        // Below threshold never fires.
        assert!(!allocator_slack_relief_due(
            Some(ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES - 1),
            None,
            now
        ));
        // At threshold with no prior pass fires.
        assert!(allocator_slack_relief_due(big, None, now));
        // A recent pass suppresses the next one...
        let recent = now - Duration::from_secs(10);
        assert!(!allocator_slack_relief_due(big, Some(recent), now));
        // ...until the minimum spacing has elapsed.
        let stale = now - ALLOCATOR_SLACK_RELIEF_MIN_INTERVAL;
        assert!(allocator_slack_relief_due(big, Some(stale), now));
    }

    #[test]
    fn json_estimator_scales_with_payload_content() {
        let empty = estimated_json_bytes(&serde_json::json!({}));
        let populated = estimated_json_bytes(&serde_json::json!({"message": "hello"}));
        assert_eq!(empty, 0);
        assert!(populated >= 12);
    }

    #[test]
    fn process_snapshot_exposes_sqlite_and_allocator_sections() {
        let shared = MemoryEstimate::estimated(7);
        let snapshot = crate::memory_census::process_from_roots(&BTreeMap::new(), &shared);
        assert_eq!(snapshot.sqlite.status, "measured");
        assert!(snapshot.sqlite.memory_highwater_bytes >= snapshot.sqlite.memory_used_bytes);
        // The data layer's block cache is counted once for the process when
        // a test of this process has bound a checkout.
        assert_eq!(
            snapshot.total_attributed_bytes,
            snapshot
                .sqlite
                .memory_used_bytes
                .saturating_add(7)
                .saturating_add(snapshot.data_layer_block_cache_bytes.unwrap_or(0))
        );

        let serialized = serde_json::to_value(&snapshot).expect("serialize process memory");
        assert!(serialized["sqlite"]["memory_used_bytes"].is_u64());
        assert!(serialized["allocator"].get("bytes_in_use").is_some());
        assert!(serialized["allocator"].get("size_allocated").is_some());
        assert!(serialized["allocator"]
            .get("retained_slack_bytes")
            .is_some());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn allocator_snapshot_reports_measured_slack() {
        let allocator = allocator_memory_snapshot();
        if allocator.status == "measured" {
            let in_use = allocator.bytes_in_use.expect("allocator bytes in use");
            let allocated = allocator.size_allocated.expect("allocator size allocated");
            assert_eq!(
                allocator.retained_slack_bytes,
                Some(allocated.saturating_sub(in_use))
            );
        } else {
            assert_eq!(allocator.status, "not_estimated_on_this_platform");
            assert_eq!(allocator.bytes_in_use, None);
            assert_eq!(allocator.size_allocated, None);
            assert_eq!(allocator.retained_slack_bytes, None);
            assert_eq!(
                allocator.not_estimated,
                Some("jemalloc_statistics_unavailable")
            );
        }
    }

    fn measured_allocator(bytes_in_use: u64, size_allocated: u64) -> AllocatorMemorySnapshot {
        AllocatorMemorySnapshot {
            status: "measured",
            bytes_in_use: Some(bytes_in_use),
            size_allocated: Some(size_allocated),
            retained_slack_bytes: Some(size_allocated.saturating_sub(bytes_in_use)),
            retained_slack_label: ALLOCATOR_SLACK_LABEL,
            not_estimated: None,
            #[cfg(target_os = "linux")]
            jemalloc: Some(JemallocMemorySnapshot {
                accounting_source: "jemalloc",
                allocated: bytes_in_use,
                active: bytes_in_use,
                resident: size_allocated,
                retained: 0,
            }),
        }
    }

    #[cfg(target_os = "linux")]
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    /// Kernel RSS can fall independently of the allocator's resident estimate.
    /// The record must carry both figures, each labelled.
    #[cfg(target_os = "linux")]
    #[test]
    fn relief_reports_rss_drop_when_allocator_accounting_is_unchanged() {
        let in_use = 2 * GIB;
        let size_allocated = in_use + 7560 * MIB;
        let before = ResidentReading {
            rss_bytes: Some(6440 * MIB),
            phys_footprint_bytes: None,
        };
        let after = ResidentReading {
            rss_bytes: Some(4910 * MIB),
            phys_footprint_bytes: None,
        };
        let relief = AllocatorPressureRelief::from_jemalloc_resident_drop(
            before,
            after,
            measured_allocator(in_use, size_allocated),
            measured_allocator(in_use, size_allocated),
        );
        assert_eq!(relief.allocator_accounting_bytes, 0);
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_JEMALLOC
        );
        assert_eq!(relief.rss_drop_bytes, Some(1530 * MIB));
        assert_eq!(relief.rss_before_bytes, Some(6440 * MIB));
        assert_eq!(relief.rss_after_bytes, Some(4910 * MIB));
        assert_eq!(relief.phys_footprint_drop_bytes, None);
        // Slack is unchanged even though memory went back to the OS: it is
        // address space, not residency.
        assert_eq!(
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn relief_keeps_allocator_accounting_and_resident_drops_independent() {
        // jemalloc's resident estimate fell by 64 MiB while RSS fell by 300 MiB.
        let relief = AllocatorPressureRelief::from_jemalloc_resident_drop(
            ResidentReading {
                rss_bytes: Some(1000 * MIB),
                phys_footprint_bytes: None,
            },
            ResidentReading {
                rss_bytes: Some(700 * MIB),
                phys_footprint_bytes: None,
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 436 * MIB),
        );
        assert_eq!(relief.allocator_accounting_bytes, 64 * MIB);
        assert_eq!(relief.rss_drop_bytes, Some(300 * MIB));

        // RSS growth during the pass floors the drop at zero; a missing
        // reading yields no drop rather than a guessed one.
        let grew = AllocatorPressureRelief::from_jemalloc_resident_drop(
            ResidentReading {
                rss_bytes: Some(700 * MIB),
                phys_footprint_bytes: Some(500 * MIB),
            },
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: None,
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 500 * MIB),
        );
        assert_eq!(grew.rss_drop_bytes, Some(0));
        assert_eq!(grew.phys_footprint_drop_bytes, None);
    }

    #[test]
    fn macos_style_relief_reports_allocator_return_and_footprint_drop() {
        let relief = AllocatorPressureRelief::from_observations(
            40 * MIB,
            RELIEF_ACCOUNTING_SOURCE_MACOS,
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: Some(600 * MIB),
            },
            ResidentReading {
                rss_bytes: Some(900 * MIB),
                phys_footprint_bytes: Some(520 * MIB),
            },
            measured_allocator(100 * MIB, 500 * MIB),
            measured_allocator(100 * MIB, 500 * MIB),
        );
        assert_eq!(relief.allocator_accounting_bytes, 40 * MIB);
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_MACOS
        );
        assert_eq!(relief.rss_drop_bytes, Some(0));
        assert_eq!(relief.phys_footprint_drop_bytes, Some(80 * MIB));
    }

    #[test]
    fn allocator_snapshot_labels_slack_as_address_space() {
        let value = serde_json::to_value(measured_allocator(1, 3)).unwrap();
        assert_eq!(value["retained_slack_bytes"], 2);
        assert_eq!(value["retained_slack_label"], ALLOCATOR_SLACK_LABEL);
        assert!(ALLOCATOR_SLACK_LABEL.starts_with("address-space slack"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_allocator_pressure_relief_smoke() {
        let mut allocation = vec![0u8; 32 * 1024 * 1024];
        for byte in allocation.iter_mut().step_by(4096) {
            *byte = 1;
        }
        std::hint::black_box(&allocation);
        drop(allocation);

        let relief = relieve_allocator_pressure();
        assert_eq!(
            relief.allocator_accounting_source,
            RELIEF_ACCOUNTING_SOURCE_JEMALLOC
        );
        assert!(relief.allocator_before.jemalloc.is_some());
        assert!(relief.allocator_after.jemalloc.is_some());
        purge_allocator().expect("jemalloc all-arena purge must succeed");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn allocator_snapshot_is_honest_when_platform_counters_are_unavailable() {
        let allocator = allocator_memory_snapshot();
        assert_eq!(allocator.status, "not_estimated_on_this_platform");
        assert_eq!(allocator.bytes_in_use, None);
        assert_eq!(allocator.size_allocated, None);
        assert_eq!(allocator.retained_slack_bytes, None);
        assert_eq!(
            allocator.not_estimated,
            Some("platform_allocator_statistics_unavailable")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "bounded live RSS experiment; run explicitly after allocator changes"]
    fn allocator_pressure_relief_warm_then_idle_measurement() {
        let warm_pages = (0..16 * 1024)
            .map(|seed| {
                let mut page = Box::new([0u8; 4096]);
                page[0] = seed as u8;
                page
            })
            .collect::<Vec<_>>();
        std::hint::black_box(&warm_pages);
        drop(warm_pages);

        let relief = relieve_allocator_pressure();
        let sqlite = SqliteMemorySnapshot::measure();
        eprintln!(
            "warm-then-idle pressure relief: rss_before={:?} rss_after={:?} footprint_before={:?} footprint_after={:?} allocator_in_use_before={:?} allocator_in_use_after={:?} allocator_allocated_before={:?} allocator_allocated_after={:?} address_space_slack_before={:?} address_space_slack_after={:?} allocator_accounting={} ({}) sqlite_used={} sqlite_highwater={}",
            relief.rss_before_bytes,
            relief.rss_after_bytes,
            relief.phys_footprint_before_bytes,
            relief.phys_footprint_after_bytes,
            relief.allocator_before.bytes_in_use,
            relief.allocator_after.bytes_in_use,
            relief.allocator_before.size_allocated,
            relief.allocator_after.size_allocated,
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes,
            relief.allocator_accounting_bytes,
            relief.allocator_accounting_source,
            sqlite.memory_used_bytes,
            sqlite.memory_highwater_bytes,
        );
        assert_eq!(relief.allocator_before.status, "measured");
        assert_eq!(relief.allocator_after.status, "measured");
    }
}
