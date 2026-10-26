//! Opt-in trigram views: shared segment, generation overlay, and live delta.
//!
//! Public lexical queries select a checkout-local v3 reader only with views on.
//! Views off retains the legacy `SearchIndex` route.
//!
//! Runtime call sequence: obtain current bytes with `live_delta::strict_walk`,
//! project those entries into the complete v2 manifest, call `materialize`
//! while keeping its returned pin alive, materialize the other planes, and
//! publish with the core generation CAS. Open the verified pinned generation
//! with `TrigramAdapter::open_generation`. Supply `TrigramDriver` with a
//! `Publisher` that performs this complete publication and an `Opener` that
//! delegates to `TrigramAdapter::open_index`; the loader then calls `install`
//! with the reconciliation revision. Register adapter/driver routing only at
//! cutover. Watcher events use `live_delta::apply_event` for both rename paths;
//! gaps and overflow use `live_delta::reconcile` before declaring healthy.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyPlane, TrigramPolicy};

use super::live_delta::{strict_walk, Attachment};
use super::segment_store::{rel_path_to_os, SegmentReader, TrigramFlag, TrigramPayload};
use super::snapshot::{DiskState, Snapshot, Source, WatcherState};
use super::RelPath;

/// Checkout-local lexical authority, independent of the semantic worker's
/// lifetime and model availability. Queries serialize reconciliation so an
/// installed membership and its payloads always come from the same snapshot.
pub(crate) struct LexicalView {
    pub(crate) root: PathBuf,
    storage: crate::root_cache::StoragePaths,
    family: String,
    policy: TrigramPolicy,
    source: Arc<dyn LexicalSnapshotSource>,
    query: std::sync::Mutex<()>,
    ready: std::sync::atomic::AtomicBool,
}

/// Supplies a verified checkout snapshot without exposing its storage layout
/// to the lexical query engine. Implementations must never publish v1/v2 data.
pub(crate) trait LexicalSnapshotSource: Send + Sync {
    fn acquire(&self) -> Result<Snapshot, String>;
    fn record_change(&self, path: &Path);
}

impl LexicalView {
    pub(crate) fn matches(
        &self,
        root: &Path,
        storage: &crate::root_cache::StoragePaths,
        family: &str,
        policy: TrigramPolicy,
    ) -> bool {
        self.root == root
            && self.storage == *storage
            && self.family == family
            && self.policy == policy
    }

    pub(crate) fn new(
        _root: PathBuf,
        _storage: crate::root_cache::StoragePaths,
        _family: String,
        _policy: TrigramPolicy,
    ) -> Result<Self, String> {
        // A legacy registration would write blobs/v2 even before loading. Until
        // a v3-only source is available, refuse rather than silently creating it.
        Err("v3-only lexical snapshot source unavailable".into())
    }

    #[cfg(test)]
    pub(crate) fn with_source(
        root: PathBuf,
        storage: crate::root_cache::StoragePaths,
        family: String,
        policy: TrigramPolicy,
        source: Arc<dyn LexicalSnapshotSource>,
    ) -> Self {
        Self {
            root,
            storage,
            family,
            policy,
            source,
            query: std::sync::Mutex::new(()),
            ready: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn record_change(&self, path: &Path) {
        self.ready
            .store(false, std::sync::atomic::Ordering::Release);
        self.source.record_change(path);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Keep acquisition separate from the read projection so a source that
    /// verifies a saved generation can supply the same query snapshot.
    fn acquire(&self) -> Result<Snapshot, String> {
        self.source.acquire()
    }

    pub(crate) fn snapshot(&self) -> Result<crate::search_index::SearchIndexSnapshot, String> {
        let result = self.project();
        self.ready
            .store(result.is_ok(), std::sync::atomic::Ordering::Release);
        result
    }

    fn project(&self) -> Result<crate::search_index::SearchIndexSnapshot, String> {
        use super::segment_store::PayloadSource;
        let _query = self
            .query
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = self.acquire()?;
        if snapshot.pending_intent().next().is_some() || snapshot.watcher() != WatcherState::Healthy
        {
            return Err("lexical view has unverified writes".into());
        }
        let source = crate::blob_store::v3::PlaneReader::open_existing(
            &self.storage.artifacts,
            &self.family,
            FamilyPlane::Trigram,
        )
        .map_err(|error| error.to_string())?;
        let mut rows = Vec::new();
        for (path, disk) in snapshot.membership() {
            let DiskState::Present { content, size } = disk else {
                continue;
            };
            let absolute = self
                .root
                .join(rel_path_to_os(&path).map_err(|error| error.to_string())?);
            let key = crate::blob_store::v2::TrigramKey {
                content,
                policy: self.policy,
            }
            .family_key();
            let payload = match snapshot.source(&path) {
                Source::Live(entry) => entry
                    .attachments
                    .get(&FamilyPlane::Trigram)
                    .and_then(|value| value.downcast_ref::<Attachment>())
                    .filter(|attachment| {
                        attachment.content == content
                            && attachment.policy == self.policy.fingerprint()
                    })
                    .map(|attachment| attachment.payload.clone()),
                _ => None,
            };
            let payload = match payload {
                Some(payload) => payload,
                None => {
                    if snapshot.plane_readiness(&path, FamilyPlane::Trigram)
                        != Some(&super::readiness::PlaneState::ready(&key))
                    {
                        return Err(format!("lexical payload not ready: {}", absolute.display()));
                    }
                    let bytes = source
                        .payload(&key)
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| {
                            format!("lexical payload missing: {}", absolute.display())
                        })?;
                    TrigramPayload::decode(&bytes).map_err(|error| error.to_string())?
                }
            };
            let metadata = fs::metadata(&absolute).map_err(|error| error.to_string())?;
            rows.push((
                crate::search_index::FileEntry {
                    path: absolute,
                    size,
                    modified: metadata.modified().map_err(|error| error.to_string())?,
                    content_hash: blake3::Hash::from_bytes(*content.as_bytes()),
                },
                (payload.flag == TrigramFlag::Indexed).then(|| {
                    payload
                        .records
                        .into_iter()
                        .map(|record| (record.trigram, record.next_mask, record.loc_mask))
                        .collect()
                }),
            ));
        }
        Ok(
            crate::search_index::SearchIndexSnapshot::from_verified_rows(
                &self.root,
                self.policy.max_file_size,
                rows,
            ),
        )
    }
}

#[cfg(test)]
pub(crate) struct BaseBuilderTestSource {
    root: PathBuf,
    storage: PathBuf,
    family: String,
    policy: TrigramPolicy,
    snapshots: std::sync::Mutex<BTreeMap<[u8; 32], (Snapshot, super::base_builder::BuildClaim)>>,
}

#[cfg(test)]
impl BaseBuilderTestSource {
    pub(crate) fn new(
        root: &Path,
        storage: &Path,
        family: &str,
        policy: TrigramPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            storage: storage.into(),
            family: family.into(),
            policy,
            snapshots: Default::default(),
        })
    }
}

