//! Named wait timers for one search call.
//!
//! A search call can block in many places: on a lock another request or a
//! background build holds, on the query-embedding worker, on a Rayon join, on
//! a file read. The slow-call log line already reports a call's total wall
//! time and its thread CPU time; the difference between the two is time the
//! searching thread spent not running, and these timers say where it went.
//!
//! Collection is per thread and only while a [`WaitScope`] is open on that
//! thread, so the same helpers called from build, fill or other tool threads
//! cost one thread-local flag read and record nothing. A timer is two
//! `Instant` reads and a fixed-size array update: no allocation per wait.
//!
//! Timers of different kinds may nest (a source file read inside the exact
//! memo's single-flight wait, for example), so the per-kind totals must not
//! be summed into one "waited" figure. A timer nested inside another timer of
//! the same kind is not counted twice.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// One place a search call can block, or one known CPU-heavy step timed so
/// the slow-call line can tell it apart from waiting. The names are the labels
/// the slow-call line and the wait-measurement example print.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum SearchWait {
    /// The context's data-layer checkout slot (`AppContext::data_checkout`),
    /// held while a checkout binds; a bind scans the folder.
    CheckoutSlot,
    /// Cloning the data layer's current checkout view for the text lanes,
    /// under the checkout's `live` read lock.
    TextView,
    /// Reading the data layer's meaning plane state (`DataSemantic::current`
    /// after the checkout slot): the meaning plane's mutexes and the vector
    /// plane's `loaded`/`members` mutexes, which can be held across a
    /// generation reload or a member-cache build.
    MeaningView,
    /// Artifact reads through `try_read_with_budget`: the legacy semantic
    /// status and index and the symbol cache, each retried in 1 ms sleeps
    /// while a writer holds it, up to the interactive budget.
    Readiness,
    /// Waiting for the root context's query embedding model slot (the
    /// `semantic_embedding_model` mutex). Another search of the same root
    /// holds it for its whole embedding round trip.
    ModelLock,
    /// The whole query embedding: building the query model on first use, the
    /// query-cache lookup, and on a miss the backend round trip
    /// ([`SearchWait::EmbedBackend`] is nested in it).
    Embed,
    /// A query-cache miss waiting for its backend: the local fastembed query
    /// worker's reply (queueing behind the process-wide resident model and
    /// inference), or the Synapse/HTTP request. Nested in `Embed`.
    EmbedBackend,
    /// Scoring stored vectors for the query (snapshot refresh and scan).
    /// Mostly CPU, but store block reads on a cache miss land here.
    VectorSearch,
    /// Data-layer text lanes: lexical candidate selection, the exact pass's
    /// and the pattern route's candidate lookups over the published text
    /// plane, including store block reads.
    TextSearch,
    /// The exact memo: its locks, and waiting for another request that is
    /// verifying the same key. This request's own verification is not in it.
    ExactMemo,
    /// Exact/matching text preparation caches: their map mutexes, and waiting
    /// for another request that is preparing the same content.
    PrepareCache,
    /// Reading source files on the searching thread (lexical and exact
    /// verification, memo digest checks, code-reference checks, snippets).
    FileRead,
    /// Tree-sitter parsing of candidate files at query time (the exact lane's
    /// code-reference check). CPU work, not a wait.
    Parse,
    /// A Rayon parallel section the searching thread joins (pattern
    /// verification, degraded grep). Includes the pool's work for this call
    /// as well as waiting for pool threads.
    Rayon,
    /// Walking the project folder on the searching thread: the path-lookup
    /// lane's walk and the degraded fallback's file collection.
    Walk,
    /// A grep-executor run (regex and literal searches, and the pattern route
    /// when no ready index serves it): candidate lookup or walk, file reads
    /// and verification on the Rayon pool.
    Grep,
    /// The optional cross-encoder rerank of the first page's head.
    Rerank,
    /// Candidate text assembly, including source reads and matching-line work.
    RerankCandidates,
    /// The installed reranker's scoring call (including its queue or network).
    RerankBackend,
    /// Content-addressed matching-source lookup and normalization.
    MatchingPrepare,
    /// Waiting for another search to initialize the same matching source.
    MatchingSourceWait,
    /// The normalized exact-phrase scan before fuzzy line scoring.
    MatchingPhrase,
    /// Best-line scoring, including lazy line trigram preparation.
    MatchingBestLine,
    /// Building and sorting a line's query-independent byte trigrams.
    MatchingTrigrams,
    /// Waiting for another search to initialize the same line's trigrams.
    MatchingTrigramWait,
    /// Hashing source bytes for the matching preparation cache.
    MatchingDigest,
    /// Acquiring the matching preparation cache's map mutex.
    MatchingCacheLock,
    /// Normalizing and compacting a source on a matching-cache miss.
    MatchingNormalize,
    /// Evicting matching-cache entries, including destruction of their text.
    MatchingEvict,
}

