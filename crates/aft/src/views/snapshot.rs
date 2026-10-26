//! Pinned query snapshots, the live delta, and the generation switch.
//!
//! A query reads one `(generation, delta version, intent version)` snapshot,
//! taken before its first index read, and never re-reads the pointer
//! mid-query. Taking a snapshot is an `Arc` clone: the generation is kept
//! resident by one read marker per open generation, not one per query.
//!
//! The live delta holds the difference between the checkout's disk and its
//! generation. Its invariant: **an entry exists for a path exactly when the
//! last observed disk state differs from the generation's entry**. Every
//! disk mutation is journaled, including one that removes an entry. Readiness
//! and write intents are versioned separately: neither observes new disk bytes.
//!
//! When a new generation `G+1` is published (by this daemon or by another
//! one), the successor delta is derived over
//! `K = keys(delta@v) ∪ keys(readiness@v) ∪ paths(diff(G, G+1))`: for each path in `K` the last
//! observed disk state is the live entry's if there is one, else `G`'s entry
//! (exact, by the invariant), and the successor has an entry exactly when that
//! state differs from `G+1`. The rule never reads what the fold itself read,
//! so it is correct for a revert to `G`'s bytes or an add-then-delete anywhere
//! between the fold's read and the swap, and for a foreign CAS winner.
//! Mutations after `v` are replayed from the journal under the swap lock, and
//! a successor built against an older epoch is discarded.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::blob_store::v2::{ContentHash, FamilyPlane};

use super::manifest_v2::{
    EntryPlanes, EntryV2, ManifestHeader, ManifestV2, Producers, PublishedFrontier,
};
use super::readiness::{LiveEntries, PlaneState};
use super::registry::ProtectedGeneration;
use super::RelPath;

/// The last observed disk state of one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum DiskState {
    Present { content: ContentHash, size: u64 },
    Absent,
}

impl DiskState {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self::Present {
            content: ContentHash::of(bytes),
            size: bytes.len() as u64,
        }
    }

    /// What a generation says about a path's disk bytes. Only regular files
    /// have content; symlinks, gitlinks and synthetic entries read as absent
    /// here and are reconciled through membership instead.
    pub fn in_generation(manifest: &ManifestV2, rel_path: &RelPath) -> Self {
        match manifest.get(rel_path) {
            Some(EntryV2::Regular { content, size, .. }) => Self::Present {
                content: *content,
                size: *size,
            },
            _ => Self::Absent,
        }
    }
}

/// How a generation is kept resident while snapshots of it exist.
#[derive(Debug)]
pub enum Residency {
    /// This process's own view: a read marker in its view directory.
    Marker(crate::root_cache::ReadMarker),
    /// A registry reader's pin, taken with pin-then-verify.
    Protected(ProtectedGeneration),
}

/// One opened, immutable generation. Dropping the last `Arc` releases its
/// residency pin.
///
/// A plane cannot bypass composition through a raw manifest:
/// ```compile_fail
/// fn bypass(generation: &aft::views::snapshot::ComposedGeneration) {
///     let _ = generation.manifest();
/// }
/// ```
#[derive(Debug)]
pub struct ComposedGeneration {
    name: String,
    delta: Arc<ManifestV2>,
    base: Option<Arc<ComposedGeneration>>,
    overrides: BTreeMap<RelPath, EntryV2>,
    route: Option<V3Route>,
    _residency: Option<Residency>,
}

/// Where a v3 generation's artifacts live: plane rows in
/// `blobs/v3/<family>/<plane>.sqlite`, and its manifest, pointer and derived
/// files in the member directory `blobs/v3/<family>/views/<scope>`. A
/// generation without a route is a v2 generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct V3Route {
    pub storage: std::path::PathBuf,
    pub family: String,
    pub scope: String,
}

impl V3Route {
    pub fn member_dir(&self) -> std::path::PathBuf {
        crate::blob_store::v3::view_dir(&self.storage, &self.family, &self.scope)
            .unwrap_or_else(|_| self.storage.join("blobs/v3/invalid"))
    }
}

/// Compatibility name for callers that open a flat generation. Both names
/// expose only the composed projection, never a raw manifest accessor.
pub type OpenGeneration = ComposedGeneration;

impl ComposedGeneration {
    pub fn new(
        name: impl Into<String>,
        manifest: ManifestV2,
        residency: Option<Residency>,
    ) -> Self {
        assert!(
            manifest.base_header().is_none(),
            "a worker must be opened with ComposedGeneration::compose"
        );
        Self {
            name: name.into(),
            delta: Arc::new(manifest),
            base: None,
            overrides: BTreeMap::new(),
            route: None,
            _residency: residency,
        }
    }

    pub fn compose(
        name: impl Into<String>,
        delta: ManifestV2,
        base: Arc<ComposedGeneration>,
        residency: Option<Residency>,
    ) -> super::Result<Self> {
        if delta.base_header().is_none()
            || base.base.is_some()
            || base.delta.base_header().is_some()
        {
            return Err(super::ViewError::InvalidManifest(
                "composition requires a worker and a depth-one base".into(),
            ));
        }
        delta.ensure_producers(&base.header().producers)?;
        for path in delta.tombstones() {
            if base.entry(path).is_none() {
                return Err(super::ViewError::InvalidManifest(
                    "a worker tombstone must name an inherited base entry".into(),
                ));
            }
        }
        let mut overrides = BTreeMap::new();
        for (path, patch) in delta.readiness_overrides() {
            let Some(EntryV2::Regular { .. }) = base.entry(path) else {
                return Err(super::ViewError::InvalidManifest(
                    "readiness override must name an inherited regular file".into(),
                ));
            };
            let mut entry = base.entry(path).unwrap().clone();
            if let EntryV2::Regular { planes, .. } = &mut entry {
                for plane in FamilyPlane::ALL {
                    if let Some(state) = patch.get(plane) {
                        *planes.get_mut(plane) = Some(state.clone());
                    }
                }
            }
            overrides.insert(path.clone(), entry);
        }
        Ok(Self {
            name: name.into(),
            delta: Arc::new(delta),
            base: Some(base),
            overrides,
            route: None,
            _residency: residency,
        })
    }

    /// Marks this generation as stored in the v3 namespace at `route`.
    pub fn with_v3_route(mut self, route: V3Route) -> Self {
        self.route = Some(route);
        self
    }

    /// The v3 location of this generation, or `None` for a v2 generation.
    pub fn v3_route(&self) -> Option<&V3Route> {
        self.route.as_ref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn resident_key(&self) -> String {
        match &self.base {
            Some(base) => format!(
                "{}:{}:{}",
                self.name,
                self.base_header().unwrap().scope,
                base.name()
            ),
            None => self.name.clone(),
        }
    }

    pub fn header(&self) -> &ManifestHeader {
        self.delta.header()
    }
    pub fn base_header(&self) -> Option<&super::manifest_v2::BaseHeader> {
        self.delta.base_header()
    }
    pub fn published(&self) -> Option<&PublishedFrontier> {
        self.delta.published()
    }
    pub fn base(&self) -> Option<&Arc<ComposedGeneration>> {
        self.base.as_ref()
    }
    pub fn ensure_producers(&self, producers: &Producers) -> super::Result<()> {
        self.delta.ensure_producers(producers)
    }

    pub fn entry(&self, path: &RelPath) -> Option<&EntryV2> {
        if self.delta.tombstones().contains(path) {
            return None;
        }
        self.delta
            .get(path)
            .or_else(|| self.overrides.get(path))
            .or_else(|| self.base.as_ref().and_then(|base| base.entry(path)))
    }

    /// Sorted, lazy merge: a caller's existing walk budget stops the producing
    /// iterator without first collecting the inherited membership.
    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &EntryV2)> {
        let mut delta = self.delta.entries().peekable();
        let mut base = self
            .base
            .as_ref()
            .into_iter()
            .flat_map(|base| base.delta.entries())
            .peekable();
        std::iter::from_fn(move || loop {
            let next = match (delta.peek(), base.peek()) {
                (Some((d, _)), Some((b, _))) => match d.cmp(b) {
                    std::cmp::Ordering::Less => delta.next(),
                    std::cmp::Ordering::Greater => base.next(),
                    std::cmp::Ordering::Equal => {
                        base.next();
                        delta.next()
                    }
                },
                (Some(_), None) => delta.next(),
                (None, Some(_)) => base.next(),
                (None, None) => return None,
            };
            if let Some((path, entry)) = next {
                if !self.delta.tombstones().contains(path) {
                    return Some((path, self.overrides.get(path).unwrap_or(entry)));
                }
            }
        })
    }

    pub fn plane_readiness(&self, path: &RelPath, plane: FamilyPlane) -> Option<&PlaneState> {
        self.entry(path).and_then(|entry| entry.plane_state(plane))
    }

    pub fn owns_work(&self, path: &RelPath, plane: FamilyPlane) -> bool {
        self.base.is_none()
            || self.delta.get(path).is_some_and(|entry| {
                delta_owns_plane(
                    self.base.as_ref().and_then(|base| base.entry(path)),
                    entry,
                    plane,
                )
            })
            || self
                .delta
                .readiness_overrides()
                .get(path)
                .and_then(|patch| patch.get(plane))
                .is_some()
    }

    pub fn segment_ids(&self) -> BTreeSet<[u8; 32]> {
        self.delta
            .segment_id()
            .into_iter()
            .chain(self.base.as_ref().and_then(|base| base.delta.segment_id()))
            .collect()
    }

    pub fn segment_id(&self) -> Option<[u8; 32]> {
        self.delta
            .segment_id()
            .or_else(|| self.base.as_ref().and_then(|base| base.delta.segment_id()))
    }

