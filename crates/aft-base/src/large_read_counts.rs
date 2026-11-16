//! Shared large-read health counters, independent of the readers.

use std::sync::atomic::{AtomicU64, Ordering};

pub const LARGE_READ_BYTES: u64 = 4 * 1024 * 1024;

/// The readers whose large reads health counts, by site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    /// Indexed grep verification (`search_candidate_file`).
    GrepIndexed,
    /// Indexed grep collection by file (`matching_lines_in_content`).
    GrepCollect,
    /// Folder-walk grep (`fallback_search_file`).
    GrepFallback,
    /// The degraded search fallback's read phase.
    DegradedGrep,
    /// The literal presence check.
    Presence,
    /// `matching_line_from_source`.
    MatchingLine,
    /// The reranker's candidate text.
    CandidateText,
    /// `request_source_file` (definition and reference helpers).
    SourceFile,
    /// `lexical_candidate_exactness`.
    LexicalExactness,
    /// Exact-memo digest reads and the direct digest proof.
    MemoDigest,
    /// The external exact sweep.
    ExternalSweep,
    /// Exact-corpus and anchored verification reads.
    ExactCorpus,
    /// Data-file classification.
    DataFile,
    /// Nearest-name lookups.
    NearestNames,
    /// Grep row line windows.
    LineWindows,
    /// Bounded snippet line reads.
    SnippetLines,
    /// `parser::read_source`.
    ParseSource,
}

pub const SITES: [(Site, &str); 17] = [
    (Site::GrepIndexed, "grep_indexed"),
    (Site::GrepCollect, "grep_collect"),
    (Site::GrepFallback, "grep_fallback"),
    (Site::DegradedGrep, "degraded_grep"),
    (Site::Presence, "presence"),
    (Site::MatchingLine, "matching_line"),
    (Site::CandidateText, "candidate_text"),
    (Site::SourceFile, "source_file"),
    (Site::LexicalExactness, "lexical_exactness"),
    (Site::MemoDigest, "memo_digest"),
    (Site::ExternalSweep, "external_sweep"),
    (Site::ExactCorpus, "exact_corpus"),
    (Site::DataFile, "data_file"),
    (Site::NearestNames, "nearest_names"),
    (Site::LineWindows, "line_windows"),
    (Site::SnippetLines, "snippet_lines"),
    (Site::ParseSource, "parse_source"),
];

pub struct Counter {
    pub count: AtomicU64,
    pub bytes: AtomicU64,
    pub largest: AtomicU64,
    pub whole_count: AtomicU64,
    pub whole_bytes: AtomicU64,
    pub skipped: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
pub const ZERO: Counter = Counter {
    count: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
    largest: AtomicU64::new(0),
    whole_count: AtomicU64::new(0),
    whole_bytes: AtomicU64::new(0),
    skipped: AtomicU64::new(0),
};

pub static COUNTERS: [Counter; SITES.len()] = [ZERO; SITES.len()];

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    // Recording tests can exercise the public entry points without racing
    // readers in other tests or resetting their process-wide observations.
    pub static TEST_COUNTERS: std::cell::RefCell<Option<[Counter; SITES.len()]>> = const { std::cell::RefCell::new(None) };
}

pub fn with_counter<T>(site: Site, work: impl FnOnce(&Counter) -> T) -> T {
    #[cfg(any(test, feature = "test-support"))]
    let mut work = Some(work);
    #[cfg(any(test, feature = "test-support"))]
    if let Some(result) = TEST_COUNTERS.with(|counters| {
        counters
            .borrow()
            .as_ref()
            .map(|counters| work.take().unwrap()(&counters[site as usize]))
    }) {
        return result;
    }
    #[cfg(any(test, feature = "test-support"))]
    let work = work.unwrap();
    work(&COUNTERS[site as usize])
}

fn record_read(site: Site, size: u64, bytes: u64, whole: bool) {
    if size <= LARGE_READ_BYTES && bytes <= LARGE_READ_BYTES {
        return;
    }
    with_counter(site, |counter| {
        counter.count.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(bytes, Ordering::Relaxed);
        counter.largest.fetch_max(bytes, Ordering::Relaxed);
        if whole {
            counter.whole_count.fetch_add(1, Ordering::Relaxed);
            counter.whole_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// Count a streamed read at `site` of a file that was `size` bytes at open, of which
/// `bytes` were read, when the file is a large read. Bytes and the largest
/// read are in bytes read.
pub fn record(site: Site, size: u64, bytes: u64) {
    record_read(site, size, bytes, false);
}

/// Count a whole read of `bytes` bytes at `site`.
pub fn record_whole(site: Site, bytes: usize) {
    record_read(site, bytes as u64, bytes as u64, true);
}

/// Twice the largest file production readers touched (8.1 MB), so whole reads
/// cannot turn a growing log into a multi-gigabyte allocation.
pub const WHOLE_READ_CEILING: u64 = 16 * 1024 * 1024;

/// Kept in one place so the agent-visible explanation can be reviewed separately.
pub const WHOLE_READ_SKIP_REASON: &str =
    "Skipped: reading this file would exceed the 16 MiB in-memory read ceiling.";

pub fn allow_whole(site: Site, path: &std::path::Path, size: u64) -> std::io::Result<()> {
    if size <= WHOLE_READ_CEILING {
        return Ok(());
    }
    skip(site, path);
    Err(std::io::Error::other(WHOLE_READ_SKIP_REASON))
}

pub fn skip(site: Site, path: &std::path::Path) {
    with_counter(site, |counter| {
        counter.skipped.fetch_add(1, Ordering::Relaxed);
    });
    crate::bounded_io::note_large_read_skip(path);
}

pub fn is_ceiling_error(error: &std::io::Error) -> bool {
    error.to_string() == WHOLE_READ_SKIP_REASON
}
