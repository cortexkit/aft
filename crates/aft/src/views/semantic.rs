//! Semantic plane of a per-checkout view, reusing the shared-base overlay.
//!
//! A checkout's semantic answer is the overlay the resident semantic index
//! already implements (`SemanticIndex` over a `SharedSemanticBase`), keyed by
//! view content keys instead of by one root's artifact:
//!
//! - **Base.** The runs the current generation names as `Ready`, decoded once
//!   into the family arena when the generation is admitted and shared by every
//!   view that names the same key.
//! - **Replacements.** Vectors for paths whose current content is not the
//!   base's: live edits, entries the generation still has pending, and every
//!   member after a producer (model, chunker or template) change. They come
//!   from this checkout's fill map, keyed by `(path, content)`.
//! - **Tombstones.** Base files that are superseded, deleted or no longer
//!   members. They are never scored.
//!
//! Keys hash the content, the path (the embedded text names the file) and the
//! producer, so identical content embeds once per family: two views, and two
//! sessions of one view, reuse the same key instead of calling the model again.
//! A fill looks a key up in the arena, then in the family store, and embeds
//! only true misses, claiming each one so a concurrent fill of another view
//! waits for it rather than embedding it too.
//!
//! Pending and failed work is recorded in the published manifest, so it
//! survives folds, eviction and restart; completions are admitted only for the
//! content and producer they were computed for.
//!
//! Registration is opt-in: constructing a plane changes no routing.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane, FamilyStoreReader, PutOrTouch};
use crate::blob_store::{FullKey, SemanticKey, SEMANTIC_PRODUCER_VERSION};
use crate::refresh::{FailureClass, FailureTracker, PreparedWork};
use crate::semantic_index::{
    EmbedTextCaps, SemanticIndex, SemanticResult, ViewPayloadProducer, ViewSemanticBase,
};

use super::contracts::{PlaneAdapter, PlaneError, ViewAccess};
use super::manifest_v2::{EntryV2, ManifestV2};
use super::readiness::{
    Admission, Completion, FillMap, LiveEntries, PlaneReadiness, PlaneState, WorkItem,
};
use super::registry::ViewRegistration;
use super::segment_store::rel_path_to_os;
use super::snapshot::{DiskState, LiveEntry, OpenGeneration, Snapshot, Source, WatcherState};
use super::RelPath;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn plane_error(reason: impl Into<String>) -> PlaneError {
    PlaneError {
        plane: FamilyPlane::Semantic,
        reason: reason.into(),
    }
}

/// Everything that decides a semantic payload: the chunker, the embedding
/// text template (including its size caps) and the model. Each is part of
/// every key, so vectors of another producer are never reused or scored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticProducer {
    pub chunker_version: String,
    pub template_version: String,
    pub model_fingerprint: String,
    pub caps: EmbedTextCaps,
}

impl SemanticProducer {
    /// This release's chunker and template for `model_fingerprint`, which the
    /// runtime derives from its embedding configuration.
    pub fn current(model_fingerprint: impl Into<String>, caps: EmbedTextCaps) -> Self {
        Self {
            chunker_version: SEMANTIC_PRODUCER_VERSION.to_owned(),
            // The caps change the embedded text, so they are template identity.
            template_version: format!(
                "{SEMANTIC_PRODUCER_VERSION};caps={}/{}/{}/{}",
                caps.signature_chars, caps.body_lines, caps.body_chars, caps.total_chars
            ),
            model_fingerprint: model_fingerprint.into(),
            caps,
        }
    }

    /// The producer string recorded in manifest headers.
    pub fn id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            &self.chunker_version,
            &self.template_version,
            &self.model_fingerprint,
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        format!("semantic-{}", hasher.finalize().to_hex())
    }

    fn full_key(&self, bytes: &[u8], rel_path: &RelPath) -> FullKey {
        SemanticKey::from_bytes(
            bytes,
            rel_path.as_bytes(),
            self.chunker_version.as_str(),
            self.template_version.as_str(),
            self.model_fingerprint.as_str(),
        )
        .full_key()
    }

    /// The family key of `bytes` at `rel_path` under this producer.
    pub fn key(&self, bytes: &[u8], rel_path: &RelPath) -> FamilyKey {
        FamilyKey::from(&self.full_key(bytes, rel_path))
    }

    fn payload(&self) -> ViewPayloadProducer<'_> {
        ViewPayloadProducer {
            chunker_version: &self.chunker_version,
            template_version: &self.template_version,
            model_fingerprint: &self.model_fingerprint,
        }
    }
}

/// Whether the semantic plane has work for `rel_path`.
pub fn applies_to(rel_path: &RelPath) -> bool {
    !rel_path.is_synthetic()
        && rel_path_to_os(rel_path)
            .is_ok_and(|path| crate::semantic_index::is_semantic_indexed_extension(&path))
}

/// The key of a file's bytes, computed during strict reconciliation from the
/// buffer that was hashed. Publication uses it to mark content that any view
/// or earlier session already embedded as ready without a model call.
#[derive(Clone, Debug)]
pub struct SemanticAttachment {
    pub content: ContentHash,
    pub producer: String,
    pub key: FamilyKey,
}

/// How much one fill may do for one checkout.
#[derive(Clone, Copy, Debug)]
pub struct FillBudget {
    /// Work items taken per fill; the rest stay queued for the next fill.
    pub max_files: usize,
    /// Texts per model call.
    pub max_batch: usize,
    /// How long to wait for another view that is embedding a shared key.
    pub shared_wait: Duration,
}

impl Default for FillBudget {
    fn default() -> Self {
        Self {
            max_files: 256,
            max_batch: 64,
            shared_wait: Duration::from_secs(60),
        }
    }
}

/// What one fill did. `model_calls` and `embedded_texts` count real calls
/// into the embedding function; every other counter is work that was avoided
/// or deferred.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FillReport {
    /// Work items queued when the fill started.
    pub queued: usize,
    /// Items left for a later fill because of the budget.
    pub deferred: usize,
    /// Items whose file no longer held the queued content.
    pub moved: usize,
    /// Keys already resident in the family arena.
    pub resident_hits: usize,
    /// Keys found in the family store (another view or an earlier session).
    pub stored_hits: usize,
    /// Keys another view of this process embedded while this fill waited.
    pub shared_waits: usize,
    /// Keys this fill embedded.
    pub embedded_keys: usize,
    pub embedded_texts: usize,
    pub model_calls: usize,
    /// Completions installed into the fill map.
    pub installed: usize,
    /// Completions dropped because content or producer moved on.
    pub dropped: usize,
    /// Items recorded as deterministic failures.
    pub failed: usize,
    /// Transient errors; their items stay pending.
    pub errors: Vec<String>,
}

/// A semantic answer for one snapshot. Paths without current vectors are
/// named so the caller can report `complete: false` with a named gap.
#[derive(Clone, Debug)]
pub struct SemanticQuery {
    pub results: Vec<SemanticResult>,
    pub pending: Vec<PathBuf>,
    pub failed: Vec<PathBuf>,
    /// True when membership or content could not be vouched for (the watcher
    /// was not healthy); membership was re-walked, but vectors may lag disk.
    pub unvouched: bool,
}

impl SemanticQuery {
    pub fn complete(&self) -> bool {
        self.pending.is_empty() && self.failed.is_empty() && !self.unvouched
    }
}

