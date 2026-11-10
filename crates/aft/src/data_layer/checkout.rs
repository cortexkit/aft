//! A bound checkout's data: built, published and read through one handle.
//!
//! Binding a checkout opens its repository's family store. Building scans
//! the checkout once and, from each file it reads, takes what every plane
//! needs: the file's trigrams, and for a file the call graph parses, its
//! content record. It then resolves the call graph, writes one segment
//! holding the text, content and graph sections, and publishes it with the
//! checkout's state, which names each parsed path's fragment.
//!
//! Readers take a [`CheckoutView`]: the published generation, held open
//! until the view drops. A view answers text and call-graph queries; it is
//! immutable, so a query never sees a half-applied change.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind, PathRecord};
use aft_store::content::{
    ContentRecord, ContentSectionsBuilder, ContentSegment, EncodedRecord, RecordBody, RecordKey,
};
use aft_store::family::{Family, Publication, Snapshot};
use aft_store::graph::{FragmentKey, GraphSegment};
use aft_store::ids::{CheckoutId, ContentKey, Digest128, FamilyId, SegmentId};
use aft_store::scan::{ContentKind, ContentSink, PathScan, Scan, ScanError, ScanPolicy};
use aft_store::segment::{KnownSection, Segment};
use aft_store::text::{
    candidates, ContentTrigrams, Located, OverlayEntry, PathText, Query, Scope, TextLocator,
    TextMembership, TextOverlay, TextSections, TextSectionsBuilder, TextSegment, TextStatus,
};

use super::callgraph::{CheckoutInputs, EncodedResolution, FileResolution};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{Held, SectionsWriter, StoredSlices};
use super::incremental::{LiveCheckout, UpdateStats};
use super::overlay::{CacheKey, OverlaySlices, PreparedSlice};
use super::query::{SliceReader, SliceSource};
use super::slice::FileSlice;
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};

mod engines;
mod fold;

pub use fold::{FoldReport, Folded, OtherFoldReport};

/// [`aft_store::scan::scan`]: every pass over the checkout's files goes
/// through here, so tests can tell work that reads none.
fn scan(
    root: &Path,
    policy: &ScanPolicy,
    previous: Option<&CheckoutState>,
    sink: &dyn ContentSink,
) -> Result<Scan, ScanError> {
    note_source_work(root, |work| work.scans += 1);
    aft_store::scan::scan(root, policy, previous, sink)
}

/// [`aft_store::scan::scan_paths`], counted as [`scan`] is.
fn scan_paths(
    root: &Path,
    policy: &ScanPolicy,
    previous: &CheckoutState,
    paths: &[Vec<u8>],
    sink: &dyn ContentSink,
) -> Result<PathScan, ScanError> {
    note_source_work(root, |work| work.scans += 1);
    aft_store::scan::scan_paths(root, policy, previous, paths, sink)
}

/// How often the checkout at a root read its files or resolved its call
/// graph, for tests of work that must do neither. Only tests keep count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SourceWork {
    /// Passes over the files: a scan reads every file whose stat does not
    /// prove its recorded content, and a build's scan parses what it reads.
    pub(crate) scans: usize,
    /// Call-graph resolutions: a build's, or a refresh's update.
    pub(crate) resolutions: usize,
    /// The most stored parses, and separately resolutions, a refresh's
    /// update held decoded when it finished: everything it decoded and
    /// kept, since nothing is let go before then.
    pub(crate) decoded_parses: usize,
    pub(crate) decoded_resolutions: usize,
}

#[cfg(test)]
fn source_work() -> &'static Mutex<HashMap<PathBuf, SourceWork>> {
    static WORK: std::sync::OnceLock<Mutex<HashMap<PathBuf, SourceWork>>> =
        std::sync::OnceLock::new();
    WORK.get_or_init(Default::default)
}

/// What the checkout at `root` did so far (see [`SourceWork`]).
#[cfg(test)]
pub(crate) fn source_work_for_test(root: &Path) -> SourceWork {
    source_work()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
fn note_source_work(root: &Path, note: impl FnOnce(&mut SourceWork)) {
    note(
        source_work()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root.to_path_buf())
            .or_default(),
    );
}

#[cfg(not(test))]
fn note_source_work(_root: &Path, _note: impl FnOnce(&mut SourceWork)) {}

/// The name status gives the data layer's family store when it refuses a
/// file a newer build wrote: the `store` of its `storage_refusals` entry and
/// of its `storage_requires_newer_reader:<store>` degraded reason.
///
/// It is deliberately not a [`crate::persisted_format::PersistedStore`], so
/// the store has no reader-floor entry and `aft --formats` does not list it.
/// `scripts/stage-card.sh` refuses to stage a build whose `--formats` lacks
/// a store the floor names, so a floor entry would stop an older build from
/// before the data layer existed from being staged for rollback. Such a
/// build never reads this store, so it needs no floor to protect it; the
/// refusal below protects it from builds that do read it.
pub const INDEX_STORE: &str = "index_store";

/// Why the data layer could not do what was asked.
#[derive(Clone, Debug)]
pub enum DataError {
    /// The family store failed: its files could not be read or written.
    Store(String),
    /// A newer build wrote a file of the family store in a format version
    /// above the highest this build reads. The file is refused: nothing is
    /// built over it, reclaimed or written in its place, and what needs it
    /// fails with this until a build that reads it runs.
    NewerFormat(aft_store::family::NewerFormat),
    /// The checkout could not be scanned.
    Scan(String),
    /// The call graph could not be resolved.
    Resolve(String),
    /// Another process kept the repository's store busy past the wait.
    Busy(String),
    /// A stored record or fragment could not be read back, or reading it
    /// stopped the update; the checkout is built again from its files.
    Damaged(String),
    /// The executor job running the work was cancelled (its root unbound,
    /// or AFT is draining): it stopped at a checkpoint, published nothing,
    /// and is not a failure. The work runs again once the root is bound.
    Cancelled,
    /// Writing would leave less than AFT's free-disk floor on the store's
    /// disk, so nothing new is written; what is published keeps serving.
    /// Holds the approved pause sentence.
    DiskFloor(String),
    /// Git could not tell which repository the checkout belongs to, so it
    /// is not bound yet; the bind is tried again later.
    Identity(String),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "AFT's index store could not be read: {error}"),
            Self::NewerFormat(refusal) => f.write_str(&refusal_message(refusal)),
            Self::Scan(error) => write!(f, "AFT could not scan this checkout: {error}"),
            Self::Resolve(error) => write!(f, "AFT could not build this checkout's call graph: {error}"),
            Self::Busy(_) => f.write_str("Another AFT process is writing this repository's index; try again shortly."),
            Self::Damaged(error) => write!(f, "AFT found damaged index data and rebuilds it from the files: {error}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::DiskFloor(message) => f.write_str(message),
            Self::Identity(_) => f.write_str("AFT could not tell which repository this folder belongs to; it tries again shortly."),
        }
    }
}

impl std::error::Error for DataError {}

impl DataError {
    /// A store failure. A family-store file a newer build wrote stays typed
    /// as [`Self::NewerFormat`], whichever store error carries it, so every
    /// path that reaches the store reports the refusal by name.
    pub(crate) fn store(error: impl std::fmt::Display + 'static) -> Self {
        let error_any: &dyn std::any::Any = &error;
        let refusal = error_any
            .downcast_ref::<aft_store::family::FamilyError>()
            .and_then(|error| error.newer_format().cloned())
            .or_else(|| {
                error_any
                    .downcast_ref::<aft_store::segment::SegmentError>()
                    .and_then(aft_store::family::NewerFormat::of_segment)
            });
        match refusal {
            Some(refusal) => Self::NewerFormat(refusal),
            None => Self::Store(error.to_string()),
        }
    }

    /// The refusal, when a newer build wrote a file this work needs.
    #[must_use]
    pub fn refusal(&self) -> Option<&aft_store::family::NewerFormat> {
        match self {
            Self::NewerFormat(refusal) => Some(refusal),
            _ => None,
        }
    }
}

/// A refused family-store file, in the words today's stores use for an
/// artifact they refuse (see [`crate::persisted_format`]).
pub(crate) fn refusal_message(refusal: &aft_store::family::NewerFormat) -> String {
    format!(
        "{}: {INDEX_STORE} at {} has format version {}; this build reads up to {}",
        crate::persisted_format::CODE,
        refusal.path.display(),
        refusal.found,
        refusal.supported
    )
}

/// Stops work that runs as an executor job once the job is cancelled. Long
/// work calls it between phases; outside a job it never stops anything.
fn checkpoint() -> Result<(), DataError> {
    if crate::executor::current_job_cancelled() {
        Err(DataError::Cancelled)
    } else {
        Ok(())
    }
}

/// What one process's data layer shares across every checkout it binds.
#[derive(Debug)]
pub struct DataLayer {
    cache: Arc<BlockCache>,
    threads: usize,
    policy: ScanPolicy,
    /// Every checkout's call-graph engine, kept between refreshes until it
    /// falls idle or the engines together hold too much.
    engines: Arc<engines::EngineKeeper>,
    /// Every checkout bound through this data layer, by its family's
    /// folder: a merge of vector segments keeps what their current states
    /// read, published or not. Entries of dropped checkouts are pruned as
    /// checkouts are bound.
    bound: Mutex<Vec<(PathBuf, std::sync::Weak<RwLock<Live>>)>>,
}

impl DataLayer {
    /// A data layer whose segment reads share a cache of `cache_bytes`, and
    /// whose builds use `threads` threads.
    #[must_use]
    pub fn new(cache_bytes: usize, threads: usize) -> Self {
        Self {
            cache: Arc::new(BlockCache::new(cache_bytes)),
            threads: threads.max(1),
            policy: super::content::scan_policy(),
            engines: Arc::new(engines::EngineKeeper::new()),
            bound: Mutex::new(Vec::new()),
        }
    }

    /// Sets how long a checkout's call-graph engine stays after the
    /// checkout's last edit or applied refresh. For tests and measurement.
    #[doc(hidden)]
    pub fn set_engine_idle(&self, idle: std::time::Duration) {
        self.engines.set_idle(idle);
    }

    /// Sets how many bytes the call-graph engines of every checkout may
    /// hold together. For tests and measurement.
    #[doc(hidden)]
    pub fn set_engine_budget(&self, bytes: u64) {
        self.engines.set_budget(bytes);
    }
}

impl Drop for DataLayer {
    fn drop(&mut self) {
        self.engines.stop();
    }
}

/// Every section a segment of this data layer can hold.
fn known_sections() -> Vec<KnownSection> {
    aft_store::text::KNOWN
        .into_iter()
        .chain(super::graph_store::known_sections())
        .chain(aft_store::vector::KNOWN)
        .collect()
}

/// Which planes a checkout builds and serves. A plane that is off is not
/// built, and its readers answer as they do with that index switched off:
/// grep and glob walk the folder, call-graph commands refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Planes {
    pub text: bool,
    pub callgraph: bool,
}

impl Planes {
    pub const ALL: Self = Self {
        text: true,
        callgraph: true,
    };
}

/// One bound checkout.
pub struct Checkout {
    layer: Arc<DataLayer>,
    planes: Planes,
    root: PathBuf,
    family: Family,
    checkout: CheckoutId,
    /// The last published generation, and the changes made since. Shared
    /// with the data layer's list of bound checkouts, which only reads it.
    live: Arc<RwLock<Live>>,
    /// The call graph's incremental state, built from the publication on
    /// the first refresh and moved onto each publication of its changes;
    /// shared with the data layer, which lets it go once it falls idle or
    /// the engines of every checkout hold too much.
    engine: Arc<engines::EngineSlot>,
    /// Orders recordings: each one brings the state the previous left up to
    /// date.
    recording: Mutex<()>,
    /// Moves whenever what the checkout holds in memory may have changed: a
    /// recorded change, a call-graph refresh, a new publication, an engine
    /// let go. A memory figure walked at one value stays right until it
    /// moves (see [`Self::memory_epoch`]).
    memory_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Wakes readers at publication, including the text-only publication
    /// that precedes the rest of a checkout's first build.
    readiness: (Mutex<u64>, std::sync::Condvar),
}

/// A checkout's call graph kept up to date between publications.
/// What a change to one path makes a recording scan again, beyond the
/// path itself.
enum IgnoreScope {
    /// Not an ignore file: only the path.
    None,
    /// An ignore file in this folder: the whole folder.
    Folder(Vec<u8>),
    /// An ignore file at the root, or Git's own exclude file: the whole
    /// checkout.
    Whole,
}

struct GraphEngine {
    /// The publication the engine started from; a newer one replaces it.
    published: Arc<Published>,
    live: LiveCheckout,
    /// Each changed path's change count as the engine last applied it.
    applied: BTreeMap<Vec<u8>, u64>,
    /// The call graph as of the last refresh: the publication with every
    /// file resolved since laid over it. Each refresh updates only the files
    /// its update touched, then serves a copy.
    overlay: OverlaySlices<Arc<StoredSlices>>,
    /// What the overlay replaces and removes, for the dead-code snapshot.
    refreshed: Refreshed,
    /// A refresh measured the overlay, or a broad publication discarded
    /// substantial state. The publication must replace that size estimate;
    /// small publications otherwise keep the ordinary measurement cadence.
    measure_after_publication: bool,
}

impl GraphEngine {
    /// This engine moved onto `to`, the publication of what it resolved:
    /// the engine a start from `to` gives, keeping every file it bound and
    /// every file's method-call inputs it computed (see
    /// [`LiveCheckout::rebased`]). It must have applied exactly the changes
    /// `applied` names, each changed path with its change count, which `to`
    /// publishes; and `to` must hold the paths, parse keys and
    /// configuration the engine resolves over, with no stored read of
    /// either publication failed. `Err` says which does not hold.
    fn rebased(
        self,
        to: &Arc<Published>,
        applied: &BTreeMap<Vec<u8>, u64>,
    ) -> Result<Self, String> {
        if self.applied != *applied {
            return Err("it applied other changes than the publication holds".to_string());
        }
        if !to.graph_built {
            return Err("the publication holds no call graph".to_string());
        }
        let failures = [self.published.graph.failures(), to.graph.failures()].concat();
        if !failures.is_empty() {
            return Err(format!("a stored read failed: {}", failures.join("; ")));
        }
        let configs = to.graph.configs(&to.state)?;
        if configs != self.live.inputs.configs {
            return Err("the publication's configuration differs from the engine's".to_string());
        }
        let broad = self.applied.len() > ENGINE_DECODED_FILES
            || self.refreshed.replaced.len() > ENGINE_DECODED_FILES;
        let mut live = self
            .live
            .rebased(to.state.clone(), Arc::clone(&to.graph))
            .map_err(str::to_string)?;
        // Changed parses could not be released before publication. The rebased
        // slots now read from storage and are unshared, so the same decoded-file
        // bound can release those too, without losing bindings or method inputs.
        let release_parses = live.inputs.parses.decoded_count() > ENGINE_DECODED_FILES;
        if release_parses {
            live.inputs.parses.release_decoded();
        }
        Ok(Self {
            published: Arc::clone(to),
            live,
            measure_after_publication: self.measure_after_publication || broad || release_parses,
            applied: BTreeMap::new(),
            overlay: OverlaySlices::new(Arc::clone(&to.graph)),
            refreshed: Refreshed {
                state: to.state.clone(),
                replaced: BTreeMap::new(),
                removed: BTreeSet::new(),
                records: BTreeMap::new(),
                dead_code: DeadCodeMemo::default(),
            },
        })
    }

