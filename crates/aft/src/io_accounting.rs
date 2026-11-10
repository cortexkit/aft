//! Process-wide work counters and the path context attached to I/O errors.
//! The counters are shared by every root and by background work, so a
//! difference between two readings can include work done for other roots.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Keep the error kind and native source available to retry/absence checks
/// while naming the I/O that failed. OS errors alone do not include its path.
pub(crate) fn at(
    operation: &str,
    path: &std::path::Path,
    source: std::io::Error,
) -> std::io::Error {
    std::io::Error::new(
        source.kind(),
        PathIoError {
            operation: operation.to_owned(),
            path: path.to_path_buf(),
            source,
        },
    )
}

#[derive(Debug)]
struct PathIoError {
    operation: String,
    path: std::path::PathBuf,
    source: std::io::Error,
}

impl std::fmt::Display for PathIoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for PathIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Lifetime totals for measured work sites, shared by every root in this process.
/// Relaxed loads are intentionally not a transactional snapshot; difference them
/// only within one daemon lifetime, never sum overlapping per-root intervals.
pub(crate) struct WorkCounters {
    pub source_hashes: AtomicU64,
    pub source_hash_bytes: AtomicU64,
    pub content_reads: AtomicU64,
    pub content_read_bytes: AtomicU64,
    pub full_walks: AtomicU64,
    pub git_subprocesses: AtomicU64,
    pub applicability_walks: AtomicU64,
    pub logical_aggregates: AtomicU64,
    pub own_generation_folds: AtomicU64,
    pub dispatch_blobs_decoded: AtomicU64,
    pub dispatch_files_resolved: AtomicU64,
    pub dispatch_sites_resolved: AtomicU64,
    pub dispatch_full_passes: AtomicU64,
    pub dispatch_type_target_files: AtomicU64,
    pub semantic_store_gets: AtomicU64,
    pub semantic_store_payload_bytes: AtomicU64,
    pub semantic_touch_rows_written: AtomicU64,
    pub semantic_touch_transactions: AtomicU64,
    pub semantic_fills: AtomicU64,
    pub semantic_files_filled: AtomicU64,
}

pub(crate) static WORK: WorkCounters = WorkCounters {
    source_hashes: AtomicU64::new(0),
    source_hash_bytes: AtomicU64::new(0),
    content_reads: AtomicU64::new(0),
    content_read_bytes: AtomicU64::new(0),
    full_walks: AtomicU64::new(0),
    git_subprocesses: AtomicU64::new(0),
    applicability_walks: AtomicU64::new(0),
    logical_aggregates: AtomicU64::new(0),
    own_generation_folds: AtomicU64::new(0),
    dispatch_blobs_decoded: AtomicU64::new(0),
    dispatch_files_resolved: AtomicU64::new(0),
    dispatch_sites_resolved: AtomicU64::new(0),
    dispatch_full_passes: AtomicU64::new(0),
    dispatch_type_target_files: AtomicU64::new(0),
    semantic_store_gets: AtomicU64::new(0),
    semantic_store_payload_bytes: AtomicU64::new(0),
    semantic_touch_rows_written: AtomicU64::new(0),
    semantic_touch_transactions: AtomicU64::new(0),
    semantic_fills: AtomicU64::new(0),
    semantic_files_filled: AtomicU64::new(0),
};

/// Live harness-selected runtimes, including builders before a base is ready.
pub(crate) static BASE_REF_PROTOTYPE_USERS: AtomicUsize = AtomicUsize::new(0);

/// Hash source bytes, counting repeated hashes as repeated work. Payload and
/// embedding-text checksums are not source hashes and must not use this helper.
pub(crate) fn source_hash(bytes: &[u8]) -> blake3::Hash {
    WORK.source_hashes.fetch_add(1, Ordering::Relaxed);
    WORK.source_hash_bytes
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    blake3::hash(bytes)
}

pub(crate) fn work_snapshot() -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "pid": std::process::id(),
        "scope": "process_lifetime_instrumented_sites",
        "base_ref_prototype": BASE_REF_PROTOTYPE_USERS.load(Ordering::Relaxed) > 0,
        "source_hashes": WORK.source_hashes.load(Ordering::Relaxed),
        "source_hash_bytes": WORK.source_hash_bytes.load(Ordering::Relaxed),
        "content_reads": WORK.content_reads.load(Ordering::Relaxed),
        "content_read_bytes": WORK.content_read_bytes.load(Ordering::Relaxed),
        "full_walks": WORK.full_walks.load(Ordering::Relaxed),
        "git_subprocesses": WORK.git_subprocesses.load(Ordering::Relaxed),
        "applicability_walks": WORK.applicability_walks.load(Ordering::Relaxed),
        "logical_aggregates": WORK.logical_aggregates.load(Ordering::Relaxed),
        "own_generation_folds": WORK.own_generation_folds.load(Ordering::Relaxed),
        "dispatch_blobs_decoded": WORK.dispatch_blobs_decoded.load(Ordering::Relaxed),
        "dispatch_files_resolved": WORK.dispatch_files_resolved.load(Ordering::Relaxed),
        "dispatch_sites_resolved": WORK.dispatch_sites_resolved.load(Ordering::Relaxed),
        "dispatch_full_passes": WORK.dispatch_full_passes.load(Ordering::Relaxed),
        "dispatch_type_target_files": WORK.dispatch_type_target_files.load(Ordering::Relaxed),
        "semantic_store_gets": WORK.semantic_store_gets.load(Ordering::Relaxed),
        "semantic_store_payload_bytes": WORK.semantic_store_payload_bytes.load(Ordering::Relaxed),
        "semantic_touch_rows_written": WORK.semantic_touch_rows_written.load(Ordering::Relaxed),
        "semantic_touch_transactions": WORK.semantic_touch_transactions.load(Ordering::Relaxed),
        "semantic_fills": WORK.semantic_fills.load(Ordering::Relaxed),
        "semantic_files_filled": WORK.semantic_files_filled.load(Ordering::Relaxed),
    })
}

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
