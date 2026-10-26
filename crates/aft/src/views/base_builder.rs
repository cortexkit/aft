//! v3 base scopes: registration, publication and readiness successors, and
//! (with the build claim) single-flight construction.
//!
//! A base is published once its membership and lexical plane are complete.
//! From then on its membership digest, identity and trigram segment are
//! fixed; its pointer moves only to readiness successors, which may turn
//! `Pending` into `Ready` or `Failed`, `Failed` into `Ready`, and nothing
//! else. A `Ready` key never changes.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, Transaction};

use crate::blob_store::v2::FamilyPlane;
use crate::blob_store::v3;

use super::base_refs::{with_base_barrier, BaseIdentity, BaseRefError, Result, KIND_BASE};
use super::contracts::{boundary, DurabilityObserver, DurabilityStep};
use super::manifest_v2::{GenerationName, ManifestV2, PublishV2};
use super::readiness::PlaneState;
use super::registry::STATE_EVICTING;
use super::ViewStore;

/// Registers the base member row (`kind = base`, no root) inside a barrier.
/// A member in `evicting` is never reused.
pub(crate) fn register_base_in(
    tx: &Transaction<'_>,
    storage: &Path,
    family: &str,
    scope: &str,
) -> Result<(PathBuf, bool)> {
    let dir = v3::view_dir(storage, family, scope)?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,state FROM members WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((_, state)) if state == STATE_EVICTING => Err(BaseRefError::Unavailable(format!(
            "base {scope} is evicting"
        ))),
        Some((kind, _)) if kind != KIND_BASE => Err(BaseRefError::Corrupt(format!(
            "{scope} is registered as a {kind}"
        ))),
        Some(_) => Ok((dir, false)),
        None => {
            tx.execute(
                "INSERT INTO members(scope,kind,root,view_dir,registered_at_ms,last_bind_ms,state) VALUES(?1,'base',NULL,?2,?3,?3,'active')",
                params![scope, dir.to_string_lossy(), crate::pins::now_ms() as i64],
            )?;
            Ok((dir, true))
        }
    }
}

/// Whether `next` may replace `current` as the base's generation: same
/// identity and segment, and every entry's per-plane state either unchanged
/// or one of the allowed monotone transitions.
pub fn check_successor(family: &str, current: &ManifestV2, next: &ManifestV2) -> Result<()> {
    let old = BaseIdentity::of(family, current)?;
    let new = BaseIdentity::of(family, next)?;
    if old != new {
        return Err(BaseRefError::Corrupt(
            "a readiness successor changes the base identity".into(),
        ));
    }
    if current.header().segment != next.header().segment {
        return Err(BaseRefError::Corrupt(
            "a readiness successor replaces the trigram segment".into(),
        ));
    }
    for ((path, before), (_, after)) in current.entries().zip(next.entries()) {
        for plane in FamilyPlane::ALL {
            let allowed = match (before.plane_state(plane), after.plane_state(plane)) {
                (a, b) if a == b => true,
                (Some(PlaneState::Pending { .. }), Some(PlaneState::Ready { .. })) => true,
                (Some(PlaneState::Pending { .. }), Some(PlaneState::Failed { .. })) => true,
                (Some(PlaneState::Failed { .. }), Some(PlaneState::Ready { .. })) => true,
                // A retry may record a new reason for a still-pending entry.
                (Some(PlaneState::Pending { .. }), Some(PlaneState::Pending { .. })) => true,
                _ => false,
            };
            if !allowed {
                return Err(BaseRefError::Corrupt(format!(
                    "forbidden {} transition for {:?}",
                    plane.as_str(),
                    path
                )));
            }
        }
    }
    Ok(())
}

/// A first base publication needs complete membership and a complete lexical
/// plane: no trigram entry may still be pending. Graph and semantic entries
/// may be.
pub fn check_lexical_complete(manifest: &ManifestV2) -> Result<()> {
    for (path, entry) in manifest.entries() {
        if entry
            .plane_state(FamilyPlane::Trigram)
            .is_some_and(PlaneState::is_pending)
        {
            return Err(BaseRefError::Corrupt(format!(
                "first base publication with a pending lexical entry {path:?}"
            )));
        }
    }
    Ok(())
}

/// What a base publication did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BasePublication {
    /// The pointer names `generation`.
    Published { generation: String, first: bool },
    /// Another publisher moved the pointer first; nothing was published.
    Lost { current: Option<String> },
}

/// Publishes `manifest` as the base's first generation, or as a readiness
/// successor of its current one. Callers own the base's build claim; the
/// manifest's Ready keys must already be durable in the family's v3 plane
/// stores and protected by the caller's pin until this returns.
pub fn publish_base(
    storage: &Path,
    family: &str,
    manifest: &ManifestV2,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<BasePublication> {
    let identity = BaseIdentity::of(family, manifest)?;
    let (dir, registered) = with_base_barrier(storage, family, |tx| {
        register_base_in(tx, storage, family, &identity.scope)
    })?;
    if registered {
        boundary(observer, DurabilityStep::BaseRegistered)?;
    }
    let store = ViewStore::open_dir(dir)?;
    let current = store.current_generation()?;
    match &current {
        Some(current) => {
            let current_manifest = store.load_manifest_v2(current)?;
            check_successor(family, &current_manifest, manifest)?;
        }
        None => check_lexical_complete(manifest)?,
    }
    let name = GenerationName::for_manifest(manifest)?;
    let prepared = store.prepare_v2(&name, current.as_deref(), manifest, observer)?;
    match store.commit_v2(prepared, observer)? {
        PublishV2::Published => {
            if current.is_some() {
                boundary(observer, DurabilityStep::BaseSuccessorPublished)?;
            }
            Ok(BasePublication::Published {
                generation: name.to_string(),
                first: current.is_none(),
            })
        }
        PublishV2::Conflict { current } => Ok(BasePublication::Lost { current }),
        PublishV2::EquivalentWinner { current } => Ok(BasePublication::Lost {
            current: Some(current),
        }),
    }
}