    /// The bytes this engine holds, counted as the memory census counts an
    /// engine's (see [`Checkout::memory`]): everything it reaches apart
    /// from the publication it started from, which the census counts as
    /// the checkout's.
    fn census_bytes(&self) -> u64 {
        use aft_store::heap::HeapBytes as _;
        let seen = &mut aft_store::heap::Seen::new();
        MemoryParts::default().published(&self.published, seen);
        let (state, rest) = self.live.memory(seen);
        let mut parts = MemoryParts::default();
        parts.refreshed(&self.refreshed, seen);
        let bytes = state
            + rest
            + self.applied.heap_bytes(seen)
            + self.overlay.heap_bytes(seen)
            + parts.shared
            + parts.callgraph;
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

/// A checkout's current data: its last publication and the changes made
/// since, which queries see in place of the published records.
#[derive(Default)]
struct Live {
    published: Option<Arc<Published>>,
    /// Paths in a parsed language that the call graph does not cover,
    /// because extracting them failed: the publication's, with recorded
    /// changes applied.
    unparsed: Arc<BTreeSet<Vec<u8>>>,
    /// Changed contents' trigrams by path, which text queries see at once.
    text: Arc<TextOverlay>,
    /// The publication's text members with `text` set aside, built by the
    /// first trigram lookup and shared by every view until `text` or the
    /// publication changes (each replaces `text`, which resets it).
    text_members: Arc<std::sync::OnceLock<aft_store::text::TextMembers>>,
    /// Paths changed since the publication whose call graph is not
    /// resolved yet.
    callgraph_pending: Arc<BTreeSet<Vec<u8>>>,
    /// A rescan is comparing the files with the checkout's state, so
    /// changes nobody reported may be found: until it ends, the call graph
    /// is not reported ready.
    rescanning: bool,
    /// Whether a build is running.
    building: bool,
    /// Counts recorded changes; each changed path keeps the count of its
    /// latest change, so a build knows which changes its scan covered.
    sequence: u64,
    /// The checkout's state now: the publication's, with every change
    /// recorded since applied. `None` while it equals the publication's.
    state: Option<Arc<CheckoutState>>,
    /// Each path changed since the publication.
    changes: BTreeMap<Vec<u8>, Change>,
    /// Paths reported while nothing was published, with their change
    /// counts: a build's scan covers those reported before it started, and
    /// the rest are recorded once it publishes.
    early: BTreeMap<Vec<u8>, u64>,
    /// The call graph with the files resolved since the publication laid
    /// over it; `None` until the first refresh.
    graph: Option<Arc<dyn SliceSource + Send + Sync>>,
    /// What the last refresh resolved, for readers that need whole
    /// resolutions rather than slices; `None` until a refresh.
    refreshed: Option<Arc<Refreshed>>,
    /// Counts the publications served, so a recording whose scan started
    /// from an earlier one knows to scan again.
    generation: u64,
    /// Why the last build or call-graph refresh failed, until one succeeds
    /// (see [`Checkout::note_outcome`]).
    failure: Option<Failure>,
}

/// Why the last build or call-graph refresh of a checkout failed, kept until
/// a later one succeeds, so status and health can say why a plane is not
/// ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// `build` or `refresh`.
    pub stage: &'static str,
    pub error: String,
    /// The family-store file a newer build wrote, when that made it fail;
    /// `error` is then its refusal message.
    pub refusal: Option<aft_store::family::NewerFormat>,
    /// Another process held the family store's writer past the wait; the
    /// next attempt may succeed without any change.
    pub busy: bool,
    /// When it failed, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// The files resolved again since a publication, with the paths they were
/// resolved over.
#[derive(Clone)]
struct Refreshed {
    state: CheckoutState,
    /// Each re-resolved file's resolution, encoded, and its parse.
    replaced: BTreeMap<String, RefreshedFile>,
    /// Files the publication has that the checkout no longer parses.
    removed: BTreeSet<String>,
    /// The content record of each file whose content changed since the
    /// publication, which dead code reads its stored facts from: the
    /// publication holds the file's old record. Kept encoded and shared
    /// with the changes that hold them.
    records: BTreeMap<String, Arc<aft_store::content::EncodedRecord>>,
    /// The dead-code snapshot of this call graph, built on first use.
    dead_code: DeadCodeMemo,
}

/// One file resolved again since a publication, as [`Refreshed`] keeps it.
#[derive(Clone)]
struct RefreshedFile {
    /// Its resolution, encoded as a publication stores it, with its parse
    /// key; shared with the engine that resolved it.
    resolution: Arc<EncodedResolution>,
    /// Its parse, when the publication holds no parse of that key for its
    /// path because its content changed since; the changed path's record
    /// holds the same parse. `None` when the publication's parse is it.
    parse: Option<Arc<ParseBlob>>,
}

/// Each published path's [`PathText`] once read, by position: a lexical query
/// scores thousands of candidates, and every read otherwise goes through the
/// block cache. A publication never changes, so an entry never goes stale.
struct PathTextMemo(Box<[std::sync::atomic::AtomicU32]>);

impl PathTextMemo {
    /// Not read yet.
    const UNKNOWN: u32 = u32::MAX;
    const UNINDEXED: u32 = u32::MAX - 1;
    const EXCLUDED: u32 = u32::MAX - 2;

    fn new(paths: usize) -> Self {
        Self(
            (0..paths)
                .map(|_| std::sync::atomic::AtomicU32::new(Self::UNKNOWN))
                .collect(),
        )
    }

    fn get(&self, index: usize) -> Option<PathText> {
        match self.0[index].load(std::sync::atomic::Ordering::Relaxed) {
            Self::UNKNOWN => None,
            Self::UNINDEXED => Some(PathText::Unindexed),
            Self::EXCLUDED => Some(PathText::Excluded),
            distinct => Some(PathText::Indexed { distinct }),
        }
    }

    /// Records `text`, unless it is a distinct count too large to tell from
    /// the markers, which is then read again each time.
    fn set(&self, index: usize, text: PathText) {
        let code = match text {
            PathText::Indexed { distinct } if distinct < Self::EXCLUDED => distinct,
            PathText::Indexed { .. } => return,
            PathText::Unindexed => Self::UNINDEXED,
            PathText::Excluded => Self::EXCLUDED,
        };
        self.0[index].store(code, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A dead-code snapshot built once for one call graph and shared by every
/// request that asks for it while that graph is served. Building it reads
/// every stored record and fragment, so concurrent first requests wait for
/// one build rather than each starting their own. A copy starts empty: a
/// memo belongs to the call graph it was built for.
#[derive(Default)]
struct DeadCodeMemo(Mutex<Option<Arc<crate::inspect::job::CallgraphSnapshot>>>);

impl Clone for DeadCodeMemo {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl DeadCodeMemo {
    fn get_or_build(
        &self,
        build: impl FnOnce() -> Result<crate::inspect::job::CallgraphSnapshot, DataError>,
    ) -> Result<Arc<crate::inspect::job::CallgraphSnapshot>, DataError> {
        let mut slot = self.0.lock().expect("no panics while holding it");
        if let Some(snapshot) = slot.as_ref() {
            return Ok(Arc::clone(snapshot));
        }
        let snapshot = Arc::new(build()?);
        *slot = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    /// Drops the built snapshot, if any, so the next request builds it
    /// again. True when one was held.
    fn release(&self) -> bool {
        self.0
            .lock()
            .expect("no panics while holding it")
            .take()
            .is_some()
    }
}

/// How long a build waits for another process to release the store's
/// writer before it gives up and reports the store busy.
const WRITER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many parses, and separately resolutions, the call-graph engine
/// keeps decoded between updates when it can decode them again: from the
/// publication, or for a resolution an update made, from its encoding (see
/// `update_engine`). A decoded resolution of a large repository's file is
/// about half a megabyte: at 1,024 files each worktree an agent was editing
/// held 0.5 to 3 GB, and four such worktrees took the daemon past 20 GB.
/// The next update decodes again what it reads, at a fraction of a
/// millisecond a file.
const ENGINE_DECODED_FILES: usize = 64;

/// One path changed since the publication.
#[derive(Clone)]
struct Change {
    /// The change count of its latest change.
    sequence: u64,
    /// Its record now; `None` once it left the checkout.
    record: Option<PathRecord>,
    /// What the call graph reads from it now, extracted when first needed.
    /// Every copy of the change shares the one extraction.
    content: Arc<ChangeContent>,
}

impl Change {
    /// Whether the change is a file in a parsed language whose extraction
    /// ran and failed. Until a refresh extracts it, a changed file is
    /// pending in the call graph, not unparsed.
    fn failed(&self) -> bool {
        self.content
            .peek()
            .is_some_and(|extracted| matches!(extracted, Extracted::Nothing))
            && self.record.as_ref().is_some_and(is_unparsed)
    }
}

/// What the call graph reads from one changed path, extracted the first
/// time a call-graph refresh (or a publication of the changes) needs it
/// rather than when the change is recorded: recording an edit only reads
/// the file and indexes its text, so an edit tool's call returns without
/// parsing the file, and a burst of edits to one file is parsed once, by
/// the refresh that follows it.
struct ChangeContent {
    /// The bytes the recording read, with what they are parsed as, until
    /// they are extracted; `None` once extracted, or when there is nothing
    /// to extract.
    source: Mutex<Option<RecordedSource>>,
    extracted: std::sync::OnceLock<Extracted>,
}

/// A changed file's bytes as the recording read them, to extract later.
struct RecordedSource {
    /// The file's path, relative to the root: a configuration file's TODO
    /// markers are read as the language its name calls for.
    path: Vec<u8>,
    /// The language the file's path and bytes chose (see `language_for`).
    language: String,
    content: ContentKey,
    bytes: Vec<u8>,
}

impl ChangeContent {
    /// Content the call graph reads nothing from: not a parsed language,
    /// not text, or gone.
    fn nothing() -> Arc<Self> {
        Arc::new(Self {
            source: Mutex::new(None),
            extracted: std::sync::OnceLock::from(Extracted::Nothing),
        })
    }

    /// `record`'s content, read by the recording as `bytes`: extracted when
    /// first needed if the record is text in a parsed language (its
    /// language already set), nothing otherwise.
    fn recorded(record: &PathRecord, bytes: Vec<u8>) -> Arc<Self> {
        if !matches!(record.kind, PathKind::Text | PathKind::Large) || record.language.is_empty() {
            return Self::nothing();
        }
        Arc::new(Self {
            source: Mutex::new(Some(RecordedSource {
                path: record.path.clone(),
                language: record.language.clone(),
                content: record.content,
                bytes,
            })),
            extracted: std::sync::OnceLock::new(),
        })
    }

    /// What the call graph reads from the file, extracting it now if no
    /// one has yet; the bytes are let go once extracted. An extraction that
    /// panics leaves them, so the next refresh extracts them again.
    fn get(&self) -> &Extracted {
        self.extracted.get_or_init(|| {
            let mut source = self
                .source
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let extracted = source.as_ref().map_or(Extracted::Nothing, extract_recorded);
            *source = None;
            extracted
        })
    }

    /// What the call graph reads from the file, if it was extracted.
    fn peek(&self) -> Option<&Extracted> {
        self.extracted.get()
    }
}

/// What the call graph reads from one changed path.
#[derive(Clone)]
enum Extracted {
    /// Nothing: not a parsed language, not text, or gone.
    Nothing,
    /// A parsed file: its parse key and parse, and the content record
    /// they came from, encoded, which a publication of the change stores.
    /// Decoded, a large file's record takes about as much as its parse,
    /// and nothing reads it before the publication.
    Parse(String, Arc<ParseBlob>, Arc<EncodedRecord>),
    /// A resolver configuration file's bytes, and its content record,
    /// encoded.
    Config(Arc<[u8]>, Arc<EncodedRecord>),
    /// A file the call graph does not read but whose TODO markers are
    /// counted: its record of the markers alone, encoded.
    Markers(Arc<EncodedRecord>),
}

/// What publishing the changes since a publication wrote.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct DeltaReport {
    /// Paths changed since the publication.
    pub paths: usize,
    /// Fragments written: files resolved again, or whose stored terms no
    /// longer applied.
    pub fragments: usize,
    /// Content records written: the changed parsed and configuration files.
    pub records: usize,
    pub text_units: usize,
    /// Segments this checkout's text, content and call graph read from
    /// afterwards. Vector segments and other checkouts' segments are not
    /// counted: a fold of this checkout's segments leaves them.
    pub segments: usize,
    /// Every segment this checkout's text, content and call graph read from
    /// afterwards, those other checkouts read too included.
    pub read_segments: usize,
    /// What the new segment left out because another checkout already reads
    /// it.
    pub shared: SharedUnits,
    /// Segments no checkout read any more, and other checkouts whose folder
    /// is gone, left out of the new generation as a build leaves them out.
    pub retired_segments: usize,
    pub retired_checkouts: usize,
    pub ms: f64,
}

/// How many segments a build or a fold reads shared units from besides its
/// own: the segments that serve it the most units other checkouts read too.
/// Whatever other segments hold for it, it writes again.
pub(crate) const SHARE_FROM: usize = 4;

/// A checkout whose text and call graph read from more segments than this
/// that no other checkout reads has them folded ([`Checkout::fold`]), which
/// copies every unit it reads into one segment and retires what no other
/// checkout reads; each publication of its changes adds one. Vector
/// segments are not counted, since a fold does not copy them.
pub(crate) const COMPACT_AT: usize = 8;

/// A checkout that reads from more segments than this in all, those other
/// checkouts read too included, has them folded as well: a fold reads from
/// at most [`SHARE_FROM`] of them besides its own, so the units it reads
/// from any others move into its new segment. Without it, a checkout that
/// takes in other checkouts' changes (a merge, a pull) read from each of
/// their change segments, and kept every one of them in the store for as
/// long as it held the same contents.
pub(crate) const READ_AT: usize = 16;

const _: () = assert!(
    READ_AT > SHARE_FROM + 1,
    "a fold must leave a checkout reading fewer segments than call for one"
);

impl DeltaReport {
    /// Whether the checkout reads from so many segments that they are to be
    /// folded: past [`COMPACT_AT`] only it reads, or past [`READ_AT`] in
    /// all.
    #[must_use]
    pub fn wants_fold(&self) -> bool {
        self.segments > COMPACT_AT || self.read_segments > READ_AT
    }
}

/// What a build did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BuildReport {
    pub files: usize,
    pub text_units: usize,
    /// What the new segment left out because another checkout already
    /// reads it from a segment this build shares.
    pub shared: SharedUnits,
    pub parsed_files: usize,
    pub extract_failures: usize,
    pub scan_ms: f64,
    pub resolve_ms: f64,
    pub write_ms: f64,
    pub publish_ms: f64,
    /// For a checkout with nothing published yet, when its text index was
    /// published, ahead of the call graph: milliseconds from the build's
    /// start.
    pub text_ready_ms: Option<f64>,
    pub segment_bytes: u64,
    /// Segments no checkout needs any more, left out of the new generation.
    pub retired_segments: usize,
    /// Other checkouts of the family whose folder is gone, left out of the
    /// new generation with their states.
    pub retired_checkouts: usize,
    /// Another process published this checkout while the build ran, so
    /// nothing was published; the checkout serves that publication.
    pub superseded: bool,
    /// When the build started from another checkout's publication, how
    /// many paths differed from it; `None` for a build from scratch.
    pub started_from_sibling: Option<usize>,
    /// Content records the build's scan read from the store instead of
    /// parsing their files again.
    pub stored_records: usize,
    /// Files the reclamation after publishing removed, or why it could not
    /// run (the publication stands either way).
    pub reclaimed_files: usize,
    pub reclaim_error: Option<String>,
    /// What the build held at each stage, in bytes, as `stage.part`; only
    /// from [`Checkout::build_measuring_memory`], since walking the
    /// structures costs time.
    pub memory: Vec<(String, usize)>,
}

impl Checkout {
    /// Binds the checkout at `root` to the family store at `family_dir`.
    /// If the family has a published state for this checkout, the view
    /// serves it at once; otherwise there is no view until
    /// [`build`](Self::build).
    ///
    /// # Errors
    ///
    /// When the family store cannot be opened, or the published state
    /// cannot be read.
    pub fn bind(
        layer: Arc<DataLayer>,
        family_dir: &Path,
        family: FamilyId,
        root: &Path,
    ) -> Result<Self, DataError> {
        Self::bind_planes(layer, family_dir, family, root, Planes::ALL)
    }

    /// As [`Self::bind`], building and serving only `planes`.
    ///
    /// # Errors
    ///
    /// As [`Self::bind`].
    pub fn bind_planes(
        layer: Arc<DataLayer>,
        family_dir: &Path,
        family: FamilyId,
        root: &Path,
        planes: Planes,
    ) -> Result<Self, DataError> {
        let checkout = CheckoutId::from_digest(Digest128::of(root.as_os_str().as_encoded_bytes()));
        let family = Family::open(family_dir, family).map_err(DataError::store)?;
        let live = Arc::new(RwLock::new(Live::default()));
        {
            let mut bound = layer.bound.lock().expect("no panics while holding it");
            bound.retain(|(_, live)| live.strong_count() > 0);
            bound.push((family.dir().to_path_buf(), Arc::downgrade(&live)));
        }
        let memory_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let engine = layer
            .engines
            .slot(root.to_path_buf(), Arc::clone(&memory_epoch));
        let handle = Self {
            layer,
            planes,
            root: root.to_path_buf(),
            family,
            checkout,
            live,
            engine,
            recording: Mutex::new(()),
            memory_epoch,
            readiness: (Mutex::new(0), std::sync::Condvar::new()),
        };
        handle.adopt_newest()?;
        Ok(handle)
    }

    /// Serves the newest publication of this checkout, if any. Files may
    /// have changed while no process watched them (or since another process
    /// scanned), so before the view is served the checkout is compared with
    /// what was published: the scan reads only files whose stat no longer
    /// proves their recorded content, and every difference is recorded as a
    /// change.
    fn adopt_newest(&self) -> Result<(), DataError> {
        // A recording waits, so none lands between the scan and the swap.
        let _order = self.recording.lock().expect("no panics while holding it");
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let Some(published) = self.open_published(snapshot)? else {
            return Ok(());
        };
        let published = Arc::new(published);
        let sink = ChangeSink::default();
        let scanned = scan(
            &self.root,
            &self.layer.policy,
            Some(&published.state),
            &sink,
        )
        .map_err(|error| DataError::Scan(error.to_string()))?;
        let changed = changed_members(&published.state, &scanned.state);
        let generation = {
            // The scan compared the files on disk with this publication, so
            // changes recorded against an earlier one are replaced by what it
            // found.
            let mut live = self.write_live();
            live.unparsed = Arc::new(published.unparsed.clone());
            live.published = Some(Arc::clone(&published));
            live.changes.clear();
            live.early.clear();
            live.state = None;
            live.text = Arc::new(TextOverlay::default());
            live.text_members = Arc::default();
            live.callgraph_pending = Arc::new(BTreeSet::new());
            live.graph = None;
            live.refreshed = None;
            live.generation += 1;
            live.generation
        };
        let applied = self.apply_scan(
            &published.state,
            generation,
            scanned.state,
            &changed,
            sink.finish(),
        );
        debug_assert!(applied, "the recording lock keeps the publication");
        self.notify_readiness_change();
        Ok(())
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        *self
            .readiness
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Capture the generation before checking readiness, then wait with no
    /// checkout or writer lock held. A publication between the check and this
    /// wait changes the generation, so its wake cannot be lost.
    pub(crate) fn wait_for_readiness_change(
        &self,
        generation: &mut u64,
        wait: std::time::Duration,
    ) {
        let guard = self
            .readiness
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (guard, _) = self
            .readiness
            .1
            .wait_timeout_while(guard, wait, |current| *current == *generation)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *generation = *guard;
    }

    pub(crate) fn notify_readiness_change(&self) {
        let mut generation = self
            .readiness
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *generation = generation.wrapping_add(1);
        self.readiness.1.notify_all();
    }

    /// The checkout's root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The planes this checkout builds and serves.
    #[must_use]
    pub fn planes(&self) -> Planes {
        self.planes
    }

    fn write_live(&self) -> std::sync::RwLockWriteGuard<'_, Live> {
        self.live.write().expect("no panics while holding it")
    }

    /// The checkout's data as queries see it now: the last publication with
    /// the changes made since laid over it. `None` until the first build
    /// publishes.
    #[must_use]
    pub fn view(&self) -> Option<CheckoutView> {
        let live = self.live.read().expect("no panics while holding it");
        let published = Arc::clone(live.published.as_ref()?);
        let graph = live.graph.clone().unwrap_or_else(|| {
            let graph: Arc<dyn SliceSource + Send + Sync> = published.graph.clone();
            graph
        });
        Some(CheckoutView {
            published,
            planes: self.planes,
            text: Arc::clone(&live.text),
            text_members: Arc::clone(&live.text_members),
            callgraph_pending: Arc::clone(&live.callgraph_pending),
            rescanning: live.rescanning,
            graph,
            refreshed: live.refreshed.clone(),
            sequence: live.sequence,
            unparsed: Arc::clone(&live.unparsed),
            current: live.state.clone(),
        })
    }

    /// How each plane stands, for status and for replies that disclose
    /// what they could not cover.
    #[must_use]
    pub fn status(&self) -> CheckoutStatus {
        let building = self
            .live
            .read()
            .expect("no panics while holding it")
            .building;
        let failed = self
            .live
            .read()
            .expect("no panics while holding it")
            .failure
            .is_some();
        if let Some(view) = self.view() {
            return CheckoutStatus {
                building,
                failed,
                ..view.status()
            };
        }
        let plane = |on: bool| {
            if building && on {
                PlaneStatus::Building {
                    done: 0,
                    total: None,
                }
            } else {
                PlaneStatus::Off
            }
        };
        CheckoutStatus {
            text: plane(self.planes.text),
            callgraph: plane(self.planes.callgraph),
            callgraph_published: false,
            planes: self.planes,
            callgraph_pending: 0,
            building,
            unparsed: 0,
            failed,
        }
    }

    /// Why the last build or call-graph refresh failed, until one succeeds.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        self.last_failure().map(|failure| failure.error)
    }

    /// The last failed build or call-graph refresh, until one succeeds.
    #[must_use]
    pub fn last_failure(&self) -> Option<Failure> {
        self.live
            .read()
            .expect("no panics while holding it")
            .failure
            .clone()
    }

    /// Records how a build or refresh (`stage`) ended. A failure replaces
    /// the previous one; a cancelled run changes nothing. A successful build
    /// clears any failure, since it serves a new publication. A successful
    /// refresh clears only a refresh failure: a failed build still left the
    /// checkout without what it was building.
    pub(crate) fn note_outcome<T>(&self, stage: &'static str, result: &Result<T, DataError>) {
        let mut live = self.write_live();
        match result {
            Err(DataError::Cancelled) => {}
            Err(error) => {
                live.failure = Some(Failure {
                    stage,
                    error: error.to_string(),
                    refusal: error.refusal().cloned(),
                    busy: matches!(error, DataError::Busy(_)),
                    at_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |elapsed| {
                            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                        }),
                });
            }
            Ok(_) => {
                if stage == "build"
                    || live
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.stage == stage)
                {
                    live.failure = None;
                }
            }
        }
        drop(live);
        self.notify_readiness_change();
    }

    /// Whether the checkout needs a build: nothing is published, or what is
    /// published has no call graph while this checkout builds one (a build
    /// published the text index first and stopped before the call graph,
    /// or another process did).
    #[must_use]
    pub fn needs_build(&self) -> bool {
        self.live
            .read()
            .expect("no panics while holding it")
            .published
            .as_ref()
            .is_none_or(|published| self.planes.callgraph && !published.graph_built)
    }

    /// Marks a rescan as started: until [`Self::end_rescan`], the call
    /// graph is reported building, so a reader that needs exact answers
    /// waits for what the rescan finds instead of answering without it.
    pub fn begin_rescan(&self) {
        self.write_live().rescanning = true;
    }

    /// Ends what [`Self::begin_rescan`] started.
    pub fn end_rescan(&self) {
        self.write_live().rescanning = false;
    }

    /// Compares the checkout's files with its current state and records
    /// every difference as a change: files edited, added or removed while no
    /// watcher reported them, because the root was unbound or the event had
    /// not arrived yet. Like a bind, it reads only files whose stat no longer
    /// proves their recorded content. Returns how many paths it recorded.
    ///
    /// # Errors
    ///
    /// When the checkout cannot be scanned.
    pub fn rescan(&self) -> Result<usize, DataError> {
        let current = {
            let live = self.live.read().expect("no panics while holding it");
            match (&live.state, &live.published) {
                (Some(state), _) => Arc::clone(state),
                (None, Some(published)) => Arc::new(published.state.clone()),
                (None, None) => return Ok(0),
            }
        };
        let scanned = scan(
            &self.root,
            &self.layer.policy,
            Some(&current),
            &ChangeSink::default(),
        )
        .map_err(|error| DataError::Scan(error.to_string()))?;
        let changed = changed_members(&current, &scanned.state);
        // Recording scans each path again under the same rules, so a change
        // recorded meanwhile is taken from the disk once more, not lost.
        self.record_paths(&changed)?;
        Ok(changed.len())
    }

    /// Records that `paths` (relative to the root, such as a watcher or an
    /// edit reports them) may have changed, so queries see them at once.
    /// Each path, and everything under it when it is a folder, is scanned
    /// again under the same rules a build's scan applies: ignored files,
    /// build folders, files over the size limit and binary files are
    /// treated as a build treats them. Text queries then read each changed
    /// file's new trigrams, and the call graph reports the paths as pending
    /// until they are resolved again.
    ///
    /// # Errors
    ///
    /// When a path's folder cannot be walked. Nothing is recorded then.
    pub fn record_paths(&self, paths: &[Vec<u8>]) -> Result<(), DataError> {
        if paths.is_empty() {
            return Ok(());
        }
        self.layer.engines.touch(&self.engine);
        let _order = self.recording.lock().expect("no panics while holding it");
        // An ignore file changes which files under its folder are members,
        // so that folder is scanned again whole; one at the root (or Git's
        // own exclude file) means the whole checkout.
        let mut whole = false;
        let mut scoped = Vec::with_capacity(paths.len());
        for path in paths {
            match self.ignore_scope(path) {
                IgnoreScope::None => scoped.push(path.clone()),
                IgnoreScope::Folder(folder) => {
                    scoped.push(path.clone());
                    scoped.push(folder);
                }
                IgnoreScope::Whole => whole = true,
            }
        }
        // A build may publish while this scan runs. The scan's state is
        // built from the publication it started from, so it is applied only
        // if that publication is still served, and scanned again otherwise.
        loop {
            let (base, generation) = {
                let live = self.live.read().expect("no panics while holding it");
                let base = live.state.clone().or_else(|| {
                    live.published
                        .as_ref()
                        .map(|published| Arc::new(published.state.clone()))
                });
                (base, live.generation)
            };
            let Some(base) = base else {
                let mut live = self.write_live();
                live.sequence += 1;
                let sequence = live.sequence;
                for path in paths {
                    live.early.insert(path.clone(), sequence);
                }
                self.touch_memory();
                return Ok(());
            };
            let sink = ChangeSink::default();
            let (state, changed) = if whole {
                let scanned = scan(&self.root, &self.layer.policy, Some(&base), &sink)
                    .map_err(|error| DataError::Scan(error.to_string()))?;
                let changed = changed_members(&base, &scanned.state);
                (scanned.state, changed)
            } else {
                let scanned = scan_paths(&self.root, &self.layer.policy, &base, &scoped, &sink)
                    .map_err(|error| DataError::Scan(error.to_string()))?;
                (scanned.state, scanned.changed)
            };
            if self.apply_scan(&base, generation, state, &changed, sink.finish()) {
                return Ok(());
            }
        }
    }

    /// Which folder a change to `path` makes a scan cover again, when
    /// `path` is an ignore file the scan reads.
    fn ignore_scope(&self, path: &[u8]) -> IgnoreScope {
        if path == b".git/info/exclude" {
            return IgnoreScope::Whole;
        }
        let (folder, name) = match path.iter().rposition(|byte| *byte == b'/') {
            Some(index) => (&path[..index], &path[index + 1..]),
            None => (&path[..0], path),
        };
        let is_ignore_file = name == b".gitignore"
            || name == b".ignore"
            || self
                .layer
                .policy
                .ignore_files
                .iter()
                .any(|file| file.as_bytes() == name);
        match (is_ignore_file, folder.is_empty()) {
            (false, _) => IgnoreScope::None,
            (true, true) => IgnoreScope::Whole,
            (true, false) => IgnoreScope::Folder(folder.to_vec()),
        }
    }

    /// Makes `state` the checkout's current state, recording each path in
    /// `changed` (compared with `base`) as a change, with the files the scan
    /// read in `read`.
    /// Returns `false`, applying nothing, when a publication served since
    /// `generation` replaced the one `base` came from.
    fn apply_scan(
        &self,
        base: &CheckoutState,
        generation: u64,
        mut state: CheckoutState,
        changed: &[Vec<u8>],
        mut read: HashMap<Vec<u8>, Vec<u8>>,
    ) -> bool {
        // A record the scan read again keeps its language when its content
        // did not change; a changed text file gets the language its path and
        // bytes choose. Only changed paths get a language anew: a file read
        // again only because its stat moved (a touch, a copied checkout) has
        // the content it had. The scanned state is this scan's own, so its
        // records take their languages in place rather than being copied.
        // Choosing a language reads the path, and only for a C header parses
        // the bytes; the changed files' extraction waits for the call graph
        // (see `ChangeContent`).
        let mut languages = Vec::new();
        for path in changed {
            let (Some(bytes), Some(index)) = (read.get(path), state.position(path)) else {
                continue;
            };
            languages.push((index, changed_language(&state.paths()[index], bytes)));
        }
        // Every other record without a language takes the base's when its
        // content is the same. Both states are in path order, so one pass
        // walks the two together.
        let mut extracted_at = languages
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        extracted_at.sort_unstable();
        let mut before = base.paths().iter().peekable();
        for (index, record) in state.paths().iter().enumerate() {
            if !record.language.is_empty() || extracted_at.binary_search(&index).is_ok() {
                continue;
            }
            while before.next_if(|before| before.path < record.path).is_some() {}
            if let Some(before) = before.peek().filter(|before| {
                before.path == record.path
                    && before.content == record.content
                    && !before.language.is_empty()
            }) {
                languages.push((index, before.language.clone()));
            }
        }
        for (index, language) in languages {
            state
                .set_language(index, language)
                .expect("a language a state stores");
        }
        if changed.is_empty() {
            let mut live = self.write_live();
            if live.generation != generation {
                return false;
            }
            live.state = Some(Arc::new(state));
            return true;
        }
        let mut entries = Vec::with_capacity(changed.len());
        for path in changed {
            let record = state.get(path).cloned();
            // Text queries read the new trigrams at once; what the call graph
            // reads is extracted from the same bytes when it first needs it.
            let bytes = read.remove(path);
            let entry = match (&record, &bytes) {
                (None, _) => OverlayEntry::Deleted,
                (Some(record), Some(bytes)) if record.kind == PathKind::Text => {
                    OverlayEntry::Content(Arc::new(ContentTrigrams::of(bytes)))
                }
                (Some(_), _) => OverlayEntry::Unindexed,
            };
            let content = match (&record, bytes) {
                (Some(record), Some(bytes)) => ChangeContent::recorded(record, bytes),
                _ => ChangeContent::nothing(),
            };
            entries.push((path.clone(), entry, record, content));
        }
        let mut guard = self.write_live();
        let live = &mut *guard;
        if live.generation != generation {
            return false;
        }
        live.sequence += 1;
        self.touch_memory();
        let sequence = live.sequence;
        // Each set is copied only when a view still holds it; otherwise the
        // changed paths are set in place.
        let text = Arc::make_mut(&mut live.text);
        let pending = Arc::make_mut(&mut live.callgraph_pending);
        let unparsed = Arc::make_mut(&mut live.unparsed);
        for (path, entry, record, content) in entries {
            pending.insert(path.clone());
            let change = Change {
                sequence,
                record,
                content,
            };
            if change.failed() {
                unparsed.insert(path.clone());
            } else {
                unparsed.remove(&path);
            }
            text.set(path.clone(), entry);
            live.changes.insert(path, change);
        }
        live.text_members = Arc::default();
        live.state = Some(Arc::new(state));
        true
    }

    /// Scans the checkout, builds every plane cold, publishes them, and
    /// serves the new view.
    ///
    /// # Errors
    ///
    /// When the scan, resolution or store fails. Nothing is published then,
    /// and the previous view, if any, keeps serving.
    pub fn build(&self) -> Result<BuildReport, DataError> {
        self.build_as(false)
    }

    /// [`Self::build`], also reporting in [`BuildReport::memory`] what the
    /// build held at each stage. For measurement.
    ///
    /// # Errors
    ///
    /// As [`Self::build`].
    #[doc(hidden)]
    pub fn build_measuring_memory(&self) -> Result<BuildReport, DataError> {
        self.build_as(true)
    }

    fn build_as(&self, measure: bool) -> Result<BuildReport, DataError> {
        let _alloc_label =
            crate::alloc_trace::scope(crate::alloc_trace::Label::INDEX_BUILD, Some(&self.root));
        self.write_live().building = true;
        let result = self.build_inner(measure);
        self.write_live().building = false;
        self.note_outcome("build", &result);
        result
    }

    fn build_inner(&self, measure: bool) -> Result<BuildReport, DataError> {
        let mut report = BuildReport::default();
        let build_started = Instant::now();
        // A newer build wrote a file the build would retire, reclaim or
        // need: the writer refuses then, so refuse before the scan and
        // resolution rather than after them. So does the writer below the
        // free-disk floor.
        self.family.check_formats().map_err(DataError::store)?;
        self.check_disk_floor()?;
        if !self.planes.callgraph {
            // Without a call graph, a build is the text scan alone.
            checkpoint()?;
            let published = self.publish_text(&mut report)?;
            report.superseded = !published;
            report.text_ready_ms = Some(elapsed_ms(build_started));
            return Ok(report);
        }
        // A checkout with nothing published gets its text index first: a
        // text-only scan takes about a second where the call graph takes
        // far longer, and text queries need nothing else.
        let unpublished = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .is_none();
        checkpoint()?;
        // A checkout another one of the family is close to starts from that
        // one's publication rather than building cold.
        if unpublished && self.planes.text && self.planes.callgraph {
            if let Some(report) = self.start_from_sibling(build_started)? {
                return Ok(report);
            }
        }
        if unpublished && self.planes.text {
            if !self.publish_text_first()? {
                report.superseded = true;
                return Ok(report);
            }
            report.text_ready_ms = Some(elapsed_ms(build_started));
        }
        #[cfg(test)]
        while call_graph_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&self.root)
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        checkpoint()?;
        // A change recorded before the scan starts is in what the scan
        // reads; one recorded later may not be.
        let covered = self
            .live
            .read()
            .expect("no panics while holding it")
            .sequence;
        // What this checkout's published state was when the scan started:
        // if another process publishes a newer one before this build does,
        // this build's scan may be the older one.
        let started_from = self
            .family
            .snapshot()
            .map_err(DataError::store)?
            .manifest()
            .checkout(self.checkout)
            .cloned();
        let started = Instant::now();
        let sink = BuildSink {
            skip_text: !self.planes.text,
            stored: self.stored_records()?,
            ..BuildSink::default()
        };
        let scanned = scan(&self.root, &self.layer.policy, None, &sink)
            .map_err(|error| DataError::Scan(error.to_string()))?;
        let mut collected = sink.finish();
        // Contents too large to read are members whose text is not
        // indexed; record them so grep reports them rather than walking
        // them.
        for record in scanned.state.paths() {
            if self.planes.text
                && record.kind == PathKind::Text
                && record.stat.size > self.layer.policy.max_indexed_len
                && collected.seen_text.insert(record.content)
            {
                collected.text.add_unindexed(
                    record.content,
                    record.stat.size,
                    TextStatus::Oversize,
                );
            }
        }
        report.files = scanned.state.paths().len();
        report.text_units = collected.text.len();
        report.extract_failures = collected.failures;
        report.stored_records = collected.reused;
        report.scan_ms = elapsed_ms(started);

        // The state, with each parsed path's language.
        let records = scanned
            .state
            .paths()
            .iter()
            .map(|record| PathRecord {
                language: collected
                    .languages
                    .get(&record.path)
                    .cloned()
                    .unwrap_or_default(),
                ..record.clone()
            })
            .collect::<Vec<_>>();
        let state =
            CheckoutState::new(scanned.state.git.clone(), scanned.state.conversion, records)
                .map_err(DataError::store)?
                .with_root_of(&scanned.state);
        if measure {
            use aft_store::heap::HeapBytes as _;
            let mut seen = aft_store::heap::Seen::new();
            report.memory.extend([
                (
                    "scan.text".to_string(),
                    collected.text.heap_bytes(&mut seen),
                ),
                (
                    "scan.records".to_string(),
                    collected
                        .records
                        .iter()
                        .map(|(key, (_, record))| {
                            key.heap_bytes(&mut seen) + record.heap_bytes(&mut seen)
                        })
                        .sum(),
                ),
                ("scan.state".to_string(), state.heap_bytes(&mut seen)),
            ]);
        }
        // The segment needs only each record's encoded bytes, a fifth of the
        // decoded size, and the resolver decodes each parse from them when it
        // reads it; the decoded records go before resolving.
        let taking = Instant::now();
        let (mut inputs, encoded) = collected.take_inputs(&state);
        report.parsed_files = inputs.parses.len();
        if measure {
            report.memory.push((
                "ms.take_inputs".to_string(),
                usize::try_from(taking.elapsed().as_millis()).unwrap_or(usize::MAX),
            ));
        }
        if measure {
            use aft_store::heap::HeapBytes as _;
            let mut seen = aft_store::heap::Seen::new();
            report.memory.extend([
                (
                    "scan.records_encoded".to_string(),
                    encoded.heap_bytes(&mut seen),
                ),
                (
                    "scan.configs".to_string(),
                    inputs.configs.heap_bytes(&mut seen),
                ),
            ]);
        }
        checkpoint()?;

        // Each file's fragment is built as soon as its resolution is done, so
        // no more than a batch of resolutions is held decoded at once;
        // `resolve_ms` covers building the fragments too.
        let started = Instant::now();
        note_source_work(&self.root, |work| work.resolutions += 1);
        let mut sections = super::graph_store::SectionsWriter::new(
            &inputs.state,
            &collected.records,
            true,
            Some(encoded),
        );
        super::callgraph::resolve_checkout_into(
            &mut inputs,
            self.layer.threads,
            measure.then_some(&mut report.memory),
            &mut |inputs, files| {
                sections
                    .add_batch(inputs, files)
                    .map_err(crate::callgraph_store::join::ManifestJoinError::Parse)
            },
        )
        .map_err(|error| DataError::Resolve(error.to_string()))?;
        report.resolve_ms = elapsed_ms(started);
        checkpoint()?;

        let started = Instant::now();
        #[cfg(test)]
        wait_while_publication_held_for_test(&self.root);
        let mut session = self.writer()?;
        if session.base().checkout(self.checkout) != started_from.as_ref() {
            // Another process published this checkout after the scan began.
            // Its state may be newer than this scan, so this build stands
            // down: it serves that publication and records what differs from
            // the files now on disk, as a bind does.
            drop(session);
            report.superseded = true;
            self.adopt_newest()?;
            return Ok(report);
        }
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        report.shared =
            self.leave_out_shared(&snapshot, &mut collected.text, Some(&mut sections))?;
        let mut pending = session.create_segment().map_err(DataError::store)?;
        collected
            .text
            .write(pending.writer())
            .map_err(DataError::store)?;
        let fragments = sections
            .finish(pending.writer(), measure.then_some(&mut report.memory))
            .map_err(DataError::Store)?;
        let segment = session.finish_segment(pending).map_err(DataError::store)?;
        report.write_ms = elapsed_ms(started);

        let started = Instant::now();
        let per_path = state
            .paths()
            .iter()
            .map(|record| {
                std::str::from_utf8(&record.path)
                    .ok()
                    .and_then(|path| fragments.get(path))
                    .map(|key| key.digest())
            })
            .collect();
        let state = state
            .with_fragments_for(per_path, EXTRACTOR_VERSION)
            .map_err(DataError::store)?;
        let written = session
            .write_checkout_state(self.checkout, &state)
            .map_err(DataError::store)?;
        // The new segment holds everything this checkout's new state needs,
        // so an older segment stays only while another checkout reads from
        // it. A checkout whose folder is gone reads nothing any more.
        let unneeded = self.unneeded(&state, false)?;
        report.retired_segments = unneeded.segments.len();
        report.retired_checkouts = unneeded.checkouts.len();
        session
            .publish(&Publication {
                add_segments: vec![segment],
                remove_segments: unneeded.segments,
                states: vec![written],
                retire_checkouts: unneeded.checkouts,
            })
            .map_err(DataError::store)?;
        // Removes what no generation at or above the horizon names. A view
        // still reading an older generation holds a lease on it, so its
        // files stay until the view drops.
        match session.reclaim() {
            Ok(reclaimed) => {
                report.reclaimed_files = reclaimed.removed_segments
                    + reclaimed.removed_states
                    + reclaimed.removed_manifests;
            }
            Err(error) => report.reclaim_error = Some(error.to_string()),
        }
        drop(session);
        report.publish_ms = elapsed_ms(started);

        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        report.segment_bytes = snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| entry.len)
            .sum();
        self.serve_new(snapshot, covered)?;
        Ok(report)
    }

    /// Builds a checkout with nothing published from the publication of
    /// another checkout of the family, the one whose files differ least
    /// from this one's, the way an edit is processed. That checkout's state
    /// is published for this one as it stands, naming this checkout's root;
    /// the files that differ from it are read and recorded as changes, as a
    /// bind records what changed while nothing watched; the call graph is
    /// resolved again for them and every file their changes reach; and the
    /// changes are published. A worktree made from another checkout differs
    /// from it in a handful of files, and every new one used to read and
    /// parse every file and resolve the whole call graph again (15 to 60 s
    /// on 6 to 8 cores for a large repository), only for its build to find
    /// nearly every unit already stored.
    ///
    /// Every input the call graph records is keyed by a path inside the
    /// checkout: a symlink is recorded by its target's bytes, which differ
    /// when a link leaves the checkout for another folder, and resolution
    /// never follows a link out of the checkout. So two checkouts whose
    /// files are the same resolve the same wherever their folders are, and
    /// the paths that differ are all the changes there are.
    ///
    /// Every file is read once to learn its content, and none is parsed.
    /// `None` when no other checkout has a call graph published, when more
    /// than a quarter of the paths differ from the closest one (a cold
    /// build then costs about as much), or when that checkout publishes
    /// again first, since its new publication may retire what the state
    /// read here names; the caller then builds cold.
    fn start_from_sibling(&self, build_started: Instant) -> Result<Option<BuildReport>, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        if snapshot.manifest().checkout(self.checkout).is_some() {
            return Ok(None);
        }
        let mut siblings = Vec::new();
        for state_ref in snapshot.manifest().checkouts() {
            if state_ref.checkout == self.checkout {
                continue;
            }
            match snapshot.checkout_state(state_ref.checkout) {
                Ok(Some(state))
                    if state.has_fragments_for(EXTRACTOR_VERSION)
                        && !state.root().is_some_and(folder_is_gone) =>
                {
                    siblings.push((*state_ref, state));
                }
                Ok(_) => {}
                // One this build cannot read is no place to start from.
                Err(error) if error.is_unusable_file() => {}
                Err(error) => return Err(DataError::store(error)),
            }
        }
        drop(snapshot);
        if siblings.is_empty() {
            return Ok(None);
        }
        checkpoint()?;
        let started = Instant::now();
        let scanned = scan(&self.root, &self.layer.policy, None, &ContentOnly)
            .map_err(|error| DataError::Scan(error.to_string()))?;
        let Some((sibling_ref, sibling, differing)) = siblings
            .into_iter()
            .map(|(state_ref, state)| {
                let differing = changed_members(&state, &scanned.state).len();
                (state_ref, state, differing)
            })
            .min_by_key(|(_, _, differing)| *differing)
        else {
            return Ok(None);
        };
        let paths = scanned.state.paths().len().max(sibling.paths().len());
        if differing * 4 > paths {
            return Ok(None);
        }
        // The sibling's records, with this checkout's stat for each path
        // whose member is the same, so serving the state reads only the
        // files that differ. Another file's stat never proves a file
        // unchanged: it names another file id.
        let records = sibling
            .paths()
            .iter()
            .map(|record| match scanned.state.get(&record.path) {
                Some(now) if same_member(record, now) => PathRecord {
                    stat: now.stat,
                    ..record.clone()
                },
                _ => record.clone(),
            })
            .collect::<Vec<_>>();
        let fragments = (0..records.len())
            .map(|index| sibling.fragment(index))
            .collect();
        let start =
            CheckoutState::new(scanned.state.git.clone(), scanned.state.conversion, records)
                .map_err(DataError::store)?
                .with_root_of(&scanned.state)
                .with_fragments_for(fragments, EXTRACTOR_VERSION)
                .map_err(DataError::store)?;
        let scan_ms = elapsed_ms(started);
        checkpoint()?;