#[cfg(test)]
impl LexicalSnapshotSource for BaseBuilderTestSource {
    fn acquire(&self) -> Result<Snapshot, String> {
        let walk = strict_walk(&self.root, &self.policy, 0);
        if !walk.gaps.is_empty() {
            return Err(format!("strict test walk gaps: {:?}", walk.gaps));
        }
        let mut hash = blake3::Hasher::new();
        for (path, entry) in &walk.entries {
            hash.update(&(path.as_bytes().len() as u64).to_be_bytes());
            hash.update(path.as_bytes());
            if let DiskState::Present { content, .. } = entry.disk {
                hash.update(content.as_bytes());
            }
        }
        let key = *hash.finalize().as_bytes();
        let mut snapshots = self.snapshots.lock().unwrap();
        if let Some((snapshot, _)) = snapshots.get(&key) {
            return Ok(snapshot.clone());
        }
        let paths = walk.entries.into_keys().collect::<Vec<_>>();
        let built = super::base_builder::build_base(
            super::base_builder::BaseBuildInput {
                storage: &self.storage,
                family: &self.family,
                root: &self.root,
                paths: &paths,
                policy: self.policy,
                semantic_producer: None,
                callgraph_producer: super::callgraph::PRODUCER.into(),
                ignore_fingerprint: None,
                history: None,
                budget: super::eviction::DiskBudget::default(),
            },
            None,
        )
        .map_err(|error| error.to_string())?;
        let super::base_builder::BaseBuild::Published {
            identity, claim, ..
        } = built
        else {
            return Err(format!("test base build refused: {built:?}"));
        };
        let base = super::base_refs::resolve_base(&self.storage, &self.family, &identity.scope)
            .map_err(|error| error.to_string())?;
        let worker = super::manifest_v2::ManifestV2::new_v3(
            base.manifest().header().clone(),
            Some(base.header()),
            super::manifest_v2::PublishedFrontier::default(),
        );
        let composed = super::snapshot::ComposedGeneration::compose(
            "lexical-test-worker",
            worker,
            base.into_generation(&self.storage, &self.family),
            None,
        )
        .map_err(|error| error.to_string())?;
        let composed = composed.with_v3_route(super::snapshot::V3Route {
            storage: self.storage.clone(),
            family: self.family.clone(),
            scope: "lexical-test-worker".into(),
        });
        let snapshot = super::snapshot::LiveDelta::new(Arc::new(composed)).snapshot();
        snapshots.insert(key, (snapshot.clone(), claim));
        Ok(snapshot)
    }

    fn record_change(&self, _: &Path) {}
}

/// Generation-local replacements and tombstones relative to the shared segment.
/// Missing payloads are gaps, not empty postings.
#[derive(Debug)]
pub struct GenerationIndex {
    pub segment: Arc<SegmentReader>,
    overlay: BTreeMap<RelPath, Option<Arc<Attachment>>>,
    policy: TrigramPolicy,
}