    pub fn owned_ready_keys(&self) -> impl Iterator<Item = crate::blob_store::v2::FamilyKey> + '_ {
        self.delta.ready_keys()
    }
    pub fn ready_keys(&self) -> BTreeSet<crate::blob_store::v2::FamilyKey> {
        self.owned_ready_keys()
            .chain(
                self.base
                    .as_ref()
                    .into_iter()
                    .flat_map(|base| base.owned_ready_keys()),
            )
            .collect()
    }

    pub fn membership(&self) -> BTreeMap<RelPath, DiskState> {
        self.entries()
            .filter_map(|(path, entry)| match entry {
                EntryV2::Regular { content, size, .. } => Some((
                    path.clone(),
                    DiskState::Present {
                        content: *content,
                        size: *size,
                    },
                )),
                _ => None,
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries().count()
    }
    pub fn is_empty(&self) -> bool {
        self.entries().next().is_none()
    }

    pub fn diff_paths(&self, other: &Self) -> BTreeSet<RelPath> {
        let mut changed = BTreeSet::new();
        for (path, entry) in self.entries() {
            if other.entry(path) != Some(entry)
                || FamilyPlane::ALL.into_iter().any(|plane| {
                    self.plane_readiness(path, plane) != other.plane_readiness(path, plane)
                })
            {
                changed.insert(path.clone());
            }
        }
        for (path, _) in other.entries() {
            if self.entry(path).is_none() {
                changed.insert(path.clone());
            }
        }
        changed
    }

    /// Explicit export for legacy materializers and parity tests. Query consumers
    /// use the lazy projection, rather than allocating a full worker copy.
    pub fn flatten(&self) -> super::Result<ManifestV2> {
        let mut manifest = ManifestV2::new(self.header().clone());
        if let Some(id) = self.segment_id() {
            manifest.header_mut().segment = Some(crate::blob_store::v2::to_hex(&id));
        }
        for (path, entry) in self.entries() {
            let mut entry = entry.clone();
            if let EntryV2::Regular { planes, .. } = &mut entry {
                for plane in FamilyPlane::ALL {
                    *planes.get_mut(plane) = self.plane_readiness(path, plane).cloned();
                }
            }
            manifest.insert(path.clone(), entry)?;
        }
        Ok(manifest)
    }

    pub fn fold(
        &self,
        producers: &Producers,
        updates: impl IntoIterator<Item = (RelPath, Option<EntryV2>)>,
        fill: &super::readiness::FillMap,
    ) -> super::Result<ManifestV2> {
        self.ensure_producers(producers)?;
        let mut next = self.delta.as_ref().clone();
        for (path, entry) in updates {
            if let (Some(base), Some(entry)) = (&self.base, &entry) {
                if let Some(inherited) = base.entry(&path) {
                    if same_entry_identity(inherited, entry) {
                        let mut patch = EntryPlanes::default();
                        let mut representable = true;
                        for plane in FamilyPlane::ALL {
                            if entry.plane_state(plane) != inherited.plane_state(plane) {
                                match entry.plane_state(plane) {
                                    Some(state) => *patch.get_mut(plane) = Some(state.clone()),
                                    None => {
                                        representable = false;
                                        break;
                                    }
                                }
                            }
                        }
                        if representable {
                            let patch = FamilyPlane::ALL
                                .into_iter()
                                .any(|plane| patch.get(plane).is_some())
                                .then_some(patch);
                            next.inherit(path, patch);
                            continue;
                        }
                    }
                }
            }
            next.set(path, entry)?;
        }
        for (path, entry) in next.entries_mut() {
            let owned = FamilyPlane::ALL.map(|plane| {
                delta_owns_plane(
                    self.base.as_ref().and_then(|base| base.entry(path)),
                    entry,
                    plane,
                )
            });
            if let EntryV2::Regular {
                content, planes, ..
            } = entry
            {
                for (plane, owned) in FamilyPlane::ALL.into_iter().zip(owned) {
                    if owned {
                        apply_plane_fill(path, content, plane, planes.get_mut(plane), fill);
                    }
                }
            }
        }
        let overrides: Vec<_> = next
            .readiness_overrides()
            .iter()
            .map(|(path, patch)| (path.clone(), patch.clone()))
            .collect();
        for (path, mut patch) in overrides {
            if let Some(content) = self
                .base
                .as_ref()
                .and_then(|base| base.entry(&path))
                .and_then(EntryV2::content)
            {
                apply_fills(&path, &content, &mut patch, fill);
            }
            next.set_readiness_override(path, patch)?;
        }
        Ok(next)
    }

    pub fn disk_state(&self, rel_path: &RelPath) -> DiskState {
        match self.entry(rel_path) {
            Some(EntryV2::Regular { content, size, .. }) => DiskState::Present {
                content: *content,
                size: *size,
            },
            _ => DiskState::Absent,
        }
    }
}

fn same_entry_identity(inherited: &EntryV2, entry: &EntryV2) -> bool {
    match (inherited, entry) {
        (
            EntryV2::Regular {
                mode: a_mode,
                content: a_content,
                size: a_size,
                git_oid: a_oid,
                resolution_input: a_input,
                ..
            },
            EntryV2::Regular {
                mode: b_mode,
                content: b_content,
                size: b_size,
                git_oid: b_oid,
                resolution_input: b_input,
                ..
            },
        ) => {
            (a_mode, a_content, a_size, a_oid, a_input)
                == (b_mode, b_content, b_size, b_oid, b_input)
        }
        _ => inherited == entry,
    }
}

fn delta_owns_plane(inherited: Option<&EntryV2>, entry: &EntryV2, plane: FamilyPlane) -> bool {
    // Clearing applicability needs a full entry because an absent override
    // means inheritance. That representation must not claim unchanged planes.
    inherited.is_none_or(|base| {
        !same_entry_identity(base, entry) || base.plane_state(plane) != entry.plane_state(plane)
    })
}

fn apply_plane_fill(
    path: &RelPath,
    content: &ContentHash,
    plane: FamilyPlane,
    state: &mut Option<PlaneState>,
    fill: &super::readiness::FillMap,
) {
    if matches!(state, Some(PlaneState::Pending { .. })) {
        if let Some(key) = fill.get(path, content, plane) {
            *state = Some(PlaneState::ready(key));
        }
    }
}

fn apply_fills(
    path: &RelPath,
    content: &ContentHash,
    planes: &mut EntryPlanes,
    fill: &super::readiness::FillMap,
) {
    for plane in FamilyPlane::ALL {
        apply_plane_fill(path, content, plane, planes.get_mut(plane), fill);
    }
}

/// Plane data attached to a live entry, such as the trigram postings computed
/// when the entry was applied. Planes downcast their own attachment.
pub type PlaneAttachment = Arc<dyn Any + Send + Sync>;

#[derive(Clone)]
pub struct LiveEntry {
    pub disk: DiskState,
    /// The watcher or intent sequence that produced this state.
    pub seq: u64,
    pub attachments: BTreeMap<FamilyPlane, PlaneAttachment>,
    pub reconciliation: super::readiness::ReconciledDelta,
}

impl fmt::Debug for LiveEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveEntry")
            .field("disk", &self.disk)
            .field("seq", &self.seq)
            .field("planes", &self.attachments.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl LiveEntry {
    pub fn new(disk: DiskState, seq: u64) -> Self {
        Self {
            disk,
            seq,
            attachments: BTreeMap::new(),
            reconciliation: Default::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRecord {
    pub version: u64,
    pub rel_path: RelPath,
    pub disk: DiskState,
}

/// Whether the watcher can vouch for disk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatcherState {
    Healthy,
    Overflowed,
    Stopped,
    /// A strict reconcile is running; the index must not prune.
    Reconciling,
}

/// Supplies plane attachments for an entry the switch creates because disk
/// went back to the old generation's bytes. The old generation is pinned for
/// the whole switch, so its keys are protected and nothing is recomputed.
pub type CarryFromGeneration<'a> = dyn Fn(&RelPath, &ComposedGeneration) -> LiveEntry + 'a;

type Entries = BTreeMap<RelPath, LiveEntry>;

/// Checkout-specific readiness overrides for the recorded content hash.
/// Equal disk bytes do not erase the override; changed bytes cannot use it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanePatch {
    pub content: ContentHash,
    pub planes: EntryPlanes,
}

/// The per-root live delta. Owned by the root actor; callers serialize access
/// with the root's index write lock.
#[derive(Debug)]
pub struct LiveDelta {
    base: Arc<ComposedGeneration>,
    epoch: u64,
    version: u64,
    entries: Arc<Entries>,
    readiness: Arc<BTreeMap<RelPath, PlanePatch>>,
    journal: Vec<JournalRecord>,
    observation_journal: BTreeMap<RelPath, u64>,
    readiness_journal: BTreeMap<RelPath, u64>,
    intent: Arc<BTreeMap<RelPath, u64>>,
    intent_version: u64,
    watcher: WatcherState,
    root_ignore_files: Option<Arc<RootIgnoreFiles>>,
}

/// The disk state of each root ignore file (see
/// `live_delta::IGNORE_FILE_NAMES`), present or absent, as read just
/// before the walk that decided a snapshot's membership.
pub type RootIgnoreFiles = BTreeMap<RelPath, DiskState>;

/// The delta as a builder read it at version `v`.
#[derive(Clone, Debug)]
pub struct DeltaCut {
    pub base: Arc<ComposedGeneration>,
    pub epoch: u64,
    pub version: u64,
    entries: Arc<Entries>,
    readiness: Arc<BTreeMap<RelPath, PlanePatch>>,
}

impl DeltaCut {
    pub fn keys(&self) -> impl Iterator<Item = &RelPath> {
        self.entries.keys()
    }
}

/// A successor delta derived against a new generation, waiting for its swap.
#[derive(Debug)]
pub struct SuccessorDraft {
    expected_base: Arc<ComposedGeneration>,
    successor: Arc<ComposedGeneration>,
    epoch: u64,
    cut_version: u64,
    entries: Entries,
    readiness: BTreeMap<RelPath, PlanePatch>,
}

impl SuccessorDraft {
    pub fn entries(&self) -> impl Iterator<Item = (&RelPath, &LiveEntry)> {
        self.entries.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SwitchError {
    /// The delta was reconciled (overflow, rescan, membership) after the cut;
    /// rebuild the successor from the reconciled delta.
    EpochChanged { cut: u64, current: u64 },
    /// The draft was derived against a different base than the delta's.
    BaseChanged,
}

impl LiveDelta {
    pub fn new(base: Arc<ComposedGeneration>) -> Self {
        Self {
            base,
            epoch: 0,
            version: 0,
            entries: Arc::new(Entries::new()),
            readiness: Arc::new(BTreeMap::new()),
            journal: Vec::new(),
            observation_journal: BTreeMap::new(),
            readiness_journal: BTreeMap::new(),
            intent: Arc::new(BTreeMap::new()),
            intent_version: 0,
            watcher: WatcherState::Healthy,
            root_ignore_files: None,
        }
    }

    /// Records the root ignore files the latest full membership walk ran
    /// under. The record covers every root ignore file, including one that
    /// ignores itself and so is never a member: it still decides membership.
    pub fn record_root_ignore_files(&mut self, files: Option<RootIgnoreFiles>) {
        self.root_ignore_files = files.map(Arc::new);
    }

    pub fn base(&self) -> &Arc<ComposedGeneration> {
        &self.base
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn watcher(&self) -> WatcherState {
        self.watcher
    }

    pub fn set_watcher(&mut self, watcher: WatcherState) {
        if self.watcher != watcher {
            self.version += 1;
            if watcher != WatcherState::Healthy {
                self.bump_epoch();
            }
        }
        self.watcher = watcher;
    }

    pub fn entry(&self, rel_path: &RelPath) -> Option<&LiveEntry> {
        self.entries.get(rel_path)
    }

    pub fn set_readiness(&mut self, path: RelPath, patch: PlanePatch) {
        self.version += 1;
        self.readiness_journal.insert(path.clone(), self.version);
        Arc::make_mut(&mut self.readiness).insert(path, patch);
    }

    pub fn readiness(&self) -> &BTreeMap<RelPath, PlanePatch> {
        &self.readiness
    }

    pub fn journal(&self) -> &[JournalRecord] {
        &self.journal
    }

    /// Records an observed disk state. The entry is kept only while it
    /// differs from the base generation. Replay needs only the last changed
    /// state or attachment per path, not repeated observations of one entry.
    pub fn apply(&mut self, rel_path: RelPath, entry: LiveEntry) -> u64 {
        let previous = self
            .entries
            .get(&rel_path)
            .map_or_else(|| self.base.disk_state(&rel_path), |entry| entry.disk);
        self.version += 1;
        self.observation_journal
            .insert(rel_path.clone(), self.version);
        let attachments_changed = self.entries.get(&rel_path).is_some_and(|previous| {
            previous.attachments.len() != entry.attachments.len()
                || previous.attachments.iter().any(|(plane, attachment)| {
                    entry
                        .attachments
                        .get(plane)
                        .is_none_or(|new| !Arc::ptr_eq(attachment, new))
                })
        });
        if previous != entry.disk || attachments_changed {
            self.journal.retain(|record| record.rel_path != rel_path);
            self.journal.push(JournalRecord {
                version: self.version,
                rel_path: rel_path.clone(),
                disk: entry.disk,
            });
        }
        let entries = Arc::make_mut(&mut self.entries);
        if entry.disk == self.base.disk_state(&rel_path) {
            entries.remove(&rel_path);
        } else {
            entries.insert(rel_path, entry);
        }
        self.version
    }

    /// Records that an AFT write to `rel_path` was acknowledged and not yet
    /// applied. Queries match such paths on their current bytes.
    pub fn record_intent(&mut self, rel_path: RelPath) -> u64 {
        self.intent_version += 1;
        self.version += 1;
        // An intent is not a disk observation. Replaying its old disk value
        // would overwrite bytes a later strict walk actually observed.
        Arc::make_mut(&mut self.intent).insert(rel_path, self.intent_version);
        self.intent_version
    }

    /// Clears the intent for `rel_path` once an apply at or after
    /// `intent_version` has landed.
    pub fn resolve_intent(&mut self, rel_path: &RelPath, intent_version: u64) {
        let intent = Arc::make_mut(&mut self.intent);
        if intent
            .get(rel_path)
            .is_some_and(|recorded| *recorded <= intent_version)
        {
            intent.remove(rel_path);
            self.intent_version += 1;
        }
    }

    /// Marks the delta as reconciled from scratch (overflow, rescan, bind,
    /// membership change). Successors built from an older cut are discarded.
    pub fn bump_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    /// The delta as a builder reads it now.
    pub fn cut(&self) -> DeltaCut {
        DeltaCut {
            base: Arc::clone(&self.base),
            epoch: self.epoch,
            version: self.version,
            entries: Arc::clone(&self.entries),
            readiness: Arc::clone(&self.readiness),
        }
    }

    /// A query snapshot: `Arc` clones only.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            inner: Arc::new(SnapshotInner {
                generation: Arc::clone(&self.base),
                delta_version: self.version,
                intent_version: self.intent_version,
                epoch: self.epoch,
                entries: Arc::clone(&self.entries),
                readiness: Arc::clone(&self.readiness),
                intent: Arc::clone(&self.intent),
                watcher: self.watcher,
                root_ignore_files: self.root_ignore_files.clone(),
                install_cut: None,
                reconciled_epoch: None,
                publication_base: None,
                readers: None,
                needs_reconcile: self.base.name() == "empty"
                    || !self.intent.is_empty()
                    || self.watcher != WatcherState::Healthy
                    || self.entries.values().any(|entry| {
                        entry.reconciliation != super::readiness::ReconciledDelta::Reconciled
                    }),
            }),
        }
    }

    /// Replays the journal after the draft's cut against the delta as it is
    /// now, then swaps in the successor. Callers hold the root's index write
    /// lock, the same lock every apply takes, so nothing lands in between.
    pub fn replay_and_swap(
        &mut self,
        mut draft: SuccessorDraft,
        carry: &CarryFromGeneration<'_>,
    ) -> Result<(), SwitchError> {
        if draft.epoch != self.epoch {
            return Err(SwitchError::EpochChanged {
                cut: draft.epoch,
                current: self.epoch,
            });
        }
        if !Arc::ptr_eq(&draft.expected_base, &self.base) {
            return Err(SwitchError::BaseChanged);
        }
        let mut replay = self
            .observation_journal
            .iter()
            .filter(|(_, version)| **version > draft.cut_version)
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        replay.extend(
            self.readiness_journal
                .iter()
                .filter(|(_, version)| **version > draft.cut_version)
                .map(|(path, _)| path.clone()),
        );
        for path in replay {
            let observed = self.entries.get(&path).cloned();
            rederive_path(
                &path,
                observed,
                &self.base,
                &draft.successor,
                carry,
                &mut draft.entries,
            );
            rederive_readiness(
                &path,
                self.readiness.get(&path),
                &draft.successor,
                &mut draft.readiness,
            );
        }
        self.base = draft.successor;
        self.entries = Arc::new(draft.entries);
        self.readiness = Arc::new(draft.readiness);
        // One successor is in flight at a time per root, so nothing older than
        // the swap is needed again.
        self.journal.clear();
        self.observation_journal.clear();
        self.readiness_journal.clear();
        Ok(())
    }

    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        let inner = &snapshot.inner;
        Self {
            base: inner.generation.clone(),
            epoch: inner.epoch,
            version: inner.delta_version,
            entries: inner.entries.clone(),
            readiness: inner.readiness.clone(),
            journal: Vec::new(),
            observation_journal: BTreeMap::new(),
            readiness_journal: BTreeMap::new(),
            intent: inner.intent.clone(),
            intent_version: inner.intent_version,
            watcher: inner.watcher,
            root_ignore_files: inner.root_ignore_files.clone(),
        }
    }

    /// Replay paths delivered since a strict walk began. A path's latest intent
    /// remains pending even if it has not yet supplied a stable content read.
    pub fn replay_onto(&self, snapshot: &Snapshot, cut: &InstallCut) -> Result<Self, SwitchError> {
        if self.epoch != cut.epoch {
            return Err(SwitchError::EpochChanged {
                cut: cut.epoch,
                current: self.epoch,
            });
        }
        if !Arc::ptr_eq(&self.base, &cut.resident) {
            return Err(SwitchError::BaseChanged);
        }
        let mut next = Self::from_snapshot(snapshot);
        for (path, _) in self
            .observation_journal
            .iter()
            .filter(|(_, version)| **version > cut.version)
        {
            let entry = self
                .entries
                .get(path)
                .cloned()
                .unwrap_or_else(|| carry_disk_state_only(path, &self.base));
            next.apply(path.clone(), entry);
        }
        for (path, version) in &self.readiness_journal {
            if *version > cut.version {
                if let Some(patch) = self.readiness.get(path) {
                    next.set_readiness(path.clone(), patch.clone());
                }
            }
        }
        let intent = Arc::make_mut(&mut next.intent);
        intent.retain(|path, _| self.intent.contains_key(path));
        intent.extend(
            self.intent
                .iter()
                .filter(|(_, version)| **version > cut.intent_version)
                .map(|(path, version)| (path.clone(), *version)),
        );
        next.intent_version = self.intent_version;
        next.epoch = self.epoch;
        next.version = self.version;
        next.watcher = self.watcher;
        next.journal.clear();
        next.observation_journal.clear();
        next.readiness_journal.clear();
        Ok(next)
    }
}

impl LiveEntries for LiveDelta {
    fn live_states(&self) -> Box<dyn Iterator<Item = (&RelPath, &DiskState)> + '_> {
        Box::new(self.entries.iter().map(|(path, entry)| (path, &entry.disk)))
    }

    fn live_state(&self, rel_path: &RelPath) -> Option<&DiskState> {
        self.entries.get(rel_path).map(|entry| &entry.disk)
    }
    fn readiness_patch(&self, path: &RelPath, plane: FamilyPlane) -> Option<&PlaneState> {
        let patch = self.readiness.get(path)?;
        let disk = self
            .entries
            .get(path)
            .map_or_else(|| self.base.disk_state(path), |entry| entry.disk);
        if matches!(disk, DiskState::Present { content, .. } if content == patch.content) {
            patch.planes.get(plane)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;
    use crate::views::contracts::{PlaneAdapter, PlaneError, ViewAccess};
    use crate::views::manifest_v2::{ManifestHeader, Producers};

    fn delta() -> LiveDelta {
        LiveDelta::new(Arc::new(ComposedGeneration::new(
            "base",
            ManifestV2::new(ManifestHeader {
                producers: Producers {
                    trigram: "test".into(),
                    semantic: None,
                    callgraph: "test".into(),
                },
                head_tree: None,
                ignore_fingerprint: None,
                segment: None,
            }),
            None,
        )))
    }

    struct LeaseAdapter {
        resident: std::sync::atomic::AtomicBool,
        releases: std::sync::atomic::AtomicUsize,
    }

    struct ReaderPause {
        entered: std::sync::mpsc::Sender<()>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl ReaderPause {
        fn wait(&self) {
            self.entered.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
        }
    }

    struct PausedLeaseAdapter {
        inner: LeaseAdapter,
        open: Option<ReaderPause>,
        retire: Option<ReaderPause>,
    }

    impl PlaneAdapter for PausedLeaseAdapter {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Trigram
        }
        fn producer(&self) -> String {
            "test".into()
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            true
        }
        fn open_generation(
            &self,
            access: &ViewAccess,
            generation: &Arc<ComposedGeneration>,
        ) -> Result<(), PlaneError> {
            if let Some(pause) = &self.open {
                pause.wait();
            }
            self.inner.open_generation(access, generation)
        }
        fn release_generation(&self, access: &ViewAccess, generation: &str) {
            if let Some(pause) = &self.retire {
                pause.wait();
            }
            self.inner.release_generation(access, generation);
        }
        fn readiness(
            &self,
            access: &ViewAccess,
            snapshot: &Snapshot,
        ) -> super::super::readiness::PlaneReadiness {
            self.inner.readiness(access, snapshot)
        }
    }

    #[test]
    fn reader_open_io_releases_the_global_cache_lock() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "reader-io").unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let access = ViewAccess::Owner(owner);
        let (entered, arrived) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        let pause = ReaderPause {
            entered,
            resume: std::sync::Mutex::new(released),
        };
        let blocked: Arc<dyn PlaneAdapter> = Arc::new(PausedLeaseAdapter {
            inner: LeaseAdapter {
                resident: false.into(),
                releases: 0.into(),
            },
            open: Some(pause),
            retire: None,
        });
        let generation = delta().base().clone();
        let first_access = access.clone();
        let first = std::thread::spawn(move || {
            PlaneReaders::open(&first_access, generation, &[blocked]).unwrap()
        });
        arrived
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let fast: Arc<dyn PlaneAdapter> = Arc::new(LeaseAdapter {
            resident: false.into(),
            releases: 0.into(),
        });
        let (done, completed) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            done.send(PlaneReaders::open(&access, delta().base().clone(), &[fast]).unwrap())
                .unwrap()
        });
        let progressed = completed.recv_timeout(std::time::Duration::from_secs(2));
        resume.send(()).unwrap();
        let held = first.join().unwrap();
        second.join().unwrap();
        assert!(
            progressed.is_ok(),
            "an unrelated reader waited for adapter open I/O"
        );
        drop(held);
    }

    #[test]
    fn reader_retirement_io_releases_the_global_cache_lock() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "reader-retire").unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let access = ViewAccess::Owner(owner);
        let (entered, arrived) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        let blocked: Arc<dyn PlaneAdapter> = Arc::new(PausedLeaseAdapter {
            inner: LeaseAdapter {
                resident: false.into(),
                releases: 0.into(),
            },
            open: None,
            retire: Some(ReaderPause {
                entered,
                resume: std::sync::Mutex::new(released),
            }),
        });
        let held = PlaneReaders::open(&access, delta().base().clone(), &[blocked]).unwrap();
        let first = std::thread::spawn(move || drop(held));
        arrived
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let fast: Arc<dyn PlaneAdapter> = Arc::new(LeaseAdapter {
            resident: false.into(),
            releases: 0.into(),
        });
        let (done, completed) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            done.send(PlaneReaders::open(&access, delta().base().clone(), &[fast]).unwrap())
                .unwrap()
        });
        let progressed = completed.recv_timeout(std::time::Duration::from_secs(2));
        resume.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert!(
            progressed.is_ok(),
            "an unrelated reader waited for adapter retirement I/O"
        );
    }

    #[test]
    fn concurrent_reader_opens_share_one_per_key_lease() {
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "single-reader").unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let access = ViewAccess::Owner(owner);
        let (entered, arrived) = std::sync::mpsc::channel();
        let (resume, released) = std::sync::mpsc::channel();
        let adapter: Arc<dyn PlaneAdapter> = Arc::new(PausedLeaseAdapter {
            inner: LeaseAdapter {
                resident: false.into(),
                releases: 0.into(),
            },
            open: Some(ReaderPause {
                entered,
                resume: std::sync::Mutex::new(released),
            }),
            retire: None,
        });
        let first_access = access.clone();
        let first_adapter = adapter.clone();
        let first = std::thread::spawn(move || {
            PlaneReaders::open(&first_access, delta().base().clone(), &[first_adapter]).unwrap()
        });
        arrived
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let (done, completed) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            done.send(PlaneReaders::open(&access, delta().base().clone(), &[adapter]).unwrap())
                .unwrap()
        });
        assert!(completed
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        resume.send(()).unwrap();
        let first = first.join().unwrap();
        let second_lease = completed
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        second.join().unwrap();
        assert!(Arc::ptr_eq(&first.adapters[0], &second_lease.adapters[0]));
    }

    impl super::super::contracts::PlaneAdapter for LeaseAdapter {
        fn plane(&self) -> FamilyPlane {
            FamilyPlane::Trigram
        }
        fn producer(&self) -> String {
            "test".into()
        }
        fn applies_to(&self, _: &RelPath) -> bool {
            true
        }
        fn open_generation(
            &self,
            _: &super::super::contracts::ViewAccess,
            _: &Arc<ComposedGeneration>,
        ) -> Result<(), super::super::contracts::PlaneError> {
            self.resident
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn release_generation(&self, _: &super::super::contracts::ViewAccess, _: &str) {
            self.releases
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.resident
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn readiness(
            &self,
            _: &super::super::contracts::ViewAccess,
            _: &Snapshot,
        ) -> super::super::readiness::PlaneReadiness {
            super::super::readiness::PlaneReadiness::Ready {
                pending: 0,
                failed: 0,
            }
        }
    }

    #[test]
    fn retiring_reader_lease_cannot_remove_its_replacement() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let registry =
            super::super::registry::FamilyRegistry::open(storage.path(), "leases").unwrap();
        let owner = registry
            .register_view_with_storage(storage.path(), "worker", root.path())
            .unwrap();
        let access = super::super::contracts::ViewAccess::Owner(owner);
        let concrete = Arc::new(LeaseAdapter {
            resident: false.into(),
            releases: 0.into(),
        });
        let adapter: Arc<dyn super::super::contracts::PlaneAdapter> = concrete.clone();
        let generation = delta().base().clone();
        let old = PlaneReaders::open(&access, generation.clone(), std::slice::from_ref(&adapter))
            .unwrap();
        let key = old.adapters[0].key.clone();
        let weak = Arc::downgrade(&old.adapters[0]);
        let slot = old.adapters[0].slot.clone();
        let mut state = slot.state.lock().unwrap();
        let (done, completed) = std::sync::mpsc::channel();
        let retiring = std::thread::spawn(move || {
            drop(old);
            done.send(()).unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while weak.strong_count() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            weak.strong_count(),
            0,
            "the old destructor must be waiting for its slot lock"
        );
        adapter.open_generation(&access, &generation).unwrap();
        let replacement = Arc::new(AdapterLease {
            key: key.clone(),
            slot: slot.clone(),
            token: next_reader_token(),
            adapter,
            access,
            resource: None,
            generation,
        });
        *state = ReaderSlotState::Live {
            token: replacement.token,
            lease: Arc::downgrade(&replacement),
        };
        drop(state);
        completed
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        retiring.join().unwrap();
        assert_eq!(concrete.releases.load(Ordering::SeqCst), 0);
        assert!(concrete.resident.load(Ordering::SeqCst));
        drop(replacement);
        assert_eq!(concrete.releases.load(Ordering::SeqCst), 1);
        assert!(!concrete.resident.load(Ordering::SeqCst));
    }

    #[test]
    fn locked_install_replays_observed_bytes_deletions_attachments_and_readiness() {
        let mut live = delta();
        let cut = InstallCut {
            seq: 0,
            resident: live.base().clone(),
            epoch: live.epoch(),
            version: live.version(),
            intent_version: 0,
            verification: None,
        };
        let changed = RelPath::new(b"changed.rs".to_vec()).unwrap();
        let transient = RelPath::new(b"transient.rs".to_vec()).unwrap();
        let mut manifest = live.base().flatten().unwrap();
        for path in [&changed, &transient] {
            manifest
                .insert(
                    path.clone(),
                    EntryV2::regular(
                        ContentHash::of(b"builder bytes"),
                        13,
                        EntryPlanes::default(),
                    ),
                )
                .unwrap();
        }
        let draft =
            LiveDelta::new(Arc::new(ComposedGeneration::new("draft", manifest, None))).snapshot();
        let disk = DiskState::of_bytes(b"latest delivered bytes");
        let mut latest = LiveEntry::new(disk, 2);
        latest
            .attachments
            .insert(FamilyPlane::Trigram, Arc::new(42_usize));
        live.apply(changed.clone(), latest);
        live.set_readiness(
            changed.clone(),
            PlanePatch {
                content: ContentHash::of(b"latest delivered bytes"),
                planes: EntryPlanes {
                    semantic: Some(PlaneState::pending("dependency changed")),
                    ..Default::default()
                },
            },
        );
        live.apply(
            transient.clone(),
            LiveEntry::new(DiskState::of_bytes(b"temporary"), 3),
        );
        live.apply(transient.clone(), LiveEntry::new(DiskState::Absent, 4));
        let next = live.replay_onto(&draft, &cut).unwrap();
        assert_eq!(next.snapshot().disk_state(&changed), disk);
        assert_eq!(next.snapshot().disk_state(&transient), DiskState::Absent);
        assert_eq!(
            next.entry(&changed).unwrap().attachments[&FamilyPlane::Trigram]
                .downcast_ref::<usize>(),
            Some(&42)
        );
        assert_eq!(next.readiness()[&changed], live.readiness()[&changed]);
        assert_eq!(next.version(), live.version());
    }

    #[test]
    fn intent_and_readiness_replay_do_not_undo_strictly_observed_bytes() {
        let mut live = delta();
        let file = RelPath::new(b"file.rs".to_vec()).unwrap();
        live.apply(file.clone(), LiveEntry::new(DiskState::of_bytes(b"A"), 1));
        let cut = InstallCut {
            seq: 0,
            resident: live.base().clone(),
            epoch: live.epoch(),
            version: live.version(),
            intent_version: 0,
            verification: None,
        };
        let mut walked = LiveDelta::new(live.base().clone());
        walked.apply(file.clone(), LiveEntry::new(DiskState::of_bytes(b"B"), 2));
        live.record_intent(file.clone());
        live.set_readiness(
            file.clone(),
            PlanePatch {
                content: ContentHash::of(b"A"),
                planes: EntryPlanes {
                    trigram: Some(super::super::readiness::PlaneState::pending("old A fill")),
                    ..Default::default()
                },
            },
        );
        let replayed = live
            .replay_onto(&walked.snapshot(), &cut)
            .unwrap()
            .snapshot();
        assert_eq!(replayed.disk_state(&file), DiskState::of_bytes(b"B"));
        assert!(replayed.pending_intent().any(|path| path == &file));
        assert_eq!(replayed.plane_readiness(&file, FamilyPlane::Trigram), None);
    }

    #[test]
    fn repeated_observation_after_a_cut_still_replays_over_a_walk() {
        let mut live = delta();
        let file = RelPath::new(b"file.rs".to_vec()).unwrap();
        live.apply(file.clone(), LiveEntry::new(DiskState::of_bytes(b"A"), 1));
        let cut = InstallCut {
            seq: 0,
            resident: live.base().clone(),
            epoch: live.epoch(),
            version: live.version(),
            intent_version: 0,
            verification: None,
        };
        let mut walked = LiveDelta::new(live.base().clone());
        walked.apply(file.clone(), LiveEntry::new(DiskState::of_bytes(b"B"), 2));
        live.apply(file.clone(), LiveEntry::new(DiskState::of_bytes(b"A"), 3));
        let replayed = live
            .replay_onto(&walked.snapshot(), &cut)
            .unwrap()
            .snapshot();
        assert_eq!(replayed.disk_state(&file), DiskState::of_bytes(b"A"));
    }

    #[test]
    fn draft_refuses_a_different_base_at_the_same_epoch() {
        let mut delta = delta();
        let cut = delta.cut();
        let stale = derive_successor(&cut, cut.base.clone(), &carry_disk_state_only);
        let other = Arc::new(ComposedGeneration::new(
            "other",
            cut.base.flatten().unwrap(),
            None,
        ));
        delta
            .replay_and_swap(
                derive_successor(&cut, other, &carry_disk_state_only),
                &carry_disk_state_only,
            )
            .unwrap();
        assert_eq!(
            delta.replay_and_swap(stale, &carry_disk_state_only),
            Err(SwitchError::BaseChanged)
        );
        assert_eq!(delta.base().name(), "other");
    }

    #[test]
    fn unchanged_observations_do_not_grow_the_journal() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        for _ in 0..128 {
            delta.apply(path.clone(), LiveEntry::new(DiskState::Absent, 0));
        }
        assert!(delta.journal().is_empty());
        let disk = DiskState::of_bytes(b"a");
        delta.apply(path.clone(), LiveEntry::new(disk, 1));
        for _ in 0..128 {
            delta.apply(path.clone(), LiveEntry::new(disk, 1));
        }
        assert_eq!(delta.journal().len(), 1);
    }

    #[test]
    fn repeated_path_changes_keep_one_replay_record() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let successor = delta.base().clone();
        let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
        for sequence in 0..128 {
            delta.apply(
                path.clone(),
                LiveEntry::new(
                    DiskState::of_bytes(sequence.to_string().as_bytes()),
                    sequence,
                ),
            );
        }
        assert_eq!(delta.journal().len(), 1);
        let expected = delta.snapshot().disk_state(&path);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(delta.snapshot().disk_state(&path), expected);
    }

    #[test]
    fn same_content_attachment_updates_are_replayed() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let disk = DiskState::of_bytes(b"a");
        let mut old = LiveEntry::new(disk, 1);
        old.attachments
            .insert(FamilyPlane::Trigram, Arc::new(1_usize));
        delta.apply(path.clone(), old);
        let draft = derive_successor(&delta.cut(), delta.base().clone(), &carry_disk_state_only);
        let mut new = LiveEntry::new(disk, 2);
        new.attachments
            .insert(FamilyPlane::Trigram, Arc::new(2_usize));
        delta.apply(path.clone(), new);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(
            delta.entry(&path).unwrap().attachments[&FamilyPlane::Trigram].downcast_ref::<usize>(),
            Some(&2)
        );
    }

    #[test]
    fn watcher_and_intent_resolution_advance_the_revision_fence() {
        let mut delta = delta();
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let before = delta.version() + delta.epoch() + delta.snapshot().intent_version();
        delta.set_watcher(WatcherState::Overflowed);
        assert!(delta.version() + delta.epoch() + delta.snapshot().intent_version() > before);
        let intent = delta.record_intent(path.clone());
        let before = delta.version() + delta.epoch() + delta.snapshot().intent_version();
        delta.resolve_intent(&path, intent);
        assert!(delta.version() + delta.epoch() + delta.snapshot().intent_version() > before);
        assert!(delta.snapshot().pending_intent().next().is_none());
    }

    #[test]
    fn retained_cut_installs_after_edit_traffic_pauses() {
        let root = tempfile::tempdir().unwrap();
        let delta = Arc::new(std::sync::Mutex::new(delta()));
        let path = RelPath::new(b"a.rs".to_vec()).unwrap();
        let successor = delta.lock().unwrap().base().clone();
        let (edit_tx, edit_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_delta = delta.clone();
        let worker_root = root.path().to_path_buf();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            for sequence in edit_rx {
                std::fs::write(worker_root.join("a.rs"), format!("edit {sequence}")).unwrap();
                crate::views::live_delta::apply_event(
                    &mut worker_delta.lock().unwrap(),
                    &worker_root,
                    &worker_path,
                    &crate::blob_store::v2::TrigramPolicy {
                        max_file_size: 1 << 20,
                    },
                )
                .unwrap();
                done_tx.send(()).unwrap();
            }
        });
        let mut epoch_changes = 0;
        for sequence in 0..64 {
            let draft = derive_successor(
                &delta.lock().unwrap().cut(),
                successor.clone(),
                &carry_disk_state_only,
            );
            edit_tx.send(sequence).unwrap();
            done_rx.recv().unwrap();
            match delta
                .lock()
                .unwrap()
                .replay_and_swap(draft, &carry_disk_state_only)
            {
                Err(SwitchError::EpochChanged { .. }) => epoch_changes += 1,
                other => panic!("unexpected retained-cut result: {other:?}"),
            }
        }
        drop(edit_tx);
        worker.join().unwrap();
        let mut delta = delta.lock().unwrap();
        let draft = derive_successor(&delta.cut(), successor, &carry_disk_state_only);
        delta
            .replay_and_swap(draft, &carry_disk_state_only)
            .unwrap();
        assert_eq!(epoch_changes, 64);
        eprintln!("retained-cut attempts: 64 EpochChanged, 0 obsolete-revision, 1 successful install after edits paused");
    }
}

