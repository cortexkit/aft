pub use aft_base::io_accounting::*;

#[cfg(test)]
mod tests {
    #[test]
    fn path_context_preserves_absence_and_retry_kinds() {
        for kind in [std::io::ErrorKind::NotFound, std::io::ErrorKind::WouldBlock] {
            let error = super::at(
                "reading",
                std::path::Path::new("missing-input"),
                std::io::Error::new(kind, "original failure"),
            );
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), "reading missing-input: original failure");
        }
    }

    #[test]
    fn path_context_preserves_native_error_sources() {
        let error = super::at(
            "opening",
            std::path::Path::new("busy-database"),
            std::io::Error::from_raw_os_error(32),
        );
        let context = error
            .get_ref()
            .unwrap()
            .downcast_ref::<super::PathIoError>()
            .unwrap();
        assert_eq!(context.source.raw_os_error(), Some(32));
        assert_eq!(
            std::error::Error::source(context)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(32)
        );
        assert!(error.to_string().starts_with("opening busy-database: "));
    }

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