#[cfg(test)]
thread_local! {
    static CANDIDATE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl GenerationIndex {
    /// Load replacement payloads through the existing family-store connection.
    /// Unavailable or non-ready payloads remain direct-scan paths.
    pub fn open(
        segment: Arc<SegmentReader>,
        snapshot: &Snapshot,
        policy: TrigramPolicy,
        source: &dyn super::segment_store::PayloadSource,
    ) -> Result<Self, String> {
        let mut payloads = BTreeMap::new();
        let base = segment
            .files()
            .iter()
            .map(|file| (&file.rel_path, file.content))
            .collect::<BTreeMap<_, _>>();
        for (path, entry) in snapshot.generation().entries() {
            let Some(content) = entry.content() else {
                continue;
            };
            if base.get(path) == Some(&content) {
                continue;
            }
            let key = crate::blob_store::v2::TrigramKey { content, policy }.family_key();
            if snapshot
                .generation()
                .plane_readiness(path, FamilyPlane::Trigram)
                != Some(&super::readiness::PlaneState::ready(&key))
            {
                continue;
            }
            if let Some(bytes) = source.payload(&key).map_err(|e| e.to_string())? {
                payloads.insert(
                    content,
                    TrigramPayload::decode(&bytes).map_err(|e| e.to_string())?,
                );
            }
        }
        Self::new(segment, snapshot, policy, &payloads)
    }

    pub fn new(
        segment: Arc<SegmentReader>,
        snapshot: &Snapshot,
        policy: TrigramPolicy,
        payloads: &BTreeMap<ContentHash, TrigramPayload>,
    ) -> Result<Self, String> {
        if segment.policy_fingerprint() != policy.fingerprint()
            || snapshot.generation().header().producers.trigram != policy.fingerprint_hex()
        {
            return Err("trigram producer mismatch".into());
        }
        let manifest = snapshot.generation();
        let mut overlay = BTreeMap::new();
        let base = segment
            .files()
            .iter()
            .map(|file| (file.rel_path.clone(), file.content))
            .collect::<BTreeMap<_, _>>();
        for path in base.keys() {
            if manifest
                .entry(path)
                .and_then(|entry| entry.content())
                .is_none()
            {
                overlay.insert(path.clone(), None);
            }
        }
        for (path, entry) in manifest.entries() {
            let Some(content) = entry.content() else {
                continue;
            };
            if base.get(path) == Some(&content) {
                continue;
            }
            overlay.insert(
                path.clone(),
                payloads.get(&content).map(|payload| {
                    Arc::new(Attachment {
                        content,
                        policy: policy.fingerprint(),
                        payload: payload.clone(),
                    })
                }),
            );
        }
        Ok(Self {
            segment,
            overlay,
            policy,
        })
    }

    /// Live entries and overlay replacements exclude the segment's old path
    /// before its postings can select candidates.
    /// `members` is the reconciled current ignore membership, not disk presence.
    pub fn candidates(
        &self,
        snapshot: &Snapshot,
        members: &BTreeSet<RelPath>,
        literal: &str,
    ) -> Candidates {
        #[cfg(test)]
        CANDIDATE_SCANS.with(|count| count.set(count.get() + 1));
        let required = crate::search_index::SearchIndex::query_trigrams_from_tokens(&[literal]);
        let intents = snapshot.pending_intent().cloned().collect::<BTreeSet<_>>();
        let live = snapshot
            .live_entries()
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        let mut answer = Candidates::default();
        let selected = required
            .iter()
            .map(|trigram| {
                self.segment
                    .postings(*trigram)
                    .iter()
                    .map(|posting| posting.file_id)
                    .collect::<BTreeSet<_>>()
            })
            .reduce(|left, right| left.intersection(&right).copied().collect());
        for (id, file) in self.segment.files().iter().enumerate() {
            let path = &file.rel_path;
            if !members.contains(path)
                || self.overlay.contains_key(path)
                || live.contains(path)
                || intents.contains(path)
            {
                continue;
            }
            if snapshot.disk_state(path)
                != (DiskState::Present {
                    content: file.content,
                    size: file.size,
                })
            {
                answer.direct.insert(path.clone());
                continue;
            }
            if file.flag != TrigramFlag::Indexed
                || selected
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&(id as u32)))
            {
                answer.indexed.insert(path.clone());
            }
        }
        for (path, attachment) in &self.overlay {
            if !members.contains(path) || live.contains(path) || intents.contains(path) {
                continue;
            }
            if matches!(snapshot.source(path), Source::Absent) {
                continue;
            }
            select_attachment(
                &mut answer,
                path,
                attachment.as_deref(),
                snapshot,
                &self.policy,
                &required,
            );
        }
        for (path, entry) in snapshot.live_entries() {
            if !members.contains(path) || intents.contains(path) || entry.disk == DiskState::Absent
            {
                continue;
            }
            let attachment = entry
                .attachments
                .get(&FamilyPlane::Trigram)
                .and_then(|value| value.downcast_ref::<Attachment>());
            select_attachment(
                &mut answer,
                path,
                attachment,
                snapshot,
                &self.policy,
                &required,
            );
        }
        // Old postings can say a new literal is absent. Pending writes must
        // therefore be verified even when no indexed trigram matches.
        answer
            .direct
            .extend(intents.into_iter().filter(|path| members.contains(path)));
        answer
    }

    /// Literal matched-line verification reads candidates on disk. If the
    /// watcher has overflowed, stopped, or is reconciling, scan every current
    /// member instead. Traversal/read failures remain in `QueryResult::gaps`.
    pub fn query(&self, root: &Path, snapshot: &Snapshot, literal: &str) -> QueryResult {
        let active = super::intent::active(root);
        let uncertain = active
            || snapshot.watcher() != WatcherState::Healthy
            || snapshot.pending_intent().next().is_some();
        let (members, gaps) = if uncertain {
            let walk = strict_walk(root, &self.policy, snapshot.delta_version());
            (walk.entries.into_keys().collect::<BTreeSet<_>>(), walk.gaps)
        } else {
            (snapshot.membership().into_keys().collect(), Vec::new())
        };
        let candidates = if active || snapshot.watcher() != WatcherState::Healthy {
            Candidates {
                indexed: BTreeSet::new(),
                direct: members.clone(),
            }
        } else {
            self.candidates(snapshot, &members, literal)
        };
        let mut result = QueryResult {
            membership: members,
            matches: Vec::new(),
            gaps,
            relative_gaps: Vec::new(),
        };
        for path in candidates.indexed.union(&candidates.direct) {
            let absolute = match rel_path_to_os(path) {
                Ok(relative) => root.join(relative),
                Err(_) => {
                    record_conversion_gap(&mut result, path);
                    continue;
                }
            };
            match fs::read(&absolute) {
                Ok(bytes) => {
                    if crate::search_index::is_binary_bytes(&bytes) {
                        continue;
                    }
                    for (line, text) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                        if text.contains(literal) {
                            result.matches.push(MatchedLine {
                                path: path.clone(),
                                line: line + 1,
                                text: text.into(),
                            });
                        }
                    }
                }
                Err(_) => result.gaps.push(absolute),
            }
        }
        result.gaps.sort();
        result.gaps.dedup();
        result
    }

    /// Logical resident bytes, excluding allocator overhead and Arc bookkeeping.
    /// Callers count the shared segment once, not once per checkout.
    pub fn checkout_bytes(&self) -> usize {
        self.overlay
            .iter()
            .map(|(path, value)| {
                path.as_bytes().len()
                    + std::mem::size_of::<Option<Arc<Attachment>>>()
                    + value.as_ref().map_or(0, |value| {
                        std::mem::size_of::<Attachment>()
                            + value.payload.records.len()
                                * std::mem::size_of::<super::segment_store::TrigramRecord>()
                    })
            })
            .sum()
    }
}

fn select_attachment(
    answer: &mut Candidates,
    path: &RelPath,
    attachment: Option<&Attachment>,
    snapshot: &Snapshot,
    policy: &TrigramPolicy,
    required: &[u32],
) {
    let Some(attachment) = attachment.filter(|value| value.policy == policy.fingerprint()
        && matches!(snapshot.disk_state(path), DiskState::Present { content, .. } if content == value.content)) else {
        answer.direct.insert(path.clone());
        return;
    };
    if attachment.payload.flag != TrigramFlag::Indexed
        || required.iter().all(|trigram| {
            attachment
                .payload
                .records
                .binary_search_by_key(trigram, |record| record.trigram)
                .is_ok()
        })
    {
        answer.indexed.insert(path.clone());
    }
}

