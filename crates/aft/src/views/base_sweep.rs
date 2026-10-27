//! The v3 family sweep of the base-reference protocol: marking, dangling-edge
//! records, bounded referrer reconciliation, the two-sweep base reclaim and
//! the plane-row sweep.
//!
//! The whole sweep runs under the storage-root reservation lock. Every v3
//! registry barrier and every v3 plane write takes that lock first, so no
//! referrer row, member row or plane row changes while the sweep marks.
//! Pointer compare-and-swaps and reader markers do not take the lock; the
//! protocol stays safe against them because:
//!
//! - a worker's `intent` row exists before its CAS, and an `intent` row whose
//!   owner is live is never touched, so a reference that lands after marking
//!   is still held by its row;
//! - rows written after the sweep began carry `updated_epoch >= S` and are not
//!   visited by this sweep;
//! - readers mark first and verify the pointer after, so a reader either sees
//!   a generation this sweep marked, or retries.
//!
//! Only I/O uncertainty aborts a sweep with nothing deleted. A dangling or
//! corrupt edge is counted, recorded in `strict_rebuild`, and the sweep goes
//! on.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, OptionalExtension, Transaction};

use crate::blob_store::v3::{self, PlaneRowCursor, RegistryWriter, ReservationLock};
use crate::pins::PinOwner;

use super::base_refs::{
    barrier, gc_epoch, BaseIdentity, BaseRefError, Result, KIND_BASE, REF_INTENT, REF_ORPHANED,
    REF_PUBLISHED,
};
use super::contracts::{boundary, DurabilityObserver, DurabilityStep};
use super::eviction::{v3_base_hold, Hold};
use super::manifest_v2::ManifestV2;
use super::registry::STATE_EVICTING;
use super::ViewStore;

/// Ruled referrer-reconciliation caps per family pass (pre-slice-4 ruling):
/// 256 rows or 25 ms, whichever comes first, cumulatively across
/// transactions.
pub const RECONCILE_ROWS: usize = 256;
pub const RECONCILE_TIME: Duration = Duration::from_millis(25);
/// Consecutive unreferenced sweeps before a base is reclaimed, mirroring
/// `MISSING_ROOT_SWEEPS`.
pub const UNREFERENCED_SWEEPS: i64 = 2;
/// Rows the plane-row sweep examines per visit before resuming next time.
pub const PLANE_ROWS_PER_VISIT: u64 = 100_000;

/// Bounds of one sweep. Tests shrink them to reach a bound with a few rows.
#[derive(Clone, Copy, Debug)]
pub struct SweepBounds {
    pub reconcile_rows: usize,
    pub reconcile_time: Duration,
    pub plane_rows: u64,
    pub deadline: Option<Instant>,
}

impl Default for SweepBounds {
    fn default() -> Self {
        Self {
            reconcile_rows: RECONCILE_ROWS,
            reconcile_time: RECONCILE_TIME,
            plane_rows: PLANE_ROWS_PER_VISIT,
            deadline: None,
        }
    }
}

/// An exact reference edge seen while marking.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    pub worker_scope: String,
    pub worker_generation: String,
    pub base_scope: String,
    pub membership_digest: String,
}

#[derive(Clone, Debug, Default)]
pub struct V3SweepReport {
    pub epoch: i64,
    pub marked_generations: usize,
    pub marked_keys: usize,
    pub valid_edges: usize,
    /// Edges to a missing, non-base, chained, evicting or different-membership
    /// base, counted on every sweep that sees them.
    pub dangling_base_edges: usize,
    pub reconciled_rows: usize,
    pub kept_rows: usize,
    pub deleted_rows: usize,
    pub orphaned_rows: usize,
    pub repaired_rows: usize,
    /// Every row older than the sweep was visited in this pass.
    pub reconcile_complete: bool,
    pub counted_bases: Vec<String>,
    pub reclaimed_bases: Vec<String>,
    pub held_bases: Vec<(String, String)>,
    pub plane_rows_examined: u64,
    pub plane_rows_deleted: u64,
    pub plane_rows_complete: bool,
    /// Why nothing was swept (no registry, a version-1 registry, or a live
    /// sweeper holding the lease).
    pub skipped: Option<String>,
}