/// The paths a switch must re-derive: the cut's live keys plus every path
/// whose entry differs between the two generations. Paths outside this set
/// are equal in both generations and had no live entry, so disk equals the
/// successor there.
pub fn rebase_paths(cut: &DeltaCut, successor: &ComposedGeneration) -> BTreeSet<RelPath> {
    let mut paths = cut.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(cut.readiness.keys().cloned());
    paths.extend(cut.base.diff_paths(successor));
    paths
}

/// Derives the successor delta of `cut` against `successor`.
pub fn derive_successor(
    cut: &DeltaCut,
    successor: Arc<ComposedGeneration>,
    carry: &CarryFromGeneration<'_>,
) -> SuccessorDraft {
    let mut entries = Entries::new();
    let mut readiness = BTreeMap::new();
    for path in rebase_paths(cut, &successor) {
        let observed = cut.entries.get(&path).cloned();
        rederive_path(&path, observed, &cut.base, &successor, carry, &mut entries);
        rederive_readiness(&path, cut.readiness.get(&path), &successor, &mut readiness);
    }
    SuccessorDraft {
        expected_base: cut.base.clone(),
        successor,
        epoch: cut.epoch,
        cut_version: cut.version,
        entries,
        readiness,
    }
}

fn rederive_readiness(
    path: &RelPath,
    patch: Option<&PlanePatch>,
    successor: &ComposedGeneration,
    readiness: &mut BTreeMap<RelPath, PlanePatch>,
) {
    match patch {
        Some(patch)
            if successor.entry(path).and_then(EntryV2::content) != Some(patch.content)
                || FamilyPlane::ALL.into_iter().any(|plane| {
                    patch
                        .planes
                        .get(plane)
                        .is_some_and(|state| successor.plane_readiness(path, plane) != Some(state))
                }) =>
        {
            readiness.insert(path.clone(), patch.clone());
        }
        _ => {
            readiness.remove(path);
        }
    }
}

