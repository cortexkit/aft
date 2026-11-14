pub mod profile;
pub use aft_base::memory::*;
#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
#[allow(unused_imports)]
use serde::Serialize;
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::Value;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;

#[cfg(test)]
mod snapshot_cap_tests {
    use super::*;
    #[allow(unused_imports)]
    pub(crate) use aft_base::memory::snapshot_cap_tests::*;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
