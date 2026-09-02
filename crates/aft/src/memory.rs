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
        }
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
        }
    }

    fn rollup(&self) -> RootMemoryRollup {
        RootMemoryRollup::from_estimates(&[
            &self.semantic,
            &self.trigram,
            &self.symbols,
            &self.callgraph,
            &self.callgraph_projection,
            &self.inspect,
            &self.bash,
            &self.lsp,
            &self.parser_pool,
        ])
    }

    pub fn busy_subsystem_count(&self) -> usize {
        self.rollup().busy_subsystems
    }

    pub fn not_estimated_subsystem_count(&self) -> usize {
        self.rollup().not_estimated_subsystems
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SqliteMemorySnapshot {
    pub status: &'static str,
    pub memory_used_bytes: u64,
    pub memory_highwater_bytes: u64,
}

impl SqliteMemorySnapshot {
    fn measure() -> Self {
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

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AllocatorMemorySnapshot {
    pub status: &'static str,
    pub bytes_in_use: Option<u64>,
    pub size_allocated: Option<u64>,
    pub retained_slack_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_estimated: Option<&'static str>,
}

impl AllocatorMemorySnapshot {
    fn measured(bytes_in_use: u64, size_allocated: u64) -> Self {
        Self {
            status: "measured",
            bytes_in_use: Some(bytes_in_use),
            size_allocated: Some(size_allocated),
            retained_slack_bytes: Some(size_allocated.saturating_sub(bytes_in_use)),
            not_estimated: None,
        }
    }

    fn not_estimated(reason: &'static str) -> Self {
        Self {
            status: "not_estimated_on_this_platform",
            bytes_in_use: None,
            size_allocated: None,
            retained_slack_bytes: None,
            not_estimated: Some(reason),
        }
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
    pub total_attributed_bytes: u64,
    pub unattributed_bytes: Option<i64>,
    pub root_count: usize,
    pub busy_subsystems: usize,
    pub not_estimated_subsystems: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocatorPressureRelief {
    pub bytes_released: u64,
    pub rss_before_bytes: Option<u64>,
    pub rss_after_bytes: Option<u64>,
    pub allocator_before: AllocatorMemorySnapshot,
    pub allocator_after: AllocatorMemorySnapshot,
}

impl ProcessMemorySnapshot {
    pub fn from_roots(
        roots: &BTreeMap<String, RootMemorySnapshot>,
        shared_semantic_bases: &MemoryEstimate,
    ) -> Self {
        let rollups: Vec<_> = roots.values().map(RootMemorySnapshot::rollup).collect();
        Self::from_root_rollups(rollups.iter(), roots.len(), shared_semantic_bases)
    }

    fn from_root_rollups<'a>(
        roots: impl Iterator<Item = &'a RootMemoryRollup>,
        root_count: usize,
        shared_semantic_bases: &MemoryEstimate,
    ) -> Self {
        let mut root_attributed_bytes = 0u64;
        let mut busy_subsystems = 0usize;
        let mut not_estimated_subsystems = 0usize;
        for root in roots {
            root_attributed_bytes = root_attributed_bytes.saturating_add(root.attributed_bytes);
            busy_subsystems = busy_subsystems.saturating_add(root.busy_subsystems);
            not_estimated_subsystems =
                not_estimated_subsystems.saturating_add(root.not_estimated_subsystems);
        }
        let sqlite = SqliteMemorySnapshot::measure();
        let allocator = allocator_memory_snapshot();
        let total_attributed_bytes = root_attributed_bytes
            .saturating_add(shared_semantic_bases.estimated_bytes.unwrap_or(0))
            .saturating_add(sqlite.memory_used_bytes);
        let rss_bytes = process_rss_bytes();
        let phys_footprint_bytes = process_phys_footprint_bytes();
        // Attribute against the footprint when available: it excludes
        // already-surrendered pages, so the residual actually means
        // "held memory we cannot explain" instead of allocator noise.
        let unattributed_basis = phys_footprint_bytes.or(rss_bytes);
        let unattributed_bytes =
            unattributed_basis.map(|held| signed_difference(held, total_attributed_bytes));
        Self {
            rss_status: if rss_bytes.is_some() {
                "estimated"
            } else {
                "not_estimated_on_this_platform"
            },
            rss_bytes,
            phys_footprint_bytes,
            rss_not_estimated: rss_bytes
                .is_none()
                .then_some("platform_process_rss_unavailable"),
            sqlite,
            allocator,
            total_attributed_bytes,
            unattributed_bytes,
            root_count,
            busy_subsystems,
            not_estimated_subsystems,
        }
    }
}

/// Cap on per-root detail entries in serialized snapshots. Process totals
/// always cover every root; only the per-root breakdown is capped so a
/// many-root daemon process cannot balloon the status payload past
/// downstream consumers' size limits (the daemon metrics cache truncates
/// around 27 KB, and JSON keys serialize alphabetically, so an oversized
/// `memory.roots` map pushes later sections past the cut).
pub const MEMORY_SNAPSHOT_ROOT_DETAIL_CAP: usize = 8;

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
    /// Immutable borrowed semantic snapshots, attributed once process-wide.
    pub shared_semantic_bases: MemoryEstimate,
    pub process: ProcessMemorySnapshot,
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

impl MemoryRollupSnapshot {
    pub(crate) fn new(
        roots_status: &'static str,
        roots: BTreeMap<String, RootMemoryRollup>,
    ) -> Self {
        let shared_semantic_bases = crate::semantic_index::shared_semantic_bases_memory();
        let process = ProcessMemorySnapshot::from_root_rollups(
            roots.values(),
            roots.len(),
            &shared_semantic_bases,
        );
        let roots_total = roots.len();
        let (roots, roots_omitted, roots_omitted_bytes) =
            cap_root_rollups(roots, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        Self {
            roots_status,
            roots,
            roots_total,
            roots_omitted,
            roots_omitted_bytes,
            process,
        }
    }
}

impl MemorySnapshot {
    pub fn new(roots_status: &'static str, roots: BTreeMap<String, RootMemorySnapshot>) -> Self {
        let shared_semantic_bases = crate::semantic_index::shared_semantic_bases_memory();
        // Totals cover EVERY root before the detail map is capped.
        let process = ProcessMemorySnapshot::from_roots(&roots, &shared_semantic_bases);
        let roots_total = roots.len();
        let (roots, roots_omitted, roots_omitted_bytes) =
            cap_root_detail(roots, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
        Self {
            roots_status,
            roots,
            roots_total,
            roots_omitted,
            roots_omitted_bytes,
            shared_semantic_bases,
            process,
        }
    }
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

fn cap_root_rollups(
    roots: BTreeMap<String, RootMemoryRollup>,
    cap: usize,
) -> (BTreeMap<String, RootMemoryRollup>, usize, u64) {
    cap_roots(roots, cap, |snapshot| snapshot.attributed_bytes)
}

/// Keep the `cap` roots with the highest attributed bytes; report the rest
/// as an omitted-count + omitted-bytes rollup.
fn cap_root_detail(
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
        let snapshot = MemorySnapshot::new("ready", roots);

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
        let snapshot = MemoryRollupSnapshot::new("ready", roots);

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

        let snapshot = MemoryRollupSnapshot::new("ready", roots);
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
        let snapshot = MemorySnapshot::new("ready", roots);
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

fn signed_difference(lhs: u64, rhs: u64) -> i64 {
    let difference = i128::from(lhs) - i128::from(rhs);
    difference.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn nonnegative_i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub const fn allocator_backend_name() -> &'static str {
    "mimalloc"
}

fn allocator_memory_snapshot() -> AllocatorMemorySnapshot {
    let Ok(statistics) = mimalloc::MiMalloc::stats_json() else {
        return AllocatorMemorySnapshot::not_estimated("mimalloc_statistics_unavailable");
    };
    let Ok(statistics) = serde_json::from_slice::<Value>(statistics.to_bytes()) else {
        return AllocatorMemorySnapshot::not_estimated("mimalloc_statistics_invalid");
    };
    let current = |field: &str| {
        statistics
            .get(field)
            .and_then(|value| value.get("current"))
            .and_then(Value::as_u64)
    };
    let Some(bytes_in_use) = current("malloc_requested") else {
        return AllocatorMemorySnapshot::not_estimated("mimalloc_statistics_incomplete");
    };
    let Some(size_allocated) = current("committed") else {
        return AllocatorMemorySnapshot::not_estimated("mimalloc_statistics_incomplete");
    };
    AllocatorMemorySnapshot::measured(bytes_in_use, size_allocated)
}

unsafe extern "C" {
    fn mi_collect(force: bool);
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocatorReliefCoverage {
    pub mimalloc: bool,
    pub platform_allocator: bool,
}

pub const fn allocator_relief_coverage() -> AllocatorReliefCoverage {
    AllocatorReliefCoverage {
        mimalloc: true,
        platform_allocator: cfg!(any(
            target_os = "macos",
            all(target_os = "linux", target_env = "gnu")
        )),
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
type MallocTrimFn = unsafe extern "C" fn(libc::size_t) -> libc::c_int;

/// Resolve glibc's optional trimming primitive without a link-time dependency.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn resolved_malloc_trim() -> Option<MallocTrimFn> {
    use std::sync::OnceLock;
    static MALLOC_TRIM: OnceLock<Option<MallocTrimFn>> = OnceLock::new();
    MALLOC_TRIM
        .get_or_init(|| {
            let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"malloc_trim".as_ptr()) };
            if symbol.is_null() {
                None
            } else {
                // SAFETY: glibc declares malloc_trim as `int (size_t)`.
                Some(unsafe { std::mem::transmute::<*mut libc::c_void, MallocTrimFn>(symbol) })
            }
        })
        .as_ref()
        .copied()
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut libc::malloc_zone_t, goal: usize) -> usize;
}

fn relieve_platform_allocator_pressure() -> u64 {
    #[cfg(target_os = "macos")]
    {
        return usize_to_u64(unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) });
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        if let Some(malloc_trim) = resolved_malloc_trim() {
            // SAFETY: resolved_malloc_trim validated the symbol's C ABI.
            unsafe { malloc_trim(0) };
        }
        return 0;
    }
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    {
        0
    }
}

/// Allocator slack (mapped-but-unused arena bytes) above which opportunistic
/// pressure relief is worth the zone-lock contention it briefly causes.
pub const ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES: u64 = 1024 * 1024 * 1024;

/// Minimum spacing between allocator slack scans.
///
/// Keep allocator statistics and collection off the transport thread.
pub const ALLOCATOR_SLACK_SCAN_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(300);

/// Decide whether an allocator slack scan is due.
pub fn allocator_slack_scan_due(
    last_scan: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    match last_scan {
        None => true,
        Some(at) => now.duration_since(at) >= ALLOCATOR_SLACK_SCAN_MIN_INTERVAL,
    }
}

/// Decide whether an opportunistic allocator relief pass is due for a measured
/// slack value.
pub fn allocator_slack_relief_due(retained_slack_bytes: Option<u64>) -> bool {
    retained_slack_bytes.is_some_and(|slack| slack >= ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES)
}

/// Measure allocator slack and return unused pages from a detached thread.
///
/// Returns true when a scan was spawned. The caller records that time so its
/// frequent transport or stdin tick performs only a cheap cadence comparison.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn spawn_allocator_slack_scan_if_due(
    last_scan: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !allocator_slack_scan_due(last_scan, now) {
        return false;
    }
    std::thread::Builder::new()
        .name("aft-mem-relief".to_string())
        .spawn(|| {
            let slack = allocator_memory_snapshot().retained_slack_bytes;
            if !allocator_slack_relief_due(slack) {
                return;
            }
            let relief = relieve_allocator_pressure();
            log::info!(
                "allocator slack relief: released={} allocator_slack_bytes_before={:?} allocator_slack_bytes_after={:?} rss_bytes_before={:?} rss_bytes_after={:?}",
                relief.bytes_released,
                relief.allocator_before.retained_slack_bytes,
                relief.allocator_after.retained_slack_bytes,
                relief.rss_before_bytes,
                relief.rss_after_bytes,
            );
        })
        .is_ok()
}

/// Ask both allocator domains to return unused pages after a process-wide idle
/// gate. Rust allocations use mimalloc. Native libraries can still allocate
/// through the platform allocator, so its relief primitive remains necessary.
pub fn relieve_allocator_pressure() -> AllocatorPressureRelief {
    let rss_before_bytes = process_rss_bytes();
    let allocator_before = allocator_memory_snapshot();
    // SAFETY: `mi_collect` is provided by the linked mimalloc global allocator.
    unsafe { mi_collect(true) };
    let platform_released = relieve_platform_allocator_pressure();
    let allocator_after = allocator_memory_snapshot();
    let rss_after_bytes = process_rss_bytes();
    let allocator_released = allocator_before
        .size_allocated
        .zip(allocator_after.size_allocated)
        .map(|(before, after)| before.saturating_sub(after))
        .unwrap_or(0);
    let rss_released = rss_before_bytes
        .zip(rss_after_bytes)
        .map(|(before, after)| before.saturating_sub(after))
        .unwrap_or(0);
    let bytes_released = allocator_released.max(platform_released).max(rss_released);
    AllocatorPressureRelief {
        bytes_released,
        rss_before_bytes,
        rss_after_bytes,
        allocator_before,
        allocator_after,
    }
}

#[cfg(target_os = "macos")]
fn process_rss_bytes() -> Option<u64> {
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
fn process_phys_footprint_bytes() -> Option<u64> {
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
fn process_phys_footprint_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    resident_pages.checked_mul(page_size as u64)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_preserves_negative_residuals() {
        assert_eq!(signed_difference(5, 8), -3);
    }

    #[test]
    fn allocator_backend_is_mimalloc() {
        assert_eq!(allocator_backend_name(), "mimalloc");
    }

    #[test]
    fn allocator_snapshot_uses_mimalloc_statistics() {
        let snapshot = allocator_memory_snapshot();
        assert_eq!(snapshot.status, "measured");
        assert!(snapshot.bytes_in_use.is_some());
        assert!(snapshot.size_allocated.is_some());
        assert!(snapshot.retained_slack_bytes.is_some());
    }
    #[test]
    fn pressure_relief_covers_rust_and_native_allocators() {
        let coverage = allocator_relief_coverage();
        assert!(coverage.mimalloc);
        #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
        assert!(coverage.platform_allocator);
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn glibc_native_relief_is_runtime_resolved() {
        assert!(resolved_malloc_trim().is_some());
    }

    #[test]
    fn slack_relief_requires_large_measured_slack() {
        let threshold = ALLOCATOR_SLACK_RELIEF_THRESHOLD_BYTES;
        assert!(!allocator_slack_relief_due(None));
        assert!(!allocator_slack_relief_due(Some(threshold - 1)));
        assert!(allocator_slack_relief_due(Some(threshold)));
    }

    #[test]
    fn slack_scan_runs_once_per_interval() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        assert!(allocator_slack_scan_due(None, now));
        assert!(!allocator_slack_scan_due(
            Some(now - Duration::from_secs(10)),
            now
        ));
        assert!(allocator_slack_scan_due(
            Some(now - ALLOCATOR_SLACK_SCAN_MIN_INTERVAL),
            now
        ));
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
        let snapshot = ProcessMemorySnapshot::from_roots(&BTreeMap::new(), &shared);
        assert_eq!(snapshot.sqlite.status, "measured");
        assert!(snapshot.sqlite.memory_highwater_bytes >= snapshot.sqlite.memory_used_bytes);
        assert_eq!(
            snapshot.total_attributed_bytes,
            snapshot.sqlite.memory_used_bytes.saturating_add(7)
        );

        let serialized = serde_json::to_value(&snapshot).expect("serialize process memory");
        assert!(serialized["sqlite"]["memory_used_bytes"].is_u64());
        assert!(serialized["allocator"].get("bytes_in_use").is_some());
        assert!(serialized["allocator"].get("size_allocated").is_some());
        assert!(serialized["allocator"]
            .get("retained_slack_bytes")
            .is_some());
    }

    #[test]
    fn allocator_snapshot_reports_measured_slack() {
        let allocator = allocator_memory_snapshot();
        let in_use = allocator.bytes_in_use.expect("allocator bytes in use");
        let allocated = allocator.size_allocated.expect("allocator size allocated");
        assert_eq!(allocator.status, "measured");
        assert_eq!(
            allocator.retained_slack_bytes,
            Some(allocated.saturating_sub(in_use))
        );
    }

    #[test]
    fn allocator_pressure_relief_smoke() {
        let mut allocation = vec![0u8; 32 * 1024 * 1024];
        for byte in allocation.iter_mut().step_by(4096) {
            *byte = 1;
        }
        std::hint::black_box(&allocation);
        drop(allocation);

        let relief = relieve_allocator_pressure();
        assert_eq!(relief.allocator_before.status, "measured");
        assert_eq!(relief.allocator_after.status, "measured");
    }

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
            "warm-then-idle pressure relief: rss_before={:?} rss_after={:?} allocator_in_use_before={:?} allocator_in_use_after={:?} allocator_allocated_before={:?} allocator_allocated_after={:?} allocator_slack_before={:?} allocator_slack_after={:?} allocator_reported_released={} sqlite_used={} sqlite_highwater={}",
            relief.rss_before_bytes,
            relief.rss_after_bytes,
            relief.allocator_before.bytes_in_use,
            relief.allocator_after.bytes_in_use,
            relief.allocator_before.size_allocated,
            relief.allocator_after.size_allocated,
            relief.allocator_before.retained_slack_bytes,
            relief.allocator_after.retained_slack_bytes,
            relief.bytes_released,
            sqlite.memory_used_bytes,
            sqlite.memory_highwater_bytes,
        );
        assert_eq!(relief.allocator_before.status, "measured");
        assert_eq!(relief.allocator_after.status, "measured");
        assert!(relief.bytes_released > 0);
    }
}