fn rederive_path(
    path: &RelPath,
    observed: Option<LiveEntry>,
    old: &Arc<ComposedGeneration>,
    successor: &Arc<ComposedGeneration>,
    carry: &CarryFromGeneration<'_>,
    entries: &mut Entries,
) {
    let entry = match observed {
        Some(entry) => entry,
        // No live entry means disk equalled the old generation, exactly.
        None => carry(path, old),
    };
    if entry.disk == successor.disk_state(path) {
        entries.remove(path);
    } else {
        entries.insert(path.clone(), entry);
    }
}

/// A carry function that attaches nothing: the entry records only the old
/// generation's disk state.
pub fn carry_disk_state_only(path: &RelPath, generation: &ComposedGeneration) -> LiveEntry {
    LiveEntry::new(generation.disk_state(path), 0)
}

#[derive(Clone, Debug)]
struct SnapshotInner {
    needs_reconcile: bool,
    reconciled_epoch: Option<u64>,
    publication_base: Option<Option<String>>,
    generation: Arc<ComposedGeneration>,
    delta_version: u64,
    intent_version: u64,
    epoch: u64,
    entries: Arc<Entries>,
    readiness: Arc<BTreeMap<RelPath, PlanePatch>>,
    intent: Arc<BTreeMap<RelPath, u64>>,
    watcher: WatcherState,
    root_ignore_files: Option<Arc<RootIgnoreFiles>>,
    install_cut: Option<InstallCut>,
    readers: Option<Arc<PlaneReaders>>,
}

