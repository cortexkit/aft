//! Runtime census: collect subsystem estimates into the base accounting types.

use std::collections::BTreeMap;

use crate::memory::{
    cap_root_detail, cap_root_rollups, process_phys_footprint_bytes, process_rss_bytes,
    signed_difference, AllocatorObservation, AllocatorSource, MemoryEstimate, MemoryRollupSnapshot,
    MemorySnapshot, ProcessMemorySnapshot, RootMemoryRollup, RootMemorySnapshot,
    SharedProcessMemory, SqliteMemorySnapshot, CENSUS_ALLOCATION_LABELS,
    MEMORY_SNAPSHOT_ROOT_DETAIL_CAP,
};

pub fn process_from_roots(
    roots: &BTreeMap<String, RootMemorySnapshot>,
    shared_semantic_bases: &MemoryEstimate,
) -> ProcessMemorySnapshot {
    let rollups: Vec<_> = roots.values().map(RootMemorySnapshot::rollup).collect();
    process_from_root_rollups(
        rollups.iter(),
        roots.len(),
        shared_semantic_bases,
        crate::data_layer::surfaces::block_cache_estimate().as_ref(),
        AllocatorObservation::measure(),
    )
}

pub(crate) fn process_from_root_rollups<'a>(
    roots: impl Iterator<Item = &'a RootMemoryRollup>,
    root_count: usize,
    shared_semantic_bases: &MemoryEstimate,
    data_layer_block_cache: Option<&MemoryEstimate>,
    allocator_observation: AllocatorObservation,
) -> ProcessMemorySnapshot {
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
    let embedding = crate::local_embed::session_memory();
    let AllocatorObservation {
        snapshot: allocator,
        age_ms: allocator_observation_age_ms,
    } = allocator_observation;
    let data_layer_block_cache_bytes =
        data_layer_block_cache.and_then(|cache| cache.estimated_bytes);
    let total_attributed_bytes = root_attributed_bytes
        .saturating_add(shared_semantic_bases.estimated_bytes.unwrap_or(0))
        .saturating_add(data_layer_block_cache_bytes.unwrap_or(0))
        .saturating_add(embedding.model_bytes)
        .saturating_add(sqlite.memory_used_bytes);
    let rss_bytes = process_rss_bytes();
    let phys_footprint_bytes = process_phys_footprint_bytes();
    // Attribute against the footprint when available: it excludes
    // already-surrendered pages, so the residual actually means
    // "held memory we cannot explain" instead of allocator noise.
    let unattributed_basis = phys_footprint_bytes.or(rss_bytes);
    let unattributed_bytes =
        unattributed_basis.map(|held| signed_difference(held, total_attributed_bytes));
    ProcessMemorySnapshot {
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
        allocator_slack_measured: allocator.retained_slack_bytes.is_some(),
        allocator,
        allocator_observation_age_ms,
        total_attributed_bytes,
        embedding_sessions: embedding.sessions,
        embedding_model_bytes: embedding.model_bytes,
        data_layer_block_cache_bytes,
        unattributed_bytes,
        root_count,
        busy_subsystems,
        not_estimated_subsystems,
    }
}

#[doc(hidden)]
pub fn shared_process_memory() -> SharedProcessMemory {
    SharedProcessMemory {
        shared_semantic_bases: crate::semantic_index::shared_semantic_bases_memory(),
        data_layer_block_cache: crate::data_layer::surfaces::block_cache_estimate(),
    }
}

#[cfg(test)]
pub(crate) fn rollup_snapshot(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemoryRollup>,
    allocator: AllocatorObservation,
) -> MemoryRollupSnapshot {
    rollup_snapshot_with_shared(
        roots_status,
        roots,
        allocator,
        &crate::memory_census::shared_process_memory(),
    )
}

/// Build a rollup with the process-wide shared memory already measured, so
/// one health rollup measures it once for every surface it fills.
#[doc(hidden)]
pub fn rollup_snapshot_with_shared(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemoryRollup>,
    allocator: AllocatorObservation,
    shared: &SharedProcessMemory,
) -> MemoryRollupSnapshot {
    let process = crate::memory_census::process_from_root_rollups(
        roots.values(),
        roots.len(),
        &shared.shared_semantic_bases,
        shared.data_layer_block_cache.as_ref(),
        allocator,
    );
    let roots_total = roots.len();
    let (roots, roots_omitted, roots_omitted_bytes) =
        cap_root_rollups(roots, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP);
    MemoryRollupSnapshot {
        roots_status,
        roots,
        roots_total,
        roots_omitted,
        roots_omitted_bytes,
        process,
    }
}

pub fn snapshot(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemorySnapshot>,
) -> MemorySnapshot {
    snapshot_with_cap(
        roots_status,
        roots,
        true,
        AllocatorObservation::from_source(AllocatorSource::Cached),
        crate::memory_census::shared_process_memory(),
    )
}

/// Build the uncapped form used by the explicit memory census operation.
pub fn snapshot_uncapped(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemorySnapshot>,
) -> MemorySnapshot {
    snapshot_uncapped_with_allocator(roots_status, roots, AllocatorObservation::measure())
}

pub(crate) fn snapshot_uncapped_with_allocator(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemorySnapshot>,
    allocator: AllocatorObservation,
) -> MemorySnapshot {
    snapshot_with_cap(
        roots_status,
        roots,
        false,
        allocator,
        crate::memory_census::shared_process_memory(),
    )
}

/// The uncapped form with the process-wide shared memory already
/// measured; see [`rollup_snapshot_with_shared`].
#[doc(hidden)]
pub fn snapshot_uncapped_with_shared(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemorySnapshot>,
    allocator: AllocatorObservation,
    shared: SharedProcessMemory,
) -> MemorySnapshot {
    snapshot_with_cap(roots_status, roots, false, allocator, shared)
}

fn snapshot_with_cap(
    roots_status: &'static str,
    roots: BTreeMap<String, RootMemorySnapshot>,
    cap_detail: bool,
    allocator: AllocatorObservation,
    shared: SharedProcessMemory,
) -> MemorySnapshot {
    let SharedProcessMemory {
        shared_semantic_bases,
        data_layer_block_cache,
    } = shared;
    // Totals cover EVERY root before the detail map is capped.
    let rollups: Vec<_> = roots.values().map(RootMemorySnapshot::rollup).collect();
    let process = crate::memory_census::process_from_root_rollups(
        rollups.iter(),
        roots.len(),
        &shared_semantic_bases,
        data_layer_block_cache.as_ref(),
        allocator,
    );
    let roots_total = roots.len();
    let (roots, roots_omitted, roots_omitted_bytes) = if cap_detail {
        cap_root_detail(roots, MEMORY_SNAPSHOT_ROOT_DETAIL_CAP)
    } else {
        (roots, 0, 0)
    };
    // Reading the allocation peaks restarts their tracking, so only the census reads them.
    let allocation_labels = if cap_detail {
        Vec::new()
    } else {
        crate::alloc_trace::take_census_peaks(CENSUS_ALLOCATION_LABELS)
    };
    MemorySnapshot {
        roots_status,
        roots,
        roots_total,
        roots_omitted,
        roots_omitted_bytes,
        shared_semantic_bases,
        data_layer_block_cache,
        process,
        allocation_labels,
    }
}
