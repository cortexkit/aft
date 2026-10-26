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

/// Ruled live-claim wait (item 24): a binder that finds a live claim waits at
/// most this long, from first encountering it, for the base's first
/// lexical-ready publication, then takes the strict path. Heartbeats and
/// takeovers never reset that clock.
pub const LIVE_CLAIM_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
/// Builder heartbeat cadence and the age at which a binder inspects the
/// owner's liveness. Staleness alone never permits a takeover.
pub const CLAIM_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);
pub const CLAIM_STALE: std::time::Duration = std::time::Duration::from_secs(15);
/// Ruled cap of live bases per family, counting claimed or registered but
/// unpublished bases.
pub const LIVE_BASES_PER_FAMILY: i64 = 64;
/// Initial build reservation cap and extension step (pre-slice-4 ruling).
pub const INITIAL_RESERVATION_CAP: u64 = 1 << 30;
pub const RESERVATION_STEP: u64 = 256 << 20;

/// The ruled initial reservation for a base of `files` members and
/// `source_bytes` bytes: `ceil_MiB(64 MiB + max(64 KiB × N, 2 × B,
/// 1.25 × P × max(N/Np, B/Bp)))`, capped at 1 GiB. `history` is a comparable
/// base's `(peak allocated build bytes P, Np, Bp)`; it is ignored when absent
/// or when `Np` or `Bp` is zero.
pub fn initial_reservation(files: u64, source_bytes: u64, history: Option<(u64, u64, u64)>) -> u64 {
    const MIB: u128 = 1 << 20;
    let mut term = (64u128 * 1024 * u128::from(files)).max(2 * u128::from(source_bytes));
    if let Some((peak, np, bp)) = history.filter(|(_, np, bp)| *np > 0 && *bp > 0) {
        // 1.25 × P × max(N/Np, B/Bp), in exact integer arithmetic.
        let by_files = 5 * u128::from(peak) * u128::from(files) / (4 * u128::from(np));
        let by_bytes = 5 * u128::from(peak) * u128::from(source_bytes) / (4 * u128::from(bp));
        term = term.max(by_files).max(by_bytes);
    }
    let total = 64 * MIB + term;
    let rounded = total.div_ceil(MIB) * MIB;
    rounded.min(u128::from(INITIAL_RESERVATION_CAP)) as u64
}

/// The holder of a base's build claim. Every staging and derived name it
/// creates includes `(attempt, owner_token)`, so a takeover never reuses a
/// dead attempt's names.
#[derive(Debug)]
pub struct BuildClaim {
    storage: PathBuf,
    family: String,
    scope: String,
    token: String,
    attempt: i64,
}

/// What [`claim_base`] found.
#[derive(Debug)]
pub enum ClaimOutcome {
    /// This process now owns the build, at `attempt`.
    Claimed(BuildClaim),
    /// A live owner (running, stopped, or of uncertain liveness) holds it.
    Busy { owner_pid: u32, attempt: i64 },
    /// The family already has the ruled number of live bases; the cap hit is
    /// counted in `sweep_state.live_base_cap_hits`.
    CapReached { live_bases: i64 },
}

fn live_base_count(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT (SELECT COUNT(*) FROM members WHERE kind='base' AND state!='evicting') + (SELECT COUNT(*) FROM base_builds WHERE base_scope NOT IN (SELECT scope FROM members))",
        [],
        |r| r.get(0),
    )
}

/// Claims the build of base `scope` with the import ledger's owner-token CAS:
/// insert when absent; `Busy` while the owner lives; a dead owner's row is
/// taken over with `attempt + 1` and a fresh token, and the dead attempt's
/// staging becomes reclaimable.
pub fn claim_base(
    storage: &Path,
    family: &str,
    scope: &str,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<ClaimOutcome> {
    let me = super::registry::current_owner();
    let token = super::base_refs::fresh_nonce();
    let (outcome, dead_attempt) = with_base_barrier(storage, family, |tx| {
        let existing: Option<(u32, i64, String, i64)> = tx
            .query_row(
                "SELECT owner_pid,owner_start,owner_token,attempt FROM base_builds WHERE base_scope=?1",
                [scope],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let now = crate::pins::now_ms() as i64;
        match existing {
            None => {
                let registered: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM members WHERE scope=?1 AND state!='evicting')",
                    [scope],
                    |r| r.get(0),
                )?;
                let live = live_base_count(tx)?;
                if !registered && live >= LIVE_BASES_PER_FAMILY {
                    tx.execute(
                        "UPDATE sweep_state SET live_base_cap_hits=live_base_cap_hits+1 WHERE singleton=1",
                        [],
                    )?;
                    return Ok((ClaimOutcome::CapReached { live_bases: live }, None));
                }
                tx.execute(
                    "INSERT INTO base_builds VALUES(?1,?2,?3,?4,1,'building',?5)",
                    params![scope, me.pid, me.start_time as i64, token, now],
                )?;
                Ok((
                    ClaimOutcome::Claimed(BuildClaim::new(storage, family, scope, &token, 1)),
                    None,
                ))
            }
            Some((pid, start, old_token, attempt)) => {
                let owner = crate::pins::PinOwner {
                    pid,
                    start_time: start as u64,
                };
                if crate::pins::owner_is_live(&owner) {
                    return Ok((
                        ClaimOutcome::Busy {
                            owner_pid: pid,
                            attempt,
                        },
                        None,
                    ));
                }
                let updated = tx.execute(
                    "UPDATE base_builds SET owner_pid=?1,owner_start=?2,owner_token=?3,attempt=attempt+1,state='building',heartbeat_at_ms=?4 WHERE base_scope=?5 AND owner_token=?6",
                    params![me.pid, me.start_time as i64, token, now, scope, old_token],
                )?;
                if updated != 1 {
                    return Err(BaseRefError::Unavailable(format!("claim of {scope} moved")));
                }
                Ok((
                    ClaimOutcome::Claimed(BuildClaim::new(
                        storage,
                        family,
                        scope,
                        &token,
                        attempt + 1,
                    )),
                    Some((attempt, old_token)),
                ))
            }
        }
    })?;
    if let ClaimOutcome::Claimed(claim) = &outcome {
        if let Some((attempt, token)) = dead_attempt {
            claim.remove_dead_staging(attempt, &token)?;
        }
        boundary(observer, DurabilityStep::BaseClaimed)?;
    }
    Ok(outcome)
}

