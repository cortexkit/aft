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

use std::collections::{BTreeMap, HashMap, HashSet};
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
    candidates, ContentTrigrams, Query, Scope, TextLocator, TextMembership, TextOverlay,
    TextSectionsBuilder, TextSegment, TextStatus,
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
    view: RwLock<Option<Arc<CheckoutView>>>,
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
            view: RwLock::new(None),
        };
        let snapshot = handle.family.snapshot().map_err(DataError::store)?;
        if let Some(view) = handle.open_view(&snapshot)? {
            *handle.view.write().expect("no panics while holding it") = Some(Arc::new(view));
        }
        Ok(handle)
    }

    /// The checkout's root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The published view, if the checkout has been built.
    #[must_use]
    pub fn view(&self) -> Option<Arc<CheckoutView>> {
        self.view
            .read()
            .expect("no panics while holding it")
            .clone()
    }

    /// Scans the checkout, builds every plane cold, publishes them, and
    /// serves the new view.
    ///
    /// # Errors
    ///
    /// When the scan, resolution or store fails. Nothing is published then,
    /// and the previous view, if any, keeps serving.
    pub fn build(&self) -> Result<BuildReport, DataError> {
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
        let view = self
            .open_view(&snapshot)?
            .ok_or_else(|| DataError::Store("the published state is missing".to_string()))?;
        report.segment_bytes = snapshot
            .manifest()
            .segments()
            .iter()
            .map(|entry| entry.len)
            .sum();
        *self.view.write().expect("no panics while holding it") = Some(Arc::new(view));
        Ok(report)
    }

    /// The view of this checkout in `snapshot`, or `None` when the
    /// snapshot has no state for it.
    fn open_view(&self, snapshot: &Snapshot) -> Result<Option<CheckoutView>, DataError> {
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
        CheckoutView::open(self.root.clone(), state, segments).map(Some)
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// A published generation of one checkout: its state and segments, and
/// what each plane needs to answer from them.
pub struct CheckoutView {
    root: PathBuf,
    state: CheckoutState,
    segments: Vec<Arc<Segment>>,
    membership: TextMembership,
    graph: Arc<StoredSlices>,
}

impl CheckoutView {
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

    /// The checkout's root folder.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The checkout's paths, in byte order, with their contents and stat
    /// evidence.
    #[must_use]
    pub fn state(&self) -> &CheckoutState {
        &self.state
    }

    /// How many members' contents no segment indexes yet; they are always
    /// candidates.
    #[must_use]
    pub fn unindexed_text(&self) -> usize {
        self.membership.missing()
    }

    /// The paths in `scope` that may match `query`, plus `overlay`'s paths
    /// in scope that may, in byte order. Every match is among them; some
    /// may not match, so the caller reads each and checks.
    ///
    /// # Errors
    ///
    /// When a segment cannot be read.
    pub fn text_candidates(
        &self,
        query: &Query,
        scope: Scope<'_>,
        overlay: &TextOverlay,
    ) -> Result<Vec<Vec<u8>>, DataError> {
        let texts = self
            .segments
            .iter()
            .filter_map(|segment| TextSegment::open(segment).transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(DataError::store)?;
        candidates(&self.state, &self.membership, &texts, overlay, query, scope)
            .map_err(DataError::store)
    }

    /// The call graph of this generation, as the call-graph commands read
    /// it.
    #[must_use]
    pub(crate) fn callgraph(&self) -> SliceReader<Arc<StoredSlices>> {
        SliceReader::new(
            Arc::clone(&self.graph),
            self.root.clone(),
            self.root.display().to_string(),
        )
    }

    /// Reads of the call graph that failed: a checksum or a malformed
    /// record. A caller that saw a file as absent can check here whether
    /// that was a failure.
    #[must_use]
    pub fn callgraph_failures(&self) -> Vec<String> {
        self.graph.failures()
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
            let found = view
                .text_candidates(&query, Scope::All, &TextOverlay::new())
                .unwrap();
            assert_eq!(found, vec![b"src/lib.rs".to_vec(), b"src/util.rs".to_vec()]);
            let callers = callers_of(view, "src/util.rs", "callee");
            assert!(callers.contains("caller"), "{callers}");
            assert!(callers.contains("src/lib.rs"), "{callers}");
            assert!(view.callgraph_failures().is_empty());
        };
        check(&checkout.view().unwrap());

        // A restarted process binds the same checkout and serves the
        // published generation without building.
        drop(checkout);
        let restarted = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
        check(
            &restarted
                .view()
                .expect("the published view is served at bind"),
        );
    }
}
