//! The family GC protocol for per-checkout (v2) stores.
//!
//! One sweep, under the family's one-sweeper lease:
//!
//! 1. bump the registry's GC epoch to `S` and raise every plane store to `S`;
//! 2. mark, for **every** registered member: its current manifest, every
//!    generation protected by a pin or read marker in its `pins/` and
//!    `readers/`, and the keys listed by its assembly and live pins;
//! 3. delete each unmarked row with `DELETE … WHERE full_key = ? AND
//!    ref_epoch < S`, each in its own IMMEDIATE transaction, then return the
//!    freed pages;
//! 4. under the registry's write lock (the handoff barrier), count sweeps for
//!    members whose checkout root is gone and that have no protection of any
//!    class; the second such consecutive sweep re-checks every protection
//!    under the barrier and then removes the member and its view directory.
//!    Assembly and live pins are created under the same barrier, so a pin
//!    cannot appear between that re-check and the removal.
//!
//! Why deletion is safe: any work W that relies on key K made its protection
//! durable, then touched K. If the touch committed after the epoch bump, K's
//! `ref_epoch >= S` and the conditional delete does nothing. If the delete
//! committed first, the touch reports K missing and W puts it again. If the
//! touch committed before the bump, W's protection was durable before marking
//! began, so marking saw it. The stores' SQLite write locks order these
//! events; no wall clock is involved.
//!
//! Any error reading a member's manifest, `pins/`, `readers/` or a pin's keys,
//! and any malformed pin, aborts the sweep with nothing deleted. Pins are
//! reclaimed only when their owner process is gone, never by age alone, so a
//! stopped but live owner keeps its protection.
//!
//! This module does not decide *what* to evict under disk pressure; the byte
//! budget here only bounds how much unmarked garbage a sweep collects. Disk
//! limits and least-recently-used eviction of whole views live in
//! `views::eviction`, which runs this sweep afterwards so that keys only an
//! evicted view referenced are reclaimed too.
//!
//! Every walk is bounded: the rows a sweep considers are capped in SQL, the
//! entries of a member's `pins/` and `readers/` are capped at the iterator,
//! and an optional deadline is checked inside each loop. A marking walk that
//! reaches a bound aborts the sweep with nothing deleted, because a partial
//! mark could miss a protected key; a deletion walk that reaches one simply
//! stops, which only leaves garbage for the next sweep.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Instant;

use rusqlite::{params, OptionalExtension};

use crate::blob_store::v2::{
    plane_path, segment_path, FamilyPlane, FamilyStore, StoreError, StoreWriteAccess,
};
use crate::pins::{self, PinError};
use crate::views::registry::{
    read_members, FamilyRegistry, MemberRecord, RegistryError, MISSING_ROOT_SWEEPS,
};
use crate::views::ViewStore;

#[derive(Debug)]
pub enum FamilySweepError {
    /// Another live process holds the family's sweep lease.
    Busy,
    /// Protection state could not be read with certainty; nothing was deleted.
    Uncertain(String),
    /// Marking reached the sweep's deadline or an entry bound before it
    /// covered every member; nothing was deleted.
    Bounded(String),
    Registry(RegistryError),
    Store(StoreError),
    Io(std::io::Error),
}

impl fmt::Display for FamilySweepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(f, "another process is sweeping this family"),
            Self::Uncertain(reason) => {
                write!(f, "sweep aborted with nothing deleted: {reason}")
            }
            Self::Bounded(reason) => {
                write!(
                    f,
                    "sweep stopped before marking finished, nothing deleted: {reason}"
                )
            }
            Self::Registry(error) => write!(f, "{error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "family sweep I/O error: {error}"),
        }
    }
}

impl std::error::Error for FamilySweepError {}

impl From<RegistryError> for FamilySweepError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}
impl From<StoreError> for FamilySweepError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
impl From<std::io::Error> for FamilySweepError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// How much unmarked garbage one sweep collects.
#[derive(Clone, Copy, Debug)]
pub struct FamilySweepPolicy {
    /// Unmarked rows are deleted, oldest epoch first, while a plane's payload
    /// bytes exceed this budget. Zero collects every unmarked row.
    pub byte_budget: u64,
}