struct Member {
    scope: String,
    kind: String,
    state: String,
    view_dir: PathBuf,
}

#[derive(Default)]
struct Marks {
    keys: HashSet<[u8; 32]>,
    generations: usize,
    edges: BTreeSet<Edge>,
    /// Each base member's current identity; `None` when its current manifest
    /// is not a valid base (a chain, a git_oid claim, a mismatched scope).
    base_current: BTreeMap<String, Option<BaseIdentity>>,
}

fn not_found(error: &super::ViewError) -> bool {
    match error {
        super::ViewError::IoAt { source, .. } | super::ViewError::Io(source) => {
            source.kind() == io::ErrorKind::NotFound
        }
        _ => false,
    }
}

/// Generations of one member directory that must stay readable: current,
/// read-marked by a live owner, and pinned by a live owner. Pinned key lists
/// are marked directly. Markers and pins of dead owners are reclaimed on the
/// way. An unreadable pin or an over-long listing is uncertainty.
fn protected_generations(
    view_dir: &Path,
    keys: &mut HashSet<[u8; 32]>,
) -> Result<BTreeSet<String>> {
    let mut generations = BTreeSet::new();
    if let Some(store) = ViewStore::existing_dir(view_dir.to_path_buf()) {
        if let Some(current) = store.current_generation_read_only()? {
            generations.insert(current);
        }
    }
    let cap = crate::storage_retention::TREE_ENTRIES;
    for entry in crate::storage_retention::bounded_dirs(&view_dir.join("readers"), cap)? {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(generation) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(BaseRefError::Io(io::Error::other(
                "non-UTF-8 reader directory",
            )));
        };
        if crate::root_cache::sweep_read_markers(view_dir, &generation).protected {
            generations.insert(generation);
        }
    }
    for entry in crate::storage_retention::bounded_dirs(&view_dir.join("pins"), cap)? {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata = crate::pins::read_metadata_strict(&path)
            .map_err(|error| BaseRefError::Io(io::Error::other(error.to_string())))?;
        let (metadata_path, keys_path) = crate::pins::pin_paths(view_dir, &metadata.generation);
        if metadata_path != path {
            return Err(BaseRefError::Io(io::Error::other(format!(
                "pin {} names another generation",
                path.display()
            ))));
        }
        if !crate::pins::owner_is_live(&metadata.owner) {
            let _ = std::fs::remove_file(&metadata_path);
            let _ = std::fs::remove_file(&keys_path);
            continue;
        }
        keys.extend(
            crate::pins::read_keys(&keys_path)
                .map_err(|error| BaseRefError::Io(io::Error::other(error.to_string())))?,
        );
        generations.insert(metadata.generation);
    }
    Ok(generations)
}

