pub use aft_base::io_accounting::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[cfg(test)]
mod tests {

    use super::*;
    #[test]
    fn performance_work_counts_real_source_operations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("counter.rs");
        std::fs::write(&path, b"fn counter() {}\n").unwrap();
        let before = work_snapshot();
        let bytes = crate::project_content::read(&path).unwrap();
        assert_eq!(
            crate::cache_freshness::hash_bytes(&bytes),
            blake3::hash(&bytes)
        );
        let _ = crate::search_index::project_walk_builder(root.path())
            .build()
            .count();
        let after = work_snapshot();
        for field in ["source_hashes", "content_reads", "full_walks"] {
            assert!(
                after[field].as_u64().unwrap() > before[field].as_u64().unwrap(),
                "{field}"
            );
        }
        for field in ["source_hash_bytes", "content_read_bytes"] {
            assert!(
                after[field].as_u64().unwrap()
                    >= before[field].as_u64().unwrap() + bytes.len() as u64,
                "{field}"
            );
        }
        for field in [
            "applicability_walks",
            "logical_aggregates",
            "own_generation_folds",
        ] {
            assert!(after[field].is_u64(), "{field}");
        }
        assert_eq!(after["pid"], std::process::id());
    }
}