/// Bounds on the walks of one sweep.
#[derive(Clone, Copy, Debug)]
pub struct SweepBounds {
    /// Rows (and segments) of one store considered for deletion, oldest
    /// epoch first. Rows past the bound wait for a later sweep.
    pub max_rows_per_store: usize,
    /// Entries read from one member's `pins/` or `readers/` directory. A
    /// directory with more entries aborts the sweep, since marking must
    /// see every protection.
    pub max_entries_per_dir: usize,
    /// Checked inside every loop. Marking past it aborts with nothing
    /// deleted; deletion past it stops early.
    pub deadline: Option<Instant>,
}

impl Default for SweepBounds {
    fn default() -> Self {
        Self {
            max_rows_per_store: 1_000_000,
            max_entries_per_dir: 100_000,
            deadline: None,
        }
    }
}

impl SweepBounds {
    fn past_deadline(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

/// Points in a sweep where tests can pause it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepStep {
    EpochRaised,
    Marked,
    Deleted,
    /// A missing-root member is due for removal. The sweep has released the
    /// registry barrier after counting it and has not yet taken it again to
    /// re-check its protection and remove it.
    RemovalPending,
    /// A segment's row is deleted inside its still-open transaction and its
    /// file is not unlinked yet.
    SegmentRowDeleted,
    /// That segment's file is unlinked and the transaction has not committed.
    SegmentFileUnlinked,
}

pub trait SweepObserver {
    fn reached(&self, step: SweepStep);
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FamilySweepReport {
    pub epoch: u64,
    pub marked_keys: usize,
    pub deleted_blobs: usize,
    pub deleted_bytes: u64,
    pub deleted_segments: usize,
    pub reclaimed_pins: usize,
    pub reclaimed_readers: usize,
    /// Members whose root is gone but that are kept this sweep.
    pub missing_root_retained: Vec<String>,
    /// Members removed with their view directories.
    pub deregistered: Vec<String>,
    /// True when deletion stopped at a row bound or the deadline.
    pub stopped_early: bool,
    /// True when a store's freed space could not be fully returned to the
    /// filesystem because another connection held its write-ahead log; the
    /// next sweep returns it.
    pub reclaim_deferred: bool,
}

/// What one member contributes to marking.
#[derive(Debug, Default)]
struct MemberMarks {
    keys: BTreeSet<[u8; 32]>,
    reclaimed_pins: usize,
}

/// Runs one family sweep. `requested_by` names the view on whose behalf the
/// sweep runs; it does not narrow marking, which always covers every member.
pub fn sweep_family(
    registry: &FamilyRegistry,
    requested_by: Option<&str>,
    policy: FamilySweepPolicy,
    observer: Option<&dyn SweepObserver>,
) -> Result<FamilySweepReport, FamilySweepError> {
    sweep_family_bounded(
        registry,
        requested_by,
        policy,
        SweepBounds::default(),
        observer,
    )
}

/// [`sweep_family`] with explicit [`SweepBounds`]: a cap on the rows of each
/// store it considers, a cap on the entries it reads from each member's
/// `pins/` and `readers/`, and a deadline. Background passes use it so one
/// sweep of a very large family cannot run unbounded.
pub fn sweep_family_bounded(
    registry: &FamilyRegistry,
    requested_by: Option<&str>,
    policy: FamilySweepPolicy,
    bounds: SweepBounds,
    observer: Option<&dyn SweepObserver>,
) -> Result<FamilySweepReport, FamilySweepError> {
    let _ = requested_by;
    let lease = registry.begin_sweep()?.ok_or(FamilySweepError::Busy)?;
    let epoch = lease.epoch();
    let mut report = FamilySweepReport {
        epoch,
        ..FamilySweepReport::default()
    };
    let access = StoreWriteAccess::for_registered_view(registry.storage(), registry.family());
    let mut stores = Vec::new();
    for plane in FamilyPlane::ALL {
        if plane_path(registry.storage(), registry.family(), plane)?.is_file() {
            let store = FamilyStore::open(&access, plane)?;
            store.raise_epoch(epoch)?;
            stores.push(store);
        }
    }
    observe(observer, SweepStep::EpochRaised);

    report.reclaimed_readers = registry.reclaim_dead_readers()?;
    let members = registry.members_bounded(8192)?;
    let mut marked = BTreeSet::new();
    for member in &members {
        if bounds.past_deadline() {
            return Err(FamilySweepError::Bounded(format!(
                "deadline reached before marking {}",
                member.scope
            )));
        }
        let marks =
            mark_member(member, registry.family(), &bounds, true).map_err(
                |failure| match failure {
                    MarkFailure::Uncertain(reason) => {
                        FamilySweepError::Uncertain(format!("{}: {reason}", member.scope))
                    }
                    MarkFailure::Bounded(reason) => {
                        FamilySweepError::Bounded(format!("{}: {reason}", member.scope))
                    }
                },
            )?;
        report.reclaimed_pins += marks.reclaimed_pins;
        marked.extend(marks.keys);
    }
    report.marked_keys = marked.len();
    observe(observer, SweepStep::Marked);

    for store in &stores {
        delete_unmarked(
            registry,
            store,
            &marked,
            epoch,
            policy,
            &bounds,
            observer,
            &mut report,
        )?;
    }
    observe(observer, SweepStep::Deleted);

    for member in &members {
        if bounds.past_deadline() {
            report.stopped_early = true;
            break;
        }
        deregister_if_missing(registry, member, observer, &mut report)?;
    }
    lease.release();
    Ok(report)
}

/// Every key and segment one member relies on (its current generation, the
/// generations its pins and read markers protect, and the keys its pins
/// list), read without changing anything. Disk accounting uses this to count
/// a key shared by several views once and to tell which keys only one view
/// holds. An `Err` is uncertainty, like in the sweep.
pub(crate) fn member_references(
    member: &MemberRecord,
    family: &str,
    bounds: &SweepBounds,
) -> Result<BTreeSet<[u8; 32]>, String> {
    mark_member(member, family, bounds, false)
        .map(|marks| marks.keys)
        .map_err(|failure| match failure {
            MarkFailure::Uncertain(reason) | MarkFailure::Bounded(reason) => reason,
        })
}

/// Protection of any class for a view directory: a pin whose owner lives, or
/// a protected read or residency marker. `Err` means it could not be read,
/// which callers treat as protected.
pub(crate) fn view_has_protection(view_dir: &Path) -> Result<bool, String> {
    member_has_protection(view_dir)
}

fn observe(observer: Option<&dyn SweepObserver>, step: SweepStep) {
    if let Some(observer) = observer {
        observer.reached(step);
    }
}

#[allow(clippy::too_many_arguments)]
fn delete_unmarked(
    registry: &FamilyRegistry,
    store: &FamilyStore,
    marked: &BTreeSet<[u8; 32]>,
    epoch: u64,
    policy: FamilySweepPolicy,
    bounds: &SweepBounds,
    observer: Option<&dyn SweepObserver>,
    report: &mut FamilySweepReport,
) -> Result<(), FamilySweepError> {
    // The total comes from SQLite's own sums, not from the listing, so the
    // budget check stays right when the listing below is cut at its bound.
    let segments = store.segments()?;
    let mut total =
        store.usage()?.payload_bytes + segments.iter().map(|row| row.byte_len).sum::<u64>();
    let rows = store.rows_oldest(bounds.max_rows_per_store)?;
    let mut deleted_any = false;
    for row in rows {
        if total <= policy.byte_budget {
            break;
        }
        if bounds.past_deadline() {
            report.stopped_early = true;
            break;
        }
        // The epoch check happens in the DELETE itself, not here: a touch can
        // land between this listing and the delete.
        if marked.contains(row.key.as_bytes()) {
            continue;
        }
        if store.delete_if_unreferenced_since(&row.key, epoch)? {
            total = total.saturating_sub(row.payload_bytes);
            report.deleted_blobs += 1;
            report.deleted_bytes += row.payload_bytes;
            deleted_any = true;
        }
    }
    for segment in segments.into_iter().take(bounds.max_rows_per_store) {
        if total <= policy.byte_budget {
            break;
        }
        if bounds.past_deadline() {
            report.stopped_early = true;
            break;
        }
        if marked.contains(&segment.segment_id) {
            continue;
        }
        let file = segment_path(registry.storage(), registry.family(), &segment.segment_id)?;
        let step = |step: crate::blob_store::v2::SegmentDeletionStep| {
            observe(
                observer,
                match step {
                    crate::blob_store::v2::SegmentDeletionStep::RowDeleted => {
                        SweepStep::SegmentRowDeleted
                    }
                    crate::blob_store::v2::SegmentDeletionStep::FileUnlinked => {
                        SweepStep::SegmentFileUnlinked
                    }
                },
            )
        };
        if store.delete_segment_observed(&segment.segment_id, epoch, &file, &step)?
            == crate::blob_store::v2::SegmentDeletion::Deleted
        {
            total = total.saturating_sub(segment.byte_len);
            report.deleted_segments += 1;
            report.deleted_bytes += segment.byte_len;
        }
    }
    if deleted_any && !store.reclaim_space()? {
        report.reclaim_deferred = true;
    }
    Ok(())
}

enum MarkFailure {
    Uncertain(String),
    Bounded(String),
}

impl From<String> for MarkFailure {
    fn from(reason: String) -> Self {
        Self::Uncertain(reason)
    }
}

/// Marks everything one member relies on. Any uncertainty is an error.
/// With `reclaim`, pins and read markers of dead owners are removed on the
/// way, as a sweep does; without it nothing on disk changes.
fn mark_member(
    member: &MemberRecord,
    family: &str,
    bounds: &SweepBounds,
    reclaim: bool,
) -> Result<MemberMarks, MarkFailure> {
    let mut marks = MemberMarks::default();
    let view_dir = &member.view_dir;
    match fs::symlink_metadata(view_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(marks),
        Err(error) => return Err(format!("view directory unreadable: {error}").into()),
    }
    let store = ViewStore::existing_dir(view_dir.clone());
    if let Some(store) = &store {
        let current = store
            .current_generation()
            .map_err(|error| format!("pointer unreadable: {error}"))?;
        if let Some(generation) = current {
            mark_generation(store, &generation, &mut marks)?;
        }
    }
    mark_pins(
        view_dir,
        family,
        store.as_ref(),
        bounds,
        reclaim,
        &mut marks,
    )?;
    mark_readers(view_dir, store.as_ref(), bounds, reclaim, &mut marks)?;
    Ok(marks)
}

/// The entries of a protection directory, at most `bounds` of them. More
/// entries than the bound, or the deadline, is a [`MarkFailure::Bounded`]:
/// a mark that skipped an entry could miss a protection.
fn bounded_entries(
    dir: &Path,
    what: &str,
    bounds: &SweepBounds,
) -> Result<Option<Vec<fs::DirEntry>>, MarkFailure> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{what} unreadable: {error}").into()),
    };
    let mut result = Vec::new();
    for entry in entries.take(bounds.max_entries_per_dir.saturating_add(1)) {
        if bounds.past_deadline() {
            return Err(MarkFailure::Bounded(format!(
                "deadline reached reading {what}"
            )));
        }
        result.push(entry.map_err(|error| format!("{what} unreadable: {error}"))?);
    }
    if result.len() > bounds.max_entries_per_dir {
        return Err(MarkFailure::Bounded(format!(
            "{what} holds more than {} entries",
            bounds.max_entries_per_dir
        )));
    }
    Ok(Some(result))
}