fn load(view_dir: &Path, generation: &str) -> Result<Option<ManifestV2>> {
    let Some(store) = ViewStore::existing_dir(view_dir.to_path_buf()) else {
        return Ok(None);
    };
    match store.load_manifest_v2(generation) {
        Ok(manifest) => Ok(Some(manifest)),
        // Nothing to mark, exactly as `mark_generation_if_present` treats a
        // missing v2 manifest (for example, a private name pinned before its
        // manifest was written).
        Err(error) if not_found(&error) => Ok(None),
        Err(super::ViewError::GenerationMismatch(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn mark_member(family: &str, member: &Member, marks: &mut Marks) -> Result<()> {
    let current = ViewStore::existing_dir(member.view_dir.clone())
        .map(|store| store.current_generation_read_only())
        .transpose()?
        .flatten();
    for generation in protected_generations(&member.view_dir, &mut marks.keys)? {
        let Some(manifest) = load(&member.view_dir, &generation)? else {
            continue;
        };
        marks.generations += 1;
        marks
            .keys
            .extend(manifest.ready_keys().map(|key| *key.as_bytes()));
        if member.kind == KIND_BASE {
            if current.as_deref() == Some(generation.as_str()) {
                let identity = BaseIdentity::of(family, &manifest)
                    .ok()
                    .filter(|identity| identity.scope == member.scope);
                marks.base_current.insert(member.scope.clone(), identity);
            }
            continue;
        }
        if let Some(header) = manifest.base_header() {
            marks.edges.insert(Edge {
                worker_scope: member.scope.clone(),
                worker_generation: generation,
                base_scope: header.scope.clone(),
                membership_digest: header.membership_digest.clone(),
            });
        }
    }
    Ok(())
}

/// Why an edge protects nothing, or `None` for a valid edge.
fn dangling_reason(
    edge: &Edge,
    members: &BTreeMap<String, &Member>,
    marks: &Marks,
) -> Option<&'static str> {
    let Some(base) = members.get(&edge.base_scope) else {
        return Some("base_not_registered");
    };
    if base.kind != KIND_BASE {
        return Some("not_a_base");
    }
    if base.state == STATE_EVICTING {
        return Some("base_evicting");
    }
    match marks.base_current.get(&edge.base_scope) {
        None => Some("base_unpublished"),
        Some(None) => Some("base_corrupt"),
        Some(Some(identity))
            if crate::blob_store::v2::to_hex(&identity.membership) != edge.membership_digest =>
        {
            Some("membership_changed")
        }
        Some(Some(_)) => None,
    }
}

/// Runs one sweep, taking the storage-root reservation lock.
pub fn sweep_family(
    storage: &Path,
    family: &str,
    observer: Option<&dyn DurabilityObserver>,
) -> Result<V3SweepReport> {
    // A standalone sweep treats `storage` as both the artifact and the
    // durable root, and declares it so a reclaimed base directory may be
    // removed. The retention collector declares split roots itself.
    crate::root_cache::register_durable_owner(&crate::root_cache::StoragePaths::same(storage))?;
    let lock = ReservationLock::acquire(storage)?;
    sweep_family_locked(
        storage,
        family,
        &lock,
        observer,
        SweepBounds::default(),
        None,
    )
}

/// Runs one sweep under the caller's reservation lock. `plane_cursor` is the
/// file that keeps the plane-row sweep's resume position between visits; a
/// sweep without one restarts the plane scan from the beginning.
pub fn sweep_family_locked(
    storage: &Path,
    family: &str,
    lock: &ReservationLock,
    observer: Option<&dyn DurabilityObserver>,
    bounds: SweepBounds,
    plane_cursor: Option<&Path>,
) -> Result<V3SweepReport> {
    let mut report = V3SweepReport::default();
    if !v3::family_dir(storage, family)?
        .join("members.sqlite")
        .try_exists()?
    {
        report.skipped = Some("no_registry".into());
        return Ok(report);
    }
    let mut registry = RegistryWriter::open_for_barrier(storage, family, lock)?;
    if registry.registry_version()? < v3::REGISTRY_BASE_REFS_VERSION {
        report.skipped = Some("registry_without_base_refs".into());
        return Ok(report);
    }
    let me = super::registry::current_owner();
    // Begin: take the sweep lease and raise the epoch. Everything written
    // from here on carries `updated_epoch >= S`.
    let begun = barrier(&mut registry, |tx| {
        let lease: (Option<u32>, Option<i64>) = tx.query_row(
            "SELECT lease_pid,lease_start FROM sweep_state WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if let (Some(pid), Some(start)) = lease {
            let owner = PinOwner {
                pid,
                start_time: start as u64,
            };
            if owner != me && crate::pins::owner_is_live(&owner) {
                return Ok(None);
            }
        }
        tx.execute(
            "UPDATE sweep_state SET gc_epoch=gc_epoch+1,lease_pid=?1,lease_start=?2 WHERE singleton=1",
            params![me.pid, me.start_time as i64],
        )?;
        Ok(Some(gc_epoch(tx)?))
    })?;
    let Some(epoch) = begun else {
        report.skipped = Some("sweep_lease_held".into());
        return Ok(report);
    };
    report.epoch = epoch;
    let result = sweep_inner(
        storage,
        family,
        &mut registry,
        epoch,
        observer,
        bounds,
        plane_cursor,
        &mut report,
    );
    let released = barrier(&mut registry, |tx| {
        tx.execute(
            "UPDATE sweep_state SET lease_pid=NULL,lease_start=NULL WHERE singleton=1 AND lease_pid=?1",
            [me.pid],
        )?;
        Ok(())
    });
    result?;
    released?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn sweep_inner(
    storage: &Path,
    family: &str,
    registry: &mut RegistryWriter<'_>,
    epoch: i64,
    observer: Option<&dyn DurabilityObserver>,
    bounds: SweepBounds,
    plane_cursor: Option<&Path>,
    report: &mut V3SweepReport,
) -> Result<()> {
    let members: Vec<Member> = barrier(registry, |tx| {
        let mut statement =
            tx.prepare("SELECT scope,kind,state,view_dir FROM members ORDER BY scope")?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?
    .into_iter()
    .filter_map(|(scope, kind, state, stored)| {
        // A row naming any other directory is never followed or deleted.
        let dir = v3::view_dir(storage, family, &scope).ok()?;
        (Path::new(&stored) == dir).then_some(Member {
            scope,
            kind,
            state,
            view_dir: dir,
        })
    })
    .collect();

    // Finish base reclaims a crash interrupted after `evicting` committed.
    for member in members
        .iter()
        .filter(|member| member.kind == KIND_BASE && member.state == STATE_EVICTING)
    {
        finish_base_reclaim(registry, member)?;
        report.reclaimed_bases.push(member.scope.clone());
    }

    // Mark outside the barrier. Any I/O uncertainty aborts with nothing
    // deleted.
    let mut marks = Marks::default();
    for member in members
        .iter()
        .filter(|member| member.state != STATE_EVICTING)
    {
        mark_member(family, member, &mut marks)?;
    }
    report.marked_generations = marks.generations;
    report.marked_keys = marks.keys.len();
    let by_scope: BTreeMap<String, &Member> = members
        .iter()
        .map(|member| (member.scope.clone(), member))
        .collect();
    let mut valid = BTreeSet::new();
    let mut dangling = Vec::new();
    for edge in &marks.edges {
        match dangling_reason(edge, &by_scope, &marks) {
            None => {
                valid.insert(edge.clone());
            }
            Some(reason) => dangling.push((edge.clone(), reason)),
        }
    }
    report.valid_edges = valid.len();
    report.dangling_base_edges = dangling.len();
    if !dangling.is_empty() {
        barrier(registry, |tx| {
            for (edge, reason) in &dangling {
                tx.execute(
                    "INSERT INTO strict_rebuild(worker_scope,worker_generation,reason,recorded_epoch) VALUES(?1,?2,?3,?4) ON CONFLICT(worker_scope,worker_generation) DO UPDATE SET reason=excluded.reason,recorded_epoch=excluded.recorded_epoch",
                    params![edge.worker_scope, edge.worker_generation, reason, epoch],
                )?;
            }
            tx.execute(
                "UPDATE sweep_state SET dangling_base_edges=dangling_base_edges+?1 WHERE singleton=1",
                [dangling.len() as i64],
            )?;
            Ok(())
        })?;
    }

    reconcile(registry, epoch, &valid, bounds, report)?;
    repair_missing(registry, epoch, &valid, report)?;
    let referenced: BTreeSet<&str> = valid.iter().map(|edge| edge.base_scope.as_str()).collect();
    for member in members
        .iter()
        .filter(|member| member.kind == KIND_BASE && member.state != STATE_EVICTING)
    {
        count_or_reclaim(registry, member, &referenced, observer, report)?;
    }

    // Plane rows last: the markers and pins listed above may have been the
    // only protection of rows that a reclaimed base named.
    let mut cursor = plane_cursor
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<PlaneRowCursor>(&bytes).ok())
        .unwrap_or_default();
    let swept = registry.sweep_plane_rows(
        epoch,
        &marks.keys,
        &mut cursor,
        bounds.plane_rows,
        bounds.deadline,
    )?;
    report.plane_rows_examined = swept.examined;
    report.plane_rows_deleted = swept.deleted;
    report.plane_rows_complete = swept.complete;
    if let Some(path) = plane_cursor {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&cursor).map_err(io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, path)?;
    }
    Ok(())
}

type Row = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    u32,
    i64,
);

fn row_key(row: &Row) -> String {
    format!("{}\u{1f}{}\u{1f}{}", row.1, row.0, row.2)
}

/// Visits `base_refs` rows older than the sweep (`updated_epoch < S`) in key
/// order from the saved position, within the ruled row and time caps. Rows
/// written during this sweep are never visited, so never deleted by it.
fn reconcile(
    registry: &mut RegistryWriter<'_>,
    epoch: i64,
    valid: &BTreeSet<Edge>,
    bounds: SweepBounds,
    report: &mut V3SweepReport,
) -> Result<()> {
    let started = Instant::now();
    let saved: Option<String> = barrier(registry, |tx| {
        Ok(tx.query_row(
            "SELECT reconcile_cursor FROM sweep_state WHERE singleton=1",
            [],
            |r| r.get(0),
        )?)
    })?;
    // One lap: from the saved position to the end, then from the start up to
    // and including the saved position.
    let mut after = saved.clone().unwrap_or_default();
    let mut wrapped = saved.is_none();
    let mut complete = false;
    let edge_of = |row: &Row| Edge {
        worker_scope: row.0.clone(),
        worker_generation: row.4.clone().unwrap_or_default(),
        base_scope: row.1.clone(),
        membership_digest: row.3.clone(),
    };
    // Any valid edge for this worker/base/membership: the repair target for a
    // dead owner's row, whose generation the row itself may not record.
    let repair_target = |row: &Row| -> Option<String> {
        valid
            .iter()
            .filter(|edge| {
                edge.worker_scope == row.0
                    && edge.base_scope == row.1
                    && edge.membership_digest == row.3
            })
            .map(|edge| edge.worker_generation.clone())
            .next_back()
    };
    while report.reconciled_rows < bounds.reconcile_rows
        && started.elapsed() < bounds.reconcile_time
    {
        let room = (bounds.reconcile_rows - report.reconciled_rows).min(64);
        let stop_at = (wrapped).then(|| saved.clone()).flatten();
        let batch_started = Instant::now();
        let done = barrier(registry, |tx| {
            let rows: Vec<Row> = tx
                .prepare(
                    "SELECT worker_scope,base_scope,nonce,membership_digest,worker_generation,state,owner_pid,owner_start FROM base_refs WHERE updated_epoch<?1 AND (base_scope||char(31)||worker_scope||char(31)||nonce)>?2 ORDER BY (base_scope||char(31)||worker_scope||char(31)||nonce) LIMIT ?3",
                )?
                .query_map(params![epoch, after, room as i64], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            if rows.is_empty() {
                return Ok(true);
            }
            for row in rows {
                let key = row_key(&row);
                if stop_at.as_deref().is_some_and(|stop| key.as_str() > stop) {
                    return Ok(true);
                }
                after = key;
                report.reconciled_rows += 1;
                let id = params![row.0, row.1, row.2];
                let owner_live = crate::pins::owner_is_live(&PinOwner {
                    pid: row.6,
                    start_time: row.7 as u64,
                });
                match row.5.as_str() {
                    REF_PUBLISHED => {
                        let edge = edge_of(&row);
                        // Coalesce duplicate acquisitions of one edge: keep
                        // the first refreshed row, retire the rest.
                        let duplicate: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM base_refs WHERE worker_scope=?1 AND base_scope=?2 AND worker_generation=?3 AND membership_digest=?4 AND state='published' AND updated_epoch>=?5)",
                            params![row.0, row.1, edge.worker_generation, row.3, epoch],
                            |r| r.get(0),
                        )?;
                        if valid.contains(&edge) && !duplicate {
                            tx.execute(
                                "UPDATE base_refs SET updated_epoch=?4 WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3",
                                params![row.0, row.1, row.2, epoch],
                            )?;
                            report.kept_rows += 1;
                        } else {
                            tx.execute(
                                "DELETE FROM base_refs WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3",
                                id,
                            )?;
                            report.deleted_rows += 1;
                        }
                    }
                    REF_INTENT if owner_live => report.kept_rows += 1,
                    REF_INTENT | REF_ORPHANED => match repair_target(&row) {
                        Some(generation) => {
                            tx.execute(
                                "UPDATE base_refs SET state='published',worker_generation=?4,updated_epoch=?5 WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3",
                                params![row.0, row.1, row.2, generation, epoch],
                            )?;
                            report.repaired_rows += 1;
                        }
                        // A dead owner's intent may describe a CAS that landed
                        // after this sweep marked its worker. Keep it one more
                        // sweep, whose marking starts after the owner died.
                        None if row.5 == REF_INTENT => {
                            tx.execute(
                                "UPDATE base_refs SET state='orphaned',updated_epoch=?4 WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3",
                                params![row.0, row.1, row.2, epoch],
                            )?;
                            report.orphaned_rows += 1;
                        }
                        None => {
                            tx.execute(
                                "DELETE FROM base_refs WHERE worker_scope=?1 AND base_scope=?2 AND nonce=?3",
                                id,
                            )?;
                            report.deleted_rows += 1;
                        }
                    },
                    // Unknown states are kept: an unknown row is a hold.
                    _ => report.kept_rows += 1,
                }
                if batch_started.elapsed() >= bounds.reconcile_time {
                    break;
                }
            }
            Ok(false)
        })?;
        if done {
            if wrapped {
                complete = true;
                break;
            }
            wrapped = true;
            after.clear();
        }
    }
    report.reconcile_complete = complete;
    let next = (!complete).then_some(after);
    barrier(registry, |tx| {
        tx.execute(
            "UPDATE sweep_state SET reconcile_cursor=?1 WHERE singleton=1",
            [next],
        )?;
        Ok(())
    })
}

/// Inserts a `published` row for every valid edge that has none, so a
/// reference whose owner died before `ReferrerPublished` (and whose intent
/// row was already retired) is held again. Dangling observations never
/// create rows.
fn repair_missing(
    registry: &mut RegistryWriter<'_>,
    epoch: i64,
    valid: &BTreeSet<Edge>,
    report: &mut V3SweepReport,
) -> Result<()> {
    if valid.is_empty() {
        return Ok(());
    }
    let me = super::registry::current_owner();
    barrier(registry, |tx| {
        for edge in valid {
            let present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM base_refs WHERE worker_scope=?1 AND base_scope=?2 AND worker_generation=?3 AND membership_digest=?4 AND state='published')",
                params![edge.worker_scope, edge.base_scope, edge.worker_generation, edge.membership_digest],
                |r| r.get(0),
            )?;
            if present {
                continue;
            }
            tx.execute(
                "INSERT INTO base_refs(worker_scope,base_scope,nonce,membership_digest,worker_generation,state,owner_pid,owner_start,updated_epoch) VALUES(?1,?2,?3,?4,?5,'published',?6,?7,?8)",
                params![
                    edge.worker_scope,
                    edge.base_scope,
                    format!("repair-{}", super::base_refs::fresh_nonce()),
                    edge.membership_digest,
                    edge.worker_generation,
                    me.pid,
                    me.start_time as i64,
                    epoch
                ],
            )?;
            report.repaired_rows += 1;
        }
        Ok(())
    })
}