        let mut session = self.writer()?;
        if session.base().checkout(self.checkout).is_some() {
            // Another process published this checkout meanwhile: serve that.
            drop(session);
            self.adopt_newest()?;
            return Ok(Some(BuildReport {
                superseded: true,
                ..BuildReport::default()
            }));
        }
        if session.base().checkout(sibling_ref.checkout) != Some(&sibling_ref) {
            return Ok(None);
        }
        let written = session
            .write_checkout_state(self.checkout, &start)
            .map_err(DataError::store)?;
        session
            .publish(&Publication {
                states: vec![written],
                ..Publication::default()
            })
            .map_err(DataError::store)?;
        drop(session);
        // Served as a bind serves a publication: what differs from it on
        // disk is recorded as changes, and text queries read those at once.
        self.adopt_newest()?;
        let text_ready_ms = elapsed_ms(build_started);
        checkpoint()?;
        let started = Instant::now();
        self.refresh_callgraph()?;
        let resolve_ms = elapsed_ms(started);
        let started = Instant::now();
        let delta = self.publish_changes_as(true)?;
        let publish_ms = elapsed_ms(started);
        let segment_bytes = self
            .family
            .snapshot()
            .map_err(DataError::store)?
            .manifest()
            .segments()
            .iter()
            .map(|entry| entry.len)
            .sum();
        Ok(Some(BuildReport {
            files: start.paths().len(),
            started_from_sibling: Some(differing),
            shared: delta.as_ref().map(|delta| delta.shared).unwrap_or_default(),
            text_units: delta.as_ref().map_or(0, |delta| delta.text_units),
            scan_ms,
            resolve_ms,
            publish_ms,
            text_ready_ms: Some(text_ready_ms),
            segment_bytes,
            retired_segments: delta.as_ref().map_or(0, |delta| delta.retired_segments),
            retired_checkouts: delta.as_ref().map_or(0, |delta| delta.retired_checkouts),
            ..BuildReport::default()
        }))
    }

    /// The content records the family's newest generation stores, for a
    /// build's scan to read instead of parsing those files again. A
    /// segment this build cannot use is left out: its files are parsed.
    fn stored_records(&self) -> Result<StoredRecords, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let known = known_sections();
        let mut segments = Vec::new();
        for entry in snapshot.manifest().segments() {
            match snapshot.open_shared_segment(entry.id, &known, &self.layer.cache) {
                Ok(segment) => segments.push(segment),
                Err(error) if error.is_unusable_file() => {}
                Err(error) => return Err(DataError::store(error)),
            }
        }
        Ok(StoredRecords {
            _snapshot: Some(snapshot),
            segments,
        })
    }

    /// Waits up to [`WRITER_WAIT`] for the store's writer, and refuses it
    /// below the free-disk floor. The store itself refuses it, before
    /// anything is written, when a newer build wrote a file the newest
    /// generation names ([`DataError::NewerFormat`]).
    fn writer(&self) -> Result<aft_store::family::WriteSession<'_>, DataError> {
        let deadline = Instant::now() + WRITER_WAIT;
        loop {
            if let Some(mut session) = self.family.try_write().map_err(DataError::store)? {
                if let Err(error) = self.check_disk_floor() {
                    // Nothing new is written below the floor, but files no
                    // generation names any more are still removed, as the
                    // pause sentence promises.
                    let _ = session.reclaim();
                    return Err(error);
                }
                return Ok(session);
            }
            if Instant::now() >= deadline {
                return Err(DataError::Busy(format!(
                    "another process held the store's writer for over {} s",
                    WRITER_WAIT.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Refuses with the approved pause sentence when the store's disk has
    /// less free space than AFT's floor: 5% of the disk, at least 4 GiB and
    /// at most 32 GiB. A disk whose free space cannot be measured is refused
    /// too, as an error rather than a pause.
    fn check_disk_floor(&self) -> Result<(), DataError> {
        let dir = self.family.dir();
        let Some((free, capacity)) = crate::disk_floor::free_space(dir) else {
            return Err(DataError::Store(format!(
                "the free space on the disk holding {} could not be measured",
                dir.display()
            )));
        };
        let floor = crate::disk_floor::free_disk_floor(capacity);
        if free >= floor {
            return Ok(());
        }
        Err(DataError::DiskFloor(crate::view_storage_reserve::message(
            free, floor,
        )))
    }

    /// Scans the checkout's text only, then publishes and serves a state
    /// that names no call-graph fragments, so text queries can start while
    /// the call graph builds. Returns `false` when another process published
    /// this checkout first; the checkout then serves that publication.
    pub(crate) fn publish_text_first(&self) -> Result<bool, DataError> {
        self.publish_text(&mut BuildReport::default())
    }

    /// [`Self::publish_text_first`], filling in what it did in `report`.
    fn publish_text(&self, report: &mut BuildReport) -> Result<bool, DataError> {
        let covered = self
            .live
            .read()
            .expect("no panics while holding it")
            .sequence;
        let started_from = self
            .family
            .snapshot()
            .map_err(DataError::store)?
            .manifest()
            .checkout(self.checkout)
            .cloned();
        let started = Instant::now();
        let sink = BuildSink::text_only();
        let scanned = scan(&self.root, &self.layer.policy, None, &sink)
            .map_err(|error| DataError::Scan(error.to_string()))?;
        let mut collected = sink.finish();
        for record in scanned.state.paths() {
            if record.kind == PathKind::Text
                && record.stat.size > self.layer.policy.max_indexed_len
                && collected.seen_text.insert(record.content)
            {
                collected.text.add_unindexed(
                    record.content,
                    record.stat.size,
                    TextStatus::Oversize,
                );
            }
        }
        report.files = scanned.state.paths().len();
        report.text_units = collected.text.len();
        report.scan_ms = elapsed_ms(started);
        let mut session = self.writer()?;
        if session.base().checkout(self.checkout) != started_from.as_ref() {
            drop(session);
            self.adopt_newest()?;
            return Ok(false);
        }
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        report.shared = self.leave_out_shared(&snapshot, &mut collected.text, None)?;
        let mut pending = session.create_segment().map_err(DataError::store)?;
        collected
            .text
            .write(pending.writer())
            .map_err(DataError::store)?;
        let segment = session.finish_segment(pending).map_err(DataError::store)?;
        let written = session
            .write_checkout_state(self.checkout, &scanned.state)
            .map_err(DataError::store)?;
        // A checkout without a call graph is built this way every time, so
        // what it no longer reads is retired here as a full build retires
        // it.
        let unneeded = self.unneeded(&scanned.state, false)?;
        report.retired_segments = unneeded.segments.len();
        report.retired_checkouts = unneeded.checkouts.len();
        session
            .publish(&Publication {
                add_segments: vec![segment],
                remove_segments: unneeded.segments,
                states: vec![written],
                retire_checkouts: unneeded.checkouts,
            })
            .map_err(DataError::store)?;
        // As in a full build: the publication stands whether or not
        // reclamation runs, and the next one tries again.
        match session.reclaim() {
            Ok(reclaimed) => {
                report.reclaimed_files = reclaimed.removed_segments
                    + reclaimed.removed_states
                    + reclaimed.removed_manifests;
            }
            Err(error) => report.reclaim_error = Some(error.to_string()),
        }
        drop(session);
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        report.segment_bytes = snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| entry.len)
            .sum();
        self.serve_new(snapshot, covered)?;
        Ok(true)
    }

    /// Serves this checkout's publication in `snapshot`, which a build
    /// whose scan started at change `covered` just made. The scan read every
    /// file as it was once the scan started, so the build covers every
    /// change recorded before then. A change recorded while it ran may be
    /// newer than what it read, so it stays laid over the new publication.
    /// Returns the publication it serves.
    fn serve_new(&self, snapshot: Snapshot, covered: u64) -> Result<Arc<Published>, DataError> {
        self.touch_memory();
        let published = Arc::new(
            self.open_published(snapshot)?
                .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?,
        );
        let late = {
            let mut live = self.write_live();
            live.changes.retain(|_, change| change.sequence > covered);
            let kept = live.changes.keys().cloned().collect::<BTreeSet<_>>();
            // The changes the scan may have missed stay laid over the new
            // publication, and so does the state they describe.
            live.state = if kept.is_empty() {
                None
            } else {
                Some(Arc::new(with_changes(&published.state, &live.changes)))
            };
            let mut unparsed = published.unparsed.clone();
            for (path, change) in &live.changes {
                if change.failed() {
                    unparsed.insert(path.clone());
                } else {
                    unparsed.remove(path);
                }
            }
            live.unparsed = Arc::new(unparsed);
            live.published = Some(Arc::clone(&published));
            live.generation += 1;
            // The new publication's graph replaces the old one and whatever
            // was laid over it; the next refresh starts from it. So every
            // change kept laid over it is pending in the call graph again,
            // even one a refresh resolved while the build ran: that refresh
            // laid its answer over the old publication's graph.
            live.graph = None;
            live.refreshed = None;
            let mut text = (*live.text).clone();
            text.retain(|path| kept.contains(path));
            live.text = Arc::new(text);
            live.text_members = Arc::default();
            live.callgraph_pending = Arc::new(kept);
            let early = std::mem::take(&mut live.early);
            early
                .into_iter()
                .filter(|(_, sequence)| *sequence > covered)
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        };
        self.notify_readiness_change();
        // Paths reported before anything was published, after the scan
        // started: the scan may have read them before they changed.
        self.record_paths(&late)?;
        Ok(published)
    }

    /// Segments of the current generation that no other checkout's state
    /// reads (see [`Self::segments_read_by`]), nor `own`, the state this
    /// publication publishes, when `own_reads` says it reads from them.
    /// A build's new segment holds everything its state needs that other
    /// checkouts do not already read, so a build passes `false`; a
    /// publication of changes holds only what changed, so its state keeps
    /// reading the rest from older segments. A unit only the new segment
    /// holds counts as found nowhere, so it keeps any segment this build
    /// cannot read; a unit the new segment holds again keeps its older
    /// holder for one more publication. Vector segments follow their own
    /// rule ([`super::vectors::needed_vector_segments`]), over every other
    /// checkout's state and `own`, since no new segment holds vectors.
    fn unneeded(&self, own: &CheckoutState, own_reads: bool) -> Result<Unneeded, DataError> {
        self.unneeded_besides(self.checkout, own, own_reads, &[])
    }

    /// [`Self::unneeded`] for a publication of checkout `besides` with the
    /// state `own`, which may be another checkout of the family than this
    /// one (a fold on its behalf). The vector rule also keeps what the
    /// states of `bound` read: checkouts bound in this process, whose
    /// changes not yet published read vectors no published state names.
    fn unneeded_besides(
        &self,
        besides: CheckoutId,
        own: &CheckoutState,
        own_reads: bool,
        bound: &[CheckoutState],
    ) -> Result<Unneeded, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let (states, gone) = match self.states_besides(&snapshot, besides)? {
            OtherStates::Read { states, gone, .. } => (states, gone),
            // A state this build cannot read could need any segment.
            OtherStates::Unreadable { gone } => {
                return Ok(Unneeded {
                    segments: Vec::new(),
                    checkouts: gone,
                });
            }
        };
        let mut readers = states.iter().collect::<Vec<_>>();
        if own_reads {
            readers.push(own);
        }
        let read = self.segments_read_by(&snapshot, &readers)?;
        let mut needed = read.needed;
        if !own_reads {
            readers.push(own);
        }
        readers.extend(bound);
        let usable = read
            .usable
            .iter()
            .map(|&index| {
                read.opened[index]
                    .as_deref()
                    .expect("usable segments are open")
            })
            .collect::<Vec<_>>();
        for (&index, vectors) in read
            .usable
            .iter()
            .zip(super::vectors::needed_vector_segments(&usable, &readers)?)
        {
            needed[index] |= vectors;
        }
        Ok(Unneeded {
            segments: read
                .entries
                .iter()
                .zip(needed)
                .filter(|(_, needed)| !needed)
                .map(|(entry, _)| entry.id)
                .collect(),
            checkouts: gone,
        })
    }

    /// For each segment of `snapshot`, in manifest order, whether a member
    /// of `states` reads a text unit, content record or call-graph fragment
    /// from it. For each one a member needs, readers take the newest
    /// segment that holds it, so only that segment is marked. A member that
    /// needs something no usable segment holds may find it in one this
    /// build cannot read (an older format version, or damaged), so then
    /// every such segment is marked too. A segment a newer build wrote fails
    /// the call with its refusal instead: none is ever left unmarked.
    fn segments_read_by(
        &self,
        snapshot: &Snapshot,
        states: &[&CheckoutState],
    ) -> Result<SegmentsRead, DataError> {
        self.segments_read_noting(snapshot, states, |_, _, _| {})
    }

    /// [`Self::segments_read_by`], also calling `note` with each unit a
    /// member of `states` reads: the member's place in `states`, the unit,
    /// and the place in the manifest of the segment it reads it from.
    fn segments_read_noting(
        &self,
        snapshot: &Snapshot,
        states: &[&CheckoutState],
        mut note: impl FnMut(usize, ReadUnit, usize),
    ) -> Result<SegmentsRead, DataError> {
        let known = known_sections();
        let entries = snapshot.manifest().segments().to_vec();
        // Each segment this build can use, by its place in the manifest;
        // `None` for one it cannot.
        let mut opened = Vec::with_capacity(entries.len());
        for entry in &entries {
            match snapshot.open_shared_segment(entry.id, &known, &self.layer.cache) {
                Ok(segment) => opened.push(Some(segment)),
                Err(error) if error.is_unusable_file() => opened.push(None),
                Err(error) => return Err(DataError::store(error)),
            }
        }
        let usable = (0..opened.len())
            .filter(|&index| opened[index].is_some())
            .collect::<Vec<_>>();
        let mut needed = vec![false; entries.len()];
        {
            let segment = |index: usize| opened[index].as_ref().expect("usable segments are open");
            // TextLocator numbers only the segments that have a text
            // section, so keep each one's place in the manifest beside it.
            let mut text_index = Vec::new();
            let mut texts = Vec::new();
            for &index in &usable {
                if let Some(text) = TextSegment::open(segment(index)).map_err(DataError::store)? {
                    text_index.push(index);
                    texts.push(text);
                }
            }
            let contents = usable
                .iter()
                .map(|&index| ContentSegment::open(segment(index)).map_err(DataError::store))
                .collect::<Result<Vec<_>, _>>()?;
            let graphs = usable
                .iter()
                .map(|&index| GraphSegment::open(segment(index)).map_err(DataError::store))
                .collect::<Result<Vec<_>, _>>()?;
            let locator = TextLocator::build(&texts).map_err(DataError::store)?;
            let mut needs_unusable = false;
            for (member, state) in states.iter().enumerate() {
                for (position, record) in state.paths().iter().enumerate() {
                    match locator.get(record.content) {
                        Some(Located::Indexed { segment, .. }) => {
                            let holder = text_index[segment as usize];
                            needed[holder] = true;
                            note(member, ReadUnit::Text(record.content), holder);
                        }
                        Some(_) => {}
                        None if record.kind == PathKind::Text
                            && record.stat.size <= self.layer.policy.max_indexed_len =>
                        {
                            needs_unusable = true;
                        }
                        None => {}
                    }
                    if !record.language.is_empty() {
                        let key =
                            RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
                        match newest_holder(&contents, |content| content.find(key))? {
                            Some(holder) => {
                                needed[usable[holder]] = true;
                                note(member, ReadUnit::Record(key), usable[holder]);
                            }
                            None => needs_unusable = true,
                        }
                    }
                    if let Some(fragment) = state.fragment(position) {
                        let key = FragmentKey::from_digest(fragment);
                        match newest_holder(&graphs, |graph| graph.find(key))? {
                            Some(holder) => {
                                needed[usable[holder]] = true;
                                note(member, ReadUnit::Fragment(key), usable[holder]);
                            }
                            None => needs_unusable = true,
                        }
                    }
                }
            }
            if needs_unusable {
                for (index, segment) in opened.iter().enumerate() {
                    if segment.is_none() {
                        needed[index] = true;
                    }
                }
            }
        }
        Ok(SegmentsRead {
            entries,
            opened,
            usable,
            needed,
        })
    }

    /// The states of the family's other checkouts in `snapshot`, apart from
    /// those whose folder is gone, which are listed by id.
    fn other_states(&self, snapshot: &Snapshot) -> Result<OtherStates, DataError> {
        self.states_besides(snapshot, self.checkout)
    }

    /// The current state of every checkout of this family bound through
    /// this data layer, this one included: its publication's state with
    /// the changes recorded since, which its views read whether published
    /// or not. Taken before the store's writer, never under it.
    pub(crate) fn bound_states(&self) -> Vec<CheckoutState> {
        let lives = self
            .layer
            .bound
            .lock()
            .expect("no panics while holding it")
            .iter()
            .filter(|(dir, _)| dir == self.family.dir())
            .filter_map(|(_, live)| live.upgrade())
            .collect::<Vec<_>>();
        lives
            .iter()
            .filter_map(|live| {
                let live = live.read().expect("no panics while holding it");
                live.state.as_deref().cloned().or_else(|| {
                    live.published
                        .as_ref()
                        .map(|published| published.state.clone())
                })
            })
            .collect()
    }

    /// [`Self::other_states`]: every checkout's state in `snapshot` but
    /// that of `besides`.
    fn states_besides(
        &self,
        snapshot: &Snapshot,
        besides: CheckoutId,
    ) -> Result<OtherStates, DataError> {
        let mut states = Vec::new();
        let mut ids = Vec::new();
        let mut gone = Vec::new();
        for state_ref in snapshot.manifest().checkouts() {
            if state_ref.checkout == besides {
                continue;
            }
            match snapshot.checkout_state(state_ref.checkout) {
                Ok(Some(state)) if state.root().is_some_and(folder_is_gone) => {
                    gone.push(state_ref.checkout);
                }
                Ok(Some(state)) => {
                    states.push(state);
                    ids.push(state_ref.checkout);
                }
                Ok(None) => {}
                // A state this build cannot read: an older format version,
                // or damaged. One a newer build wrote is not among them: it
                // fails the build with its refusal, so nothing is written,
                // retired or reclaimed over it.
                Err(error) if error.is_unusable_file() => {
                    return Ok(OtherStates::Unreadable { gone });
                }
                Err(error) => return Err(DataError::store(error)),
            }
        }
        Ok(OtherStates::Read { states, ids, gone })
    }

    /// The units the family's other checkouts read, each with the segment
    /// they read it from; `None` when another checkout's state cannot be
    /// read, since that state could read anything.
    fn read_by_others(&self, snapshot: &Snapshot) -> Result<Option<ReadByOthers>, DataError> {
        self.read_by_others_than(snapshot, self.checkout)
    }

    /// [`Self::read_by_others`] for the checkouts other than `besides`.
    fn read_by_others_than(
        &self,
        snapshot: &Snapshot,
        besides: CheckoutId,
    ) -> Result<Option<ReadByOthers>, DataError> {
        let OtherStates::Read { states, .. } = self.states_besides(snapshot, besides)? else {
            return Ok(None);
        };
        let mut read = ReadByOthers::default();
        if states.is_empty() {
            return Ok(Some(read));
        }
        let others = states.iter().collect::<Vec<_>>();
        self.segments_read_noting(snapshot, &others, |_, unit, holder| match unit {
            ReadUnit::Text(key) => {
                read.text.insert(key, holder);
            }
            ReadUnit::Record(key) => {
                read.records.insert(key, holder);
            }
            ReadUnit::Fragment(key) => {
                read.fragments.insert(key, holder);
            }
        })?;
        Ok(Some(read))
    }

    /// Leaves out of a build's new segment the text units, content records
    /// and fragments another checkout already reads, when it reads them
    /// from one of the [`SHARE_FROM`] segments that serve the most of them.
    /// Worktrees of one repository hold mostly the same contents, which
    /// each first build used to store again (about 155 MB for each
    /// worktree of one large repository); nearly all of them sit in a
    /// sibling's build
    /// segment, which is among those kept.
    ///
    /// Everything else is written again. Other checkouts' small change
    /// segments are folded in that way: those checkouts then read the
    /// units from this build's segment, the newest that holds them, and a
    /// change segment nobody reads any more leaves the store. Sharing from
    /// every segment instead left two checkouts reading one change segment
    /// each waiting on the other to fold it, which neither ever did. A unit
    /// a sibling's segment holds but no other checkout reads is written
    /// again too, so a build never keeps a segment alive that only it reads.
    /// Leaves nothing out when another checkout's state cannot be read.
    fn leave_out_shared(
        &self,
        snapshot: &Snapshot,
        text: &mut TextSectionsBuilder,
        mut sections: Option<&mut super::graph_store::SectionsWriter<'_>>,
    ) -> Result<SharedUnits, DataError> {
        let Some(read) = self.read_by_others(snapshot)? else {
            return Ok(SharedUnits::default());
        };
        // The units this build writes, which `shared_holders` counts by the
        // segment the others read each from.
        let mut units = Vec::with_capacity(text.len());
        text.retain(|key| {
            units.push(ReadUnit::Text(key));
            true
        });
        let mut fragments = Vec::new();
        if let Some(sections) = sections.as_deref_mut() {
            sections.leave_out(
                |key| {
                    units.push(ReadUnit::Record(key));
                    false
                },
                |key| {
                    fragments.push(ReadUnit::Fragment(key));
                    false
                },
            );
        }
        let kept = fold::shared_holders(&read, units.into_iter().chain(fragments));
        Ok(read.leave_out(Some(text), sections, |holder| kept.contains(&holder)))
    }

    /// Resolves the call graph again for the paths changed since the
    /// publication, and lays the files whose resolution changed over the
    /// published graph. Returns `None` when nothing needed it.
    ///
    /// # Errors
    ///
    /// When a stored record or fragment cannot be read, or resolution
    /// fails. The pending paths stay pending then.
    pub fn refresh_callgraph(&self) -> Result<Option<UpdateStats>, DataError> {
        let _alloc_label = crate::alloc_trace::scope(
            crate::alloc_trace::Label::CALLGRAPH_REFRESH,
            Some(&self.root),
        );
        checkpoint()?;
        let result = self.refresh_inner();
        self.touch_memory();
        self.note_outcome("refresh", &result);
        result
    }

    /// See the `memory_epoch` field: equal values mean nothing the checkout
    /// holds in memory has changed between two reads.
    #[must_use]
    pub fn memory_epoch(&self) -> u64 {
        self.memory_epoch.load(std::sync::atomic::Ordering::Acquire)
    }

    fn touch_memory(&self) {
        self.memory_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn refresh_inner(&self) -> Result<Option<UpdateStats>, DataError> {
        let (published, state, changes, covered) = {
            let live = self.live.read().expect("no panics while holding it");
            let Some(published) = live.published.clone() else {
                return Ok(None);
            };
            let state = live
                .state
                .clone()
                .unwrap_or_else(|| Arc::new(published.state.clone()));
            (published, state, live.changes.clone(), live.sequence)
        };
        // Without a call graph there is nothing to update; the build that
        // makes one covers every change recorded before its scan.
        if changes.is_empty() || !published.graph_built || !self.planes.callgraph {
            return Ok(None);
        }
        // An update that stopped part way through dropped its engine before
        // anything else could read the mutex, so a poisoned lock only means
        // the engine must start again.
        let mut slot = self.engine.lock().unwrap_or_else(|poisoned| {
            self.engine.clear_poison();
            let mut slot = poisoned.into_inner();
            *slot = None;
            slot
        });
        // A damaged stored record or fragment can stop the update where it
        // expects one (every file the engine lists has a parse and a
        // resolution). Such a stop fails this refresh as damage, and the
        // engine, whose state it may have left half-updated, starts again.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.update_engine(&mut slot, &published, &state, &changes)
        }));
        let (stats, served) = match outcome {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                *slot = None;
                return Err(error);
            }
            Err(panic) => {
                *slot = None;
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "no message".to_string());
                let failures = published.graph.failures();
                return Err(DataError::Damaged(if failures.is_empty() {
                    format!("the call-graph update stopped: {message}")
                } else {
                    failures.join("; ")
                }));
            }
        };
        // An engine a refresh changed is kept for the next one, and counts
        // against the budget every checkout's engine shares.
        let kept = stats.is_some().then(|| {
            self.layer.engines.touch(&self.engine);
            slot.as_mut()
                .filter(|_| self.layer.engines.measure_due(&self.engine))
                .map(|engine| {
                    engine.measure_after_publication = true;
                    engine.census_bytes()
                })
        });
        drop(slot);
        if let Some(bytes) = kept {
            self.layer.engines.kept(&self.engine, bytes);
        }
        let mut live = self.write_live();
        if live
            .published
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &published))
        {
            if let Some((overlay, refreshed)) = served {
                let graph: Arc<dyn SliceSource + Send + Sync> = Arc::new(overlay);
                live.graph = Some(graph);
                live.refreshed = Some(Arc::new(refreshed));
            }
            let resolved = live
                .changes
                .iter()
                .filter(|(_, change)| change.sequence <= covered)
                .map(|(path, _)| path.clone())
                .collect::<BTreeSet<_>>();
            // The refresh extracted these changes, so each one whose
            // extraction failed now counts as unparsed. The set is copied
            // only when that changes it and a view still holds it.
            let live = &mut *live;
            for path in &resolved {
                let failed = live.changes.get(path).is_some_and(Change::failed);
                if failed != live.unparsed.contains(path) {
                    let unparsed = Arc::make_mut(&mut live.unparsed);
                    if failed {
                        unparsed.insert(path.clone());
                    } else {
                        unparsed.remove(path);
                    }
                }
            }
            live.callgraph_pending = Arc::new(
                live.callgraph_pending
                    .iter()
                    .filter(|path| !resolved.contains(*path))
                    .cloned()
                    .collect(),
            );
        }
        Ok(stats)
    }

    /// Applies `changes` to the engine in `slot` (started from `published`
    /// when it is missing or was started from an older one), returning the
    /// update's statistics and, when it applied anything, the graph and
    /// resolutions to serve.
    fn update_engine(
        &self,
        slot: &mut Option<GraphEngine>,
        published: &Arc<Published>,
        state: &Arc<CheckoutState>,
        changes: &BTreeMap<Vec<u8>, Change>,
    ) -> Result<
        (
            Option<UpdateStats>,
            Option<(OverlaySlices<Arc<StoredSlices>>, Refreshed)>,
        ),
        DataError,
    > {
        if slot
            .as_ref()
            .is_some_and(|engine| !Arc::ptr_eq(&engine.published, &published))
        {
            *slot = None;
        }
        if slot.is_none() {
            *slot = Some(self.start_engine(published)?);
            self.engine.note_start();
        }
        let engine = slot.as_mut().expect("started");
        let changed = changes
            .iter()
            .filter(|(path, change)| engine.applied.get(*path) != Some(&change.sequence))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut stats = None;
        if !changed.is_empty() {
            note_source_work(&self.root, |work| work.resolutions += 1);
            // The changed files are extracted here, all at once on the
            // engine's threads, rather than each when its edit was recorded.
            let extracting = changed
                .iter()
                .filter_map(|path| changes.get(path))
                .collect::<Vec<_>>();
            engine.live.install(|| {
                use rayon::prelude::*;
                let carry = crate::alloc_trace::carry();
                extracting.par_iter().for_each(|change| {
                    let _label = carry.enter();
                    change.content.get();
                });
            });
            let next = next_inputs(state, &engine.live.inputs, changes, &changed);
            let changed_names = changed
                .iter()
                .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_string))
                .collect::<BTreeSet<_>>();
            let update = engine.live.update(next, &changed_names);
            // The engine reads stored parses and resolutions as it goes; a
            // read that failed made it work from an empty one.
            let failures = published.graph.failures();
            if !failures.is_empty() {
                return Err(DataError::Damaged(format!(
                    "reading the stored call graph failed: {}",
                    failures.join("; ")
                )));
            }
            // The caller drops a partly updated engine after either error,
            // so the next refresh starts again from the publication.
            let update = update.map_err(|error| DataError::Resolve(error.to_string()))?;
            // Only the files this update touched get new slices; every file
            // touched by an earlier update keeps the slice it got then. Each
            // is laid over the publication encoded, as a publication stores
            // it, and the refreshed files keep each resolution encoded, so
            // what an edit resolved again costs its encoded size until the
            // next publication. The slices and encodings are made on the
            // engine's threads.
            let cached_as = CacheKey::new();
            let files = update.touched.iter().collect::<Vec<_>>();
            let (resolved, parses) = (&engine.live.resolved, &engine.live.inputs.parses);
            let graph = &published.graph;
            let made = engine.live.install(|| {
                use rayon::prelude::*;
                let carry = crate::alloc_trace::carry();
                files
                    .par_iter()
                    .map(|file| {
                        let _label = carry.enter();
                        let resolution = resolved.get(file)?;
                        let (key, parse) = parses.get(file)?;
                        let slice = FileSlice::build(file, key, parse, resolution);
                        let refreshed = RefreshedFile {
                            resolution: resolved.encoded(file, key)?,
                            parse: (graph.record_key_of(file) != Some(key.as_str()))
                                .then(|| Arc::clone(parse)),
                        };
                        Some((PreparedSlice::of(&slice, key, &cached_as), refreshed))
                    })
                    .collect::<Vec<_>>()
            });
            for (file, made) in files.into_iter().zip(made) {
                match made {
                    Some((slice, refreshed)) => {
                        engine.overlay.replace_encoded(file, slice);
                        engine.refreshed.removed.remove(file);
                        engine.refreshed.replaced.insert(file.clone(), refreshed);
                    }
                    None => {
                        engine.overlay.remove(file);
                        engine.refreshed.replaced.remove(file);
                        engine.refreshed.removed.insert(file.clone());
                    }
                }
            }
            engine.refreshed.state = engine.live.inputs.state.clone();
            for path in &changed {
                engine.applied.insert(path.clone(), changes[path].sequence);
                let Ok(name) = std::str::from_utf8(path) else {
                    continue;
                };
                match changes[path].content.get() {
                    Extracted::Parse(_, _, record) => {
                        engine
                            .refreshed
                            .records
                            .insert(name.to_string(), Arc::clone(record));
                    }
                    Extracted::Config(..) | Extracted::Markers(_) | Extracted::Nothing => {
                        engine.refreshed.records.remove(name);
                    }
                }
            }
            stats = Some(update);
            note_source_work(&self.root, |work| {
                let live = &engine.live;
                work.decoded_parses = work.decoded_parses.max(live.inputs.parses.decoded_count());
                work.decoded_resolutions =
                    work.decoded_resolutions.max(live.resolved.decoded_count());
            });
            // An update keeps every parse and resolution it decoded. A broad
            // one reads thousands (every file declaring a method named `new`,
            // say), and resolves a thousand again after a widely read change,
            // so past a budget every one that can be decoded again goes: from
            // the publication, or for a resolution an update made, from the
            // encoding it got above. A later update decodes again what it
            // reads. A changed file's parse stays, since only the changes
            // hold it until the next publication.
            if engine.live.inputs.parses.decoded_count() > ENGINE_DECODED_FILES {
                engine.live.inputs.parses.release_decoded();
            }
            if engine.live.resolved.decoded_count() > ENGINE_DECODED_FILES {
                engine.live.resolved.release_decoded();
            }
        }
        // A refresh that applied nothing keeps the served graph, and with it
        // any dead-code snapshot already built for it.
        let served = stats
            .is_some()
            .then(|| (engine.overlay.clone(), engine.refreshed.clone()));
        Ok((stats, served))
    }

    /// Publishes what changed since the publication in a segment of its
    /// own: the changed contents' text and records, and the fragments of
    /// the files whose resolution or stored terms changed, with the
    /// checkout's new state naming every path's fragment. Queries then
    /// answer from that publication with nothing laid over it, and a
    /// restart starts from it. `None` when there is nothing to publish, the
    /// call graph has not resolved every change yet (refresh first), a
    /// build is running, or another process published this checkout
    /// meanwhile; the checkout then serves that publication.
    ///
    /// # Errors
    ///
    /// When stored call-graph data is damaged or the store cannot be written;
    /// nothing is published then.
    pub fn publish_changes(&self) -> Result<Option<DeltaReport>, DataError> {
        self.publish_changes_as(false)
    }

    /// [`Self::publish_changes`]; `by_build` when the build running now
    /// publishes them itself, having started from another checkout's state
    /// ([`Self::start_from_sibling`]).
    fn publish_changes_as(&self, by_build: bool) -> Result<Option<DeltaReport>, DataError> {
        let started = Instant::now();
        let (published, changes, covered, text) = {
            let live = self.live.read().expect("no panics while holding it");
            let Some(published) = live.published.clone() else {
                return Ok(None);
            };
            if (live.building && !by_build) || live.changes.is_empty() || !published.graph_built {
                return Ok(None);
            }
            (
                published,
                live.changes.clone(),
                live.sequence,
                Arc::clone(&live.text),
            )
        };
        // What the engine resolved, for exactly these changes.
        let (inputs, resolved) = {
            let Ok(slot) = self.engine.lock() else {
                return Ok(None);
            };
            let Some(engine) = slot
                .as_ref()
                .filter(|engine| Arc::ptr_eq(&engine.published, &published))
            else {
                return Ok(None);
            };
            if changes
                .iter()
                .any(|(path, change)| engine.applied.get(path) != Some(&change.sequence))
            {
                return Ok(None);
            }
            let mut files = engine
                .refreshed
                .replaced
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>();
            files.extend(engine.live.refiled());
            // Each file's resolution encoded, decoded again one at a time as
            // its fragment is written, so publishing never holds every
            // re-resolved file decoded at once.
            let parses = &engine.live.inputs.parses;
            let resolved = files
                .iter()
                .filter_map(|file| {
                    let key = parses.key(file)?;
                    engine
                        .live
                        .resolved
                        .encoded(file, &key)
                        .map(|resolution| (file.clone(), resolution))
                })
                .collect::<BTreeMap<_, _>>();
            (engine.live.inputs.clone(), resolved)
        };
        // The changed contents' records, already encoded, and their text,
        // each content once.
        let mut records = ContentSectionsBuilder::new();
        let mut text_units = TextSectionsBuilder::new();
        let mut seen = HashSet::new();
        for (path, change) in &changes {
            let Some(record) = &change.record else {
                continue;
            };
            let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
            match change.content.get() {
                Extracted::Parse(_, _, stored)
                | Extracted::Config(_, stored)
                | Extracted::Markers(stored) => {
                    records.add_encoded(key, Arc::clone(stored));
                }
                Extracted::Nothing => {}
            }
            if !seen.insert(record.content) {
                continue;
            }
            let size = record.stat.size;
            match text.get(path) {
                Some(OverlayEntry::Content(trigrams)) => {
                    text_units.add_indexed(record.content, size, trigrams);
                }
                Some(OverlayEntry::Unindexed) => match record.kind {
                    PathKind::Binary => {
                        text_units.add_unindexed(record.content, size, TextStatus::Binary);
                    }
                    PathKind::Large => {
                        text_units.add_unindexed(record.content, size, TextStatus::Oversize);
                    }
                    PathKind::Text if size > self.layer.policy.max_indexed_len => {
                        text_units.add_unindexed(record.content, size, TextStatus::Oversize);
                    }
                    _ => {}
                },
                Some(OverlayEntry::Deleted) | None => {}
            }
        }
        let text_unit_count = text_units.len();
        let record_count = records.len();
        // A path keeps the fragment the publication stored for it unless it
        // got a new one, or its content or language changed.
        let stored_fragments = published
            .state
            .paths()
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                published
                    .state
                    .fragment(index)
                    .map(|fragment| (record.path.as_slice(), (record, fragment)))
            })
            .collect::<HashMap<_, _>>();

        let mut session = self.writer()?;
        let started_from = published
            ._snapshot
            .manifest()
            .checkout(self.checkout)
            .cloned();
        if session.base().checkout(self.checkout) != started_from.as_ref() {
            // Another process published this checkout since; serve its
            // publication, as a build that stands down does.
            drop(session);
            self.adopt_newest()?;
            return Ok(None);
        }
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        // What another checkout already reads stays where it reads it from,
        // as a build leaves it. Written again here, it would be newest in
        // this segment, and that checkout would read it from here; once
        // this checkout edited it away, the other one would keep this
        // segment in the store alone, without ever counting it as its own.
        let read_by_others = self.read_by_others(&snapshot)?;
        let mut shared = read_by_others
            .as_ref()
            .map_or_else(SharedUnits::default, |read| {
                read.leave_out(Some(&mut text_units), None, |_| true)
            });
        let mut pending = session.create_segment().map_err(DataError::store)?;
        text_units
            .write(pending.writer())
            .map_err(DataError::store)?;
        let fragments = {
            // As `write_sections` writes them, from each file's resolution
            // decoded in turn. The content sections hold exactly the changed
            // records: every changed parsed file is resolved again, so its
            // fragment is written here, and every changed configuration
            // file is one of the state's.
            let no_records = HashMap::new();
            let mut sections =
                SectionsWriter::new(&inputs.state, &no_records, false, Some(records));
            for (file, resolution) in &resolved {
                sections
                    .add(&inputs, file, &resolution.decode(file))
                    .map_err(DataError::Damaged)?;
            }
            // Recorded input terms can read other files' stored parses too.
            // As in pass 4, an empty fallback after a failed read is damage,
            // not a graph that may be committed as the next publication.
            let failures = published.graph.failures();
            if !failures.is_empty() {
                return Err(DataError::Damaged(format!(
                    "reading the stored call graph failed: {}",
                    failures.join("; ")
                )));
            }
            if let Some(read) = &read_by_others {
                let left = read.leave_out(None, Some(&mut sections), |_| true);
                shared.records = left.records;
                shared.fragments = left.fragments;
            }
            sections
                .finish(pending.writer(), None)
                .map_err(DataError::Store)?
        };
        let segment = session.finish_segment(pending).map_err(DataError::store)?;
        let per_path = inputs
            .state
            .paths()
            .iter()
            .map(|record| {
                let path = std::str::from_utf8(&record.path).ok();
                if let Some(key) = path.and_then(|path| fragments.get(path)) {
                    return Some(key.digest());
                }
                stored_fragments
                    .get(record.path.as_slice())
                    .filter(|(stored, _)| {
                        stored.content == record.content && stored.language == record.language
                    })
                    .map(|(_, fragment)| *fragment)
            })
            .collect();
        let state = inputs
            .state
            .clone()
            .with_fragments_for(per_path, EXTRACTOR_VERSION)
            .map_err(DataError::store)?;
        let written = session
            .write_checkout_state(self.checkout, &state)
            .map_err(DataError::store)?;
        // The new segment holds only what changed, so a segment whose every
        // unit its readers now take from a newer one (an earlier publication
        // of the same files, say) is read by no checkout. Builds are the
        // only other place such a segment leaves, and a checkout that keeps
        // editing the same files reads few segments and so is seldom built
        // again: without this, every publication of its changes stayed in
        // the store, one more open file each.
        let unneeded = self.unneeded(&state, true)?;
        let retired_segments = unneeded.segments.len();
        let retired_checkouts = unneeded.checkouts.len();
        session
            .publish(&Publication {
                add_segments: vec![segment],
                remove_segments: unneeded.segments,
                states: vec![written],
                retire_checkouts: unneeded.checkouts,
            })
            .map_err(DataError::store)?;
        // The state this one replaces, and older manifests, go once no
        // reader holds them.
        let _ = session.reclaim();
        drop(session);
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let read = self.segments_read_by(&snapshot, &[&state])?;
        // A segment another checkout reads too holds what this checkout
        // shares with it; a build of this checkout leaves those units where
        // they are, so it is not counted. When another checkout's state
        // cannot be read, every segment this checkout reads is.
        let others_read = match self.other_states(&snapshot)? {
            OtherStates::Read { states, .. } if !states.is_empty() => {
                let others = states.iter().collect::<Vec<_>>();
                self.segments_read_by(&snapshot, &others)?.needed
            }
            _ => vec![false; read.needed.len()],
        };
        let report = DeltaReport {
            paths: changes.len(),
            fragments: fragments.len(),
            records: record_count,
            text_units: text_unit_count,
            segments: read
                .needed
                .iter()
                .zip(&others_read)
                .filter(|(own, other)| **own && !**other)
                .count(),
            read_segments: read.needed.iter().filter(|own| **own).count(),
            shared,
            retired_segments,
            retired_checkouts,
            ms: 0.0,
        };
        let served = self.serve_new(snapshot, covered)?;
        // The engine resolved what this publication holds: it goes on from
        // there.
        let applied = changes
            .iter()
            .map(|(path, change)| (path.clone(), change.sequence))
            .collect();
        self.carry_engine(&published, &served, &applied);
        Ok(Some(DeltaReport {
            ms: elapsed_ms(started),
            ..report
        }))
    }

    /// Moves the call-graph engine started from `from` onto `to`, the
    /// publication now served in its place, when the engine resolved exactly
    /// what `to` holds: it applied exactly the changes `applied` names (each
    /// changed path with its change count), and `to` holds the paths,
    /// parses and configuration it resolves over (see
    /// [`GraphEngine::rebased`]). The next refresh then goes on from what
    /// the engine bound and computed so far, which a new engine works out
    /// again on its first refresh. An engine started from `from` that
    /// cannot be moved is dropped, as the next refresh would start a new one
    /// anyway; one started from another publication is left as it is.
    fn carry_engine(
        &self,
        from: &Arc<Published>,
        to: &Arc<Published>,
        applied: &BTreeMap<Vec<u8>, u64>,
    ) {
        let Ok(mut slot) = self.engine.lock() else {
            return;
        };
        if !slot
            .as_ref()
            .is_some_and(|engine| Arc::ptr_eq(&engine.published, from))
        {
            return;
        }
        let Some(engine) = slot.take() else {
            return;
        };
        self.touch_memory();
        let started = Instant::now();
        match engine.rebased(to, applied) {
            Ok(mut engine) => {
                let bytes = (engine.measure_after_publication
                    || self.layer.engines.measure_due(&self.engine))
                .then(|| engine.census_bytes());
                engine.measure_after_publication = false;
                *slot = Some(engine);
                drop(slot);
                self.layer.engines.kept(&self.engine, bytes);
                crate::slog_info!(
                    "perf data_layer engine_rebased: root={} {:.0}ms",
                    self.root.display(),
                    elapsed_ms(started)
                );
            }
            Err(why) => crate::slog_info!(
                "data layer: the call-graph engine of {} starts again on its next refresh: {why}",
                self.root.display()
            ),
        }
    }

    /// The bytes the call-graph engine holds, as the budget every
    /// checkout's engine shares counts them; `None` without an engine or
    /// while a refresh holds it. For measurement.
    #[doc(hidden)]
    #[must_use]
    pub fn engine_census_bytes(&self) -> Option<u64> {
        let slot = self.engine.try_lock().ok()?;
        slot.as_ref().map(GraphEngine::census_bytes)
    }

    /// The engine budget's cached size estimate, for comparing it with a fresh census.
    #[doc(hidden)]
    #[must_use]
    pub fn engine_budget_bytes(&self) -> u64 {
        self.engine.budget_bytes()
    }

    /// How many call-graph engines refreshes of this checkout have started:
    /// one for the first refresh after a bind, and one more after each time
    /// the engine went. For tests and measurement.
    #[doc(hidden)]
    #[must_use]
    pub fn engine_starts(&self) -> u64 {
        self.engine.starts()
    }

    /// How many segments the newest generation of the family store names,
    /// for every checkout of the family. For gates and tests.
    ///
    /// # Errors
    ///
    /// When the store's newest generation cannot be read.
    #[doc(hidden)]
    pub fn store_segments(&self) -> Result<usize, DataError> {
        Ok(self
            .family
            .snapshot()
            .map_err(DataError::store)?
            .manifest()
            .segments()
            .len())
    }

    /// How many paths changed since the publication.
    #[must_use]
    pub fn changed_paths(&self) -> usize {
        self.live
            .read()
            .expect("no panics while holding it")
            .changes
            .len()
    }

    /// How many files the call graph resolved again, or dropped, since the
    /// publication: what the engine holds until the changes are published.
    #[must_use]
    pub fn refreshed_files(&self) -> usize {
        self.live
            .read()
            .expect("no panics while holding it")
            .refreshed
            .as_ref()
            .map_or(0, |refreshed| {
                refreshed.replaced.len() + refreshed.removed.len()
            })
    }

    /// The incremental state for `published`: every stored resolution, and
    /// the parses and configuration the publication was built from.
    fn start_engine(&self, published: &Arc<Published>) -> Result<GraphEngine, DataError> {
        let _alloc_label =
            crate::alloc_trace::scope(crate::alloc_trace::Label::CALLGRAPH_START, Some(&self.root));
        let mut timings = Vec::new();
        let engine = self.start_engine_timed(published, &mut timings)?;
        crate::slog_info!(
            "perf data_layer engine_start: root={} {}",
            self.root.display(),
            timings
                .iter()
                .map(|(step, ms)| format!("{step}={ms:.0}ms"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(engine)
    }

    /// Starts the incremental engine for this checkout's publication and
    /// returns each step's time, without keeping the engine. For
    /// measurement only.
    ///
    /// # Errors
    ///
    /// When there is no publication, or a step fails.
    #[doc(hidden)]
    pub fn measure_engine_start(&self) -> Result<Vec<(&'static str, f64)>, DataError> {
        let published = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .clone()
            .ok_or_else(|| DataError::Resolve("nothing is published".to_string()))?;
        let mut timings = Vec::new();
        let started = Instant::now();
        let engine = self.start_engine_timed(&published, &mut timings)?;
        timings.push(("total", started.elapsed().as_secs_f64() * 1000.0));
        drop(engine);
        Ok(timings)
    }

    /// Starts the incremental engine for this checkout's publication and
    /// checks that the publication's stored input terms file every reader
    /// the engine indexes. Returns what they miss. For gates.
    ///
    /// # Errors
    ///
    /// When there is no publication, or the engine cannot start.
    #[doc(hidden)]
    pub fn check_stored_inputs(&self) -> Result<Vec<String>, DataError> {
        let published = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .clone()
            .ok_or_else(|| DataError::Resolve("nothing is published".to_string()))?;
        let engine = self.start_engine_timed(&published, &mut Vec::new())?;
        Ok(engine.live.stored_readers_missing(&published.graph))
    }

    /// Runs `inspect` on the incremental engine this checkout's call graph is
    /// served from: the one the last refresh updated, or, when no refresh has
    /// started one for the current publication, one started from that
    /// publication the way the next refresh starts it, dropped afterwards.
    /// `None` when no call graph is published. For gates.
    ///
    /// # Errors
    ///
    /// When an engine cannot be started from the publication.
    #[doc(hidden)]
    pub(crate) fn inspect_engine<T>(
        &self,
        inspect: impl FnOnce(&LiveCheckout) -> T,
    ) -> Result<Option<T>, DataError> {
        let published = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .clone();
        let Some(published) = published.filter(|published| published.graph_built) else {
            return Ok(None);
        };
        {
            let slot = self
                .engine
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(engine) = slot
                .as_ref()
                .filter(|engine| Arc::ptr_eq(&engine.published, &published))
            {
                return Ok(Some(inspect(&engine.live)));
            }
        }
        let engine = self.start_engine_timed(&published, &mut Vec::new())?;
        Ok(Some(inspect(&engine.live)))
    }

    /// The references of `file` whose text contains `needle`, each with the
    /// rows its resolution stored, and the callers `callers` reports for
    /// each `(target file, symbol)` in `targets`. For diagnosing a missed
    /// edge on a real checkout.
    ///
    /// # Errors
    ///
    /// When there is no publication, or the engine cannot start.
    #[doc(hidden)]
    pub fn probe_references(
        &self,
        file: &str,
        needle: &str,
        targets: &[(String, String)],
    ) -> Result<Vec<String>, DataError> {
        let published = self
            .live
            .read()
            .expect("no panics while holding it")
            .published
            .clone()
            .ok_or_else(|| DataError::Resolve("nothing is published".to_string()))?;
        let engine = self.start_engine_timed(&published, &mut Vec::new())?;
        let live = &engine.live;
        let mut lines = Vec::new();
        if let (Some((_, parse)), Some(resolution)) =
            (live.inputs.parses.get(file), live.resolved.get(file))
        {
            for reference in &parse.refs {
                let text = format!("{reference:?}");
                if !text.contains(needle) {
                    continue;
                }
                lines.push(format!(
                    "ref {}",
                    text.chars().take(400).collect::<String>()
                ));
                for row in resolution
                    .rows
                    .iter()
                    .filter(|row| row.ref_ordinal == reference.ordinal)
                {
                    lines.push(format!("  row {row:?}"));
                }
            }
        } else {
            lines.push(format!("{file}: no parse or resolution"));
        }
        let view = self
            .view()
            .ok_or_else(|| DataError::Resolve("nothing is published".to_string()))?;
        for (target, symbol) in targets {
            let callers = crate::commands::callgraph_store_adapter::callers_result(
                &view.callgraph(),
                &view.root().join(target),
                symbol,
                1,
                true,
            );
            let text = format!("{callers:?}");
            lines.push(format!(
                "callers of {target}::{symbol}: {}",
                text.chars().take(600).collect::<String>()
            ));
        }
        Ok(lines)
    }

    fn start_engine_timed(
        &self,
        published: &Arc<Published>,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Result<GraphEngine, DataError> {
        if !published.state.has_fragments_for(EXTRACTOR_VERSION) || !published.graph_built {
            return Err(DataError::Damaged(
                "the call graph was not built by this extractor".to_string(),
            ));
        }
        // Every parse and resolution is decoded the first time an update
        // reads it.
        let parses = super::callgraph::Parses::stored(&published.graph);
        let resolved = super::callgraph::Resolutions::stored(&published.graph);
        let started = Instant::now();
        let configs = published
            .graph
            .configs(&published.state)
            .map_err(DataError::Store)?;
        timings.push(("configs", started.elapsed().as_secs_f64() * 1000.0));
        let inputs = CheckoutInputs {
            state: published.state.clone(),
            parses,
            configs,
        };
        let live = LiveCheckout::from_resolved_timed(
            inputs,
            resolved,
            Arc::clone(&published.graph),
            timings,
        )
        .map_err(|error| DataError::Resolve(error.to_string()))?;
        Ok(GraphEngine {
            measure_after_publication: false,
            published: Arc::clone(published),
            live,
            applied: BTreeMap::new(),
            overlay: OverlaySlices::new(Arc::clone(&published.graph)),
            refreshed: Refreshed {
                state: published.state.clone(),
                replaced: BTreeMap::new(),
                removed: BTreeSet::new(),
                records: BTreeMap::new(),
                dead_code: DeadCodeMemo::default(),
            },
        })
    }

    /// This checkout's publication in `snapshot`, or `None` when the
    /// snapshot has no state for it.
    fn open_published(&self, snapshot: Snapshot) -> Result<Option<Published>, DataError> {
        // A state this build cannot use (an older format version, or bytes
        // that fail their checks) is treated as never published: the
        // checkout is built again. One a newer build wrote fails with its
        // refusal instead, and nothing is built over it.
        let state = match snapshot.checkout_state(self.checkout) {
            Ok(Some(state)) => state,
            Ok(None) => return Ok(None),
            Err(error) if error.is_unusable_file() => {
                crate::slog_warn!(
                    "data layer: {} has a published state this build cannot use; building again: {error}",
                    self.root.display()
                );
                return Ok(None);
            }
            Err(error) => return Err(DataError::store(error)),
        };
        let (segments, unusable) = self.open_segments(&snapshot)?;
        let identity = format!(
            "{}:{}",
            snapshot.generation(),
            Digest128::of(&state.encode())
        );
        let published = Published::open(self.root.clone(), identity, state, segments, snapshot);
        if unusable.is_empty() {
            return published.map(Some);
        }
        // Some segment could not be used. If the rest still hold everything
        // this checkout's state names, serve them; otherwise build again.
        match published {
            Ok(published) if published.membership.missing() == 0 => Ok(Some(published)),
            outcome => {
                crate::slog_warn!(
                    "data layer: {} needs data in segments this build cannot use ({}); building again{}",
                    self.root.display(),
                    unusable.join("; "),
                    outcome.err().map(|error| format!(": {error}")).unwrap_or_default()
                );
                Ok(None)
            }
        }
    }

    /// Opens every segment of `snapshot` this build can use, and names the
    /// ones it cannot (an older format version, or failing their checks).
    /// A segment a newer build wrote fails the call with its refusal.
    fn open_segments(
        &self,
        snapshot: &Snapshot,
    ) -> Result<(Vec<Arc<Segment>>, Vec<String>), DataError> {
        let known = known_sections();
        let mut segments = Vec::new();
        let mut unusable = Vec::new();
        for entry in snapshot.manifest().segments() {
            match snapshot.open_shared_segment(entry.id, &known, &self.layer.cache) {
                Ok(segment) => segments.push(segment),
                Err(error) if error.is_unusable_file() => unusable.push(error.to_string()),
                Err(error) => return Err(DataError::store(error)),
            }
        }
        Ok((segments, unusable))
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// A published generation of one checkout: its state and segments, and
/// what each plane needs to answer from them. Immutable: its segments stay
/// readable for as long as anything holds it.
struct Published {
    root: PathBuf,
    /// The family generation it was read from and its state's digest.
    identity: String,
    state: CheckoutState,
    segments: Vec<Arc<Segment>>,
    /// Each segment's text sections that hold any, checked when the
    /// publication was opened, in `segments` order: a text query rebuilds
    /// its [`TextSegment`]s from them without reading the segments again.
    texts: Vec<(usize, TextSections)>,
    membership: TextMembership,
    graph: Arc<StoredSlices>,
    /// Whether the call graph was built: a checkout's first publication
    /// can hold its text index alone.
    graph_built: bool,
    /// Paths in a parsed language with no stored resolution: their
    /// extraction failed.
    unparsed: BTreeSet<Vec<u8>>,
    /// The dead-code snapshot of the published call graph, built on first
    /// use.
    dead_code: DeadCodeMemo,
    /// Each published path's text standing as the lexical score reads it,
    /// by position, read from its segment on first use (see
    /// [`CheckoutView::published_path_text`]).
    path_texts: PathTextMemo,
    /// Holds the lease that keeps this generation's files on disk while any
    /// view reads them.
    _snapshot: Snapshot,
}

/// How one plane of a checkout stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlaneStatus {
    /// Not built, and no build running.
    Off,
    /// Being built or brought up to date: `done` of `total` items, when the
    /// total is known.
    Building { done: usize, total: Option<usize> },
    /// Answers cover every path of the checkout.
    Ready,
}

/// How each plane of a checkout stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize)]
pub struct CheckoutStatus {
    pub text: PlaneStatus,
    /// Ready only when the call graph covers every recorded change, as a
    /// reader that needs exact answers requires.
    pub callgraph: PlaneStatus,
    /// A call graph has been published for this checkout. The call graph is
    /// refreshed lazily, so a published graph with changes pending is its
    /// normal state between edits, not a graph still being built.
    pub callgraph_published: bool,
    /// Recorded changes the call graph has not resolved yet.
    pub callgraph_pending: usize,
    /// A build is running. Answers still come from the current view, which
    /// covers every change recorded so far.
    pub building: bool,
    /// Files in a parsed language that the call graph does not cover,
    /// because extracting them failed. The plane can be ready with some.
    pub unparsed: usize,
    /// The last build or call-graph refresh failed, and none has succeeded
    /// since; [`Checkout::failure`] says why.
    pub failed: bool,
    /// The planes the checkout builds; a plane that is off reports
    /// [`PlaneStatus::Off`].
    pub planes: Planes,
}

/// A checkout's data as queries see it at one moment: the last publication
/// with the changes recorded since. Cheap to take; it holds the publication
/// open until it drops, so a query never sees a half-applied change.
#[derive(Clone)]
pub struct CheckoutView {
    published: Arc<Published>,
    planes: Planes,
    text: Arc<TextOverlay>,
    /// The publication's indexed paths not in `text`, grouped by segment,
    /// built on the first trigram lookup and shared by this view's clones.
    text_members: Arc<std::sync::OnceLock<aft_store::text::TextMembers>>,
    callgraph_pending: Arc<BTreeSet<Vec<u8>>>,
    /// A rescan was running when the view was taken (see
    /// [`Checkout::rescan`]).
    rescanning: bool,
    graph: Arc<dyn SliceSource + Send + Sync>,
    /// What the call graph resolved since the publication.
    refreshed: Option<Arc<Refreshed>>,
    /// The checkout's change count when the view was taken.
    sequence: u64,
    unparsed: Arc<BTreeSet<Vec<u8>>>,
    /// The checkout's state when the view was taken, when it differs from
    /// the publication's: the changes recorded since applied.
    current: Option<Arc<CheckoutState>>,
}

impl Published {
    fn open(
        root: PathBuf,
        identity: String,
        state: CheckoutState,
        segments: Vec<Arc<Segment>>,
        snapshot: Snapshot,
    ) -> Result<Self, DataError> {
        let mut sections = Vec::new();
        let mut texts = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            if let Some(text) = TextSegment::open(segment).map_err(DataError::store)? {
                sections.push((index, text.sections()));
                texts.push(text);
            }
        }
        let text = TextLocator::build(&texts).map_err(DataError::store)?;
        let membership = TextMembership::build(&state, &text);
        drop(texts);
        // Validate the graph's producer before pairing any state fragment
        // with a current record. Another checkout in the shared family can
        // supply current records while this publication names old fragments.
        let mut files = Vec::new();
        let mut unparsed = BTreeSet::new();
        let graph_built = state.has_fragments_for(EXTRACTOR_VERSION);
        for (index, record) in state.paths().iter().enumerate() {
            if !graph_built {
                break;
            }
            let Some(fragment) = state.fragment(index) else {
                if graph_built && is_unparsed(record) {
                    unparsed.insert(record.path.clone());
                }
                continue;
            };
            let Ok(path) = std::str::from_utf8(&record.path) else {
                continue;
            };
            let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
            files.push((
                path.to_string(),
                key.digest().to_string(),
                FragmentKey::from_digest(fragment),
            ));
        }
        // A state from another extractor, or without a stamp, serves text
        // alone. Missing records or fragments also make the graph unbuilt;
        // the checkout's own bind builds it again (see `needs_build`), while a read-only bind
        // answers text alone instead of failing.
        let (graph, graph_built) = match StoredSlices::open_files_if_held(segments.clone(), &files)
            .map_err(DataError::Store)?
        {
            Held::All(graph) => (graph, graph_built),
            Held::Missing(what) => {
                crate::slog_info!(
                    "index store call graph of {} needs a rebuild: {what}",
                    root.display()
                );
                let graph = match StoredSlices::open_files_if_held(segments.clone(), &[])
                    .map_err(DataError::Store)?
                {
                    Held::All(graph) => graph,
                    Held::Missing(what) => return Err(DataError::Store(what)),
                };
                unparsed.clear();
                (graph, false)
            }
        };
        let path_texts = PathTextMemo::new(state.paths().len());
        Ok(Self {
            root,
            identity,
            state,
            segments,
            texts: sections,
            membership,
            graph: Arc::new(graph),
            graph_built,
            unparsed,
            dead_code: DeadCodeMemo::default(),
            path_texts,
            _snapshot: snapshot,
        })
    }

    /// The text segments, in the order the membership indexes them.
    fn texts(&self) -> Result<Vec<TextSegment<'_>>, DataError> {
        Ok(self
            .texts
            .iter()
            .map(|(index, sections)| TextSegment::with_sections(&self.segments[*index], *sections))
            .collect())
    }
}

impl CheckoutView {
    /// The checkout's root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.published.root
    }

    /// The checkout's paths as last published, in byte order, with their
    /// contents and stat evidence. Paths changed since are in
    /// [`changed_paths`](Self::changed_paths).
    #[must_use]
    pub fn state(&self) -> &CheckoutState {
        &self.published.state
    }

    /// Names what text answers this view gives: it differs between two
    /// views whenever any text answer can, that is a new publication or any
    /// recorded change (a second edit of one path included). A call-graph
    /// refresh alone keeps it. For keying caches of text answers.
    #[must_use]
    pub fn text_generation(&self) -> String {
        format!("{}:{}", self.published.identity, self.sequence)
    }

    /// The paths changed since the publication, which queries answer from
    /// the changes rather than from [`state`](Self::state).
    #[must_use]
    pub fn changed_paths(&self) -> Vec<Vec<u8>> {
        // Every recorded change stays in the text changes until a build
        // covers it, whether or not the call graph has resolved it since.
        self.text.paths().map(<[u8]>::to_vec).collect()
    }

    /// How each plane stands in this view.
    #[must_use]
    pub fn status(&self) -> CheckoutStatus {
        let paths = self.published.state.paths().len();
        let text = match self.published.membership.missing() {
            0 => PlaneStatus::Ready,
            missing => PlaneStatus::Building {
                done: paths.saturating_sub(missing),
                total: Some(paths),
            },
        };
        let callgraph = match self.callgraph_pending.len() {
            _ if !self.published.graph_built => PlaneStatus::Building {
                done: 0,
                total: None,
            },
            0 if self.rescanning => PlaneStatus::Building {
                done: 0,
                total: None,
            },
            0 => PlaneStatus::Ready,
            pending => PlaneStatus::Building {
                done: 0,
                total: Some(pending),
            },
        };
        let on = |plane: PlaneStatus, on: bool| if on { plane } else { PlaneStatus::Off };
        CheckoutStatus {
            text: on(text, self.planes.text),
            callgraph: on(callgraph, self.planes.callgraph),
            callgraph_published: self.published.graph_built && self.planes.callgraph,
            callgraph_pending: self.callgraph_pending.len(),
            building: false,
            unparsed: self.unparsed.len(),
            failed: false,
            planes: self.planes,
        }
    }

    /// The planes this view's checkout builds and serves.
    #[must_use]
    pub fn planes(&self) -> Planes {
        self.planes
    }

    /// The paths in `scope` that may match `query`, in byte order, with the
    /// changes since the publication applied. Every match is among them;
    /// some may not match, so the caller reads each and checks. A path
    /// whose content is not indexed (binary, too large, not yet read) is
    /// always a candidate.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn text_candidates(
        &self,
        query: &Query,
        scope: Scope<'_>,
    ) -> Result<Vec<Vec<u8>>, DataError> {
        let published = &self.published;
        let texts = published.texts()?;
        candidates(
            &published.state,
            &published.membership,
            &texts,
            &self.text,
            query,
            scope,
        )
        .map_err(DataError::store)
    }

    /// What the text plane holds for `path`, with the changes since the
    /// publication applied; `None` when the checkout has no such path.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn path_text(&self, path: &[u8]) -> Result<Option<PathText>, DataError> {
        let published = &self.published;
        let texts = published.texts()?;
        aft_store::text::path_text(
            &published.state,
            &published.membership,
            &texts,
            &self.text,
            path,
        )
        .map_err(DataError::store)
    }

    /// How rare `trigram` is across the family's indexed contents (see
    /// [`aft_store::text::trigram_frequency`]).
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn trigram_frequency(&self, trigram: u32) -> Result<u64, DataError> {
        let texts = self.published.texts()?;
        aft_store::text::trigram_frequency(&texts, trigram).map_err(DataError::store)
    }

    /// The paths whose indexed content holds `trigram`, in byte order, with
    /// the changes since the publication applied.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn paths_with_trigram(&self, trigram: u32) -> Result<Vec<Vec<u8>>, DataError> {
        let published = &self.published;
        let texts = published.texts()?;
        aft_store::text::paths_with_trigram_among(
            &published.state,
            self.members(&texts),
            &texts,
            &self.text,
            trigram,
        )
        .map_err(DataError::store)
    }

    /// [`Self::paths_with_trigram`] without copying a path: the published
    /// paths by their position in [`Self::published_path`]'s list, and the
    /// paths changed since the publication by name. For a caller that asks
    /// about many trigrams and many paths, such as lexical scoring.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn trigram_holders(
        &self,
        trigram: u32,
    ) -> Result<aft_store::text::TrigramHolders, DataError> {
        let texts = self.published.texts()?;
        aft_store::text::trigram_holders_among(self.members(&texts), &texts, &self.text, trigram)
            .map_err(DataError::store)
    }

    /// The published path at `index`, a position
    /// [`Self::trigram_holders`] gives.
    #[must_use]
    pub fn published_path(&self, index: usize) -> &[u8] {
        &self.published.state.paths()[index].path
    }

    /// What the text plane holds for the published path at `index`, a
    /// position [`Self::trigram_holders`] gives (so one the changes since
    /// the publication have not replaced).
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn published_path_text(&self, index: usize) -> Result<PathText, DataError> {
        let memo = &self.published.path_texts;
        if let Some(text) = memo.get(index) {
            return Ok(text);
        }
        let texts = self.published.texts()?;
        let text = aft_store::text::published_path_text(&self.published.membership, &texts, index)
            .map_err(DataError::store)?;
        memo.set(index, text);
        Ok(text)
    }

    /// The publication's indexed paths not in this view's changes, grouped
    /// by segment, built on the first trigram lookup.
    fn members(&self, texts: &[TextSegment<'_>]) -> &aft_store::text::TextMembers {
        self.text_members.get_or_init(|| {
            aft_store::text::TextMembers::build(
                &self.published.state,
                &self.published.membership,
                texts.len(),
                &self.text,
            )
        })
    }

    #[cfg(test)]
    /// [`Self::dead_code_snapshot_shared`] as an owned snapshot, for tests
    /// that hand one to an inspect job.
    ///
    /// # Errors
    ///
    /// When a stored parse cannot be read.
    pub(crate) fn dead_code_snapshot(
        &self,
    ) -> Result<crate::inspect::job::CallgraphSnapshot, DataError> {
        self.dead_code_snapshot_shared()
            .map(|snapshot| crate::inspect::job::CallgraphSnapshot::clone(&snapshot))
    }

    #[cfg(test)]
    /// The dead-code snapshot `aft_inspect` analyses: the call graph's
    /// files, exports and entry points, carrying this view's call graph for
    /// the rollup to read one file at a time. The parts it holds are built
    /// once per call graph: every view of the same publication and refresh
    /// shares them.
    ///
    /// # Errors
    ///
    /// When a stored parse cannot be read.
    pub(crate) fn dead_code_snapshot_shared(
        &self,
    ) -> Result<Arc<crate::inspect::job::CallgraphSnapshot>, DataError> {
        self.dead_code_snapshot_in(None)
    }

    /// [`Self::dead_code_snapshot_shared`], whose rollup keeps the dead-code
    /// graph of this view's publication under `section_dir` (see
    /// `dead_code_graph::ViewCallGraph`).
    ///
    /// # Errors
    ///
    /// When a stored parse cannot be read.
    pub(crate) fn dead_code_snapshot_in(
        &self,
        section_dir: Option<PathBuf>,
    ) -> Result<Arc<crate::inspect::job::CallgraphSnapshot>, DataError> {
        let parts = self
            .dead_code_memo()
            .get_or_build(|| self.build_dead_code_snapshot())?;
        // The memo holds no view: a view holds its publication, which holds
        // the memo.
        Ok(Arc::new(crate::inspect::job::CallgraphSnapshot {
            data_layer: Some(super::dead_code_graph::ViewCallGraph::new(
                self.clone(),
                section_dir,
            )),
            ..crate::inspect::job::CallgraphSnapshot::clone(&parts)
        }))
    }

    /// The memo of the dead-code snapshot this view's call graph serves.
    fn dead_code_memo(&self) -> &DeadCodeMemo {
        match &self.refreshed {
            Some(refreshed) => &refreshed.dead_code,
            None => &self.published.dead_code,
        }
    }

    /// Drops the dead-code snapshot this view's call graph keeps for later
    /// requests (see [`Self::dead_code_snapshot_in`]); the next request
    /// builds it again. True when one was held.
    pub(crate) fn release_dead_code_snapshot(&self) -> bool {
        self.dead_code_memo().release()
    }

    /// Whether this view's call graph holds a built dead-code snapshot.
    #[cfg(test)]
    pub(crate) fn dead_code_snapshot_held(&self) -> bool {
        self.dead_code_memo().try_files().is_some()
    }

    fn build_dead_code_snapshot(
        &self,
    ) -> Result<crate::inspect::job::CallgraphSnapshot, DataError> {
        // The snapshot lists the files and entry points only: its exports
        // are read from the view's files by whoever needs them (see
        // `dead_code_graph::ViewCallGraph::exported_symbols_of`), so a rollup
        // that reads a kept dead-code graph never reads every file's parse,
        // and the memo stays small.
        let files = self.dead_code_files();
        let snapshot_files =
            super::dead_code_graph::snapshot_files_of(&self.published.root, &files);
        let resolved = crate::inspect::resolve_entry_points(&self.published.root);
        let entry_points = snapshot_files
            .iter()
            .filter(|file| resolved.is_entry_point(file))
            .cloned()
            .collect();
        Ok(crate::inspect::job::CallgraphSnapshot {
            generated_at: Some(std::time::SystemTime::now()),
            files: snapshot_files,
            entry_points,
            ..crate::inspect::job::CallgraphSnapshot::default()
        })
    }

    /// A dead-code snapshot holding every call row of the call graph, built
    /// from every file's stored record and fragment in one pass. The rollup
    /// reads the call graph file by file instead; the dead-code gate uses
    /// this one as an independent check of that reading.
    ///
    /// # Errors
    ///
    /// When a record or fragment cannot be read.
    pub(crate) fn rows_dead_code_snapshot(
        &self,
    ) -> Result<crate::inspect::job::CallgraphSnapshot, DataError> {
        let (mut parses, mut resolved) = self
            .published
            .graph
            .resolutions()
            .map_err(DataError::Store)?;
        // The call graph as callgraph() answers it: what was resolved since
        // the publication replaces what the publication stored.
        let state = match &self.refreshed {
            Some(refreshed) => {
                for file in &refreshed.removed {
                    parses.remove(file);
                    resolved.remove(file);
                }
                for (file, refreshed_file) in &refreshed.replaced {
                    let key = refreshed_file.resolution.record_key();
                    match &refreshed_file.parse {
                        Some(parse) => {
                            parses.insert(file.clone(), (key.to_string(), Arc::clone(parse)));
                        }
                        // The publication's parse of this path, read above,
                        // has the key the file was resolved with.
                        None => debug_assert_eq!(
                            parses.get(file).map(|(stored, _)| stored.as_str()),
                            Some(key)
                        ),
                    }
                    resolved.insert(file.clone(), refreshed_file.resolution.decode(file));
                }
                refreshed.state.clone()
            }
            None => self.published.state.clone(),
        };
        let inputs = CheckoutInputs {
            state,
            parses: parses.into(),
            configs: HashMap::new(),
        };
        let rows = super::graph_rows::assemble(&inputs, &resolved);
        Ok(super::dead_code::snapshot(&self.published.root, &rows))
    }

    /// The call graph dead code reads, file by file: the publication, with
    /// what was resolved since replacing what it stored, read on demand
    /// rather than all at once (see `dead_code_graph`).
    pub(crate) fn dead_code_files(&self) -> ViewGraphFiles<'_> {
        ViewGraphFiles {
            published: &self.published.graph,
            refreshed: self.refreshed.as_deref(),
            state: self
                .refreshed
                .as_deref()
                .map_or(&self.published.state, |refreshed| &refreshed.state),
        }
    }

    /// The call graph as the call-graph commands read it: the publication,
    /// with the files resolved since laid over it. A file in
    /// [`changed_paths`](Self::changed_paths) answers as it was last
    /// resolved until it is resolved again; the status says so.
    #[must_use]
    pub(crate) fn callgraph(&self) -> SliceReader<Arc<dyn SliceSource + Send + Sync>> {
        SliceReader::new(
            Arc::clone(&self.graph),
            self.published.root.clone(),
            self.published.root.display().to_string(),
        )
    }

    /// Whether the call graph has `file` (relative to the root): a parsed
    /// file of the checkout.
    #[must_use]
    pub fn callgraph_has_file(&self, file: &str) -> bool {
        self.graph.slice(file).is_some()
    }

    /// Reads of the call graph that failed: a checksum or a malformed
    /// record. A caller that saw a file as absent can check here whether
    /// that was a failure.
    #[must_use]
    pub fn callgraph_failures(&self) -> Vec<String> {
        self.published.graph.failures()
    }

    /// Runs one call-graph command on this view for a measurement probe:
    /// `callers` at depth 1, `impact` at depth 3 or `trace_to`, for `symbol`
    /// in `file` (relative to the root). Returns how many result rows it
    /// produced, or the error text.
    #[doc(hidden)]
    pub fn probe_query(&self, command: &str, file: &str, symbol: &str) -> Result<usize, String> {
        use crate::commands::callgraph_store_adapter as adapter;
        let graph = self.callgraph();
        let path = self.published.root.join(file);
        match command {
            "callers" => adapter::callers_result(&graph, &path, symbol, 1, true)
                .map(|result| result.total_callers)
                .map_err(|error| format!("{error:?}")),
            "impact" => adapter::impact_result(&graph, &path, symbol, 3, true)
                .map(|result| format!("{result:?}").len())
                .map_err(|error| format!("{error:?}")),
            "trace_to" => adapter::trace_to_result(&graph, &path, symbol, 10, true)
                .map(|result| format!("{result:?}").len())
                .map_err(|error| format!("{error:?}")),
            other => Err(format!("unknown probe command {other}")),
        }
    }

    /// Sums [`FileSlice::probe_breakdown`] over every file of the call
    /// graph, for a measurement probe.
    #[doc(hidden)]
    pub fn probe_breakdown(&self) -> [usize; 10] {
        let mut total = [0usize; 10];
        for record in self.published.state.paths() {
            let Ok(path) = std::str::from_utf8(&record.path) else {
                continue;
            };
            if let Some(slice) = self.graph.slice(path) {
                for (sum, part) in total.iter_mut().zip(slice.probe_breakdown()) {
                    *sum += part;
                }
            }
        }
        total
    }

    /// Decodes every file of the call graph, as a whole-graph command does,
    /// for a measurement probe. Returns the files decoded and each decode's
    /// time in microseconds.
    #[doc(hidden)]
    pub fn probe_decode_all(&self) -> Vec<u64> {
        self.published
            .state
            .paths()
            .iter()
            .filter_map(|record| std::str::from_utf8(&record.path).ok())
            .filter_map(|path| {
                let started = std::time::Instant::now();
                self.graph.slice(path)?;
                Some(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX))
            })
            .collect()
    }
}