fn mark_generation(
    store: &ViewStore,
    generation: &str,
    marks: &mut MemberMarks,
) -> Result<(), String> {
    let manifest = store
        .load_manifest_v2(generation)
        .map_err(|error| format!("manifest {generation} unreadable: {error}"))?;
    marks
        .keys
        .extend(manifest.ready_keys().map(|key| *key.as_bytes()));
    if let Some(segment) = manifest.segment_id() {
        marks.keys.insert(segment);
    }
    Ok(())
}

fn mark_pins(
    view_dir: &Path,
    family: &str,
    store: Option<&ViewStore>,
    bounds: &SweepBounds,
    reclaim: bool,
    marks: &mut MemberMarks,
) -> Result<(), MarkFailure> {
    let pins_dir = view_dir.join("pins");
    let Some(entries) = bounded_entries(&pins_dir, "pins", bounds)? else {
        return Ok(());
    };
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata = pins::read_metadata_strict(&path)
            .map_err(|error| format!("malformed pin {}: {error}", path.display()))?;
        let (metadata_path, keys_path) = pins::pin_paths(view_dir, &metadata.generation);
        if metadata_path != path || metadata.family != family {
            return Err(format!(
                "pin {} does not belong to this view and family",
                path.display()
            )
            .into());
        }
        if !pins::owner_is_live(&metadata.owner) {
            if reclaim {
                let _ = fs::remove_file(&metadata_path);
                let _ = fs::remove_file(&keys_path);
                marks.reclaimed_pins += 1;
            }
            continue;
        }
        let keys = pins::read_keys(&keys_path).map_err(|error: PinError| {
            format!("pin keys {} unreadable: {error}", keys_path.display())
        })?;
        marks.keys.extend(keys);
        if let Some(store) = store {
            mark_generation_if_present(store, &metadata.generation, marks)?;
        }
    }
    Ok(())
}

