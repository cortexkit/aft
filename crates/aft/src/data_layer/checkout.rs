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
    TextMembership, TextOverlay, TextSectionsBuilder, TextSegment, TextStatus,
};

use super::callgraph::{resolve_checkout, CheckoutInputs, FileResolution};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{write_sections, StoredSlices};
use super::incremental::{LiveCheckout, UpdateStats};
use super::overlay::OverlaySlices;
use super::query::{SliceReader, SliceSource};
use super::slice::FileSlice;
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};

/// Why the data layer could not do what was asked.
#[derive(Debug)]
pub enum DataError {
    /// The family store failed: its files could not be read or written.
    Store(String),
    /// The checkout could not be scanned.
    Scan(String),
    /// The call graph could not be resolved.
    Resolve(String),
    /// Another process kept the repository's store busy past the wait.
    Busy(String),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "data store: {error}"),
            Self::Scan(error) => write!(f, "checkout scan: {error}"),
            Self::Resolve(error) => write!(f, "call-graph resolution: {error}"),
            Self::Busy(error) => write!(f, "data store busy: {error}"),
        }
    }
}

impl std::error::Error for DataError {}

impl DataError {
    fn store(error: impl std::fmt::Display) -> Self {
        Self::Store(error.to_string())
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
            policy: ScanPolicy::default(),
        }
    }
}

/// Every section a segment of this data layer can hold.
fn known_sections() -> Vec<KnownSection> {
    aft_store::text::KNOWN
        .into_iter()
        .chain(super::graph_store::known_sections())
        .collect()
}

/// One bound checkout.
pub struct Checkout {
    layer: Arc<DataLayer>,
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
}

/// A checkout's call graph kept up to date between publications.
struct GraphEngine {
    /// The publication the engine started from; a newer one replaces it.
    published: Arc<Published>,
    live: LiveCheckout,
    /// Each changed path's change count as the engine last applied it.
    applied: BTreeMap<Vec<u8>, u64>,
    /// Files whose resolution changed since the publication.
    dirty: BTreeSet<String>,
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
    /// Paths changed since the publication whose call graph is not
    /// resolved yet.
    callgraph_pending: Arc<BTreeSet<Vec<u8>>>,
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
}

/// The files resolved again since a publication, with the paths they were
/// resolved over.
struct Refreshed {
    state: CheckoutState,
    /// Each re-resolved file's record key, parse and resolution.
    replaced: BTreeMap<String, (String, Arc<ParseBlob>, FileResolution)>,
    /// Files the publication has that the checkout no longer parses.
    removed: BTreeSet<String>,
}

/// How long a build waits for another process to release the store's
/// writer before it gives up and reports the store busy.
const WRITER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// A parsed file: its parse key and parse.
    Parse(String, Arc<ParseBlob>),
    /// A resolver configuration file's bytes.
    Config(Arc<[u8]>),
}

