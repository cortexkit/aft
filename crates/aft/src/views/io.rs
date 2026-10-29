//! Process-wide work counters and kernel I/O deltas, not per-root counters.
//! Other roots and background work can contribute to every interval. Overlap
//! counts flag publications but cannot rule out other concurrent I/O.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

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

pub(crate) use crate::process_io::Bytes;
#[derive(Default)]
struct Activity {
    active: u64,
    starts: u64,
}
fn activity() -> &'static Mutex<Activity> {
    static STATE: OnceLock<Mutex<Activity>> = OnceLock::new();
    STATE.get_or_init(Mutex::default)
}
/// Counts other publication lifetimes intersecting the measured interval.
pub(crate) struct Overlap {
    active: u64,
    starts: u64,
    registered: bool,
    finished: Option<u64>,
}
impl Overlap {
    pub(crate) fn new(register: bool) -> Self {
        let mut state = activity()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = state.active;
        if register {
            state.active += 1;
            state.starts += 1;
        }
        Self {
            active,
            starts: state.starts,
            registered: register,
            finished: None,
        }
    }
    pub(crate) fn finish(&mut self) -> u64 {
        if let Some(value) = self.finished {
            return value;
        }
        let mut state = activity()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value = self.active + state.starts - self.starts;
        if self.registered {
            state.active -= 1;
        }
        self.finished = Some(value);
        value
    }
}
impl Drop for Overlap {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Manifest,
    Blobs,
    Clone,
    Materialize,
    Closure,
    DerivedOther,
    Cas,
}
/// Disjoint intervals use the same sample at each boundary. Nested derived
/// subtotals are aliases, not additional bytes to add to the top-level sum.
///
/// The derived-output index row is written after the publication's phases,
/// once the publisher has left its commit barrier. That separate interval is
/// `derived_index`; it is part of the derived subtotal and of the total.
pub(crate) struct PublicationIo {
    previous: Option<Bytes>,
    buckets: [Bytes; 7],
    derived_index: Bytes,
    available: bool,
    phase: Phase,
    blob_counter: crate::write_ledger::Counter,
    derived_counter: crate::write_ledger::Counter,
    closure_counter: crate::write_ledger::Counter,
}
impl PublicationIo {
    pub(crate) fn new(root: &std::path::Path) -> Self {
        Self::from_sample(Bytes::capture(), root)
    }
    fn from_sample(sample: Option<Bytes>, root: &std::path::Path) -> Self {
        let root = root.display().to_string();
        Self {
            previous: sample,
            buckets: [Bytes::default(); 7],
            derived_index: Bytes::default(),
            available: sample.is_some(),
            phase: Phase::Manifest,
            blob_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsBlob,
                root.clone(),
            ),
            derived_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsDerived,
                root.clone(),
            ),
            closure_counter: crate::write_ledger::register(
                crate::write_ledger::Domain::ViewsClosure,
                root,
            ),
        }
    }
    fn advance(&mut self, next: Option<Bytes>) {
        if let Some(delta) = self.previous.zip(next).and_then(|(a, b)| b.delta(a)) {
            self.buckets[self.phase as usize] = self.buckets[self.phase as usize].add(delta);
            let counter = match self.phase {
                Phase::Blobs => Some(&self.blob_counter),
                Phase::Clone | Phase::Materialize | Phase::DerivedOther => {
                    Some(&self.derived_counter)
                }
                Phase::Closure => Some(&self.closure_counter),
                Phase::Manifest | Phase::Cas => None,
            };
            if let Some(counter) = counter {
                counter.credit(delta.logical, delta.written);
            }
        } else {
            self.available = false;
        }
        self.previous = next;
    }
    pub(crate) fn enter(&mut self, phase: Phase) {
        self.advance(Bytes::capture());
        self.phase = phase;
    }
    pub(crate) fn finish(&mut self) {
        self.advance(Bytes::capture());
    }
    /// Add the deferred derived-index write, sampled from `before` to `after`
    /// after [`Self::finish`], and credit it to the derived write ledger.
    pub(crate) fn credit_derived_index(&mut self, before: Option<Bytes>, after: Option<Bytes>) {
        match before.zip(after).and_then(|(a, b)| b.delta(a)) {
            Some(delta) => {
                self.derived_index = self.derived_index.add(delta);
                self.derived_counter.credit(delta.logical, delta.written);
            }
            None => self.available = false,
        }
    }
    pub(crate) fn fields(&self) -> String {
        let derived = self.buckets[2..6]
            .iter()
            .copied()
            .fold(self.derived_index, Bytes::add);
        let total = self
            .buckets
            .iter()
            .copied()
            .fold(self.derived_index, Bytes::add);
        let mut fields = format!("io_scope=process io_available={}", self.available);
        for (name, value) in [
            ("manifest", self.buckets[0]),
            ("blobs", self.buckets[1]),
            ("derived", derived),
            ("cas", self.buckets[6]),
            ("derived_clone", self.buckets[2]),
            ("materialize", self.buckets[3]),
            ("closure", self.buckets[4]),
            ("derived_other", self.buckets[5]),
            ("derived_index", self.derived_index),
            ("total", total),
        ] {
            fields.push(' ');
            fields.push_str(&Bytes::fields(self.available.then_some(value), name));
        }
        fields
    }
}
pub(crate) struct Window {
    before: Option<Bytes>,
    overlap: Overlap,
}
impl Window {
    pub(crate) fn new() -> Self {
        Self {
            before: Bytes::capture(),
            overlap: Overlap::new(false),
        }
    }
    pub(crate) fn finish(&mut self) -> String {
        let delta = self
            .before
            .zip(Bytes::capture())
            .and_then(|(a, b)| b.delta(a));
        format!(
            "io_scope=process io_available={} concurrent_publications={} {}",
            delta.is_some(),
            self.overlap.finish(),
            Bytes::fields(delta, "total")
        )
    }
    pub(crate) fn event(kind: &'static str, root: &std::path::Path) -> Event {
        Event {
            window: Self::new(),
            kind,
            root: root.to_owned(),
        }
    }
}
pub(crate) struct Event {
    window: Window,
    kind: &'static str,
    root: std::path::PathBuf,
}
impl Drop for Event {
    fn drop(&mut self) {
        crate::slog_info!(
            "index_event kind={} root={} {}",
            self.kind,
            self.root.display(),
            self.window.finish()
        );
    }
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