/// The resident semantic state of one open generation.
#[derive(Debug)]
struct Resident {
    base: ViewSemanticBase,
    /// Base members and the content their runs were computed for.
    members: BTreeMap<RelPath, ContentHash>,
    /// Failures the generation records for this producer.
    failed: BTreeMap<RelPath, ContentHash>,
}

type CacheKey = (String, [u8; 32], u64, bool);

/// Per-checkout mutable state: installed completions and failures that are
/// not folded yet, the live pin protecting their keys, and the last overlay.
#[derive(Default)]
struct ViewState {
    fill: FillMap,
    fill_version: u64,
    failed: BTreeMap<(RelPath, ContentHash), String>,
    tracker: FailureTracker,
    live: Option<crate::pins::LivePin>,
    cache: Option<(CacheKey, Arc<SemanticIndex>)>,
}

type ViewKey = (String, String);
type ResidentKey = (String, String, String);

/// The semantic plane: one per process, serving every view of every family.
pub struct SemanticPlane {
    storage: PathBuf,
    producer: SemanticProducer,
    views: Mutex<BTreeMap<ViewKey, Arc<Mutex<ViewState>>>>,
    residents: Mutex<BTreeMap<ResidentKey, Arc<Resident>>>,
    arenas: Mutex<BTreeMap<String, Arc<super::semantic_arena::SemanticArena>>>,
    /// Separator of the native relative paths the overlay holds; see
    /// [`overlay_rel_path`]. Always the platform's own outside tests.
    separator: char,
}

/// Converts a view path into the relative path the semantic overlay holds.
///
/// View keys, manifests and fill maps name files by `RelPath`: bytes with `/`
/// separators on every platform. The overlay is a `SemanticIndex`, whose
/// chunk paths, tombstones and search results use the platform's native form,
/// the form a full build produces from walker paths (`src\a.rs` on Windows).
/// That native form also appears in the text that is embedded, so view
/// vectors, tombstones and results only agree with a full build when every
/// view path is converted here, once, as it enters the overlay. Nothing in
/// the overlay compares a `RelPath` with a native path.
pub(crate) fn overlay_rel_path(rel_path: &RelPath, separator: char) -> Option<PathBuf> {
    if separator == '/' {
        return rel_path_to_os(rel_path).ok();
    }
    let text = std::str::from_utf8(rel_path.as_bytes()).ok()?;
    Some(PathBuf::from(text.replace('/', &separator.to_string())))
}

impl SemanticPlane {
    /// A plane for the views stored under `storage`, the process's AFT
    /// storage root.
    pub fn new(storage: PathBuf, producer: SemanticProducer) -> Self {
        Self {
            storage,
            producer,
            views: Mutex::new(BTreeMap::new()),
            residents: Mutex::new(BTreeMap::new()),
            arenas: Mutex::new(BTreeMap::new()),
            separator: std::path::MAIN_SEPARATOR,
        }
    }

    /// Lets a test on any platform run the overlay with Windows' separator.
    /// The arena caches runs with their overlay paths, so a plane with another
    /// separator must use its own storage root.
    #[cfg(test)]
    pub(crate) fn with_path_separator(mut self, separator: char) -> Self {
        self.separator = separator;
        self
    }

    fn overlay_path(&self, rel_path: &RelPath) -> Option<PathBuf> {
        overlay_rel_path(rel_path, self.separator)
    }

    pub fn semantic_producer(&self) -> &SemanticProducer {
        &self.producer
    }

    /// The family arena this plane uses for `family`.
    pub fn arena(&self, family: &str) -> Arc<super::semantic_arena::SemanticArena> {
        Arc::clone(
            lock(&self.arenas)
                .entry(family.to_owned())
                .or_insert_with(|| super::semantic_arena::arena_for(&self.storage, family)),
        )
    }

    fn view(&self, access: &ViewAccess) -> Arc<Mutex<ViewState>> {
        Arc::clone(
            lock(&self.views)
                .entry((access.family().to_owned(), access.scope().to_owned()))
                .or_default(),
        )
    }

    fn resident(&self, access: &ViewAccess, generation: &str) -> Option<Arc<Resident>> {
        lock(&self.residents)
            .get(&(
                access.family().to_owned(),
                access.scope().to_owned(),
                generation.to_owned(),
            ))
            .cloned()
    }

    fn store_reader(access: &ViewAccess) -> Result<Option<FamilyStoreReader>, PlaneError> {
        match access {
            ViewAccess::Owner(owner) => owner
                .open_store(FamilyPlane::Semantic)
                .map(|store| Some(store.reader()))
                .map_err(|error| plane_error(error.to_string())),
            ViewAccess::Reader { registration, .. } => registration
                .open_store(FamilyPlane::Semantic)
                .map_err(|error| plane_error(error.to_string())),
        }
    }

    /// Makes `generation`'s semantic base resident, admitting every run it
    /// names into the family arena. Runs another view already admitted are
    /// shared, not decoded again.
    fn admit(
        &self,
        access: &ViewAccess,
        generation: &OpenGeneration,
    ) -> Result<Resident, PlaneError> {
        let manifest = generation.manifest();
        let id = self.producer.id();
        let compatible = manifest.header().producers.semantic.as_deref() == Some(id.as_str());
        let mut runs = Vec::new();
        let mut members = BTreeMap::new();
        let mut failed = BTreeMap::new();
        if compatible {
            let arena = self.arena(access.family());
            let store = Self::store_reader(access)?;
            for (path, entry) in manifest.entries() {
                let (Some(content), Some(state)) =
                    (entry.content(), entry.plane_state(FamilyPlane::Semantic))
                else {
                    continue;
                };
                match state {
                    PlaneState::Ready { key } => {
                        let loaded = match (
                            crate::blob_store::v2::parse_hex32(key),
                            store.as_ref(),
                            self.overlay_path(path),
                        ) {
                            (Some(bytes), Some(store), Some(relative)) => arena
                                .load(
                                    store,
                                    &FamilyKey::new(FamilyPlane::Semantic, bytes),
                                    &relative,
                                    &self.producer.payload(),
                                )
                                .ok()
                                .flatten(),
                            _ => None,
                        };
                        // A run that cannot be loaded leaves the path out of
                        // the base, so it is queued and refilled like any
                        // other path without current vectors.
                        if let Some(run) = loaded {
                            runs.push(run);
                            members.insert(path.clone(), content);
                        }
                    }
                    PlaneState::Failed { producer, .. } if *producer == id => {
                        failed.insert(path.clone(), content);
                    }
                    _ => {}
                }
            }
        }
        Ok(Resident {
            base: ViewSemanticBase::new(runs),
            members,
            failed,
        })
    }

    /// Where the vectors for `path` at `content` come from in this snapshot.
    fn vector_source(
        state: &ViewState,
        resident: &Resident,
        snapshot: &Snapshot,
        path: &RelPath,
        content: ContentHash,
    ) -> VectorSource {
        // A live entry supersedes the generation: its base run, if any, holds
        // other content and must never be scored.
        let live = matches!(snapshot.source(path), Source::Live(_));
        if !live && resident.members.get(path) == Some(&content) {
            return VectorSource::Base;
        }
        if let Some(key) = state.fill.get(path, &content, FamilyPlane::Semantic) {
            return VectorSource::Fill(*key);
        }
        if state.failed.contains_key(&(path.clone(), content))
            || (!live && resident.failed.get(path) == Some(&content))
        {
            return VectorSource::Failed;
        }
        VectorSource::Pending
    }