impl BuildClaim {
    fn new(storage: &Path, family: &str, scope: &str, token: &str, attempt: i64) -> Self {
        Self {
            storage: storage.to_path_buf(),
            family: family.to_owned(),
            scope: scope.to_owned(),
            token: token.to_owned(),
            attempt,
        }
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn attempt(&self) -> i64 {
        self.attempt
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    /// `M/staging/a<attempt>-<owner_token>-<nonce>/`, created fresh.
    pub fn staging_dir(&self) -> Result<PathBuf> {
        let dir = v3::view_dir(&self.storage, &self.family, &self.scope)?
            .join("staging")
            .join(format!(
                "a{}-{}-{}",
                self.attempt,
                self.token,
                super::base_refs::fresh_nonce()
            ));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
    /// `derived-<generation>-a<attempt>-<owner_token>.sqlite`.
    pub fn derived_name(&self, generation: &str) -> String {
        format!(
            "derived-{generation}-a{}-{}.sqlite",
            self.attempt, self.token
        )
    }
    /// Deletes the staging directories of a dead attempt. Called only after
    /// the claim CAS confirmed that attempt's owner dead.
    fn remove_dead_staging(&self, attempt: i64, token: &str) -> Result<()> {
        let staging = v3::view_dir(&self.storage, &self.family, &self.scope)?.join("staging");
        let prefix = format!("a{attempt}-{token}-");
        let entries = match std::fs::read_dir(&staging) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }
    /// True while the claim row still carries this owner's token.
    pub fn verify_held(&self) -> Result<bool> {
        with_base_barrier(&self.storage, &self.family, |tx| {
            Ok(tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM base_builds WHERE base_scope=?1 AND owner_token=?2)",
                params![self.scope, self.token],
                |r| r.get(0),
            )?)
        })
    }
    pub fn heartbeat(&self) -> Result<()> {
        with_base_barrier(&self.storage, &self.family, |tx| {
            let updated = tx.execute(
                "UPDATE base_builds SET heartbeat_at_ms=?1 WHERE base_scope=?2 AND owner_token=?3",
                params![crate::pins::now_ms() as i64, self.scope, self.token],
            )?;
            if updated != 1 {
                return Err(BaseRefError::Unavailable(
                    "the build claim was taken over".into(),
                ));
            }
            Ok(())
        })
    }
    /// Marks the base serving after its first publication. The owner keeps
    /// the claim, and with it the base's readiness successors, until
    /// [`Self::release`].
    pub fn mark_serving(&self) -> Result<()> {
        with_base_barrier(&self.storage, &self.family, |tx| {
            tx.execute(
                "UPDATE base_builds SET state='serving' WHERE base_scope=?1 AND owner_token=?2",
                params![self.scope, self.token],
            )?;
            Ok(())
        })
    }
    /// Releases the claim once the base is complete.
    pub fn release(self) -> Result<()> {
        with_base_barrier(&self.storage, &self.family, |tx| {
            tx.execute(
                "DELETE FROM base_builds WHERE base_scope=?1 AND owner_token=?2",
                params![self.scope, self.token],
            )?;
            Ok(())
        })
    }
}

/// What a binder that needs base `scope` does next.
#[derive(Debug)]
pub enum BinderPlan {
    /// Build the base under this claim.
    Build(BuildClaim),
    /// Another owner is building it: ACK the bind at once, serve the bounded
    /// direct scan with honest coverage, and wait for the first publication.
    Wait { remaining: std::time::Duration },
    /// Take the strict path to an own generation, for this reason.
    Strict(&'static str),
}

/// The binder decision of the views design: only the claim owner builds;
/// everyone else waits for the first lexical-ready publication, and goes
/// strict only on a failed claim, a refused admission (decided by the caller
/// after `Build`), the live-base cap, or an exhausted wait.
pub fn plan_binder(
    storage: &Path,
    family: &str,
    scope: &str,
    waited: std::time::Duration,
) -> BinderPlan {
    match claim_base(storage, family, scope, None) {
        Ok(ClaimOutcome::Claimed(claim)) => BinderPlan::Build(claim),
        Ok(ClaimOutcome::Busy { .. }) if waited >= LIVE_CLAIM_WAIT => {
            BinderPlan::Strict("live_claim_wait_exhausted")
        }
        Ok(ClaimOutcome::Busy { .. }) => BinderPlan::Wait {
            remaining: LIVE_CLAIM_WAIT - waited,
        },
        Ok(ClaimOutcome::CapReached { .. }) => BinderPlan::Strict("live_base_cap"),
        Err(_) => BinderPlan::Strict("claim_failed"),
    }
}

#[cfg(test)]
#[path = "base_builder_tests.rs"]
mod tests;