fn mark_readers(
    view_dir: &Path,
    store: Option<&ViewStore>,
    bounds: &SweepBounds,
    reclaim: bool,
    marks: &mut MemberMarks,
) -> Result<(), MarkFailure> {
    let readers = view_dir.join("readers");
    let Some(entries) = bounded_entries(&readers, "readers", bounds)? else {
        return Ok(());
    };
    for entry in entries {
        let is_dir = entry
            .file_type()
            .map_err(|error| format!("reader entry unreadable: {error}"))?
            .is_dir();
        if !is_dir {
            continue;
        }
        let Some(generation) = entry.file_name().to_str().map(str::to_owned) else {
            return Err("reader directory with a non-UTF-8 name".to_string().into());
        };
        // Unreadable markers count as protected here, so uncertainty retains.
        let protected = if reclaim {
            crate::root_cache::sweep_read_markers(view_dir, &generation).protected
        } else {
            crate::root_cache::protected_read_marker_exists(view_dir, &generation)
        };
        if protected {
            if let Some(store) = store {
                mark_generation_if_present(store, &generation, marks)?;
            }
        }
    }
    Ok(())
}

fn mark_generation_if_present(
    store: &ViewStore,
    generation: &str,
    marks: &mut MemberMarks,
) -> Result<(), String> {
    let path = store
        .manifest_path(generation)
        .map_err(|error| error.to_string())?;
    match fs::symlink_metadata(&path) {
        Ok(_) => mark_generation(store, generation, marks),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("manifest {generation} unreadable: {error}")),
    }
}

