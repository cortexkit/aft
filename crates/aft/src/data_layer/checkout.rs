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
use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::family::{Family, Publication, Snapshot};
use aft_store::graph::FragmentKey;
use aft_store::ids::{CheckoutId, ContentKey, Digest128, FamilyId};
use aft_store::scan::{scan, ContentKind, ContentSink, ScanPolicy};
use aft_store::segment::{KnownSection, Segment};
use aft_store::text::{
    candidates, ContentTrigrams, OverlayEntry, PathText, Query, Scope, TextLocator, TextMembership,
    TextOverlay, TextSectionsBuilder, TextSegment, TextStatus,
};

use super::callgraph::{resolve_checkout, CheckoutInputs};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{write_sections, StoredSlices};
use super::query::SliceReader;
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
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "data store: {error}"),
            Self::Scan(error) => write!(f, "checkout scan: {error}"),
            Self::Resolve(error) => write!(f, "call-graph resolution: {error}"),
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
}

/// A checkout's current data: its last publication and the changes made
/// since, which queries see in place of the published records.
#[derive(Default)]
struct Live {
    published: Option<Arc<Published>>,
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
    changed_at: BTreeMap<Vec<u8>, u64>,
}

/// One changed path, as an edit or the watcher reports it.
#[derive(Clone, Debug)]
pub enum PathChange {
    /// The file at the path now holds these bytes.
    Written { path: Vec<u8>, bytes: Arc<[u8]> },
    /// The path no longer exists.
    Removed { path: Vec<u8> },
}

impl PathChange {
    fn path(&self) -> &[u8] {
        match self {
            Self::Written { path, .. } | Self::Removed { path } => path,
        }
    }
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
        };
        let snapshot = handle.family.snapshot().map_err(DataError::store)?;
        if let Some(published) = handle.open_published(&snapshot)? {
            handle.write_live().published = Some(Arc::new(published));
        }
        Ok(handle)
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
        Some(CheckoutView {
            published: Arc::clone(live.published.as_ref()?),
            text: Arc::clone(&live.text),
            callgraph_pending: Arc::clone(&live.callgraph_pending),
        })
    }

    /// How each plane stands, for status and for replies that disclose
    /// what they could not cover.
    #[must_use]
    pub fn status(&self) -> CheckoutStatus {
        if let Some(view) = self.view() {
            return view.status();
        }
        let plane = if self
            .live
            .read()
            .expect("no panics while holding it")
            .building
        {
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
        }
    }

    /// Records changed paths, so queries see them at once: text queries
    /// read each written file's new trigrams, and the call graph reports
    /// the paths as pending until they are resolved again.
    pub fn record_changes(&self, changes: &[PathChange]) {
        if changes.is_empty() {
            return;
        }
        let max = self.layer.policy.max_indexed_len;
        let entries = changes
            .iter()
            .map(|change| {
                let entry = match change {
                    PathChange::Removed { .. } => OverlayEntry::Deleted,
                    PathChange::Written { bytes, .. } => {
                        if bytes.len() as u64 > max || bytes.contains(&0) {
                            OverlayEntry::Unindexed
                        } else {
                            OverlayEntry::Content(Arc::new(ContentTrigrams::of(bytes)))
                        }
                    }
                };
                (change.path().to_vec(), entry)
            })
            .collect::<Vec<_>>();
        let mut live = self.write_live();
        live.sequence += 1;
        let sequence = live.sequence;
        let mut text = (*live.text).clone();
        let mut pending = (*live.callgraph_pending).clone();
        for (path, entry) in entries {
            pending.insert(path.clone());
            live.changed_at.insert(path.clone(), sequence);
            text.set(path, entry);
        }
        live.text = Arc::new(text);
        live.callgraph_pending = Arc::new(pending);
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
        let mut session = loop {
            if let Some(session) = self.family.try_write().map_err(DataError::store)? {
                break session;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
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
        session
            .publish(&Publication {
                add_segments: vec![segment],
                states: vec![written],
                ..Publication::default()
            })
            .map_err(DataError::store)?;
        drop(session);
        report.publish_ms = elapsed_ms(started);

        let snapshot = self.family.snapshot().map_err(DataError::store)?;
        let published = self
            .open_published(&snapshot)?
            .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?;
        report.segment_bytes = snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| entry.len)
            .sum();
        // The scan read every file as it was once the scan started, so the
        // build covers every change recorded before then. A change recorded
        // while it ran may be newer than what it read, so it stays laid over
        // the new publication.
        let mut live = self.write_live();
        live.published = Some(Arc::new(published));
        live.changed_at.retain(|_, sequence| *sequence > covered);
        let kept = live.changed_at.keys().cloned().collect::<BTreeSet<_>>();
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
        Ok(report)
    }

    /// This checkout's publication in `snapshot`, or `None` when the
    /// snapshot has no state for it.
    fn open_published(&self, snapshot: &Snapshot) -> Result<Option<Published>, DataError> {
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
        Published::open(self.root.clone(), state, segments).map(Some)
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
    state: CheckoutState,
    segments: Vec<Arc<Segment>>,
    membership: TextMembership,
    graph: Arc<StoredSlices>,
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
}

/// A checkout's data as queries see it at one moment: the last publication
/// with the changes recorded since. Cheap to take; it holds the publication
/// open until it drops, so a query never sees a half-applied change.
#[derive(Clone)]
pub struct CheckoutView {
    published: Arc<Published>,
    text: Arc<TextOverlay>,
    callgraph_pending: Arc<BTreeSet<Vec<u8>>>,
}

impl Published {
    fn open(
        root: PathBuf,
        state: CheckoutState,
        segments: Vec<Arc<Segment>>,
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
        for (index, record) in state.paths().iter().enumerate() {
            let Some(fragment) = state.fragment(index) else {
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
            state,
            segments,
            membership,
            graph: Arc::new(graph),
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

    /// The paths changed since the publication, which queries answer from
    /// the changes rather than from [`state`](Self::state).
    #[must_use]
    pub fn changed_paths(&self) -> Vec<Vec<u8>> {
        self.callgraph_pending.iter().cloned().collect()
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
        CheckoutStatus { text, callgraph }
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
        let (parses, resolved) = self
            .published
            .graph
            .resolutions()
            .map_err(DataError::Store)?;
        let inputs = CheckoutInputs {
            state: self.published.state.clone(),
            parses,
            configs: HashMap::new(),
        };
        let rows = super::graph_rows::assemble(&inputs, &resolved);
        Ok(super::dead_code::snapshot(&self.published.root, &rows))
    }

    /// The call graph of the publication, as the call-graph commands read
    /// it. Files in [`changed_paths`](Self::changed_paths) answer as they
    /// were published until they are resolved again; the status says so.
    #[must_use]
    pub(crate) fn callgraph(&self) -> SliceReader<Arc<StoredSlices>> {
        SliceReader::new(
            Arc::clone(&self.published.graph),
            self.published.root.clone(),
            self.published.root.display().to_string(),
        )
    }

    /// Reads of the call graph that failed: a checksum or a malformed
    /// record. A caller that saw a file as absent can check here whether
    /// that was a failure.
    #[must_use]
    pub fn callgraph_failures(&self) -> Vec<String> {
        self.published.graph.failures()
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
        checkout.record_changes(&[PathChange::Written {
            path: b"notes.txt".to_vec(),
            bytes: Arc::from(&b"now mentions callee\n"[..]),
        }]);
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