/// A view's call graph read file by file: the publication's stored records
/// and fragments, with each file resolved since the publication answered
/// from memory and each file it no longer parses left out.
pub(crate) struct ViewGraphFiles<'v> {
    published: &'v StoredSlices,
    refreshed: Option<&'v Refreshed>,
    /// The checkout's paths the call graph was resolved over.
    state: &'v CheckoutState,
}

/// What identifies one file's facts in a view's call graph, so two views'
/// files can be told apart without reading them: its content record key,
/// and its resolution's fragment key when the publication's fragment is
/// current, else a digest of the resolution's bytes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GraphIdentity {
    pub(crate) record: String,
    pub(crate) fragment: Option<String>,
    pub(crate) digest: Option<String>,
}

impl ViewGraphFiles<'_> {
    /// `path`'s identity in this view; `None` when the view does not parse
    /// it.
    pub(crate) fn identity(&self, path: &str) -> Option<GraphIdentity> {
        if let Some(refreshed) = self.refreshed {
            if refreshed.removed.contains(path) {
                return None;
            }
            if let Some(file) = refreshed.replaced.get(path) {
                return Some(GraphIdentity {
                    record: file.resolution.record_key().to_string(),
                    fragment: None,
                    digest: Some(blake3::hash(file.resolution.bytes()).to_hex().to_string()),
                });
            }
        }
        let (record, fragment) = self.published.identity_of(path)?;
        Some(GraphIdentity {
            record: record.to_string(),
            fragment: Some(format!("{fragment:?}")),
            digest: None,
        })
    }

    /// The publication's own identity of `path` (ignoring what was resolved
    /// since), with the digest of its stored fragment bytes.
    ///
    /// # Errors
    ///
    /// When the fragment cannot be read.
    pub(crate) fn published_identity(&self, path: &str) -> Result<Option<GraphIdentity>, String> {
        let Some((record, fragment)) = self.published.identity_of(path) else {
            return Ok(None);
        };
        Ok(Some(GraphIdentity {
            record: record.to_string(),
            fragment: Some(format!("{fragment:?}")),
            digest: self.published.fragment_digest(path)?,
        }))
    }
}

impl super::dead_code_graph::CallGraphFiles for ViewGraphFiles<'_> {
    fn is_regular_file(&self, path: &str) -> bool {
        self.state
            .get(path.as_bytes())
            .is_some_and(|record| record.kind != PathKind::Symlink)
    }

    fn has_file(&self, path: &str) -> bool {
        match self.refreshed {
            Some(refreshed) if refreshed.removed.contains(path) => false,
            Some(refreshed) if refreshed.replaced.contains_key(path) => true,
            _ => self.published.has_parsed_file(path),
        }
    }

    fn paths(&self) -> Vec<String> {
        let mut paths = self
            .published
            .parsed_files()
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if let Some(refreshed) = self.refreshed {
            for path in &refreshed.removed {
                paths.remove(path);
            }
            paths.extend(refreshed.replaced.keys().cloned());
        }
        paths.into_iter().collect()
    }

    fn parse(&self, path: &str) -> Result<Arc<ParseBlob>, String> {
        // A refreshed file without its own parse uses the publication's.
        if let Some(parse) = self
            .refreshed
            .and_then(|refreshed| refreshed.replaced.get(path))
            .and_then(|file| file.parse.as_ref())
        {
            return Ok(Arc::clone(parse));
        }
        self.published
            .parse_of(path)
            .map(|(_, parse)| parse)
            .ok_or_else(|| format!("no stored parse for {path}"))
    }

    fn resolution(&self, path: &str) -> Result<Arc<FileResolution>, String> {
        if let Some(file) = self
            .refreshed
            .and_then(|refreshed| refreshed.replaced.get(path))
        {
            return Ok(Arc::new(file.resolution.decode(path)));
        }
        self.published
            .resolution_of(path)
            .map(Arc::new)
            .ok_or_else(|| format!("no stored resolution for {path}"))
    }

    fn dead_code_facts(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(encoded) = self
            .refreshed
            .and_then(|refreshed| refreshed.records.get(path))
        {
            let record = encoded
                .decode()
                .map_err(|error| format!("{path}: changed content record: {error:?}"))?;
            return Ok(match &record.data {
                RecordBody::Parsed { body, .. } => body.dead_code.clone(),
                RecordBody::Config { .. } | RecordBody::Markers => None,
            });
        }
        self.published.dead_code_facts_of(path)
    }
}

/// The resolver's inputs for the checkout's current `state`: `previous`'s
/// parses and configuration, with each path in `changed` taken from what its
/// change extracted.
fn next_inputs(
    state: &CheckoutState,
    previous: &CheckoutInputs,
    changes: &BTreeMap<Vec<u8>, Change>,
    changed: &BTreeSet<Vec<u8>>,
) -> CheckoutInputs {
    let mut parses = previous.parses.clone();
    let mut configs = previous.configs.clone();
    for path in changed {
        let Ok(name) = std::str::from_utf8(path) else {
            continue;
        };
        parses.remove(name);
        configs.remove(path);
        match changes.get(path).map(|change| change.content.get()) {
            Some(Extracted::Parse(key, parse, _)) => {
                parses.insert(name.to_string(), (key.clone(), Arc::clone(parse)));
            }
            Some(Extracted::Config(source, _)) => {
                configs.insert(path.clone(), Arc::clone(source));
            }
            Some(Extracted::Markers(_) | Extracted::Nothing) | None => {}
        }
    }
    CheckoutInputs {
        state: state.clone(),
        parses,
        configs,
    }
}

/// `base` with every change's record in place of the base's.
fn with_changes(base: &CheckoutState, changes: &BTreeMap<Vec<u8>, Change>) -> CheckoutState {
    let mut records = base
        .paths()
        .iter()
        .filter(|record| !changes.contains_key(&record.path))
        .cloned()
        .collect::<Vec<_>>();
    records.extend(changes.values().filter_map(|change| change.record.clone()));
    CheckoutState::new(base.git.clone(), base.conversion, records)
        .expect("base paths without the changed ones, plus the changed ones")
        .with_root_of(base)
}

/// Paths whose member differs between two states of one checkout: added,
/// removed, or changed in content, kind, mode or link target.
fn changed_members(before: &CheckoutState, after: &CheckoutState) -> Vec<Vec<u8>> {
    let same = same_member;
    let mut changed = BTreeSet::new();
    for record in before.paths() {
        match after.get(&record.path) {
            Some(now) if same(record, now) => {}
            _ => {
                changed.insert(record.path.clone());
            }
        }
    }
    for record in after.paths() {
        if before.get(&record.path).is_none() {
            changed.insert(record.path.clone());
        }
    }
    changed.into_iter().collect()
}

/// Whether two records of one path describe the same member: the same
/// content, kind, mode and link target, whatever their stat says.
fn same_member(a: &PathRecord, b: &PathRecord) -> bool {
    a.kind == b.kind && a.content == b.content && a.mode == b.mode && a.link == b.link
}

/// A scan's sink that keeps nothing: the scan records each file's content
/// key, and nothing else is needed from the bytes it reads.
struct ContentOnly;

impl ContentSink for ContentOnly {
    fn read(&self, _path: &[u8], _key: ContentKey, _kind: ContentKind, _bytes: &[u8]) {}
}

