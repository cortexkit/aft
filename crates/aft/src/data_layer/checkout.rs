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
use aft_store::content::{ContentRecord, ContentSegment, RecordBody, RecordKey};
use aft_store::family::{Family, Publication, Snapshot};
use aft_store::graph::{FragmentKey, GraphSegment};
use aft_store::ids::{CheckoutId, ContentKey, Digest128, FamilyId, SegmentId};
use aft_store::scan::{scan, scan_paths, ContentKind, ContentSink, ScanPolicy};
use aft_store::segment::{KnownSection, Segment};
use aft_store::text::{
    candidates, ContentTrigrams, Located, OverlayEntry, PathText, Query, Scope, TextLocator,
    TextMembership, TextOverlay, TextSections, TextSectionsBuilder, TextSegment, TextStatus,
};

use super::callgraph::{CheckoutInputs, FileResolution};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{write_sections, Held, StoredSlices};
use super::incremental::{LiveCheckout, UpdateStats};
use super::overlay::OverlaySlices;
use super::query::{SliceReader, SliceSource};
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};

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
        }
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
    /// The last published generation, and the changes made since.
    live: RwLock<Live>,
    /// The call graph's incremental state between publications, built from
    /// the publication on the first refresh.
    engine: Mutex<Option<GraphEngine>>,
    /// Orders recordings: each one brings the state the previous left up to
    /// date.
    recording: Mutex<()>,
    /// Moves whenever what the checkout holds in memory may have changed: a
    /// recorded change, a call-graph refresh, a new publication. A memory
    /// figure walked at one value stays right until it moves (see
    /// [`Self::memory_epoch`]).
    memory_epoch: std::sync::atomic::AtomicU64,
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
    /// Each re-resolved file's record key, parse and resolution.
    replaced: BTreeMap<String, (String, Arc<ParseBlob>, Arc<FileResolution>)>,
    /// Files the publication has that the checkout no longer parses.
    removed: BTreeSet<String>,
    /// The dead-code snapshot of this call graph, built on first use.
    dead_code: DeadCodeMemo,
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
}

/// How long a build waits for another process to release the store's
/// writer before it gives up and reports the store busy.
const WRITER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many unchanged files' parses, and separately resolutions, the
/// call-graph engine keeps decoded between updates (see `update_engine`).
/// A decoded resolution of a large repository's file is about half a
/// megabyte: at 1,024 files each worktree an agent was editing held 0.5 to
/// 3 GB, and four prefrontal worktrees took the daemon past 20 GB. The next
/// update decodes again what it reads, at a fraction of a millisecond a
/// file.
const ENGINE_DECODED_FILES: usize = 64;

/// One path changed since the publication.
#[derive(Clone)]
struct Change {
    /// The change count of its latest change.
    sequence: u64,
    /// Its record now; `None` once it left the checkout.
    record: Option<PathRecord>,
    /// What the call graph reads from it now.
    extracted: Extracted,
}

/// What the call graph reads from one changed path.
#[derive(Clone)]
enum Extracted {
    /// Nothing: not a parsed language, not text, or gone.
    Nothing,
    /// A parsed file: its parse key and parse, and the content record
    /// they came from, which a publication of the change stores.
    Parse(String, Arc<ParseBlob>, Arc<ContentRecord>),
    /// A resolver configuration file's bytes, and its content record.
    Config(Arc<[u8]>, Arc<ContentRecord>),
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
    /// counted: a build of this checkout cannot fold them.
    pub segments: usize,
    pub ms: f64,
}