    /// Work for this checkout: every current member with no vectors for its
    /// current content under this producer, and no recorded failure.
    fn work_items(
        &self,
        state: &ViewState,
        resident: &Resident,
        snapshot: &Snapshot,
    ) -> Vec<WorkItem> {
        let producer = self.producer.id();
        snapshot
            .membership()
            .into_iter()
            .filter_map(|(path, disk)| {
                let DiskState::Present { content, .. } = disk else {
                    return None;
                };
                (applies_to(&path)
                    && matches!(
                        Self::vector_source(state, resident, snapshot, &path, content),
                        VectorSource::Pending
                    ))
                .then(|| WorkItem {
                    plane: FamilyPlane::Semantic,
                    rel_path: path,
                    content,
                    producer: producer.clone(),
                })
            })
            .collect()
    }

    /// Embeds this checkout's semantic work within `budget`.
    ///
    /// Each item's bytes are read and checked against the queued content, so
    /// a file that moved on is left for the next fill. Keys are looked up in
    /// the family arena, then the family store, and only misses reach
    /// `embed`; a key another view of this process is embedding is waited
    /// for, not embedded again. Every key goes into this view's live pin
    /// before it is stored or touched. Completions are admitted against
    /// `current()`, the checkout's snapshot once the work is done, and only
    /// for the content and producer they were computed for.
    pub fn fill<F>(
        &self,
        owner: &ViewRegistration,
        snapshot: &Snapshot,
        budget: FillBudget,
        embed: &mut F,
        current: &dyn Fn() -> Snapshot,
    ) -> Result<FillReport, PlaneError>
    where
        F: FnMut(Vec<String>) -> Result<Vec<Vec<f32>>, String>,
    {
        let access = ViewAccess::Owner(owner.clone());
        if owner.registry().storage() != self.storage {
            return Err(plane_error("view belongs to another storage root"));
        }
        let resident = self
            .resident(&access, snapshot.generation().name())
            .ok_or_else(|| plane_error("semantic generation not resident"))?;
        let view = self.view(&access);
        let mut report = FillReport::default();
        // The view's state is locked only for short bookkeeping steps, never
        // across file reads, model calls or store writes, so queries and
        // publication of this checkout proceed while it fills.
        let mut items = {
            let mut state = lock(&view);
            if state.live.is_none() {
                state.live = Some(
                    crate::pins::LivePin::create(owner)
                        .map_err(|error| plane_error(error.to_string()))?,
                );
            }
            self.work_items(&state, &resident, snapshot)
        };
        report.queued = items.len();
        report.deferred = items.len().saturating_sub(budget.max_files);
        items.truncate(budget.max_files);

        let root = owner.root().to_path_buf();
        let mut prepared = Vec::new();
        let mut by_path = BTreeMap::new();
        for item in items {
            // The file is read through the OS path; `relative` is the overlay
            // form its chunks and embedded text carry.
            let (Ok(source), Some(relative)) = (
                rel_path_to_os(&item.rel_path),
                self.overlay_path(&item.rel_path),
            ) else {
                continue;
            };
            match std::fs::read(root.join(&source)) {
                Ok(bytes) if ContentHash::of(&bytes) == item.content => {
                    prepared.push(PreparedWork {
                        rel_path: item.rel_path.as_bytes().to_vec(),
                        full_key: self.producer.full_key(&bytes, &item.rel_path),
                        payload: bytes,
                    });
                    by_path.insert(item.rel_path.as_bytes().to_vec(), (item, relative));
                }
                _ => report.moved += 1,
            }
        }
        // Equal keys name equal bytes; grouping them makes each key's work
        // happen once however many paths ask for it.
        let mut groups = crate::refresh::deduplicate_full_keys(prepared)
            .map_err(|error| plane_error(error.to_string()))?;
        {
            let state = lock(&view);
            groups.retain(|group| !state.tracker.is_quarantined(&group.full_key));
        }

        let arena = self.arena(owner.family());
        let store = owner
            .open_store(FamilyPlane::Semantic)
            .map_err(|error| plane_error(error.to_string()))?;
        let payload_producer = self.producer.payload();
        let mut ready = Vec::new();
        let mut claimed = Vec::new();
        let mut waiting = Vec::new();
        for group in groups {
            let key = FamilyKey::from(&group.full_key);
            let Some((item, relative)) = group.rel_paths.first().and_then(|path| by_path.get(path))
            else {
                continue;
            };
            if arena.get(&key).is_some() {
                report.resident_hits += 1;
                ready.push((item.clone(), key));
            } else if arena
                .load(&store.reader(), &key, relative, &payload_producer)
                .map_err(plane_error)?
                .is_some()
            {
                report.stored_hits += 1;
                ready.push((item.clone(), key));
            } else if let Some(claim) = arena.claim(key) {
                claimed.push((item.clone(), relative.clone(), group, claim));
            } else {
                waiting.push((item.clone(), relative.clone(), key));
            }
        }

        // Chunk what this fill must embed, from the bytes that were hashed.
        let mut to_embed = Vec::new();
        let mut chunks = Vec::new();
        let mut failures = Vec::new();
        for (item, relative, group, claim) in claimed {
            match crate::semantic_index::chunk_view_file(
                &root,
                &relative,
                &group.payload,
                self.producer.caps,
            ) {
                Ok(file_chunks) => {
                    chunks.push(file_chunks.unwrap_or_default());
                    to_embed.push((item, relative, group.full_key, claim));
                }
                Err(reason) => failures.push((item, group.full_key, reason)),
            }
        }
        let mut transient = Vec::new();
        let mut succeeded = Vec::new();
        if !to_embed.is_empty() {
            let (mut calls, mut texts) = (0usize, 0usize);
            let embedded = {
                let mut counted = |batch: Vec<String>| {
                    calls += 1;
                    texts += batch.len();
                    embed(batch)
                };
                crate::semantic_index::embed_view_files(chunks, &mut counted, budget.max_batch)
            };
            report.model_calls += calls;
            report.embedded_texts += texts;
            match embedded {
                Ok(runs) => {
                    for ((item, relative, full_key, claim), mut run) in
                        to_embed.into_iter().zip(runs)
                    {
                        let key = *claim.key();
                        let payload = run.encode_view_payload(&payload_producer);
                        protect(&view, &[key])?;
                        match store.put_or_touch(&key, &payload) {
                            Ok(PutOrTouch::Inserted { .. } | PutOrTouch::Reused { .. }) => {
                                succeeded.push(full_key);
                                run.strip_embed_text();
                                arena.install(key, run);
                                report.embedded_keys += 1;
                                ready.push((item, key));
                            }
                            Ok(PutOrTouch::Quarantined) => report.failed += 1,
                            // Another process stored a payload under the
                            // same key first (a model need not be bit-stable
                            // across calls); read the stored payload so every
                            // view uses the same vectors for the key.
                            Err(crate::blob_store::v2::StoreError::ConflictingPayload(_)) => {
                                if arena
                                    .load(&store.reader(), &key, &relative, &payload_producer)
                                    .map_err(plane_error)?
                                    .is_some()
                                {
                                    report.stored_hits += 1;
                                    ready.push((item, key));
                                }
                            }
                            Err(error) => report.errors.push(error.to_string()),
                        }
                        // Releasing the claim wakes views waiting on the key.
                        drop(claim);
                    }
                }
                Err(error) => {
                    transient.extend(to_embed.into_iter().map(|(_, _, full_key, _)| full_key));
                    report.errors.push(error);
                }
            }
        }

        for (item, relative, key) in waiting {
            arena.wait_released(&key, budget.shared_wait);
            if arena.get(&key).is_some()
                || arena
                    .load(&store.reader(), &key, &relative, &payload_producer)
                    .map_err(plane_error)?
                    .is_some()
            {
                report.shared_waits += 1;
                ready.push((item, key));
            }
        }

        let keys = ready.iter().map(|(_, key)| *key).collect::<Vec<_>>();
        protect(&view, &keys)?;
        // A family GC sweep can delete a blob row between the lookup above and
        // the live-pin write. Such a key is not admitted: its path stays
        // pending and the next fill stores the blob again.
        let swept = store
            .touch(&keys)
            .map_err(|error| plane_error(error.to_string()))?
            .missing
            .into_iter()
            .collect::<HashSet<_>>();
        ready.retain(|(_, key)| !swept.contains(key));
        let now = current();
        let producer = self.producer.id();
        let mut state = lock(&view);
        for full_key in &succeeded {
            state.tracker.record_success(full_key);
        }
        for full_key in &transient {
            state
                .tracker
                .record_failure(owner.family(), full_key, FailureClass::Transient);
        }
        for (item, full_key, reason) in failures {
            state
                .tracker
                .record_failure(owner.family(), &full_key, FailureClass::NonTransient);
            state
                .failed
                .insert((item.rel_path.clone(), item.content), reason);
            report.failed += 1;
        }
        for (item, key) in ready {
            let live = now.live_state(&item.rel_path).copied();
            let admission = state.fill.admit(
                &Completion { item, key },
                &producer,
                live.as_ref(),
                Some(now.generation().manifest()),
            );
            match admission {
                Admission::Installed => report.installed += 1,
                Admission::DroppedContent | Admission::DroppedProducer => report.dropped += 1,
            }
        }
        if report.installed > 0 || report.failed > 0 {
            state.fill_version += 1;
        }
        Ok(report)
    }
}