/// Whether `record` is a text file in a parsed language that has no
/// resolution: given that it has none, its extraction failed. Resolver
/// configuration files are read raw and never resolved, and files whose
/// record holds only their TODO markers are not resolved either.
fn is_unparsed(record: &PathRecord) -> bool {
    matches!(record.kind, PathKind::Text | PathKind::Large)
        && super::content::is_parsed_language(&record.language)
}

/// The language a changed file is parsed as: the one its path and bytes
/// choose, for a text file.
fn changed_language(record: &PathRecord, bytes: &[u8]) -> String {
    if !matches!(record.kind, PathKind::Text | PathKind::Large) {
        return String::new();
    }
    std::str::from_utf8(&record.path)
        .ok()
        .and_then(|path| language_for(path, bytes))
        .unwrap_or_default()
}

/// What the call graph reads from a changed file recorded as `source`. As
/// in a build, a file the extractor fails on is not parsed.
fn extract_recorded(source: &RecordedSource) -> Extracted {
    let language = &source.language;
    let Ok(mut extracted) = extract(&source.bytes, language) else {
        return Extracted::Nothing;
    };
    if let Ok(path) = std::str::from_utf8(&source.path) {
        // Without them a configuration file's TODOs are reported unread.
        let _ = super::content::add_config_markers(&mut extracted, path, &source.bytes);
    }
    match &extracted.data {
        RecordBody::Config { source } => {
            let source = Arc::from(source.as_slice());
            Extracted::Config(source, Arc::new(EncodedRecord::of(&extracted)))
        }
        RecordBody::Markers => Extracted::Markers(Arc::new(EncodedRecord::of(&extracted))),
        RecordBody::Parsed { .. } => match blob_from_record(&extracted) {
            Ok(CallgraphBlob::Parse(parse)) => {
                let key = RecordKey::new(source.content, language, EXTRACTOR_VERSION);
                Extracted::Parse(
                    key.digest().to_string(),
                    Arc::new(parse),
                    Arc::new(EncodedRecord::of(&extracted)),
                )
            }
            _ => Extracted::Nothing,
        },
    }
}

/// The index of the newest segment `find` locates a key in.
/// What [`Checkout::segments_read_by`] found in one generation.
struct SegmentsRead {
    entries: Vec<aft_store::family::SegmentEntry>,
    /// Each segment by its place in the manifest; `None` when this build
    /// cannot use it.
    opened: Vec<Option<Arc<Segment>>>,
    /// The places of the segments in `opened` that are open.
    usable: Vec<usize>,
    /// By place in the manifest: some given state reads from it.
    needed: Vec<bool>,
}

/// A unit a checkout's state reads from a segment.
#[derive(Clone, Copy)]
enum ReadUnit {
    Text(ContentKey),
    Record(RecordKey),
    Fragment(FragmentKey),
}

/// The units the family's other checkouts read, each with the place in the
/// manifest of the segment they read it from: the newest that holds it.
#[derive(Default)]
struct ReadByOthers {
    text: HashMap<ContentKey, usize>,
    records: HashMap<RecordKey, usize>,
    fragments: HashMap<FragmentKey, usize>,
}

impl ReadByOthers {
    /// Where the other checkouts read `unit` from, if they read it.
    fn holder(&self, unit: ReadUnit) -> Option<usize> {
        match unit {
            ReadUnit::Text(key) => self.text.get(&key),
            ReadUnit::Record(key) => self.records.get(&key),
            ReadUnit::Fragment(key) => self.fragments.get(&key),
        }
        .copied()
    }

    /// Leaves out of a new segment the units the other checkouts read from
    /// a segment `keep` accepts, and returns how many of each it left out.
    /// The checkout writing the segment then reads them from where the
    /// others do.
    fn leave_out(
        &self,
        text: Option<&mut TextSectionsBuilder>,
        sections: Option<&mut super::graph_store::SectionsWriter<'_>>,
        keep: impl Fn(usize) -> bool,
    ) -> SharedUnits {
        let shared = |unit| self.holder(unit).is_some_and(&keep);
        let mut left = SharedUnits::default();
        if let Some(text) = text {
            let before = text.len();
            text.retain(|key| !shared(ReadUnit::Text(key)));
            left.text_units = before - text.len();
        }
        if let Some(sections) = sections {
            (left.records, left.fragments) = sections.leave_out(
                |key| shared(ReadUnit::Record(key)),
                |key| shared(ReadUnit::Fragment(key)),
            );
        }
        left
    }
}

fn newest_holder<T>(
    segments: &[Option<T>],
    find: impl Fn(&T) -> Result<Option<u32>, aft_store::segment::SegmentError>,
) -> Result<Option<usize>, DataError> {
    for (index, segment) in segments.iter().enumerate().rev() {
        let Some(segment) = segment else {
            continue;
        };
        if find(segment).map_err(DataError::store)?.is_some() {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// Keeps the bytes of every text file a change scan reads, for the text
/// overlay and the call graph.
#[derive(Default)]
struct ChangeSink {
    read: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl ChangeSink {
    fn finish(self) -> HashMap<Vec<u8>, Vec<u8>> {
        self.read.into_inner().expect("no panics while holding it")
    }
}

impl ContentSink for ChangeSink {
    fn read(&self, path: &[u8], _key: ContentKey, kind: ContentKind, bytes: &[u8]) {
        if kind != ContentKind::Binary {
            self.read
                .lock()
                .expect("no panics while holding it")
                .insert(path.to_vec(), bytes.to_vec());
        }
    }
}

/// What a build leaves out of its publication.
/// The family's other checkouts' states (see [`Checkout::other_states`]).
enum OtherStates {
    Read {
        states: Vec<CheckoutState>,
        /// Each state's checkout, in the same order.
        ids: Vec<CheckoutId>,
        gone: Vec<CheckoutId>,
    },
    /// One of them cannot be read: it could need any segment.
    Unreadable { gone: Vec<CheckoutId> },
}

/// What a build left out of its new segment, or a publication of changes
/// out of its own, because another checkout already reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SharedUnits {
    pub text_units: usize,
    pub records: usize,
    pub fragments: usize,
}

struct Unneeded {
    /// Segments no checkout reads any more.
    segments: Vec<SegmentId>,
    /// Other checkouts of the family whose folder is gone.
    checkouts: Vec<CheckoutId>,
}

/// Whether the checkout folder `root` is confirmed gone: it is not found
/// while its parent folder can be listed. Any other answer is "can't tell"
/// and keeps the checkout: a volume that is not mounted (its parent is
/// missing too), a folder that cannot be read, or one that exists. Worker
/// worktrees are created and deleted all day, and a deleted one's state
/// would otherwise keep every segment it read for good.
fn folder_is_gone(root: &Path) -> bool {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return false,
    }
    root.parent()
        .is_some_and(|parent| std::fs::read_dir(parent).is_ok())
}

/// What a build's scan collects from the files it reads.
#[derive(Default)]
struct BuildSink {
    /// Whether parsed files are extracted, or only text is indexed.
    text_only: bool,
    /// Whether the text index is off, so no text is indexed.
    skip_text: bool,
    text: Mutex<TextSectionsBuilder>,
    seen_text: Mutex<HashSet<ContentKey>>,
    /// Each parsed path's language.
    languages: Mutex<HashMap<Vec<u8>, String>>,
    /// Each extracted record, by the parse key the resolver uses.
    records: Mutex<HashMap<String, (RecordKey, Arc<ContentRecord>)>>,
    failures: std::sync::atomic::AtomicUsize,
    /// The records the family already stores, read instead of parsing a
    /// file again, and how many were.
    stored: StoredRecords,
    reused: std::sync::atomic::AtomicUsize,
}

/// The content records the family stores, which a build reads instead of
/// parsing a file again: a record's key names its content, language and
/// extractor version, so the stored one is what extraction would give.
#[derive(Default)]
struct StoredRecords {
    /// Held so the segments' files stay on disk while the build reads them.
    _snapshot: Option<Snapshot>,
    /// Newest last.
    segments: Vec<Arc<Segment>>,
}

impl StoredRecords {
    /// The stored record with `key`, from the newest segment holding it. A
    /// segment that fails to read counts as not holding it, so the file is
    /// parsed instead.
    fn record(&self, key: RecordKey) -> Option<ContentRecord> {
        self.segments.iter().rev().find_map(|segment| {
            let content = ContentSegment::open(segment).ok().flatten()?;
            let ordinal = content.find(key).ok().flatten()?;
            content.record(ordinal).ok()
        })
    }
}

/// A build sink's results, after the scan.
struct Collected {
    text: TextSectionsBuilder,
    seen_text: HashSet<ContentKey>,
    languages: HashMap<Vec<u8>, String>,
    records: HashMap<String, (RecordKey, Arc<ContentRecord>)>,
    failures: usize,
    /// Records read from the store rather than parsed.
    reused: usize,
}

impl BuildSink {
    fn text_only() -> Self {
        Self {
            text_only: true,
            ..Self::default()
        }
    }

    fn finish(self) -> Collected {
        Collected {
            text: self.text.into_inner().expect("no panics while holding it"),
            seen_text: self
                .seen_text
                .into_inner()
                .expect("no panics while holding it"),
            languages: self
                .languages
                .into_inner()
                .expect("no panics while holding it"),
            records: self
                .records
                .into_inner()
                .expect("no panics while holding it"),
            failures: self.failures.into_inner(),
            reused: self.reused.into_inner(),
        }
    }
}

impl ContentSink for BuildSink {
    fn read(&self, path: &[u8], key: ContentKey, kind: ContentKind, bytes: &[u8]) {
        let first = self
            .seen_text
            .lock()
            .expect("no panics while holding it")
            .insert(key);
        if first && !self.skip_text {
            let len = bytes.len() as u64;
            match kind {
                ContentKind::Text => {
                    let trigrams = ContentTrigrams::of(bytes);
                    self.text
                        .lock()
                        .expect("no panics while holding it")
                        .add_indexed(key, len, &trigrams);
                }
                ContentKind::Binary => self
                    .text
                    .lock()
                    .expect("no panics while holding it")
                    .add_unindexed(key, len, TextStatus::Binary),
                // Read only to be parsed; text queries read it directly.
                ContentKind::Large => self
                    .text
                    .lock()
                    .expect("no panics while holding it")
                    .add_unindexed(key, len, TextStatus::Oversize),
            }
        }
        if kind == ContentKind::Binary || self.text_only {
            return;
        }
        let Ok(path_text) = std::str::from_utf8(path) else {
            return;
        };
        let Some(language) = language_for(path_text, bytes) else {
            return;
        };
        self.languages
            .lock()
            .expect("no panics while holding it")
            .insert(path.to_vec(), language.clone());
        let record_key = RecordKey::new(key, &language, EXTRACTOR_VERSION);
        let parse_key = record_key.digest().to_string();
        if self
            .records
            .lock()
            .expect("no panics while holding it")
            .contains_key(&parse_key)
        {
            return;
        }
        let extracted = match self.stored.record(record_key) {
            Some(record) => {
                self.reused
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(record)
            }
            None => extract(bytes, &language).map(|mut record| {
                // Without them a configuration file's TODOs are reported
                // unread; the resolver still reads its bytes.
                let _ = super::content::add_config_markers(&mut record, path_text, bytes);
                record
            }),
        };
        match extracted {
            Ok(record) => {
                self.records
                    .lock()
                    .expect("no panics while holding it")
                    .insert(parse_key, (record_key, Arc::new(record)));
            }
            Err(_) => {
                self.failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

impl Collected {
    /// What resolving `state` reads: every parsed path's parse and every
    /// configuration file's bytes. Takes the extracted records, which leave
    /// as the returned builder's encoded ones: each parse is kept as its
    /// encoded record and decoded when the resolver reads it (see
    /// [`super::callgraph::Parses::encoded`]), so the build never holds the
    /// decoded records and every decoded parse at once.
    fn take_inputs(
        &mut self,
        state: &CheckoutState,
    ) -> (CheckoutInputs, aft_store::content::ContentSectionsBuilder) {
        let mut configs = HashMap::new();
        // A record whose parse does not convert is left out, as reading it
        // decoded would leave it out; checked once while it is at hand.
        let mut converts = HashMap::<String, bool>::new();
        for record in state.paths() {
            if record.language.is_empty() {
                continue;
            }
            let record_key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
            let parse_key = record_key.digest().to_string();
            let Some((_, stored)) = self.records.get(&parse_key) else {
                continue;
            };
            match &stored.data {
                RecordBody::Config { source } => {
                    configs.insert(record.path.clone(), Arc::from(source.as_slice()));
                }
                RecordBody::Parsed { .. } => {
                    converts.entry(parse_key).or_insert_with(|| {
                        matches!(blob_from_record(stored), Ok(CallgraphBlob::Parse(_)))
                    });
                }
                RecordBody::Markers => {}
            }
        }
        let mut encoded = aft_store::content::ContentSectionsBuilder::new();
        for (_, (record_key, record)) in std::mem::take(&mut self.records) {
            encoded.add(record_key, &record);
        }
        let mut parses = Vec::new();
        for record in state.paths() {
            let Ok(path) = std::str::from_utf8(&record.path) else {
                continue;
            };
            let record_key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
            let parse_key = record_key.digest().to_string();
            if converts.get(&parse_key) != Some(&true) {
                continue;
            }
            let Some(stored) = encoded.get(&record_key) else {
                continue;
            };
            parses.push((path.to_string(), parse_key, Arc::clone(stored)));
        }
        (
            CheckoutInputs {
                state: state.clone(),
                parses: super::callgraph::Parses::encoded(parses),
                configs,
            },
            encoded,
        )
    }
}

impl DataLayer {
    /// The shared block cache's counters, for the memory census.
    #[must_use]
    pub fn cache_stats(&self) -> aft_store::cache::CacheStats {
        self.cache.stats()
    }

    /// The shared block cache's bytes in memory, for the memory census; see
    /// [`aft_store::cache::BlockCache::memory_bytes`].
    #[must_use]
    pub fn cache_memory_bytes(&self) -> Option<usize> {
        self.cache.memory_bytes()
    }
}

/// What one checkout holds in memory, for the memory census, measured as
/// the [`aft_store::heap`] module describes and split by who reads it:
///
/// - shared by the planes: the checkout state lists (the publication's, the
///   current one while changes are recorded, the ones the call graph's
///   engine and its last refresh resolved over), each changed path's record,
///   and the publication's open segments and snapshot;
/// - the text plane: the publication's text membership and section handles,
///   each published path's memoised text-index status (indexed with its
///   distinct trigram count, not indexed, or excluded), the text overlay of
///   changed contents, and the text members trigram lookups share;
/// - the call graph: the publication's stored slices with every slice
///   decoded so far, the call graph served with the files resolved since laid
///   over it, the last refresh's parses and resolutions, the dead-code
///   snapshot, the parses of changed files, and the incremental engine.
///
/// An allocation reached from more than one part is counted once, with the
/// part the walk reaches first. Taken without waiting for any lock: a part
/// another thread holds is left out and counted in the walk's
/// [`aft_store::heap::Seen::contended`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CheckoutMemory {
    /// The publication and the changes since; `None` when their lock was
    /// held.
    pub live: Option<LiveMemory>,
    /// The call graph's incremental state between publications.
    pub engine: EngineMemory,
}

/// The memory a checkout's publication and recorded changes hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LiveMemory {
    /// Paths in the published state; zero before the first publication.
    pub state_paths: usize,
    /// Bytes of what the planes share (see [`CheckoutMemory`]).
    pub shared_bytes: u64,
    /// Bytes the text plane holds, the overlay's included.
    pub text_bytes: u64,
    /// Paths whose changed text queries read from memory.
    pub text_overlay_paths: usize,
    /// Bytes of those paths and their trigrams.
    pub text_overlay_bytes: u64,
    /// Bytes the call graph holds apart from its engine.
    pub callgraph_bytes: u64,
    /// Paths changed since the publication.
    pub changes: usize,
    /// Changed paths the call graph has not resolved yet.
    pub pending_callgraph_paths: usize,
    /// Files the call graph resolved again, or dropped, since the
    /// publication.
    pub refreshed_files: usize,
    /// Files in the dead-code snapshot of the served call graph, once one is
    /// built; `None` while none is, or while one is being built.
    pub dead_code_files: Option<usize>,
}

/// The call graph's incremental engine, as the memory census sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EngineMemory {
    /// No refresh has started it since the publication.
    #[default]
    NotStarted,
    /// A refresh holds it.
    Busy,
    /// Started: its parse and resolution slots (each decoded on first use),
    /// the bytes of the checkout state lists it holds, and the bytes of
    /// everything else it holds.
    Running {
        parses: usize,
        resolutions: usize,
        state_bytes: u64,
        bytes: u64,
    },
}

impl DeadCodeMemo {
    /// The built snapshot's file count; `None` when none is built or the
    /// memo is being built.
    fn try_files(&self) -> Option<usize> {
        let slot = self.0.try_lock().ok()?;
        slot.as_ref().map(|snapshot| snapshot.files.len())
    }
}

impl aft_store::heap::HeapBytes for DeadCodeMemo {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.0.heap_bytes(seen)
    }
}

impl aft_store::heap::HeapBytes for PathTextMemo {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.0.heap_bytes(seen)
    }
}

aft_store::heap_bytes_of_fields!(RefreshedFile { resolution, parse });
aft_store::heap_bytes_of_fields!(ChangeContent { source, extracted });
aft_store::heap_bytes_of_fields!(RecordedSource {
    path,
    language,
    content,
    bytes,
});

impl aft_store::heap::HeapBytes for Extracted {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Nothing => 0,
            Self::Parse(key, parse, record) => {
                key.heap_bytes(seen) + parse.heap_bytes(seen) + record.heap_bytes(seen)
            }
            Self::Config(bytes, record) => bytes.heap_bytes(seen) + record.heap_bytes(seen),
            Self::Markers(record) => record.heap_bytes(seen),
        }
    }
}

/// Bytes counted for each part of a checkout (see [`CheckoutMemory`]).
#[derive(Default)]
struct MemoryParts {
    shared: usize,
    text: usize,
    callgraph: usize,
}

impl MemoryParts {
    /// Counts `published`, unless the walk has counted it already: its
    /// state, segments and snapshot as shared, its text lookups as the text
    /// plane's, and its stored slices, unparsed paths and dead-code snapshot
    /// as the call graph's.
    fn published(&mut self, published: &Arc<Published>, seen: &mut aft_store::heap::Seen) {
        use aft_store::heap::HeapBytes as _;
        if !seen.first(Arc::as_ptr(published)) {
            return;
        }
        let Published {
            root,
            identity,
            state,
            segments,
            texts,
            membership,
            graph,
            graph_built: _,
            unparsed,
            dead_code,
            path_texts,
            _snapshot,
        } = &**published;
        self.shared += aft_store::heap::COUNTS
            + std::mem::size_of::<Published>()
            + root.heap_bytes(seen)
            + identity.heap_bytes(seen)
            + state.heap_bytes(seen)
            + segments.heap_bytes(seen)
            + _snapshot.heap_bytes(seen);
        self.text +=
            texts.heap_bytes(seen) + membership.heap_bytes(seen) + path_texts.heap_bytes(seen);
        self.callgraph +=
            graph.heap_bytes(seen) + unparsed.heap_bytes(seen) + dead_code.heap_bytes(seen);
    }

    /// Counts what a call-graph refresh resolved: the state it resolved
    /// over as shared, the rest as the call graph's.
    fn refreshed(&mut self, refreshed: &Refreshed, seen: &mut aft_store::heap::Seen) {
        use aft_store::heap::HeapBytes as _;
        let Refreshed {
            state,
            replaced,
            removed,
            records,
            dead_code,
        } = refreshed;
        self.shared += state.heap_bytes(seen);
        self.callgraph += replaced.heap_bytes(seen)
            + removed.heap_bytes(seen)
            + records.heap_bytes(seen)
            + dead_code.heap_bytes(seen);
    }
}

impl Checkout {
    /// The folder of the family store this checkout is bound to.
    #[must_use]
    pub fn family_dir(&self) -> &Path {
        self.family.dir()
    }

    /// The call-graph bytes of [`Self::memory`]'s live part, part by part,
    /// walked in the same order so each part counts what it counts there.
    /// For finding what the served call graph holds between publications;
    /// `None` while another thread holds the checkout's live state.
    #[doc(hidden)]
    #[must_use]
    pub fn callgraph_memory_parts(&self) -> Option<Vec<(&'static str, usize)>> {
        use aft_store::heap::HeapBytes as _;
        let live = self.live.try_read().ok()?;
        let seen = &mut aft_store::heap::Seen::new();
        let mut published = MemoryParts::default();
        if let Some(graph) = &live.published {
            published.published(graph, seen);
        }
        // What the census counts as the text plane's and as shared first.
        live.text.heap_bytes(seen);
        live.text_members.heap_bytes(seen);
        live.state.heap_bytes(seen);
        for (path, change) in &live.changes {
            path.heap_bytes(seen);
            change.record.heap_bytes(seen);
        }
        let extracted = live
            .changes
            .values()
            .map(|change| change.content.heap_bytes(seen))
            .sum();
        let pending = live.unparsed.heap_bytes(seen) + live.callgraph_pending.heap_bytes(seen);
        let served = live.graph.heap_bytes(seen);
        let (replaced, other) = live.refreshed.as_ref().map_or((0, 0), |refreshed| {
            refreshed.state.heap_bytes(seen);
            (
                refreshed.replaced.heap_bytes(seen),
                refreshed.removed.heap_bytes(seen) + refreshed.dead_code.heap_bytes(seen),
            )
        });
        Some(vec![
            ("published", published.callgraph),
            ("changes_extracted", extracted),
            ("pending_and_unparsed", pending),
            ("served_graph", served),
            ("refreshed_replaced", replaced),
            ("refreshed_other", other),
        ])
    }

    /// What this checkout holds in memory now, for the memory census (see
    /// [`CheckoutMemory`]). `seen` carries the allocations already counted
    /// elsewhere. Never waits: a part whose lock is held is left out and
    /// counted in `seen` as contended.
    pub fn memory(&self, seen: &mut aft_store::heap::Seen) -> CheckoutMemory {
        use aft_store::heap::HeapBytes as _;
        let to_u64 = |bytes: usize| u64::try_from(bytes).unwrap_or(u64::MAX);
        let live = self.live.try_read().ok().map(|live| {
            let mut parts = MemoryParts::default();
            if let Some(published) = &live.published {
                parts.published(published, seen);
            }
            let text_overlay = live.text.heap_bytes(seen);
            parts.text += text_overlay + live.text_members.heap_bytes(seen);
            parts.shared += live.state.heap_bytes(seen)
                + live.early.heap_bytes(seen)
                + live.failure.as_ref().map_or(0, |failure| {
                    failure.error.heap_bytes(seen) + failure.refusal.heap_bytes(seen)
                });
            // A changed path's record is the checkout's; what the call graph
            // reads from it (its parse, or a configuration file's bytes) is
            // the call graph's.
            parts.shared += aft_store::heap::btree_bytes(
                live.changes.len(),
                std::mem::size_of::<Vec<u8>>() + std::mem::size_of::<Change>(),
            );
            for (path, change) in &live.changes {
                parts.shared += path.heap_bytes(seen) + change.record.heap_bytes(seen);
                parts.callgraph += change.content.heap_bytes(seen);
            }
            parts.callgraph += live.unparsed.heap_bytes(seen)
                + live.callgraph_pending.heap_bytes(seen)
                + live.graph.heap_bytes(seen);
            if let Some(refreshed) = &live.refreshed {
                if seen.first(Arc::as_ptr(refreshed)) {
                    parts.shared += aft_store::heap::COUNTS + std::mem::size_of::<Refreshed>();
                    parts.refreshed(refreshed, seen);
                }
            }
            let published = live.published.as_ref();
            // The memo of the call graph views serve now.
            let dead_code_files = match &live.refreshed {
                Some(refreshed) => refreshed.dead_code.try_files(),
                None => published.and_then(|published| published.dead_code.try_files()),
            };
            LiveMemory {
                state_paths: published.map_or(0, |published| published.state.paths().len()),
                shared_bytes: to_u64(parts.shared),
                text_bytes: to_u64(parts.text),
                text_overlay_paths: live.text.len(),
                text_overlay_bytes: to_u64(text_overlay),
                callgraph_bytes: to_u64(parts.callgraph),
                changes: live.changes.len(),
                pending_callgraph_paths: live.callgraph_pending.len(),
                refreshed_files: live.refreshed.as_ref().map_or(0, |refreshed| {
                    refreshed.replaced.len() + refreshed.removed.len()
                }),
                dead_code_files,
            }
        });
        let engine = match self.engine.try_lock() {
            Ok(engine) => match engine.as_ref() {
                Some(engine) => {
                    let GraphEngine {
                        published,
                        live: resolution,
                        applied,
                        overlay,
                        refreshed,
                        measure_after_publication: _,
                    } = engine;
                    // The engine started from the publication the checkout
                    // serves, counted above, unless a newer one replaced it
                    // since; an older one it still holds is the engine's.
                    let mut parts = MemoryParts::default();
                    parts.published(published, seen);
                    let (state, rest) = resolution.memory(seen);
                    parts.shared += state;
                    parts.callgraph += rest + applied.heap_bytes(seen) + overlay.heap_bytes(seen);
                    parts.refreshed(refreshed, seen);
                    EngineMemory::Running {
                        parses: resolution.inputs.parses.len(),
                        resolutions: resolution.resolved.len(),
                        state_bytes: to_u64(parts.shared),
                        bytes: to_u64(parts.text + parts.callgraph),
                    }
                }
                None => EngineMemory::NotStarted,
            },
            Err(_) => EngineMemory::Busy,
        };
        CheckoutMemory { live, engine }
    }

    /// The call-graph engine's memory part by part, for finding what an
    /// update holds; `None` while it is not started or busy. The engine's
    /// own parts come first, so the refreshed files and the overlay show
    /// only what they hold besides the engine's copies.
    #[must_use]
    pub fn engine_memory_parts(&self) -> Option<Vec<(&'static str, usize)>> {
        use aft_store::heap::HeapBytes as _;
        let engine = self.engine.try_lock().ok()?;
        let GraphEngine {
            published,
            live: resolution,
            applied,
            overlay,
            refreshed,
            measure_after_publication: _,
        } = engine.as_ref()?;
        let mut seen = aft_store::heap::Seen::new();
        let mut parts = resolution.memory_parts(&mut seen);
        let mut published_parts = MemoryParts::default();
        published_parts.published(published, &mut seen);
        parts.push((
            "published",
            published_parts.shared + published_parts.text + published_parts.callgraph,
        ));
        parts.push((
            "refreshed_replaced",
            refreshed.replaced.heap_bytes(&mut seen),
        ));
        parts.push(("refreshed_other", {
            let Refreshed {
                state,
                replaced: _,
                removed,
                records,
                dead_code,
            } = refreshed;
            state.heap_bytes(&mut seen)
                + removed.heap_bytes(&mut seen)
                + records.heap_bytes(&mut seen)
                + dead_code.heap_bytes(&mut seen)
        }));
        parts.push(("overlay", overlay.heap_bytes(&mut seen)));
        parts.push(("applied", applied.heap_bytes(&mut seen)));
        Some(parts)
    }
}

/// The vector plane's access to the store: it publishes vector segments of
/// its own, in generations that change no checkout's state.
impl Checkout {
    /// The family store and the block cache every segment read shares, for
    /// [`super::vectors`], which reads vector segments from the newest
    /// generation and publishes through the family's writer.
    pub(crate) fn vector_parts(&self) -> super::vectors::VectorParts<'_> {
        super::vectors::VectorParts {
            family: &self.family,
            cache: &self.layer.cache,
            root: &self.root,
            known: known_sections(),
        }
    }
}

/// The checkout's state as of a view, for planes that read every member.
impl CheckoutView {
    /// The checkout's state when the view was taken: the publication's
    /// ([`state`](Self::state)) with every change recorded since applied, so
    /// a path changed since holds its new content.
    #[must_use]
    pub fn current_state(&self) -> &CheckoutState {
        self.current.as_deref().unwrap_or(&self.published.state)
    }

    /// Whether a rescan against the files on disk was pending when the view
    /// was taken. Until it ends, [`current_state`](Self::current_state) can
    /// miss changes made while the checkout was unbound, so a reader that
    /// needs every current path asks the disk instead.
    #[must_use]
    pub fn rescanning(&self) -> bool {
        self.rescanning
    }
}

/// Outline's symbols for one file, from its stored content record.
impl CheckoutView {
    /// The record of the file at `path` (relative to the root,
    /// `/`-separated) in the view's current state, when the file has not
    /// changed since the publication and is in a parsed language: the files
    /// whose published records [`outline_symbols`](Self::outline_symbols)
    /// reads.
    pub(crate) fn unchanged_record(&self, path: &[u8]) -> Option<&aft_store::checkout::PathRecord> {
        let published = self.published.state.get(path)?;
        let current = self.current_state().get(path)?;
        (current.content == published.content
            && current.language == published.language
            && super::content::is_parsed_language(&published.language))
        .then_some(current)
    }

    /// The symbols outline lists for the file at `path` (relative to the
    /// root, `/`-separated), as the record of its published content holds
    /// them, with that content's key. `None` when the path
    /// changed since the publication, is not parsed, or its record holds no
    /// outline. The file on disk may still have moved on: compare its bytes
    /// with the key before using the symbols.
    ///
    /// # Errors
    ///
    /// When the record cannot be read.
    pub(crate) fn outline_symbols(
        &self,
        path: &[u8],
    ) -> Result<Option<super::outline::StoredOutline>, DataError> {
        let Some(published) = self.unchanged_record(path) else {
            return Ok(None);
        };
        let key = RecordKey::new(published.content, &published.language, EXTRACTOR_VERSION);
        for segment in self.published.segments.iter().rev() {
            let Some(content) = ContentSegment::open(segment).map_err(DataError::store)? else {
                continue;
            };
            let Some(ordinal) = content.find(key).map_err(DataError::store)? else {
                continue;
            };
            #[cfg(test)]
            super::outline::note_record_read();
            let record = content.record(ordinal).map_err(DataError::store)?;
            let RecordBody::Parsed { body, .. } = record.data else {
                return Ok(None);
            };
            return Ok(body.outline.map(|symbols| super::outline::StoredOutline {
                content: published.content,
                symbols,
            }));
        }
        Ok(None)
    }
}

/// One file's TODO markers as a checkout holds them, for the TODO count.
#[derive(Debug)]
pub(crate) enum PathTodos {
    /// The path is not one of the view's files.
    Absent,
    /// Larger than the count reads.
    TooLarge,
    /// The file has no record: the store neither parses it nor counts its
    /// comments (it is binary, or a C header that is not UTF-8).
    Unrecorded,
    /// Its record, with the TODO markers it holds, if any.
    Recorded(Option<aft_store::content::TodoMarkers>),
    /// The store could not extract the file, so it has no record.
    Unextracted,
    /// Its record is not there to read: the view's state names a record a
    /// build of this extractor version has not written yet, or the file
    /// changed again since the view was taken.
    Missing,
}

impl Checkout {
    /// The TODO markers of the files of `view` at `paths` (relative to the
    /// root, `/`-separated), from the records the view's state names: the
    /// publication's for a file unchanged since it, and for a changed file
    /// the record its change extracted. A changed file not extracted yet is
    /// extracted now, once for the change, as a call-graph refresh would;
    /// nothing else is parsed. Files larger than `max_len` are not read.
    ///
    /// # Errors
    ///
    /// When a record cannot be read.
    pub(crate) fn todo_markers(
        &self,
        view: &CheckoutView,
        paths: &[Vec<u8>],
        max_len: u64,
    ) -> Result<Vec<PathTodos>, DataError> {
        // Each changed path's extraction, taken under the lock and run
        // outside it.
        let changes = {
            let live = self.live.read().expect("no panics while holding it");
            paths
                .iter()
                .filter_map(|path| {
                    let change = live.changes.get(path)?;
                    let record = change.record.as_ref()?;
                    Some((
                        path.as_slice(),
                        (record.content, Arc::clone(&change.content)),
                    ))
                })
                .collect::<HashMap<_, _>>()
        };
        // Newest first, as a reader of the publication finds a record.
        let contents = view
            .published
            .segments
            .iter()
            .rev()
            .filter_map(|segment| ContentSegment::open(segment).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(DataError::store)?;
        paths
            .iter()
            .map(|path| {
                let Some(record) = view.current_state().get(path) else {
                    return Ok(PathTodos::Absent);
                };
                if record.stat.size > max_len {
                    return Ok(PathTodos::TooLarge);
                }
                if record.language.is_empty() {
                    return Ok(PathTodos::Unrecorded);
                }
                let published = view.published.state.get(path).is_some_and(|published| {
                    published.content == record.content && published.language == record.language
                });
                if published {
                    if view.unparsed.contains(path) {
                        return Ok(PathTodos::Unextracted);
                    }
                    let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
                    for content in &contents {
                        if let Some(ordinal) = content.find(key).map_err(DataError::store)? {
                            let todos = content.todos(ordinal).map_err(DataError::store)?;
                            return Ok(PathTodos::Recorded(todos));
                        }
                    }
                    return Ok(PathTodos::Missing);
                }
                let Some((content, change)) = changes.get(path.as_slice()) else {
                    return Ok(PathTodos::Missing);
                };
                if *content != record.content {
                    return Ok(PathTodos::Missing);
                }
                Ok(match change.get() {
                    Extracted::Parse(_, _, stored)
                    | Extracted::Config(_, stored)
                    | Extracted::Markers(stored) => {
                        PathTodos::Recorded(stored.todos().map_err(DataError::store)?)
                    }
                    Extracted::Nothing => PathTodos::Unextracted,
                })
            })
            .collect()
    }
}

#[cfg(test)]
fn call_graph_holds_for_test() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static HOLDS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

/// Holds every build of the checkout at `root` after its text index
/// publishes and before its call graph is built, until the returned guard
/// drops, so a test can look at a checkout whose text serves while its call
/// graph is building.
#[cfg(test)]
pub(crate) fn hold_call_graph_for_test(root: &Path) -> CallGraphHold {
    call_graph_holds_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf());
    CallGraphHold(root.to_path_buf())
}

/// See [`hold_call_graph_for_test`].
#[cfg(test)]
pub(crate) struct CallGraphHold(PathBuf);

#[cfg(test)]
impl Drop for CallGraphHold {
    fn drop(&mut self) {
        call_graph_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Roots whose builds wait before they publish, each with whether a build
/// is waiting now.
#[cfg(test)]
fn publication_holds_for_test() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static HOLDS: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    HOLDS.get_or_init(Default::default)
}

/// Holds every build of the checkout at `root` once it has scanned the
/// files and resolved its call graph, before it publishes, until the
/// returned guard drops, so a test can record and resolve changes while a
/// build that did not read them runs.
#[cfg(test)]
pub(crate) fn hold_build_publication_for_test(root: &Path) -> PublicationHold {
    publication_holds_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), false);
    PublicationHold(root.to_path_buf())
}

/// See [`hold_build_publication_for_test`].
#[cfg(test)]
pub(crate) struct PublicationHold(PathBuf);