#[derive(Clone, Debug)]
pub struct InstallCut {
    pub seq: u64,
    pub resident: Arc<ComposedGeneration>,
    pub epoch: u64,
    pub version: u64,
    pub intent_version: u64,
    pub(crate) verification: Option<Arc<super::verification::BindGuard>>,
}

/// A lease on resident adapters. Query snapshots retain this lease, so removing
/// the driver's old bundle cannot invalidate a lazy open by an old query.
pub struct PlaneReaders {
    // Bundle-owned state drops before the shared reader and the generation Arc
    // that retains the residency pins.
    resources: Vec<Option<PlaneAttachment>>,
    adapters: Vec<Arc<AdapterLease>>,
    generation: Arc<ComposedGeneration>,
}

struct AdapterLease {
    key: ReaderCacheKey,
    slot: Arc<ReaderSlot>,
    token: u64,
    adapter: Arc<dyn super::contracts::PlaneAdapter>,
    access: super::contracts::ViewAccess,
    resource: Option<PlaneAttachment>,
    generation: Arc<ComposedGeneration>,
}

impl Drop for AdapterLease {
    fn drop(&mut self) {
        let owns_slot = {
            let mut state = self
                .slot
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let owns = matches!(&*state,
                ReaderSlotState::Live { token, .. } | ReaderSlotState::Opening(token)
                if *token == self.token);
            if owns {
                *state = ReaderSlotState::Retiring(self.token);
            }
            owns
        };
        // Neither the global map nor the per-key slot is locked during teardown.
        // The generation Arc retains residency pins until the lease fields drop.
        if owns_slot {
            let _retiring = ReaderTransition {
                slot: self.slot.clone(),
                token: self.token,
                armed: true,
            };
            drop(self.resource.take());
            self.adapter
                .release_generation(&self.access, &self.generation.resident_key());
        } else {
            drop(self.resource.take());
        }
    }
}