    #[test]
    fn publication_byte_buckets_sum_to_process_delta() {
        let bytes = |n| Bytes {
            written: n,
            logical: n * 2,
            read: n * 3,
        };
        let root = std::path::Path::new("/ledger-view-fixture");
        let mut p = PublicationIo::from_sample(Some(bytes(100)), root);
        for (phase, n) in [
            (Phase::Manifest, 101),
            (Phase::Blobs, 103),
            (Phase::Clone, 107),
            (Phase::Materialize, 115),
            (Phase::Closure, 131),
            (Phase::DerivedOther, 163),
            (Phase::Cas, 227),
        ] {
            p.phase = phase;
            p.advance(Some(bytes(n)));
        }
        let fields = p.fields();
        for expected in [
            "manifest_physical_bytes_written=1 ",
            "blobs_physical_bytes_written=2 ",
            "derived_physical_bytes_written=60 ",
            "cas_physical_bytes_written=64 ",
            "total_physical_bytes_written=127 ",
            "total_logical_bytes_written=254 ",
            "total_bytes_read=381",
        ] {
            assert!(fields.contains(expected), "{fields}");
        }
        assert_eq!(
            p.buckets.iter().copied().fold(Bytes::default(), Bytes::add),
            bytes(227).delta(bytes(100)).unwrap()
        );
        let root = root.display().to_string();
        let blob =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsBlob, &root);
        let derived =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsDerived, &root);
        let closure =
            crate::write_ledger::pending_for_test(crate::write_ledger::Domain::ViewsClosure, &root);
        assert_eq!(blob, (4, 2));
        assert_eq!(derived, (88, 44));
        assert_eq!(closure, (32, 16));
        assert_eq!(
            (
                blob.0 + derived.0 + closure.0,
                blob.1 + derived.1 + closure.1
            ),
            (124, 62),
            "ledger uses the exact blobs + derived publication byte buckets"
        );
    }
    #[test]
    fn deferred_derived_index_write_counts_in_derived_total_and_ledger() {
        let bytes = |n| Bytes {
            written: n,
            logical: n * 2,
            read: n * 3,
        };
        let root = std::path::Path::new("/ledger-derived-index-fixture");
        let mut p = PublicationIo::from_sample(Some(bytes(100)), root);
        p.phase = Phase::Materialize;
        p.advance(Some(bytes(110)));
        p.credit_derived_index(Some(bytes(500)), Some(bytes(507)));
        let fields = p.fields();
        for expected in [
            "derived_index_physical_bytes_written=7 ",
            "derived_physical_bytes_written=17 ",
            "total_physical_bytes_written=17 ",
            "total_logical_bytes_written=34 ",
        ] {
            assert!(fields.contains(expected), "{expected}: {fields}");
        }
        assert_eq!(
            crate::write_ledger::pending_for_test(
                crate::write_ledger::Domain::ViewsDerived,
                &root.display().to_string()
            ),
            (34, 17)
        );
        p.credit_derived_index(None, Some(bytes(600)));
        assert!(p.fields().contains("io_available=false"));
    }

    #[test]
    fn missing_counters_are_unknown_not_zero() {
        let mut p = PublicationIo::from_sample(None, std::path::Path::new("/missing"));
        p.advance(Some(Bytes::default()));
        assert!(p.fields().contains("total_physical_bytes_written=unknown"));
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_logical_bytes_observe_file_write() {
        crate::process_io::assert_darwin_logical_bytes_observe_file_write();
    }
}