/// True when the root is certainly gone. An unreadable parent is not "gone".
fn root_is_missing(member: &MemberRecord) -> bool {
    match &member.root {
        Some(root) => matches!(
            fs::symlink_metadata(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        None => false,
    }
}

/// Protection of any class for a member, checked inside the barrier: live
/// pins (assembly, live or seed), and protected read or residency markers.
/// An error is uncertainty and keeps the member; so is a directory with more
/// entries than the walk reads.
fn member_has_protection(view_dir: &Path) -> Result<bool, String> {
    let bounds = SweepBounds::default();
    let failure = |failure: MarkFailure| match failure {
        MarkFailure::Uncertain(reason) | MarkFailure::Bounded(reason) => reason,
    };
    let pins = bounded_entries(&view_dir.join("pins"), "pins", &bounds).map_err(failure)?;
    for entry in pins.into_iter().flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let metadata = pins::read_metadata_strict(&path).map_err(|error| error.to_string())?;
        if pins::owner_is_live(&metadata.owner) {
            return Ok(true);
        }
    }
    let readers =
        bounded_entries(&view_dir.join("readers"), "readers", &bounds).map_err(failure)?;
    for entry in readers.into_iter().flatten() {
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let generation = entry
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "non-UTF-8 reader directory".to_string())?;
        if crate::root_cache::sweep_read_markers(view_dir, &generation).protected {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Counts a sweep for a member whose root is gone and, on the sweep that
/// reaches [`MISSING_ROOT_SWEEPS`], removes it with its view directory.
///
/// This runs in two steps under the registry barrier. The first counts the
/// sweep and decides whether this sweep is the removing one. The second takes
/// the barrier again and checks everything once more (the member row, its
/// root, and every pin and marker) immediately before removing. Assembly and
/// live pins are created under the same barrier (see
/// `ViewRegistration::under_pin_barrier`), so a live owner either pinned
/// before the second check, which then keeps the member, or pins after the
/// removal and is refused because the member is no longer registered.
fn deregister_if_missing(
    registry: &FamilyRegistry,
    member: &MemberRecord,
    observer: Option<&dyn SweepObserver>,
    report: &mut FamilySweepReport,
) -> Result<(), FamilySweepError> {
    let missing = root_is_missing(member)
        && member.root.as_ref().is_some_and(|root| {
            crate::storage_retention::missing_root_due(
                registry.storage(),
                root,
                member.last_bind_ms,
            )
        });
    let counted = registry.with_barrier(|tx| count_missing_sweep(tx, member, missing))?;
    let outcome = match counted {
        Deregistration::Due { recorded } => {
            observe(observer, SweepStep::RemovalPending);
            registry.with_barrier(|tx| remove_if_still_due(tx, &member.scope, recorded))?
        }
        other => other,
    };
    match outcome {
        Deregistration::Retained => report.missing_root_retained.push(member.scope.clone()),
        Deregistration::Removed => report.deregistered.push(member.scope.clone()),
        Deregistration::Present | Deregistration::Gone | Deregistration::Due { .. } => {}
    }
    Ok(())
}

/// The first step, inside the barrier: resets or advances the member's count
/// of consecutive missing-root sweeps, or reports that removal is due.
fn count_missing_sweep(
    tx: &rusqlite::Transaction<'_>,
    member: &MemberRecord,
    missing: bool,
) -> Result<Deregistration, RegistryError> {
    let recorded: Option<i64> = tx
        .query_row(
            "SELECT missing_sweeps FROM members WHERE scope = ?1",
            params![member.scope],
            |row| row.get(0),
        )
        .optional()?;
    let Some(recorded) = recorded else {
        return Ok(Deregistration::Gone);
    };
    if !missing {
        if recorded != 0 {
            reset_missing_sweeps(tx, &member.scope)?;
        }
        return Ok(Deregistration::Present);
    }
    if let Some(kept) = protection_outcome(tx, &member.scope, &member.view_dir)? {
        return Ok(kept);
    }
    let count = recorded.max(0) as u32 + 1;
    if count < MISSING_ROOT_SWEEPS {
        tx.execute(
            "UPDATE members SET missing_sweeps = ?2 WHERE scope = ?1",
            params![member.scope, i64::from(count)],
        )?;
        return Ok(Deregistration::Retained);
    }
    Ok(Deregistration::Due { recorded })
}

/// The second step, inside the barrier again: re-reads the member and
/// re-checks its root and every protection, then removes the view directory
/// and the member row. Holding the barrier across the check and the removal
/// is what keeps a registration or a new pin from landing in between.
fn remove_if_still_due(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    recorded: i64,
) -> Result<Deregistration, RegistryError> {
    let Some(current) = read_members(tx)?
        .into_iter()
        .find(|candidate| candidate.scope == scope)
    else {
        return Ok(Deregistration::Gone);
    };
    // A registration in between resets the count and may name a new root.
    if i64::from(current.missing_sweeps) != recorded {
        return Ok(Deregistration::Retained);
    }
    let storage = current
        .view_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent);
    if !root_is_missing(&current)
        || !storage
            .zip(current.root.as_ref())
            .is_some_and(|(storage, root)| {
                crate::storage_retention::missing_root_due(storage, root, current.last_bind_ms)
            })
    {
        reset_missing_sweeps(tx, scope)?;
        return Ok(Deregistration::Present);
    }
    if let Some(kept) = protection_outcome(tx, scope, &current.view_dir)? {
        return Ok(kept);
    }
    match crate::views::registry::remove_view_dir(&current.view_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Ok(Deregistration::Retained),
    }
    tx.execute("DELETE FROM members WHERE scope = ?1", params![scope])?;
    Ok(Deregistration::Removed)
}

/// `Some(Retained)` when the member has protection of any class (which also
/// resets its count) or its protection cannot be read (which keeps the count);
/// `None` when it certainly has none.
fn protection_outcome(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    view_dir: &Path,
) -> Result<Option<Deregistration>, RegistryError> {
    match member_has_protection(view_dir) {
        Ok(false) => Ok(None),
        Ok(true) => {
            reset_missing_sweeps(tx, scope)?;
            Ok(Some(Deregistration::Retained))
        }
        Err(_) => Ok(Some(Deregistration::Retained)),
    }
}

fn reset_missing_sweeps(tx: &rusqlite::Transaction<'_>, scope: &str) -> Result<(), RegistryError> {
    tx.execute(
        "UPDATE members SET missing_sweeps = 0 WHERE scope = ?1",
        params![scope],
    )?;
    Ok(())
}

enum Deregistration {
    Present,
    Retained,
    Removed,
    Gone,
    /// This sweep reaches the removal threshold; `recorded` is the count it
    /// read, so the removing step can tell whether anything changed since.
    Due {
        recorded: i64,
    },
}
