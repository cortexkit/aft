//! Per-plane readiness that does not depend on whether disk differs from the
//! generation.
//!
//! Pending and failed plane work is part of each published manifest entry, so
//! it survives folds, eviction and restart. A fold that publishes a file's
//! content before that file's semantic fill finishes keeps the entry
//! `Pending`; the pending count comes from the generation's entries **and**
//! from live entries, never from live entries alone, because the live entry
//! disappears as soon as disk equals the new generation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};

use super::manifest_v2::{EntryV2, ManifestV2, Producers};
use super::snapshot::{ComposedGeneration, DiskState};
use super::{RelPath, Result, ViewError};

/// Whether a live delta's bytes were stably observed, independently of whether
/// its plane artifacts have finished materializing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReconciledDelta {
    #[default]
    Unreconciled,
    Reconciled,
    /// Repeated unstable reads supplied no bytes safe to insert.
    Pending,
}

/// The state of one plane for one manifest entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlaneState {
    /// The plane's artifact is stored under this key (lower-case hex).
    Ready { key: String },
    /// Work for this content has not completed.
    Pending { reason: String },
    /// A deterministic failure of `producer` for this content. It persists
    /// while content and producer are unchanged and never blocks other paths.
    Failed { reason: String, producer: String },
}

impl PlaneState {
    pub fn ready(key: &FamilyKey) -> Self {
        Self::Ready { key: key.to_hex() }
    }

    pub fn pending(reason: impl Into<String>) -> Self {
        Self::Pending {
            reason: reason.into(),
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Readiness of one plane in one snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaneReadiness {
    /// No generation exists.
    Absent,
    /// A generation exists, but the plane is not materialized for it.
    Building,
    /// The plane answers, with this much work outstanding.
    Ready { pending: usize, failed: usize },
}

/// A unit of plane work: produce `plane`'s artifact for `content` at
/// `rel_path` with `producer`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkItem {
    pub plane: FamilyPlane,
    pub rel_path: RelPath,
    pub content: ContentHash,
    pub producer: String,
}

/// A finished work item and the key its artifact was stored under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub item: WorkItem,
    pub key: FamilyKey,
}

/// What happened to a completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Installed,
    /// The producer changed since the work was queued.
    DroppedProducer,
    /// The path's content changed since the work was queued.
    DroppedContent,
}

/// Installed completions not yet folded into a generation:
/// `(rel_path, content, plane) -> key`. Queries use it at once; the next fold
/// writes it into the manifest, turning `Pending` into `Ready`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FillMap {
    keys: BTreeMap<(RelPath, ContentHash, FamilyPlane), FamilyKey>,
}