#[derive(Debug, Default)]
pub struct Candidates {
    pub indexed: BTreeSet<RelPath>,
    pub direct: BTreeSet<RelPath>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct MatchedLine {
    pub path: RelPath,
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct QueryResult {
    pub membership: BTreeSet<RelPath>,
    pub matches: Vec<MatchedLine>,
    pub gaps: Vec<PathBuf>,
    /// Exact manifest keys that cannot be represented as native paths.
    pub relative_gaps: Vec<RelPath>,
}

fn record_conversion_gap(result: &mut QueryResult, path: &RelPath) {
    result.relative_gaps.push(path.clone());
}

impl QueryResult {
    pub fn complete(&self) -> bool {
        self.gaps.is_empty() && self.relative_gaps.is_empty()
    }

    /// Preserve OS bytes for native paths and the manifest encoding for other keys.
    pub fn gap_diagnostics(&self) -> Vec<String> {
        self.gaps
            .iter()
            .map(|path| format!("{path:?}"))
            .chain(self.relative_gaps.iter().map(|path| {
                format!(
                    "unrepresentable relative path: {}",
                    serde_json::to_string(path).expect("relative paths always serialize")
                )
            }))
            .collect()
    }
}

/// Runtime-owned publication includes semantic and callgraph states as well as
/// trigrams. Delegating publication prevents a trigram-only manifest from
/// erasing the other indexes' readiness or keys.
pub type Publisher = dyn Fn(
        &super::contracts::ViewAccess,
        &Snapshot,
        bool,
    ) -> Result<Arc<super::snapshot::ComposedGeneration>, super::contracts::PlaneError>
    + Send
    + Sync;
pub type Opener = dyn Fn(
        &super::contracts::ViewAccess,
        &Snapshot,
    ) -> Result<GenerationIndex, super::contracts::PlaneError>
    + Send
    + Sync;

/// Checkout-local trigram helpers for the runtime's composite driver. This
/// type does not implement or register `FirstLoadDriver` or `QueryState`;
/// the runtime alone combines all planes into those contracts.
/// Construction is opt-in; it does not register global search routing.
pub struct TrigramDriver {
    root: PathBuf,
    scope: String,
    producers: super::manifest_v2::Producers,
    policy: TrigramPolicy,
    pub delta: Arc<std::sync::Mutex<super::snapshot::LiveDelta>>,
    resident: std::sync::Mutex<Option<(Snapshot, Arc<GenerationIndex>)>>,
    publish: Arc<Publisher>,
    open: Arc<Opener>,
}

impl TrigramDriver {
    pub fn new(
        root: PathBuf,
        scope: String,
        producers: super::manifest_v2::Producers,
        policy: TrigramPolicy,
        delta: Arc<std::sync::Mutex<super::snapshot::LiveDelta>>,
        publish: Arc<Publisher>,
        open: Arc<Opener>,
    ) -> Self {
        super::intent::register(&root, &delta);
        Self {
            root,
            scope,
            producers,
            policy,
            delta,
            resident: std::sync::Mutex::new(None),
            publish,
            open,
        }
    }

    fn validate(
        &self,
        access: &super::contracts::ViewAccess,
    ) -> Result<(), super::contracts::PlaneError> {
        if access.scope() != self.scope || access.is_read_only() {
            return Err(plane_error("driver requires its owning checkout"));
        }
        Ok(())
    }

    pub fn query(&self, literal: &str) -> Result<QueryResult, super::contracts::PlaneError> {
        let delta = self.delta.lock().unwrap_or_else(|e| e.into_inner());
        let resident = self.resident.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, index)) = resident.as_ref() else {
            return Err(plane_error("trigram resident not installed"));
        };
        Ok(index.query(&self.root, &delta.snapshot(), literal))
    }
}

fn plane_error(reason: impl Into<String>) -> super::contracts::PlaneError {
    super::contracts::PlaneError {
        plane: Some(FamilyPlane::Trigram),
        reason: reason.into(),
    }
}

fn revision(delta: &super::snapshot::LiveDelta) -> u64 {
    let snapshot = delta.snapshot();
    snapshot
        .delta_version()
        .wrapping_add(snapshot.intent_version())
        .wrapping_add(snapshot.epoch())
}