fn protect(view: &Mutex<ViewState>, keys: &[FamilyKey]) -> Result<(), PlaneError> {
    match lock(view).live.as_mut() {
        Some(pin) => pin
            .protect(keys)
            .map_err(|error| plane_error(error.to_string())),
        None => Err(plane_error("semantic fill has no live pin")),
    }
}

enum VectorSource {
    Base,
    Fill(FamilyKey),
    Failed,
    Pending,
}

/// File names whose content decides walker membership. When one of them
/// differs from the snapshot, the snapshot's membership is not trusted.
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".aftignore", ".ignore"];

/// Whether an ignore file on disk differs from what `snapshot` recorded, so
/// its membership may be out of date. It reads only the ignore files, which
/// lets the first query after an ignore edit notice it even before the
/// watcher's reconcile has reached the snapshot.
fn ignore_rules_changed(
    root: &Path,
    snapshot: &Snapshot,
    members: &BTreeMap<RelPath, DiskState>,
) -> bool {
    let is_ignore_file = |path: &RelPath| {
        path.as_bytes()
            .rsplit(|byte| *byte == b'/')
            .next()
            .is_some_and(|name| {
                IGNORE_FILE_NAMES
                    .iter()
                    .any(|ignore| ignore.as_bytes() == name)
            })
    };
    let on_disk = |path: &RelPath| match rel_path_to_os(path)
        .map(|relative| std::fs::read(root.join(relative)))
    {
        Ok(Ok(bytes)) => DiskState::of_bytes(&bytes),
        _ => DiskState::Absent,
    };
    let recorded_changed = members
        .iter()
        .filter(|(path, _)| is_ignore_file(path))
        .any(|(path, disk)| on_disk(path) != *disk);
    let root_added = IGNORE_FILE_NAMES.iter().any(|name| {
        RelPath::new(name.as_bytes().to_vec()).is_ok_and(|path| {
            !members.contains_key(&path)
                && !matches!(snapshot.source(&path), Source::Live(_))
                && root.join(name).is_file()
        })
    });
    recorded_changed || root_added
}

/// Current walker membership of `root`, paths only.
fn walked_members(root: &Path) -> BTreeSet<RelPath> {
    crate::search_index::walk_project_files(root, &crate::search_index::PathFilters::default())
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(root)
                .ok()
                .and_then(|relative| RelPath::from_os_path(relative).ok())
        })
        .collect()
}

impl SemanticPlane {
    /// A gap path named to the user, in the same native form as results.
    fn absolute(&self, root: &Path, path: &RelPath) -> PathBuf {
        self.overlay_path(path)
            .map(|relative| root.join(relative))
            .unwrap_or_else(|| root.to_path_buf())
    }

    /// Scores `query_vector` against the checkout `snapshot` describes.
    ///
    /// Only current members are scored, each with the vectors of its current
    /// content under this producer: base runs for files unchanged since the
    /// generation, fill-map runs for everything else. Superseded base runs are
    /// tombstoned. Nothing here reads the family store; a generation that is
    /// not resident is an error the caller reports as a gap. Membership comes
    /// from the snapshot unless the watcher cannot vouch for it or an ignore
    /// file changed on disk, in which case the checkout is re-walked.
    pub fn search(
        &self,
        access: &ViewAccess,
        root: &Path,
        snapshot: &Snapshot,
        query_vector: &[f32],
        top_k: usize,
        include: &dyn Fn(&Path) -> bool,
    ) -> Result<SemanticQuery, PlaneError> {
        let resident = self
            .resident(access, snapshot.generation().name())
            .ok_or_else(|| plane_error("semantic generation not resident"))?;
        let arena = self.arena(access.family());
        let view = self.view(access);
        let mut state = lock(&view);
        let mut members = snapshot.membership();
        let unvouched = snapshot.watcher() != WatcherState::Healthy
            || ignore_rules_changed(root, snapshot, &members);
        let mut pending = BTreeSet::new();
        let mut failed = BTreeSet::new();
        if unvouched {
            let walked = walked_members(root);
            members.retain(|path, _| walked.contains(path));
            // Newly included files have no vectors until a fill sees them.
            pending.extend(
                walked
                    .into_iter()
                    .filter(|path| applies_to(path) && !members.contains_key(path)),
            );
        }
        let intent = snapshot.pending_intent().cloned().collect::<BTreeSet<_>>();
        let mut via_base = HashSet::new();
        let mut replacements = Vec::new();
        let mut shape = blake3::Hasher::new();
        for (path, disk) in &members {
            let DiskState::Present { content, .. } = disk else {
                continue;
            };
            if !applies_to(path) {
                continue;
            }
            // An acknowledged write not yet applied has unknown bytes: no
            // stored vector can be trusted for it.
            if intent.contains(path) {
                pending.insert(path.clone());
                continue;
            }
            match Self::vector_source(&state, &resident, snapshot, path, *content) {
                VectorSource::Base => {
                    via_base.insert(path.clone());
                }
                VectorSource::Fill(key) => match arena.get(&key) {
                    Some(run) => {
                        shape.update(path.as_bytes());
                        shape.update(&[0]);
                        shape.update(key.as_bytes());
                        replacements.push(run);
                    }
                    None => {
                        pending.insert(path.clone());
                    }
                },
                VectorSource::Failed => {
                    failed.insert(path.clone());
                }
                VectorSource::Pending => {
                    pending.insert(path.clone());
                }
            }
        }
        let tombstones = resident
            .members
            .keys()
            .filter(|path| !via_base.contains(*path))
            .filter_map(|path| self.overlay_path(path))
            .collect::<HashSet<_>>();
        let mut sorted_tombstones = tombstones.iter().collect::<Vec<_>>();
        sorted_tombstones.sort();
        shape.update(&[1]);
        for path in sorted_tombstones {
            shape.update(path.as_os_str().as_encoded_bytes());
            shape.update(&[0]);
        }
        let cache_key = (
            snapshot.generation().name().to_owned(),
            *shape.finalize().as_bytes(),
            state.fill_version,
            unvouched,
        );
        let index = match state.cache.as_ref() {
            Some((key, index)) if *key == cache_key => Arc::clone(index),
            _ => {
                let index = Arc::new(SemanticIndex::for_view(
                    root.to_path_buf(),
                    &resident.base,
                    tombstones,
                    &replacements,
                ));
                state.cache = Some((cache_key, Arc::clone(&index)));
                index
            }
        };
        drop(state);
        let results = index.search_filtered(query_vector, top_k, include);
        Ok(SemanticQuery {
            results,
            pending: pending
                .iter()
                .map(|path| self.absolute(root, path))
                .collect(),
            failed: failed
                .iter()
                .map(|path| self.absolute(root, path))
                .collect(),
            unvouched,
        })
    }

