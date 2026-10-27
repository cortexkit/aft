//! Tree-keyed bases and their blob receipts
//! (docs/design/views-v3-format.md, "Tree-keyed bases and blob receipts").
//!
//! A fresh checkout gets a tree-keyed base only when Git itself proves the
//! claim: the coherent index computes HEAD's tree, and every member's bytes,
//! read once under a stable native stat, hash to that member's index blob.
//! Each such read leaves a receipt in the base's proof file. A later bind of
//! the same checkout can then skip rereading a member whose receipt still
//! holds. Anything short of that proof keeps today's membership-keyed base and
//! its full check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use crate::blob_store::v2::{to_hex, FamilyPlane, TrigramPolicy};
use crate::blob_store::v3;
use crate::git_index_reader::{index_tree, read_index, IndexRead, ObjectFormat};

use super::base_builder::{claim_base, initial_reservation, BuildClaim, ClaimOutcome};
use super::base_refs::{
    resolve_base, with_base_barrier, AuthenticatedBase, BaseIdentity, BaseKind,
};
use super::manifest_v2::{EntryPlanes, EntryV2, ManifestHeader, ManifestV2, Producers};
use super::readiness::PlaneState;
use super::snapshot::{ComposedGeneration, V3Route};
use super::verification::{BindGuard, BlobReceipt, Evidence, NativeStat, TreeProof};
use super::RelPath;

/// What a fresh checkout needs to build a tree-keyed base.
pub(crate) struct TreeBaseInput<'a> {
    pub route: &'a V3Route,
    pub root: &'a Path,
    /// Walker-admitted paths of the checkout; only tracked regular files
    /// among them become members.
    pub paths: &'a [RelPath],
    pub policy: TrigramPolicy,
    pub producers: &'a Producers,
    pub ignore_fingerprint: Option<String>,
}

/// A base manifest whose every member was read and matched to the index,
/// with the bytes read and their receipts.
struct ProvenMembers {
    manifest: ManifestV2,
    bytes: BTreeMap<RelPath, Vec<u8>>,
    receipts: BTreeMap<RelPath, BlobReceipt>,
    format: ObjectFormat,
}

/// Why a fresh checkout gets no tree-keyed base.
pub(crate) type NoTreeProof = &'static str;

/// Reads and proves the members, or names the first check that failed.
fn prove(input: &TreeBaseInput<'_>, guard: &BindGuard) -> Result<ProvenMembers, NoTreeProof> {
    if !guard.policy.reusable {
        return Err("conversion_policy_not_reusable");
    }
    let head_tree = guard
        .head_tree
        .as_deref()
        .and_then(|tree| std::str::from_utf8(tree.trim_ascii()).ok())
        .ok_or("no_head_tree")?;
    let format = ObjectFormat::from_hex_len(head_tree.len()).ok_or("unknown_object_format")?;
    let index_path = guard.gitdir.as_ref().ok_or("no_gitdir")?.0.join("index");
    let IndexRead::Snapshot(snapshot) = read_index(&index_path, format, Default::default()) else {
        return Err("index_not_coherent");
    };
    if NativeStat::strict_read(&index_path).ok().as_ref()
        != guard.index.as_ref().map(|(stat, _)| stat)
    {
        return Err("index_changed_since_guard");
    }
    if to_hex(&index_tree(&snapshot).map_err(|_| "index_tree_refused")?) != head_tree {
        return Err("index_tree_differs_from_head");
    }
    let index: BTreeMap<&[u8], _> = snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.as_slice(), entry))
        .collect();
    let header = ManifestHeader {
        producers: input.producers.clone(),
        head_tree: Some(head_tree.to_owned()),
        ignore_fingerprint: input.ignore_fingerprint.clone(),
        segment: None,
    };
    let mut manifest = ManifestV2::new_v3(header, None, Default::default());
    let mut bytes = BTreeMap::new();
    let mut receipts = BTreeMap::new();
    for path in input.paths {
        // Untracked files and tracked symlinks or gitlinks stay out of the
        // base; the worker's own delta carries whatever the walker admits.
        let Some(entry) = index.get(path.as_bytes()) else {
            continue;
        };
        if !matches!(entry.stat.mode, 0o100644 | 0o100755) {
            continue;
        }
        let absolute = input
            .root
            .join(super::segment_store::rel_path_to_os(path).map_err(|_| "unrepresentable_path")?);
        let observed_ns = super::verification::now_ns();
        let unreadable = |_| "member_unreadable";
        let before = NativeStat::read(&absolute).map_err(unreadable)?;
        if !std::fs::symlink_metadata(&absolute)
            .map_err(unreadable)?
            .is_file()
        {
            return Err("member_not_a_regular_file");
        }
        let source = std::fs::read(&absolute).map_err(unreadable)?;
        let after = NativeStat::read(&absolute).map_err(unreadable)?;
        if before != after || before.size != source.len() as u64 {
            return Err("member_changed_during_read");
        }
        let oid = format.blob_oid(&source);
        if oid != entry.oid {
            return Err("member_differs_from_index_blob");
        }
        #[cfg(unix)]
        if (before.mode & 0o111 != 0) != (entry.stat.mode == 0o100755) {
            return Err("member_mode_differs_from_index");
        }
        let content = crate::blob_store::v2::ContentHash::of(&source);
        let planes = EntryPlanes {
            trigram: Some(PlaneState::pending("lexical plane not built")),
            semantic: input
                .producers
                .semantic
                .as_ref()
                .filter(|_| super::semantic::applies_to(path))
                .map(|_| PlaneState::pending("awaiting the base owner")),
            callgraph: Some(PlaneState::pending("awaiting the base owner")),
        };
        manifest
            .insert(
                path.clone(),
                EntryV2::Regular {
                    mode: entry.stat.mode,
                    content,
                    size: source.len() as u64,
                    git_oid: Some(to_hex(&oid)),
                    resolution_input: false,
                    planes,
                },
            )
            .map_err(|_| "invalid_manifest_entry")?;
        receipts.insert(
            path.clone(),
            BlobReceipt {
                version: 1,
                mode: entry.stat.mode,
                size: source.len() as u64,
                content: content.to_hex(),
                git_oid: to_hex(&oid),
                stat: before,
                observed_ns,
                policy: guard.policy.digest.clone(),
                delivery_seq: 0,
            },
        );
        bytes.insert(path.clone(), source);
    }
    if manifest.is_empty() {
        return Err("no_tracked_members");
    }
    Ok(ProvenMembers {
        manifest,
        bytes,
        receipts,
        format,
    })
}