impl TrigramDriver {
    pub fn producers(&self, _: &super::contracts::ViewAccess) -> super::manifest_v2::Producers {
        self.producers.clone()
    }
    pub fn head_tree(&self, _: &super::contracts::ViewAccess) -> Option<String> {
        None
    }
    pub fn reconcile(
        &self,
        access: &super::contracts::ViewAccess,
    ) -> Result<super::first_load::ReconciledCheckout, super::contracts::PlaneError> {
        self.validate(access)?;
        let mut delta = self.delta.lock().unwrap_or_else(|e| e.into_inner());
        let walk = super::live_delta::reconcile(&mut delta, &self.root, &self.policy);
        if !walk.gaps.is_empty() || super::intent::active(&self.root) {
            return Err(plane_error(format!(
                "strict walk incomplete or active write: {:?}",
                walk.gaps
            )));
        }
        Ok(super::first_load::ReconciledCheckout {
            revision: revision(&delta),
            entries: walk.entries,
            cut: None,
            readiness: delta.readiness().clone(),
            publication_base: None,
        })
    }
    pub fn revision(&self, _: &super::contracts::ViewAccess) -> u64 {
        revision(&self.delta.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn build_own_generation(
        &self,
        access: &super::contracts::ViewAccess,
        snapshot: &Snapshot,
        seed_derived: bool,
    ) -> Result<Arc<super::snapshot::ComposedGeneration>, super::contracts::PlaneError> {
        self.validate(access)?;
        (self.publish)(access, snapshot, seed_derived)
    }
    pub fn install(
        &self,
        access: &super::contracts::ViewAccess,
        snapshot: &Snapshot,
        expected_revision: u64,
    ) -> Result<(), super::contracts::PlaneError> {
        self.validate(access)?;
        // Resolve immutable plane data before locking; the revision is checked
        // afterwards, so edits racing the open cannot install obsolete state.
        let index = Arc::new((self.open)(access, snapshot)?);
        let mut delta = self.delta.lock().unwrap_or_else(|e| e.into_inner());
        if revision(&delta) != expected_revision {
            return Err(plane_error("obsolete checkout revision"));
        }
        let draft = super::snapshot::derive_successor(
            &delta.cut(),
            Arc::clone(snapshot.generation()),
            &super::snapshot::carry_disk_state_only,
        );
        delta
            .replay_and_swap(draft, &super::snapshot::carry_disk_state_only)
            .map_err(|error| plane_error(format!("switch failed: {error:?}")))?;
        *self.resident.lock().unwrap_or_else(|e| e.into_inner()) = Some((delta.snapshot(), index));
        Ok(())
    }
}

impl TrigramDriver {
    pub fn installed_state(
        &self,
        access: &super::contracts::ViewAccess,
        plane: FamilyPlane,
    ) -> (Snapshot, Vec<PathBuf>) {
        let delta = self.delta.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = delta.snapshot();
        let resident = self.resident.lock().unwrap_or_else(|e| e.into_inner());
        let mut gaps = snapshot
            .pending_intent()
            .filter_map(|path| rel_path_to_os(path).ok().map(|path| self.root.join(path)))
            .collect::<Vec<_>>();
        if plane != FamilyPlane::Trigram
            || access.scope() != self.scope
            || resident.is_none()
            || snapshot.watcher() != WatcherState::Healthy
        {
            gaps.push(self.root.clone());
        }
        if let Some((_, index)) = resident.as_ref() {
            let members = snapshot.membership().into_keys().collect();
            gaps.extend(
                index
                    .candidates(&snapshot, &members, "")
                    .direct
                    .iter()
                    .filter_map(|path| rel_path_to_os(path).ok().map(|path| self.root.join(path))),
            );
        }
        for (path, entry) in snapshot.live_entries() {
            if entry.disk != DiskState::Absent
                && !entry.attachments.contains_key(&FamilyPlane::Trigram)
            {
                if let Ok(path) = rel_path_to_os(path) {
                    gaps.push(self.root.join(path));
                }
            }
        }
        gaps.sort();
        gaps.dedup();
        (snapshot, gaps)
    }
}

/// Protected trigram artifacts for a projected manifest. Keep this value alive
/// until the complete generation has been published and installed.
pub struct MaterializedTrigrams {
    pub pin: crate::pins::LivePin,
    pub segment: [u8; 32],
}

/// Materialize only the trigram plane of the runtime's projected manifest.
/// Semantic/callgraph entries and their states are left untouched. Live
/// attachments are preferred; any fallback source read must match the manifest
/// content hash or construction fails rather than publishing stale bytes.
pub fn materialize(
    registration: &super::registry::ViewRegistration,
    root: &Path,
    manifest: &mut super::manifest_v2::ManifestV2,
    observed: &BTreeMap<RelPath, super::snapshot::LiveEntry>,
    policy: TrigramPolicy,
) -> Result<MaterializedTrigrams, super::contracts::PlaneError> {
    if manifest.header().producers.trigram != policy.fingerprint_hex() {
        return Err(plane_error("trigram producer mismatch"));
    }
    let store = registration
        .open_store(FamilyPlane::Trigram)
        .map_err(|e| plane_error(e.to_string()))?;
    let mut pin =
        crate::pins::LivePin::create(registration).map_err(|e| plane_error(e.to_string()))?;
    // Pin every blob key in one batch before the first put: protecting key by
    // key rewrote and fsynced the whole pin file once per file.
    let keys: Vec<_> = manifest
        .entries()
        .filter_map(|(_, entry)| match entry {
            super::manifest_v2::EntryV2::Regular { content, .. } => Some(
                crate::blob_store::v2::TrigramKey {
                    content: *content,
                    policy,
                }
                .family_key(),
            ),
            _ => None,
        })
        .collect();
    pin.protect(&keys).map_err(|e| plane_error(e.to_string()))?;
    let mut members = Vec::new();
    for (path, entry) in manifest.entries_mut() {
        let super::manifest_v2::EntryV2::Regular {
            content,
            size,
            planes,
            ..
        } = entry
        else {
            continue;
        };
        let key = crate::blob_store::v2::TrigramKey {
            content: *content,
            policy,
        }
        .family_key();
        let attachment = observed
            .get(path)
            .and_then(|entry| entry.attachments.get(&FamilyPlane::Trigram))
            .and_then(|value| value.downcast_ref::<Attachment>())
            .filter(|value| value.content == *content && value.policy == policy.fingerprint());
        let payload = if let Some(attachment) = attachment {
            attachment.payload.clone()
        } else {
            let relative = rel_path_to_os(path).map_err(|e| plane_error(e.to_string()))?;
            let bytes = fs::read(root.join(relative)).map_err(|e| plane_error(e.to_string()))?;
            if ContentHash::of(&bytes) != *content || bytes.len() as u64 != *size {
                return Err(plane_error("source changed during trigram construction"));
            }
            TrigramPayload::extract(&bytes, &policy)
        };
        store
            .put_or_touch(&key, &payload.encode())
            .map_err(|e| plane_error(e.to_string()))?;
        planes.trigram = Some(super::readiness::PlaneState::ready(&key));
        members.push(super::segment_store::SegmentMember {
            rel_path: path.clone(),
            content: *content,
            size: *size,
        });
    }
    let existing_segment = manifest
        .header()
        .segment
        .as_ref()
        .and_then(|id| crate::blob_store::v2::parse_hex32(id));
    if let Some(id) = existing_segment {
        pin.protect_segment(&id)
            .map_err(|e| plane_error(e.to_string()))?;
        let path = crate::blob_store::v2::segment_path(
            registration.registry().storage(),
            registration.family(),
            &id,
        )
        .map_err(|e| plane_error(e.to_string()))?;
        let segment = SegmentReader::open(&path).map_err(|e| plane_error(e.to_string()))?;
        if segment.id() != id || segment.policy_fingerprint() != policy.fingerprint() {
            return Err(plane_error("seed segment identity or producer mismatch"));
        }
        // Keep the verified shared segment. Replacement keys and tombstones in
        // the projected manifest become the generation's small overlay at open.
        return Ok(MaterializedTrigrams { pin, segment: id });
    }
    let segment = super::segment_store::build_from_blobs(&store, &members, &policy)
        .map_err(|e| plane_error(e.to_string()))?;
    pin.protect_segment(&segment.id)
        .map_err(|e| plane_error(e.to_string()))?;
    super::segment_store::write_segment(&store, registration.registry().storage(), &segment, None)
        .map_err(|e| plane_error(e.to_string()))?;
    manifest.header_mut().segment = Some(crate::blob_store::v2::to_hex(&segment.id));
    Ok(MaterializedTrigrams {
        pin,
        segment: segment.id,
    })
}

/// The lexical plane of a v3 generation (a base, or a worker's delta): every
/// regular entry's trigram payload goes into `blobs/v3/<family>/trigram.sqlite`
/// through one v3 publication, after `live` protects every key, and the entry
/// becomes `Ready`. `bytes` holds the strictly read source of each entry; an
/// entry whose bytes no longer hash to its content fails the whole call.
///
/// No segment is written: v3 has no segment rows or segment collection yet,
/// so v3 generations carry `segment: None` and readers build from payloads.
pub fn materialize_v3(
    storage: &Path,
    family: &str,
    manifest: &mut super::manifest_v2::ManifestV2,
    bytes: &BTreeMap<RelPath, Vec<u8>>,
    live: &mut crate::pins::LivePin,
    policy: TrigramPolicy,
) -> Result<(), super::contracts::PlaneError> {
    if manifest.header().producers.trigram != policy.fingerprint_hex() {
        return Err(plane_error("trigram producer mismatch"));
    }
    let mut payloads = Vec::new();
    for (path, entry) in manifest.entries() {
        let super::manifest_v2::EntryV2::Regular { content, size, .. } = entry else {
            continue;
        };
        let source = bytes
            .get(path)
            .ok_or_else(|| plane_error(format!("no strict read for {path:?}")))?;
        if ContentHash::of(source) != *content || source.len() as u64 != *size {
            return Err(plane_error("source changed during trigram construction"));
        }
        let key = crate::blob_store::v2::TrigramKey {
            content: *content,
            policy,
        }
        .family_key();
        payloads.push((
            path.clone(),
            key,
            TrigramPayload::extract(source, &policy).encode(),
        ));
    }
    put_v3_payloads(storage, family, manifest, payloads, live)
}

/// [`materialize_v3`] for a worker delta: each regular entry's payload comes
/// from its strict-reconcile attachment when that attachment names the same
/// content, otherwise from a fresh read of the checkout that must hash to the
/// entry's content.
pub fn materialize_v3_observed(
    storage: &Path,
    family: &str,
    root: &Path,
    manifest: &mut super::manifest_v2::ManifestV2,
    observed: &BTreeMap<RelPath, super::snapshot::LiveEntry>,
    live: &mut crate::pins::LivePin,
    policy: TrigramPolicy,
) -> Result<(), super::contracts::PlaneError> {
    if manifest.header().producers.trigram != policy.fingerprint_hex() {
        return Err(plane_error("trigram producer mismatch"));
    }
    let mut payloads = Vec::new();
    for (path, entry) in manifest.entries() {
        let super::manifest_v2::EntryV2::Regular { content, size, .. } = entry else {
            continue;
        };
        let attachment = observed
            .get(path)
            .and_then(|entry| entry.attachments.get(&FamilyPlane::Trigram))
            .and_then(|value| value.downcast_ref::<Attachment>())
            .filter(|value| value.content == *content && value.policy == policy.fingerprint());
        let payload = match attachment {
            Some(attachment) => attachment.payload.clone(),
            None => {
                let relative = rel_path_to_os(path).map_err(|e| plane_error(e.to_string()))?;
                let bytes =
                    fs::read(root.join(relative)).map_err(|e| plane_error(e.to_string()))?;
                if ContentHash::of(&bytes) != *content || bytes.len() as u64 != *size {
                    return Err(plane_error("source changed during trigram construction"));
                }
                TrigramPayload::extract(&bytes, &policy)
            }
        };
        let key = crate::blob_store::v2::TrigramKey {
            content: *content,
            policy,
        }
        .family_key();
        payloads.push((path.clone(), key, payload.encode()));
    }
    put_v3_payloads(storage, family, manifest, payloads, live)
}

fn put_v3_payloads(
    storage: &Path,
    family: &str,
    manifest: &mut super::manifest_v2::ManifestV2,
    payloads: Vec<(RelPath, crate::blob_store::v2::FamilyKey, Vec<u8>)>,
    live: &mut crate::pins::LivePin,
) -> Result<(), super::contracts::PlaneError> {
    if payloads.is_empty() {
        return Ok(());
    }
    // Protect every key before the first put, as the v2 materializer does.
    live.protect(&payloads.iter().map(|(_, key, _)| *key).collect::<Vec<_>>())
        .map_err(|e| plane_error(e.to_string()))?;
    let request = payloads
        .iter()
        .map(|(_, _, payload)| payload.len() as u64)
        .sum::<u64>()
        .saturating_mul(2)
        .saturating_add(1 << 20);
    let schema = FamilyPlane::Trigram.payload_schema();
    crate::blob_store::v3::publish(storage, family, request, None, |publication| {
        for (_, key, payload) in &payloads {
            publication.put(key, payload, schema)?;
        }
        Ok(())
    })
    .map_err(|e| plane_error(e.to_string()))?;
    for (path, key, _) in &payloads {
        if let Some(super::manifest_v2::EntryV2::Regular { planes, .. }) = manifest.get_mut(path) {
            planes.trigram = Some(super::readiness::PlaneState::ready(key));
        }
    }
    Ok(())
}

type Residents = BTreeMap<(String, String), Arc<GenerationIndex>>;

/// Resident immutable data shared across checkout opens. Register explicitly
/// with `PlaneHooks::register_adapter`; construction alone changes no routing.
pub struct TrigramAdapter {
    storage: PathBuf,
    policy: TrigramPolicy,
    residents: std::sync::Mutex<Residents>,
    segments: std::sync::Mutex<BTreeMap<(String, [u8; 32]), std::sync::Weak<SegmentReader>>>,
}

#[cfg(test)]
thread_local! {
    static BEFORE_SEGMENT_OPEN: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_READINESS_SCAN: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

impl TrigramAdapter {
    pub fn new(storage: PathBuf, policy: TrigramPolicy) -> Self {
        Self {
            storage,
            policy,
            residents: std::sync::Mutex::new(BTreeMap::new()),
            segments: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    pub fn resident(&self, scope: &str, generation: &str) -> Option<Arc<GenerationIndex>> {
        self.residents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(scope.into(), generation.into()))
            .cloned()
    }

    /// Open a pinned generation's segment and generation-local replacement
    /// blobs. Readers use read-only store handles and never repair artifacts.
    pub fn open_index(
        &self,
        access: &super::contracts::ViewAccess,
        snapshot: &Snapshot,
    ) -> Result<GenerationIndex, super::contracts::PlaneError> {
        if let super::contracts::StorageRoute::V3(route) = access.route(snapshot.generation()) {
            return self.open_v3_index(&route, snapshot);
        }
        let id = snapshot
            .generation()
            .segment_id()
            .ok_or_else(|| plane_error("generation has no trigram segment"))?;
        let segment_key = (access.family().to_string(), id);
        let cached = {
            let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
            segments.retain(|_, segment| segment.strong_count() != 0);
            segments
                .get(&segment_key)
                .and_then(|segment| segment.upgrade())
        };
        let segment = if let Some(segment) = cached {
            segment
        } else {
            #[cfg(test)]
            BEFORE_SEGMENT_OPEN.with(|hook| {
                if let Some(hook) = hook.borrow().as_ref() {
                    hook();
                }
            });
            let path = crate::blob_store::v2::segment_path(&self.storage, access.family(), &id)
                .map_err(|e| plane_error(e.to_string()))?;
            let reader =
                Arc::new(SegmentReader::open(&path).map_err(|e| plane_error(e.to_string()))?);
            if reader.id() != id {
                return Err(plane_error("segment identity mismatch"));
            }
            // Another opener may have admitted the same immutable segment
            // while this one parsed it. Share that reader rather than replace it.
            let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = segments
                .get(&segment_key)
                .and_then(|segment| segment.upgrade())
            {
                existing
            } else {
                segments.insert(segment_key, Arc::downgrade(&reader));
                reader
            }
        };
        match access {
            super::contracts::ViewAccess::Owner(registration) => {
                let store = registration
                    .open_store(FamilyPlane::Trigram)
                    .map_err(|e| plane_error(e.to_string()))?;
                GenerationIndex::open(segment, snapshot, self.policy, &store).map_err(plane_error)
            }
            super::contracts::ViewAccess::Reader { registration, .. } => {
                let store = registration
                    .open_store(FamilyPlane::Trigram)
                    .map_err(|e| plane_error(e.to_string()))?
                    .ok_or_else(|| plane_error("trigram store absent"))?;
                GenerationIndex::open(segment, snapshot, self.policy, &store).map_err(plane_error)
            }
        }
    }
}

impl TrigramAdapter {
    /// A v3 generation has no segment file: v3 keeps no segment rows, so the
    /// base's segment is assembled in memory from the base's stored payloads
    /// (shared through the segment cache while an index holds it), and
    /// the worker's changed entries load as the usual overlay. Missing or
    /// not-ready payloads stay direct-scan gaps, as on the v2 path.
    fn open_v3_index(
        &self,
        route: &super::snapshot::V3Route,
        snapshot: &Snapshot,
    ) -> Result<GenerationIndex, super::contracts::PlaneError> {
        let generation = snapshot.generation();
        if !generation.segment_ids().is_empty() {
            return Err(plane_error("a v3 generation names a trigram segment"));
        }
        let source = crate::blob_store::v3::PlaneReader::open_existing(
            &route.storage,
            &route.family,
            FamilyPlane::Trigram,
        )
        .map_err(|e| plane_error(e.to_string()))?;
        let base = generation
            .base()
            .map_or(generation.as_ref(), |base| base.as_ref());
        let base_scope = base
            .v3_route()
            .map_or(route.scope.as_str(), |base| base.scope.as_str());
        let segment_key = (
            format!("v3:{}", route.family),
            *blake3::hash(format!("{base_scope}\0{}", base.name()).as_bytes()).as_bytes(),
        );
        let cached = {
            let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
            segments.retain(|_, segment| segment.strong_count() != 0);
            segments
                .get(&segment_key)
                .and_then(|segment| segment.upgrade())
        };
        let segment = match cached {
            Some(segment) => segment,
            None => {
                let members: Vec<_> = base
                    .entries()
                    .filter_map(|(path, entry)| {
                        let content = entry.content()?;
                        let key = crate::blob_store::v2::TrigramKey {
                            content,
                            policy: self.policy,
                        }
                        .family_key();
                        (base.plane_readiness(path, FamilyPlane::Trigram)
                            == Some(&super::readiness::PlaneState::ready(&key)))
                        .then(|| super::segment_store::SegmentMember {
                            rel_path: path.clone(),
                            content,
                            size: match entry {
                                super::manifest_v2::EntryV2::Regular { size, .. } => *size,
                                _ => 0,
                            },
                        })
                    })
                    .collect();
                let bytes = super::segment_store::build_from_blobs(&source, &members, &self.policy)
                    .map_err(|e| plane_error(e.to_string()))?;
                let reader = Arc::new(
                    SegmentReader::from_bytes(&bytes.bytes)
                        .map_err(|e| plane_error(e.to_string()))?,
                );
                let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
                match segments
                    .get(&segment_key)
                    .and_then(|segment| segment.upgrade())
                {
                    Some(existing) => existing,
                    None => {
                        segments.insert(segment_key, Arc::downgrade(&reader));
                        reader
                    }
                }
            }
        };
        GenerationIndex::open(segment, snapshot, self.policy, &source).map_err(plane_error)
    }
}

impl super::contracts::PlaneAdapter for TrigramAdapter {
    fn plane(&self) -> FamilyPlane {
        FamilyPlane::Trigram
    }
    fn producer(&self) -> String {
        self.policy.fingerprint_hex()
    }
    fn applies_to(&self, path: &RelPath) -> bool {
        !path.is_synthetic()
    }
    fn open_generation(
        &self,
        access: &super::contracts::ViewAccess,
        generation: &Arc<super::snapshot::ComposedGeneration>,
    ) -> Result<(), super::contracts::PlaneError> {
        let snapshot = super::snapshot::LiveDelta::new(generation.clone()).snapshot();
        let index = Arc::new(self.open_index(access, &snapshot)?);
        self.residents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((access.scope().into(), generation.resident_key()), index);
        Ok(())
    }
    fn release_generation(&self, access: &super::contracts::ViewAccess, generation: &str) {
        self.residents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(access.scope().into(), generation.into()));
    }
    fn readiness(
        &self,
        access: &super::contracts::ViewAccess,
        snapshot: &Snapshot,
    ) -> super::readiness::PlaneReadiness {
        let residents = self.residents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = residents
            .get(&(
                access.scope().to_owned(),
                snapshot.generation().resident_key(),
            ))
            .cloned()
        else {
            return super::readiness::PlaneReadiness::Building;
        };
        drop(residents);
        #[cfg(test)]
        BEFORE_READINESS_SCAN.with(|hook| {
            if let Some(hook) = hook.borrow().as_ref() {
                hook();
            }
        });
        let members = snapshot.membership().into_keys().collect();
        let pending = index.candidates(snapshot, &members, "").direct.len();
        let failed = snapshot
            .generation()
            .entries()
            .filter(|(path, entry)| {
                !matches!(snapshot.source(path), Source::Live(_))
                    && matches!(
                        entry.plane_state(FamilyPlane::Trigram),
                        Some(super::readiness::PlaneState::Failed { .. })
                    )
            })
            .count();
        super::readiness::PlaneReadiness::Ready { pending, failed }
    }
}

#[cfg(test)]
mod tests {
    struct FixedSource(Snapshot);
    impl LexicalSnapshotSource for FixedSource {
        fn acquire(&self) -> Result<Snapshot, String> {
            Ok(self.0.clone())
        }
        fn record_change(&self, _: &Path) {}
    }

    #[test]
    fn lexical_views_projection_refuses_uncertain_or_wrong_policy_snapshots() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let storage = temp.path().join("storage");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), "needle\n").unwrap();
        let policy = TrigramPolicy {
            max_file_size: 1024,
        };
        let source = BaseBuilderTestSource::new(&root, &storage, "lexical-guards", policy);
        let snapshot = source.acquire().unwrap();
        let make_view = |snapshot, policy| {
            LexicalView::with_source(
                root.clone(),
                crate::root_cache::StoragePaths::same(&storage),
                "lexical-guards".into(),
                policy,
                Arc::new(FixedSource(snapshot)),
            )
        };
        for state in [
            WatcherState::Overflowed,
            WatcherState::Stopped,
            WatcherState::Reconciling,
        ] {
            let mut delta = super::super::snapshot::LiveDelta::new(snapshot.generation().clone());
            delta.set_watcher(state);
            assert!(make_view(delta.snapshot(), policy)
                .snapshot()
                .unwrap_err()
                .contains("unverified writes"));
        }
        let mut delta = super::super::snapshot::LiveDelta::new(snapshot.generation().clone());
        delta.record_intent(RelPath::new(b"a.rs".to_vec()).unwrap());
        assert!(make_view(delta.snapshot(), policy)
            .snapshot()
            .unwrap_err()
            .contains("unverified writes"));
        assert!(make_view(
            snapshot,
            TrigramPolicy {
                max_file_size: 1025
            }
        )
        .snapshot()
        .unwrap_err()
        .contains("not ready"));
    }