/// What a build did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BuildReport {
    pub files: usize,
    pub text_units: usize,
    pub parsed_files: usize,
    pub extract_failures: usize,
    pub scan_ms: f64,
    pub resolve_ms: f64,
    pub write_ms: f64,
    pub publish_ms: f64,
    pub segment_bytes: u64,
    /// Segments no checkout needs any more, left out of the new generation.
    pub retired_segments: usize,
    /// Another process published this checkout while the build ran, so
    /// nothing was published; the checkout serves that publication.
    pub superseded: bool,
    /// Files the reclamation after publishing removed, or why it could not
    /// run (the publication stands either way).
    pub reclaimed_files: usize,
    pub reclaim_error: Option<String>,
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
        let checkout = CheckoutId::from_digest(Digest128::of(root.as_os_str().as_encoded_bytes()));
        let family = Family::open(family_dir, family).map_err(DataError::store)?;
        let handle = Self {
            layer,
            root: root.to_path_buf(),
            family,
            checkout,
            live: RwLock::new(Live::default()),
            engine: Mutex::new(None),
            recording: Mutex::new(()),
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
        {
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
            live.callgraph_pending = Arc::new(BTreeSet::new());
            live.graph = None;
            live.refreshed = None;
        }
        self.apply_scan(&published.state, scanned.state, &changed, sink.finish());
        Ok(())
    }

    /// The checkout's root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
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
            text: Arc::clone(&live.text),
            callgraph_pending: Arc::clone(&live.callgraph_pending),
            graph,
            refreshed: live.refreshed.clone(),
            sequence: live.sequence,
            unparsed: Arc::clone(&live.unparsed),
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
        if let Some(view) = self.view() {
            return CheckoutStatus {
                building,
                ..view.status()
            };
        }
        let plane = if building {
            PlaneStatus::Building {
                done: 0,
                total: None,
            }
        } else {
            PlaneStatus::Off
        };
        CheckoutStatus {
            text: plane,
            callgraph: plane,
            building,
            unparsed: 0,
        }
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
        let base = {
            let live = self.live.read().expect("no panics while holding it");
            live.state.clone().or_else(|| {
                live.published
                    .as_ref()
                    .map(|published| Arc::new(published.state.clone()))
            })
        };
        let Some(base) = base else {
            let mut live = self.write_live();
            live.sequence += 1;
            let sequence = live.sequence;
            for path in paths {
                live.early.insert(path.clone(), sequence);
            }
            return Ok(());
        };
        let sink = ChangeSink::default();
        let scanned = scan_paths(&self.root, &self.layer.policy, &base, paths, &sink)
            .map_err(|error| DataError::Scan(error.to_string()))?;
        self.apply_scan(&base, scanned.state, &scanned.changed, sink.finish());
        Ok(())
    }

    /// Makes `state` the checkout's current state, recording each path in
    /// `changed` (compared with `base`) as a change, with the files the scan
    /// read in `read`.
    fn apply_scan(
        &self,
        base: &CheckoutState,
        state: CheckoutState,
        changed: &[Vec<u8>],
        read: HashMap<Vec<u8>, Vec<u8>>,
    ) {
        // A record the scan read again keeps its language when its content
        // did not change; a changed text file gets the language its path and
        // bytes choose.
        let mut extracted = HashMap::new();
        let records = state
            .paths()
            .iter()
            .map(|record| {
                let mut record = record.clone();
                if let Some(bytes) = read.get(&record.path) {
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
            .expect("the scanned state's paths, unchanged");
        if changed.is_empty() {
            self.write_live().state = Some(Arc::new(state));
            return;
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
        live.sequence += 1;
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
        live.callgraph_pending = Arc::new(pending);
        live.unparsed = Arc::new(unparsed);
        live.state = Some(Arc::new(state));
    }

    /// Scans the checkout, builds every plane cold, publishes them, and
    /// serves the new view.
    ///
    /// # Errors
    ///
    /// When the scan, resolution or store fails. Nothing is published then,
    /// and the previous view, if any, keeps serving.
    pub fn build(&self) -> Result<BuildReport, DataError> {
        self.write_live().building = true;
        let result = self.build_inner();
        self.write_live().building = false;
        result
    }

    fn build_inner(&self) -> Result<BuildReport, DataError> {
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
        let mut report = BuildReport::default();
        let started = Instant::now();
        let sink = BuildSink::default();
        let scanned = scan(&self.root, &self.layer.policy, None, &sink)
            .map_err(|error| DataError::Scan(error.to_string()))?;
        let mut collected = sink.finish();
        // Contents too large to read are members whose text is not
        // indexed; record them so grep reports them rather than walking
        // them.
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
                .map_err(DataError::store)?;
        let inputs = collected.inputs(&state);
        report.parsed_files = inputs.parses.len();

        let started = Instant::now();
        let resolved = resolve_checkout(&inputs, self.layer.threads)
            .map_err(|error| DataError::Resolve(error.to_string()))?;
        report.resolve_ms = elapsed_ms(started);

        let started = Instant::now();
        let deadline = Instant::now() + WRITER_WAIT;
        let mut session = loop {
            if let Some(session) = self.family.try_write().map_err(DataError::store)? {
                break session;
            }
            if Instant::now() >= deadline {
                return Err(DataError::Busy(format!(
                    "another process held the store's writer for over {} s",
                    WRITER_WAIT.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
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
        let mut pending = session.create_segment().map_err(DataError::store)?;
        collected
            .text
            .write(pending.writer())
            .map_err(DataError::store)?;
        let fragments = write_sections(pending.writer(), &inputs, &resolved, &collected.records)
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
        // it.
        let retired = self.unneeded_segments()?;
        report.retired_segments = retired.len();
        session
            .publish(&Publication {
                add_segments: vec![segment],
                remove_segments: retired,
                states: vec![written],
                ..Publication::default()
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
        let published = self
            .open_published(snapshot)?
            .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?;
        // The scan read every file as it was once the scan started, so the
        // build covers every change recorded before then. A change recorded
        // while it ran may be newer than what it read, so it stays laid over
        // the new publication.
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
            // The new publication's graph replaces the old one and whatever
            // was laid over it; the next refresh starts from it.
            live.graph = None;
            live.refreshed = None;
            let mut text = (*live.text).clone();
            text.retain(|path| kept.contains(path));
            live.text = Arc::new(text);
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
        Ok(report)
    }

    /// Segments of the current generation that no other checkout's state
    /// reads: for every text unit, content record and fragment another
    /// checkout needs, readers take the newest segment that holds it, so
    /// only those segments are kept.
    fn unneeded_segments(&self) -> Result<Vec<SegmentId>, DataError> {
        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let known = known_sections();
        let entries = snapshot.manifest().segments().to_vec();
        let segments = entries
            .iter()
            .map(|entry| {
                snapshot
                    .open_segment(entry.id, &known, Arc::clone(&self.layer.cache))
                    .map_err(DataError::store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // TextLocator numbers only the segments that have a text section,
        // so keep each one's place in the manifest beside it.
        let mut text_index = Vec::new();
        let mut texts = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            if let Some(text) = TextSegment::open(segment).map_err(DataError::store)? {
                text_index.push(index);
                texts.push(text);
            }
        }
        let contents = segments
            .iter()
            .map(|segment| ContentSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let graphs = segments
            .iter()
            .map(|segment| GraphSegment::open(segment).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let locator = TextLocator::build(&texts).map_err(DataError::store)?;
        let mut needed = vec![false; entries.len()];
        for state_ref in snapshot.manifest().checkouts() {
            if state_ref.checkout == self.checkout {
                continue;
            }
            let Some(state) = snapshot
                .checkout_state(state_ref.checkout)
                .map_err(DataError::store)?
            else {
                continue;
            };
            for (index, record) in state.paths().iter().enumerate() {
                if let Some(Located::Indexed { segment, .. }) = locator.get(record.content) {
                    needed[text_index[segment as usize]] = true;
                }
                if !record.language.is_empty() {
                    let key = RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION);
                    if let Some(holder) = newest_holder(&contents, |content| content.find(key))? {
                        needed[holder] = true;
                    }
                }
                if let Some(fragment) = state.fragment(index) {
                    let key = FragmentKey::from_digest(fragment);
                    if let Some(holder) = newest_holder(&graphs, |graph| graph.find(key))? {
                        needed[holder] = true;
                    }
                }
            }
        }
        Ok(entries
            .iter()
            .zip(needed)
            .filter(|(_, needed)| !needed)
            .map(|(entry, _)| entry.id)
            .collect())
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
        if changes.is_empty() {
            return Ok(None);
        }
        let mut slot = self.engine.lock().expect("no panics while holding it");
        if slot
            .as_ref()
            .is_some_and(|engine| !Arc::ptr_eq(&engine.published, &published))
        {
            *slot = None;
        }
        if slot.is_none() {
            *slot = Some(self.start_engine(&published)?);
        }
        let engine = slot.as_mut().expect("started");
        let changed = changes
            .iter()
            .filter(|(path, change)| engine.applied.get(*path) != Some(&change.sequence))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut stats = None;
        if !changed.is_empty() {
            let next = next_inputs(&state, &engine.live.inputs, &changes, &changed);
            let changed_names = changed
                .iter()
                .filter_map(|path| std::str::from_utf8(path).ok().map(str::to_string))
                .collect::<BTreeSet<_>>();
            let update = engine
                .live
                .update(next, &changed_names)
                .map_err(|error| DataError::Resolve(error.to_string()))?;
            engine.dirty.extend(update.touched.iter().cloned());
            for path in &changed {
                engine.applied.insert(path.clone(), changes[path].sequence);
            }
            stats = Some(update);
        }
        let mut overlay = OverlaySlices::new(Arc::clone(&published.graph));
        let mut refreshed = Refreshed {
            state: engine.live.inputs.state.clone(),
            replaced: BTreeMap::new(),
            removed: BTreeSet::new(),
        };
        for file in &engine.dirty {
            match (
                engine.live.resolved.get(file),
                engine.live.inputs.parses.get(file),
            ) {
                (Some(resolution), Some((key, parse))) => {
                    overlay.replace(FileSlice::build(file, key, parse, resolution));
                    refreshed.replaced.insert(
                        file.clone(),
                        (key.clone(), Arc::clone(parse), resolution.clone()),
                    );
                }
                _ => {
                    overlay.remove(file);
                    refreshed.removed.insert(file.clone());
                }
            }
        }
        let mut live = self.write_live();
        if live
            .published
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &published))
        {
            let graph: Arc<dyn SliceSource + Send + Sync> = Arc::new(overlay);
            live.graph = Some(graph);
            live.refreshed = Some(Arc::new(refreshed));
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

    /// The incremental state for `published`: every stored resolution, and
    /// the parses and configuration the publication was built from.
    fn start_engine(&self, published: &Arc<Published>) -> Result<GraphEngine, DataError> {
        let (parses, resolved) = published.graph.resolutions().map_err(DataError::Store)?;
        let configs = published
            .graph
            .configs(&published.state)
            .map_err(DataError::Store)?;
        let inputs = CheckoutInputs {
            state: published.state.clone(),
            parses,
            configs,
        };
        let live = LiveCheckout::from_resolved(inputs, resolved, self.layer.threads)
            .map_err(|error| DataError::Resolve(error.to_string()))?;
        Ok(GraphEngine {
            published: Arc::clone(published),
            live,
            applied: BTreeMap::new(),
            dirty: BTreeSet::new(),
        })
    }

    /// This checkout's publication in `snapshot`, or `None` when the
    /// snapshot has no state for it.
    fn open_published(&self, snapshot: Snapshot) -> Result<Option<Published>, DataError> {
        let Some(state) = snapshot
            .checkout_state(self.checkout)
            .map_err(DataError::store)?
        else {
            return Ok(None);
        };
        let known = known_sections();
        let segments = snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| {
                snapshot
                    .open_segment(entry.id, &known, Arc::clone(&self.layer.cache))
                    .map(Arc::new)
                    .map_err(DataError::store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let identity = format!(
            "{}:{}",
            snapshot.generation(),
            Digest128::of(&state.encode())
        );
        Published::open(self.root.clone(), identity, state, segments, snapshot).map(Some)
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
    membership: TextMembership,
    graph: Arc<StoredSlices>,
    /// Paths in a parsed language with no stored resolution: their
    /// extraction failed.
    unparsed: BTreeSet<Vec<u8>>,
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
    pub callgraph: PlaneStatus,
    /// A build is running. Answers still come from the current view, which
    /// covers every change recorded so far.
    pub building: bool,
    /// Files in a parsed language that the call graph does not cover,
    /// because extracting them failed. The plane can be ready with some.
    pub unparsed: usize,
}

/// A checkout's data as queries see it at one moment: the last publication
/// with the changes recorded since. Cheap to take; it holds the publication
/// open until it drops, so a query never sees a half-applied change.
#[derive(Clone)]
pub struct CheckoutView {
    published: Arc<Published>,
    text: Arc<TextOverlay>,
    callgraph_pending: Arc<BTreeSet<Vec<u8>>>,
    graph: Arc<dyn SliceSource + Send + Sync>,
    /// What the call graph resolved since the publication.
    refreshed: Option<Arc<Refreshed>>,
    /// The checkout's change count when the view was taken.
    sequence: u64,
    unparsed: Arc<BTreeSet<Vec<u8>>>,
}

impl Published {
    fn open(
        root: PathBuf,
        identity: String,
        state: CheckoutState,
        segments: Vec<Arc<Segment>>,
        snapshot: Snapshot,
    ) -> Result<Self, DataError> {
        let texts = segments
            .iter()
            .filter_map(|segment| TextSegment::open(segment).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(DataError::store)?;
        let text = TextLocator::build(&texts).map_err(DataError::store)?;
        let membership = TextMembership::build(&state, &text);
        drop(texts);
        // Each parsed path's record key follows from its content and
        // language; its fragment is the one the state names.
        let mut files = Vec::new();
        let mut unparsed = BTreeSet::new();
        for (index, record) in state.paths().iter().enumerate() {
            let Some(fragment) = state.fragment(index) else {
                if is_unparsed(record) {
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
        let graph = StoredSlices::open_files(segments.clone(), &files).map_err(DataError::Store)?;
        Ok(Self {
            root,
            identity,
            state,
            segments,
            membership,
            graph: Arc::new(graph),
            unparsed,
            _snapshot: snapshot,
        })
    }

    fn texts(&self) -> Result<Vec<TextSegment<'_>>, DataError> {
        self.segments
            .iter()
            .filter_map(|segment| TextSegment::open(segment).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(DataError::store)
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
            0 => PlaneStatus::Ready,
            pending => PlaneStatus::Building {
                done: 0,
                total: Some(pending),
            },
        };
        CheckoutStatus {
            text,
            callgraph,
            building: false,
            unparsed: self.unparsed.len(),
        }
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
        aft_store::text::paths_with_trigram(
            &published.state,
            &published.membership,
            &texts,
            &self.text,
            trigram,
        )
        .map_err(DataError::store)
    }

    /// The dead-code snapshot `aft_inspect` analyses, built from every
    /// file's stored record and fragment in one pass.
    ///
    /// # Errors
    ///
    /// When a record or fragment cannot be read.
    pub(crate) fn dead_code_snapshot(
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
                    resolved.insert(file.clone(), resolution.clone());
                }
                refreshed.state.clone()
            }
            None => self.published.state.clone(),
        };
        let inputs = CheckoutInputs {
            state,
            parses,
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
            Some(Extracted::Parse(key, parse)) => {
                parses.insert(name.to_string(), (key.clone(), Arc::clone(parse)));
            }
            Some(Extracted::Config(source)) => {
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
    record.kind == PathKind::Text && !record.language.is_empty() && record.language != "config"
}

/// The language a changed file is parsed as, and what the call graph reads
/// from it. As in a build, a file the extractor fails on is not parsed.
fn extract_changed(record: &PathRecord, bytes: &[u8]) -> (String, Extracted) {
    if record.kind != PathKind::Text {
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
        RecordBody::Config { source } => Extracted::Config(Arc::from(source.as_slice())),
        RecordBody::Parsed { .. } => match blob_from_record(&extracted) {
            Ok(CallgraphBlob::Parse(parse)) => {
                let key = RecordKey::new(record.content, &language, EXTRACTOR_VERSION);
                Extracted::Parse(key.digest().to_string(), Arc::new(parse))
            }
            _ => Extracted::Nothing,
        },
    };
    (language, content)
}

/// The index of the newest segment `find` locates a key in.
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
        if kind == ContentKind::Text {
            self.read
                .lock()
                .expect("no panics while holding it")
                .insert(path.to_vec(), bytes.to_vec());
        }
    }
}

/// What a build's scan collects from the files it reads.
#[derive(Default)]
struct BuildSink {
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
        if first {
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
            }
        }
        if kind != ContentKind::Text {
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
    /// configuration file's bytes.
    fn inputs(&self, state: &CheckoutState) -> CheckoutInputs {
        let mut parses = BTreeMap::new();
        let mut configs = HashMap::new();
        let mut decoded = HashMap::<String, Arc<ParseBlob>>::new();
        for record in state.paths() {
            if record.language.is_empty() {
                continue;
            }
            let Ok(path) = std::str::from_utf8(&record.path) else {
                continue;
            };
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
                    let parse = if let Some(parse) = decoded.get(&parse_key) {
                        Arc::clone(parse)
                    } else {
                        let Ok(CallgraphBlob::Parse(parse)) = blob_from_record(stored) else {
                            continue;
                        };
                        let parse = Arc::new(parse);
                        decoded.insert(parse_key.clone(), Arc::clone(&parse));
                        parse
                    };
                    parses.insert(path.to_string(), (parse_key, parse));
                }
            }
        }
        CheckoutInputs {
            state: state.clone(),
            parses,
            configs,
        }
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

    fn candidates_for(view: &CheckoutView, pattern: &str) -> Vec<String> {
        let query = plan_pattern(pattern, false).unwrap();
        view.text_candidates(&query, Scope::All)
            .unwrap()
            .into_iter()
            .map(|path| String::from_utf8(path).unwrap())
            .collect()
    }

    /// A small Rust crate whose `caller` calls `util::callee`.
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

        // A file over the size limit is a member that is not parsed, as in a
        // build.
        let mut big = String::from("use crate::util;\npub fn big() { util::callee(); }\n");
        while big.len() <= 1024 * 1024 {
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
            format!("{refreshed_snapshot:?}").contains("src/extra.rs"),
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