type ReaderCacheKey = (usize, String, String, String);
type ReaderCache = std::sync::Mutex<BTreeMap<ReaderCacheKey, std::sync::Weak<ReaderSlot>>>;

enum ReaderSlotState {
    Vacant,
    Opening(u64),
    Live {
        token: u64,
        lease: std::sync::Weak<AdapterLease>,
    },
    Retiring(u64),
}

struct ReaderSlot {
    state: std::sync::Mutex<ReaderSlotState>,
    changed: std::sync::Condvar,
}

impl ReaderSlot {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(ReaderSlotState::Vacant),
            changed: std::sync::Condvar::new(),
        }
    }
}

struct ReaderTransition {
    slot: Arc<ReaderSlot>,
    token: u64,
    armed: bool,
}

impl Drop for ReaderTransition {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(&*state, ReaderSlotState::Opening(token) | ReaderSlotState::Retiring(token) if *token == self.token)
        {
            *state = ReaderSlotState::Vacant;
        }
        drop(state);
        self.slot.changed.notify_all();
    }
}

fn next_reader_token() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn reader_cache() -> &'static ReaderCache {
    static CACHE: std::sync::OnceLock<ReaderCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

impl fmt::Debug for PlaneReaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaneReaders")
            .field("generation", &self.generation.name())
            .finish()
    }
}

impl PlaneReaders {
    /// Refresh per-binding query state after publication without reopening any
    /// immutable reader or taking the global reader-cache lock.
    pub(crate) fn rebind(
        &self,
        access: &super::contracts::ViewAccess,
    ) -> Result<Arc<Self>, super::contracts::PlaneError> {
        let mut resources = Vec::with_capacity(self.adapters.len());
        for lease in &self.adapters {
            if lease.key.1 != access.family() || lease.key.2 != access.scope() {
                return Err(super::contracts::PlaneError::loader(
                    super::contracts::LoaderStage::Installation,
                    "reader lease belongs to another view",
                ));
            }
            resources.push(lease.adapter.bind_reader(
                access,
                &self.generation,
                lease.resource.clone(),
            )?);
        }
        Ok(Arc::new(Self {
            resources,
            adapters: self.adapters.clone(),
            generation: self.generation.clone(),
        }))
    }

    pub(super) fn resource(
        &self,
        adapter: usize,
        family: &str,
        scope: &str,
        generation: &str,
    ) -> Option<PlaneAttachment> {
        self.adapters
            .iter()
            .zip(&self.resources)
            .find(|(lease, _)| {
                lease.key.0 == adapter
                    && lease.key.1 == family
                    && lease.key.2 == scope
                    && lease.key.3 == generation
            })
            .and_then(|(_, resource)| resource.clone())
    }

    pub fn open_available(
        access: &super::contracts::ViewAccess,
        generation: Arc<ComposedGeneration>,
        adapters: &[Arc<dyn super::contracts::PlaneAdapter>],
    ) -> (Arc<Self>, Vec<super::contracts::PlaneError>) {
        let mut readers = Self {
            generation: generation.clone(),
            adapters: Vec::new(),
            resources: Vec::new(),
        };
        let mut errors = Vec::new();
        for adapter in adapters {
            match Self::open(access, generation.clone(), std::slice::from_ref(adapter)) {
                Ok(open) => {
                    readers.adapters.extend(open.adapters.iter().cloned());
                    readers.resources.extend(open.resources.iter().cloned());
                }
                Err(error) => errors.push(error),
            }
        }
        (Arc::new(readers), errors)
    }

    pub fn open(
        access: &super::contracts::ViewAccess,
        generation: Arc<ComposedGeneration>,
        adapters: &[Arc<dyn super::contracts::PlaneAdapter>],
    ) -> Result<Arc<Self>, super::contracts::PlaneError> {
        let mut readers = Self {
            generation,
            adapters: Vec::new(),
            resources: Vec::new(),
        };
        for adapter in adapters {
            let key = (
                Arc::as_ptr(adapter) as *const () as usize,
                access.family().to_owned(),
                access.scope().to_owned(),
                readers.generation.resident_key(),
            );
            let slot = {
                let mut cache = reader_cache()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.retain(|_, slot| slot.strong_count() != 0);
                match cache.get(&key).and_then(std::sync::Weak::upgrade) {
                    Some(slot) => slot,
                    None => {
                        let slot = Arc::new(ReaderSlot::new());
                        cache.insert(key.clone(), Arc::downgrade(&slot));
                        slot
                    }
                }
            };
            let lease = loop {
                let mut state = slot
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*state {
                    ReaderSlotState::Live { lease, .. } if lease.strong_count() != 0 => {
                        if let Some(lease) = lease.upgrade() {
                            break lease;
                        }
                    }
                    ReaderSlotState::Vacant => {
                        let token = next_reader_token();
                        *state = ReaderSlotState::Opening(token);
                        drop(state);
                        let mut opening = ReaderTransition {
                            slot: slot.clone(),
                            token,
                            armed: true,
                        };
                        let resource = adapter.open_reader(access, &readers.generation)?;
                        let lease = Arc::new(AdapterLease {
                            key: key.clone(),
                            slot: slot.clone(),
                            token,
                            adapter: adapter.clone(),
                            access: access.clone(),
                            resource,
                            generation: readers.generation.clone(),
                        });
                        *slot
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            ReaderSlotState::Live {
                                token,
                                lease: Arc::downgrade(&lease),
                            };
                        opening.armed = false;
                        slot.changed.notify_all();
                        break lease;
                    }
                    _ => {}
                }
                // An expired live weak reference still owns retirement. Wait for
                // its destructor, rather than opening over a half-closed reader.
                state = slot
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                drop(state);
            };
            let resource =
                adapter.bind_reader(access, &readers.generation, lease.resource.clone())?;
            readers.adapters.push(lease);
            readers.resources.push(resource);
        }
        Ok(Arc::new(readers))
    }
}