/// Hysteresis counter and reclaim of one base. The barrier re-check is the
/// safety authority; the count only delays deletion by one sweep.
fn count_or_reclaim(
    registry: &mut RegistryWriter<'_>,
    member: &Member,
    referenced: &BTreeSet<&str>,
    observer: Option<&dyn DurabilityObserver>,
    report: &mut V3SweepReport,
) -> Result<()> {
    enum Step {
        Held(Hold),
        Counted,
        Reclaim,
    }
    let step = barrier(registry, |tx| {
        let hold = if referenced.contains(member.scope.as_str()) {
            Some(Hold::Referenced)
        } else {
            v3_base_hold(tx, &member.scope, &member.view_dir)
        };
        if let Some(hold) = hold {
            tx.execute(
                "DELETE FROM base_sweeps WHERE base_scope=?1",
                [&member.scope],
            )?;
            return Ok(Step::Held(hold));
        }
        let count: i64 = tx
            .query_row(
                "SELECT unreferenced_sweeps FROM base_sweeps WHERE base_scope=?1",
                [&member.scope],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0)
            + 1;
        if count < UNREFERENCED_SWEEPS {
            tx.execute(
                "INSERT INTO base_sweeps(base_scope,unreferenced_sweeps) VALUES(?1,?2) ON CONFLICT(base_scope) DO UPDATE SET unreferenced_sweeps=excluded.unreferenced_sweeps",
                params![member.scope, count],
            )?;
            return Ok(Step::Counted);
        }
        // Second consecutive unreferenced sweep: commit `evicting` in the
        // same transaction as the re-check.
        tx.execute(
            "UPDATE members SET state=?2 WHERE scope=?1 AND kind='base'",
            params![member.scope, STATE_EVICTING],
        )?;
        Ok(Step::Reclaim)
    })?;
    match step {
        Step::Held(hold) => report
            .held_bases
            .push((member.scope.clone(), hold.to_string())),
        Step::Counted => {
            report.counted_bases.push(member.scope.clone());
            boundary(observer, DurabilityStep::BaseUnreferencedCounted)?;
        }
        Step::Reclaim => {
            remove_pointer(&member.view_dir)?;
            boundary(observer, DurabilityStep::BaseReclaimBegun)?;
            finish_base_reclaim(registry, member)?;
            report.reclaimed_bases.push(member.scope.clone());
        }
    }
    Ok(())
}

/// Removes the base pointer first, so a binder that finds the directory
/// without a pointer treats the base as absent.
fn remove_pointer(view_dir: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        match std::fs::remove_file(view_dir.join(format!("{}{suffix}", super::POINTER_DATABASE))) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Removes an `evicting` base: pointer, directory, then its rows and member
/// row in one transaction. Safe to repeat after a crash.
fn finish_base_reclaim(registry: &mut RegistryWriter<'_>, member: &Member) -> Result<()> {
    remove_pointer(&member.view_dir)?;
    super::registry::remove_view_dir(&member.view_dir)?;
    barrier(registry, |tx| {
        delete_base_rows(tx, &member.scope)?;
        Ok(())
    })
}

pub(crate) fn delete_base_rows(tx: &Transaction<'_>, scope: &str) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM base_refs WHERE base_scope=?1", [scope])?;
    tx.execute("DELETE FROM base_builds WHERE base_scope=?1", [scope])?;
    tx.execute("DELETE FROM base_sweeps WHERE base_scope=?1", [scope])?;
    tx.execute(
        "DELETE FROM members WHERE scope=?1 AND state=?2",
        params![scope, STATE_EVICTING],
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "base_sweep_tests.rs"]
mod tests;