impl FillMap {
    pub fn get(
        &self,
        rel_path: &RelPath,
        content: &ContentHash,
        plane: FamilyPlane,
    ) -> Option<&FamilyKey> {
        self.keys.get(&(rel_path.clone(), *content, plane))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = &FamilyKey> {
        self.keys.values()
    }

    /// Use on a new binding's copy: older readers can still need retired fills.
    pub(super) fn retain_current(&mut self, snapshot: &super::snapshot::Snapshot) {
        self.keys.retain(|(path, content, _), _| {
            matches!(snapshot.disk_state(path), DiskState::Present { content: current, .. } if current == *content)
        });
    }

    /// Drops fills the given manifest already records as `Ready`.
    pub fn trim_folded(&mut self, manifest: &ComposedGeneration) {
        self.keys.retain(|(path, content, plane), key| {
            !matches!(
                manifest.entry(path),
                Some(entry) if entry.content() == Some(*content)
                    && manifest.plane_readiness(path, *plane) == Some(&PlaneState::ready(key))
            )
        });
    }

    /// Admits a completion under the rule of the readiness contract: the
    /// producer must be the current one, and the path's current content (its
    /// live entry if it has one, else the generation's entry) must be the
    /// content the work was computed for.
    pub fn admit(
        &mut self,
        completion: &Completion,
        current_producer: &str,
        live: Option<&DiskState>,
        generation: Option<&ComposedGeneration>,
    ) -> Admission {
        let item = &completion.item;
        if item.producer != current_producer {
            return Admission::DroppedProducer;
        }
        let current = match live {
            Some(DiskState::Present { content, .. }) => Some(*content),
            Some(DiskState::Absent) => None,
            None => generation
                .and_then(|manifest| manifest.entry(&item.rel_path))
                .and_then(EntryV2::content),
        };
        if current != Some(item.content) {
            return Admission::DroppedContent;
        }
        self.keys.insert(
            (item.rel_path.clone(), item.content, item.plane),
            completion.key,
        );
        Admission::Installed
    }
}

/// A live entry as readiness sees it.
pub trait LiveEntries {
    /// The last observed disk state of every path with a live entry.
    fn live_states(&self) -> Box<dyn Iterator<Item = (&RelPath, &DiskState)> + '_>;
    fn live_state(&self, rel_path: &RelPath) -> Option<&DiskState>;
    fn readiness_patch(&self, _path: &RelPath, _plane: FamilyPlane) -> Option<&PlaneState> {
        None
    }
}

/// Counts outstanding work for `plane` in one snapshot.
///
/// - A path with a live entry counts from that entry: pending when present,
///   applicable, and not in the fill map.
/// - A path without a live entry counts from the generation's entry state,
///   unless the fill map already holds its key.
///
/// `applies_to` says whether the plane has work for a path.
pub fn plane_readiness(
    plane: FamilyPlane,
    generation: Option<&ComposedGeneration>,
    live: &dyn LiveEntries,
    fill: &FillMap,
    applies_to: &dyn Fn(&RelPath) -> bool,
) -> PlaneReadiness {
    let Some(generation) = generation else {
        return PlaneReadiness::Absent;
    };
    let mut pending = 0;
    let mut failed = 0;
    for (path, entry) in generation.entries() {
        if live.live_state(path).is_some() {
            continue;
        }
        let Some(content) = entry.content() else {
            continue;
        };
        match live
            .readiness_patch(path, plane)
            .or_else(|| generation.plane_readiness(path, plane))
        {
            Some(PlaneState::Pending { .. }) if fill.get(path, &content, plane).is_none() => {
                pending += 1;
            }
            Some(PlaneState::Failed { .. }) => failed += 1,
            _ => {}
        }
    }
    for (path, state) in live.live_states() {
        if let DiskState::Present { content, .. } = state {
            if applies_to(path) && fill.get(path, content, plane).is_none() {
                match live.readiness_patch(path, plane) {
                    Some(PlaneState::Ready { .. }) => {}
                    Some(PlaneState::Failed { .. }) => failed += 1,
                    _ => pending += 1,
                }
            }
        }
    }
    PlaneReadiness::Ready { pending, failed }
}

/// The work queue for `plane`: live entries whose plane is not ready, plus
/// the generation's `Pending` entries for paths without a live entry.
pub fn work_queue(
    plane: FamilyPlane,
    producer: &str,
    generation: Option<&ComposedGeneration>,
    live: &dyn LiveEntries,
    fill: &FillMap,
    applies_to: &dyn Fn(&RelPath) -> bool,
) -> Vec<WorkItem> {
    let mut items = Vec::new();
    if let Some(generation) = generation {
        for (path, entry) in generation.entries() {
            if live.live_state(path).is_some() {
                continue;
            }
            let Some(content) = entry.content() else {
                continue;
            };
            if (generation.owns_work(path, plane) || live.readiness_patch(path, plane).is_some())
                && matches!(
                    live.readiness_patch(path, plane)
                        .or_else(|| generation.plane_readiness(path, plane)),
                    Some(PlaneState::Pending { .. })
                )
                && fill.get(path, &content, plane).is_none()
            {
                items.push(WorkItem {
                    plane,
                    rel_path: path.clone(),
                    content,
                    producer: producer.to_owned(),
                });
            }
        }
    }
    for (path, state) in live.live_states() {
        if let DiskState::Present { content, .. } = state {
            if applies_to(path)
                && fill.get(path, content, plane).is_none()
                && !matches!(
                    live.readiness_patch(path, plane),
                    Some(PlaneState::Ready { .. } | PlaneState::Failed { .. })
                )
            {
                items.push(WorkItem {
                    plane,
                    rel_path: path.clone(),
                    content: *content,
                    producer: producer.to_owned(),
                });
            }
        }
    }
    items.sort();
    items.dedup();
    items
}

/// Builds the successor manifest from `base`, the entries the fold read, and
/// the installed fills. Unchanged entries keep their `Pending` and `Failed`
/// states; any `Pending` state whose key is in the fill map becomes `Ready`.
/// A producer change is not a fold: it needs a rebuild, so it is refused.
pub fn fold(
    base: &ComposedGeneration,
    producers: &Producers,
    updates: impl IntoIterator<Item = (RelPath, Option<EntryV2>)>,
    fill: &FillMap,
) -> Result<ManifestV2> {
    base.ensure_producers(producers).map_err(|_| {
        ViewError::ProducerMismatch(
            "a fold cannot change producers; rebuild the generation instead".to_string(),
        )
    })?;
    base.fold(producers, updates, fill)
}