impl SearchWait {
    pub const ALL: [SearchWait; 29] = [
        SearchWait::CheckoutSlot,
        SearchWait::TextView,
        SearchWait::MeaningView,
        SearchWait::Readiness,
        SearchWait::ModelLock,
        SearchWait::Embed,
        SearchWait::EmbedBackend,
        SearchWait::VectorSearch,
        SearchWait::TextSearch,
        SearchWait::ExactMemo,
        SearchWait::PrepareCache,
        SearchWait::FileRead,
        SearchWait::Parse,
        SearchWait::Rayon,
        SearchWait::Walk,
        SearchWait::Grep,
        SearchWait::Rerank,
        SearchWait::RerankCandidates,
        SearchWait::RerankBackend,
        SearchWait::MatchingPrepare,
        SearchWait::MatchingSourceWait,
        SearchWait::MatchingPhrase,
        SearchWait::MatchingBestLine,
        SearchWait::MatchingTrigrams,
        SearchWait::MatchingTrigramWait,
        SearchWait::MatchingDigest,
        SearchWait::MatchingCacheLock,
        SearchWait::MatchingNormalize,
        SearchWait::MatchingEvict,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SearchWait::CheckoutSlot => "checkout_slot",
            SearchWait::TextView => "text_view",
            SearchWait::MeaningView => "meaning_view",
            SearchWait::Readiness => "readiness",
            SearchWait::ModelLock => "model_lock",
            SearchWait::Embed => "embed",
            SearchWait::EmbedBackend => "embed_backend",
            SearchWait::VectorSearch => "vector_search",
            SearchWait::TextSearch => "text_search",
            SearchWait::ExactMemo => "exact_memo",
            SearchWait::PrepareCache => "prepare_cache",
            SearchWait::FileRead => "file_read",
            SearchWait::Parse => "parse",
            SearchWait::Rayon => "rayon",
            SearchWait::Walk => "walk",
            SearchWait::Grep => "grep",
            SearchWait::Rerank => "rerank",
            SearchWait::RerankCandidates => "rerank_candidates",
            SearchWait::RerankBackend => "rerank_backend",
            SearchWait::MatchingPrepare => "matching_prepare",
            SearchWait::MatchingSourceWait => "matching_source_wait",
            SearchWait::MatchingPhrase => "matching_phrase",
            SearchWait::MatchingBestLine => "matching_best_line",
            SearchWait::MatchingTrigrams => "matching_trigrams",
            SearchWait::MatchingTrigramWait => "matching_trigram_wait",
            SearchWait::MatchingDigest => "matching_digest",
            SearchWait::MatchingCacheLock => "matching_cache_lock",
            SearchWait::MatchingNormalize => "matching_normalize",
            SearchWait::MatchingEvict => "matching_evict",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

const KINDS: usize = SearchWait::ALL.len();

/// Time spent in one kind of wait during one call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaitSlot {
    /// Sum of the outermost timers of this kind.
    pub total: Duration,
    /// How many outermost timers of this kind ran.
    pub count: u32,
    /// The longest single outermost timer of this kind.
    pub max: Duration,
    /// Searching-thread CPU time inside those timers, when the scope was
    /// opened with [`begin_with_wait_cpu`]; zero otherwise. `total - cpu` is
    /// the time the thread spent off CPU (blocked or preempted) in this kind.
    pub cpu: Duration,
}

/// Every named wait of one search call, its wall time, and its thread CPU
/// time when the platform can measure it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchWaits {
    pub slots: [WaitSlot; KINDS],
    /// Wall time from [`begin`] to [`WaitScope::finish`].
    pub wall: Duration,
    /// Searching-thread CPU time over the same interval; `None` on platforms
    /// without a fine thread clock (Windows). Work done for the call on other
    /// threads (Rayon workers, the embedding worker) is not included.
    pub cpu: Option<Duration>,
    /// Each slot's `cpu` was sampled (see [`begin_with_wait_cpu`]).
    pub wait_cpu_sampled: bool,
    /// What the call's request file sets read from disk and kept, summed
    /// over nested requests (see `request_files`).
    pub files: super::request_files::RequestFilesReport,
    /// How the exact lane's prepared-text cache served this call.
    pub prepared: PreparedTextCounts,
    /// Content-addressed code-reference answer-cache activity.
    pub code_references: CodeReferenceCounts,
}