    /// This checkout's own resident semantic bytes: the replacement entries
    /// and tombstones of its last overlay. Base runs live in the family arena
    /// and are counted there once.
    pub fn private_memory(&self, access: &ViewAccess) -> u64 {
        let view = self.view(access);
        let state = lock(&view);
        state.cache.as_ref().map_or(0, |(_, index)| {
            index.estimated_memory().estimated_bytes.unwrap_or(0)
        })
    }

    /// Outstanding semantic work for the checkout, as a fill would see it.
    pub fn pending_work(&self, access: &ViewAccess, snapshot: &Snapshot) -> Vec<WorkItem> {
        let Some(resident) = self.resident(access, snapshot.generation().name()) else {
            return Vec::new();
        };
        let view = self.view(access);
        let state = lock(&view);
        self.work_items(&state, &resident, snapshot)
    }

    /// Forgets everything this process holds for a checkout that unbinds:
    /// its fill map and live pin, its resident generations, and any arena run
    /// no other view still references.
    pub fn unbind(&self, access: &ViewAccess) {
        let family = access.family().to_owned();
        let scope = access.scope().to_owned();
        lock(&self.views).remove(&(family.clone(), scope.clone()));
        lock(&self.residents).retain(|(f, s, _), _| *f != family || *s != scope);
        for arena in lock(&self.arenas).values() {
            arena.trim();
        }
    }
}

impl PlaneAdapter for SemanticPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Semantic
    }

    fn producer(&self) -> String {
        self.producer.id()
    }

    fn applies_to(&self, rel_path: &RelPath) -> bool {
        applies_to(rel_path)
    }

    /// Admits the generation's runs into the family arena (this is the only
    /// place, besides a fill's own puts, that decodes store rows) and, for the
    /// owner, drops fills and live-pin keys the generation now records.
    fn open_generation(
        &self,
        access: &ViewAccess,
        generation: &Arc<OpenGeneration>,
    ) -> Result<(), PlaneError> {
        let key = (
            access.family().to_owned(),
            access.scope().to_owned(),
            generation.name().to_owned(),
        );
        if !lock(&self.residents).contains_key(&key) {
            let resident = Arc::new(self.admit(access, generation)?);
            lock(&self.residents).entry(key).or_insert(resident);
        }
        if let ViewAccess::Owner(_) = access {
            let view = self.view(access);
            let mut state = lock(&view);
            let before = state.fill.len();
            state.fill.trim_folded(generation.manifest());
            if state.fill.len() != before {
                state.fill_version += 1;
            }
            let keep = state
                .fill
                .keys()
                .map(FamilyKey::to_hex)
                .collect::<HashSet<_>>();
            if let Some(pin) = state.live.as_mut() {
                pin.trim(|key| keep.contains(key))
                    .map_err(|error| plane_error(error.to_string()))?;
            }
        }
        Ok(())
    }

    fn release_generation(&self, access: &ViewAccess, generation: &str) {
        lock(&self.residents).remove(&(
            access.family().to_owned(),
            access.scope().to_owned(),
            generation.to_owned(),
        ));
        self.arena(access.family()).trim();
    }

    fn readiness(&self, snapshot: &Snapshot) -> PlaneReadiness {
        let owner = lock(&self.residents)
            .keys()
            .find(|(_, _, generation)| generation == snapshot.generation().name())
            .map(|(family, scope, _)| (family.clone(), scope.clone()));
        let Some((family, scope)) = owner else {
            return PlaneReadiness::Building;
        };
        self.readiness_for(&family, &scope, snapshot)
    }
}

impl SemanticPlane {
    fn readiness_for(&self, family: &str, scope: &str, snapshot: &Snapshot) -> PlaneReadiness {
        let key = (
            family.to_owned(),
            scope.to_owned(),
            snapshot.generation().name().to_owned(),
        );
        let Some(resident) = lock(&self.residents).get(&key).cloned() else {
            return PlaneReadiness::Building;
        };
        let view = lock(&self.views)
            .get(&(family.to_owned(), scope.to_owned()))
            .cloned()
            .unwrap_or_default();
        let state = lock(&view);
        let mut pending = 0;
        let mut failed = 0;
        for (path, disk) in snapshot.membership() {
            let DiskState::Present { content, .. } = disk else {
                continue;
            };
            if !applies_to(&path) {
                continue;
            }
            match Self::vector_source(&state, &resident, snapshot, &path, content) {
                VectorSource::Pending => pending += 1,
                VectorSource::Failed => failed += 1,
                VectorSource::Base | VectorSource::Fill(_) => {}
            }
        }
        PlaneReadiness::Ready { pending, failed }
    }
}