#[cfg(test)]
impl PublicationHold {
    /// Waits until a build of the checkout is held before publishing.
    pub(crate) fn wait_until_held(&self) {
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !publication_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.0)
            .copied()
            .unwrap_or(false)
        {
            assert!(
                Instant::now() < deadline,
                "no build reached its publication"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
impl Drop for PublicationHold {
    fn drop(&mut self) {
        publication_holds_for_test()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// Waits while [`hold_build_publication_for_test`] holds `root`'s builds.
#[cfg(test)]
fn wait_while_publication_held_for_test(root: &Path) {
    loop {
        {
            let mut holds = publication_holds_for_test()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match holds.get_mut(root) {
                Some(held) => *held = true,
                None => return,
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft_store::text::plan_pattern;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn callers_of(view: &CheckoutView, file: &str, symbol: &str) -> String {
        let callers = crate::commands::callgraph_store_adapter::callers_result(
            &view.callgraph(),
            &view.root().join(file),
            symbol,
            1,
            true,
        );
        format!("{callers:?}")
    }

    /// `call_tree` and `trace_to_symbol` follow a call written inside a
    /// `macro_rules!` template from each use of the macro, matched by name
    /// and marked approximate, as `callers` lists it. A name two symbols
    /// share stays unresolved.
    #[test]
    fn forward_queries_follow_macro_template_calls_by_name() {
        use crate::commands::callgraph_store_adapter as adapter;
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "Cargo.toml",
            "[package]\nname = \"m\"\nversion = \"0.1.0\"\n",
        );
        write(
            root.path(),
            "src/lib.rs",
            "pub mod other;\npub fn template_target() {}\npub fn shared() {}\n\
             macro_rules! run_template { () => { template_target(); shared() }; }\n\
             pub fn template_caller() { run_template!(); }\n",
        );
        write(root.path(), "src/other.rs", "pub fn shared() {}\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        let graph = view.callgraph();
        let lib = view.root().join("src/lib.rs");

        let tree = adapter::call_tree_result(&graph, &lib, "template_caller", 3, true).unwrap();
        let [invocation] = tree.children.as_slice() else {
            panic!("{tree:?}");
        };
        assert_eq!(
            (invocation.name.as_str(), invocation.resolved),
            ("run_template!", true)
        );
        let template_calls = invocation
            .children
            .iter()
            .map(|call| {
                (
                    call.name.as_str(),
                    call.resolved,
                    call.approximate,
                    call.resolved_by.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            template_calls,
            [
                ("template_target", true, Some(true), Some("name_match")),
                ("shared", false, None, None),
            ],
            "{tree:?}"
        );

        let traced = adapter::trace_to_symbol_result(
            &graph,
            &lib,
            "template_caller",
            "template_target",
            None,
            5,
            true,
        )
        .unwrap();
        let hops = traced
            .path
            .unwrap_or_default()
            .into_iter()
            .map(|hop| (hop.symbol, hop.approximate))
            .collect::<Vec<_>>();
        assert_eq!(
            hops,
            [
                ("template_caller".to_string(), None),
                ("run_template!".to_string(), None),
                ("template_target".to_string(), Some(true)),
            ]
        );
    }

    /// Rust calls to free functions through module paths: a full `crate::`
    /// path into a `pub(crate) mod`, a module declared with `#[path]`, and a
    /// module imported with `{self, ...}`. Today's legacy store resolves all
    /// three; the views resolver was found to miss them.
    #[test]
    fn rust_calls_through_module_paths_have_callers() {
        // AFT's own crate has a binary root beside its library root, a
        // library name that differs from the package name, and sits in a
        // workspace; each layout is checked, so a miss names its cause.
        let mut failures = Vec::new();
        for workspace in [false, true] {
            for lib_name in [false, true] {
                for binary in [false, true] {
                    let missing = module_path_callers_missing(workspace, lib_name, binary);
                    if !missing.is_empty() {
                        failures.push(format!(
                            "workspace {workspace}, lib name {lib_name}, binary {binary}: {missing:?}"
                        ));
                    }
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    fn module_path_callers_missing(workspace: bool, lib_name: bool, binary: bool) -> Vec<String> {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let top = root.path();
        let crate_dir = if workspace {
            write(
                top,
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/paths\"]\n",
            );
            top.join("crates/paths")
        } else {
            top.to_path_buf()
        };
        let r = crate_dir.as_path();
        let mut manifest = String::from("[package]\nname = \"paths-package\"\n");
        if lib_name {
            manifest.push_str("\n[lib]\nname = \"paths\"\npath = \"src/lib.rs\"\n");
        }
        if binary {
            manifest.push_str("\n[[bin]]\nname = \"paths\"\npath = \"src/main.rs\"\n");
            write(r, "src/main.rs", "mod cli;\nfn main() { cli::run(); }\n");
            write(r, "src/cli.rs", "pub fn run() {}\n");
        }
        write(r, "Cargo.toml", &manifest);
        write(
            r,
            "src/lib.rs",
            "pub mod outer;\npub mod access;\npub mod graph;\npub mod user;\n",
        );
        write(r, "src/outer/mod.rs", "pub(crate) mod runtime;\n");
        write(
            r,
            "src/outer/runtime.rs",
            "pub fn enabled() -> bool { true }\n",
        );
        write(
            r,
            "src/access.rs",
            "#[path = \"access_plugin.rs\"]\npub(crate) mod plugin;\n",
        );
        write(r, "src/access_plugin.rs", "pub fn issue() {}\n");
        write(r, "src/graph.rs", "pub fn aliases() {}\npub struct Edge;\n");
        write(
            r,
            "src/user.rs",
            "use crate::graph::{self, Edge};\n\
             pub struct Context;\n\
             impl Context {\n\
             \x20   pub fn user(&self) {\n\
             \x20       if crate::outer::runtime::enabled() {\n\
             \x20           crate::access::plugin::issue();\n\
             \x20       }\n\
             \x20       graph::aliases();\n\
             \x20       let _ = Edge;\n\
             \x20   }\n\
             }\n",
        );
        let checkout = Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 2)),
            store.path(),
            family(),
            top,
        )
        .unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        let prefix = if workspace { "crates/paths/" } else { "" };
        let mut missing = Vec::new();
        for (file, symbol) in [
            ("src/outer/runtime.rs", "enabled"),
            ("src/access_plugin.rs", "issue"),
            ("src/graph.rs", "aliases"),
        ] {
            let callers = callers_of(&view, &format!("{prefix}{file}"), symbol);
            if !callers.contains(&format!("{prefix}src/user.rs")) {
                missing.push(format!("{file}::{symbol}: {callers}"));
            }
        }
        missing
    }

    fn candidates_for(view: &CheckoutView, pattern: &str) -> Vec<String> {
        let query = plan_pattern(pattern, false).unwrap();
        view.text_candidates(&query, Scope::All)
            .unwrap()
            .into_iter()
            .map(|path| String::from_utf8(path).unwrap())
            .collect()
    }

    /// A small Rust crate whose `caller` calls `util::callee`.
    /// The census measures what a checkout holds rather than guessing it:
    /// walking a copy of its state, a decoded call-graph slice and a decoded
    /// parse adds up what the allocator handed out to make that copy. A
    /// copy of a state shares the original's records, and the census counts
    /// an allocation reached twice once, so walked after the original the
    /// copy counts exactly what copying it allocated. A state and a parse
    /// are vectors and strings, so the two agree to the byte; a slice's
    /// B-tree maps are counted as full nodes, a lower bound on what a copy
    /// allocates.
    #[test]
    fn the_census_walk_adds_up_what_the_allocator_gave_a_copy() {
        use aft_store::heap::{heap_bytes_of, HeapBytes as _};
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();

        let state = view.state().clone();
        let (copy, allocated) = crate::test_allocations::count_bytes(|| state.clone());
        assert!(allocated > 0);
        let mut seen = aft_store::heap::Seen::new();
        let whole = state.heap_bytes(&mut seen);
        assert_eq!(copy.heap_bytes(&mut seen), allocated);
        assert!(
            allocated < whole,
            "a copy of {whole} bytes allocated {allocated}"
        );

        let graph = Arc::clone(&view.published.graph);
        let slice = graph.slice("src/lib.rs").expect("a stored slice");
        let (copy, allocated) = crate::test_allocations::count_bytes(|| {
            crate::data_layer::slice::FileSlice::clone(&slice)
        });
        let walked = heap_bytes_of(&copy);
        assert!(
            walked <= allocated && 2 * walked > allocated,
            "walked {walked}, allocated {allocated}"
        );

        let (parses, _) = graph.resolutions().unwrap();
        let parse = Arc::clone(&parses["src/lib.rs"].1);
        let (copy, allocated) = crate::test_allocations::count_bytes(|| ParseBlob::clone(&parse));
        assert!(allocated > 0);
        assert_eq!(heap_bytes_of(&copy), allocated);

        // A walk of the whole checkout counts each part once: the slice
        // decoded above is in the call graph's bytes, and walking again with
        // everything already seen adds nothing.
        let mut seen = aft_store::heap::Seen::new();
        let memory = checkout.memory(&mut seen);
        let live = memory.live.unwrap();
        assert!(live.shared_bytes > 0 && live.text_bytes > 0 && live.callgraph_bytes > 0);
        assert_eq!(seen.contended(), 0);
        let again = checkout.memory(&mut seen).live.unwrap();
        assert_eq!(
            (again.shared_bytes, again.text_bytes, again.callgraph_bytes),
            (0, 0, 0)
        );
    }

    fn demo(root: &Path) {
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            root,
            "src/lib.rs",
            "mod util;\npub fn caller() { util::callee(); }\n",
        );
        write(root, "src/util.rs", "pub fn callee() {}\n");
        write(root, "notes.txt", "nothing to see\n");
    }

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"demo"))
    }

    #[test]
    fn a_source_file_past_the_text_index_limit_still_has_its_calls() {
        // AFT's own context.rs and callgraph_store/mod.rs are over 1 MiB; a
        // scan that never read them left every call they make out of the
        // graph, and their callees looked dead.
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let padding = "// padding\n".repeat(120_000);
        write(
            root.path(),
            "src/big.rs",
            &format!("pub fn big_caller() {{ crate::util::callee(); }}\n{padding}"),
        );
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod big;\npub fn caller() { util::callee(); }\n",
        );
        assert!(
            std::fs::metadata(root.path().join("src/big.rs"))
                .unwrap()
                .len()
                > 1 << 20
        );
        let checkout = Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 2)),
            store.path(),
            family(),
            root.path(),
        )
        .unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        let callers = callers_of(&view, "src/util.rs", "callee");
        assert!(callers.contains("src/big.rs"), "{callers}");
        assert!(callers.contains("src/lib.rs"), "{callers}");
        // Its text is not indexed, so text queries read it directly.
        assert!(candidates_for(&view, "big_caller").contains(&"src/big.rs".to_string()));
        assert_eq!(view.status().unparsed, 0);

        // An edit to it is parsed too.
        write(
            root.path(),
            "src/big.rs",
            &format!("pub fn big_caller() {{}}\n{padding}"),
        );
        checkout.record_paths(&[b"src/big.rs".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(!callers.contains("src/big.rs"), "{callers}");
        assert!(callers.contains("src/lib.rs"), "{callers}");
    }

    #[test]
    fn a_first_build_serves_text_before_the_call_graph() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        assert!(checkout.publish_text_first().unwrap());
        let view = checkout.view().unwrap();
        let status = view.status();
        assert_eq!(status.text, PlaneStatus::Ready);
        assert_eq!(
            status.callgraph,
            PlaneStatus::Building {
                done: 0,
                total: None
            }
        );
        // No path counts as failing extraction while nothing was extracted.
        assert_eq!(status.unparsed, 0);
        assert_eq!(
            candidates_for(&view, "callee"),
            ["src/lib.rs", "src/util.rs"]
        );
        // An edit is served to text queries at once, and nothing resolves it
        // while there is no call graph.
        write(
            root.path(),
            "src/util.rs",
            "pub fn callee() {}
pub fn other() { callee(); }
",
        );
        checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
        assert!(checkout.refresh_callgraph().unwrap().is_none());
        assert_eq!(
            candidates_for(&checkout.view().unwrap(), "other"),
            ["src/util.rs"]
        );

        // The build then publishes the call graph, covering the edit.
        let report = checkout.build().unwrap();
        assert!(
            report.text_ready_ms.is_none(),
            "a checkout with a publication is rebuilt in one pass"
        );
        let view = checkout.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        let callers = callers_of(&view, "src/util.rs", "callee");
        assert!(callers.contains("src/lib.rs"), "{callers}");
        assert!(callers.contains("other"), "{callers}");

        // A fresh checkout's build reports when its text was ready.
        let fresh_store = tempfile::tempdir().unwrap();
        let fresh = Checkout::bind(layer, fresh_store.path(), family(), root.path()).unwrap();
        let report = fresh.build().unwrap();
        assert!(report.text_ready_ms.is_some(), "{report:?}");
        assert_eq!(fresh.view().unwrap().status().callgraph, PlaneStatus::Ready);
    }

    /// A build that publishes while a recording scans: the recording's
    /// state came from the older publication, so it must not be applied;
    /// the recording scans again from the new one.
    #[test]
    fn a_recording_that_straddles_a_publication_scans_again() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "main.ts",
            "import { helper } from './extra';\nexport function main() { helper(); }\n",
        );
        write(root.path(), "notes.txt", "nothing\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        write(root.path(), "extra.ts", "export function helper() {}\n");
        write(root.path(), "notes.txt", "changed\n");
        // A recording of notes.txt reads its base and scans...
        let (base, generation) = {
            let live = checkout.live.read().unwrap();
            (
                Arc::new(live.published.as_ref().unwrap().state.clone()),
                live.generation,
            )
        };
        let sink = ChangeSink::default();
        let scanned = scan_paths(
            &checkout.root,
            &checkout.layer.policy,
            &base,
            &[b"notes.txt".to_vec()],
            &sink,
        )
        .unwrap();
        // ...a build publishes, with extra.ts...
        checkout.build().unwrap();
        assert!(checkout.view().unwrap().state().get(b"extra.ts").is_some());
        // ...and the recording's scan, from the older base, is refused.
        assert!(!checkout.apply_scan(
            &base,
            generation,
            scanned.state,
            &scanned.changed,
            sink.finish()
        ));
        checkout.record_paths(&[b"notes.txt".to_vec()]).unwrap();
        let state = checkout.live.read().unwrap().state.clone();
        assert!(state.is_none_or(|state| state.get(b"extra.ts").is_some()));
        // An ordinary edit then resolves over the new publication's files.
        write(
            root.path(),
            "main.ts",
            "import { helper } from './extra';\nexport function main() { helper(); }\nexport function more() {}\n",
        );
        checkout.record_paths(&[b"main.ts".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        let ours = callers_of(&checkout.view().unwrap(), "extra.ts", "helper");
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(layer, cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        let theirs = callers_of(&cold.view().unwrap(), "extra.ts", "helper");
        assert!(theirs.contains("main"), "{theirs}");
        assert_eq!(ours, theirs);
    }

    /// A stored resolution that does not decode, met by the method-call
    /// pass of an update: the refresh fails as damage, every later one does
    /// too (none panics on a poisoned lock), and a build replaces it.
    #[test]
    fn a_damaged_stored_resolution_fails_the_refresh_until_a_build_replaces_it() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(root.path(), "go.mod", "module example.com/m\n");
        write(
            root.path(),
            "p/t.go",
            "package p\n\ntype T struct{}\n\nfunc (T) Run() {}\n",
        );
        write(
            root.path(),
            "p/use.go",
            "package p\n\nfunc Use(t T) { t.Run(); t.Stop() }\n",
        );
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        super::super::graph_store::DAMAGED_FRAGMENT
            .with(|damaged| *damaged.borrow_mut() = Some("p/use.go".to_string()));
        checkout.build().unwrap();
        super::super::graph_store::DAMAGED_FRAGMENT.with(|damaged| *damaged.borrow_mut() = None);
        write(
            root.path(),
            "p/stop.go",
            "package p\n\nfunc (T) Stop() {}\n",
        );
        checkout.record_paths(&[b"p/stop.go".to_vec()]).unwrap();
        // Exercise the update without refresh's last-resort panic catcher:
        // malformed stored data must return an error, not unwind a worker.
        let (published, state, changes) = {
            let live = checkout.live.read().unwrap();
            (
                live.published.clone().unwrap(),
                live.state.clone().unwrap(),
                live.changes.clone(),
            )
        };
        assert!(matches!(
            checkout.update_engine(&mut None, &published, &state, &changes),
            Err(DataError::Damaged(_))
        ));
        for attempt in 0..2 {
            let result = checkout.refresh_callgraph();
            assert!(
                matches!(result, Err(DataError::Damaged(_))),
                "refresh {attempt}: {:?}",
                result.map(|stats| stats.map(|stats| stats.touched))
            );
            assert!(checkout.status().failed);
            assert!(checkout.failure().is_some());
        }
        // A build reads every file again and replaces the damaged fragment.
        checkout.build().unwrap();
        assert!(!checkout.status().failed);
        let view = checkout.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        assert!(view.callgraph_failures().is_empty());
        let callers = callers_of(&view, "p/stop.go", "Stop");
        assert!(callers.contains("p/use.go"), "{callers}");
    }

    #[test]
    fn a_damaged_stored_parse_fails_publication_without_committing_or_panicking() {
        use aft_store::content::ContentSegment;

        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        // One cached block cannot retain the parse body while its surface is
        // read. Publication therefore reads the changed disk bytes below.
        let layer = Arc::new(DataLayer::new(64 << 10, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        edit(&checkout, "src/util.rs", "pub fn replacement() {}\n");
        checkout.refresh_callgraph().unwrap().expect("applied");
        {
            let mut slot = checkout.engine.lock().unwrap();
            let engine = slot.as_mut().unwrap();
            assert!(engine.refreshed.replaced.contains_key("src/lib.rs"));
            engine.live.inputs.parses.release_decoded();
            assert_eq!(engine.live.inputs.parses.decoded_count(), 0);
        }
        let published = checkout.live.read().unwrap().published.clone().unwrap();
        assert!(published.graph.failures().is_empty());
        let path = published
            .state
            .paths()
            .iter()
            .find(|path| path.path == b"src/lib.rs")
            .unwrap();
        let key = RecordKey::new(path.content, &path.language, EXTRACTOR_VERSION);
        let mut damaged = false;
        for segment in &published.segments {
            let Some(content) = ContentSegment::open(segment).unwrap() else {
                continue;
            };
            let Some(ordinal) = content.find(key).unwrap() else {
                continue;
            };
            let body = content.body(ordinal).unwrap();
            assert!(!body.is_empty());
            let mut bytes = std::fs::read(segment.path()).unwrap();
            let offsets = bytes
                .windows(body.len())
                .enumerate()
                .filter_map(|(offset, bytes)| (bytes == body.as_slice()).then_some(offset))
                .collect::<Vec<_>>();
            assert_eq!(offsets.len(), 1, "the body must have one location on disk");
            bytes[offsets[0] + body.len() / 2] ^= 0x40;
            std::fs::write(segment.path(), bytes).unwrap();
            // Evict the old cached body without decoding the damaged parse.
            content.surface(ordinal).unwrap();
            damaged = true;
            break;
        }
        assert!(damaged, "the stored parse was found");
        let before = checkout
            .family
            .snapshot()
            .unwrap()
            .manifest()
            .checkout(checkout.checkout)
            .cloned();
        // No panic catcher: a panic from publication fails this test directly.
        for _ in 0..2 {
            let result = checkout.publish_changes();
            assert!(
                matches!(result, Err(DataError::Damaged(ref why)) if why.contains("src/lib.rs")),
                "{result:?}"
            );
        }
        assert!(!published.graph.failures().is_empty());
        assert_eq!(
            checkout
                .family
                .snapshot()
                .unwrap()
                .manifest()
                .checkout(checkout.checkout),
            before.as_ref()
        );
        assert_eq!(checkout.changed_paths(), 1, "the edit was not committed");
        drop(published);
        checkout.build().unwrap();
        assert!(checkout.view().unwrap().callgraph_failures().is_empty());
        edit(
            &checkout,
            "src/util.rs",
            "pub fn replacement() {}\npub fn callee() {}\n",
        );
        checkout
            .refresh_callgraph()
            .unwrap()
            .expect("refreshed after rebuilding");
        checkout
            .publish_changes()
            .unwrap()
            .expect("published after rebuilding");
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(callers.contains("caller"), "{callers}");
    }

    /// Publishes the older string-table representation with keys from another
    /// extractor, without running another binary or sharing process globals.
    fn publish_legacy_graph(checkout: &Checkout, stamp: Option<&str>) {
        use crate::callgraph_store::join::CallgraphBlob;
        use aft_store::content::ContentSectionsBuilder;
        let version = "ruled-callgraph-v14-resolver-precision+record-12";
        let sink = BuildSink::default();
        let scanned = scan(&checkout.root, &checkout.layer.policy, None, &sink).unwrap();
        let collected = sink.finish();
        let mut state = scanned.state;
        let mut parses = BTreeMap::new();
        let mut configs = HashMap::new();
        let mut records = HashMap::new();
        let mut encoded = ContentSectionsBuilder::default();
        for index in 0..state.paths().len() {
            let path = state.paths()[index].path.clone();
            let Some(language) = collected.languages.get(&path) else {
                continue;
            };
            state.set_language(index, language.clone()).unwrap();
            let content = state.paths()[index].content;
            let current = RecordKey::new(content, language, EXTRACTOR_VERSION);
            let mut record = (*collected.records[&current.digest().to_string()].1).clone();
            record.extractor_version = version.to_string();
            let key = RecordKey::new(content, language, version);
            encoded.add(key, &record);
            match super::super::content::blob_from_record(&record).unwrap() {
                CallgraphBlob::Parse(parse) => {
                    parses.insert(
                        String::from_utf8(path).unwrap(),
                        (key.digest().to_string(), Arc::new(parse)),
                    );
                }
                CallgraphBlob::Config(config) => {
                    configs.insert(path, Arc::from(config.source));
                }
            }
            records.insert(key.digest().to_string(), (key, Arc::new(record)));
        }
        let inputs = CheckoutInputs {
            state,
            parses: parses.into(),
            configs,
        };
        let resolved = super::super::callgraph::resolve_checkout(&inputs, 2).unwrap();
        super::super::graph_store::LEGACY_FRAGMENTS.with(|legacy| legacy.set(true));
        let mut sections = super::super::graph_store::SectionsWriter::new(
            &inputs.state,
            &records,
            true,
            Some(encoded),
        );
        super::super::graph_store::LEGACY_FRAGMENTS.with(|legacy| legacy.set(false));
        for (path, resolution) in &resolved {
            sections.add(&inputs, path, resolution).unwrap();
        }
        let mut writer = checkout.writer().unwrap();
        let mut pending = writer.create_segment().unwrap();
        collected.text.write(pending.writer()).unwrap();
        let fragments = sections.finish(pending.writer(), None).unwrap();
        let segment = writer.finish_segment(pending).unwrap();
        let per_path = inputs
            .state
            .paths()
            .iter()
            .map(|record| {
                std::str::from_utf8(&record.path)
                    .ok()
                    .and_then(|path| fragments.get(path))
                    .map(|key| key.digest())
            })
            .collect();
        let state = match stamp {
            Some(version) => inputs.state.with_fragments_for(per_path, version),
            None => inputs.state.with_fragments(per_path),
        }
        .unwrap();
        let written = writer
            .write_checkout_state(checkout.checkout, &state)
            .unwrap();
        writer
            .publish(&Publication {
                add_segments: vec![segment],
                states: vec![written],
                ..Publication::default()
            })
            .unwrap();
    }

    #[test]
    fn a_legacy_publication_with_current_sibling_records_rebuilds_before_refresh() {
        legacy_restart(None);
    }

    #[test]
    fn a_different_extractor_publication_rebuilds_before_refresh() {
        legacy_restart(Some("ruled-callgraph-v14-resolver-precision+record-12"));
    }

    fn legacy_restart(stamp: Option<&str>) {
        fn answers(view: &CheckoutView) -> (String, String) {
            let callers = callers_of(view, "p/stop.go", "Stop");
            let tree = crate::commands::callgraph_store_adapter::call_tree_result(
                &view.callgraph(),
                &view.root().join("p/use.go"),
                "Use",
                2,
                true,
            )
            .unwrap();
            (callers, format!("{tree:?}"))
        }
        let root = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        for dir in [root.path(), sibling.path()] {
            write(dir, "go.mod", "module example.com/m\n");
            write(
                dir,
                "p/t.go",
                "package p\ntype T struct{}\nfunc (T) Run() {}\n",
            );
            write(
                dir,
                "p/use.go",
                "package p\nfunc Use(t T) { t.Run(); t.Stop() }\n",
            );
        }
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let old = Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        publish_legacy_graph(&old, stamp);
        drop(old);
        let other =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), sibling.path()).unwrap();
        assert!(other.start_from_sibling(Instant::now()).unwrap().is_none());
        other.publish_text_first().unwrap();
        other.build().unwrap();
        // The new records coexist with the old fragments in one family.
        assert!(!other
            .view()
            .unwrap()
            .published
            .graph
            .parsed_files()
            .collect::<Vec<_>>()
            .is_empty());
        write(root.path(), "p/stop.go", "package p\nfunc (T) Stop() {}\n");
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        assert!(checkout.changed_paths() > 0);
        assert!(checkout.refresh_callgraph().unwrap().is_none());
        assert_eq!(checkout.engine_starts(), 0);
        assert!(checkout.needs_build());
        checkout.build().unwrap();
        assert!(!checkout.needs_build());
        let ours = answers(&checkout.view().unwrap());
        assert!(ours.0.contains("p/use.go"), "{ours:?}");
        assert!(ours.1.contains("Stop"), "{ours:?}");
        let cold_store = tempfile::tempdir().unwrap();
        let cold =
            Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        assert_eq!(ours, answers(&cold.view().unwrap()));
        drop(checkout);
        let reopened = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        assert!(!reopened.needs_build());
        assert_eq!(ours, answers(&reopened.view().unwrap()));
    }

    /// Editing an ignore file changes which files are members, as a cold
    /// scan would see them: at the root, and in a folder.
    #[test]
    fn an_ignore_file_edit_changes_members_as_a_cold_scan_would() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        write(root.path(), ".gitignore", "gen/\n");
        write(root.path(), "gen/out.txt", "zzneedle generated\n");
        write(root.path(), "docs/notes.txt", "zzneedle docs\n");
        write(root.path(), "pkg/.aftignore", "");
        write(root.path(), "pkg/a.txt", "zzneedle a\n");
        write(root.path(), "pkg/b.txt", "zzneedle b\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        assert_eq!(
            candidates_for(&checkout.view().unwrap(), "zzneedle"),
            ["docs/notes.txt", "pkg/a.txt", "pkg/b.txt"]
        );
        let cold = |layer: &Arc<DataLayer>| {
            let cold_store = tempfile::tempdir().unwrap();
            let cold = Checkout::bind(Arc::clone(layer), cold_store.path(), family(), root.path())
                .unwrap();
            cold.build().unwrap();
            candidates_for(&cold.view().unwrap(), "zzneedle")
        };
        // At the root: the rule moves from gen/ to docs/.
        write(root.path(), ".gitignore", "docs/\n");
        checkout.record_paths(&[b".gitignore".to_vec()]).unwrap();
        let theirs = cold(&layer);
        assert_eq!(theirs, ["gen/out.txt", "pkg/a.txt", "pkg/b.txt"]);
        assert_eq!(
            candidates_for(&checkout.view().unwrap(), "zzneedle"),
            theirs
        );
        // In a folder: its own ignore file now leaves out one of its files.
        write(root.path(), "pkg/.aftignore", "a.txt\n");
        checkout
            .record_paths(&[b"pkg/.aftignore".to_vec()])
            .unwrap();
        let theirs = cold(&layer);
        assert_eq!(theirs, ["gen/out.txt", "pkg/b.txt"]);
        assert_eq!(
            candidates_for(&checkout.view().unwrap(), "zzneedle"),
            theirs
        );
    }

    /// An ignore file that leaves a caller out takes it out of the call
    /// graph too.
    #[test]
    fn an_ignore_file_edit_removes_an_ignored_caller() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(root.path(), ".aftignore", "");
        write(
            root.path(),
            "main.ts",
            "import { run } from './lib';\nexport function main() { run(); }\n",
        );
        write(root.path(), "lib.ts", "export function run() {}\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        assert!(callers_of(&checkout.view().unwrap(), "lib.ts", "run").contains("main"));
        write(root.path(), ".aftignore", "main.ts\n");
        checkout.record_paths(&[b".aftignore".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        let callers = callers_of(&checkout.view().unwrap(), "lib.ts", "run");
        assert!(!callers.contains("main"), "{callers}");
    }

    /// Two checkouts resolve a file to the same answers through different
    /// symlink chains. Each keeps its own record of the paths it consulted,
    /// so retargeting a link only one of them consulted reaches its file.
    #[cfg(unix)]
    #[test]
    fn checkouts_that_consulted_different_paths_keep_their_own_fragments() {
        use std::os::unix::fs::symlink;
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        for root in [a.path(), b.path()] {
            write(
                root,
                "main.ts",
                "import { run } from './alias/lib';\nexport function main() { run(); }\n",
            );
            write(root, "real/lib.ts", "export function run() { return 1; }\n");
            write(
                root,
                "other/lib.ts",
                "export function run() { return 2; }\n",
            );
            symlink("real", root.join("hop_a")).unwrap();
            symlink("real", root.join("hop_b")).unwrap();
        }
        symlink("hop_a", a.path().join("alias")).unwrap();
        symlink("hop_b", b.path().join("alias")).unwrap();
        let first = Checkout::bind(Arc::clone(&layer), store.path(), family(), a.path()).unwrap();
        first.build().unwrap();
        assert!(callers_of(&first.view().unwrap(), "real/lib.ts", "run").contains("main"));
        drop(first);
        let second = Checkout::bind(Arc::clone(&layer), store.path(), family(), b.path()).unwrap();
        second.build().unwrap();
        drop(second);
        let first = Checkout::bind(Arc::clone(&layer), store.path(), family(), a.path()).unwrap();
        assert!(first.check_stored_inputs().unwrap().is_empty());
        std::fs::remove_file(a.path().join("hop_a")).unwrap();
        symlink("other", a.path().join("hop_a")).unwrap();
        first.record_paths(&[b"hop_a".to_vec()]).unwrap();
        first.refresh_callgraph().unwrap();
        let ours = callers_of(&first.view().unwrap(), "other/lib.ts", "run");
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(layer, cold_store.path(), family(), a.path()).unwrap();
        cold.build().unwrap();
        let theirs = callers_of(&cold.view().unwrap(), "other/lib.ts", "run");
        assert!(theirs.contains("main"), "{theirs}");
        assert_eq!(ours, theirs);
    }

    /// A minified bundle is searched by grep but neither parsed for the
    /// call graph nor left as a gap the call graph does not cover.
    #[test]
    fn a_minified_bundle_is_searched_but_not_parsed() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "web/app.js",
            "import { helper } from './vendor.min.js';\nexport function run() { helper(); }\n",
        );
        write(
            root.path(),
            "web/vendor.min.js",
            "export function helper(){return 1}export function zzbundled(){helper()}\n",
        );
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        assert_eq!(candidates_for(&view, "zzbundled"), ["web/vendor.min.js"]);
        let callers = callers_of(&view, "web/vendor.min.js", "helper");
        assert!(!callers.contains("zzbundled"), "{callers}");
        assert_eq!(view.status().unparsed, 0, "a bundle is not a gap");
    }

    /// A second worktree of a repository, built whole, stores only what the
    /// first one's segment does not already hold, and answers as a cold
    /// build does. It differs from the first in half its files, so it is
    /// built whole rather than started from the first one's publication.
    /// Once the first worktree's folder is gone, the second one's next build
    /// stores everything again and retires the first one's segments.
    #[test]
    fn a_second_worktree_stores_only_what_no_sibling_holds() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(a.path());
        demo(b.path());
        write(
            b.path(),
            "src/util.rs",
            "pub fn callee() {}\npub fn helper() { callee(); }\n",
        );
        write(b.path(), "notes.txt", "something to see\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let first = Checkout::bind(Arc::clone(&layer), store.path(), family(), a.path()).unwrap();
        let built_first = first.build().unwrap();
        assert_eq!(
            built_first.shared,
            SharedUnits::default(),
            "nothing to share yet"
        );

        let second = Checkout::bind(Arc::clone(&layer), store.path(), family(), b.path()).unwrap();
        let built = second.build().unwrap();
        assert_eq!(built.started_from_sibling, None, "{built:?}");
        // `Cargo.toml` and `src/lib.rs` are the same in both worktrees, and
        // so is how `src/lib.rs` resolves; `src/util.rs` and `notes.txt`
        // differ.
        assert_eq!(built.shared.text_units, 2, "{built:?}");
        assert_eq!(built.shared.records, 2, "{built:?}");
        assert_eq!(built.shared.fragments, 1, "{built:?}");
        let view = second.view().unwrap();
        let callers = callers_of(&view, "src/util.rs", "callee");
        assert!(
            callers.contains("caller") && callers.contains("helper"),
            "{callers}"
        );
        assert_eq!(candidates_for(&view, "something to see"), ["notes.txt"]);
        drop(view);

        // As a cold build of the same folder answers.
        let cold_store = tempfile::tempdir().unwrap();
        let cold =
            Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), b.path()).unwrap();
        cold.build().unwrap();
        for (file, symbol) in [("src/util.rs", "callee"), ("src/util.rs", "helper")] {
            assert_eq!(
                callers_of(&second.view().unwrap(), file, symbol),
                callers_of(&cold.view().unwrap(), file, symbol),
                "{file} {symbol}"
            );
        }

        // The first worktree goes: the second one stores everything again,
        // and the first one's segments go with its state.
        drop(first);
        drop(a);
        let rebuilt = second.build().unwrap();
        assert_eq!(rebuilt.shared, SharedUnits::default(), "{rebuilt:?}");
        assert!(rebuilt.retired_segments >= 1, "{rebuilt:?}");
        assert_eq!(rebuilt.retired_checkouts, 1, "{rebuilt:?}");
        assert!(callers_of(&second.view().unwrap(), "src/util.rs", "callee").contains("caller"));
    }

    /// Publishing the changes since a publication: the checkout then
    /// serves the new one with nothing laid over it, answers as a cold
    /// build does, and a restart starts from it with nothing to redo.
    #[test]
    fn published_changes_serve_as_a_cold_build_and_survive_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        write(
            root.path(),
            "src/util.rs",
            "pub fn callee() {}\npub fn other() { callee(); }\n",
        );
        write(root.path(), "notes.txt", "zzneedle\n");
        checkout
            .record_paths(&[b"src/util.rs".to_vec(), b"notes.txt".to_vec()])
            .unwrap();
        // The call graph has not resolved the changes yet.
        assert!(checkout.publish_changes().unwrap().is_none());
        checkout.refresh_callgraph().unwrap();
        let report = checkout.publish_changes().unwrap().expect("published");
        assert_eq!(report.paths, 2);
        assert_eq!(report.segments, 2, "{report:?}");
        assert!(report.fragments >= 1, "{report:?}");
        assert_eq!(checkout.changed_paths(), 0);
        let view = checkout.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        let ours = callers_of(&view, "src/util.rs", "callee");
        assert!(ours.contains("other"), "{ours}");
        assert!(ours.contains("caller"), "{ours}");
        assert_eq!(candidates_for(&view, "zzneedle"), ["notes.txt"]);
        let cold_store = tempfile::tempdir().unwrap();
        let cold =
            Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        assert_eq!(
            ours,
            callers_of(&cold.view().unwrap(), "src/util.rs", "callee")
        );
        drop(view);
        drop(checkout);
        // A restart finds nothing changed since the publication.
        let restarted = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        assert_eq!(restarted.changed_paths(), 0);
        let view = restarted.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        assert_eq!(ours, callers_of(&view, "src/util.rs", "callee"));
        assert_eq!(candidates_for(&view, "zzneedle"), ["notes.txt"]);
    }

    /// Edits `path` in `checkout`'s folder to `text` and records it.
    fn edit(checkout: &Checkout, path: &str, text: &str) {
        write(checkout.root(), path, text);
        checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
    }

    /// A publication of the changes a refresh resolved moves the call-graph
    /// engine onto it: each later edit's refresh goes on with that engine
    /// instead of starting a new one from the publication, and answers as a
    /// cold build of the same files does, recorded reads included.
    #[test]
    fn a_publish_followed_by_an_edit_starts_no_new_engine() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let edits = [
            (
                "src/util.rs",
                "pub fn callee() {}\npub fn other() { callee(); }\n",
            ),
            (
                "src/lib.rs",
                "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
            ),
            (
                "src/extra.rs",
                "use crate::util;\npub fn more() { util::other(); }\n",
            ),
            (
                "src/util.rs",
                "pub fn callee() {}\npub fn other() { callee(); }\npub fn third() { other(); }\n",
            ),
        ];
        for (round, (path, text)) in edits.iter().enumerate() {
            edit(&checkout, path, text);
            assert!(
                checkout.refresh_callgraph().unwrap().is_some(),
                "round {round}"
            );
            assert_eq!(
                checkout.engine_starts(),
                1,
                "round {round}: the refresh started another engine"
            );
            let cold_store = tempfile::tempdir().unwrap();
            let cold = Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path())
                .unwrap();
            cold.build().unwrap();
            for (file, symbol) in [
                ("src/util.rs", "callee"),
                ("src/util.rs", "other"),
                ("src/lib.rs", "caller"),
            ] {
                assert_eq!(
                    callers_of(&checkout.view().unwrap(), file, symbol),
                    callers_of(&cold.view().unwrap(), file, symbol),
                    "round {round}: callers of {file} {symbol}"
                );
            }
            let stale = checkout
                .inspect_engine(LiveCheckout::stale_recordings)
                .unwrap()
                .unwrap();
            assert!(stale.is_empty(), "round {round}: stale {stale:?}");
            checkout.publish_changes().unwrap().expect("published");
            assert!(
                checkout.engine_census_bytes().is_some(),
                "round {round}: the publication dropped the engine"
            );
        }
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "other");
        assert!(
            callers.contains("more") && callers.contains("third"),
            "{callers}"
        );
    }

    #[test]
    fn a_broad_publication_bounds_decoded_parses_and_replaces_its_budget_measurement() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let files = ENGINE_DECODED_FILES + 16;
        let source = (0..80)
            .map(|n| format!("pub fn item_{n}() {{}}\n"))
            .collect::<String>();
        write(
            root.path(),
            "Cargo.toml",
            "[package]\nname='broad'\nversion='0.1.0'\n",
        );
        let modules = (0..files)
            .map(|n| format!("mod file_{n};\n"))
            .collect::<String>();
        write(root.path(), "src/lib.rs", &modules);
        for n in 0..files {
            write(root.path(), &format!("src/file_{n}.rs"), &source);
        }
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        for n in 0..files {
            edit(
                &checkout,
                &format!("src/file_{n}.rs"),
                &format!("{source}\npub fn extra() {{}}\n"),
            );
        }
        checkout.refresh_callgraph().unwrap().expect("applied");
        let before = checkout.engine_budget_bytes();
        assert!(before > 0);
        checkout.publish_changes().unwrap().expect("published");
        let held = checkout.engine_census_bytes().expect("kept");
        assert_eq!(
            checkout.engine_budget_bytes(),
            held,
            "must not keep the overlay's cached size"
        );
        let decoded = checkout
            .inspect_engine(|engine| engine.inputs.parses.decoded_count())
            .unwrap()
            .unwrap();
        assert!(
            decoded <= ENGINE_DECODED_FILES,
            "kept {decoded} decoded parses"
        );
        // Bound the whole retained heap as well as its decoded entry count;
        // the fixture has eighty files with eighty tiny definitions each.
        assert!(held < 8 << 20, "kept {held} bytes after publication");
        assert!(
            held < before,
            "publication did not release overlay state: {before} -> {held}"
        );
        assert_eq!(checkout.engine_starts(), 1);
        edit(
            &checkout,
            "src/file_0.rs",
            &format!("{source}\npub fn later() {{}}\n"),
        );
        checkout
            .refresh_callgraph()
            .unwrap()
            .expect("next edit applied");
        let estimated = checkout.engine_budget_bytes();
        checkout
            .publish_changes()
            .unwrap()
            .expect("published again");
        assert_eq!(
            checkout.engine_budget_bytes(),
            estimated,
            "a small publication inside the measurement interval must not walk again"
        );
        assert_eq!(checkout.engine_starts(), 1);
    }

    /// Waits up to ten seconds for `holds` to turn false.
    fn wait_until_not(mut holds: impl FnMut() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while holds() {
            assert!(Instant::now() < deadline, "still holds after ten seconds");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A kept engine goes once its checkout has gone the idle period with no
    /// edit recorded and no refresh applying anything, and not before: an
    /// edit starts the period again. The next refresh starts a new engine.
    #[test]
    fn an_idle_engine_is_released_after_its_quiet_period() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let idle = std::time::Duration::from_secs(2);
        layer.set_engine_idle(idle);
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        edit(
            &checkout,
            "src/util.rs",
            "pub fn callee() {}\npub fn other() {}\n",
        );
        checkout.refresh_callgraph().unwrap().expect("applied");
        assert!(checkout.engine_census_bytes().is_some());
        std::thread::sleep(idle / 2);
        let edited = Instant::now();
        edit(
            &checkout,
            "src/util.rs",
            "pub fn callee() {}\npub fn third() {}\n",
        );
        std::thread::sleep(idle * 3 / 4);
        // Past the period since the refresh, within it since the edit.
        assert!(edited.elapsed() < idle, "the test machine is too slow");
        assert!(
            checkout.engine_census_bytes().is_some(),
            "the engine went though its checkout was edited within the period"
        );
        let epoch = checkout.memory_epoch();
        wait_until_not(|| checkout.engine_census_bytes().is_some());
        assert!(edited.elapsed() >= idle);
        assert!(checkout.memory_epoch() > epoch, "the census was not told");
        checkout.refresh_callgraph().unwrap().expect("applied");
        assert_eq!(checkout.engine_starts(), 2);
    }

    /// Past the budget the engines of every checkout share, a refresh lets
    /// the least recently used other checkout's engine go, and only as many
    /// as bring them under it: here the one whose checkout went longest
    /// without an edit or a refresh, not the one started first.
    #[test]
    fn over_budget_the_least_recently_used_idle_engine_goes_first() {
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let mut folders = Vec::new();
        let mut checkouts = Vec::new();
        for _ in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let store = tempfile::tempdir().unwrap();
            demo(root.path());
            let checkout =
                Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
            checkout.build().unwrap();
            edit(
                &checkout,
                "src/util.rs",
                "pub fn callee() {}\npub fn other() {}\n",
            );
            checkout.refresh_callgraph().unwrap().expect("applied");
            checkouts.push(checkout);
            folders.push((root, store));
            // Uses are told apart by the millisecond.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let bytes = checkouts
            .iter()
            .map(|checkout| checkout.engine_census_bytes().expect("kept"))
            .collect::<Vec<_>>();
        assert!(bytes.iter().all(|bytes| *bytes > 0), "{bytes:?}");
        // The first checkout is edited again, so the second one's engine is
        // now the least recently used.
        edit(
            &checkouts[0],
            "src/lib.rs",
            "mod util;\npub fn caller() {}\n",
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Room for the first and third engines, and half the second's.
        layer.set_engine_budget(bytes[0] + bytes[2] + bytes[1] / 2);
        edit(
            &checkouts[2],
            "src/util.rs",
            "pub fn callee() {}\npub fn third() {}\n",
        );
        checkouts[2].refresh_callgraph().unwrap().expect("applied");
        let kept = checkouts
            .iter()
            .map(|checkout| checkout.engine_census_bytes().is_some())
            .collect::<Vec<_>>();
        assert_eq!(kept, [true, false, true], "bytes {bytes:?}");
        assert!(checkouts
            .iter()
            .all(|checkout| checkout.engine_starts() == 1));
    }

    /// The live segments of the family store at `checkout`'s.
    fn live_segments(checkout: &Checkout) -> usize {
        checkout.store_segments().unwrap()
    }

    /// A checkout that keeps editing the same file publishes each round of
    /// changes in a segment of its own, which replaces everything the one
    /// before held. That one is read by nothing and leaves the store with a
    /// later publication, with no build in between; it used to stay until
    /// a build, and such a checkout reads too few segments to be built.
    #[test]
    fn publications_of_the_same_files_leave_no_unread_segment_behind() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let mut retired = 0;
        for round in 0..12 {
            write(
                root.path(),
                "src/util.rs",
                &format!("pub fn callee() {{}}\npub fn other{round}() {{ callee(); }}\n"),
            );
            checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
            checkout.refresh_callgraph().unwrap();
            let report = checkout.publish_changes().unwrap().expect("published");
            retired += report.retired_segments;
            // The build's segment and this publication's, and at most the
            // one before it: a unit this publication writes again keeps
            // its older holder for one more publication.
            assert!(
                live_segments(&checkout) <= 3,
                "round {round}: {} segments, {report:?}",
                live_segments(&checkout)
            );
        }
        assert!(retired >= 10, "{retired} segments retired");
        let ours = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(
            ours.contains("other11") && ours.contains("caller"),
            "{ours}"
        );
        let cold_store = tempfile::tempdir().unwrap();
        let cold =
            Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        assert_eq!(
            ours,
            callers_of(&cold.view().unwrap(), "src/util.rs", "callee")
        );
        // A restart serves the last publication.
        drop(checkout);
        let restarted = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        assert_eq!(restarted.changed_paths(), 0);
        assert_eq!(
            ours,
            callers_of(&restarted.view().unwrap(), "src/util.rs", "callee")
        );
    }

    /// A change recorded while a build runs, after its scan, stays laid over
    /// the build's publication. A refresh that resolved it during the build
    /// laid its answer over the old publication's call graph, which the
    /// build's replaces, so the change is pending again until a refresh
    /// resolves it over the new one. The call graph used to report ready
    /// then and answer without the change.
    #[test]
    fn a_change_resolved_while_a_build_ran_is_resolved_again_over_its_publication() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Arc::new(
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap(),
        );
        checkout.build().unwrap();
        let hold = hold_build_publication_for_test(root.path());
        let building = {
            let checkout = Arc::clone(&checkout);
            std::thread::spawn(move || checkout.build())
        };
        hold.wait_until_held();
        // The build has read the files; this change is newer than its scan.
        write(
            root.path(),
            "src/extra.rs",
            "use crate::util;\npub fn other() { util::callee(); }\n",
        );
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
        );
        checkout
            .record_paths(&[b"src/extra.rs".to_vec(), b"src/lib.rs".to_vec()])
            .unwrap();
        checkout.refresh_callgraph().unwrap();
        let view = checkout.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        let resolved = callers_of(&view, "src/util.rs", "callee");
        assert!(resolved.contains("src/extra.rs"), "{resolved}");
        drop(view);

        drop(hold);
        building.join().unwrap().unwrap();
        let status = checkout.status();
        assert!(
            matches!(status.callgraph, PlaneStatus::Building { .. }),
            "the call graph reports ready without the change: {status:?}, callers {}",
            callers_of(&checkout.view().unwrap(), "src/util.rs", "callee")
        );
        assert_eq!(status.callgraph_pending, 2);
        checkout.refresh_callgraph().unwrap();
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert_eq!(callers, resolved);
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(layer, cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        assert_eq!(
            callers,
            callers_of(&cold.view().unwrap(), "src/util.rs", "callee")
        );
    }

    /// How many modules the fold tests' crate has.
    const FOLD_MODULES: usize = 4;

    /// The fold tests' crate: `top` calls `m0::f0`, each module's function
    /// calls the next one's, and each of `added` is a module of its own
    /// calling one of them. A binary file and a text file sit beside them.
    fn fold_crate(root: &Path, added: &[(usize, usize)]) {
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        fold_lib(root, added);
        for index in 0..FOLD_MODULES {
            fold_module(root, index, "");
        }
        write(root, "notes.txt", "nothing to see\n");
        std::fs::write(root.join("logo.bin"), [0u8, 1, 2, 0, 255, 0, 7]).unwrap();
    }

    /// The fold tests' `src/lib.rs`, declaring the modules of `added`, and
    /// those modules.
    fn fold_lib(root: &Path, added: &[(usize, usize)]) {
        let mut lib = String::new();
        for index in 0..FOLD_MODULES {
            lib.push_str(&format!("mod m{index};\n"));
        }
        for (round, index) in added {
            lib.push_str(&format!("mod added{round};\n"));
            write(
                root,
                &format!("src/added{round}.rs"),
                &format!("pub fn added{round}() {{ crate::m{index}::f{index}(); }}\n"),
            );
        }
        lib.push_str("pub fn top() { m0::f0(); }\n");
        write(root, "src/lib.rs", &lib);
    }

    /// The fold tests' module `index`, with `extra` after its function.
    fn fold_module(root: &Path, index: usize, extra: &str) {
        let body = if index + 1 < FOLD_MODULES {
            format!("crate::m{}::f{}();", index + 1, index + 1)
        } else {
            String::new()
        };
        write(
            root,
            &format!("src/m{index}.rs"),
            &format!("pub fn f{index}() {{ {body} }}\n{extra}"),
        );
    }

    /// What `view` answers that a fold must not change: callers, impact and
    /// traces of every function, text candidates, and the dead-code
    /// snapshot.
    fn fold_answers(view: &CheckoutView, rounds: usize) -> Vec<String> {
        let mut functions = vec![("src/lib.rs".to_string(), "top".to_string())];
        for index in 0..FOLD_MODULES {
            functions.push((format!("src/m{index}.rs"), format!("f{index}")));
        }
        for round in 0..rounds {
            functions.push((format!("src/added{round}.rs"), format!("added{round}")));
        }
        let mut answers = Vec::new();
        for (file, symbol) in &functions {
            answers.push(format!(
                "{file} {symbol}: {}",
                callers_of(view, file, symbol)
            ));
            for command in ["callers", "impact", "trace_to"] {
                answers.push(format!(
                    "{command} {file} {symbol}: {:?}",
                    view.probe_query(command, file, symbol)
                ));
            }
        }
        for pattern in ["pub fn", "nothing to see", "crate::m2", "added1", "marker"] {
            answers.push(format!("{pattern}: {:?}", candidates_for(view, pattern)));
        }
        let mut snapshot = view.rows_dead_code_snapshot().unwrap();
        snapshot.generated_at = None;
        answers.push(format!("{snapshot:?}"));
        answers
    }

    /// A checkout that published its changes round after round reads from
    /// a segment per round. Folding them copies what it reads into one new
    /// segment and retires the rest, without reading a file or resolving
    /// anything. It answers exactly as before, and as a cold build of its
    /// files does; the store keeps as many segments as a build leaves; a
    /// view held across the fold keeps answering from the old segments; a
    /// change recorded and not published stays laid over the folded
    /// publication; and a restart serves the fold.
    #[test]
    fn folding_published_changes_answers_as_before_and_as_a_cold_build() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        fold_crate(root.path(), &[]);
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        // Each round edits a module and adds a module calling it, which no
        // later round changes, so every round's segment stays read.
        let rounds = COMPACT_AT + 1;
        let mut added = Vec::new();
        let mut report = None;
        for round in 0..rounds {
            let index = round % FOLD_MODULES;
            added.push((round, index));
            fold_lib(root.path(), &added);
            fold_module(root.path(), index, &format!("pub fn r{round}() {{}}\n"));
            checkout
                .record_paths(&[
                    b"src/lib.rs".to_vec(),
                    format!("src/m{index}.rs").into_bytes(),
                    format!("src/added{round}.rs").into_bytes(),
                ])
                .unwrap();
            checkout.refresh_callgraph().unwrap();
            report = checkout.publish_changes().unwrap();
        }
        let report = report.expect("published");
        assert!(report.wants_fold(), "{report:?}");
        let segments_before = live_segments(&checkout);
        // A change recorded and resolved, but not published.
        write(
            root.path(),
            "src/m3.rs",
            "pub fn f3() { marker(); }\npub fn marker() {}\n",
        );
        checkout.record_paths(&[b"src/m3.rs".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        let held = checkout.view().unwrap();
        let before = fold_answers(&held, rounds);
        assert!(before.iter().any(|answer| answer.contains("marker")));

        let work = source_work_for_test(root.path());
        let Folded::Done(folded) = checkout.fold().unwrap() else {
            panic!("the fold did not finish");
        };
        assert_eq!(
            source_work_for_test(root.path()),
            work,
            "a fold reads no file and resolves nothing"
        );
        assert!(folded.read_before > COMPACT_AT, "{folded:?}");
        assert_eq!(folded.retired_segments, segments_before, "{folded:?}");
        let segments = live_segments(&checkout);
        assert_eq!(segments, 1, "{folded:?}");
        // The unpublished change is still there, and pending again in the
        // call graph until a refresh resolves it over the new publication.
        assert_eq!(checkout.changed_paths(), 1);
        assert_eq!(checkout.view().unwrap().status().callgraph_pending, 1);
        assert!(
            candidates_for(&checkout.view().unwrap(), "marker").contains(&"src/m3.rs".to_string())
        );
        checkout.refresh_callgraph().unwrap();
        assert_eq!(fold_answers(&checkout.view().unwrap(), rounds), before);
        // The view taken before the fold still reads the old segments.
        assert_eq!(fold_answers(&held, rounds), before);
        drop(held);

        // A cold build of the same files answers the same, and writes as
        // many segments.
        let cold_store = tempfile::tempdir().unwrap();
        let cold =
            Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root.path()).unwrap();
        cold.build().unwrap();
        assert_eq!(fold_answers(&cold.view().unwrap(), rounds), before);
        assert_eq!(live_segments(&cold), segments);

        // The change publishes over the folded publication as over any.
        checkout.publish_changes().unwrap().expect("published");
        assert_eq!(fold_answers(&checkout.view().unwrap(), rounds), before);
        // A restart serves what was published.
        drop(checkout);
        let restarted = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        assert_eq!(restarted.changed_paths(), 0);
        assert_eq!(fold_answers(&restarted.view().unwrap(), rounds), before);
    }

    /// Publishes `rounds` rounds of changes to `checkout`, the fold tests'
    /// crate at `root`: each edits a module and adds a module calling it,
    /// so every round's segment stays read. Returns the last report.
    fn publish_fold_rounds(checkout: &Checkout, root: &Path, rounds: usize) -> DeltaReport {
        let mut added = Vec::new();
        let mut report = None;
        for round in 0..rounds {
            let index = round % FOLD_MODULES;
            added.push((round, index));
            fold_lib(root, &added);
            fold_module(root, index, &format!("pub fn r{round}() {{}}\n"));
            checkout
                .record_paths(&[
                    b"src/lib.rs".to_vec(),
                    format!("src/m{index}.rs").into_bytes(),
                    format!("src/added{round}.rs").into_bytes(),
                ])
                .unwrap();
            checkout.refresh_callgraph().unwrap();
            report = checkout.publish_changes().unwrap();
        }
        report.expect("published")
    }

    /// A checkout folds its own segments only after it publishes, so one
    /// that stopped publishing kept every segment it reads. Another
    /// checkout's fold on its behalf folds them while it stays bound in the
    /// same process, with its call-graph engine kept and a change of its own
    /// recorded: it reads no file of that checkout, leaves fewer segments,
    /// and that checkout, a view of it held across the fold, and its next
    /// edit, publication, own fold and restart all answer as a cold build
    /// of its files does. With nothing due, nothing is folded.
    #[test]
    fn a_checkout_that_stopped_publishing_is_folded_on_its_behalf() {
        let idle_root = tempfile::tempdir().unwrap();
        let busy_root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        fold_crate(idle_root.path(), &[]);
        fold_crate(busy_root.path(), &[]);
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let idle =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), idle_root.path()).unwrap();
        idle.build().unwrap();
        let busy =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), busy_root.path()).unwrap();
        busy.build().unwrap();
        let rounds = COMPACT_AT + 1;
        let report = publish_fold_rounds(&idle, idle_root.path(), rounds);
        assert!(report.wants_fold(), "{report:?}");
        // A change recorded and resolved, not published, so the engine is
        // kept and the change laid over the publication.
        write(
            idle_root.path(),
            "src/m3.rs",
            "pub fn f3() { marker(); }\npub fn marker() {}\n",
        );
        idle.record_paths(&[b"src/m3.rs".to_vec()]).unwrap();
        idle.refresh_callgraph().unwrap();
        let held = idle.view().unwrap();
        let before = fold_answers(&held, rounds);
        assert!(before.iter().any(|answer| answer.contains("marker")));
        let cold_answers = |rounds: usize| {
            let cold_store = tempfile::tempdir().unwrap();
            let cold = Checkout::bind(
                Arc::clone(&layer),
                cold_store.path(),
                family(),
                idle_root.path(),
            )
            .unwrap();
            cold.build().unwrap();
            fold_answers(&cold.view().unwrap(), rounds)
        };
        assert_eq!(cold_answers(rounds), before);

        // The other checkout publishes a change of its own, which calls for
        // no fold of its own.
        write(busy_root.path(), "notes.txt", "busy here\n");
        busy.record_paths(&[b"notes.txt".to_vec()]).unwrap();
        busy.refresh_callgraph().unwrap();
        let own = busy.publish_changes().unwrap().expect("published");
        assert!(!own.wants_fold(), "{own:?}");
        let segments_before = live_segments(&busy);
        let work = source_work_for_test(idle_root.path());
        let other = busy.fold_other().unwrap();
        assert_eq!(
            source_work_for_test(idle_root.path()),
            work,
            "a fold on its behalf reads no file of the checkout"
        );
        assert_eq!(other.examined, 1, "{other:?}");
        assert_eq!(
            other
                .root
                .as_deref()
                .map(|root| std::fs::canonicalize(root).unwrap()),
            Some(std::fs::canonicalize(idle_root.path()).unwrap()),
            "{other:?}"
        );
        assert!(other.own_segments > COMPACT_AT, "{other:?}");
        assert_eq!(other.skipped, None);
        let fold = other.fold.clone().expect("folded");
        assert!(fold.retired_segments >= COMPACT_AT, "{other:?}");
        assert!(live_segments(&busy) < segments_before, "{other:?}");

        // A later publication reclaims what the fold retired; the held view
        // and the publication the idle checkout serves keep reading the
        // generation their lease holds.
        write(busy_root.path(), "notes.txt", "busy again\n");
        busy.record_paths(&[b"notes.txt".to_vec()]).unwrap();
        busy.refresh_callgraph().unwrap();
        busy.publish_changes().unwrap().expect("published");
        assert_eq!(fold_answers(&held, rounds), before);
        assert_eq!(fold_answers(&idle.view().unwrap(), rounds), before);
        drop(held);

        // Edited again and queried, it answers as a cold build of its files.
        write(
            idle_root.path(),
            "src/m2.rs",
            "pub fn f2() { crate::m3::f3(); marker_two(); }\npub fn marker_two() {}\n",
        );
        idle.record_paths(&[b"src/m2.rs".to_vec()]).unwrap();
        idle.refresh_callgraph().unwrap();
        let edited = fold_answers(&idle.view().unwrap(), rounds);
        assert_ne!(edited, before);
        assert_eq!(edited, cold_answers(rounds));
        // Its own next publication and its own fold work over the fold.
        idle.publish_changes().unwrap().expect("published");
        assert_eq!(fold_answers(&idle.view().unwrap(), rounds), edited);
        let Folded::Done(_) = idle.fold().unwrap() else {
            panic!("the idle checkout's own fold did not finish");
        };
        assert_eq!(fold_answers(&idle.view().unwrap(), rounds), edited);
        drop(idle);
        let restarted =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), idle_root.path()).unwrap();
        assert_eq!(restarted.changed_paths(), 0);
        assert_eq!(fold_answers(&restarted.view().unwrap(), rounds), edited);

        // The other checkout answers as a cold build of its own files, and
        // with nothing due now, it folds nothing.
        let busy_cold_store = tempfile::tempdir().unwrap();
        let busy_cold = Checkout::bind(
            Arc::clone(&layer),
            busy_cold_store.path(),
            family(),
            busy_root.path(),
        )
        .unwrap();
        busy_cold.build().unwrap();
        assert_eq!(
            fold_answers(&busy.view().unwrap(), 0),
            fold_answers(&busy_cold.view().unwrap(), 0)
        );
        let none = busy.fold_other().unwrap();
        assert!(none.root.is_none() && none.fold.is_none(), "{none:?}");
    }

    /// One checkout of the repository the store-growth test replays: its
    /// folder, its files as written there, and the checkout.
    struct Worktree {
        dir: tempfile::TempDir,
        files: BTreeMap<String, String>,
        checkout: Checkout,
        built: BuildReport,
    }

    impl Worktree {
        fn new(layer: &Arc<DataLayer>, store: &Path, files: BTreeMap<String, String>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for (path, text) in &files {
                write(dir.path(), path, text);
            }
            Self::bind(layer, store, dir, files)
        }

        /// Binds and builds the checkout at `dir`, which holds `files`.
        fn bind(
            layer: &Arc<DataLayer>,
            store: &Path,
            dir: tempfile::TempDir,
            files: BTreeMap<String, String>,
        ) -> Self {
            let checkout = Checkout::bind(Arc::clone(layer), store, family(), dir.path()).unwrap();
            let built = checkout.build().unwrap();
            Self {
                dir,
                files,
                checkout,
                built,
            }
        }

        /// Writes each `(path, text)` of `edits`, resolves them and publishes
        /// them, and folds the checkout's segments when the publication asks
        /// for it, as the runtime does. Returns whether it folded.
        fn edit(&mut self, edits: &[(String, String)]) -> bool {
            for (path, text) in edits {
                write(self.dir.path(), path, text);
                self.files.insert(path.clone(), text.clone());
            }
            let paths = edits
                .iter()
                .map(|(path, _)| path.as_bytes().to_vec())
                .collect::<Vec<_>>();
            self.checkout.record_paths(&paths).unwrap();
            self.checkout.refresh_callgraph().unwrap();
            let report = self.checkout.publish_changes().unwrap().expect("published");
            if report.wants_fold() {
                match self.checkout.fold().unwrap() {
                    Folded::Done(_) => return true,
                    other => panic!("the fold did not finish: {other:?}"),
                }
            }
            false
        }
    }

    /// A module of the repository the store-growth test replays: each
    /// calls the next, and an edit adds a function calling it.
    fn chain_module(index: usize, added: Option<&str>) -> String {
        let mut text = if index + 1 < CHAIN {
            let next = index + 1;
            format!("import {{ f{next} }} from './m{next}';\nexport function f{index}() {{ f{next}(); }}\n")
        } else {
            format!("export function f{index}() {{}}\n")
        };
        if let Some(added) = added {
            text.push_str(&format!("export function {added}() {{ f{index}(); }}\n"));
        }
        text
    }

    const CHAIN: usize = 8;

    /// Worktrees of one repository keep editing while a main checkout takes
    /// in their changes, as one merging its workers' branches does, and a
    /// new worktree is made from main every few rounds; older worktrees go
    /// idle. Each round's edits change a module, which a later round
    /// changes again, and add a file nobody changes again. Every checkout
    /// publishes its changes and has its segments folded when the
    /// publication asks for it, as the runtime does. The family store stays
    /// within [`READ_AT`] + 2 segments per checkout throughout. It used to
    /// grow by a segment every round: the change segment holding a round's
    /// new file was read by both the worker and main, so neither ever
    /// counted it as its own or folded it. Every checkout then answers as a
    /// cold build of its files does.
    #[test]
    fn worktrees_editing_in_turn_keep_the_store_bounded() {
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let mut files = BTreeMap::new();
        files.insert(
            "package.json".to_string(),
            "{ \"name\": \"demo\" }\n".to_string(),
        );
        for index in 0..CHAIN {
            files.insert(format!("src/m{index}.ts"), chain_module(index, None));
        }
        files.insert("notes.txt".to_string(), "nothing to see\n".to_string());
        let mut main = Worktree::new(&layer, store.path(), files);
        let mut workers: Vec<Worktree> = Vec::new();
        let mut folds = 0;
        let mut most = (0, 0);
        let mut new_worktree_bytes = Vec::new();
        let mut series = Vec::new();
        for round in 0..60 {
            if round % 15 == 0 {
                let worktree = Worktree::new(&layer, store.path(), main.files.clone());
                // The build's segment is the newest the store names.
                let snapshot = worktree.checkout.family.snapshot().unwrap();
                new_worktree_bytes.push(snapshot.manifest().segments().last().unwrap().len);
                workers.push(worktree);
            }
            // The two newest worktrees edit; the rest are idle.
            let active = workers.len().saturating_sub(2);
            for (offset, worker) in workers[active..].iter_mut().enumerate() {
                let index = (round + offset) % CHAIN;
                let edits = [
                    (
                        format!("src/m{index}.ts"),
                        chain_module(index, Some(&format!("r{round}w{offset}"))),
                    ),
                    (
                        format!("src/r{round}w{offset}.ts"),
                        format!(
                            "import {{ f{index} }} from './m{index}';\nexport function g{round}w{offset}() {{ f{index}(); }}\n"
                        ),
                    ),
                ];
                folds += usize::from(worker.edit(&edits));
                // Main takes the change in.
                folds += usize::from(main.edit(&edits));
            }
            let checkouts = workers.len() + 1;
            let segments = main.checkout.store_segments().unwrap();
            series.push(segments);
            most = most.max((segments, checkouts));
            assert!(
                segments <= checkouts * (READ_AT + 2),
                "round {round}: {segments} segments for {checkouts} checkouts: {series:?}"
            );
        }
        eprintln!(
            "store growth: at most {} segments for {} checkouts; {folds} folds; new \
             worktrees wrote {new_worktree_bytes:?} bytes; {series:?}",
            most.0, most.1
        );
        assert!(folds > 0, "no checkout's segments were ever folded");
        for (name, worktree) in
            std::iter::once(("main", &main)).chain(workers.iter().map(|worker| ("worker", worker)))
        {
            let cold_store = tempfile::tempdir().unwrap();
            let cold = Checkout::bind(
                Arc::clone(&layer),
                cold_store.path(),
                family(),
                worktree.dir.path(),
            )
            .unwrap();
            let cold_bytes = cold.build().unwrap().segment_bytes;
            eprintln!("{name}: a cold build of its files writes {cold_bytes} bytes");
            let ours = worktree.checkout.view().unwrap();
            let theirs = cold.view().unwrap();
            for index in 0..CHAIN {
                let file = format!("src/m{index}.ts");
                let symbol = format!("f{index}");
                let callers = callers_of(&ours, &file, &symbol);
                assert!(
                    index == 0 || callers.contains(&format!("f{}", index - 1)),
                    "{name} {file}: {callers}"
                );
                assert_eq!(
                    callers,
                    callers_of(&theirs, &file, &symbol),
                    "{name} {file}"
                );
            }
            for pattern in ["export function g", "nothing to see", "r59"] {
                assert_eq!(
                    candidates_for(&ours, pattern),
                    candidates_for(&theirs, pattern),
                    "{name} {pattern}"
                );
            }
        }
        // Main took in every round's files, so its callers of each module
        // include the files added for it.
        let callers = callers_of(&main.checkout.view().unwrap(), "src/m3.ts", "f3");
        assert!(
            callers.contains("g59w0") || callers.contains("g58w1"),
            "{callers}"
        );
    }

    /// Every caller of the functions the sibling tests' repository declares,
    /// and a few text searches, as `view` answers them.
    fn sibling_answers(view: &CheckoutView) -> Vec<String> {
        let mut answers = Vec::new();
        for (file, symbol) in [
            ("packages/lib/index.ts", "helper"),
            ("src/util.ts", "util"),
            ("src/f3.ts", "f3"),
            ("src/extra.ts", "extra"),
        ] {
            answers.push(format!(
                "{file} {symbol}: {}",
                callers_of(view, file, symbol)
            ));
        }
        for pattern in ["helper", "util", "extra", "nothing to see"] {
            answers.push(format!("{pattern}: {:?}", candidates_for(view, pattern)));
        }
        answers
    }

    /// A cold build's answers for the checkout at `root`.
    fn cold_sibling_answers(layer: &Arc<DataLayer>, root: &Path) -> Vec<String> {
        let store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(Arc::clone(layer), store.path(), family(), root).unwrap();
        let built = cold.build().unwrap();
        assert_eq!(built.started_from_sibling, None);
        sibling_answers(&cold.view().unwrap())
    }

    /// A new worktree starts from the published checkout its files differ
    /// from least and resolves only what differs, instead of parsing and
    /// resolving everything: a worktree with the same files differs in
    /// nothing; one whose workspace package was renamed, with a file added,
    /// a file removed and a link that now leaves the checkout, differs in
    /// those four paths. Each answers as a cold build of its files does,
    /// and keeps doing so after an edit. One that differs in more than a
    /// quarter of its paths is built whole, reading the records the store
    /// already holds instead of parsing those files again.
    #[test]
    fn a_new_worktree_starts_from_its_closest_sibling() {
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let mut files = BTreeMap::new();
        let mut add = |path: &str, text: &str| {
            files.insert(path.to_string(), text.to_string());
        };
        add(
            "package.json",
            "{ \"name\": \"demo\", \"private\": true, \"workspaces\": [\"packages/*\"] }\n",
        );
        add(
            "packages/lib/package.json",
            "{ \"name\": \"lib-a\", \"main\": \"index.ts\" }\n",
        );
        add("packages/lib/index.ts", "export function helper() {}\n");
        add(
            "src/main.ts",
            "import { helper } from 'lib-a';\nimport { util } from './util';\nexport function main() { helper(); util(); }\n",
        );
        add("src/util.ts", "export function util() {}\n");
        add(
            "src/via_link.ts",
            "import { util } from './linked';\nexport function viaLink() { util(); }\n",
        );
        add("notes.txt", "nothing to see\n");
        for index in 0..24 {
            add(
                &format!("src/f{index}.ts"),
                &format!(
                    "import {{ util }} from './util';\nexport function f{index}() {{ util(); }}\n"
                ),
            );
        }
        let main_dir = tempfile::tempdir().unwrap();
        for (path, text) in &files {
            write(main_dir.path(), path, text);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("util.ts", main_dir.path().join("src/linked.ts")).unwrap();
        let main = Worktree::bind(&layer, store.path(), main_dir, files.clone());
        assert_eq!(
            main.built.started_from_sibling, None,
            "nothing to start from"
        );
        assert!(callers_of(
            &main.checkout.view().unwrap(),
            "packages/lib/index.ts",
            "helper"
        )
        .contains("main"));

        let same_dir = tempfile::tempdir().unwrap();
        for (path, text) in &files {
            write(same_dir.path(), path, text);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("util.ts", same_dir.path().join("src/linked.ts")).unwrap();
        let same = Worktree::bind(&layer, store.path(), same_dir, files.clone());
        assert_eq!(same.built.started_from_sibling, Some(0), "{:?}", same.built);
        assert_eq!(
            sibling_answers(&same.checkout.view().unwrap()),
            cold_sibling_answers(&layer, same.dir.path())
        );

        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "util.ts", "export function util() {}\n");
        let mut changed = files.clone();
        changed.insert(
            "packages/lib/package.json".to_string(),
            "{ \"name\": \"lib-b\", \"main\": \"index.ts\" }\n".to_string(),
        );
        changed.insert(
            "src/extra.ts".to_string(),
            "import { util } from './util';\nexport function extra() { util(); }\n".to_string(),
        );
        changed.remove("notes.txt");
        let changed_dir = tempfile::tempdir().unwrap();
        for (path, text) in &changed {
            write(changed_dir.path(), path, text);
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("util.ts"),
            changed_dir.path().join("src/linked.ts"),
        )
        .unwrap();
        let mut worktree = Worktree::bind(&layer, store.path(), changed_dir, changed);
        let differing = if cfg!(unix) { 4 } else { 3 };
        assert_eq!(
            worktree.built.started_from_sibling,
            Some(differing),
            "{:?}",
            worktree.built
        );
        let ours = sibling_answers(&worktree.checkout.view().unwrap());
        assert_eq!(ours, cold_sibling_answers(&layer, worktree.dir.path()));
        assert!(
            !ours[0].contains("main"),
            "the renamed package no longer serves main.ts's import: {ours:?}"
        );
        assert_ne!(
            ours,
            sibling_answers(&main.checkout.view().unwrap()),
            "the worktree answers for its own files"
        );
        // An edit then goes through as in any checkout.
        worktree.edit(&[(
            "src/extra.ts".to_string(),
            "export function extra() {}\n".to_string(),
        )]);
        assert_eq!(
            sibling_answers(&worktree.checkout.view().unwrap()),
            cold_sibling_answers(&layer, worktree.dir.path())
        );

        // Most files differ: built whole, reading the stored records of
        // the files that do not.
        let mut far = files.clone();
        for index in 0..10 {
            far.insert(
                format!("src/f{index}.ts"),
                format!("export function f{index}() {{}}\n"),
            );
        }
        let far = Worktree::new(&layer, store.path(), far);
        assert_eq!(far.built.started_from_sibling, None, "{:?}", far.built);
        assert!(far.built.stored_records > 0, "{:?}", far.built);
        assert_eq!(
            sibling_answers(&far.checkout.view().unwrap()),
            cold_sibling_answers(&layer, far.dir.path())
        );
    }

    /// A new worktree started from a sibling with the same files, edited in
    /// the crate root every module reads, keeps decoded only what the edit
    /// resolved again. The update still lists and binds the module-declaring
    /// files and checks the recorded answers of every module that reads the
    /// root, but keeps none of the stored parses and resolutions it reads
    /// decoded; keeping them decoded took over a gigabyte on AFT's own
    /// checkout. Answers stay a cold build's.
    #[test]
    fn a_fresh_worktree_keeps_decoded_only_what_its_first_edit_resolves_again() {
        const MODULES: usize = 24;
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let mut files = BTreeMap::new();
        files.insert(
            "Cargo.toml".to_string(),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_string(),
        );
        let mut lib = String::from("pub fn shared() {}\n");
        for index in 0..MODULES {
            lib.push_str(&format!("pub mod m{index};\n"));
            files.insert(
                format!("src/m{index}.rs"),
                format!("use crate::shared;\npub fn f{index}() {{ shared(); }}\n"),
            );
        }
        files.insert("src/lib.rs".to_string(), lib.clone());
        let sibling = Worktree::new(&layer, store.path(), files.clone());
        assert_eq!(sibling.built.started_from_sibling, None);
        let worktree = Worktree::new(&layer, store.path(), files);
        assert_eq!(
            worktree.built.started_from_sibling,
            Some(0),
            "{:?}",
            worktree.built
        );
        let root = worktree.dir.path();
        let before = source_work_for_test(root);

        write(root, "src/lib.rs", &format!("{lib}fn added() {{}}\n"));
        worktree
            .checkout
            .record_paths(&[b"src/lib.rs".to_vec()])
            .unwrap();
        let stats = worktree
            .checkout
            .refresh_callgraph()
            .unwrap()
            .expect("the edit was resolved");
        let work = source_work_for_test(root);
        assert_eq!(work.resolutions, before.resolutions + 1);
        // The update reached every module through the root, and resolved
        // again only what the edit changed.
        assert!(stats.replayed >= MODULES, "{stats:?}");
        assert!(stats.resolved <= 1, "{stats:?}");
        let touched = stats.touched.len();
        assert!(touched <= 2, "{stats:?}");
        assert!(
            work.decoded_parses <= touched && work.decoded_resolutions <= touched,
            "an update touching {touched} files kept {} parses and {} resolutions decoded \
             of {} files",
            work.decoded_parses,
            work.decoded_resolutions,
            MODULES + 1
        );

        let answers = |view: &CheckoutView| {
            [
                ("src/lib.rs", "shared"),
                ("src/lib.rs", "added"),
                ("src/m7.rs", "f7"),
            ]
            .map(|(file, symbol)| format!("{file} {symbol}: {}", callers_of(view, file, symbol)))
        };
        let ours = answers(&worktree.checkout.view().unwrap());
        assert!(
            ours[0].contains(&format!("total_callers: {MODULES},")),
            "{ours:?}"
        );
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(Arc::clone(&layer), cold_store.path(), family(), root).unwrap();
        cold.build().unwrap();
        assert_eq!(ours, answers(&cold.view().unwrap()));
    }

    #[test]
    fn a_restart_sees_what_changed_while_nothing_watched() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path())
            .unwrap()
            .build()
            .unwrap();

        // Changed while no process ran: a text edit, and a new caller.
        write(root.path(), "notes.txt", "newneedle\n");
        write(
            root.path(),
            "src/extra.rs",
            "use crate::util;\npub fn other() { util::callee(); }\n",
        );
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
        );

        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        let view = checkout.view().unwrap();
        assert_eq!(candidates_for(&view, "newneedle"), vec!["notes.txt"]);
        // The call graph reports the changed files pending, not ready.
        assert!(
            matches!(view.status().callgraph, PlaneStatus::Building { .. }),
            "{:?}",
            view.status()
        );
        drop(view);
        checkout.refresh_callgraph().unwrap();
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(callers.contains("src/extra.rs"), "{callers}");
    }

    /// A workspace package renamed to the name a caller imports: the caller's
    /// recorded miss must reach it through the published store, after a
    /// restart as well as in the process that built it.
    #[test]
    fn a_workspace_package_rename_reaches_its_importer_through_the_store() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(
            root.path(),
            "package.json",
            r#"{"private":true,"workspaces":["packages/*"]}"#,
        );
        write(
            root.path(),
            "packages/pkg/package.json",
            r#"{"name":"@scope/old"}"#,
        );
        write(
            root.path(),
            "packages/pkg/src/index.ts",
            "export function target(): string { return 'ok'; }\n",
        );
        write(
            root.path(),
            "packages/app/package.json",
            r#"{"name":"app"}"#,
        );
        write(
            root.path(),
            "packages/app/src/main.ts",
            "import { target } from '@scope/new';\nexport function run(): string { return target(); }\n",
        );
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let built =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        built.build().unwrap();
        let index = "packages/pkg/src/index.ts";
        assert!(
            !callers_of(&built.view().unwrap(), index, "target").contains("main.ts"),
            "no package has the imported name yet"
        );
        write(
            root.path(),
            "packages/pkg/package.json",
            r#"{"name":"@scope/new"}"#,
        );

        // In the process that built it: the change is recorded.
        built
            .record_paths(&[b"packages/pkg/package.json".to_vec()])
            .unwrap();
        built.refresh_callgraph().unwrap();
        let live = callers_of(&built.view().unwrap(), index, "target");
        drop(built);

        // After a restart: the bind finds the change against the publication.
        let restarted = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        restarted.refresh_callgraph().unwrap();
        let after_restart = callers_of(&restarted.view().unwrap(), index, "target");
        assert!(live.contains("main.ts"), "recorded change: {live}");
        assert!(
            after_restart.contains("main.ts"),
            "after restart: {after_restart}"
        );
    }

    #[test]
    fn a_rescan_records_what_changed_unreported_while_bound() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        assert_eq!(checkout.rescan().unwrap(), 0, "nothing changed yet");

        // Changed with the checkout bound but nothing recorded, as when the
        // root was unbound or a watcher event has not arrived.
        write(root.path(), "notes.txt", "newneedle\n");
        write(
            root.path(),
            "src/extra.rs",
            "use crate::util;\npub fn other() { util::callee(); }\n",
        );
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
        );
        assert!(candidates_for(&checkout.view().unwrap(), "newneedle").is_empty());

        // While a rescan runs, the call graph is not reported ready.
        checkout.begin_rescan();
        assert!(matches!(
            checkout.status().callgraph,
            PlaneStatus::Building { .. }
        ));
        assert_eq!(checkout.rescan().unwrap(), 3);
        checkout.end_rescan();
        let view = checkout.view().unwrap();
        assert_eq!(candidates_for(&view, "newneedle"), vec!["notes.txt"]);
        drop(view);
        checkout.refresh_callgraph().unwrap();
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(callers.contains("src/extra.rs"), "{callers}");
        // A second rescan finds nothing new.
        assert_eq!(checkout.rescan().unwrap(), 0);
    }

    #[test]
    fn rebuilds_retire_old_segments_and_a_held_view_keeps_reading() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        let held = checkout.view().unwrap();

        let second = checkout.build().unwrap();
        assert_eq!(second.retired_segments, 1, "{second:?}");
        assert!(second.reclaim_error.is_none(), "{second:?}");
        let third = checkout.build().unwrap();
        assert_eq!(third.retired_segments, 1, "{third:?}");
        let snapshot = Family::open(store.path(), family())
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.manifest().segments().len(), 1);
        drop(snapshot);

        // The view taken before the rebuilds still reads its generation.
        assert_eq!(
            candidates_for(&held, "callee"),
            vec!["src/lib.rs", "src/util.rs"]
        );
        drop(held);

        // Once nothing reads the old generations, a build reclaims their
        // files. What remains is the new segment and the one the checkout
        // served while it built: its own publication holds a lease until the
        // swap, so the next build removes it.
        let fourth = checkout.build().unwrap();
        assert!(fourth.reclaimed_files > 0, "{fourth:?}");
        let segments = std::fs::read_dir(store.path().join("segments"))
            .unwrap()
            .filter_map(Result::ok)
            .count();
        assert_eq!(segments, 2);
    }

    #[test]
    fn a_build_retires_checkouts_whose_folder_is_gone() {
        let parent = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let main = parent.path().join("main");
        let deleted = parent.path().join("worker");
        let unmounted = parent.path().join("volume/worker");
        for root in [&main, &deleted, &unmounted] {
            demo(root);
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root)
                .unwrap()
                .build()
                .unwrap();
        }
        // One worktree was deleted. The other went with its parent folder,
        // as a checkout on a volume that is not mounted does: that is
        // "can't tell", and keeps it.
        std::fs::remove_dir_all(&deleted).unwrap();
        std::fs::remove_dir_all(parent.path().join("volume")).unwrap();

        let checkout = Checkout::bind(Arc::clone(&layer), store.path(), family(), &main).unwrap();
        let report = checkout.build().unwrap();
        assert_eq!(report.retired_checkouts, 1, "{report:?}");
        let id = |root: &Path| {
            CheckoutId::from_digest(Digest128::of(root.as_os_str().as_encoded_bytes()))
        };
        let snapshot = Family::open(store.path(), family())
            .unwrap()
            .snapshot()
            .unwrap();
        let kept = snapshot
            .manifest()
            .checkouts()
            .iter()
            .map(|state| state.checkout)
            .collect::<Vec<_>>();
        assert!(kept.contains(&id(&main)), "{kept:?}");
        assert!(kept.contains(&id(&unmounted)), "{kept:?}");
        assert!(!kept.contains(&id(&deleted)), "{kept:?}");
    }

    #[test]
    fn below_the_disk_floor_nothing_is_written_and_the_published_view_answers() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();

        // 1 GiB free on a 1 TiB disk: below the 32 GiB floor.
        let low = crate::disk_floor::set_free_space_for_test(store.path(), 1 << 30, 1 << 40);
        let paused = "Index storage is paused: 1.0 GiB is free on this disk and AFT keeps \
                      32.0 GiB free. Unused index data is being cleaned up; retry later.";
        let error = checkout.build().unwrap_err();
        assert!(matches!(error, DataError::DiskFloor(_)), "{error}");
        assert_eq!(error.to_string(), paused);
        // Resolving an edit writes nothing; publishing it is refused too.
        write(
            root.path(),
            "src/util.rs",
            "pub fn callee() {}\npub fn other() {}\n",
        );
        checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();
        assert_eq!(checkout.publish_changes().unwrap_err().to_string(), paused);
        // What was published keeps answering, with the edit laid over it.
        let view = checkout.view().unwrap();
        assert_eq!(
            candidates_for(&view, "other"),
            vec!["src/util.rs".to_string()]
        );
        assert!(callers_of(&view, "src/util.rs", "callee").contains("caller"));

        drop(low);
        checkout.build().unwrap();
    }

    /// Rewrites the segment format version of every segment the newest
    /// generation in `store` names, keeping the length the manifest
    /// published, and returns each path with its new bytes.
    fn set_segment_versions(store: &Path, version: u16) -> Vec<(PathBuf, Vec<u8>)> {
        let snapshot = Family::open(store, family()).unwrap().snapshot().unwrap();
        snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| {
                let path = snapshot.segment_path(entry.id).unwrap();
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[8..10].copy_from_slice(&version.to_le_bytes());
                std::fs::remove_file(&path).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                (path, bytes)
            })
            .collect()
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// A segment a newer build wrote, named by a manifest this build reads:
    /// the checkout that published it refuses to bind, another checkout of
    /// the family binds but its build refuses before it scans or writes, no
    /// writer can start to reclaim it, and its bytes stay as they are.
    #[test]
    fn a_segment_a_newer_build_wrote_is_refused_and_never_rebuilt_over() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        demo(other.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path())
            .unwrap()
            .build()
            .unwrap();
        let planted = set_segment_versions(store.path(), 99);
        assert!(!planted.is_empty());
        let before =
            ["", "segments", "tmp", "checkouts"].map(|dir| listing(&store.path().join(dir)));
        let refused = |error: &DataError| {
            let refusal = error
                .refusal()
                .unwrap_or_else(|| panic!("not refused: {error}"));
            assert!(
                planted.iter().any(|(path, _)| *path == refusal.path),
                "{refusal:?}"
            );
            assert_eq!((refusal.found, refusal.supported), (99, 1));
            assert_eq!(
                error.to_string(),
                format!(
                    "storage_requires_newer_reader: index_store at {} has format version 99; this build reads up to 1",
                    refusal.path.display()
                )
            );
        };

        // The checkout that reads the segment cannot bind.
        let error = Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path())
            .err()
            .expect("the bind is refused");
        refused(&error);

        // One with nothing published binds; its build fails with the
        // refusal, which its failure record keeps, as does a retry.
        let second =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), other.path()).unwrap();
        for _ in 0..2 {
            let error = second.build().unwrap_err();
            refused(&error);
            let failure = second.last_failure().unwrap();
            assert_eq!(failure.refusal.as_ref(), error.refusal());
            assert_eq!(failure.error, error.to_string());
        }
        assert!(second.view().is_none());
        drop(second);

        // No writer starts, so nothing can retire or reclaim the segment.
        let family_store = Family::open(store.path(), family()).unwrap();
        let error = family_store
            .try_write()
            .err()
            .expect("the writer is refused");
        refused(&DataError::store(error));
        drop(family_store);

        let after =
            ["", "segments", "tmp", "checkouts"].map(|dir| listing(&store.path().join(dir)));
        assert_eq!(after, before, "a file was written or removed");
        for (path, bytes) in &planted {
            assert_eq!(&std::fs::read(path).unwrap(), bytes, "{}", path.display());
        }
    }

    /// A segment an older build wrote is unusable, as it always was: the
    /// checkout treats it as absent, builds again, and the build retires it.
    #[test]
    fn a_segment_an_older_build_wrote_is_treated_as_absent_and_rebuilt() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path())
            .unwrap()
            .build()
            .unwrap();
        let planted = set_segment_versions(store.path(), 0);
        assert!(!planted.is_empty());

        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        assert!(checkout.view().is_none(), "nothing usable is served");
        assert!(checkout.needs_build());
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        assert_eq!(view.status().callgraph, PlaneStatus::Ready);
        assert!(callers_of(&view, "src/util.rs", "callee").contains("src/lib.rs"));
        drop(view);
        let snapshot = Family::open(store.path(), family())
            .unwrap()
            .snapshot()
            .unwrap();
        for (path, _) in &planted {
            assert!(
                snapshot
                    .manifest()
                    .segments()
                    .iter()
                    .all(|entry| snapshot.segment_path(entry.id).as_ref() != Some(path)),
                "{} is still published",
                path.display()
            );
        }
    }

    #[test]
    fn a_checkout_without_the_call_graph_builds_and_serves_its_text_alone() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let checkout = Checkout::bind_planes(
            Arc::new(DataLayer::new(8 << 20, 2)),
            store.path(),
            family(),
            root.path(),
            Planes {
                text: true,
                callgraph: false,
            },
        )
        .unwrap();
        assert!(checkout.needs_build());
        let report = checkout.build().unwrap();
        assert_eq!(report.parsed_files, 0, "{report:?}");
        assert!(!checkout.needs_build(), "the text index is all it builds");
        let status = checkout.status();
        assert_eq!(
            (status.text, status.callgraph, status.callgraph_published),
            (PlaneStatus::Ready, PlaneStatus::Off, false)
        );
        let view = checkout.view().unwrap();
        assert_eq!(
            candidates_for(&view, "callee"),
            vec!["src/lib.rs", "src/util.rs"]
        );
        assert!(!view.callgraph_has_file("src/lib.rs"));

        // An edit is laid over the text index and never resolved; the next
        // build folds it in and retires what the checkout no longer reads.
        write(root.path(), "src/extra.rs", "pub fn callee_too() {}\n");
        checkout.record_paths(&[b"src/extra.rs".to_vec()]).unwrap();
        assert!(checkout.refresh_callgraph().unwrap().is_none());
        assert_eq!(
            candidates_for(&checkout.view().unwrap(), "callee_too"),
            vec!["src/extra.rs"]
        );
        let rebuilt = checkout.build().unwrap();
        assert_eq!(rebuilt.retired_segments, 1, "{rebuilt:?}");
        assert_eq!(checkout.changed_paths(), 0);
    }

    #[test]
    fn a_checkout_without_the_text_index_builds_its_call_graph_alone() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let checkout = Checkout::bind_planes(
            Arc::new(DataLayer::new(8 << 20, 2)),
            store.path(),
            family(),
            root.path(),
            Planes {
                text: false,
                callgraph: true,
            },
        )
        .unwrap();
        let report = checkout.build().unwrap();
        assert_eq!(report.text_units, 0, "{report:?}");
        assert!(report.text_ready_ms.is_none(), "no text publication first");
        let status = checkout.status();
        assert_eq!(
            (status.text, status.callgraph),
            (PlaneStatus::Off, PlaneStatus::Ready)
        );
        let view = checkout.view().unwrap();
        assert!(callers_of(&view, "src/util.rs", "callee").contains("caller"));
    }

    /// A file deleted in the same batch as a file its type-qualified calls
    /// resolve through has a recorded answer that changed. Its stale answer
    /// must not send the refresh after a file that no longer exists.
    #[test]
    fn deleting_a_file_with_the_file_its_calls_resolve_through_refreshes() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(root.path(), "web/b.ts", "export class B { go() {} }\n");
        write(
            root.path(),
            "web/a.ts",
            "import { B } from './b';\nexport function run(b: B) { b.go(); }\n",
        );
        write(
            root.path(),
            "web/c.ts",
            "export function keep() {}\nexport function caller() { keep(); }\n",
        );
        write(root.path(), "package.json", "{\"name\": \"demo\"}\n");
        write(root.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(root.path(), "src/lib.rs", "mod a;\nmod b;\n");
        write(
            root.path(),
            "src/a.rs",
            "use crate::b::B;\npub struct A;\nimpl A { pub fn run(&self) { B::go(); demo::b::B::go(); } }\n",
        );
        write(
            root.path(),
            "src/b.rs",
            "pub struct B;\nimpl B { pub fn go() {} }\n",
        );
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();
        // Start the engine from the publication with a first edit.
        write(
            root.path(),
            "web/c.ts",
            "export function keep() {}\nexport function caller() { keep(); keep(); }\n",
        );
        checkout.record_paths(&[b"web/c.ts".to_vec()]).unwrap();
        checkout.refresh_callgraph().unwrap();

        std::fs::remove_file(root.path().join("web/a.ts")).unwrap();
        std::fs::remove_file(root.path().join("web/b.ts")).unwrap();
        std::fs::remove_file(root.path().join("src/a.rs")).unwrap();
        write(root.path(), "src/lib.rs", "mod b;\n");
        write(root.path(), "Cargo.toml", "[package]\nname = \"renamed\"\n");
        write(root.path(), "package.json", "{\"name\": \"renamed\"}\n");
        checkout
            .record_paths(&[
                b"web/a.ts".to_vec(),
                b"web/b.ts".to_vec(),
                b"src/a.rs".to_vec(),
                b"src/lib.rs".to_vec(),
                b"Cargo.toml".to_vec(),
                b"package.json".to_vec(),
            ])
            .unwrap();
        checkout.refresh_callgraph().unwrap();
        let view = checkout.view().unwrap();
        let callers = callers_of(&view, "web/c.ts", "keep");
        assert!(callers.contains("symbol: \"caller\""), "{callers}");
    }

    #[test]
    fn a_publication_whose_records_this_build_cannot_find_serves_text_and_rebuilds() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let built =
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap();
        built.build().unwrap();
        let published = built.view().unwrap().state().clone();
        let id = built.checkout;
        drop(built);
        // An older extractor version keyed its records differently: publish
        // the same state with each parsed path's record key changed, so no
        // segment holds the records it names.
        let paths = published
            .paths()
            .iter()
            .cloned()
            .map(|mut record| {
                if !record.language.is_empty() {
                    record.language.push_str("-older");
                }
                record
            })
            .collect::<Vec<_>>();
        let fragments = (0..paths.len())
            .map(|index| published.fragment(index))
            .collect::<Vec<_>>();
        let older = CheckoutState::new(published.git.clone(), published.conversion, paths)
            .unwrap()
            .with_root_of(&published)
            .with_fragments(fragments)
            .unwrap();
        let family_store = Family::open(store.path(), family()).unwrap();
        let mut writer = family_store.try_write().unwrap().unwrap();
        let written = writer.write_checkout_state(id, &older).unwrap();
        writer
            .publish(&Publication {
                states: vec![written],
                ..Publication::default()
            })
            .unwrap();
        drop(writer);

        // A fresh bind serves the publication's text, and counts its call
        // graph as not built, so the checkout's own bind builds it again.
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        let view = checkout.view().unwrap();
        assert!(!candidates_for(&view, "callee").is_empty());
        assert!(checkout.needs_build());
        checkout.build().unwrap();
        let view = checkout.view().unwrap();
        assert!(callers_of(&view, "src/util.rs", "callee").contains("caller"));
    }

    #[test]
    fn a_build_stands_down_when_another_process_published_first() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Arc::new(
            Checkout::bind(Arc::clone(&layer), store.path(), family(), root.path()).unwrap(),
        );
        // Another process holds the writer while this build scans.
        let family_store = Family::open(store.path(), family()).unwrap();
        let mut other = family_store.try_write().unwrap().unwrap();
        let building = std::thread::spawn({
            let checkout = Arc::clone(&checkout);
            move || checkout.build()
        });
        let started = Instant::now();
        while !checkout.status().building {
            assert!(started.elapsed().as_secs() < 30, "the build never started");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Let the build take its starting snapshot and reach the writer.
        std::thread::sleep(std::time::Duration::from_millis(500));
        // The other process publishes this checkout first.
        let state = scan(root.path(), &layer.policy, None, &ChangeSink::default())
            .unwrap()
            .state;
        let written = other
            .write_checkout_state(checkout.checkout, &state)
            .unwrap();
        other
            .publish(&Publication {
                states: vec![written],
                ..Publication::default()
            })
            .unwrap();
        drop(other);

        let report = building.join().unwrap().unwrap();
        assert!(report.superseded, "{report:?}");
        // The checkout serves the other process's publication, and nothing
        // of its own was published over it.
        let snapshot = family_store.snapshot().unwrap();
        assert!(snapshot.manifest().segments().is_empty());
        let view = checkout.view().unwrap();
        assert_eq!(view.state().paths().len(), state.paths().len());
        assert!(!checkout.status().building);
    }

    #[test]
    fn recorded_changes_follow_the_scan_rules() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        write(root.path(), "docs/needle.txt", "zzfolder\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();

        // Build output never becomes a member.
        write(root.path(), "target/debug/gen.rs", "fn zzbuild() {}\n");
        checkout
            .record_paths(&[b"target/debug/gen.rs".to_vec()])
            .unwrap();
        assert!(candidates_for(&checkout.view().unwrap(), "zzbuild").is_empty());

        // A removed folder takes its files with it.
        std::fs::remove_dir_all(root.path().join("docs")).unwrap();
        checkout.record_paths(&[b"docs".to_vec()]).unwrap();
        assert!(candidates_for(&checkout.view().unwrap(), "zzfolder").is_empty());

        // A source file over the read limit is a member that is not parsed,
        // as in a build. (One over only the text-index limit is parsed; see
        // a_source_file_past_the_text_index_limit_still_has_its_calls.)
        let mut big = String::from("use crate::util;\npub fn big() { util::callee(); }\n");
        while big.len() <= 4 * 1024 * 1024 {
            big.push_str("// padding padding padding padding padding padding\n");
        }
        write(root.path(), "src/big.rs", &big);
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod big;\npub fn caller() { util::callee(); }\n",
        );
        checkout
            .record_paths(&[b"src/big.rs".to_vec(), b"src/lib.rs".to_vec()])
            .unwrap();
        checkout.refresh_callgraph().unwrap();
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(!callers.contains("src/big.rs"), "{callers}");
        // A cold build of the same files agrees.
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 2)),
            cold_store.path(),
            family(),
            root.path(),
        )
        .unwrap();
        cold.build().unwrap();
        assert_eq!(
            callers_of(&cold.view().unwrap(), "src/util.rs", "callee"),
            callers
        );
    }

    #[test]
    fn an_edit_updates_the_call_graph_without_a_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(root.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
        );
        write(root.path(), "src/util.rs", "pub fn callee() {}\n");
        write(root.path(), "src/extra.rs", "pub fn other() {}\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let family = FamilyId::from_digest(Digest128::of(b"demo"));
        let checkout = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
        checkout.build().unwrap();
        let before = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(!before.contains("src/extra.rs"), "{before}");

        // A new caller of `callee` in another file, and the old caller
        // removed: the call graph must show both after a refresh, with no
        // rebuild.
        let extra = "use crate::util;\npub fn other() { util::callee(); }\n";
        let lib = "mod util;\nmod extra;\npub fn caller() {}\n";
        write(root.path(), "src/extra.rs", extra);
        write(root.path(), "src/lib.rs", lib);
        checkout
            .record_paths(&[b"src/extra.rs".to_vec(), b"src/lib.rs".to_vec()])
            .unwrap();
        assert!(matches!(
            checkout.status().callgraph,
            PlaneStatus::Building { .. }
        ));
        let stats = checkout
            .refresh_callgraph()
            .unwrap()
            .expect("two files changed");
        assert!(stats.resolved >= 2, "{stats:?}");
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
        let view = checkout.view().unwrap();
        assert_eq!(
            view.changed_paths(),
            vec![b"src/extra.rs".to_vec(), b"src/lib.rs".to_vec()],
            "a refresh resolves the call graph but the paths are still changed since the publication"
        );
        drop(view);
        let after = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(after.contains("src/extra.rs"), "{after}");
        assert!(!after.contains("src/lib.rs"), "{after}");

        // Nothing changed since: a second refresh does nothing, and the text
        // generation only moves with a recorded change.
        let generation = checkout.view().unwrap().text_generation();
        assert!(checkout.refresh_callgraph().unwrap().is_none());
        assert_eq!(checkout.view().unwrap().text_generation(), generation);
        // Recording a path whose content did not change changes nothing.
        checkout.record_paths(&[b"src/extra.rs".to_vec()]).unwrap();
        assert_eq!(checkout.view().unwrap().text_generation(), generation);
        let extra_again = "use crate::util;\npub fn other() { util::callee(); util::callee(); }\n";
        write(root.path(), "src/extra.rs", extra_again);
        checkout.record_paths(&[b"src/extra.rs".to_vec()]).unwrap();
        assert_ne!(checkout.view().unwrap().text_generation(), generation);
        checkout.refresh_callgraph().unwrap();

        // The dead-code snapshot follows the refreshed graph: `callee` is
        // still called (from src/extra.rs), and the snapshot equals a cold
        // build's.
        let mut refreshed_snapshot = checkout.view().unwrap().rows_dead_code_snapshot().unwrap();

        // The answers equal a cold build of the same files.
        let cold_store = tempfile::tempdir().unwrap();
        let cold = Checkout::bind(
            Arc::new(DataLayer::new(8 << 20, 2)),
            cold_store.path(),
            family,
            root.path(),
        )
        .unwrap();
        cold.build().unwrap();
        assert_eq!(
            callers_of(&cold.view().unwrap(), "src/util.rs", "callee"),
            after
        );
        let mut cold_snapshot = cold.view().unwrap().rows_dead_code_snapshot().unwrap();
        refreshed_snapshot.generated_at = None;
        cold_snapshot.generated_at = None;
        assert_eq!(
            format!("{refreshed_snapshot:?}"),
            format!("{cold_snapshot:?}"),
            "the refreshed dead-code snapshot equals a cold build's"
        );
        assert!(
            refreshed_snapshot
                .outbound_calls
                .iter()
                .any(|call| call.caller_file.ends_with("src/extra.rs")),
            "the new caller is in the snapshot: {refreshed_snapshot:?}"
        );
    }

    /// Recording an edit indexes its text at once and leaves parsing it to
    /// the call graph: the tool call that records the edit returns without
    /// running the extractor, text queries already see the new bytes, and
    /// the refresh that follows extracts the file once and answers from it.
    #[test]
    fn an_edit_is_searchable_at_once_and_parsed_by_the_refresh() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        demo(root.path());
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let checkout = Checkout::bind(layer, store.path(), family(), root.path()).unwrap();
        checkout.build().unwrap();

        let util = "pub fn callee() {}\npub fn zzfresh() { callee(); }\n";
        write(root.path(), "src/util.rs", util);
        checkout.record_paths(&[b"src/util.rs".to_vec()]).unwrap();
        let extracted = || {
            let live = checkout.live.read().unwrap();
            live.changes[&b"src/util.rs".to_vec()]
                .content
                .peek()
                .is_some()
        };
        assert!(!extracted(), "recording an edit does not parse it");
        let view = checkout.view().unwrap();
        let query = plan_pattern("zzfresh", false).unwrap();
        assert_eq!(
            view.text_candidates(&query, Scope::All).unwrap(),
            vec![b"src/util.rs".to_vec()],
            "text queries see the edit at once"
        );
        assert_eq!(
            view.current_state().get(b"src/util.rs").unwrap().language,
            "rust",
            "the edited file keeps its language"
        );
        drop(view);

        checkout
            .refresh_callgraph()
            .unwrap()
            .expect("one file changed");
        assert!(extracted(), "the refresh parsed the edit");
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(callers.contains("zzfresh"), "{callers}");
        assert_eq!(checkout.view().unwrap().status().unparsed, 0);
        // The published change carries the record the refresh extracted.
        checkout.publish_changes().unwrap().expect("one change");
        let callers = callers_of(&checkout.view().unwrap(), "src/util.rs", "callee");
        assert!(callers.contains("zzfresh"), "{callers}");
    }

    #[test]
    fn a_built_checkout_answers_text_and_call_graph_queries_and_survives_a_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write(root.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write(
            root.path(),
            "src/lib.rs",
            "mod util;\npub fn caller() { util::callee(); }\n",
        );
        write(root.path(), "src/util.rs", "pub fn callee() {}\n");
        write(root.path(), "notes.txt", "nothing to see\n");
        let layer = Arc::new(DataLayer::new(8 << 20, 2));
        let family = FamilyId::from_digest(Digest128::of(b"demo"));

        let checkout =
            Checkout::bind(Arc::clone(&layer), store.path(), family, root.path()).unwrap();
        assert!(
            checkout.view().is_none(),
            "nothing is published before a build"
        );
        let report = checkout.build().unwrap();
        assert_eq!(report.files, 4, "{report:?}");
        assert_eq!(report.parsed_files, 2, "{report:?}");
        assert_eq!(report.extract_failures, 0);

        let check = |view: &CheckoutView| {
            let query = plan_pattern("callee", false).unwrap();
            let found = view.text_candidates(&query, Scope::All).unwrap();
            assert_eq!(found, vec![b"src/lib.rs".to_vec(), b"src/util.rs".to_vec()]);
            let callers = callers_of(view, "src/util.rs", "callee");
            assert!(callers.contains("caller"), "{callers}");
            assert!(callers.contains("src/lib.rs"), "{callers}");
            assert!(view.callgraph_failures().is_empty());
        };
        check(&checkout.view().unwrap());
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);

        // The ranking primitives and the dead-code snapshot.
        let view = checkout.view().unwrap();
        let cal = aft_store::text::pack(b'c', b'a', b'l');
        assert_eq!(
            view.paths_with_trigram(cal).unwrap(),
            vec![b"src/lib.rs".to_vec(), b"src/util.rs".to_vec()]
        );
        assert!(view.trigram_frequency(cal).unwrap() >= 2);
        assert!(matches!(
            view.path_text(b"src/lib.rs").unwrap(),
            Some(PathText::Indexed { distinct }) if distinct > 10
        ));
        assert_eq!(view.path_text(b"missing.rs").unwrap(), None);
        let snapshot = view.rows_dead_code_snapshot().unwrap();
        assert_eq!(snapshot.files.len(), 2, "{:?}", snapshot.files);
        assert!(
            snapshot
                .outbound_calls
                .iter()
                .any(|call| format!("{call:?}").contains("callee")),
            "{:?}",
            snapshot.outbound_calls
        );
        drop(view);

        // An edit is visible to text queries at once, and the call graph
        // reports it pending until the next build covers it.
        write(root.path(), "notes.txt", "now mentions callee\n");
        checkout.record_paths(&[b"notes.txt".to_vec()]).unwrap();
        let query = plan_pattern("callee", false).unwrap();
        let three = vec![
            b"notes.txt".to_vec(),
            b"src/lib.rs".to_vec(),
            b"src/util.rs".to_vec(),
        ];
        let view = checkout.view().unwrap();
        assert_eq!(view.text_candidates(&query, Scope::All).unwrap(), three);
        assert_eq!(
            checkout.status().callgraph,
            PlaneStatus::Building {
                done: 0,
                total: Some(1)
            }
        );
        drop(view);
        checkout.build().unwrap();
        assert_eq!(checkout.status().callgraph, PlaneStatus::Ready);
        let view = checkout.view().unwrap();
        assert!(view.changed_paths().is_empty());
        assert_eq!(
            view.text_candidates(&query, Scope::All).unwrap(),
            three,
            "the rebuild indexed the edit"
        );
        drop(view);

        // A restarted process binds the same checkout and serves the
        // published generation without building.
        drop(checkout);
        let restarted = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
        let view = restarted
            .view()
            .expect("the published view is served at bind");
        assert_eq!(view.text_candidates(&query, Scope::All).unwrap(), three);
        let callers = callers_of(&view, "src/util.rs", "callee");
        assert!(callers.contains("src/lib.rs"), "{callers}");
    }
}