/// What a build did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BuildReport {
    pub files: usize,
    pub text_units: usize,
    /// What the new segment left out because a segment another checkout
    /// reads already holds it.
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
        let handle = Self {
            layer,
            planes,
            root: root.to_path_buf(),
            family,
            checkout,
            live: RwLock::new(Live::default()),
            engine: Mutex::new(None),
            recording: Mutex::new(()),
            memory_epoch: std::sync::atomic::AtomicU64::new(0),
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
        Ok(())
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
        state: CheckoutState,
        changed: &[Vec<u8>],
        read: HashMap<Vec<u8>, Vec<u8>>,
    ) -> bool {
        // A record the scan read again keeps its language when its content
        // did not change; a changed text file gets the language its path and
        // bytes choose. Only changed paths are extracted: a file read again
        // only because its stat moved (a touch, a copied checkout) has the
        // content it had.
        let changed_set = changed.iter().collect::<HashSet<_>>();
        let mut extracted = HashMap::new();
        let records = state
            .paths()
            .iter()
            .map(|record| {
                let mut record = record.clone();
                if let Some(bytes) = read
                    .get(&record.path)
                    .filter(|_| changed_set.contains(&record.path))
                {
                    let (language, content) = extract_changed(&record, bytes);
                    record.language = language;
                    extracted.insert(record.path.clone(), content);
                } else if record.language.is_empty() {
                    if let Some(before) = base
                        .get(&record.path)
                        .filter(|before| before.content == record.content)
                    {
                        record.language.clone_from(&before.language);
                    }
                }
                record
            })
            .collect::<Vec<_>>();
        let state = CheckoutState::new(state.git.clone(), state.conversion, records)
            .expect("the scanned state's paths, unchanged")
            .with_root_of(&state);
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
            let entry = match (&record, read.get(path)) {
                (None, _) => OverlayEntry::Deleted,
                (Some(record), Some(bytes)) if record.kind == PathKind::Text => {
                    OverlayEntry::Content(Arc::new(ContentTrigrams::of(bytes)))
                }
                (Some(_), _) => OverlayEntry::Unindexed,
            };
            let content = extracted.remove(path).unwrap_or(Extracted::Nothing);
            entries.push((path.clone(), entry, record, content));
        }
        let mut live = self.write_live();
        if live.generation != generation {
            return false;
        }
        live.sequence += 1;
        self.touch_memory();
        let sequence = live.sequence;
        let mut text = (*live.text).clone();
        let mut pending = (*live.callgraph_pending).clone();
        let mut unparsed = (*live.unparsed).clone();
        for (path, entry, record, content) in entries {
            pending.insert(path.clone());
            let failed =
                matches!(content, Extracted::Nothing) && record.as_ref().is_some_and(is_unparsed);
            if failed {
                unparsed.insert(path.clone());
            } else {
                unparsed.remove(&path);
            }
            text.set(path.clone(), entry);
            live.changes.insert(
                path,
                Change {
                    sequence,
                    record,
                    extracted: content,
                },
            );
        }
        live.text = Arc::new(text);
        live.text_members = Arc::default();
        live.callgraph_pending = Arc::new(pending);
        live.unparsed = Arc::new(unparsed);
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
        let state = state.with_fragments(per_path).map_err(DataError::store)?;
        let written = session
            .write_checkout_state(self.checkout, &state)
            .map_err(DataError::store)?;
        // The new segment holds everything this checkout's new state needs,
        // so an older segment stays only while another checkout reads from
        // it. A checkout whose folder is gone reads nothing any more.
        let unneeded = self.unneeded(&state)?;
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
        let unneeded = self.unneeded(&scanned.state)?;
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
    fn serve_new(&self, snapshot: Snapshot, covered: u64) -> Result<(), DataError> {
        self.touch_memory();
        let published = self
            .open_published(snapshot)?
            .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?;
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
                let failed = matches!(change.extracted, Extracted::Nothing)
                    && change.record.as_ref().is_some_and(is_unparsed);
                if failed {
                    unparsed.insert(path.clone());
                } else {
                    unparsed.remove(path);
                }
            }
            live.unparsed = Arc::new(unparsed);
            live.published = Some(Arc::new(published));
            live.generation += 1;
            // The new publication's graph replaces the old one and whatever
            // was laid over it; the next refresh starts from it.
            live.graph = None;
            live.refreshed = None;
            let mut text = (*live.text).clone();
            text.retain(|path| kept.contains(path));
            live.text = Arc::new(text);
            live.text_members = Arc::default();
            live.callgraph_pending = Arc::new(
                live.callgraph_pending
                    .iter()
                    .filter(|path| kept.contains(*path))
                    .cloned()
                    .collect(),
            );
            let early = std::mem::take(&mut live.early);
            early
                .into_iter()
                .filter(|(_, sequence)| *sequence > covered)
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        };
        // Paths reported before anything was published, after the scan
        // started: the scan may have read them before they changed.
        self.record_paths(&late)?;
        Ok(())
    }

    /// Segments of the current generation that no other checkout's state
    /// reads (see [`Self::segments_read_by`]). Vector segments follow their
    /// own rule ([`super::vectors::needed_vector_segments`]), over every
    /// other checkout's state and `own`, the state this build publishes,
    /// since the build's new segment holds no vectors.
    fn unneeded(&self, own: &CheckoutState) -> Result<Unneeded, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let (states, gone) = match self.other_states(&snapshot)? {
            OtherStates::Read { states, gone } => (states, gone),
            // A state this build cannot read could need any segment.
            OtherStates::Unreadable { gone } => {
                return Ok(Unneeded {
                    segments: Vec::new(),
                    checkouts: gone,
                });
            }
        };
        let others = states.iter().collect::<Vec<_>>();
        let read = self.segments_read_by(&snapshot, &others)?;
        let mut needed = read.needed;
        let mut readers = others;
        readers.push(own);
        let usable = read
            .usable
            .iter()
            .map(|&index| {
                read.opened[index]
                    .as_ref()
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
        let known = known_sections();
        let entries = snapshot.manifest().segments().to_vec();
        // Each segment this build can use, by its place in the manifest;
        // `None` for one it cannot.
        let mut opened = Vec::with_capacity(entries.len());
        for entry in &entries {
            match snapshot.open_segment(entry.id, &known, Arc::clone(&self.layer.cache)) {
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
            for state in states {
                for (position, record) in state.paths().iter().enumerate() {
                    match locator.get(record.content) {
                        Some(Located::Indexed { segment, .. }) => {
                            needed[text_index[segment as usize]] = true;
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
                            Some(holder) => needed[usable[holder]] = true,
                            None => needs_unusable = true,
                        }
                    }
                    if let Some(fragment) = state.fragment(position) {
                        let key = FragmentKey::from_digest(fragment);
                        match newest_holder(&graphs, |graph| graph.find(key))? {
                            Some(holder) => needed[usable[holder]] = true,
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
        let mut states = Vec::new();
        let mut gone = Vec::new();
        for state_ref in snapshot.manifest().checkouts() {
            if state_ref.checkout == self.checkout {
                continue;
            }
            match snapshot.checkout_state(state_ref.checkout) {
                Ok(Some(state)) if state.root().is_some_and(folder_is_gone) => {
                    gone.push(state_ref.checkout);
                }
                Ok(Some(state)) => states.push(state),
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
        Ok(OtherStates::Read { states, gone })
    }

    /// Leaves out of a build's new segment the text units, content records
    /// and fragments that a segment another checkout reads already holds.
    /// Worktrees of one repository hold mostly the same contents, which
    /// each first build used to store again (about 155 MB for each
    /// prefrontal worktree). Such a segment stays while that checkout reads
    /// from it, and this checkout's state then reads from it too, so
    /// retirement keeps it for either. Leaves nothing out when another
    /// checkout's state cannot be read.
    fn leave_out_shared(
        &self,
        snapshot: &Snapshot,
        text: &mut TextSectionsBuilder,
        sections: Option<&mut super::graph_store::SectionsWriter<'_>>,
    ) -> Result<SharedUnits, DataError> {
        let OtherStates::Read { states, .. } = self.other_states(snapshot)? else {
            return Ok(SharedUnits::default());
        };
        if states.is_empty() {
            return Ok(SharedUnits::default());
        }
        let others = states.iter().collect::<Vec<_>>();
        let read = self.segments_read_by(snapshot, &others)?;
        let shared = read
            .usable
            .iter()
            .filter(|&&index| read.needed[index])
            .map(|&index| {
                read.opened[index]
                    .as_ref()
                    .expect("usable segments are open")
            })
            .collect::<Vec<_>>();
        let mut texts = Vec::new();
        for segment in &shared {
            if let Some(found) = TextSegment::open(segment).map_err(DataError::store)? {
                texts.push(found);
            }
        }
        let locator = TextLocator::build(&texts).map_err(DataError::store)?;
        let contents = shared
            .iter()
            .map(|segment| ContentSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let graphs = shared
            .iter()
            .map(|segment| GraphSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let mut left = SharedUnits::default();
        let before = text.len();
        text.retain(|key| locator.get(key).is_none());
        left.text_units = before - text.len();
        if let Some(sections) = sections {
            // A lookup that fails counts as not held: the unit is written
            // again rather than named where it cannot be read.
            let (records, fragments) = sections.leave_out(
                |key| {
                    contents
                        .iter()
                        .flatten()
                        .any(|content| matches!(content.find(key), Ok(Some(_))))
                },
                |key| {
                    graphs
                        .iter()
                        .flatten()
                        .any(|graph| matches!(graph.find(key), Ok(Some(_))))
                },
            );
            left.records = records;
            left.fragments = fragments;
        }
        Ok(left)
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
        drop(slot);
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
        }
        let engine = slot.as_mut().expect("started");
        let changed = changes
            .iter()
            .filter(|(path, change)| engine.applied.get(*path) != Some(&change.sequence))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut stats = None;
        if !changed.is_empty() {
            let next = next_inputs(state, &engine.live.inputs, changes, &changed);
            let changed_names = changed
                .iter()
                .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_string))
                .collect::<BTreeSet<_>>();
            let update = engine.live.update(next, &changed_names);
            // The engine reads stored parses and resolutions as it goes; a
            // read that failed made it work from an empty one.
            let failures = published.graph.failures();
            let update = match update {
                Ok(update) if failures.is_empty() => update,
                // A failed update may have changed some of the engine's state
                // and not the rest; the caller drops it, so the next refresh
                // starts a new engine from the publication.
                Err(error) => return Err(DataError::Resolve(error.to_string())),
                Ok(_) => {
                    return Err(DataError::Damaged(format!(
                        "reading the stored call graph failed: {}",
                        failures.join("; ")
                    )))
                }
            };
            // Only the files this update touched get new slices; every file
            // touched by an earlier update keeps the slice it got then.
            for file in &update.touched {
                match (
                    engine.live.resolved.get_shared(file),
                    engine.live.inputs.parses.get(file),
                ) {
                    (Some(resolution), Some((key, parse))) => {
                        engine.overlay.replace_lazy(
                            file,
                            key,
                            Arc::clone(parse),
                            Arc::clone(&resolution),
                        );
                        engine.refreshed.removed.remove(file);
                        // Shared with the engine: a later update that changes
                        // this file copies it then, and only then.
                        engine
                            .refreshed
                            .replaced
                            .insert(file.clone(), (key.clone(), Arc::clone(parse), resolution));
                    }
                    _ => {
                        engine.overlay.remove(file);
                        engine.refreshed.replaced.remove(file);
                        engine.refreshed.removed.insert(file.clone());
                    }
                }
            }
            engine.refreshed.state = engine.live.inputs.state.clone();
            for path in &changed {
                engine.applied.insert(path.clone(), changes[path].sequence);
            }
            stats = Some(update);
            // An update keeps every parse and resolution it decoded. A broad
            // one reads thousands (every file declaring a method named `new`,
            // say), so past a budget the unchanged ones go and a later update
            // decodes again what it reads. The changed ones stay, since only
            // the engine holds them until the next publication.
            if engine.live.inputs.parses.decoded_count() > ENGINE_DECODED_FILES {
                engine.live.inputs.parses.release_decoded();
            }
            if engine.live.resolved.decoded_unchanged_count() > ENGINE_DECODED_FILES {
                engine.live.resolved.release_unchanged();
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
    /// When the store cannot be written; nothing is published then.
    pub fn publish_changes(&self) -> Result<Option<DeltaReport>, DataError> {
        let started = Instant::now();
        let (published, changes, covered, text) = {
            let live = self.live.read().expect("no panics while holding it");
            let Some(published) = live.published.clone() else {
                return Ok(None);
            };
            if live.building || live.changes.is_empty() || !published.graph_built {
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
            let resolved = files
                .iter()
                .filter_map(|file| {
                    engine
                        .live
                        .resolved
                        .get_shared(file)
                        .map(|resolution| (file.clone(), resolution))
                })
                .collect::<BTreeMap<_, _>>();
            (engine.live.inputs.clone(), resolved)
        };
        // The changed contents' records and text, each content once.
        let mut records = HashMap::new();
        let mut text_units = TextSectionsBuilder::new();
        let mut seen = HashSet::new();
        for (path, change) in &changes {
            let Some(record) = &change.record else {
                continue;
            };
            let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
            match &change.extracted {
                Extracted::Parse(_, _, stored) | Extracted::Config(_, stored) => {
                    records.insert(key.digest().to_string(), (key, Arc::clone(stored)));
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
        let mut pending = session.create_segment().map_err(DataError::store)?;
        text_units
            .write(pending.writer())
            .map_err(DataError::store)?;
        let fragments = write_sections(
            pending.writer(),
            &inputs,
            &resolved,
            &records,
            false,
            None,
            None,
        )
        .map_err(DataError::Store)?;
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
            .with_fragments(per_path)
            .map_err(DataError::store)?;
        let written = session
            .write_checkout_state(self.checkout, &state)
            .map_err(DataError::store)?;
        session
            .publish(&Publication {
                add_segments: vec![segment],
                states: vec![written],
                ..Publication::default()
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
            records: records.len(),
            text_units: text_unit_count,
            segments: read
                .needed
                .iter()
                .zip(&others_read)
                .filter(|(own, other)| **own && !**other)
                .count(),
            ms: 0.0,
        };
        self.serve_new(snapshot, covered)?;
        // The engine started from the publication this one replaces, so the
        // next refresh would start a new one anyway; dropping it now gives
        // back what it decoded and resolved as soon as the checkout goes
        // quiet, instead of holding it until the next edit.
        if let Ok(mut slot) = self.engine.lock() {
            if slot
                .as_ref()
                .is_some_and(|engine| Arc::ptr_eq(&engine.published, &published))
            {
                *slot = None;
            }
        }
        Ok(Some(DeltaReport {
            ms: elapsed_ms(started),
            ..report
        }))
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
            self.layer.threads,
            timings,
        )
        .map_err(|error| DataError::Resolve(error.to_string()))?;
        Ok(GraphEngine {
            published: Arc::clone(published),
            live,
            applied: BTreeMap::new(),
            overlay: OverlaySlices::new(Arc::clone(&published.graph)),
            refreshed: Refreshed {
                state: published.state.clone(),
                replaced: BTreeMap::new(),
                removed: BTreeSet::new(),
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
            match snapshot.open_segment(entry.id, &known, Arc::clone(&self.layer.cache)) {
                Ok(segment) => segments.push(Arc::new(segment)),
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
        // Each parsed path's record key follows from its content and
        // language; its fragment is the one the state names.
        let mut files = Vec::new();
        let mut unparsed = BTreeSet::new();
        let graph_built = state.has_fragments();
        for (index, record) in state.paths().iter().enumerate() {
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
        // A state an older extractor version published names records this
        // build keys differently, so none is found. Its text still serves;
        // its call graph counts as not built, so the checkout's own bind
        // builds it again (see `needs_build`) and a read-only bind of it
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
    /// The dead-code snapshot `aft_inspect` analyses, built from every
    /// file's stored record and fragment in one pass.
    ///
    /// # Errors
    ///
    /// When a record or fragment cannot be read.
    pub(crate) fn dead_code_snapshot(
        &self,
    ) -> Result<crate::inspect::job::CallgraphSnapshot, DataError> {
        self.dead_code_snapshot_shared()
            .map(|snapshot| crate::inspect::job::CallgraphSnapshot::clone(&snapshot))
    }

    /// [`Self::dead_code_snapshot`], built once per call graph: every view
    /// of the same publication and refresh shares it.
    ///
    /// # Errors
    ///
    /// When a record or fragment cannot be read.
    pub(crate) fn dead_code_snapshot_shared(
        &self,
    ) -> Result<Arc<crate::inspect::job::CallgraphSnapshot>, DataError> {
        let memo = match &self.refreshed {
            Some(refreshed) => &refreshed.dead_code,
            None => &self.published.dead_code,
        };
        memo.get_or_build(|| self.build_dead_code_snapshot())
    }

    fn build_dead_code_snapshot(
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
                for (file, (key, parse, resolution)) in &refreshed.replaced {
                    parses.insert(file.clone(), (key.clone(), Arc::clone(parse)));
                    resolved.insert(file.clone(), FileResolution::clone(resolution));
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
        match changes.get(path).map(|change| &change.extracted) {
            Some(Extracted::Parse(key, parse, _)) => {
                parses.insert(name.to_string(), (key.clone(), Arc::clone(parse)));
            }
            Some(Extracted::Config(source, _)) => {
                configs.insert(path.clone(), Arc::clone(source));
            }
            Some(Extracted::Nothing) | None => {}
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
    let same = |a: &PathRecord, b: &PathRecord| {
        a.kind == b.kind && a.content == b.content && a.mode == b.mode && a.link == b.link
    };
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

/// Whether `record` is a text file in a parsed language that has no
/// resolution: given that it has none, its extraction failed. Resolver
/// configuration files are read raw and never resolved.
fn is_unparsed(record: &PathRecord) -> bool {
    matches!(record.kind, PathKind::Text | PathKind::Large)
        && !record.language.is_empty()
        && record.language != "config"
}

/// The language a changed file is parsed as, and what the call graph reads
/// from it. As in a build, a file the extractor fails on is not parsed.
fn extract_changed(record: &PathRecord, bytes: &[u8]) -> (String, Extracted) {
    if !matches!(record.kind, PathKind::Text | PathKind::Large) {
        return (String::new(), Extracted::Nothing);
    }
    let Some(language) = std::str::from_utf8(&record.path)
        .ok()
        .and_then(|path| language_for(path, bytes))
    else {
        return (String::new(), Extracted::Nothing);
    };
    let Ok(extracted) = extract(bytes, &language) else {
        return (language, Extracted::Nothing);
    };
    let content = match &extracted.data {
        RecordBody::Config { source } => {
            let source = Arc::from(source.as_slice());
            Extracted::Config(source, Arc::new(extracted))
        }
        RecordBody::Parsed { .. } => match blob_from_record(&extracted) {
            Ok(CallgraphBlob::Parse(parse)) => {
                let key = RecordKey::new(record.content, &language, EXTRACTOR_VERSION);
                Extracted::Parse(
                    key.digest().to_string(),
                    Arc::new(parse),
                    Arc::new(extracted),
                )
            }
            _ => Extracted::Nothing,
        },
    };
    (language, content)
}

/// The index of the newest segment `find` locates a key in.
/// What [`Checkout::segments_read_by`] found in one generation.
struct SegmentsRead {
    entries: Vec<aft_store::family::SegmentEntry>,
    /// Each segment by its place in the manifest; `None` when this build
    /// cannot use it.
    opened: Vec<Option<Segment>>,
    /// The places of the segments in `opened` that are open.
    usable: Vec<usize>,
    /// By place in the manifest: some given state reads from it.
    needed: Vec<bool>,
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
        gone: Vec<CheckoutId>,
    },
    /// One of them cannot be read: it could need any segment.
    Unreadable { gone: Vec<CheckoutId> },
}

/// What a build left out of its new segment because a segment another
/// checkout reads already holds it.
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
}

/// A build sink's results, after the scan.
struct Collected {
    text: TextSectionsBuilder,
    seen_text: HashSet<ContentKey>,
    languages: HashMap<Vec<u8>, String>,
    records: HashMap<String, (RecordKey, Arc<ContentRecord>)>,
    failures: usize,
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
        match extract(bytes, &language) {
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

impl aft_store::heap::HeapBytes for Extracted {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Nothing => 0,
            Self::Parse(key, parse, record) => {
                key.heap_bytes(seen) + parse.heap_bytes(seen) + record.heap_bytes(seen)
            }
            Self::Config(bytes, record) => bytes.heap_bytes(seen) + record.heap_bytes(seen),
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
            dead_code,
        } = refreshed;
        self.shared += state.heap_bytes(seen);
        self.callgraph +=
            replaced.heap_bytes(seen) + removed.heap_bytes(seen) + dead_code.heap_bytes(seen);
    }
}

impl Checkout {
    /// The folder of the family store this checkout is bound to.
    #[must_use]
    pub fn family_dir(&self) -> &Path {
        self.family.dir()
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
                parts.callgraph += change.extracted.heap_bytes(seen);
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
                dead_code,
            } = refreshed;
            state.heap_bytes(&mut seen)
                + removed.heap_bytes(&mut seen)
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
}

/// Outline's symbols for one file, from its stored content record.
impl CheckoutView {
    /// The symbols outline lists for the file at `path` (relative to the
    /// root, `/`-separated), as the record of its published content holds
    /// them, with that content's key and language. `None` when the path
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
        let Some(published) = self.published.state.get(path) else {
            return Ok(None);
        };
        let unchanged = self.current_state().get(path).is_some_and(|current| {
            current.content == published.content && current.language == published.language
        });
        if !unchanged || published.language.is_empty() || published.language == "config" {
            return Ok(None);
        }
        let key = RecordKey::new(published.content, &published.language, EXTRACTOR_VERSION);
        for segment in self.published.segments.iter().rev() {
            let Some(content) = ContentSegment::open(segment).map_err(DataError::store)? else {
                continue;
            };
            let Some(ordinal) = content.find(key).map_err(DataError::store)? else {
                continue;
            };
            let record = content.record(ordinal).map_err(DataError::store)?;
            let RecordBody::Parsed { body, .. } = record.data else {
                return Ok(None);
            };
            return Ok(body.outline.map(|symbols| super::outline::StoredOutline {
                content: published.content,
                language: published.language.clone(),
                symbols,
            }));
        }
        Ok(None)
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
    /// state and a parse are vectors and strings, so the two agree to the
    /// byte; a slice's B-tree maps are counted as full nodes, a lower bound
    /// on what a copy allocates.
    #[test]
    fn the_census_walk_adds_up_what_the_allocator_gave_a_copy() {
        use aft_store::heap::heap_bytes_of;
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
        assert_eq!(heap_bytes_of(&copy), allocated);

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

    /// A second worktree of a repository stores only what the first one's
    /// segment does not already hold, and answers as a cold build does.
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
        // `Cargo.toml`, `src/lib.rs` and `notes.txt` are the same in both
        // worktrees, and so is how `src/lib.rs` resolves; `src/util.rs`
        // differs.
        assert_eq!(built.shared.text_units, 3, "{built:?}");
        assert_eq!(built.shared.records, 2, "{built:?}");
        assert_eq!(built.shared.fragments, 1, "{built:?}");
        let view = second.view().unwrap();
        let callers = callers_of(&view, "src/util.rs", "callee");
        assert!(
            callers.contains("caller") && callers.contains("helper"),
            "{callers}"
        );
        assert_eq!(candidates_for(&view, "nothing to see"), ["notes.txt"]);
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
        let mut refreshed_snapshot = checkout.view().unwrap().dead_code_snapshot().unwrap();

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
        let mut cold_snapshot = cold.view().unwrap().dead_code_snapshot().unwrap();
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
        let snapshot = view.dead_code_snapshot().unwrap();
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