/// Where a path's answer comes from in one snapshot. Exactly one source per
/// path: a live entry supersedes the generation's entry.
#[derive(Debug)]
pub enum Source<'a> {
    Live(&'a LiveEntry),
    Generation(&'a EntryV2),
    Absent,
}

/// One pinned `(generation, delta@version, intent@version)` view.
#[derive(Clone, Debug)]
pub struct Snapshot {
    inner: Arc<SnapshotInner>,
}

impl Snapshot {
    /// Checks the cached reconciliation flag. Work is summarized when the
    /// immutable snapshot is created, not by scanning the delta on each query.
    pub fn needs_reconcile(&self) -> bool {
        self.inner.needs_reconcile
    }

    pub(crate) fn with_publication_base(mut self, base: Option<String>) -> Self {
        Arc::make_mut(&mut self.inner).publication_base = Some(base);
        self
    }

    pub(crate) fn publication_base(&self) -> Option<&Option<String>> {
        self.inner.publication_base.as_ref()
    }

    pub(crate) fn with_reconciled_epoch(mut self, epoch: u64) -> Self {
        Arc::make_mut(&mut self.inner).reconciled_epoch = Some(epoch);
        self
    }

    pub(crate) fn reconciled_epoch(&self) -> Option<u64> {
        self.inner.reconciled_epoch
    }

    pub fn install_cut(&self) -> Option<&InstallCut> {
        self.inner.install_cut.as_ref()
    }
    pub fn with_install_cut(mut self, cut: InstallCut) -> Self {
        Arc::make_mut(&mut self.inner).install_cut = Some(cut);
        self
    }
    pub fn with_readers(mut self, readers: Arc<PlaneReaders>) -> Self {
        Arc::make_mut(&mut self.inner).readers = Some(readers);
        self
    }
    pub fn readers(&self) -> Option<&Arc<PlaneReaders>> {
        self.inner.readers.as_ref()
    }
    pub fn generation(&self) -> &Arc<ComposedGeneration> {
        &self.inner.generation
    }

    pub fn delta_version(&self) -> u64 {
        self.inner.delta_version
    }

    pub fn intent_version(&self) -> u64 {
        self.inner.intent_version
    }

    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    pub fn watcher(&self) -> WatcherState {
        self.inner.watcher
    }

    /// The root ignore files the walk behind this snapshot's membership ran
    /// under, or `None` when no walk recorded them (a snapshot taken without
    /// walking the checkout, such as a read-only load).
    pub fn root_ignore_files(&self) -> Option<&RootIgnoreFiles> {
        self.inner.root_ignore_files.as_deref()
    }

    /// Paths with an acknowledged AFT write that the delta had not applied as
    /// of this snapshot. Queries match them on their current bytes before any
    /// index pruning.
    pub fn pending_intent(&self) -> impl Iterator<Item = &RelPath> {
        self.inner.intent.keys()
    }

    pub fn live_entries(&self) -> impl Iterator<Item = (&RelPath, &LiveEntry)> {
        self.inner.entries.iter()
    }

    pub fn readiness_patches(&self) -> &BTreeMap<RelPath, PlanePatch> {
        &self.inner.readiness
    }

    pub fn plane_readiness(&self, path: &RelPath, plane: FamilyPlane) -> Option<&PlaneState> {
        if let Some(patch) = self.inner.readiness.get(path) {
            if matches!(self.disk_state(path), DiskState::Present { content, .. } if content == patch.content)
            {
                if let Some(state) = patch.planes.get(plane) {
                    return Some(state);
                }
            }
        }
        if self.inner.entries.contains_key(path) {
            return None;
        }
        self.generation().plane_readiness(path, plane)
    }

    pub fn source(&self, rel_path: &RelPath) -> Source<'_> {
        if let Some(entry) = self.inner.entries.get(rel_path) {
            return Source::Live(entry);
        }
        match self.inner.generation.entry(rel_path) {
            Some(entry) => Source::Generation(entry),
            None => Source::Absent,
        }
    }

    /// The disk state this snapshot believes `rel_path` has.
    pub fn disk_state(&self, rel_path: &RelPath) -> DiskState {
        match self.source(rel_path) {
            Source::Live(entry) => entry.disk,
            Source::Generation(_) | Source::Absent => self.inner.generation.disk_state(rel_path),
        }
    }

    /// Every path the snapshot considers present, with its content.
    pub fn membership(&self) -> BTreeMap<RelPath, DiskState> {
        let mut members = BTreeMap::new();
        for (path, entry) in self.inner.generation.entries() {
            if let EntryV2::Regular { content, size, .. } = entry {
                members.insert(
                    path.clone(),
                    DiskState::Present {
                        content: *content,
                        size: *size,
                    },
                );
            }
        }
        for (path, entry) in self.inner.entries.iter() {
            match entry.disk {
                DiskState::Present { .. } => {
                    members.insert(path.clone(), entry.disk);
                }
                DiskState::Absent => {
                    members.remove(path);
                }
            }
        }
        members
    }
}