/// Resolves or builds the tree-keyed base of a fresh checkout. `None` means no
/// tree-keyed base is available now (no proof, a busy or capped claim, a
/// refused admission or a failed build); the caller then takes its
/// membership-keyed path unchanged. A claim this call keeps is left in
/// `claim_slot` for the caller to retire, as for membership-keyed builds.
pub(crate) fn bootstrap(
    input: TreeBaseInput<'_>,
    claim_slot: &Mutex<Option<BuildClaim>>,
) -> Option<AuthenticatedBase> {
    let guard = BindGuard::capture(input.root, None).ok()?;
    let mut proven = prove(&input, &guard).ok()?;
    let route = input.route;
    let identity = BaseIdentity::of(&route.family, &proven.manifest).ok()?;
    if identity.kind() != BaseKind::Tree {
        return None;
    }
    if let Ok(base) = resolve_base(&route.storage, &route.family, &identity.scope) {
        return Some(base);
    }
    let ClaimOutcome::Claimed(claim) =
        claim_base(&route.storage, &route.family, &identity.scope, None).ok()?
    else {
        return None;
    };
    match build(route, &identity, &guard, &mut proven, input.policy, &claim) {
        Ok(()) => {
            let base = resolve_base(&route.storage, &route.family, &identity.scope).ok();
            *claim_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(claim);
            base
        }
        Err(_) => {
            let _ = claim.release();
            None
        }
    }
}

/// The base a fresh checkout binds: `tree` when [`bootstrap`] produced one,
/// otherwise the membership-keyed base `scope`, resolved as before.
pub(crate) fn resolve_preferring(
    tree: Option<AuthenticatedBase>,
    route: &V3Route,
    scope: &str,
) -> super::base_refs::Result<AuthenticatedBase> {
    match tree {
        Some(base) => Ok(base),
        None => resolve_base(&route.storage, &route.family, scope),
    }
}