/// Code-reference cache lookups and the name sets built by this search.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CodeReferenceCounts {
    pub hits: u64,
    pub misses: u64,
    pub names: u64,
    pub bytes: u64,
}

impl CodeReferenceCounts {
    const EMPTY: Self = Self {
        hits: 0,
        misses: 0,
        names: 0,
        bytes: 0,
    };
}

/// Lookups of the exact lane's prepared-text cache during one search call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreparedTextCounts {
    /// Text found prepared, or being prepared by another request.
    pub hits: u32,
    /// Text this call prepared.
    pub misses: u32,
    /// Entries this call's inserts evicted because the cache held its most
    /// entries.
    pub evicted_by_count: u32,
    /// Entries evicted because the cache held its most bytes.
    pub evicted_by_bytes: u32,
}

impl PreparedTextCounts {
    const EMPTY: Self = Self {
        hits: 0,
        misses: 0,
        evicted_by_count: 0,
        evicted_by_bytes: 0,
    };
}

impl SearchWaits {
    pub fn slot(&self, kind: SearchWait) -> WaitSlot {
        self.slots[kind.index()]
    }

    /// `waits=text_view:0.01/1,embed:3.20/1 search_cpu=12.34ms`: each kind
    /// that ran, as total milliseconds / timer count, then the searching
    /// thread's CPU time (`?` when unknown). Includes no query or source text.
    pub fn render(&self) -> String {
        let mut out = String::from("waits=");
        let mut any = false;
        for kind in SearchWait::ALL {
            let slot = self.slot(kind);
            if slot.count == 0 {
                continue;
            }
            if any {
                out.push(',');
            }
            any = true;
            let _ = write!(
                out,
                "{}:{:.2}/{}",
                kind.as_str(),
                slot.total.as_secs_f64() * 1000.0,
                slot.count
            );
        }
        if !any {
            out.push_str("none");
        }
        match self.cpu {
            Some(cpu) => {
                let _ = write!(out, " search_cpu={:.2}ms", cpu.as_secs_f64() * 1000.0);
            }
            None => out.push_str(" search_cpu=?"),
        }
        if self.files.disk_reads > 0 || self.files.served > 0 {
            let files = &self.files;
            let _ = write!(
                out,
                " files=read:{}/{}B,repeat:{},served:{},peak:{}B,line_hits:{}",
                files.disk_reads,
                files.disk_bytes,
                files.repeat_reads,
                files.served,
                files.peak_bytes,
                files.matching_line_hits
            );
        }
        let prepared = &self.prepared;
        if prepared.hits > 0 || prepared.misses > 0 {
            let _ = write!(
                out,
                " prepared=hit:{},miss:{},evict:{}/{}",
                prepared.hits,
                prepared.misses,
                prepared.evicted_by_count,
                prepared.evicted_by_bytes
            );
        }
        let references = &self.code_references;
        if references.hits + references.misses > 0 {
            let _ = write!(
                out,
                " code_refs=hit:{},miss:{},names:{},bytes:{}",
                references.hits, references.misses, references.names, references.bytes
            );
        }
        out
    }
}