impl super::first_load::CompositePlane for SemanticPlane {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Semantic
    }

    fn applies_to(&self, rel_path: &RelPath) -> bool {
        applies_to(rel_path)
    }

    fn attachment(
        &self,
        rel_path: &RelPath,
        bytes: &[u8],
    ) -> Result<super::snapshot::PlaneAttachment, PlaneError> {
        Ok(Arc::new(SemanticAttachment {
            content: ContentHash::of(bytes),
            producer: self.producer.id(),
            key: self.producer.key(bytes, rel_path),
        }))
    }

    /// Records each applicable entry's semantic state without embedding
    /// anything: publication never waits for the model. An entry is `Ready`
    /// when its generation already had current vectors, when this checkout's
    /// fill map holds them, or when its content key is already stored in the
    /// family (by another view or an earlier session). A deterministic
    /// failure for this producer is `Failed`. Everything else is `Pending`,
    /// which keeps it queued across folds and restarts until a fill runs.
    fn materialize(
        &self,
        owner: &ViewRegistration,
        _generation: &str,
        _snapshot: &Snapshot,
        observed: &BTreeMap<RelPath, LiveEntry>,
        manifest: &mut ManifestV2,
        live: &mut crate::pins::LivePin,
        _seed_derived: bool,
    ) -> Result<(), PlaneError> {
        let id = self.producer.id();
        if manifest.header().producers.semantic.as_deref() != Some(id.as_str()) {
            return Err(plane_error("semantic producer mismatch"));
        }
        let store = owner
            .open_store(FamilyPlane::Semantic)
            .map_err(|error| plane_error(error.to_string()))?;
        let view = self.view(&ViewAccess::Owner(owner.clone()));
        let state = lock(&view);
        for (path, entry) in manifest.entries_mut() {
            let EntryV2::Regular {
                content, planes, ..
            } = entry
            else {
                continue;
            };
            if !applies_to(path) {
                planes.semantic = None;
                continue;
            }
            if matches!(planes.semantic, Some(PlaneState::Ready { .. })) {
                continue;
            }
            let stored = observed
                .get(path)
                .and_then(|entry| entry.attachments.get(&FamilyPlane::Semantic))
                .and_then(|value| value.downcast_ref::<SemanticAttachment>())
                .filter(|attachment| attachment.content == *content && attachment.producer == id)
                .map(|attachment| attachment.key);
            let key = match state.fill.get(path, content, FamilyPlane::Semantic) {
                Some(key) => Some(*key),
                None => match stored {
                    Some(key) => store
                        .contains(&key)
                        .map_err(|error| plane_error(error.to_string()))?
                        .then_some(key),
                    None => None,
                },
            };
            if let Some(key) = key {
                live.protect(&[key])
                    .map_err(|error| plane_error(error.to_string()))?;
                let touched = store
                    .touch(&[key])
                    .map_err(|error| plane_error(error.to_string()))?;
                if touched.missing.is_empty() {
                    planes.semantic = Some(PlaneState::ready(&key));
                    continue;
                }
            }
            if let Some(reason) = state.failed.get(&(path.clone(), *content)) {
                planes.semantic = Some(PlaneState::Failed {
                    reason: reason.clone(),
                    producer: id.clone(),
                });
                continue;
            }
            if !matches!(&planes.semantic, Some(PlaneState::Failed { producer, .. }) if *producer == id)
            {
                planes.semantic = Some(PlaneState::pending("awaiting semantic fill"));
            }
        }
        Ok(())
    }

    fn finish_generation(
        &self,
        _owner: &ViewRegistration,
        _staging: &str,
        _published: &str,
    ) -> Result<(), PlaneError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::contracts::PlaneLoader;
    use crate::views::first_load::{
        CheckoutDriver, CompositePlane, ConfiguredMembershipWalker, MembershipWalker, QueryState,
        SiblingLoader,
    };
    use crate::views::manifest_v2::Producers;
    use crate::views::registry::FamilyRegistry;
    use crate::views::snapshot::LiveDelta;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FILES: &[(&str, &str)] = &[
        (
            "src/alpha.rs",
            "pub fn alpha_total(values: &[u32]) -> u32 {\n    values.iter().sum()\n}\n\npub fn alpha_label() -> &'static str {\n    \"alpha\"\n}\n",
        ),
        (
            "src/beta.rs",
            "pub struct BetaCache {\n    entries: Vec<String>,\n}\n\nimpl BetaCache {\n    pub fn insert(&mut self, value: String) {\n        self.entries.push(value);\n    }\n}\n",
        ),
        (
            "src/gamma.rs",
            "fn gamma_parse(input: &str) -> Option<u64> {\n    input.trim().parse().ok()\n}\n",
        ),
        ("notes.txt", "not a semantic source\n"),
    ];

    /// A deterministic model: the vector is a hash of the model name and the
    /// text, so equal texts get equal vectors and a model change changes all.
    fn vector(model: &str, text: &str) -> Vec<f32> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(model.as_bytes());
        hasher.update(&[0]);
        hasher.update(text.as_bytes());
        hasher.finalize().as_bytes()[..16]
            .iter()
            .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
            .collect()
    }

    #[derive(Default)]
    struct Model {
        calls: AtomicUsize,
        texts: AtomicUsize,
    }

    impl Model {
        fn embed(&self, model: &str, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.texts.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts.iter().map(|text| vector(model, text)).collect())
        }

        fn texts(&self) -> usize {
            self.texts.load(Ordering::SeqCst)
        }
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    fn new_plane(storage: &Path, model: &str) -> Arc<SemanticPlane> {
        Arc::new(SemanticPlane::new(
            storage.to_path_buf(),
            SemanticProducer::current(model, EmbedTextCaps::default()),
        ))
    }

    struct Checkout {
        root: PathBuf,
        owner: ViewRegistration,
        access: ViewAccess,
        driver: Arc<CheckoutDriver>,
        loader: SiblingLoader,
        plane: Arc<SemanticPlane>,
    }

    impl Checkout {
        fn open(storage: &Path, scope: &str, root: &Path, plane: &Arc<SemanticPlane>) -> Self {
            let registry = FamilyRegistry::open(storage, "family").unwrap();
            let owner = registry.register_view(scope, root).unwrap();
            let composite: Arc<dyn CompositePlane> = plane.clone();
            let adapter: Arc<dyn PlaneAdapter> = plane.clone();
            let driver = Arc::new(
                CheckoutDriver::new(
                    owner.clone(),
                    Producers {
                        trigram: "trigram".into(),
                        semantic: Some(plane.semantic_producer().id()),
                        callgraph: "callgraph".into(),
                    },
                    None,
                    Arc::new(ConfiguredMembershipWalker),
                    vec![composite],
                )
                .with_adapters(vec![adapter.clone()]),
            );
            let loader = SiblingLoader::new(driver.clone(), vec![adapter]);
            Self {
                root: root.to_path_buf(),
                access: ViewAccess::Owner(owner.clone()),
                owner,
                driver,
                loader,
                plane: plane.clone(),
            }
        }

        /// Loads the checkout, publishing and installing its own generation.
        /// Called again it folds installed fills into a new generation.
        fn load(&self) -> Snapshot {
            self.loader.load(&self.access).unwrap().snapshot
        }

        fn installed(&self) -> Snapshot {
            self.driver
                .installed_state(&self.access, FamilyPlane::Semantic)
                .0
        }

        fn fill(&self, snapshot: &Snapshot, model: &Model, name: &str) -> FillReport {
            let current = snapshot.clone();
            self.plane
                .fill(
                    &self.owner,
                    snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed(name, texts),
                    &move || current.clone(),
                )
                .unwrap()
        }

        fn query(&self, snapshot: &Snapshot, model: &str, text: &str) -> SemanticQuery {
            self.plane
                .search(
                    &self.access,
                    &self.root,
                    snapshot,
                    &vector(model, text),
                    100,
                    &|_| true,
                )
                .unwrap()
        }
    }

    type Row = (String, String, u32, u32);

    fn rows(root: &Path, results: &[SemanticResult]) -> Vec<Row> {
        results
            .iter()
            .map(|result| {
                (
                    result
                        .file
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    result.name.clone(),
                    result.start_line,
                    result.score.to_bits(),
                )
            })
            .collect()
    }

    /// An independent cold rebuild: a full `SemanticIndex` build of the
    /// checkout's current walker membership, with no view store involved.
    fn cold(root: &Path, model: &str, text: &str) -> Vec<Row> {
        let files = ConfiguredMembershipWalker.files(root).unwrap();
        let index = SemanticIndex::build(
            root,
            &files,
            &mut |texts: Vec<String>| Ok(texts.iter().map(|text| vector(model, text)).collect()),
            64,
        )
        .unwrap();
        rows(root, &index.search(&vector(model, text), 100))
    }

    fn ready_checkout(storage: &Path, root: &Path, plane: &Arc<SemanticPlane>) -> Checkout {
        let checkout = Checkout::open(storage, "scope-a", root, plane);
        let snapshot = checkout.load();
        checkout.fill(&snapshot, &Model::default(), "model-a");
        checkout.load();
        checkout
    }

    #[test]
    fn semantic_superseded_base_vectors_are_never_scored() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        let before = checkout.query(&generation, "model-a", "alpha total");
        assert!(before.complete());
        assert!(rows(root.path(), &before.results)
            .iter()
            .any(|row| row.1 == "alpha_total"));
        assert_eq!(
            rows(root.path(), &before.results),
            cold(root.path(), "model-a", "alpha total")
        );

        // Edit alpha.rs; the edit is in the live delta, not in a generation.
        write_tree(
            root.path(),
            &[(
                "src/alpha.rs",
                "pub fn omega_rewritten() -> bool {\n    true\n}\n",
            )],
        );
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(
            &mut delta,
            root.path(),
            &crate::blob_store::v2::TrigramPolicy {
                max_file_size: 1 << 20,
            },
        );
        let edited = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(
            rows(root.path(), &edited.results)
                .iter()
                .all(|row| row.0 != "src/alpha.rs"),
            "vectors of alpha.rs's previous content were scored: {:?}",
            rows(root.path(), &edited.results)
        );
        assert_eq!(edited.pending, vec![root.path().join("src/alpha.rs")]);

        let model = Model::default();
        let snapshot = delta.snapshot();
        let report = checkout
            .plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| model.embed("model-a", texts),
                &|| delta.snapshot(),
            )
            .unwrap();
        assert_eq!(report.embedded_keys, 1, "only the edited file is embedded");
        let filled = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(filled.complete());
        assert_eq!(
            rows(root.path(), &filled.results),
            cold(root.path(), "model-a", "alpha total")
        );
    }

    #[test]
    fn semantic_pending_work_survives_fold_and_reopen() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        checkout.load();
        // Fold again with nothing filled: no live entry remains for any path,
        // so the pending state has to come from the published generation.
        let folded = checkout.load();
        assert!(folded.live_entries().next().is_none());
        assert_eq!(
            plane.readiness(&folded),
            PlaneReadiness::Ready {
                pending: 3,
                failed: 0
            }
        );
        assert_eq!(plane.pending_work(&checkout.access, &folded).len(), 3);
        let query = checkout.query(&folded, "model-a", "beta cache");
        assert!(query.results.is_empty());
        assert_eq!(query.pending.len(), 3);

        // A new plane (a new session) over the same storage still sees it.
        drop(checkout);
        let reopened_plane = new_plane(storage.path(), "model-a");
        let reopened = Checkout::open(storage.path(), "scope-a", root.path(), &reopened_plane);
        let snapshot = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&snapshot),
            PlaneReadiness::Ready {
                pending: 3,
                failed: 0
            }
        );
        let model = Model::default();
        let report = reopened.fill(&snapshot, &model, "model-a");
        assert_eq!(report.installed, 3);
        let folded = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&folded),
            PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
        assert_eq!(
            rows(
                root.path(),
                &reopened.query(&folded, "model-a", "beta cache").results
            ),
            cold(root.path(), "model-a", "beta cache")
        );
    }

    #[test]
    fn two_views_one_key_one_model_call() {
        let storage = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_tree(first.path(), FILES);
        write_tree(second.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let a = Checkout::open(storage.path(), "scope-a", first.path(), &plane);
        let b = Checkout::open(storage.path(), "scope-b", second.path(), &plane);
        let snapshot_a = a.load();
        let snapshot_b = b.load();
        let model = Model::default();
        let first_fill = a.fill(&snapshot_a, &model, "model-a");
        let chunks = model.texts();
        assert!(chunks > 0);
        assert_eq!(first_fill.embedded_texts, chunks);
        let second_fill = b.fill(&snapshot_b, &model, "model-a");
        assert_eq!(
            model.texts(),
            chunks,
            "the second view embedded content the first already embedded: {second_fill:?}"
        );
        assert_eq!(second_fill.resident_hits, 3);
        assert_eq!(second_fill.installed, 3);
        // The shared runs are resident once, and each view's private overlay
        // holds nothing for unedited files.
        let arena = plane.arena("family");
        let once = arena.memory();
        assert_eq!(once.runs, 3);
        let query_a = a.query(&a.load(), "model-a", "parse input");
        let query_b = b.query(&b.load(), "model-a", "parse input");
        assert_eq!(arena.memory(), once);
        assert_eq!(
            rows(first.path(), &query_a.results),
            rows(second.path(), &query_b.results)
        );
        assert_eq!(plane.private_memory(&a.access), 0);
        assert_eq!(plane.private_memory(&b.access), 0);
    }

    fn trigram_policy() -> crate::blob_store::v2::TrigramPolicy {
        crate::blob_store::v2::TrigramPolicy {
            max_file_size: 1 << 20,
        }
    }

    #[test]
    fn late_completion_for_moved_content_is_dropped() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        // By the time the work completes, alpha.rs holds other bytes.
        let mut moved = LiveDelta::new(Arc::clone(snapshot.generation()));
        moved.apply(
            RelPath::new(b"src/alpha.rs".to_vec()).unwrap(),
            LiveEntry::new(DiskState::of_bytes(b"pub fn later() {}\n"), 1),
        );
        let model = Model::default();
        let report = plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |texts| model.embed("model-a", texts),
                &|| moved.snapshot(),
            )
            .unwrap();
        assert_eq!((report.installed, report.dropped), (2, 1));
        let query = checkout.query(&moved.snapshot(), "model-a", "alpha");
        assert_eq!(query.pending, vec![root.path().join("src/alpha.rs")]);
        assert!(rows(root.path(), &query.results)
            .iter()
            .all(|row| row.0 != "src/alpha.rs"));
    }

    #[test]
    fn model_change_never_resurrects_old_vectors() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane_a = new_plane(storage.path(), "model-a");
        let checkout_a = ready_checkout(storage.path(), root.path(), &plane_a);
        let old = checkout_a.installed();
        drop(checkout_a);

        // A plane for another model reads the old generation as incompatible:
        // none of its vectors is scored, every file is pending.
        let plane_b = new_plane(storage.path(), "model-b");
        let checkout_b = Checkout::open(storage.path(), "scope-a", root.path(), &plane_b);
        plane_b
            .open_generation(&checkout_b.access, old.generation())
            .unwrap();
        let stale = checkout_b.query(&old, "model-b", "alpha total");
        assert!(stale.results.is_empty(), "{:?}", stale.results);
        assert_eq!(stale.pending.len(), 3);

        let snapshot = checkout_b.load();
        let model = Model::default();
        let report = checkout_b.fill(&snapshot, &model, "model-b");
        assert_eq!(report.embedded_keys, 3);
        let folded = checkout_b.load();
        assert_eq!(
            rows(
                root.path(),
                &checkout_b.query(&folded, "model-b", "alpha total").results
            ),
            cold(root.path(), "model-b", "alpha total")
        );
    }

    #[test]
    fn deterministic_failure_is_recorded_and_survives_fold_and_reopen() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        std::fs::write(root.path().join("src/broken.rs"), [0xff, 0xfe, b'f', b'n']).unwrap();
        let plane = new_plane(storage.path(), "model-a");
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let model = Model::default();
        let report = checkout.fill(&snapshot, &model, "model-a");
        assert_eq!((report.installed, report.failed), (3, 1));
        let folded = checkout.load();
        assert!(matches!(
            folded
                .generation()
                .manifest()
                .get(&RelPath::new(b"src/broken.rs".to_vec()).unwrap())
                .and_then(|entry| entry.plane_state(FamilyPlane::Semantic)),
            Some(PlaneState::Failed { .. })
        ));
        drop(checkout);
        let reopened_plane = new_plane(storage.path(), "model-a");
        let reopened = Checkout::open(storage.path(), "scope-a", root.path(), &reopened_plane);
        let snapshot = reopened.load();
        assert_eq!(
            reopened_plane.readiness(&snapshot),
            PlaneReadiness::Ready {
                pending: 0,
                failed: 1
            }
        );
        let texts = model.texts();
        let again = reopened.fill(&snapshot, &model, "model-a");
        assert_eq!((again.queued, model.texts()), (0, texts));
        let query = reopened.query(&snapshot, "model-a", "beta");
        assert_eq!(query.failed, vec![root.path().join("src/broken.rs")]);
        assert_eq!(
            rows(root.path(), &query.results),
            cold(root.path(), "model-a", "beta")
        );
    }

    #[test]
    fn queries_never_decode_store_rows_and_unbind_releases_the_arena() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        let arena = plane.arena("family");
        let (decodes, reads) = (arena.decode_count(), arena.store_reads());
        for text in ["alpha", "beta", "gamma", "cache insert"] {
            checkout.query(&generation, "model-a", text);
        }
        assert_eq!(
            (arena.decode_count(), arena.store_reads()),
            (decodes, reads)
        );
        assert_eq!(plane.private_memory(&checkout.access), 0);

        // Private memory is the edited files' replacements only.
        let edit = |files: &[(&str, &str)]| {
            write_tree(root.path(), files);
            let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
            crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
            let snapshot = delta.snapshot();
            let model = Model::default();
            plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed("model-a", texts),
                    &|| snapshot.clone(),
                )
                .unwrap();
            checkout.query(&snapshot, "model-a", "edited");
            plane.private_memory(&checkout.access)
        };
        let one = edit(&[("src/gamma.rs", "fn gamma_edited() {}\n")]);
        let two = edit(&[("src/alpha.rs", "fn alpha_edited() {}\n")]);
        assert!(
            one > 0 && two > one,
            "one edit {one} bytes, two edits {two} bytes"
        );
        assert!(
            two < arena.memory().bytes,
            "private overlay {two} should stay below the shared arena {}",
            arena.memory().bytes
        );

        plane.unbind(&checkout.access);
        drop(generation);
        assert_eq!(arena.memory().runs, 0, "unbind left runs resident");
    }

    /// A fold may install a generation built from a cut taken before a fill
    /// landed. Installed completions that generation does not record stay in
    /// the fill map: the work is neither lost nor done again.
    #[test]
    fn semantic_installed_fills_survive_a_fold_that_does_not_carry_them() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = new_plane(storage.path(), "model-a");
        let checkout = ready_checkout(storage.path(), root.path(), &plane);
        let generation = checkout.installed();
        write_tree(
            root.path(),
            &[(
                "src/alpha.rs",
                "pub fn alpha_rewritten() -> u8 {\n    7\n}\n",
            )],
        );
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
        let model = Model::default();
        let snapshot = delta.snapshot();
        let fill = |model: &Model| {
            plane
                .fill(
                    &checkout.owner,
                    &snapshot,
                    FillBudget::default(),
                    &mut |texts| model.embed("model-a", texts),
                    &|| snapshot.clone(),
                )
                .unwrap()
        };
        assert_eq!(fill(&model).installed, 1);
        // Install a generation that still names alpha.rs's previous content.
        plane
            .open_generation(&checkout.access, generation.generation())
            .unwrap();
        let answer = checkout.query(&snapshot, "model-a", "alpha rewritten");
        assert!(answer.complete(), "installed work was lost: {answer:?}");
        assert_eq!(
            rows(root.path(), &answer.results),
            cold(root.path(), "model-a", "alpha rewritten")
        );
        let texts = model.texts();
        assert_eq!(fill(&model).queued, 0);
        assert_eq!(model.texts(), texts);
    }

    #[test]
    fn overlay_converts_view_paths_once_to_the_native_form() {
        assert_eq!(
            overlay_rel_path(&RelPath::new(b"src/module_2.rs".to_vec()).unwrap(), '\\'),
            Some(PathBuf::from("src\\module_2.rs"))
        );
    }

    /// Windows' path form, simulated on every platform. A full build names
    /// files `src\alpha.rs` there, in chunk paths, results and embedded text;
    /// a view must use the same form everywhere in its overlay, or superseded
    /// vectors escape their tombstones and scores differ from a full build.
    #[test]
    fn backslash_overlay_paths_hide_superseded_vectors_and_match_native_results() {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write_tree(root.path(), FILES);
        let plane = Arc::new(
            SemanticPlane::new(
                storage.path().to_path_buf(),
                SemanticProducer::current("model-a", EmbedTextCaps::default()),
            )
            .with_path_separator('\\'),
        );
        let checkout = Checkout::open(storage.path(), "scope-a", root.path(), &plane);
        let snapshot = checkout.load();
        let texts = Mutex::new(Vec::new());
        let current = snapshot.clone();
        plane
            .fill(
                &checkout.owner,
                &snapshot,
                FillBudget::default(),
                &mut |batch| {
                    texts.lock().unwrap().extend(batch.iter().cloned());
                    Ok(batch.iter().map(|text| vector("model-a", text)).collect())
                },
                &move || current.clone(),
            )
            .unwrap();
        let texts = texts.into_inner().unwrap();
        assert!(texts.iter().any(|text| text.contains("file:src\\alpha.rs")));
        assert!(
            texts.iter().all(|text| !text.contains("file:src/")),
            "embedded text used the view key form: {texts:?}"
        );
        let generation = checkout.load();
        let answer = checkout.query(&generation, "model-a", "alpha total");
        let files = rows(root.path(), &answer.results)
            .into_iter()
            .map(|row| row.0)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            files,
            BTreeSet::from([
                "src\\alpha.rs".to_owned(),
                "src\\beta.rs".to_owned(),
                "src\\gamma.rs".to_owned()
            ])
        );

        write_tree(root.path(), &[("src/alpha.rs", "pub fn omega() {}\n")]);
        let mut delta = LiveDelta::new(Arc::clone(generation.generation()));
        crate::views::live_delta::reconcile(&mut delta, root.path(), &trigram_policy());
        let edited = checkout.query(&delta.snapshot(), "model-a", "alpha total");
        assert!(
            rows(root.path(), &edited.results)
                .iter()
                .all(|row| row.0 != "src\\alpha.rs"),
            "superseded vectors of src\\alpha.rs were scored"
        );
        assert_eq!(edited.pending, vec![root.path().join("src\\alpha.rs")]);
    }
}