/// The claimed build: admission, member registration, the proof file, the
/// lexical plane and the first publication, in that order.
fn build(
    route: &V3Route,
    identity: &BaseIdentity,
    guard: &BindGuard,
    proven: &mut ProvenMembers,
    policy: TrigramPolicy,
    claim: &BuildClaim,
) -> Result<(), String> {
    let storage = route.storage.as_path();
    let family = route.family.as_str();
    let error = |e: &dyn std::fmt::Display| e.to_string();
    let source_bytes = proven.bytes.values().map(|b| b.len() as u64).sum();
    let request = initial_reservation(proven.bytes.len() as u64, source_bytes, None);
    let reservation = {
        let lock = v3::ReservationLock::acquire(storage).map_err(|e| error(&e))?;
        let mut registry =
            v3::open_base_refs_registry(storage, family, &lock, None).map_err(|e| error(&e))?;
        super::eviction::DiskBudget::default()
            .admit_v3(&mut registry, request)
            .map_err(|e| error(&e))?
            .reservation
    };
    let built = (|| -> Result<(), String> {
        let (dir, _) = with_base_barrier(storage, family, |tx| {
            super::base_builder::register_base_in(tx, storage, family, &identity.scope)
        })
        .map_err(|e| error(&e))?;
        // A base that is already published keeps its proof unchanged: a
        // proof is written only before a scope's first publication.
        if super::ViewStore::existing_dir(dir.clone())
            .map(|store| store.current_generation_read_only())
            .transpose()
            .map_err(|e| error(&e))?
            .flatten()
            .is_some()
        {
            return Ok(());
        }
        super::verification::write_tree_proof(
            storage,
            family,
            &dir,
            &TreeProof {
                base_scope: identity.scope.clone(),
                identity_digest: to_hex(&identity.identity),
                membership_digest: to_hex(&identity.membership),
                object_format: proven.format.name().to_owned(),
                head_tree: proven
                    .manifest
                    .header()
                    .head_tree
                    .clone()
                    .unwrap_or_default(),
                root: guard.root.clone(),
                policy: guard.policy.digest.clone(),
                receipts: proven.receipts.clone(),
            },
        )
        .map_err(|e| error(&e))?;
        let mut live = crate::pins::LivePin::create_v3(storage, family, &identity.scope)
            .map_err(|e| error(&e))?;
        super::trigram::materialize_v3(
            storage,
            family,
            &mut proven.manifest,
            &proven.bytes,
            &mut live,
            policy,
        )
        .map_err(|e| e.reason)?;
        if !claim.verify_held().map_err(|e| error(&e))? {
            return Err("the build claim was taken over".into());
        }
        match super::base_builder::publish_base(storage, family, &proven.manifest, None)
            .map_err(|e| error(&e))?
        {
            super::base_builder::BasePublication::Published { .. } => Ok(()),
            super::base_builder::BasePublication::Lost { .. } => {
                Err("another publisher moved the base pointer".into())
            }
        }
    })();
    let released = (|| -> Result<(), String> {
        let lock = v3::ReservationLock::acquire(storage).map_err(|e| error(&e))?;
        let mut registry =
            v3::RegistryWriter::open_for_barrier(storage, family, &lock).map_err(|e| error(&e))?;
        registry.release(reservation).map_err(|e| error(&e))
    })();
    built?;
    released?;
    claim.mark_serving().map_err(|e| error(&e))
}

/// Adds the receipts of `durable`'s tree-keyed base to the checkout's own
/// generation evidence, as candidates for the same stat-reuse checks. A
/// receipt replaces an evidence record only when it records the same stat
/// (the same observation) or the path has no record. Returns the merged
/// candidates and the paths whose candidate is a receipt. A membership-keyed
/// base, or a tree-keyed base whose proof does not hold now, adds nothing.
pub(crate) fn with_receipts(
    mut evidence: BTreeMap<RelPath, Evidence>,
    durable: Option<&ComposedGeneration>,
    guard: Option<&BindGuard>,
    route: &V3Route,
) -> (BTreeMap<RelPath, Evidence>, BTreeSet<RelPath>) {
    let mut from_receipts = BTreeSet::new();
    let (Some(durable), Some(guard)) = (durable, guard) else {
        return (evidence, from_receipts);
    };
    let Some(header) = durable.base_header() else {
        return (evidence, from_receipts);
    };
    if super::base_refs::base_kind(&header.scope) != Some(BaseKind::Tree) {
        return (evidence, from_receipts);
    }
    let Some(base) = resolve_base(&route.storage, &route.family, &header.scope)
        .ok()
        .filter(|base| base.check_header(header).is_ok())
    else {
        return (evidence, from_receipts);
    };
    let Some(receipts) = super::verification::tree_receipt_evidence(&base, guard) else {
        return (evidence, from_receipts);
    };
    for (path, mut record) in receipts {
        if evidence
            .get(&path)
            .is_some_and(|existing| existing.stat != record.stat)
        {
            continue;
        }
        // Plane keys come from the checkout's own readiness; the reuse check
        // still requires each to be ready, expected and present.
        for plane in FamilyPlane::ALL {
            if let Some(PlaneState::Ready { key }) = durable.plane_readiness(&path, plane) {
                record.keys.insert(format!("{plane:?}"), key.clone());
            }
        }
        evidence.insert(path.clone(), record);
        from_receipts.insert(path);
    }
    (evidence, from_receipts)
}

#[cfg(test)]
#[path = "tree_base_tests.rs"]
mod tests;