struct Collector {
    active: bool,
    /// Sample thread CPU at every timer, not just at the scope's ends.
    wait_cpu: bool,
    /// Advances at every [`begin`], so a timer left over from an earlier
    /// scope never records into a later one.
    generation: u32,
    depth: [u8; KINDS],
    slots: [WaitSlot; KINDS],
    files: super::request_files::RequestFilesReport,
    prepared: PreparedTextCounts,
    code_references: CodeReferenceCounts,
}

const EMPTY_SLOT: WaitSlot = WaitSlot {
    total: Duration::ZERO,
    count: 0,
    max: Duration::ZERO,
    cpu: Duration::ZERO,
};

thread_local! {
    static COLLECTOR: RefCell<Collector> = const {
        RefCell::new(Collector {
            active: false,
            wait_cpu: false,
            generation: 0,
            depth: [0; KINDS],
            slots: [EMPTY_SLOT; KINDS],
            files: super::request_files::RequestFilesReport::EMPTY,
            prepared: PreparedTextCounts::EMPTY,
            code_references: CodeReferenceCounts::EMPTY,
        })
    };
}

/// An open collection on this thread; see [`begin`].
#[must_use = "a scope collects nothing once dropped"]
pub struct WaitScope {
    /// `None` when another scope was already open on this thread: the outer
    /// scope keeps collecting and this one reports nothing of its own.
    started: Option<(Instant, Option<crate::clock::ThreadCpuSample>)>,
}

/// Start collecting this thread's search waits. If a scope is already open
/// on the thread (a search nested inside another), the outer one keeps
/// collecting and the inner scope's [`WaitScope::finish`] returns `None`.
///
/// Timers read only the wall clock: this is the form the live slow-call line
/// uses.
pub fn begin() -> WaitScope {
    open(false)
}

/// As [`begin`], and every timer also reads the thread CPU clock, so each
/// wait kind can be split into on-CPU and off-CPU time. A thread CPU read
/// costs more than a wall-clock read; measurement harnesses use this, the
/// live path does not.
pub fn begin_with_wait_cpu() -> WaitScope {
    open(true)
}

fn open(wait_cpu: bool) -> WaitScope {
    let opened = COLLECTOR.with(|collector| {
        let mut collector = collector.borrow_mut();
        if collector.active {
            return false;
        }
        collector.active = true;
        collector.wait_cpu = wait_cpu;
        collector.generation = collector.generation.wrapping_add(1);
        collector.depth = [0; KINDS];
        collector.slots = [EMPTY_SLOT; KINDS];
        collector.files = super::request_files::RequestFilesReport::EMPTY;
        collector.prepared = PreparedTextCounts::EMPTY;
        collector.code_references = CodeReferenceCounts::EMPTY;
        true
    });
    WaitScope {
        started: opened.then(|| (Instant::now(), crate::clock::ThreadCpuSample::now())),
    }
}

impl WaitScope {
    /// Stop collecting and return what this scope saw, or `None` for a scope
    /// nested inside another.
    pub fn finish(mut self) -> Option<SearchWaits> {
        let (started, cpu_started) = self.started.take()?;
        let wall = started.elapsed();
        let cpu = crate::clock::ThreadCpuSample::elapsed(
            cpu_started,
            crate::clock::ThreadCpuSample::now(),
        );
        let (slots, wait_cpu_sampled, files, prepared, code_references) =
            COLLECTOR.with(|collector| {
                let mut collector = collector.borrow_mut();
                collector.active = false;
                (
                    collector.slots,
                    collector.wait_cpu,
                    collector.files,
                    collector.prepared,
                    collector.code_references,
                )
            });
        Some(SearchWaits {
            slots,
            wall,
            cpu,
            wait_cpu_sampled,
            files,
            prepared,
            code_references,
        })
    }
}

impl Drop for WaitScope {
    fn drop(&mut self) {
        // A scope dropped without `finish` (an early return or a panic)
        // still closes collection so later calls on this thread start clean.
        if self.started.is_some() {
            COLLECTOR.with(|collector| collector.borrow_mut().active = false);
        }
    }
}

