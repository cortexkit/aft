//! Opt-in trigram views: shared segment, generation overlay, and live delta.
//!
//! No adapter is registered in `contracts::process_hooks` by this module;
//! existing search commands keep using `SearchIndex` by default.
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

/// Generation-local replacements and tombstones relative to the shared segment.
/// Missing payloads are gaps, not empty postings.
#[derive(Debug)]
pub struct GenerationIndex {
    pub segment: Arc<SegmentReader>,
    overlay: BTreeMap<RelPath, Option<Arc<Attachment>>>,
    policy: TrigramPolicy,
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
        for (path, entry) in snapshot.generation().manifest().entries() {
            let Some(content) = entry.content() else {
                continue;
            };
            if base.get(path) == Some(&content) {
                continue;
            }
            let key = crate::blob_store::v2::TrigramKey { content, policy }.family_key();
            if entry.plane_state(FamilyPlane::Trigram)
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
            || snapshot.generation().manifest().header().producers.trigram
                != policy.fingerprint_hex()
        {
            return Err("trigram producer mismatch".into());
        }
        let manifest = snapshot.generation().manifest();
        let mut overlay = BTreeMap::new();
        let base = segment
            .files()
            .iter()
            .map(|file| (file.rel_path.clone(), file.content))
            .collect::<BTreeMap<_, _>>();
        for path in base.keys() {
            if manifest
                .get(path)
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
        let mut candidates = self.candidates(snapshot, &members, literal);
        if active || snapshot.watcher() != WatcherState::Healthy {
            candidates.indexed.clear();
            candidates.direct = members.clone();
        }
        let mut result = QueryResult {
            membership: members,
            matches: Vec::new(),
            gaps,
        };
        for path in candidates.indexed.union(&candidates.direct) {
            let absolute = match rel_path_to_os(path) {
                Ok(relative) => root.join(relative),
                Err(_) => {
                    result.gaps.push(root.to_path_buf());
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
}

impl QueryResult {
    pub fn complete(&self) -> bool {
        self.gaps.is_empty()
    }
}

/// Runtime-owned publication includes semantic and callgraph states as well as
/// trigrams. Delegating publication prevents a trigram-only manifest from
/// erasing the other indexes' readiness or keys.
pub type Publisher = dyn Fn(
        &super::contracts::ViewAccess,
        &Snapshot,
        bool,
    ) -> Result<Arc<super::snapshot::OpenGeneration>, super::contracts::PlaneError>
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
        plane: FamilyPlane::Trigram,
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
    ) -> Result<Arc<super::snapshot::OpenGeneration>, super::contracts::PlaneError> {
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
        pin.protect(&[key])
            .map_err(|e| plane_error(e.to_string()))?;
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

type Residents = BTreeMap<(String, String), Arc<GenerationIndex>>;

/// Resident immutable data shared across checkout opens. Register explicitly
/// with `PlaneHooks::register_adapter`; construction alone changes no routing.
pub struct TrigramAdapter {
    storage: PathBuf,
    policy: TrigramPolicy,
    residents: std::sync::Mutex<Residents>,
    segments: std::sync::Mutex<BTreeMap<(String, [u8; 32]), std::sync::Weak<SegmentReader>>>,
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
        let id = snapshot
            .generation()
            .manifest()
            .header()
            .segment
            .as_ref()
            .and_then(|id| crate::blob_store::v2::parse_hex32(id))
            .ok_or_else(|| plane_error("generation has no trigram segment"))?;
        let mut segments = self.segments.lock().unwrap_or_else(|e| e.into_inner());
        segments.retain(|_, segment| segment.strong_count() != 0);
        let segment_key = (access.family().to_string(), id);
        let segment = if let Some(segment) = segments
            .get(&segment_key)
            .and_then(|segment| segment.upgrade())
        {
            segment
        } else {
            let path = crate::blob_store::v2::segment_path(&self.storage, access.family(), &id)
                .map_err(|e| plane_error(e.to_string()))?;
            let reader =
                Arc::new(SegmentReader::open(&path).map_err(|e| plane_error(e.to_string()))?);
            if reader.id() != id {
                return Err(plane_error("segment identity mismatch"));
            }
            segments.insert(segment_key, Arc::downgrade(&reader));
            reader
        };
        drop(segments);
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
        generation: &Arc<super::snapshot::OpenGeneration>,
    ) -> Result<(), super::contracts::PlaneError> {
        let snapshot = super::snapshot::LiveDelta::new(generation.clone()).snapshot();
        let index = Arc::new(self.open_index(access, &snapshot)?);
        self.residents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((access.scope().into(), generation.name().into()), index);
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
        let Some(index) = residents.get(&(
            access.scope().to_owned(),
            snapshot.generation().name().to_owned(),
        )) else {
            return super::readiness::PlaneReadiness::Building;
        };
        let members = snapshot.membership().into_keys().collect();
        let pending = index.candidates(snapshot, &members, "").direct.len();
        let failed = snapshot
            .generation()
            .manifest()
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