    #[test]
    fn lexical_views_projection_matches_legacy_masks_ranks_and_membership() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let storage = temp.path().join("storage");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "fn needle_alpha() {}\nneedle beta\n").unwrap();
        fs::write(root.join("src/b.rs"), "fn needle_beta() {}\n").unwrap();
        fs::write(root.join("binary.dat"), [0, 1, 2, 3]).unwrap();
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "needle ignored\n").unwrap();
        let policy = TrigramPolicy {
            max_file_size: 1024 * 1024,
        };
        let source = BaseBuilderTestSource::new(&root, &storage, "lexical-parity", policy);
        let view = LexicalView::with_source(
            root.clone(),
            crate::root_cache::StoragePaths::same(&storage),
            "lexical-parity".into(),
            policy,
            source.clone(),
        );
        let projected = view.snapshot().unwrap();
        let legacy =
            crate::search_index::SearchIndex::build_with_limit_serial(&root, policy.max_file_size)
                .snapshot();
        assert_eq!(projected.glob("**/*", &root), legacy.glob("**/*", &root));
        for pattern in [
            "needle",
            "needle_(alpha|beta)",
            "n.e.*beta",
            "^fn",
            "NEEDLE",
            "a",
        ] {
            for insensitive in [false, true] {
                assert_eq!(
                    projected
                        .grep(pattern, !insensitive, &[], &[], &root, 100)
                        .unwrap()
                        .matches,
                    legacy
                        .grep(pattern, !insensitive, &[], &[], &root, 100)
                        .unwrap()
                        .matches,
                    "{pattern} insensitive={insensitive}"
                );
            }
        }
        for query in ["needle", "needle alpha", "beta", "absent"] {
            let trigrams = crate::search_index::query_trigrams_from_tokens(
                &query.split_whitespace().collect::<Vec<_>>(),
            );
            assert_eq!(
                projected
                    .lexical_rank_at_depth(&trigrams, None, 20)
                    .unwrap()
                    .files,
                legacy
                    .lexical_rank_at_depth(&trigrams, None, 20)
                    .unwrap()
                    .files
            );
        }
        assert!(source.acquire().unwrap().generation().v3_route().is_some());
        assert!(!storage.join("blobs/v2").exists());
        assert!(!storage.join("views/v3").exists());
    }

    #[test]
    fn unrepresentable_query_gap_keeps_the_exact_relative_bytes() {
        let path = RelPath::new(b"a-\xff.rs".to_vec()).unwrap();
        let mut result = QueryResult {
            membership: BTreeSet::new(),
            matches: Vec::new(),
            gaps: Vec::new(),
            relative_gaps: Vec::new(),
        };
        record_conversion_gap(&mut result, &path);
        assert_eq!(result.relative_gaps, vec![path]);
        assert!(result.gaps.is_empty());
        assert!(!result.complete());
        assert_eq!(
            serde_json::to_string(&result.relative_gaps[0]).unwrap(),
            r#"{"b64":"YS3/LnJz"}"#
        );
        assert_eq!(
            result.gap_diagnostics(),
            vec![r#"unrepresentable relative path: {"b64":"YS3/LnJz"}"#]
        );
    }
    use super::super::contracts::{PlaneAdapter, ViewAccess};
    use super::super::manifest_v2::{ManifestHeader, ManifestV2, Producers};
    use super::super::snapshot::{ComposedGeneration, LiveDelta};
    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        ViewAccess,
        Arc<TrigramAdapter>,
        LiveDelta,
    ) {
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let registry =
            crate::views::registry::FamilyRegistry::open(storage.path(), "family").unwrap();
        let owner = registry
            .register_view_with_storage(registry.storage(), "scope", root.path())
            .unwrap();
        let store = owner.open_store(FamilyPlane::Trigram).unwrap();
        let policy = TrigramPolicy {
            max_file_size: 1 << 20,
        };
        let segment = super::super::segment_store::assemble(&policy, []).unwrap();
        let path =
            crate::blob_store::v2::segment_path(storage.path(), "family", &segment.id).unwrap();
        fs::write(path, &segment.bytes).unwrap();
        drop(store);
        let manifest = ManifestV2::new(ManifestHeader {
            producers: Producers {
                trigram: policy.fingerprint_hex(),
                semantic: None,
                callgraph: "test".into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: Some(crate::blob_store::v2::to_hex(&segment.id)),
        });
        let delta = LiveDelta::new(Arc::new(ComposedGeneration::new(
            "generation",
            manifest,
            None,
        )));
        let adapter = Arc::new(TrigramAdapter::new(storage.path().to_path_buf(), policy));
        (storage, root, ViewAccess::Owner(owner), adapter, delta)
    }

    #[test]
    fn segment_parsing_does_not_hold_the_cache_mutex() {
        let (_storage, _root, access, adapter, delta) = fixture();
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let count = checked.clone();
        let weak = Arc::downgrade(&adapter);
        BEFORE_SEGMENT_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                assert!(weak.upgrade().unwrap().segments.try_lock().is_ok());
                count.store(true, std::sync::atomic::Ordering::SeqCst);
            }))
        });
        let result = adapter.open_index(&access, &delta.snapshot());
        BEFORE_SEGMENT_OPEN.with(|hook| hook.borrow_mut().take());
        result.unwrap();
        assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn readiness_scanning_does_not_hold_the_resident_mutex() {
        let (_storage, _root, access, adapter, delta) = fixture();
        adapter.open_generation(&access, delta.base()).unwrap();
        let checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let count = checked.clone();
        let weak = Arc::downgrade(&adapter);
        BEFORE_READINESS_SCAN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                assert!(weak.upgrade().unwrap().residents.try_lock().is_ok());
                count.store(true, std::sync::atomic::Ordering::SeqCst);
            }))
        });
        let ready = adapter.readiness(&access, &delta.snapshot());
        BEFORE_READINESS_SCAN.with(|hook| hook.borrow_mut().take());
        assert_eq!(
            ready,
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0
            }
        );
        assert!(checked.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn unhealthy_queries_bypass_indexed_candidate_selection() {
        let (_storage, root, access, adapter, mut delta) = fixture();
        fs::write(root.path().join("a.rs"), b"pub fn alpha() {}").unwrap();
        let index = adapter.open_index(&access, &delta.snapshot()).unwrap();
        delta.set_watcher(WatcherState::Overflowed);
        CANDIDATE_SCANS.with(|count| count.set(0));
        let result = index.query(root.path(), &delta.snapshot(), "alpha");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(CANDIDATE_SCANS.with(|count| count.get()), 0);
    }
}