/// Times one wait of `kind` until dropped. Inert unless a [`WaitScope`] is
/// open on this thread.
#[must_use = "a wait timer measures until it is dropped"]
pub struct WaitTimer {
    kind: SearchWait,
    /// Wall start, the scope generation it belongs to, and the thread CPU
    /// start when the scope samples it; `None` when no scope was open.
    started: Option<TimerStart>,
}

type TimerStart = (Instant, u32, Option<crate::clock::ThreadCpuSample>);

/// Start timing a wait of `kind` on this thread.
pub fn time(kind: SearchWait) -> WaitTimer {
    let open = COLLECTOR.with(|collector| {
        let mut collector = collector.borrow_mut();
        if !collector.active {
            return None;
        }
        let depth = &mut collector.depth[kind.index()];
        *depth = depth.saturating_add(1);
        Some((collector.generation, collector.wait_cpu))
    });
    WaitTimer {
        kind,
        started: open.map(|(generation, wait_cpu)| {
            let cpu = if wait_cpu {
                crate::clock::ThreadCpuSample::now()
            } else {
                None
            };
            (Instant::now(), generation, cpu)
        }),
    }
}

/// Add a finished request's file-set report to this thread's open scope, if
/// any.
pub(crate) fn record_request_files(report: super::request_files::RequestFilesReport) {
    COLLECTOR.with(|collector| {
        let mut collector = collector.borrow_mut();
        if collector.active {
            collector.files.add(report);
        }
    });
}

/// Count one prepared-text cache lookup in this thread's open scope, if any.
pub(crate) fn record_prepared_text(update: impl FnOnce(&mut PreparedTextCounts)) {
    COLLECTOR.with(|collector| {
        let mut collector = collector.borrow_mut();
        if collector.active {
            update(&mut collector.prepared);
        }
    });
}

/// Count code-reference answer-cache work in this thread's open scope.
pub(crate) fn record_code_references(update: impl FnOnce(&mut CodeReferenceCounts)) {
    COLLECTOR.with(|collector| {
        let mut collector = collector.borrow_mut();
        if collector.active {
            update(&mut collector.code_references);
        }
    });
}

/// Run `f` as one wait of `kind`.
pub fn timed<R>(kind: SearchWait, f: impl FnOnce() -> R) -> R {
    let _timer = time(kind);
    f()
}