impl LiveEntries for Snapshot {
    fn live_states(&self) -> Box<dyn Iterator<Item = (&RelPath, &DiskState)> + '_> {
        Box::new(
            self.inner
                .entries
                .iter()
                .map(|(path, entry)| (path, &entry.disk)),
        )
    }

    fn live_state(&self, rel_path: &RelPath) -> Option<&DiskState> {
        self.inner.entries.get(rel_path).map(|entry| &entry.disk)
    }
    fn readiness_patch(&self, path: &RelPath, plane: FamilyPlane) -> Option<&PlaneState> {
        let patch = self.inner.readiness.get(path)?;
        if matches!(self.disk_state(path), DiskState::Present { content, .. } if content == patch.content)
        {
            patch.planes.get(plane)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod composition_tests {
    use super::super::manifest_v2::{BaseHeader, PublishedFrontier};
    use super::super::readiness::{self, FillMap, PlaneReadiness};
    use super::*;

    fn path(name: &str) -> RelPath {
        RelPath::new(name.as_bytes().to_vec()).unwrap()
    }
    fn header() -> ManifestHeader {
        ManifestHeader {
            producers: Producers {
                trigram: "tri".into(),
                semantic: Some("sem".into()),
                callgraph: super::super::callgraph::PRODUCER.into(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: Some("11".repeat(32)),
        }
    }
    fn planes(state: PlaneState) -> EntryPlanes {
        EntryPlanes {
            trigram: Some(state.clone()),
            semantic: Some(state.clone()),
            callgraph: Some(state),
        }
    }
    fn entry(bytes: &[u8], state: PlaneState) -> EntryV2 {
        EntryV2::regular(ContentHash::of(bytes), bytes.len() as u64, planes(state))
    }
    fn ready() -> PlaneState {
        PlaneState::Ready {
            key: "22".repeat(32),
        }
    }
    fn worker() -> ManifestV2 {
        let mut header = header();
        header.segment = None;
        ManifestV2::new_v3(
            header,
            Some(BaseHeader {
                scope: "base".into(),
                membership_digest: "33".repeat(32),
                identity_digest: "44".repeat(32),
                bound_generation: "base-generation".into(),
            }),
            PublishedFrontier {
                delta_cut_version: 7,
                watcher_epoch: 2,
                published_at_ms: 50,
                verified_frontier: None,
            },
        )
    }
    fn base() -> Arc<ComposedGeneration> {
        let mut manifest = ManifestV2::new_v3(header(), None, PublishedFrontier::default());
        for name in ["inherited.rs", "deleted.rs", "replaced.rs", "exception.rs"] {
            manifest
                .insert(
                    path(name),
                    entry(name.as_bytes(), PlaneState::pending("base fill")),
                )
                .unwrap();
        }
        Arc::new(ComposedGeneration::new("base-generation", manifest, None))
    }

    #[test]
    fn fold_keeps_unchanged_inherited_planes_out_of_worker_overrides() {
        for trigram in [Some(ready()), None] {
            let base = base();
            let file = path("inherited.rs");
            let mut update = base.entry(&file).unwrap().clone();
            let EntryV2::Regular { planes, .. } = &mut update else {
                unreachable!()
            };
            planes.trigram = trigram.clone();
            let composed =
                ComposedGeneration::compose("worker", worker(), base.clone(), None).unwrap();
            let folded = composed
                .fold(
                    &header().producers,
                    [(file.clone(), Some(update))],
                    &FillMap::default(),
                )
                .unwrap();
            let next = Arc::new(ComposedGeneration::compose("next", folded, base, None).unwrap());
            assert!(
                !next.owns_work(&file, FamilyPlane::Semantic),
                "a trigram-only fold claimed inherited semantic work"
            );
            assert_eq!(
                next.plane_readiness(&file, FamilyPlane::Trigram),
                trigram.as_ref()
            );
            let live = LiveDelta::new(next.clone());
            assert!(readiness::work_queue(
                FamilyPlane::Semantic,
                "sem",
                Some(&next),
                &live,
                &FillMap::default(),
                &|_| true
            )
            .is_empty());
        }
    }

    #[test]
    fn fold_uses_post_update_content_when_applying_override_fills() {
        use super::super::readiness::{Admission, Completion, WorkItem};
        use crate::blob_store::v2::FamilyKey;
        let base = base();
        let file = path("replaced.rs");
        let mut delta = worker();
        delta
            .set(
                file.clone(),
                Some(entry(b"B", PlaneState::pending("B fill"))),
            )
            .unwrap();
        let composed = ComposedGeneration::compose("worker", delta, base.clone(), None).unwrap();
        let key = FamilyKey::new(FamilyPlane::Semantic, [0x55; 32]);
        let mut fills = FillMap::default();
        let disk = DiskState::of_bytes(b"B");
        assert_eq!(
            fills.admit(
                &Completion {
                    item: WorkItem {
                        plane: FamilyPlane::Semantic,
                        rel_path: file.clone(),
                        content: ContentHash::of(b"B"),
                        producer: "sem".into()
                    },
                    key,
                },
                "sem",
                Some(&disk),
                Some(&composed)
            ),
            Admission::Installed
        );
        let mut restored = base.entry(&file).unwrap().clone();
        let EntryV2::Regular { planes, .. } = &mut restored else {
            unreachable!()
        };
        planes.semantic = Some(PlaneState::pending("repair inherited A"));
        let folded = composed
            .fold(
                &header().producers,
                [(file.clone(), Some(restored))],
                &fills,
            )
            .unwrap();
        let next = ComposedGeneration::compose("next", folded, base.clone(), None).unwrap();
        assert_eq!(next.disk_state(&file), base.disk_state(&file));
        assert_eq!(
            next.plane_readiness(&file, FamilyPlane::Semantic),
            Some(&PlaneState::pending("repair inherited A")),
            "a fill for replaced B was attached to inherited A"
        );
    }

    #[test]
    fn a_worker_tombstone_must_name_an_inherited_member() {
        let mut delta = worker();
        delta.set(path("not-in-base.rs"), None).unwrap();
        assert!(ComposedGeneration::compose("nonmember-tombstone", delta, base(), None).is_err());
    }

    #[test]
    fn composed_answers_equal_independently_flattened_manifest_for_every_plane() {
        let base = base();
        let mut delta = worker();
        delta.set(path("deleted.rs"), None).unwrap();
        delta
            .set(path("replaced.rs"), Some(entry(b"replacement", ready())))
            .unwrap();
        delta
            .set(path("added.rs"), Some(entry(b"addition", ready())))
            .unwrap();
        delta
            .set_readiness_override(path("exception.rs"), planes(ready()))
            .unwrap();
        let composed = ComposedGeneration::compose("worker", delta, base, None).unwrap();
        let mut flat = ManifestV2::new(header());
        flat.insert(
            path("inherited.rs"),
            entry(b"inherited.rs", PlaneState::pending("base fill")),
        )
        .unwrap();
        flat.insert(path("exception.rs"), entry(b"exception.rs", ready()))
            .unwrap();
        flat.insert(path("replaced.rs"), entry(b"replacement", ready()))
            .unwrap();
        flat.insert(path("added.rs"), entry(b"addition", ready()))
            .unwrap();
        let flat = ComposedGeneration::new("flat", flat, None);
        assert_eq!(
            composed.entries().collect::<Vec<_>>(),
            flat.entries().collect::<Vec<_>>()
        );
        assert_eq!(composed.membership(), flat.membership());
        assert_eq!(composed.segment_ids(), flat.segment_ids());
        let live = LiveDelta::new(Arc::new(ComposedGeneration::new(
            "empty",
            ManifestV2::new(header()),
            None,
        )));
        for name in [
            "inherited.rs",
            "deleted.rs",
            "replaced.rs",
            "exception.rs",
            "added.rs",
            "unknown.rs",
        ] {
            let path = path(name);
            assert_eq!(composed.entry(&path), flat.entry(&path));
            assert_eq!(composed.disk_state(&path), flat.disk_state(&path));
            for plane in FamilyPlane::ALL {
                assert_eq!(
                    composed.plane_readiness(&path, plane),
                    flat.plane_readiness(&path, plane)
                );
                assert_eq!(
                    readiness::plane_readiness(
                        plane,
                        Some(&composed),
                        &live,
                        &FillMap::default(),
                        &|_| true
                    ),
                    readiness::plane_readiness(
                        plane,
                        Some(&flat),
                        &live,
                        &FillMap::default(),
                        &|_| true
                    )
                );
            }
        }
        // The graph projection is honest about the one inherited pending member.
        assert!(super::super::callgraph::project_manifest(&composed).is_err());
        assert_eq!(composed.flatten().unwrap(), flat.flatten().unwrap());
    }

    #[test]
    fn partial_overrides_preserve_other_planes_and_delta_none_does_not_inherit() {
        let path = path("partial.rs");
        let replaced = self::path("replaced.rs");
        let original = EntryPlanes {
            trigram: Some(ready()),
            semantic: Some(PlaneState::pending("base semantic")),
            callgraph: Some(PlaneState::Failed {
                reason: "extractor failed".into(),
                producer: "extractor".into(),
            }),
        };
        let mut manifest = ManifestV2::new(header());
        manifest
            .insert(
                path.clone(),
                EntryV2::regular(ContentHash::of(b"same"), 4, original.clone()),
            )
            .unwrap();
        manifest
            .insert(
                replaced.clone(),
                EntryV2::regular(ContentHash::of(b"old"), 3, original.clone()),
            )
            .unwrap();
        let base = Arc::new(ComposedGeneration::new("base", manifest, None));
        let mut delta = worker();
        delta
            .set_readiness_override(
                path.clone(),
                EntryPlanes {
                    semantic: Some(ready()),
                    ..Default::default()
                },
            )
            .unwrap();
        delta
            .set(
                replaced.clone(),
                Some(EntryV2::regular(
                    ContentHash::of(b"new"),
                    3,
                    EntryPlanes::default(),
                )),
            )
            .unwrap();
        let composed = ComposedGeneration::compose("worker", delta, base, None).unwrap();
        assert_eq!(
            composed.plane_readiness(&path, FamilyPlane::Trigram),
            original.trigram.as_ref()
        );
        assert_eq!(
            composed.plane_readiness(&path, FamilyPlane::Semantic),
            Some(&ready())
        );
        assert_eq!(
            composed.plane_readiness(&path, FamilyPlane::Callgraph),
            original.callgraph.as_ref()
        );
        for plane in FamilyPlane::ALL {
            assert_eq!(composed.plane_readiness(&replaced, plane), None);
        }
    }

    #[test]
    fn worker_queue_excludes_inherited_base_work_but_counts_its_gap() {
        let mut delta = worker();
        delta
            .set_readiness_override(
                path("exception.rs"),
                planes(PlaneState::pending("dependency changed")),
            )
            .unwrap();
        let composed =
            Arc::new(ComposedGeneration::compose("worker", delta, base(), None).unwrap());
        let live = LiveDelta::new(composed.clone());
        for plane in FamilyPlane::ALL {
            assert_eq!(
                readiness::plane_readiness(
                    plane,
                    Some(&composed),
                    &live,
                    &FillMap::default(),
                    &|_| true
                ),
                PlaneReadiness::Ready {
                    pending: 4,
                    failed: 0
                }
            );
            let work = readiness::work_queue(
                plane,
                "producer",
                Some(&composed),
                &live,
                &FillMap::default(),
                &|_| true,
            );
            assert_eq!(
                work.iter()
                    .map(|item| item.rel_path.clone())
                    .collect::<Vec<_>>(),
                vec![path("exception.rs")]
            );
        }
    }

    #[test]
    fn equality_preserves_readiness_and_rebase_drops_only_satisfied_patches() {
        let base = base();
        let path = path("exception.rs");
        let mut live = LiveDelta::new(base.clone());
        live.set_readiness(
            path.clone(),
            PlanePatch {
                content: ContentHash::of(b"exception.rs"),
                planes: planes(ready()),
            },
        );
        live.apply(path.clone(), LiveEntry::new(base.disk_state(&path), 1));
        assert!(live.entry(&path).is_none());
        assert!(live.readiness().contains_key(&path));
        assert!(rebase_paths(&live.cut(), &base).contains(&path));
        let draft = derive_successor(&live.cut(), base.clone(), &carry_disk_state_only);
        live.replay_and_swap(draft, &carry_disk_state_only).unwrap();
        assert!(live.readiness().contains_key(&path));
        let mut next = base.flatten().unwrap();
        next.set(path.clone(), Some(entry(b"exception.rs", ready())))
            .unwrap();
        let next = Arc::new(ComposedGeneration::new("filled", next, None));
        let draft = derive_successor(&live.cut(), next, &carry_disk_state_only);
        live.replay_and_swap(draft, &carry_disk_state_only).unwrap();
        assert!(live.readiness().is_empty());
    }

    #[test]
    fn a_readiness_patch_never_vouches_for_changed_content() {
        let path = path("exception.rs");
        let mut manifest = base().flatten().unwrap();
        manifest
            .set(path.clone(), Some(entry(b"exception.rs", ready())))
            .unwrap();
        let base = Arc::new(ComposedGeneration::new("ready-base", manifest, None));
        let mut live = LiveDelta::new(base);
        live.set_readiness(
            path.clone(),
            PlanePatch {
                content: ContentHash::of(b"exception.rs"),
                planes: planes(ready()),
            },
        );
        live.apply(
            path.clone(),
            LiveEntry::new(DiskState::of_bytes(b"changed bytes"), 1),
        );
        assert!(live.readiness().contains_key(&path));
        let snapshot = live.snapshot();
        for plane in FamilyPlane::ALL {
            assert_eq!(snapshot.plane_readiness(&path, plane), None);
            for facts in [
                &live as &dyn readiness::LiveEntries,
                &snapshot as &dyn readiness::LiveEntries,
            ] {
                assert_eq!(
                    readiness::plane_readiness(
                        plane,
                        Some(snapshot.generation()),
                        facts,
                        &FillMap::default(),
                        &|_| true
                    ),
                    PlaneReadiness::Ready {
                        pending: 4,
                        failed: 0
                    }
                );
            }
        }
    }

    #[test]
    fn fold_persists_only_worker_override_fills_and_keeps_the_base_reference() {
        use super::super::readiness::{Admission, Completion, WorkItem};
        let base = base();
        let path = path("exception.rs");
        let mut delta = worker();
        delta
            .set_readiness_override(
                path.clone(),
                EntryPlanes {
                    semantic: Some(PlaneState::pending("worker dependency")),
                    ..Default::default()
                },
            )
            .unwrap();
        let composed = ComposedGeneration::compose("worker", delta, base.clone(), None).unwrap();
        let key = crate::blob_store::v2::FamilyKey::new(FamilyPlane::Semantic, [0x55; 32]);
        let completion = Completion {
            item: WorkItem {
                plane: FamilyPlane::Semantic,
                rel_path: path.clone(),
                content: ContentHash::of(b"exception.rs"),
                producer: "sem".into(),
            },
            key,
        };
        let mut fill = FillMap::default();
        assert_eq!(
            fill.admit(&completion, "sem", None, Some(&composed)),
            Admission::Installed
        );
        let folded = readiness::fold(&composed, &header().producers, [], &fill).unwrap();
        assert_eq!(folded.base_header(), composed.base_header());
        assert!(folded.is_empty());
        assert_eq!(folded.ready_keys().collect::<Vec<_>>(), vec![key]);
        let next = ComposedGeneration::compose("folded", folded, base, None).unwrap();
        assert_eq!(
            next.plane_readiness(&path, FamilyPlane::Semantic),
            Some(&PlaneState::ready(&key))
        );
        assert!(next
            .plane_readiness(&self::path("inherited.rs"), FamilyPlane::Semantic)
            .unwrap()
            .is_pending());
    }

    #[test]
    fn v3_sections_roundtrip_and_ready_keys_include_deduplicated_overrides() {
        let mut delta = worker();
        delta
            .set(path("added.rs"), Some(entry(b"added", ready())))
            .unwrap();
        delta.set(path("deleted.rs"), None).unwrap();
        delta
            .set_readiness_override(path("exception.rs"), planes(ready()))
            .unwrap();
        let bytes = delta.to_json_bytes().unwrap();
        assert_eq!(ManifestV2::from_json_bytes(&bytes).unwrap(), delta);
        let keys: Vec<_> = delta.ready_keys().collect();
        assert_eq!(keys.len(), 3);
        delta.set(path("added.rs"), None).unwrap();
        assert_eq!(delta.ready_keys().collect::<Vec<_>>(), keys);
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["readiness_overrides"][0]["rel_path"] = json["entries"][0]["rel_path"].clone();
        assert!(ManifestV2::from_json_bytes(&serde_json::to_vec(&json).unwrap()).is_err());
    }

    #[test]
    #[should_panic(expected = "a worker must be opened with ComposedGeneration::compose")]
    fn a_worker_cannot_be_opened_as_a_flat_generation() {
        ComposedGeneration::new("worker", worker(), None);
    }

    #[test]
    fn composition_rejects_base_chains_and_missing_override_members() {
        assert!(ComposedGeneration::compose(
            "not-a-worker",
            ManifestV2::new(header()),
            base(),
            None
        )
        .is_err());
        let first = Arc::new(ComposedGeneration::compose("first", worker(), base(), None).unwrap());
        assert!(ComposedGeneration::compose("chain", worker(), first, None).is_err());
        let mut delta = worker();
        delta
            .set_readiness_override(path("unknown.rs"), planes(ready()))
            .unwrap();
        assert!(ComposedGeneration::compose("bad", delta, base(), None).is_err());
        let mut incompatible = worker();
        incompatible.header_mut().producers.trigram = "other policy".into();
        assert!(ComposedGeneration::compose("incompatible", incompatible, base(), None).is_err());
    }
}