impl Drop for WaitTimer {
    fn drop(&mut self) {
        let Some((started, generation, cpu_started)) = self.started else {
            return;
        };
        let elapsed = started.elapsed();
        let cpu = cpu_started.and_then(|cpu_started| {
            crate::clock::ThreadCpuSample::elapsed(
                Some(cpu_started),
                crate::clock::ThreadCpuSample::now(),
            )
        });
        COLLECTOR.with(|collector| {
            let mut collector = collector.borrow_mut();
            // A timer from a scope that has since closed records nothing,
            // and leaves a later scope's nesting depth alone.
            if !collector.active || collector.generation != generation {
                return;
            }
            let index = self.kind.index();
            collector.depth[index] = collector.depth[index].saturating_sub(1);
            if collector.depth[index] != 0 {
                return;
            }
            let slot = &mut collector.slots[index];
            slot.total += elapsed;
            slot.count = slot.count.saturating_add(1);
            slot.max = slot.max.max(elapsed);
            if let Some(cpu) = cpu {
                slot.cpu += cpu;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_waits_record_only_inside_a_scope() {
        drop(time(SearchWait::FileRead));
        let scope = begin();
        {
            let _read = time(SearchWait::FileRead);
            std::thread::sleep(Duration::from_millis(2));
        }
        let waits = scope.finish().expect("outermost scope reports");
        let read = waits.slot(SearchWait::FileRead);
        assert_eq!(read.count, 1);
        assert!(read.total >= Duration::from_millis(2), "{read:?}");
        assert_eq!(read.max, read.total);
        assert!(waits.wall >= read.total);
        assert_eq!(waits.slot(SearchWait::Embed), WaitSlot::default());

        // Closed again: a timer after finish records nothing anywhere.
        drop(time(SearchWait::FileRead));
        let empty = begin().finish().expect("fresh scope");
        assert_eq!(empty.slot(SearchWait::FileRead).count, 0);
    }

    #[test]
    fn search_waits_count_nested_same_kind_once_and_other_kinds_separately() {
        let scope = begin();
        {
            let _outer = time(SearchWait::ExactMemo);
            let _inner = time(SearchWait::ExactMemo);
            let _read = time(SearchWait::FileRead);
        }
        let waits = scope.finish().unwrap();
        assert_eq!(waits.slot(SearchWait::ExactMemo).count, 1);
        assert_eq!(waits.slot(SearchWait::FileRead).count, 1);
    }

    #[test]
    fn search_waits_nested_scope_defers_to_the_outer_one() {
        let outer = begin();
        let inner = begin();
        drop(time(SearchWait::Rayon));
        assert!(inner.finish().is_none());
        drop(time(SearchWait::Rayon));
        let waits = outer.finish().unwrap();
        assert_eq!(waits.slot(SearchWait::Rayon).count, 2);
    }

    #[test]
    fn search_waits_dropped_scope_stops_collection() {
        drop(begin());
        drop(time(SearchWait::Embed));
        let waits = begin().finish().unwrap();
        assert_eq!(waits.slot(SearchWait::Embed).count, 0);
    }

    /// A timer still open when its scope closes must not land in the next
    /// scope on the same thread, nor disturb that scope's nesting depth.
    #[test]
    fn search_waits_timer_from_a_closed_scope_never_records_into_a_later_one() {
        let first = begin();
        let stale = time(SearchWait::FileRead);
        first.finish().unwrap();
        let second = begin();
        let current = time(SearchWait::FileRead);
        drop(stale);
        drop(current);
        let waits = second.finish().unwrap();
        assert_eq!(waits.slot(SearchWait::FileRead).count, 1);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn search_waits_split_cpu_per_wait_only_when_asked() {
        fn spin(for_at_least: Duration) {
            let started = Instant::now();
            while started.elapsed() < for_at_least {
                std::hint::black_box(0u64);
            }
        }
        let scope = begin_with_wait_cpu();
        {
            let _parse = time(SearchWait::Parse);
            spin(Duration::from_millis(5));
        }
        {
            let _read = time(SearchWait::FileRead);
            std::thread::sleep(Duration::from_millis(5));
        }
        let waits = scope.finish().unwrap();
        assert!(waits.wait_cpu_sampled);
        let parse = waits.slot(SearchWait::Parse);
        let read = waits.slot(SearchWait::FileRead);
        assert!(parse.cpu > Duration::ZERO, "{parse:?}");
        assert!(read.cpu < read.total / 2, "{read:?}");

        let scope = begin();
        {
            let _parse = time(SearchWait::Parse);
            spin(Duration::from_millis(1));
        }
        let waits = scope.finish().unwrap();
        assert!(!waits.wait_cpu_sampled);
        assert_eq!(waits.slot(SearchWait::Parse).cpu, Duration::ZERO);
    }

    #[test]
    fn search_waits_render_names_each_kind_that_ran_and_the_cpu() {
        let mut waits = SearchWaits::default();
        assert_eq!(waits.render(), "waits=none search_cpu=?");
        waits.slots[SearchWait::ModelLock.index()] = WaitSlot {
            total: Duration::from_micros(1_500),
            count: 2,
            max: Duration::from_millis(1),
            ..WaitSlot::default()
        };
        waits.slots[SearchWait::Embed.index()] = WaitSlot {
            total: Duration::from_millis(40),
            count: 1,
            max: Duration::from_millis(40),
            ..WaitSlot::default()
        };
        waits.cpu = Some(Duration::from_micros(12_346));
        assert_eq!(
            waits.render(),
            "waits=model_lock:1.50/2,embed:40.00/1 search_cpu=12.35ms"
        );
    }

    #[test]
    fn search_waits_labels_are_unique_and_index_matches_order() {
        for (position, kind) in SearchWait::ALL.iter().enumerate() {
            assert_eq!(kind.index(), position);
        }
        let mut labels: Vec<_> = SearchWait::ALL.iter().map(|kind| kind.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), KINDS);
    }
}
