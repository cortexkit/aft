//! Disk limits for one repository family's per-checkout views, and
//! least-recently-used eviction of whole views to stay under them.
//!
//! Two resource domains are accounted separately and never added together:
//!
//! - the **view store**: every member's own view directory (pointer,
//!   manifests, derived callgraph databases, trigram overlays, pins and
//!   markers). Its limits are 4 GiB soft and 6 GiB hard per family. Above
//!   the soft limit, views without a live session are evicted, least
//!   recently bound first, until the store is back under it.
//! - the **family store**: the shared plane databases, trigram segments and
//!   the registry under `blobs/v2/<family>/`. Each key and segment lives
//!   there once however many views use it, so it is counted once. Its limits
//!   are 3 GiB soft and 4 GiB hard, on live bytes: file bytes minus the
//!   bytes inside its databases that hold no data (free pages, and a file
//!   tail a checkpoint has not cut off yet). When its file bytes are over the soft limit
//!   it first returns free pages to the filesystem, a bounded number per
//!   pass; above the soft limit in live bytes the family sweep also collects
//!   every unreferenced row.
//!
//! Nothing that is in use is deleted to meet a number. A view is never
//! evicted while a process holds a session on it, while any pin or protected
//! read or residency marker exists in it, while this process has one of its
//! SQLite files open, or while any of that cannot be read with certainty.
//! When what remains over a hard limit is all held that way, new work is not
//! admitted; the refusal names the domain and the held bytes.
//!
//! Evicting a view removes its member row and its view directory. Its next
//! registration starts it as a new view: the first load seeds from a sibling
//! or walks the checkout, exactly as for a checkout that was never bound.
//!
//! Every walk here is bounded at its iterator and checks the deadline inside
//! its loop; a walk cut short reports `complete: false` rather than a number
//! that looks exact.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{params, OptionalExtension};

use crate::blob_store::v2::{family_dir, FamilyPlane, FamilyStoreReader};
use crate::gc::family::{self, FamilySweepPolicy, FamilySweepReport, SweepBounds};

use super::registry::{
    has_live_session, remove_view_dir_with, FamilyRegistry, MemberRecord, RegistryError,
    RegistryResult, STATE_EVICTING,
};

const GIB: u64 = 1024 * 1024 * 1024;

/// View-store limits per repository family, as decided for this design: a
/// soft limit that starts eviction and a hard limit that stops admission.
pub const VIEW_STORE_SOFT_BYTES: u64 = 4 * GIB;
pub const VIEW_STORE_HARD_BYTES: u64 = 6 * GIB;
/// Family-store limits on live bytes: the bytes of every file (plane
/// databases with their write-ahead logs, segments and the registry) minus
/// the free pages inside the plane databases, which hold no data.
pub const FAMILY_STORE_SOFT_BYTES: u64 = 3 * GIB;
pub const FAMILY_STORE_HARD_BYTES: u64 = 4 * GIB;
/// Payload bytes the family sweep keeps unreferenced rows under while the
/// family store is below its soft limit. Unreferenced rows are often the
/// content of a branch a checkout will switch back to, so they are kept
/// while there is room.
pub const FAMILY_PAYLOAD_BUDGET_BYTES: u64 = 2 * GIB;

/// How often the runtime checks a bound family's limits.
pub const ENFORCEMENT_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How long one background enforcement pass may spend before it stops.
pub const ENFORCEMENT_DEADLINE: Duration = Duration::from_secs(60);

/// Entries one measurement walk reads in a view or family directory.
const MAX_WALK_ENTRIES: usize = 200_000;
/// How deep a view directory walk goes: the view directory itself, then
/// `pins/` and `readers/`, then the generation directories under `readers/`.
const MAX_WALK_DEPTH: usize = 3;

/// The G4c storage gap a refused v3 admission reports. A refusal has no
/// legacy fallback: the caller serves what v3 already holds and names the gap.
pub const V3_HARD_LIMIT_GAP: &str = "storage_gap:v3_family_hard_limit";

/// Why a v3 reservation was refused.
#[derive(Debug)]
pub enum V3Refusal {
    /// Committed usage plus the request would cross the family hard limit.
    HardLimit {
        usage: u64,
        request: u64,
        limit: u64,
    },
    /// The ledger refused it (device floor, namespace budget) or failed.
    Ledger(crate::blob_store::v3::Error),
}

impl fmt::Display for V3Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardLimit {
                usage,
                request,
                limit,
            } => write!(
                f,
                "{V3_HARD_LIMIT_GAP}: {usage} used + {request} requested > {limit}"
            ),
            Self::Ledger(error) => write!(f, "storage_gap:{error}"),
        }
    }
}

impl std::error::Error for V3Refusal {}

/// An admitted v3 reservation.
#[derive(Debug)]
pub struct V3Admission {
    pub reservation: crate::blob_store::v3::Reservation,
    /// Usage after this reservation is above the family target: the caller
    /// runs the family's v3 sweep so reclaimable bytes go before the next
    /// admission.
    pub over_target: bool,
}

impl DiskBudget {
    /// Admits a v3 reservation of `request` bytes against this family's
    /// limits: the hard limit refuses, the target (the payload budget) only
    /// asks for a sweep. Usage is the ledger's committed usage (last covering
    /// measurement, committed charges since, and every outstanding
    /// reservation), so concurrent builds are charged before they write.
    pub fn admit_v3(
        &self,
        registry: &mut crate::blob_store::v3::RegistryWriter<'_>,
        request: u64,
    ) -> Result<V3Admission, V3Refusal> {
        let usage = registry.accounting().map_err(V3Refusal::Ledger)?.usage();
        if usage.saturating_add(request) > self.family_store_hard {
            return Err(V3Refusal::HardLimit {
                usage,
                request,
                limit: self.family_store_hard,
            });
        }
        let reservation = registry.reserve(request).map_err(V3Refusal::Ledger)?;
        Ok(V3Admission {
            reservation,
            over_target: usage.saturating_add(request) > self.family_payload,
        })
    }

    /// Extends an admitted reservation before writing past it, under the same
    /// hard limit.
    pub fn extend_v3(
        &self,
        registry: &mut crate::blob_store::v3::RegistryWriter<'_>,
        admission: &V3Admission,
        additional: u64,
    ) -> Result<(), V3Refusal> {
        let usage = registry.accounting().map_err(V3Refusal::Ledger)?.usage();
        if usage.saturating_add(additional) > self.family_store_hard {
            return Err(V3Refusal::HardLimit {
                usage,
                request: additional,
                limit: self.family_store_hard,
            });
        }
        registry
            .extend(&admission.reservation, additional)
            .map_err(V3Refusal::Ledger)
    }
}

/// The limits applied to one family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiskBudget {
    pub view_store_soft: u64,
    pub view_store_hard: u64,
    pub family_store_soft: u64,
    pub family_store_hard: u64,
    pub family_payload: u64,
}

impl Default for DiskBudget {
    fn default() -> Self {
        Self {
            view_store_soft: VIEW_STORE_SOFT_BYTES,
            view_store_hard: VIEW_STORE_HARD_BYTES,
            family_store_soft: FAMILY_STORE_SOFT_BYTES,
            family_store_hard: FAMILY_STORE_HARD_BYTES,
            family_payload: FAMILY_PAYLOAD_BUDGET_BYTES,
        }
    }
}

/// Why a view cannot be evicted now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hold {
    /// A live process holds a session on the view.
    ActiveSession,
    /// A pin with a live owner, or a protected read or residency marker,
    /// protects one of its generations.
    Protected,
    /// This process has one of the view's SQLite files open.
    OpenInProcess,
    /// Its sessions, protection or files could not be read with certainty.
    Uncertain(String),
    /// A v3 base that a `base_refs` row, a live `base_builds` claim or this
    /// sweep's marked worker edges still reference.
    Referenced,
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActiveSession => write!(f, "active session"),
            Self::Protected => write!(f, "protected generation"),
            Self::OpenInProcess => write!(f, "open in this process"),
            Self::Uncertain(reason) => write!(f, "uncertain: {reason}"),
            Self::Referenced => write!(f, "referenced base"),
        }
    }
}

/// One member's view directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewUsage {
    pub scope: String,
    pub view_dir: PathBuf,
    /// File bytes under the view directory, each file once.
    pub bytes: u64,
    /// The member's most recent bind, or the end of its most recent session.
    pub last_bind_ms: u64,
    /// `None` when the view may be evicted.
    pub hold: Option<Hold>,
    /// An earlier eviction of this view began and did not finish.
    pub evicting: bool,
    /// False when the walk stopped at a bound or the deadline.
    pub complete: bool,
}

/// The family store's files, each counted once.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FamilyStoreUsage {
    /// Bytes of every file under `blobs/v2/<family>/`: plane databases with
    /// their write-ahead logs, segment files and the registry.
    pub file_bytes: u64,
    /// Bytes of the free pages inside the plane databases, with any tail of
    /// a main database file past its last page that a checkpoint has not cut
    /// off yet: part of `file_bytes`, but holding no data. The page counts
    /// are read on each plane store's own connection and the tail from the
    /// main file's length, only when a complete walk
    /// finds `file_bytes` over a family-store limit; below the limits live
    /// bytes are below them too, so measurement opens no plane store and
    /// this stays zero.
    pub free_bytes: u64,
    pub complete: bool,
}

impl FamilyStoreUsage {
    /// The bytes the family-store limits are compared with: file bytes minus
    /// `free_bytes`. Those are not counted because no row has to be
    /// deleted to give them back; a store over a limit only because of them
    /// returns them instead (see [`enforce`]).
    pub fn live_bytes(&self) -> u64 {
        self.file_bytes.saturating_sub(self.free_bytes)
    }
}

/// Disk use of one family, per domain.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiskUsage {
    pub views: Vec<ViewUsage>,
    /// Sum of every member's view directory bytes.
    pub view_store_bytes: u64,
    /// The part of `view_store_bytes` in views that cannot be evicted now.
    pub held_view_bytes: u64,
    pub family_store: FamilyStoreUsage,
    pub complete: bool,
}

impl DiskUsage {
    fn largest_view(&self) -> u64 {
        self.views.iter().map(|view| view.bytes).max().unwrap_or(0)
    }

    /// True when every view directory was walked to the end, so that
    /// `view_store_bytes` and `held_view_bytes` are totals and not a part.
    fn views_complete(&self) -> bool {
        self.views.iter().all(|view| view.complete)
    }
}

/// How the family store's keys divide among the views that rely on them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeyAccounting {
    /// Bytes of keys and segments that at least one view relies on, each
    /// counted once however many views rely on it.
    pub referenced_bytes: u64,
    /// The part of `referenced_bytes` that two or more views rely on.
    pub shared_bytes: u64,
    /// Per view, the bytes of keys no other view relies on: what a sweep
    /// could reclaim after that view alone is evicted.
    pub exclusive_bytes: BTreeMap<String, u64>,
    /// Bytes of stored keys no view relies on, which a sweep may collect.
    pub unreferenced_bytes: u64,
    pub complete: bool,
}

#[derive(Debug)]
pub enum EvictionError {
    Registry(RegistryError),
    Io(std::io::Error),
}

impl fmt::Display for EvictionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "view eviction I/O error: {error}"),
        }
    }
}

impl std::error::Error for EvictionError {}

impl From<RegistryError> for EvictionError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<std::io::Error> for EvictionError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::blob_store::v2::StoreError> for EvictionError {
    fn from(error: crate::blob_store::v2::StoreError) -> Self {
        Self::Registry(RegistryError::Store(error))
    }
}

/// Points in an eviction where tests can pause or kill the process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionStep {
    /// The member is marked `evicting` and nothing is deleted yet.
    Begun,
    /// The pointer is deleted; the rest of the view directory is not, and
    /// the transaction that removes the member has not committed.
    PointerRemoved,
}

pub trait EvictionObserver {
    fn reached(&self, step: EvictionStep);
}

/// What happened to one eviction candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvictionOutcome {
    Evicted {
        bytes: u64,
    },
    Held(Hold),
    /// The member was no longer registered.
    Gone,
    /// The member row is gone, or still `evicting`, but some files could not
    /// be removed (an open file on Windows); the next pass retries.
    Incomplete(String),
}

/// Options of one enforcement pass.
#[derive(Clone, Copy)]
pub struct EnforceOptions<'a> {
    pub deadline: Option<Instant>,
    pub observer: Option<&'a dyn EvictionObserver>,
}

impl Default for EnforceOptions<'_> {
    fn default() -> Self {
        Self {
            deadline: None,
            observer: None,
        }
    }
}

/// What one enforcement pass did.
#[derive(Clone, Debug, Default)]
pub struct EnforcementReport {
    pub before: DiskUsage,
    pub after: DiskUsage,
    /// Evicted views with the view-store bytes each one freed, in order.
    pub evicted: Vec<(String, u64)>,
    /// Candidates the pass looked at and kept, with the reason.
    pub held: Vec<(String, Hold)>,
    pub sweep: Option<FamilySweepReport>,
    /// Free pages returned before the sweep because the family store's file
    /// bytes were over its soft limit (see [`enforce`]).
    pub free_page_reclaim: crate::blob_store::reclaim::ReclaimReport,
    /// Why the family sweep did not run or deleted nothing.
    pub sweep_error: Option<String>,
    /// The named reasons new work would be queued after this pass, if any.
    pub over_hard: Vec<String>,
    /// True when the pass stopped at its deadline.
    pub stopped_early: bool,
}

fn walk_bounds(deadline: Option<Instant>) -> SweepBounds {
    SweepBounds {
        deadline,
        ..SweepBounds::default()
    }
}

fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

#[cfg(test)]
thread_local! {
    /// Directory walks started on this thread, so a test can prove that a
    /// path such as admission walks nothing.
    static TREE_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TREE_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The byte budget an enforcement pass on this thread gave the family
    /// sweep it started most recently. The sweep's report does not record
    /// its budget, so tests read it here.
    static LAST_SWEEP_BUDGET: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static TREE_LISTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn measured_metadata(path: &Path) -> std::io::Result<fs::Metadata> {
    #[cfg(test)]
    TREE_STATS.with(|stats| stats.set(stats.get() + 1));
    fs::symlink_metadata(path)
}

/// Published v1 and v2 generation artifacts are reconciled even if an
/// out-of-band writer changes them in place. SQLite files, pins, markers and
/// temporary files are never cached.
const SIZE_RECONCILE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

pub(super) struct ViewSizeCache {
    directory: Option<Vec<u64>>,
    reconciled: Option<Instant>,
    files: BTreeMap<PathBuf, (u64, u64)>,
    // Stable namespaces reuse the immutable total and list only mutable paths.
    snapshot: Option<(u64, Vec<PathBuf>)>,
    top_paths: BTreeSet<PathBuf>,
}

/// The size caches of one family's views, keyed by view directory. The
/// registry owns them (see `FamilyRegistry::view_size_caches`), so they are
/// dropped with the last handle on the registry instead of outliving the
/// family for the rest of the process.
pub(super) type ViewSizeCaches = BTreeMap<PathBuf, ViewSizeCache>;

#[cfg(test)]
thread_local! {
    /// View size caches alive on this thread, so a test can prove that a
    /// family's caches go away with its registry.
    static LIVE_SIZE_CACHES: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

impl Default for ViewSizeCache {
    fn default() -> Self {
        #[cfg(test)]
        LIVE_SIZE_CACHES.with(|live| live.set(live.get() + 1));
        Self {
            directory: None,
            reconciled: None,
            files: BTreeMap::new(),
            snapshot: None,
            top_paths: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
impl Drop for ViewSizeCache {
    fn drop(&mut self) {
        LIVE_SIZE_CACHES.with(|live| live.set(live.get() - 1));
    }
}

fn immutable_generation(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    [
        ("manifest-", ".json"),
        ("trigram-", ".bin"),
        ("derived-", ".ref"),
    ]
    .into_iter()
    .find_map(|(prefix, suffix)| {
        name.strip_prefix(prefix)
            .and_then(|name| name.strip_suffix(suffix))
            // Both formats write manifests once. The v1 publisher uses names
            // without a g2 prefix, so validate those alongside v2 names.
            .filter(|generation| super::validate_generation(generation).is_ok())
    })
}

fn directory_identity(metadata: &fs::Metadata) -> Option<Vec<u64>> {
    if !metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(vec![
            metadata.dev(),
            metadata.ino(),
            metadata.mode() as u64,
            metadata.ctime() as u64,
            metadata.ctime_nsec() as u64,
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
        ])
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        // Without a directory identity, prefer the uncached accounting path.
        None
    }
}

fn view_tree_bytes(
    dir: &Path,
    deadline: Option<Instant>,
    cache: &mut ViewSizeCache,
) -> std::io::Result<(u64, bool)> {
    let identity = measured_metadata(dir)
        .ok()
        .as_ref()
        .and_then(directory_identity);
    let reconcile = cache
        .reconciled
        .is_none_or(|last| last.elapsed() >= SIZE_RECONCILE_INTERVAL);
    let same_directory = identity
        .as_ref()
        .zip(cache.directory.as_ref())
        .is_some_and(|(current, previous)| current[..2] == previous[..2]);
    if !same_directory || reconcile {
        cache.files.clear();
        cache.reconciled = Some(Instant::now());
    }
    // Publication changes the namespace, not old generations. Re-list names
    // on that change, but validate old artifacts using readdir's inode instead
    // of throwing away every retained generation's measured length.
    if identity.is_none() || identity != cache.directory || reconcile || past(deadline) {
        cache.snapshot = None;
    }
    cache.directory = identity;
    let reused_snapshot = cache.snapshot.is_some();
    let result = tree_bytes_inner(dir, deadline, Some(cache), None);
    let after = measured_metadata(dir)
        .ok()
        .as_ref()
        .and_then(directory_identity);
    // Only artifacts with a durable manifest can be immutable. A namespace
    // change during the walk could otherwise pair an unfinished ref/overlay
    // with a manifest published after that file was measured.
    if !matches!(result, Ok((_, true))) || after != cache.directory {
        cache.files.clear();
        cache.snapshot = None;
    } else if !reused_snapshot {
        let manifests = cache
            .files
            .keys()
            .filter(|path| cache.top_paths.contains(*path))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| immutable_generation(path).map(str::to_owned))
            .collect::<BTreeSet<_>>();
        cache.files.retain(|path, _| {
            cache.top_paths.contains(path)
                && immutable_generation(path)
                    .is_some_and(|generation| manifests.contains(generation))
        });
        cache.snapshot = Some((
            cache
                .files
                .values()
                .fold(0u64, |total, (_, bytes)| total.saturating_add(*bytes)),
            cache
                .top_paths
                .iter()
                .filter(|path| !cache.files.contains_key(*path))
                .cloned()
                .collect(),
        ));
    }
    result
}

/// Total file bytes under `dir`, each file once, up to `MAX_WALK_DEPTH`
/// levels. Symlinks are not followed and count nothing.
fn tree_bytes(dir: &Path, deadline: Option<Instant>) -> std::io::Result<(u64, bool)> {
    tree_bytes_inner(dir, deadline, None, None)
}

/// Walks `dir` like [`tree_bytes`] but stops as soon as the counted bytes pass
/// `limit`, reporting the walk incomplete: a caller that only needs to know
/// whether a tree fits a budget gains nothing from counting the rest.
fn tree_bytes_within(
    dir: &Path,
    deadline: Option<Instant>,
    limit: u64,
) -> std::io::Result<(u64, bool)> {
    tree_bytes_inner(dir, deadline, None, Some(limit))
}

fn tree_bytes_inner(
    dir: &Path,
    deadline: Option<Instant>,
    mut cache: Option<&mut ViewSizeCache>,
    limit: Option<u64>,
) -> std::io::Result<(u64, bool)> {
    #[cfg(test)]
    TREE_WALKS.with(|walks| walks.set(walks.get() + 1));
    let mut bytes = cache
        .as_ref()
        .and_then(|cache| cache.snapshot.as_ref())
        .map_or(0, |(bytes, _)| *bytes);
    let mut read = cache
        .as_ref()
        .filter(|cache| cache.snapshot.is_some())
        .map_or(0, |cache| cache.files.len());
    let mut complete = true;
    let mut pending = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = pending.pop() {
        let cached_paths = (depth == 0)
            .then(|| {
                cache
                    .as_ref()
                    .and_then(|cache| cache.snapshot.as_ref())
                    .map(|(_, paths)| paths.clone())
            })
            .flatten();
        let entries: Box<dyn Iterator<Item = std::io::Result<(PathBuf, Option<u64>)>>> =
            if let Some(paths) = cached_paths {
                Box::new(paths.into_iter().map(|path| Ok((path, None))))
            } else {
                if depth == 0 {
                    if let Some(cache) = cache.as_mut() {
                        cache.top_paths.clear();
                    }
                }
                #[cfg(test)]
                TREE_LISTS.with(|lists| lists.set(lists.get() + 1));
                match fs::read_dir(&current) {
                    Ok(entries) => Box::new(entries.map(|entry| {
                        entry.map(|entry| {
                            #[cfg(unix)]
                            let inode = {
                                use std::os::unix::fs::DirEntryExt;
                                Some(entry.ino())
                            };
                            #[cfg(not(unix))]
                            let inode = None;
                            (entry.path(), inode)
                        })
                    })),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            };
        let remaining = MAX_WALK_ENTRIES.saturating_sub(read);
        for entry in entries.take(remaining.saturating_add(1)) {
            if read >= MAX_WALK_ENTRIES
                || past(deadline)
                || limit.is_some_and(|limit| bytes > limit)
            {
                complete = false;
                break;
            }
            read += 1;
            let (path, inode) = match entry {
                Ok(entry) => entry,
                // Removed while listed: nothing to count.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if depth == 0 {
                if let Some(cache) = cache.as_mut() {
                    cache.top_paths.insert(path.clone());
                }
            }
            let immutable = depth == 0 && immutable_generation(&path).is_some();
            if immutable {
                if let Some((_, size)) = cache
                    .as_ref()
                    .filter(|cache| cache.directory.is_some())
                    .and_then(|cache| cache.files.get(&path))
                    .filter(|(previous_inode, _)| inode == Some(*previous_inode))
                {
                    bytes = bytes.saturating_add(*size);
                    continue;
                }
                if let Some(cache) = cache.as_mut() {
                    cache.files.remove(&path);
                }
            }
            let metadata = match measured_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.is_dir() {
                if depth + 1 < MAX_WALK_DEPTH {
                    pending.push((path.clone(), depth + 1));
                } else {
                    complete = false;
                }
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
                if immutable {
                    if let Some(cache) = cache.as_mut() {
                        if let Some(inode) = inode {
                            cache.files.insert(path, (inode, metadata.len()));
                        }
                    }
                }
            }
        }
        if !complete {
            break;
        }
    }
    Ok((bytes, complete))
}

/// The SQLite files of a view directory (main files only; their `-wal` and
/// `-shm` belong to the same open connections).
fn sqlite_files(view_dir: &Path, deadline: Option<Instant>) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(view_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries.take(MAX_WALK_ENTRIES.saturating_add(1)) {
        if files.len() >= MAX_WALK_ENTRIES || past(deadline) {
            return Err(std::io::Error::other(
                "view directory listing stopped at its bound",
            ));
        }
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("sqlite") {
            files.push(path);
        }
    }
    Ok(files)
}

/// True when this process has any of the view's SQLite files open. Deleting
/// a file set under a live connection detaches it from its locks.
fn open_in_process(view_dir: &Path, deadline: Option<Instant>) -> Result<bool, String> {
    Ok(sqlite_files(view_dir, deadline)
        .map_err(|error| error.to_string())?
        .iter()
        .any(|path| crate::db::file_identity::open_connections(path) != 0))
}

/// Why `member` cannot be evicted, read inside the registry barrier `tx` so
/// that no registration can land between this check and the deletion.
fn hold_inside_barrier(
    tx: &rusqlite::Connection,
    member: &MemberRecord,
    deadline: Option<Instant>,
) -> RegistryResult<Option<Hold>> {
    if has_live_session(tx, &member.scope)? {
        return Ok(Some(Hold::ActiveSession));
    }
    match family::view_has_protection(&member.view_dir) {
        Ok(false) => {}
        Ok(true) => return Ok(Some(Hold::Protected)),
        Err(reason) => return Ok(Some(Hold::Uncertain(reason))),
    }
    match open_in_process(&member.view_dir, deadline) {
        Ok(false) => Ok(None),
        Ok(true) => Ok(Some(Hold::OpenInProcess)),
        Err(reason) => Ok(Some(Hold::Uncertain(reason))),
    }
}

/// Why the v3 base `scope` cannot be reclaimed now, read inside the v3
/// registry barrier `tx`. Every deleter of a v3 base (the reference sweep's
/// two-step reclaim, the retention collector and any future LRU eviction)
/// goes through this check; none parses a manifest inside the barrier.
///
/// One indexed `base_refs` query, the `base_builds` claim, then the bounded
/// `readers/`/`pins/`/session listing. A query error or an over-long listing
/// is [`Hold::Uncertain`].
pub(crate) fn v3_base_hold(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    view_dir: &Path,
) -> Option<Hold> {
    use rusqlite::OptionalExtension;
    let referenced = tx
        .query_row(
            "SELECT 1 FROM base_refs WHERE base_scope=?1 LIMIT 1",
            [scope],
            |_| Ok(()),
        )
        .optional();
    match referenced {
        Ok(Some(())) => return Some(Hold::Referenced),
        Ok(None) => {}
        Err(error) => return Some(Hold::Uncertain(error.to_string())),
    }
    let claim = tx
        .query_row(
            "SELECT owner_pid,owner_start FROM base_builds WHERE base_scope=?1",
            [scope],
            |r| {
                Ok(crate::pins::PinOwner {
                    pid: r.get(0)?,
                    start_time: r.get(1)?,
                })
            },
        )
        .optional();
    match claim {
        Ok(Some(owner)) if crate::pins::owner_is_live(&owner) => return Some(Hold::Referenced),
        Ok(_) => {}
        Err(error) => return Some(Hold::Uncertain(error.to_string())),
    }
    match crate::storage_retention::v3_member_protected(tx, scope, view_dir) {
        Ok(false) => None,
        Ok(true) => Some(Hold::Protected),
        Err(reason) => Some(Hold::Uncertain(reason)),
    }
}

fn hold_of(
    registry: &FamilyRegistry,
    member: &MemberRecord,
    deadline: Option<Instant>,
) -> Option<Hold> {
    match registry.with_barrier(|tx| hold_inside_barrier(tx, member, deadline)) {
        Ok(hold) => hold,
        Err(error) => Some(Hold::Uncertain(error.to_string())),
    }
}

/// Measures both domains of a family. Nothing is changed on disk.
pub fn measure(registry: &FamilyRegistry, deadline: Option<Instant>) -> RegistryResult<DiskUsage> {
    let mut usage = DiskUsage {
        complete: true,
        ..DiskUsage::default()
    };
    let members = registry.members()?;
    // Taken out for the walk so the lock is not held across disk I/O; a
    // concurrent measurement of the same family simply walks uncached.
    let mut caches = std::mem::take(
        &mut *registry
            .view_size_caches()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    caches.retain(|dir, _| members.iter().any(|member| &member.view_dir == dir));
    for member in members {
        let (bytes, complete) = match view_tree_bytes(
            &member.view_dir,
            deadline,
            caches.entry(member.view_dir.clone()).or_default(),
        ) {
            Ok(measured) => measured,
            Err(_) => (0, false),
        };
        let hold = hold_of(registry, &member, deadline);
        usage.complete &= complete;
        usage.view_store_bytes = usage.view_store_bytes.saturating_add(bytes);
        if hold.is_some() {
            usage.held_view_bytes = usage.held_view_bytes.saturating_add(bytes);
        }
        usage.views.push(ViewUsage {
            scope: member.scope.clone(),
            view_dir: member.view_dir.clone(),
            bytes,
            last_bind_ms: member.last_bind_ms,
            hold,
            evicting: member.evicting,
            complete,
        });
    }
    *registry
        .view_size_caches()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = caches;
    usage.family_store = measure_family_store(registry, deadline)?;
    usage.complete &= usage.family_store.complete;
    // Best effort: a failed record only leaves admission an older one.
    let _ = record_usage(registry, &usage);
    Ok(usage)
}

/// Measures the family store by its files. A family whose files are within
/// every family-store limit opens no plane store: admission, the hard limit
/// and the recorded totals read only file bytes, and a logical payload total
/// would cost one aggregate query per plane on every pass. Reports that want
/// that total call [`family_payload_bytes`]. Only when a complete walk finds
/// the files over a limit are the plane stores' free pages read, so that the
/// limit is compared with live bytes.
fn measure_family_store(
    registry: &FamilyRegistry,
    deadline: Option<Instant>,
) -> RegistryResult<FamilyStoreUsage> {
    let (file_bytes, complete) =
        family_file_bytes(registry.storage(), registry.family(), deadline)?;
    let budget = registry.disk_budget();
    let lowest_limit = budget.family_store_soft.min(budget.family_store_hard);
    let free_bytes = if complete && file_bytes > lowest_limit {
        family_free_page_bytes(registry)
    } else {
        0
    };
    Ok(FamilyStoreUsage {
        file_bytes,
        free_bytes,
        complete,
    })
}

/// Bytes of every plane store of the family that hold no data (see
/// [`FamilyStoreUsage::free_bytes`]): page counts read from the database
/// header through that store's own connection, which is opened without
/// creating a store, and the main file's length. A store that cannot be read counts nothing, so its whole file
/// counts toward the limits.
fn family_free_page_bytes(registry: &FamilyRegistry) -> u64 {
    let mut free = 0u64;
    for plane in FamilyPlane::ALL {
        let read = FamilyStoreReader::open_existing(registry.storage(), registry.family(), plane)
            .and_then(|store| store.map(|store| store.unused_file_bytes()).transpose());
        match read {
            Ok(bytes) => free = free.saturating_add(bytes.unwrap_or(0)),
            Err(error) => crate::slog_warn!(
                "view disk limits family={} plane={} free pages not read: {}",
                registry.family(),
                plane.as_str(),
                error
            ),
        }
    }
    free
}

/// Returns free pages of the family's plane stores, at most
/// [`ALL_FREE_PAGES_MAX_PAGES`](crate::blob_store::reclaim::ALL_FREE_PAGES_MAX_PAGES)
/// for the whole family in one pass, whatever their share of each file.
/// Each store is reclaimed through its own connection in bounded
/// incremental-vacuum steps, never a full `VACUUM`. A store that cannot be
/// opened or is busy is left for a later pass.
fn reclaim_family_free_pages(
    registry: &FamilyRegistry,
    deadline: Option<Instant>,
) -> crate::blob_store::reclaim::ReclaimReport {
    let mut report = crate::blob_store::reclaim::ReclaimReport::default();
    let access = crate::blob_store::v2::StoreWriteAccess::for_registered_view(
        registry.storage(),
        registry.family(),
    );
    for plane in FamilyPlane::ALL {
        let cap = crate::blob_store::reclaim::ALL_FREE_PAGES_MAX_PAGES
            .saturating_sub(report.reclaimed_pages);
        if cap == 0 || past(deadline) {
            report.deferred = true;
            break;
        }
        let exists =
            crate::blob_store::v2::plane_path(registry.storage(), registry.family(), plane)
                .is_ok_and(|path| path.is_file());
        if !exists {
            continue;
        }
        let reclaimed = crate::blob_store::v2::FamilyStore::open(&access, plane)
            .and_then(|store| store.reclaim_all_free_pages(deadline, cap));
        match reclaimed {
            Ok(reclaimed) => report.merge(reclaimed),
            Err(error) => crate::slog_warn!(
                "view disk limits family={} plane={} free pages not returned: {}",
                registry.family(),
                plane.as_str(),
                error
            ),
        }
    }
    report
}

/// Bytes of every file under `blobs/v2/<family>/`, each file once, and
/// whether the walk reached the end before `deadline` and its bounds. It
/// only lists directories and reads file sizes, so it opens no SQLite file.
/// The family sweep uses the same walk to tell that a family is too small
/// to be over its budget.
pub(crate) fn family_file_bytes(
    storage: &Path,
    family: &str,
    deadline: Option<Instant>,
) -> RegistryResult<(u64, bool)> {
    let dir = family_dir(storage, family)?;
    Ok(tree_bytes(&dir, deadline)?)
}

/// True when a complete walk of the family's files finds at most `budget`
/// bytes. The walk stops as soon as the count passes the budget, and a zero
/// budget is answered without walking, since no non-empty family fits it.
pub(crate) fn family_files_within(
    storage: &Path,
    family: &str,
    deadline: Option<Instant>,
    budget: u64,
) -> RegistryResult<bool> {
    if budget == 0 {
        return Ok(false);
    }
    let dir = family_dir(storage, family)?;
    let (bytes, complete) = tree_bytes_within(&dir, deadline, budget)?;
    Ok(complete && bytes <= budget)
}

/// `SUM(length(payload))` over every plane store of the family, for reports
/// that show the logical size beside the file size. It opens each plane
/// store and runs one aggregate query per plane, so enforcement, admission
/// and measurement never call it.
pub fn family_payload_bytes(registry: &FamilyRegistry) -> RegistryResult<u64> {
    let mut payload_bytes = 0u64;
    for plane in FamilyPlane::ALL {
        if let Some(store) =
            FamilyStoreReader::open_existing(registry.storage(), registry.family(), plane)?
        {
            payload_bytes = payload_bytes.saturating_add(store.usage()?.payload_bytes);
        }
    }
    Ok(payload_bytes)
}

/// Divides the family store's keys among the views that rely on them. A key
/// several views rely on is counted once, in `shared_bytes`, and in no
/// view's exclusive share.
pub fn account_keys(
    registry: &FamilyRegistry,
    deadline: Option<Instant>,
) -> RegistryResult<KeyAccounting> {
    let bounds = walk_bounds(deadline);
    let mut accounting = KeyAccounting {
        complete: true,
        ..KeyAccounting::default()
    };
    let mut holders: BTreeMap<[u8; 32], BTreeSet<String>> = BTreeMap::new();
    for member in registry.members()? {
        accounting.exclusive_bytes.insert(member.scope.clone(), 0);
        match family::member_references(&member, registry.family(), &bounds) {
            Ok(keys) => {
                for key in keys {
                    holders.entry(key).or_default().insert(member.scope.clone());
                }
            }
            Err(_) => accounting.complete = false,
        }
    }
    let mut sizes: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    let access = crate::blob_store::v2::StoreWriteAccess::for_registered_view(
        registry.storage(),
        registry.family(),
    );
    for plane in FamilyPlane::ALL {
        let Some(reader) =
            FamilyStoreReader::open_existing(registry.storage(), registry.family(), plane)?
        else {
            continue;
        };
        drop(reader);
        // `FamilyStore` handles share one SQLite connection per store file in
        // this process, so listing through one never opens a second
        // descriptor on the live file set.
        let store = crate::blob_store::v2::FamilyStore::open(&access, plane)?;
        let rows = store.rows_oldest(bounds.max_rows_per_store)?;
        if rows.len() >= bounds.max_rows_per_store {
            accounting.complete = false;
        }
        for row in rows {
            if past(deadline) {
                accounting.complete = false;
                break;
            }
            sizes.insert(*row.key.as_bytes(), row.payload_bytes);
        }
        for segment in store
            .segments()?
            .into_iter()
            .take(bounds.max_rows_per_store)
        {
            sizes.insert(segment.segment_id, segment.byte_len);
        }
    }
    for (key, bytes) in sizes {
        match holders.get(&key) {
            None => accounting.unreferenced_bytes += bytes,
            Some(scopes) => {
                accounting.referenced_bytes += bytes;
                if scopes.len() == 1 {
                    let scope = scopes.iter().next().expect("one holder");
                    *accounting.exclusive_bytes.entry(scope.clone()).or_default() += bytes;
                } else {
                    accounting.shared_bytes += bytes;
                }
            }
        }
    }
    Ok(accounting)
}

/// Evicts one member: its view directory and its member row. Runs in two
/// steps under the registry barrier. The first marks the member `evicting`
/// and commits, so an interrupted eviction is visible; the second checks
/// every hold again, deletes the pointer first (so no half-deleted
/// generation is ever published), then the rest of the directory, then the
/// member row.
pub fn evict_view(
    registry: &FamilyRegistry,
    scope: &str,
    deadline: Option<Instant>,
    observer: Option<&dyn EvictionObserver>,
) -> RegistryResult<EvictionOutcome> {
    let begun = registry.with_barrier(|tx| {
        let Some(member) = member_in(tx, scope)? else {
            return Ok(Err(EvictionOutcome::Gone));
        };
        if let Some(hold) = hold_inside_barrier(tx, &member, deadline)? {
            return Ok(Err(EvictionOutcome::Held(hold)));
        }
        tx.execute(
            "UPDATE members SET state = ?2 WHERE scope = ?1",
            params![scope, STATE_EVICTING],
        )?;
        Ok(Ok(member))
    })?;
    let member = match begun {
        Ok(member) => member,
        Err(outcome) => return Ok(outcome),
    };
    if let Some(observer) = observer {
        observer.reached(EvictionStep::Begun);
    }
    let (bytes, _) = tree_bytes(&member.view_dir, deadline).unwrap_or((0, false));
    registry.with_barrier(|tx| {
        let Some(current) = member_in(tx, scope)? else {
            return Ok(EvictionOutcome::Gone);
        };
        // A registration after the member was marked `evicting` set it back
        // to `active`: that view is in use again, so this eviction stops.
        if !current.evicting {
            return Ok(EvictionOutcome::Held(Hold::ActiveSession));
        }
        if let Some(hold) = hold_inside_barrier(tx, &current, deadline)? {
            tx.execute(
                "UPDATE members SET state = 'active' WHERE scope = ?1",
                params![scope],
            )?;
            return Ok(EvictionOutcome::Held(hold));
        }
        // No connection may open a file of this set between the check above
        // and the deletion below.
        let _files = crate::db::file_identity::filesystem_guard();
        if open_in_process(&current.view_dir, deadline).unwrap_or(true) {
            tx.execute(
                "UPDATE members SET state = 'active' WHERE scope = ?1",
                params![scope],
            )?;
            return Ok(EvictionOutcome::Held(Hold::OpenInProcess));
        }
        let mut pointer_removed = false;
        let removal = remove_view_dir_with(&current.view_dir, |directory| {
            remove_pointer(directory)?;
            pointer_removed = true;
            if let Some(observer) = observer {
                observer.reached(EvictionStep::PointerRemoved);
            }
            Ok(())
        });
        if let Err(error) = removal {
            if !pointer_removed {
                tx.execute(
                    "UPDATE members SET state = 'active' WHERE scope = ?1",
                    params![scope],
                )?;
                return Ok(EvictionOutcome::Held(Hold::Uncertain(format!(
                    "pointer not removable: {error}"
                ))));
            }
            // The pointer is gone, so the view publishes nothing; leave the
            // member `evicting` for the next pass or registration to finish.
            return Ok(EvictionOutcome::Incomplete(error.to_string()));
        }
        tx.execute("DELETE FROM members WHERE scope = ?1", params![scope])?;
        // Dead sessions of the evicted view, if the table exists at all.
        let _ = tx.execute("DELETE FROM view_sessions WHERE scope = ?1", params![scope]);
        Ok(EvictionOutcome::Evicted { bytes })
    })
}

fn member_in(tx: &rusqlite::Connection, scope: &str) -> RegistryResult<Option<MemberRecord>> {
    Ok(super::registry::read_members(tx)?
        .into_iter()
        .find(|member| member.scope == scope))
}

/// Deletes the pointer database set of a view directory, main file first.
fn remove_pointer(
    view_dir: &crate::bash_background::persistence::PinnedDir,
) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let name = std::ffi::OsString::from(format!("{}{suffix}", super::POINTER_DATABASE));
        match view_dir.repair_remove_file(&name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn pointer_removal_cannot_follow_a_retargeted_view_ancestor() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("views");
    let view = parent.join("scope");
    fs::create_dir_all(&view).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir_all(outside.join("scope")).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("{}{suffix}", super::POINTER_DATABASE);
        fs::write(view.join(&name), "own").unwrap();
        fs::write(outside.join("scope").join(name), "precious").unwrap();
    }
    let pinned = crate::bash_background::persistence::PinnedDir::open(&view).unwrap();
    fs::rename(&parent, temp.path().join("parked")).unwrap();
    std::os::unix::fs::symlink(&outside, &parent).unwrap();
    remove_pointer(&pinned).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("{}{suffix}", super::POINTER_DATABASE);
        assert_eq!(
            fs::read_to_string(outside.join("scope").join(name)).unwrap(),
            "precious"
        );
    }
}

/// One enforcement pass: finish interrupted evictions, evict views without a
/// hold (least recently bound first) while the view store is over its soft
/// limit, return free pages of a family store whose files are over its soft
/// limit, then sweep the family store so that keys only the evicted views
/// relied on are reclaimed and the freed space returns to the filesystem.
/// The sweep collects every unreferenced row only after an eviction or when
/// the family store's live bytes are over its soft limit.
pub fn enforce(
    registry: &FamilyRegistry,
    options: EnforceOptions<'_>,
) -> Result<EnforcementReport, EvictionError> {
    enforce_with_schedule(registry, options, None)
}

fn enforce_with_schedule(
    registry: &FamilyRegistry,
    options: EnforceOptions<'_>,
    scheduled: Option<bool>,
) -> Result<EnforcementReport, EvictionError> {
    let budget = registry.disk_budget();
    let deadline = options.deadline;
    let _ = registry.reclaim_dead_sessions();
    let before = measure(registry, deadline)?;
    let mut report = EnforcementReport {
        before: before.clone(),
        ..EnforcementReport::default()
    };
    let mut total = before.view_store_bytes;
    let mut candidates = before
        .views
        .iter()
        .filter(|view| view.hold.is_none() || view.evicting)
        .collect::<Vec<_>>();
    // Interrupted evictions first, then least recently bound first; the
    // scope breaks ties so the order never depends on listing order.
    candidates.sort_by(|a, b| {
        (!a.evicting, a.last_bind_ms, &a.scope).cmp(&(!b.evicting, b.last_bind_ms, &b.scope))
    });
    for view in candidates {
        if !view.evicting && total <= budget.view_store_soft {
            break;
        }
        if past(deadline) {
            report.stopped_early = true;
            break;
        }
        match evict_view(registry, &view.scope, deadline, options.observer)? {
            EvictionOutcome::Evicted { bytes } => {
                total = total.saturating_sub(bytes);
                report.evicted.push((view.scope.clone(), bytes));
            }
            EvictionOutcome::Held(hold) => report.held.push((view.scope.clone(), hold)),
            EvictionOutcome::Gone => {}
            EvictionOutcome::Incomplete(reason) => report
                .held
                .push((view.scope.clone(), Hold::Uncertain(reason))),
        }
    }
    for view in &before.views {
        if let Some(hold) = &view.hold {
            if !view.evicting {
                report.held.push((view.scope.clone(), hold.clone()));
            }
        }
    }

    // Free pages count in the family store's file bytes but hold no row, so
    // a store over its soft limit in file bytes gives them back first,
    // whatever their share of the file. Only live bytes then decide whether
    // rows must be deleted: returning free pages leaves live bytes as they
    // are, so the measurement taken before it still holds.
    if before.family_store.file_bytes > budget.family_store_soft
        && before.family_store.free_bytes > 0
        && !past(deadline)
    {
        report.free_page_reclaim = reclaim_family_free_pages(registry, deadline);
    }
    let collect_all =
        !report.evicted.is_empty() || before.family_store.live_bytes() > budget.family_store_soft;
    let policy = FamilySweepPolicy {
        byte_budget: if collect_all {
            0
        } else {
            budget.family_payload
        },
    };
    if !past(deadline) {
        #[cfg(test)]
        LAST_SWEEP_BUDGET.with(|last| last.set(Some(policy.byte_budget)));
        let swept = if let Some(force) = scheduled {
            family::sweep_family_scheduled(
                registry,
                walk_bounds(deadline),
                force || !report.evicted.is_empty(),
            )
        } else {
            family::sweep_family_remeasured(registry, None, policy, walk_bounds(deadline), None)
        };
        match swept {
            Ok(sweep) => {
                report.stopped_early |= sweep.stopped_early;
                report.sweep = Some(sweep);
            }
            Err(error) => report.sweep_error = Some(error.to_string()),
        }
    } else {
        report.stopped_early = true;
    }

    report.after = measure(registry, deadline)?;
    // A measurement cut short by the deadline undercounts; the recorded
    // totals keep the last complete value of whatever it could not read.
    let totals = if report.after.complete {
        (&report.after).into()
    } else {
        recorded_totals(registry)
            .ok()
            .flatten()
            .unwrap_or_else(|| (&report.after).into())
    };
    report.over_hard = over_hard(totals, budget, 0);
    Ok(report)
}

/// Totals that decide admission and refusal, from a fresh measurement or
/// from the last recorded one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub view_store_bytes: u64,
    pub held_view_bytes: u64,
    pub largest_view_bytes: u64,
    pub family_store_file_bytes: u64,
    /// The part of `family_store_file_bytes` in free pages, zero when the
    /// measurement did not read them (see [`FamilyStoreUsage::free_bytes`]).
    pub family_store_free_bytes: u64,
    /// False when the measurement behind these totals could not read every
    /// view or the family store to the end. Recorded totals then hold the
    /// previous value of each part that was not read.
    pub complete: bool,
}

impl From<&DiskUsage> for UsageTotals {
    fn from(usage: &DiskUsage) -> Self {
        Self {
            view_store_bytes: usage.view_store_bytes,
            held_view_bytes: usage.held_view_bytes,
            largest_view_bytes: usage.largest_view(),
            family_store_file_bytes: usage.family_store.file_bytes,
            family_store_free_bytes: usage.family_store.free_bytes,
            complete: usage.complete,
        }
    }
}

impl UsageTotals {
    /// Family-store bytes the hard limit is compared with; see
    /// [`FamilyStoreUsage::live_bytes`].
    pub fn family_store_live_bytes(&self) -> u64 {
        self.family_store_file_bytes
            .saturating_sub(self.family_store_free_bytes)
    }
}

/// The named reasons new work that adds `reserve` view-store bytes would be
/// queued under `budget`, given `totals`.
fn over_hard(totals: UsageTotals, budget: DiskBudget, reserve: u64) -> Vec<String> {
    let mut reasons = Vec::new();
    if totals.view_store_bytes.saturating_add(reserve) > budget.view_store_hard {
        reasons.push(format!(
            "queued: view store disk: {} of {} bytes are held by active sessions or protected \
             generations, hard limit {} bytes{}",
            totals.held_view_bytes,
            totals.view_store_bytes,
            budget.view_store_hard,
            if reserve > 0 {
                format!(", a new view reserves {reserve} bytes")
            } else {
                String::new()
            }
        ));
    }
    if totals.family_store_live_bytes() > budget.family_store_hard {
        reasons.push(format!(
            "queued: family store disk: {} bytes in use after collecting unreferenced keys, \
             hard limit {} bytes{}",
            totals.family_store_live_bytes(),
            budget.family_store_hard,
            if totals.family_store_free_bytes > 0 {
                format!(
                    ", not counting {} bytes of free pages",
                    totals.family_store_free_bytes
                )
            } else {
                String::new()
            }
        ));
    }
    reasons
}

/// Records `usage` in the registry so that admission can decide without
/// measuring. Views that left the registry drop out of the totals because
/// admission joins on the member rows.
///
/// Only what was measured to the end is written. A walk stopped by the
/// deadline or a bound reports a part of the bytes (after the deadline,
/// none at all), and recording that would let admission read a full family
/// as empty. So a view or a family store measured incompletely keeps its
/// previous row, and `usage_record.complete` is cleared to say the record
/// is not one complete measurement.
///
/// The free-page bytes of a complete family-store measurement go to
/// [`FAMILY_FREE_USAGE_SCHEMA`], created only when there are any to record,
/// so a family that never measured free pages keeps the registry it had.
fn record_usage(registry: &FamilyRegistry, usage: &DiskUsage) -> RegistryResult<()> {
    let now = crate::pins::now_ms() as i64;
    let views_complete = usage.views_complete();
    registry.with_barrier(|tx| {
        tx.execute_batch(super::registry::RUNTIME_SCHEMA)?;
        if views_complete {
            tx.execute("DELETE FROM view_usage", [])?;
        }
        for view in usage.views.iter().filter(|view| view.complete) {
            tx.execute(
                "INSERT INTO view_usage (scope, bytes, held, measured_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(scope) DO UPDATE SET bytes = excluded.bytes,
                     held = excluded.held, measured_at_ms = excluded.measured_at_ms",
                params![
                    view.scope,
                    i64::try_from(view.bytes).unwrap_or(i64::MAX),
                    view.hold.is_some(),
                    now
                ],
            )?;
        }
        if usage.family_store.complete {
            tx.execute(
                "INSERT INTO family_usage (singleton, file_bytes, measured_at_ms) VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton) DO UPDATE SET file_bytes = excluded.file_bytes,
                     measured_at_ms = excluded.measured_at_ms",
                params![
                    i64::try_from(usage.family_store.file_bytes).unwrap_or(i64::MAX),
                    now
                ],
            )?;
            if usage.family_store.free_bytes > 0 {
                tx.execute_batch(FAMILY_FREE_USAGE_SCHEMA)?;
                tx.execute(
                    "INSERT INTO family_free_usage (singleton, free_bytes, measured_at_ms)
                     VALUES (1, ?1, ?2)
                     ON CONFLICT(singleton) DO UPDATE SET free_bytes = excluded.free_bytes,
                         measured_at_ms = excluded.measured_at_ms",
                    params![
                        i64::try_from(usage.family_store.free_bytes).unwrap_or(i64::MAX),
                        now
                    ],
                )?;
            } else if table_exists(tx, "family_free_usage")? {
                tx.execute("DELETE FROM family_free_usage", [])?;
            }
        }
        tx.execute(
            "INSERT INTO usage_record (singleton, complete, measured_at_ms) VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET complete = excluded.complete,
                 measured_at_ms = excluded.measured_at_ms",
            params![usage.complete, now],
        )?;
        Ok(())
    })
}

/// The totals of the last recorded measurement, read with two queries over
/// at most one row per member and no filesystem access. `None` when the
/// family has never been measured.
pub fn recorded_totals(registry: &FamilyRegistry) -> RegistryResult<Option<UsageTotals>> {
    registry.read(|connection| {
        let measured = connection
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'family_usage'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !measured {
            return Ok(None);
        }
        let Some((family_store_file_bytes, family_measured_at_ms)) = connection
            .query_row(
                "SELECT file_bytes, measured_at_ms FROM family_usage WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        // Free pages count only when recorded by the same measurement as the
        // file bytes; a release that predates them writes file bytes alone.
        let family_store_free_bytes = if table_exists(connection, "family_free_usage")? {
            connection
                .query_row(
                    "SELECT free_bytes FROM family_free_usage
                     WHERE singleton = 1 AND measured_at_ms = ?1",
                    params![family_measured_at_ms],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0)
        } else {
            0
        };
        let (view_store_bytes, held_view_bytes, largest_view_bytes) = connection.query_row(
            "SELECT COALESCE(SUM(usage.bytes), 0),
                    COALESCE(SUM(CASE WHEN usage.held THEN usage.bytes ELSE 0 END), 0),
                    COALESCE(MAX(usage.bytes), 0)
             FROM view_usage AS usage JOIN members ON members.scope = usage.scope",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        // A record written before `usage_record` existed may hold a partial
        // measurement, so it counts as incomplete.
        let complete = connection
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'usage_record'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
            && connection
                .query_row(
                    "SELECT complete FROM usage_record WHERE singleton = 1",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false);
        Ok(Some(UsageTotals {
            view_store_bytes: view_store_bytes.max(0) as u64,
            held_view_bytes: held_view_bytes.max(0) as u64,
            largest_view_bytes: largest_view_bytes.max(0) as u64,
            family_store_file_bytes: family_store_file_bytes.max(0) as u64,
            family_store_free_bytes: family_store_free_bytes.max(0) as u64,
            complete,
        }))
    })
}

/// Free-page bytes of the family store at the last complete measurement
/// that read them; see [`FamilyStoreUsage::free_bytes`]. Kept out of the
/// registry's runtime schema so that it is created only for a family that
/// was ever measured over a limit.
const FAMILY_FREE_USAGE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS family_free_usage (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    free_bytes INTEGER NOT NULL,
    measured_at_ms INTEGER NOT NULL
);";

fn table_exists(connection: &rusqlite::Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Admission of a new view under the family's hard limits, on the path that
/// registers it, which may be a bind. It never measures and never evicts
/// inline: it reads the last recorded totals (one row per member, no
/// directory walk), and a new view's first generation is assumed to add as
/// many bytes as the largest recorded view. If that does not fit, an
/// enforcement pass starts in the background and the view is refused now
/// with the named reason; registering again after the pass may succeed. A
/// family never measured is admitted. Views already registered are never
/// refused.
pub fn admit_new_view(registry: &FamilyRegistry) -> RegistryResult<()> {
    let budget = registry.disk_budget();
    // A family never measured is admitted; the periodic pass measures it.
    let Some(totals) = recorded_totals(registry)? else {
        return Ok(());
    };
    let reasons = over_hard(totals, budget, totals.largest_view_bytes);
    if reasons.is_empty() {
        return Ok(());
    }
    let pass = match spawn_enforcement(registry.clone(), true) {
        Scheduled::Started => "an eviction pass has started, register again after it",
        Scheduled::Queued => {
            "an eviction pass is queued behind another family's disk-limits pass, \
             register again after it"
        }
        Scheduled::AlreadyPending => {
            "an eviction pass is already running or queued, register again after it"
        }
        // A forced request is always due; this arm only keeps the match total.
        Scheduled::NotDue => "no eviction pass is due",
        Scheduled::Unavailable => {
            "no eviction pass could start because its thread could not be created"
        }
    };
    let reason = format!(
        "{}; {}{}",
        reasons.join("; "),
        pass,
        if totals.complete {
            ""
        } else {
            "; the last disk measurement did not finish, so these totals keep earlier values \
             for what it could not read"
        }
    );
    crate::slog_warn!(
        "view admission refused family={} {}",
        registry.family(),
        reason
    );
    Err(RegistryError::AdmissionQueued(reason))
}

fn last_runs() -> &'static Mutex<BTreeMap<PathBuf, Instant>> {
    static RUNS: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Background enforcement in this process. At most one pass runs at a time,
/// whatever the number of families: every pass walks view directories and
/// family stores on disk, and passes of many families at once compete for
/// the same disk and CPU until all of them hit their deadline. A pass that
/// is requested while another family's pass runs waits in `queue`, and the
/// one enforcement thread runs it next.
#[derive(Default)]
struct Scheduler {
    /// Families with a pass running or waiting in `queue`, each once.
    pending: BTreeSet<PathBuf>,
    /// Passes waiting for the running one to finish, oldest first.
    queue: VecDeque<(FamilyRegistry, bool)>,
    /// A hard-limit request arriving during a pass must not be lost to cadence.
    forced: BTreeSet<PathBuf>,
    /// True while the enforcement thread exists. It drains `queue` before
    /// it clears this under the same lock, so a queued pass is never left
    /// without a thread to run it.
    runner: bool,
}

fn scheduler() -> &'static Mutex<Scheduler> {
    static SCHEDULER: OnceLock<Mutex<Scheduler>> = OnceLock::new();
    SCHEDULER.get_or_init(|| Mutex::new(Scheduler::default()))
}

fn lock_scheduler() -> std::sync::MutexGuard<'static, Scheduler> {
    scheduler()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Removes a family from the pending set when its pass ends, including when
/// the pass unwinds, so its disk limits can be checked again.
struct EnforcementFlight(PathBuf);

impl Drop for EnforcementFlight {
    fn drop(&mut self) {
        lock_scheduler().pending.remove(&self.0);
    }
}

fn is_running(key: &Path) -> bool {
    lock_scheduler().pending.contains(key)
}

/// What a request for a background enforcement pass did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheduled {
    /// The enforcement thread started with this family's pass.
    Started,
    /// Another family's pass is running; this one runs after it.
    Queued,
    /// A pass for this family is already running or queued.
    AlreadyPending,
    /// The interval since this family's last pass has not passed.
    NotDue,
    /// The enforcement thread could not be created.
    Unavailable,
}

/// True when this process has not checked `registry`'s limits within
/// [`ENFORCEMENT_INTERVAL`] and no check is running or queued.
pub fn enforcement_due(registry: &FamilyRegistry) -> bool {
    let key = registry.path().to_path_buf();
    if is_running(&key) {
        return false;
    }
    last_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_none_or(|last| last.elapsed() >= ENFORCEMENT_INTERVAL)
}

/// Requests one background enforcement pass for `registry` when one is due.
/// Returns whether a pass was started or queued behind another family's.
pub fn spawn_enforcement_if_due(registry: FamilyRegistry) -> bool {
    matches!(
        spawn_enforcement(registry, false),
        Scheduled::Started | Scheduled::Queued
    )
}

/// Requests one background enforcement pass, unless one is already running
/// or queued for the family in this process. Without `force` it also waits
/// out [`ENFORCEMENT_INTERVAL`] since the last pass. When another family's
/// pass is running, this one is queued and runs after it on the same thread.
fn spawn_enforcement(registry: FamilyRegistry, force: bool) -> Scheduled {
    if !force && !enforcement_due(&registry) {
        return Scheduled::NotDue;
    }
    let key = registry.path().to_path_buf();
    {
        let mut scheduler = lock_scheduler();
        if !scheduler.pending.insert(key.clone()) {
            if force {
                scheduler.forced.insert(key.clone());
            }
            return Scheduled::AlreadyPending;
        }
        if scheduler.runner {
            scheduler.queue.push_back((registry, force));
            return Scheduled::Queued;
        }
        scheduler.runner = true;
    }
    let spawned = std::thread::Builder::new()
        .name("aft-view-disk-limits".to_string())
        .spawn(move || run_enforcement_queue(registry, force));
    if let Err(error) = spawned {
        let mut scheduler = lock_scheduler();
        scheduler.pending.remove(&key);
        // Passes queued while the spawn was attempted have no thread either;
        // release them so their next request can start one.
        for (queued, _) in std::mem::take(&mut scheduler.queue) {
            scheduler.pending.remove(queued.path());
            scheduler.forced.remove(queued.path());
        }
        scheduler.runner = false;
        drop(scheduler);
        crate::slog_warn!("view disk limits thread unavailable: {error}");
        return Scheduled::Unavailable;
    }
    Scheduled::Started
}

/// The body of the one enforcement thread: runs `first`, then every queued
/// pass in order, and exits when the queue is empty.
fn run_enforcement_queue(first: FamilyRegistry, force: bool) {
    run_enforcement_queue_with(first, force, run_enforcement_pass);
}

fn run_enforcement_queue_with(
    first: FamilyRegistry,
    force: bool,
    mut run_pass: impl FnMut(&FamilyRegistry, bool),
) {
    let mut next = Some((first, force));
    while let Some((registry, force)) = next.take() {
        let force = lock_scheduler().forced.remove(registry.path()) || force;
        // A panicking pass must not take the queued passes with it.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_pass(&registry, force)));
        if outcome.is_err() {
            crate::slog_warn!(
                "view disk limits check panicked family={}",
                registry.family()
            );
        }
        let mut scheduler = lock_scheduler();
        if scheduler.forced.remove(registry.path()) {
            let key = registry.path().to_path_buf();
            if scheduler.pending.insert(key.clone()) {
                // Repeated admission failures must not jump ahead of families
                // that were already waiting for disk maintenance.
                scheduler.queue.push_back((registry, true));
            } else {
                // A request queued this family after its pass released the
                // pending slot. Upgrade that entry rather than duplicating it.
                scheduler.forced.insert(key);
            }
        }
        next = scheduler.queue.pop_front();
        if next.is_none() {
            scheduler.runner = false;
        }
    }
}

#[cfg(test)]
static ACTIVE_PASSES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The most background passes that ran at once since a test last reset it.
#[cfg(test)]
static MOST_CONCURRENT_PASSES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
/// Milliseconds every background pass sleeps while counted as running, so
/// that a test can make passes overlap if the scheduler would allow it.
#[cfg(test)]
static PASS_TEST_DELAY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn run_enforcement_pass(registry: &FamilyRegistry, force: bool) {
    // Unwinding must release the family as well as normal completion, or
    // this family's disk limits would never run again.
    let _flight = EnforcementFlight(registry.path().to_path_buf());
    last_runs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(registry.path().to_path_buf(), Instant::now());
    #[cfg(test)]
    let _counted = {
        use std::sync::atomic::Ordering;
        struct Counted;
        impl Drop for Counted {
            fn drop(&mut self) {
                ACTIVE_PASSES.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let active = ACTIVE_PASSES.fetch_add(1, Ordering::SeqCst) + 1;
        MOST_CONCURRENT_PASSES.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(
            PASS_TEST_DELAY_MS.load(Ordering::SeqCst),
        ));
        Counted
    };
    let options = EnforceOptions {
        deadline: Some(Instant::now() + ENFORCEMENT_DEADLINE),
        observer: None,
    };
    match enforce_with_schedule(registry, options, Some(force)) {
        Ok(report) => log_report(registry, &report),
        Err(error) => crate::slog_warn!(
            "view disk limits check failed family={} error={}",
            registry.family(),
            error
        ),
    }
}

/// Waits until no background pass for `registry` runs or waits in this
/// process.
#[cfg(test)]
pub(crate) fn wait_for_background_enforcement(registry: &FamilyRegistry) {
    let started = Instant::now();
    while is_running(registry.path()) {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "background enforcement did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn log_report(registry: &FamilyRegistry, report: &EnforcementReport) {
    crate::slog_info!("{}", report_line(registry.family(), report));
    for reason in &report.over_hard {
        crate::slog_warn!("view disk limits family={} {}", registry.family(), reason);
    }
}

/// A byte total for the log, or `unknown` when the walk behind it stopped
/// before the end and the number is only a part.
fn shown_bytes(bytes: u64, complete: bool) -> String {
    if complete {
        bytes.to_string()
    } else {
        "unknown".to_string()
    }
}

fn report_line(family: &str, report: &EnforcementReport) -> String {
    let after_views = report.after.views_complete();
    format!(
        "view disk limits family={} view_store_bytes={}->{} held_bytes={} family_store_bytes={}->{} \
         evicted={} held={} sweep_deleted_bytes={} sweep_error={} stopped_early={} \
         free_page_bytes_returned={}",
        family,
        shown_bytes(
            report.before.view_store_bytes,
            report.before.views_complete()
        ),
        shown_bytes(report.after.view_store_bytes, after_views),
        shown_bytes(report.after.held_view_bytes, after_views),
        shown_bytes(
            report.before.family_store.file_bytes,
            report.before.family_store.complete
        ),
        shown_bytes(
            report.after.family_store.file_bytes,
            report.after.family_store.complete
        ),
        report.evicted.len(),
        report.held.len(),
        report.sweep.as_ref().map_or(0, |sweep| sweep.deleted_bytes),
        report.sweep_error.as_deref().unwrap_or("none"),
        report.stopped_early,
        report.free_page_reclaim.reclaimed_bytes
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob_store::v2::{ContentHash, FamilyKey, TrigramKey, TrigramPolicy};
    use crate::pins::{AssemblyPin, LivePin, QueryPin};
    use crate::views::manifest_v2::{
        EntryPlanes, EntryV2, GenerationName, ManifestHeader, ManifestV2, Producers, PublishV2,
    };
    use crate::views::readiness::PlaneState;
    use crate::views::registry::ViewRegistration;
    use crate::views::segment_store::{self, SegmentMember, TrigramPayload};
    use crate::views::RelPath;

    const FAMILY: &str = "family";

    /// Deciding whether a family fits its budget never walks for a zero
    /// budget and stops counting once the budget is passed, so an over-budget
    /// sweep does not pay for a full walk that cannot save it any work.
    #[cfg(unix)]
    #[test]
    fn family_budget_walk_skips_zero_budget_and_stops_once_exceeded() {
        let storage = tempfile::tempdir().unwrap();
        let dir = crate::blob_store::v2::family_dir(storage.path(), FAMILY).unwrap();
        fs::create_dir_all(&dir).unwrap();
        for index in 0..200 {
            fs::write(dir.join(format!("segment-{index}.bin")), vec![0u8; 1024]).unwrap();
        }

        TREE_WALKS.with(|walks| walks.set(0));
        assert!(!family_files_within(storage.path(), FAMILY, None, 0).unwrap());
        assert_eq!(
            TREE_WALKS.with(|walks| walks.get()),
            0,
            "a zero budget walked"
        );

        TREE_STATS.with(|stats| stats.set(0));
        assert!(!family_files_within(storage.path(), FAMILY, None, 4 * 1024).unwrap());
        let early = TREE_STATS.with(|stats| stats.replace(0));
        assert!(
            early <= 6,
            "the walk kept counting past the budget: {early} stats for 200 files"
        );

        assert!(family_files_within(storage.path(), FAMILY, None, 1024 * 1024).unwrap());
        assert_eq!(TREE_STATS.with(|stats| stats.get()), 200);
    }

    #[cfg(unix)]
    #[test]
    fn republishing_views_measure_only_changed_artifacts() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let publish = |dir: &Path, generation: usize| {
            let generation = if generation % 2 == 0 {
                format!("{generation}-42-1234567890-{generation}-deadbeef")
            } else {
                format!("g2-{generation:064x}-1-1-{generation}")
            };
            for (prefix, suffix) in [("manifest", "json"), ("trigram", "bin"), ("derived", "ref")] {
                fs::write(
                    dir.join(format!("{prefix}-{generation}.{suffix}")),
                    b"immutable",
                )
                .unwrap();
            }
        };
        let mut dirs = Vec::new();
        for index in 0..24 {
            let root = storage.path().join(format!("checkout-{index}"));
            fs::create_dir(&root).unwrap();
            let view = registry
                .register_view_with_storage(storage.path(), &format!("view-{index}"), &root)
                .unwrap();
            fs::write(view.view_dir().join("derived-owner.sqlite"), vec![0; 8192]).unwrap();
            fs::create_dir_all(view.view_dir().join("readers/current")).unwrap();
            fs::write(view.view_dir().join("progress.marker"), b"reader").unwrap();
            for generation in 0..64 {
                publish(view.view_dir(), generation);
            }
            dirs.push(view.view_dir().to_path_buf());
        }
        measure(&registry, None).unwrap();
        let mut calls = Vec::new();
        for pass in 0..6 {
            // Six checkouts publish between ten-minute enforcement passes.
            for dir in dirs.iter().take(6) {
                publish(dir, 64 + pass);
                fs::write(dir.join("derived-owner.sqlite"), vec![0; 8192 + pass]).unwrap();
            }
            TREE_STATS.with(|stats| stats.set(0));
            let report = enforce(&registry, EnforceOptions::default()).unwrap();
            let stats = TREE_STATS.with(|stats| stats.replace(0));
            calls.push(stats);
            assert!(report.evicted.is_empty());
            for view in &report.after.views {
                assert_eq!(
                    (view.bytes, view.complete),
                    tree_bytes(&view.view_dir, None).unwrap()
                );
            }
            println!("republish enforcement pass {pass}: {stats} lstat/pass, {} lstat/hour at six passes/hour", stats * 6);
        }
        assert!(
            calls.iter().all(|calls| *calls < 500),
            "retained generations re-statted after publication: {calls:?}"
        );
        let usage = measure(&registry, None).unwrap();
        registry.set_disk_budget(budget(1, u64::MAX));
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(report.evicted.len(), dirs.len());
        assert_eq!(report.after.view_store_bytes, 0);
        assert!(usage.view_store_bytes > 0);
    }

    #[cfg(unix)]
    #[test]
    fn immutable_view_totals_avoid_repeated_stats() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        // A family with many retained generations, plus mutable pointer/marker files.
        for index in 0..24 {
            let scope = format!("view-{index}");
            let root = storage.path().join(&scope);
            fs::create_dir(&root).unwrap();
            let view = registry
                .register_view_with_storage(storage.path(), &scope, &root)
                .unwrap();
            fs::write(view.view_dir().join("derived-owner.sqlite"), vec![0; 8192]).unwrap();
            fs::create_dir_all(view.view_dir().join("readers/current")).unwrap();
            fs::write(view.view_dir().join("progress.marker"), b"reader").unwrap();
            for generation in 0..64 {
                let generation = format!("g2-{generation:064x}-1-1-{generation}");
                fs::write(
                    view.view_dir().join(format!("manifest-{generation}.json")),
                    b"manifest",
                )
                .unwrap();
                fs::write(
                    view.view_dir().join(format!("trigram-{generation}.bin")),
                    b"trigram",
                )
                .unwrap();
                fs::write(
                    view.view_dir().join(format!("derived-{generation}.ref")),
                    b"owner",
                )
                .unwrap();
            }
        }
        TREE_STATS.with(|stats| stats.set(0));
        TREE_WALKS.with(|walks| walks.set(0));
        TREE_LISTS.with(|lists| lists.set(0));
        let started = Instant::now();
        let cold = measure(&registry, None).unwrap();
        let cold_elapsed = started.elapsed();
        let cold_stats = TREE_STATS.with(|stats| stats.replace(0));
        let cold_lists = TREE_LISTS.with(|lists| lists.replace(0));
        let started = Instant::now();
        let warm = measure(&registry, None).unwrap();
        let warm_elapsed = started.elapsed();
        let warm_stats = TREE_STATS.with(|stats| stats.replace(0));
        let warm_lists = TREE_LISTS.with(|lists| lists.replace(0));
        let walks = TREE_WALKS.with(|walks| walks.get());
        assert_eq!(walks, 2 * (24 + 1));
        assert_eq!(cold.views, warm.views);
        println!("directory listings: cold={cold_lists}, warm={warm_lists}");
        println!(
            "disk measurement wall time: cold_us={} warm_us={}",
            cold_elapsed.as_micros(),
            warm_elapsed.as_micros()
        );
        assert_eq!(
            warm_lists + 24,
            cold_lists,
            "unchanged view namespaces were listed again"
        );
        println!("disk measurement / 24 views x 64 generations: cold={cold_stats} lstat, warm={warm_stats} lstat, {walks} tree walks; two measurements per enforcement, interval={}s", ENFORCEMENT_INTERVAL.as_secs());
        assert!(cold_stats > 24 * 64 * 3);
        assert!(
            warm_stats < cold_stats / 10,
            "immutable generations were re-statted: cold={cold_stats}, warm={warm_stats}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_view_bytes_match_full_walk_through_changes_and_reconcile() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let generation = format!("g2-{}-1-1-1", "a".repeat(64));
        let manifest = dir.join(format!("manifest-{generation}.json"));
        fs::write(&manifest, b"manifest").unwrap();
        fs::write(dir.join(format!("trigram-{generation}.bin")), b"overlay").unwrap();
        let derived = dir.join(format!("derived-{generation}.sqlite"));
        fs::write(&derived, b"database").unwrap();
        fs::create_dir_all(dir.join("readers/generation")).unwrap();
        fs::create_dir(dir.join("pins")).unwrap();
        fs::create_dir(dir.join("nested-repo")).unwrap();
        for name in [
            "pins/live.json",
            "readers/generation/marker",
            "nested-repo/.gitignore",
            ".aftignore",
            "ignored.rs",
        ] {
            fs::write(dir.join(name), b"counted even if ignored by source walks").unwrap();
        }
        symlink(&manifest, dir.join("alias")).unwrap();
        symlink("absent", dir.join("dangling")).unwrap();
        let fifo = std::ffi::CString::new(dir.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let _socket = UnixListener::bind(dir.join("socket")).unwrap();
        let mut cache = ViewSizeCache::default();
        for _ in 0..2 {
            assert_eq!(
                view_tree_bytes(dir, None, &mut cache).unwrap(),
                tree_bytes(dir, None).unwrap()
            );
        }
        // WAL checkpoints and marker refreshes change lengths without changing
        // their parent directory's identity; these files must always be measured.
        fs::write(&derived, vec![0; 8192]).unwrap();
        fs::write(dir.join("pins/live.json"), b"refreshed marker").unwrap();
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        fs::remove_file(dir.join(format!("trigram-{generation}.bin"))).unwrap();
        fs::rename(&manifest, dir.join("renamed.json")).unwrap();
        symlink("renamed.json", &manifest).unwrap();
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        fs::remove_file(&manifest).unwrap();
        fs::write(&manifest, b"replacement manifest").unwrap();
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        let replacement = dir.join("replacement.tmp");
        fs::write(
            &replacement,
            b"atomic replacement has a different inode and length",
        )
        .unwrap();
        fs::rename(&replacement, &manifest).unwrap();
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        fs::write(&manifest, b"out-of-band in-place drift is reconciled").unwrap();
        cache.reconciled = Some(Instant::now() - SIZE_RECONCILE_INTERVAL);
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        let deadline = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            view_tree_bytes(dir, deadline, &mut cache).unwrap(),
            tree_bytes(dir, deadline).unwrap()
        );
        assert!(cache.files.is_empty());
        fs::create_dir_all(dir.join("too/deep/for-the-bound")).unwrap();
        assert_eq!(
            view_tree_bytes(dir, None, &mut cache).unwrap(),
            tree_bytes(dir, None).unwrap()
        );
        assert!(cache.files.is_empty());
    }

    #[test]
    fn warm_size_cache_preserves_eviction_decisions() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b", "c"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 4);
            set_last_bind(&registry, scope, seed as u64);
        }
        measure(&registry, None).unwrap();
        let warm = measure(&registry, None).unwrap();
        let mut full_total = 0;
        for view in &warm.views {
            let (bytes, complete) = tree_bytes(&view.view_dir, None).unwrap();
            assert!(complete);
            assert_eq!(view.bytes, bytes);
            full_total += bytes;
        }
        let a_bytes = view_bytes(&warm, "a");
        registry.set_disk_budget(budget(full_total - a_bytes, full_total * 2));
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(report.evicted, vec![("a".to_owned(), a_bytes)]);
        assert_eq!(report.after.view_store_bytes, full_total - a_bytes);
        assert!(!registered(&registry, "a"));
        assert!(registered(&registry, "b"));
        assert!(registered(&registry, "c"));
    }

    /// A family's view size caches belong to its registry: once the last
    /// handle on a measured registry is dropped, none of its caches remain,
    /// so a long-running process does not keep them for closed families.
    #[test]
    fn size_caches_drop_with_their_registry() {
        let storage = tempfile::tempdir().unwrap();
        let live_before = LIVE_SIZE_CACHES.with(|live| live.get());
        {
            let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
            for (seed, scope) in ["a", "b"].into_iter().enumerate() {
                idle_view(&registry, scope, seed as u64, 4);
            }
            measure(&registry, None).unwrap();
            assert_eq!(
                LIVE_SIZE_CACHES.with(|live| live.get()) - live_before,
                2,
                "one size cache per measured view"
            );
            assert_eq!(registry.view_size_caches().lock().unwrap().len(), 2);
            wait_for_background_enforcement(&registry);
        }
        assert_eq!(
            LIVE_SIZE_CACHES.with(|live| live.get()),
            live_before,
            "a dropped registry's size caches must be dropped with it"
        );
        let reopened = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        assert!(reopened.view_size_caches().lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn tree_bytes_preserves_permission_errors_for_non_searchable_directory() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("permission fixture requires a non-root user");
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::os::unix::fs::symlink("absent", root.join("link")).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o400)).unwrap();
        // Listing still succeeds, but looking up a child requires search
        // permission. Ignoring a symlink's stat must not turn this uncertainty
        // into a complete zero-byte measurement used by admission.
        let listed = fs::read_dir(root).unwrap().count();
        let lookup = fs::symlink_metadata(root.join("link"));
        let measured = tree_bytes(root, None);
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(listed, 1);
        assert_eq!(
            lookup.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            measured.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    fn policy() -> TrigramPolicy {
        TrigramPolicy {
            max_file_size: 1 << 20,
        }
    }

    fn trigram_key(bytes: &[u8]) -> FamilyKey {
        TrigramKey {
            content: ContentHash::of(bytes),
            policy: policy(),
        }
        .family_key()
    }

    fn rel(value: &str) -> RelPath {
        RelPath::new(value.as_bytes().to_vec()).unwrap()
    }

    /// Text with many distinct trigrams, so its trigram payload has real size.
    fn text(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (0..len)
            .map(|index| {
                if index % 61 == 60 {
                    return b'\n';
                }
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                b'a' + ((state >> 33) % 26) as u8
            })
            .collect()
    }

    /// Publishes `files` as the view's next generation through the protection
    /// protocol (live pin, put-or-touch, segment, manifest, pointer), then
    /// writes a `derived_kib` KiB database as that generation's derived
    /// file, standing in for the callgraph database a real generation has.
    fn publish(view: &ViewRegistration, files: &[(&str, Vec<u8>)], derived_kib: usize) -> String {
        let storage = view.registry().storage().to_path_buf();
        let store = view.open_store(FamilyPlane::Trigram).unwrap();
        let mut live = LivePin::create(view).unwrap();
        let keys = files
            .iter()
            .map(|(_, bytes)| trigram_key(bytes))
            .collect::<Vec<_>>();
        live.protect(&keys).unwrap();
        for (_, bytes) in files {
            store
                .put_or_touch(
                    &trigram_key(bytes),
                    &TrigramPayload::extract(bytes, &policy()).encode(),
                )
                .unwrap();
        }
        let members = files
            .iter()
            .map(|(path, bytes)| SegmentMember {
                rel_path: rel(path),
                content: ContentHash::of(bytes),
                size: bytes.len() as u64,
            })
            .collect::<Vec<_>>();
        let segment = segment_store::build_from_blobs(&store, &members, &policy()).unwrap();
        live.protect_segment(&segment.id).unwrap();
        segment_store::write_segment(&store, &storage, &segment, None).unwrap();
        let mut manifest = ManifestV2::new(ManifestHeader {
            producers: Producers {
                trigram: policy().fingerprint_hex(),
                semantic: None,
                callgraph: "callgraph-v1".to_string(),
            },
            head_tree: None,
            ignore_fingerprint: None,
            segment: Some(crate::blob_store::v2::to_hex(&segment.id)),
        });
        for (path, bytes) in files {
            manifest
                .insert(
                    rel(path),
                    EntryV2::regular(
                        ContentHash::of(bytes),
                        bytes.len() as u64,
                        EntryPlanes {
                            trigram: Some(PlaneState::ready(&trigram_key(bytes))),
                            semantic: None,
                            callgraph: None,
                        },
                    ),
                )
                .unwrap();
        }
        let store_view = view.view_store().unwrap();
        let base = store_view.current_generation().unwrap();
        let name = GenerationName::for_manifest(&manifest).unwrap();
        let prepared = store_view
            .prepare_v2(&name, base.as_deref(), &manifest, None)
            .unwrap();
        assert_eq!(
            store_view.commit_v2(prepared, None).unwrap(),
            PublishV2::Published
        );
        drop(live);
        let generation = name.to_string();
        if derived_kib > 0 {
            let path = store_view.derived_path(&generation).unwrap();
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(&format!(
                    "CREATE TABLE pad (value BLOB NOT NULL);
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {derived_kib})
                     INSERT INTO pad SELECT zeroblob(1024) FROM n;"
                ))
                .unwrap();
        }
        generation
    }

    fn set_last_bind(registry: &FamilyRegistry, scope: &str, ms: u64) {
        registry
            .with_barrier(|tx| {
                tx.execute(
                    "UPDATE members SET last_bind_ms = ?2 WHERE scope = ?1",
                    params![scope, ms as i64],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn budget(soft: u64, hard: u64) -> DiskBudget {
        DiskBudget {
            view_store_soft: soft,
            view_store_hard: hard,
            ..DiskBudget::default()
        }
    }

    fn view_bytes(usage: &DiskUsage, scope: &str) -> u64 {
        usage
            .views
            .iter()
            .find(|view| view.scope == scope)
            .unwrap_or_else(|| panic!("{scope} not measured"))
            .bytes
    }

    fn evicted(report: &EnforcementReport) -> Vec<&str> {
        report
            .evicted
            .iter()
            .map(|(scope, _)| scope.as_str())
            .collect()
    }

    fn registered(registry: &FamilyRegistry, scope: &str) -> bool {
        registry.member(scope).unwrap().is_some()
    }

    #[test]
    fn enforcement_under_limit_runs_no_logical_aggregates() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("root");
        fs::create_dir(&root).unwrap();
        let view = registry
            .register_view_with_storage(storage.path(), "active", &root)
            .unwrap();
        publish(&view, &[("file.txt", text(1, 2_000))], 4);
        for plane in [FamilyPlane::Semantic, FamilyPlane::Callgraph] {
            view.open_store(plane)
                .unwrap()
                .put_or_touch(&FamilyKey::new(plane, [1; 32]), b"unreferenced")
                .unwrap();
        }
        crate::blob_store::v2::take_logical_aggregates();
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(crate::blob_store::v2::take_logical_aggregates(), 0);
        assert!(report.evicted.is_empty());
        let sweep = report.sweep.unwrap();
        assert_eq!(sweep.deleted_blobs, 0);
        assert_eq!(sweep.deleted_segments, 0);
        assert!(!report.stopped_early);
    }

    #[test]
    fn enforcement_over_limit_preserves_gc_deleted_keys() {
        fn run(enforcement: bool) -> Vec<FamilyKey> {
            let storage = tempfile::tempdir().unwrap();
            let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
            let root = storage.path().join("root");
            fs::create_dir(&root).unwrap();
            let view = registry
                .register_view_with_storage(storage.path(), "active", &root)
                .unwrap();
            let content = text(1, 2_000);
            publish(&view, &[("file.txt", content.clone())], 4);
            let protected = trigram_key(&content);
            let orphan = FamilyKey::new(FamilyPlane::Trigram, [2; 32]);
            let semantic = FamilyKey::new(FamilyPlane::Semantic, [3; 32]);
            let graph = FamilyKey::new(FamilyPlane::Callgraph, [4; 32]);
            let keys = [protected, orphan, semantic, graph];
            for key in &keys[1..] {
                view.open_store(key.plane())
                    .unwrap()
                    .put_or_touch(key, b"orphan")
                    .unwrap();
            }
            registry.set_disk_budget(DiskBudget {
                family_store_soft: 0,
                family_store_hard: u64::MAX,
                ..DiskBudget::default()
            });
            let sweep = if enforcement {
                let report = enforce(&registry, EnforceOptions::default()).unwrap();
                assert!(report.evicted.is_empty());
                assert!(report.sweep_error.is_none());
                report.sweep.unwrap()
            } else {
                family::sweep_family(&registry, None, FamilySweepPolicy { byte_budget: 0 }, None)
                    .unwrap()
            };
            assert_eq!(sweep.deleted_blobs, 3);
            assert_eq!(sweep.deleted_bytes, 3 * b"orphan".len() as u64);
            assert_eq!(sweep.deleted_segments, 0);
            let deleted = keys
                .into_iter()
                .filter(|key| !view.open_store(key.plane()).unwrap().contains(key).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(deleted, [orphan, semantic, graph]);
            deleted
        }
        assert_eq!(run(true), run(false));
    }

    #[test]
    fn enforcement_payload_budget_uses_exact_totals_below_page_bound() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("root");
        fs::create_dir(&root).unwrap();
        let view = registry
            .register_view_with_storage(storage.path(), "active", &root)
            .unwrap();
        let store = view.open_store(FamilyPlane::Semantic).unwrap();
        let oldest = FamilyKey::new(FamilyPlane::Semantic, [1; 32]);
        let newest = FamilyKey::new(FamilyPlane::Semantic, [2; 32]);
        store.put_or_touch(&oldest, b"orphan").unwrap();
        store.put_or_touch(&newest, b"orphan").unwrap();
        registry.set_disk_budget(DiskBudget {
            family_payload: 6,
            ..DiskBudget::default()
        });
        crate::blob_store::v2::take_logical_aggregates();
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(crate::blob_store::v2::take_logical_aggregates(), 1);
        let sweep = report.sweep.unwrap();
        assert_eq!(sweep.deleted_blobs, 1);
        assert_eq!(sweep.deleted_bytes, 6);
        assert!(!store.contains(&oldest).unwrap());
        assert!(store.contains(&newest).unwrap());
    }

    /// Registers `scope`, publishes one generation of its own content, and
    /// ends its session.
    fn idle_view(registry: &FamilyRegistry, scope: &str, seed: u64, derived_kib: usize) {
        let root = registry.storage().join("roots").join(scope);
        fs::create_dir_all(&root).unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), scope, &root)
            .unwrap();
        publish(&view, &[("file.txt", text(seed, 2_000))], derived_kib);
    }

    /// Over the soft limit, idle views go least recently bound first, and
    /// eviction stops as soon as the view store is back under the limit.
    #[test]
    fn eviction_takes_the_least_recently_bound_idle_views_first() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b", "c"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 64);
        }
        set_last_bind(&registry, "a", 3_000);
        set_last_bind(&registry, "b", 1_000);
        set_last_bind(&registry, "c", 2_000);
        let usage = measure(&registry, None).unwrap();
        assert!(usage.views.iter().all(|view| view.hold.is_none()));
        // Room for exactly one view: the two oldest must go, oldest first.
        registry.set_disk_budget(budget(view_bytes(&usage, "a"), u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert_eq!(evicted(&report), ["b", "c"]);
        assert!(registered(&registry, "a"));
        assert!(!registered(&registry, "b") && !registered(&registry, "c"));
        assert!(report.after.view_store_bytes <= view_bytes(&usage, "a"));
        assert!(report.over_hard.is_empty());
    }

    /// A view whose session is alive stays even when it is the least
    /// recently bound and the limit is not met without it.
    #[test]
    fn an_active_session_is_never_evicted() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("root-active");
        fs::create_dir_all(&root).unwrap();
        let active = registry
            .register_view_with_storage(registry.storage(), "active", &root)
            .unwrap();
        publish(&active, &[("file.txt", text(1, 2_000))], 64);
        idle_view(&registry, "idle", 2, 64);
        set_last_bind(&registry, "active", 1);
        set_last_bind(&registry, "idle", 2);
        registry.set_disk_budget(budget(0, u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert_eq!(evicted(&report), ["idle"]);
        assert!(report
            .held
            .contains(&("active".to_string(), Hold::ActiveSession)));
        assert!(registered(&registry, "active"));
        let store = active.view_store().unwrap();
        let generation = store.current_generation().unwrap().unwrap();
        assert!(store.manifest_path(&generation).unwrap().is_file());
        assert!(store.derived_path(&generation).unwrap().is_file());
        drop(active);
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(
            evicted(&report),
            ["active"],
            "a session that ended frees it"
        );
    }

    /// An old generation pinned by a reader keeps its whole view, though the
    /// view has no session and is over the limit.
    #[test]
    fn a_pinned_old_generation_keeps_its_view() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), "pinned", &root)
            .unwrap();
        let old = publish(&view, &[("file.txt", text(1, 2_000))], 32);
        let view_dir = view.view_dir().to_path_buf();
        // Pin while the generation is current, before publication can reclaim it.
        let pin = view
            .under_pin_barrier(|dir| QueryPin::acquire(dir, &old))
            .unwrap();
        let current = publish(&view, &[("file.txt", text(2, 2_000))], 32);
        assert_ne!(old, current);
        drop(view);
        registry.set_disk_budget(budget(0, u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert!(evicted(&report).is_empty(), "{report:?}");
        assert!(report
            .held
            .contains(&("pinned".to_string(), Hold::Protected)));
        let store = crate::views::ViewStore::existing_dir(view_dir.clone()).unwrap();
        assert!(store.manifest_path(&old).unwrap().is_file());
        assert!(store.derived_path(&old).unwrap().is_file());
        drop(pin);
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(evicted(&report), ["pinned"]);
        assert!(!view_dir.exists());
    }

    /// An assembly in progress keeps a view, and so does a pin file that
    /// cannot be parsed: an unreadable pin might be protecting a generation,
    /// so it is never guessed away to meet the limit.
    #[test]
    fn assemblies_and_unreadable_protection_keep_their_views() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let assembling = registry
            .register_view_with_storage(registry.storage(), "assembling", &root)
            .unwrap();
        publish(&assembling, &[("file.txt", text(1, 2_000))], 16);
        let assembly = AssemblyPin::create_v2(&assembling, "next-generation", &[]).unwrap();
        drop(assembling);
        idle_view(&registry, "malformed", 2, 16);
        let malformed_dir = registry.member("malformed").unwrap().unwrap().view_dir;
        fs::create_dir_all(malformed_dir.join("pins")).unwrap();
        fs::write(malformed_dir.join("pins").join("broken.json"), b"{not json").unwrap();
        registry.set_disk_budget(budget(0, u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert!(evicted(&report).is_empty(), "{report:?}");
        assert!(report
            .held
            .contains(&("assembling".to_string(), Hold::Protected)));
        assert!(report
            .held
            .iter()
            .any(|(scope, hold)| scope == "malformed" && matches!(hold, Hold::Uncertain(_))));
        drop(assembly);
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(evicted(&report), ["assembling"]);
        assert!(registered(&registry, "malformed"));
    }

    /// When everything over the hard limit is held, nothing is deleted and a
    /// new view is refused with the named reason; an existing view still
    /// re-binds. Admission decides from the last recorded measurement and
    /// never evicts inline: a refusal starts a background pass, and once a
    /// session has ended, that pass evicts it and the next registration of
    /// the new view is admitted.
    #[test]
    fn held_bytes_over_the_hard_limit_refuse_new_views_by_name() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let mut sessions = Vec::new();
        for (seed, scope) in ["a", "b"].into_iter().enumerate() {
            let root = storage.path().join(scope);
            fs::create_dir_all(&root).unwrap();
            let view = registry
                .register_view_with_storage(registry.storage(), scope, &root)
                .unwrap();
            publish(&view, &[("file.txt", text(seed as u64, 2_000))], 64);
            sessions.push(view);
        }
        let usage = measure(&registry, None).unwrap();
        let smallest = usage.views.iter().map(|view| view.bytes).min().unwrap();
        // A new view reserves the largest view's bytes: that does not fit
        // beside both held views, but fits beside one of them.
        registry.set_disk_budget(budget(0, usage.view_store_bytes + smallest / 2));
        let new_root = storage.path().join("c");
        fs::create_dir_all(&new_root).unwrap();

        let refused = registry
            .register_view_with_storage(registry.storage(), "c", &new_root)
            .unwrap_err();

        let RegistryError::AdmissionQueued(reason) = &refused else {
            panic!("unexpected error: {refused}");
        };
        assert!(reason.starts_with("queued: view store disk"), "{reason}");
        assert!(reason.contains("held by active sessions"), "{reason}");
        assert!(reason.contains("eviction pass"), "{reason}");
        assert!(!registered(&registry, "c"));
        wait_for_background_enforcement(&registry);
        let after = measure(&registry, None).unwrap();
        assert_eq!(
            after.view_store_bytes, usage.view_store_bytes,
            "the background pass deleted nothing held"
        );
        let rebound = registry
            .register_view_with_storage(registry.storage(), "a", &storage.path().join("a"))
            .expect("an existing view re-binds over the limit");
        drop(rebound);

        let b = sessions.pop().unwrap();
        drop(b);
        // The last record still shows b held, so this attempt is refused
        // too, and its background pass now finds b idle and evicts it.
        assert!(matches!(
            registry.register_view_with_storage(registry.storage(), "c", &new_root),
            Err(RegistryError::AdmissionQueued(_))
        ));
        wait_for_background_enforcement(&registry);
        assert!(!registered(&registry, "b"), "b was evicted to admit c");
        let admitted = registry
            .register_view_with_storage(registry.storage(), "c", &new_root)
            .unwrap();
        assert!(registered(&registry, "c"));
        drop(admitted);
    }

    /// Admission on the registration path reads recorded totals only: with
    /// a family of many views it walks no directory, whether it admits or
    /// refuses, and a refusal leaves the eviction to a background pass.
    #[test]
    fn admission_walks_no_directory_even_for_a_large_family() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for index in 0..40u64 {
            idle_view(&registry, &format!("idle-{index:02}"), index, 8);
        }
        let usage = measure(&registry, None).unwrap();
        assert_eq!(usage.views.len(), 40);
        let walks = || TREE_WALKS.with(std::cell::Cell::get);

        let before = walks();
        let admitted = registry
            .register_view_with_storage(
                registry.storage(),
                "admitted",
                &storage.path().join("admitted"),
            )
            .unwrap();
        assert_eq!(walks(), before, "admitting walked a directory");

        registry.set_disk_budget(budget(0, usage.largest_view() * 3));
        let before = walks();
        let refused = registry
            .register_view_with_storage(
                registry.storage(),
                "refused",
                &storage.path().join("refused"),
            )
            .unwrap_err();
        assert_eq!(walks(), before, "refusing walked a directory");
        assert!(
            matches!(&refused, RegistryError::AdmissionQueued(reason)
                if reason.starts_with("queued: view store disk")),
            "{refused}"
        );
        assert_eq!(
            registry.members().unwrap().len(),
            41,
            "nothing was evicted on the registration path"
        );

        wait_for_background_enforcement(&registry);
        let members = registry.members().unwrap();
        assert_eq!(
            members
                .iter()
                .map(|member| member.scope.as_str())
                .collect::<Vec<_>>(),
            ["admitted"],
            "the background pass evicted every idle view"
        );
        let retried = registry
            .register_view_with_storage(
                registry.storage(),
                "refused",
                &storage.path().join("refused"),
            )
            .expect("admitted after the background pass");
        drop(retried);
        drop(admitted);
    }

    /// A key two views rely on is counted once and belongs to neither
    /// view's exclusive share. Evicting one view then reclaims exactly its
    /// exclusive keys and segment file. A small database keeps the deleted
    /// payload's pages as reusable reserve instead of compacting on every pass.
    #[test]
    fn shared_keys_count_once_and_eviction_preserves_small_database_reserve() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let shared = text(100, 60_000);
        let only_a = text(101, 60_000);
        let only_b = text(102, 60_000);
        let root_a = storage.path().join("a");
        let root_b = storage.path().join("b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        let a = registry
            .register_view_with_storage(registry.storage(), "a", &root_a)
            .unwrap();
        publish(
            &a,
            &[("shared.txt", shared.clone()), ("a.txt", only_a.clone())],
            16,
        );
        let b = registry
            .register_view_with_storage(registry.storage(), "b", &root_b)
            .unwrap();
        publish(
            &b,
            &[("shared.txt", shared.clone()), ("b.txt", only_b.clone())],
            16,
        );
        let store = b.open_store(FamilyPlane::Trigram).unwrap();
        let sizes = store
            .rows()
            .unwrap()
            .into_iter()
            .map(|row| (*row.key.as_bytes(), row.payload_bytes))
            .collect::<BTreeMap<_, _>>();
        let segments = store.segments().unwrap();
        assert_eq!(segments.len(), 2, "one segment per view");
        let segment_bytes = segments.iter().map(|row| row.byte_len).sum::<u64>();
        let shared_key = sizes[trigram_key(&shared).as_bytes()];
        let a_key = sizes[trigram_key(&only_a).as_bytes()];
        let b_key = sizes[trigram_key(&only_b).as_bytes()];

        let accounting = account_keys(&registry, None).unwrap();

        assert!(accounting.complete);
        assert_eq!(accounting.shared_bytes, shared_key);
        assert_eq!(
            accounting.referenced_bytes,
            shared_key + a_key + b_key + segment_bytes
        );
        let a_segment = accounting.exclusive_bytes["a"] - a_key;
        let b_segment = accounting.exclusive_bytes["b"] - b_key;
        assert_eq!(a_segment + b_segment, segment_bytes);
        assert_eq!(accounting.unreferenced_bytes, 0);

        drop(a);
        registry.set_disk_budget(budget(0, u64::MAX));
        let plane_before = store.usage().unwrap();
        assert!(plane_before.file_bytes < 256 * 1024 * 1024);
        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert_eq!(evicted(&report), ["a"]);
        let sweep = report.sweep.as_ref().expect("the sweep ran");
        assert_eq!(sweep.deleted_blobs, 1);
        assert_eq!(sweep.deleted_segments, 1);
        assert_eq!(sweep.deleted_bytes, a_key + a_segment);
        assert!(store.get(&trigram_key(&shared)).unwrap().is_some());
        assert!(store.get(&trigram_key(&only_b)).unwrap().is_some());
        assert!(store.get(&trigram_key(&only_a)).unwrap().is_none());
        assert!(report.after.view_store_bytes < report.before.view_store_bytes);
        assert!(
            report.after.family_store.file_bytes < report.before.family_store.file_bytes,
            "{} -> {}",
            report.before.family_store.file_bytes,
            report.after.family_store.file_bytes
        );
        let plane_after = store.usage().unwrap();
        // The pass set up the store's payload totals, whose table takes one
        // page; no other page was added and none was returned.
        let page_size: u64 = 4096;
        assert_eq!(plane_after.file_bytes, plane_before.file_bytes + page_size);
        assert!(plane_after.freelist_pages > plane_before.freelist_pages);
        assert_eq!(sweep.space_reclaim.reclaimed_pages, 0);
        assert_eq!(sweep.space_reclaim.steps, 0);
        let after = account_keys(&registry, None).unwrap();
        assert_eq!(after.shared_bytes, 0);
        assert_eq!(after.referenced_bytes, shared_key + b_key + b_segment);
        drop(b);
    }

    /// An eviction interrupted after it removed the pointer leaves the
    /// member `evicting`; the next pass finishes it, and a registration in
    /// between clears the leftovers instead of reusing them.
    #[test]
    fn an_interrupted_eviction_is_finished_by_the_next_pass_or_registration() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for scope in ["next-pass", "rebind"] {
            idle_view(&registry, scope, 7, 16);
            let member = registry.member(scope).unwrap().unwrap();
            // What a killed eviction leaves: state committed, pointer gone,
            // the rest of the directory still there.
            registry
                .with_barrier(|tx| {
                    tx.execute(
                        "UPDATE members SET state = ?2 WHERE scope = ?1",
                        params![scope, STATE_EVICTING],
                    )?;
                    Ok(())
                })
                .unwrap();
            remove_pointer(
                &crate::bash_background::persistence::PinnedDir::repair_open_path(&member.view_dir)
                    .unwrap(),
            )
            .unwrap();
            assert!(registry.member(scope).unwrap().unwrap().evicting);
        }
        let rebind_dir = registry.member("rebind").unwrap().unwrap().view_dir;
        let rebound = registry
            .register_view_with_storage(
                registry.storage(),
                "rebind",
                &storage.path().join("roots").join("rebind"),
            )
            .unwrap();
        let leftovers = fs::read_dir(&rebind_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("derived-") || name.starts_with("manifest-"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(!registry.member("rebind").unwrap().unwrap().evicting);

        // The soft limit is not exceeded; the pass finishes the eviction
        // because it began, not to meet a number.
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(evicted(&report), ["next-pass"]);
        assert!(registered(&registry, "rebind"));
        drop(rebound);
    }

    // -----------------------------------------------------------------------
    // Rebuilding after eviction, against cold oracles

    use crate::views::contracts::{PlaneLoader, ViewAccess};
    use crate::views::first_load::{
        CallgraphBridge, CheckoutDriver, ConfiguredMembershipWalker, MembershipWalker,
        SiblingLoader, TrigramBridge,
    };
    use std::sync::Arc;

    const SOURCES: &[(&str, &str)] = &[
        (
            "src/alpha.rs",
            "pub fn alpha_total(values: &[u32]) -> u32 {\n    beta_sum(values)\n}\n\npub fn alpha_label() -> &'static str {\n    \"alpha\"\n}\n",
        ),
        (
            "src/beta.rs",
            "pub fn beta_sum(values: &[u32]) -> u32 {\n    values.iter().copied().sum()\n}\n\npub struct BetaCache {\n    entries: Vec<String>,\n}\n\nimpl BetaCache {\n    pub fn insert(&mut self, value: String) {\n        self.entries.push(value);\n    }\n}\n",
        ),
        (
            "src/gamma.rs",
            "fn gamma_parse(input: &str) -> Option<u64> {\n    input.trim().parse().ok()\n}\n",
        ),
        ("notes.txt", "alpha notes, not a source file\n"),
    ];

    fn write_tree(root: &Path, extra: &str) {
        for (path, text) in SOURCES {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        fs::write(
            root.join("src").join("local.rs"),
            format!("pub fn {extra}_local() {{ crate::alpha::alpha_label(); }}\n"),
        )
        .unwrap();
    }

    /// Every observable of the trigram and callgraph planes for one loaded
    /// checkout: membership, matched lines for a few literals, and the
    /// derived database's logical rows.
    #[derive(Debug, PartialEq, Eq)]
    struct PlaneObservation {
        membership: Vec<String>,
        matches: Vec<String>,
        callgraph: crate::views::materialization::parity::LogicalSnapshot,
    }

    /// Registers and loads `scope` with the real trigram and callgraph
    /// planes, observes it, and drops every handle, which ends its session.
    fn load_and_observe(registry: &FamilyRegistry, scope: &str, root: &Path) -> PlaneObservation {
        let owner = registry
            .register_view_with_storage(registry.storage(), scope, root)
            .unwrap();
        let trigram = Arc::new(TrigramBridge::new(
            registry.storage().to_path_buf(),
            policy(),
        ));
        let callgraph = Arc::new(CallgraphBridge::default());
        let driver = Arc::new(
            CheckoutDriver::new(
                owner.clone(),
                Producers {
                    trigram: policy().fingerprint_hex(),
                    semantic: None,
                    callgraph: crate::views::callgraph::PRODUCER.into(),
                },
                None,
                Arc::new(ConfiguredMembershipWalker),
                vec![trigram.clone(), callgraph.clone()],
            )
            .with_adapters(vec![trigram.adapter.clone(), callgraph.adapter.clone()]),
        );
        let loaded = SiblingLoader::new(
            driver,
            vec![trigram.adapter.clone(), callgraph.adapter.clone()],
        )
        .load(&ViewAccess::Owner(owner.clone()))
        .unwrap();
        assert!(
            loaded.pending_planes.is_empty(),
            "{scope}: {:?}",
            loaded.pending_planes
        );
        let generation = loaded.snapshot.generation().name().to_owned();
        let membership = loaded
            .snapshot
            .membership()
            .into_keys()
            .map(|path| String::from_utf8_lossy(path.as_bytes()).into_owned())
            .collect();
        let index = trigram.adapter.resident(scope, &generation).unwrap();
        let mut matches = Vec::new();
        for literal in ["alpha", "beta_sum", "_local", "entries"] {
            let answer = index.query(root, &loaded.snapshot, literal);
            assert!(
                answer.gaps.is_empty(),
                "{scope} {literal}: {:?}",
                answer.gaps
            );
            matches.push(format!("{literal}: {:?}", answer.matches));
        }
        let derived = owner
            .view_store()
            .unwrap()
            .derived_path(&generation)
            .unwrap();
        drop(index);
        drop(loaded);
        drop(trigram);
        drop(callgraph);
        drop(owner);
        PlaneObservation {
            membership,
            matches,
            callgraph: crate::views::materialization::parity::logical_snapshot(&derived),
        }
    }

    /// An evicted view re-binds as a new view: it seeds from its sibling,
    /// strictly reconciles its checkout and rebuilds, and every trigram and
    /// callgraph observable equals a cold build of the same checkout in an
    /// empty, isolated store.
    #[test]
    fn a_view_rebuilt_after_eviction_matches_a_cold_build_in_every_plane() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let root_a = roots.path().join("a");
        let root_b = roots.path().join("b");
        write_tree(&root_a, "a");
        write_tree(&root_b, "b");
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let b_session = registry
            .register_view_with_storage(registry.storage(), "b", &root_b)
            .unwrap();
        load_and_observe(&registry, "b", &root_b);
        let first = load_and_observe(&registry, "a", &root_a);
        registry.set_disk_budget(budget(0, u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert_eq!(evicted(&report), ["a"], "{:?}", report.held);
        let a_dir = crate::views::registry::view_dir(storage.path(), "a").unwrap();
        assert!(!a_dir.exists());
        // The checkout changes while its view is evicted.
        fs::write(
            root_a.join("src").join("delta.rs"),
            "pub fn delta_entries() {}\n",
        )
        .unwrap();
        fs::remove_file(root_a.join("src").join("gamma.rs")).unwrap();

        let rebuilt = load_and_observe(&registry, "a", &root_a);

        let cold_storage = tempfile::tempdir().unwrap();
        let cold_registry = FamilyRegistry::open(cold_storage.path(), FAMILY).unwrap();
        let cold = load_and_observe(&cold_registry, "a", &root_a);
        assert_eq!(rebuilt, cold);
        assert_ne!(
            first.membership, rebuilt.membership,
            "the edit is reflected"
        );
        drop(b_session);
    }

    fn vector(text: &str) -> Vec<f32> {
        blake3::hash(text.as_bytes()).as_bytes()[..16]
            .iter()
            .map(|byte| (f32::from(*byte) - 127.5) / 127.5)
            .collect()
    }

    type SemanticRow = (String, String, u32, u32);

    fn semantic_rows(
        root: &Path,
        results: &[crate::semantic_index::SemanticResult],
    ) -> Vec<SemanticRow> {
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

    fn semantic_view(
        storage: &Path,
        scope: &str,
        root: &Path,
    ) -> crate::views::semantic_runtime::CheckoutSemantic {
        let runtime = crate::views::semantic_runtime::CheckoutSemantic::new(
            &crate::root_cache::StoragePaths::same(storage),
            FAMILY,
            scope,
            root,
            crate::views::semantic::SemanticProducer::current(
                "eviction-test-model",
                crate::semantic_index::EmbedTextCaps::default(),
            ),
            std::sync::Weak::new(),
        )
        .unwrap();
        runtime.load().unwrap();
        runtime
            .refresh(
                crate::views::semantic::FillBudget::default(),
                &mut |texts: Vec<String>| Ok(texts.iter().map(|text| vector(text)).collect()),
            )
            .unwrap();
        runtime
    }

    /// The semantic plane of an evicted view, rebuilt on its next bind,
    /// ranks exactly like a full build of the checkout.
    #[test]
    fn a_semantic_view_rebuilt_after_eviction_ranks_like_a_cold_build() {
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let root_a = roots.path().join("a");
        let root_b = roots.path().join("b");
        write_tree(&root_a, "a");
        write_tree(&root_b, "b");
        let b = semantic_view(storage.path(), "b", &root_b);
        drop(semantic_view(storage.path(), "a", &root_a));
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        registry.set_disk_budget(budget(0, u64::MAX));

        let report = enforce(&registry, EnforceOptions::default()).unwrap();

        assert_eq!(evicted(&report), ["a"], "{:?}", report.held);
        fs::write(
            root_a.join("src").join("delta.rs"),
            "pub fn delta_cache_insert() {}\n",
        )
        .unwrap();
        let a = semantic_view(storage.path(), "a", &root_a);
        let query = "beta cache insert";
        let answer = a.search(&vector(query), 100, &|_| true).unwrap();
        assert!(answer.complete(), "{answer:?}");

        let files = ConfiguredMembershipWalker.files(&root_a).unwrap();
        let cold = crate::semantic_index::SemanticIndex::build(
            &root_a,
            &files,
            &mut |batch: Vec<String>| Ok(batch.iter().map(|text| vector(text)).collect()),
            64,
        )
        .unwrap();
        assert_eq!(
            semantic_rows(&root_a, &answer.results),
            semantic_rows(&root_a, &cold.search(&vector(query), 100))
        );
        drop(a);
        drop(b);
    }

    /// A view whose checkout root is gone is removed only after two passes
    /// find it with no protection of any kind. Disk limits do not shorten
    /// that: protected only by a reader's read marker, it stays, with its
    /// protected generation, through two passes and through a pass that is
    /// over the limit; after the reader leaves, two further passes remove it.
    #[test]
    fn a_missing_root_view_held_only_by_a_reader_marker_survives_enforcement() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let root = storage.path().join("removed-root");
        fs::create_dir_all(&root).unwrap();
        let view = registry
            .register_view_with_storage(registry.storage(), "removed", &root)
            .unwrap();
        let generation = publish(&view, &[("file.txt", text(5, 2_000))], 16);
        let view_dir = view.view_dir().to_path_buf();
        drop(view);
        registry
            .with_barrier(|tx| {
                tx.execute(
                    "UPDATE members SET last_bind_ms = 0 WHERE scope = 'removed'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let binding = storage.path().join(format!(
            "retention/roots/{}.json",
            crate::path_identity::project_scope_key(&root)
        ));
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&binding).unwrap()).unwrap();
        record["last_bound_ms"] = serde_json::json!(0);
        fs::write(binding, serde_json::to_vec(&record).unwrap()).unwrap();
        fs::remove_dir_all(&root).unwrap();
        let reader = registry.register_reader("parent-folder").unwrap();
        let marker = reader.protect_current("removed").unwrap().unwrap();
        assert_eq!(marker.generation(), generation);

        for pass in 0..2 {
            let report = enforce(&registry, EnforceOptions::default()).unwrap();
            assert!(evicted(&report).is_empty(), "pass {pass}");
            let sweep = report.sweep.expect("the sweep ran");
            assert!(sweep.deregistered.is_empty(), "pass {pass}");
        }
        registry.set_disk_budget(budget(0, u64::MAX));
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert!(evicted(&report).is_empty());
        assert!(report
            .held
            .contains(&("removed".to_string(), Hold::Protected)));
        let store = crate::views::ViewStore::existing_dir(view_dir.clone()).unwrap();
        assert!(store.manifest_path(&generation).unwrap().is_file());
        assert!(registered(&registry, "removed"));

        drop(marker);
        drop(reader);
        registry.set_disk_budget(DiskBudget::default());
        let first = enforce(&registry, EnforceOptions::default()).unwrap();
        assert!(first.sweep.unwrap().deregistered.is_empty());
        assert!(registered(&registry, "removed") && view_dir.is_dir());
        let second = enforce(&registry, EnforceOptions::default()).unwrap();
        assert_eq!(second.sweep.unwrap().deregistered, ["removed"]);
        assert!(!registered(&registry, "removed") && !view_dir.exists());
    }

    /// Occupancy of a small family of real views (trigram and callgraph
    /// planes) against the configured default limits, printed for the
    /// record. At the defaults nothing is evicted.
    #[test]
    fn reports_occupancy_at_the_default_limits() {
        assert_eq!(VIEW_STORE_SOFT_BYTES, 4 * 1024 * 1024 * 1024);
        assert_eq!(VIEW_STORE_HARD_BYTES, 6 * 1024 * 1024 * 1024);
        let storage = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let mut sessions = Vec::new();
        for scope in ["main", "feature", "review"] {
            let root = roots.path().join(scope);
            write_tree(&root, scope);
            sessions.push(
                registry
                    .register_view_with_storage(registry.storage(), scope, &root)
                    .unwrap(),
            );
            load_and_observe(&registry, scope, &root);
        }
        let usage = measure(&registry, None).unwrap();
        let keys = account_keys(&registry, None).unwrap();
        let percent = |bytes: u64, of: u64| bytes as f64 * 100.0 / of as f64;
        for view in &usage.views {
            eprintln!(
                "occupancy view={} bytes={} exclusive_family_bytes={} hold={:?}",
                view.scope,
                view.bytes,
                keys.exclusive_bytes.get(&view.scope).copied().unwrap_or(0),
                view.hold
            );
        }
        eprintln!(
            "occupancy view_store_bytes={} ({:.6}% of the {} byte soft limit) family_store_file_bytes={} \
             ({:.6}% of the {} byte soft limit) payload_bytes={} referenced_bytes={} shared_bytes={}",
            usage.view_store_bytes,
            percent(usage.view_store_bytes, VIEW_STORE_SOFT_BYTES),
            VIEW_STORE_SOFT_BYTES,
            usage.family_store.file_bytes,
            percent(usage.family_store.file_bytes, FAMILY_STORE_SOFT_BYTES),
            FAMILY_STORE_SOFT_BYTES,
            family_payload_bytes(&registry).unwrap(),
            keys.referenced_bytes,
            keys.shared_bytes
        );
        assert!(usage.complete && keys.complete);
        assert!(keys.shared_bytes > 0, "the three checkouts share content");
        drop(sessions);
        let report = enforce(&registry, EnforceOptions::default()).unwrap();
        assert!(evicted(&report).is_empty());
        assert!(report.over_hard.is_empty());
    }
    #[test]
    fn disk_limits_and_retention_share_cadence_and_admission_bypasses_it() {
        let storage = tempfile::tempdir().unwrap();
        let registry = two_view_family(storage.path());
        // Establish retention's completion clock even if registration already
        // requested background enforcement for this family.
        family::sweep_family_scheduled(&registry, SweepBounds::default(), true).unwrap();
        let disk =
            enforce_with_schedule(&registry, EnforceOptions::default(), Some(false)).unwrap();
        assert_eq!(disk.sweep.unwrap().skipped, Some("interval"));
        let access =
            crate::blob_store::v2::StoreWriteAccess::for_registered_view(storage.path(), FAMILY);
        let store =
            crate::blob_store::v2::FamilyStore::open(&access, FamilyPlane::Semantic).unwrap();
        let garbage = crate::blob_store::v2::FamilyKey::new(FamilyPlane::Semantic, [99; 32]);
        store.put_or_touch(&garbage, b"unreferenced").unwrap();
        let before = store.gc_epoch().unwrap();
        registry.set_disk_budget(budget(u64::MAX, 1));
        assert!(matches!(
            admit_new_view(&registry),
            Err(RegistryError::AdmissionQueued(_))
        ));
        wait_for_background_enforcement(&registry);
        assert!(store.gc_epoch().unwrap() > before);
        assert!(
            !store.contains(&garbage).unwrap(),
            "hard-limit admission must force collection inside the interval"
        );
        let retention =
            family::sweep_family_scheduled(&registry, SweepBounds::default(), false).unwrap();
        assert_eq!(retention.skipped, Some("interval"));
    }

    #[test]
    fn enforcement_flight_is_released_when_worker_unwinds() {
        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("registry.sqlite");
        lock_scheduler().pending.insert(key.clone());
        let result = std::panic::catch_unwind(|| {
            let _flight = EnforcementFlight(key.clone());
            panic!("injected enforcement failure");
        });
        assert!(result.is_err());
        assert!(!is_running(&key));
    }

    #[test]
    fn repeated_hard_limit_admissions_do_not_starve_a_queued_family() {
        if std::env::var_os("AFT_TEST_FAIR_SWEEP_CHILD").is_none() {
            // The queue is process-global. Run this ordering fixture alone so
            // unrelated tests cannot start a worker against its seeded queue.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "views::eviction::tests::repeated_hard_limit_admissions_do_not_starve_a_queued_family", "--nocapture", "--test-threads=1"])
                .env("AFT_TEST_FAIR_SWEEP_CHILD", "1")
                .output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("test result: ok. 1 passed;"),
                "isolated fairness test failed:\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let stuck_storage = tempfile::tempdir().unwrap();
        let queued_storage = tempfile::tempdir().unwrap();
        let stuck = FamilyRegistry::open(stuck_storage.path(), FAMILY).unwrap();
        let queued = FamilyRegistry::open(queued_storage.path(), FAMILY).unwrap();
        let root = stuck_storage.path().join("held-root");
        fs::create_dir_all(&root).unwrap();
        let held = stuck
            .register_view_with_storage(stuck_storage.path(), "held", &root)
            .unwrap();
        publish(&held, &[("held.txt", text(1, 2_000))], 8);
        idle_view(&queued, "idle", 2, 8);
        wait_for_background_enforcement(&stuck);
        wait_for_background_enforcement(&queued);
        stuck.set_disk_budget(budget(0, 1));
        queued.set_disk_budget(budget(0, 1));
        assert!(measure(&stuck, None).unwrap().view_store_bytes > 1);
        assert!(measure(&queued, None).unwrap().view_store_bytes > 1);
        {
            let mut scheduler = lock_scheduler();
            assert!(!scheduler.runner && scheduler.queue.is_empty());
            scheduler.runner = true;
            scheduler.pending.insert(stuck.path().to_path_buf());
            scheduler.pending.insert(queued.path().to_path_buf());
            scheduler.queue.push_back((queued.clone(), true));
        }
        let mut passes = 0;
        let mut retries = 0;
        let mut queued_pass = None;
        run_enforcement_queue_with(stuck.clone(), true, |registry, force| {
            passes += 1;
            if registry.path() == stuck.path() && retries < 4 {
                // Retry while this family is pending, as a caller can during
                // a slow enforcement pass. The held session cannot be evicted.
                assert!(matches!(
                    admit_new_view(registry),
                    Err(RegistryError::AdmissionQueued(_))
                ));
                retries += 1;
            }
            run_enforcement_pass(registry, force);
            if registry.path() == queued.path() {
                queued_pass = Some(passes);
            }
        });
        assert_eq!(retries, 4);
        assert!(recorded_totals(&stuck).unwrap().unwrap().view_store_bytes > 1);
        assert!(
            queued.member("idle").unwrap().is_none(),
            "the queued family's over-limit view must be removed"
        );
        assert_eq!(
            family_payload_bytes(&queued).unwrap(),
            0,
            "the queued family must also be swept"
        );
        assert_eq!(
            queued_pass,
            Some(2),
            "an older queued family must run before a forced retry"
        );
    }

    /// Background passes of many families run one at a time, process-wide:
    /// each pass walks directories on the same disk, and running them all
    /// at once made every one of them stop at its deadline. A pass requested
    /// while another runs is queued, not dropped, and runs afterwards.
    #[test]
    fn background_passes_of_many_families_run_one_at_a_time() {
        use std::sync::atomic::Ordering;
        struct ResetDelay;
        impl Drop for ResetDelay {
            fn drop(&mut self) {
                PASS_TEST_DELAY_MS.store(0, Ordering::SeqCst);
            }
        }
        let _reset = ResetDelay;
        let storages = (0..4)
            .map(|_| tempfile::tempdir().unwrap())
            .collect::<Vec<_>>();
        let registries = storages
            .iter()
            .enumerate()
            .map(|(seed, storage)| {
                let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
                idle_view(&registry, "main", seed as u64, 8);
                registry
            })
            .collect::<Vec<_>>();
        // Long enough that the passes would overlap if more than one could
        // run at a time.
        PASS_TEST_DELAY_MS.store(150, Ordering::SeqCst);
        MOST_CONCURRENT_PASSES.store(0, Ordering::SeqCst);

        let outcomes = registries
            .iter()
            .map(|registry| spawn_enforcement(registry.clone(), false))
            .collect::<Vec<_>>();
        for registry in &registries {
            wait_for_background_enforcement(registry);
        }

        let most = MOST_CONCURRENT_PASSES.load(Ordering::SeqCst);
        eprintln!("most concurrent disk-limit passes: {most} outcomes={outcomes:?}");
        assert_eq!(most, 1, "disk-limit passes ran concurrently");
        assert!(
            matches!(outcomes[0], Scheduled::Started | Scheduled::Queued),
            "{outcomes:?}"
        );
        assert!(
            outcomes[1..]
                .iter()
                .all(|outcome| *outcome == Scheduled::Queued),
            "later families wait for the running pass: {outcomes:?}"
        );
        let runs = last_runs().lock().unwrap();
        for registry in &registries {
            assert!(
                runs.contains_key(registry.path()),
                "a queued pass was dropped instead of run"
            );
        }
        drop(runs);
        for registry in &registries {
            assert!(
                !enforcement_due(registry),
                "a pass that ran is not due again before the interval"
            );
        }
    }

    /// The recorded totals without the completeness flag, which is the one
    /// field an incomplete pass is expected to change.
    fn recorded_bytes(registry: &FamilyRegistry) -> (u64, u64, u64, u64) {
        let totals = recorded_totals(registry).unwrap().expect("recorded");
        (
            totals.view_store_bytes,
            totals.held_view_bytes,
            totals.largest_view_bytes,
            totals.family_store_file_bytes,
        )
    }

    /// A pass that reaches its deadline measures nothing to the end: every
    /// walk after the deadline reports zero bytes. That must not replace the
    /// recorded totals, which admission reads, and the log must not show the
    /// partial sum as a total.
    #[test]
    fn a_pass_stopped_at_its_deadline_keeps_the_recorded_totals() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 64);
        }
        let complete = measure(&registry, None).unwrap();
        assert!(complete.complete);
        let before = recorded_bytes(&registry);
        assert!(before.0 > 0 && before.3 > 0, "{before:?}");
        assert!(recorded_totals(&registry).unwrap().unwrap().complete);

        let report = enforce(
            &registry,
            EnforceOptions {
                deadline: Some(Instant::now()),
                observer: None,
            },
        )
        .unwrap();

        assert!(report.stopped_early && !report.after.complete);
        assert_eq!(
            report.after.view_store_bytes, 0,
            "the walks after the deadline measured nothing"
        );
        assert_eq!(
            recorded_bytes(&registry),
            before,
            "an incomplete measurement replaced the recorded totals"
        );
        assert!(
            !recorded_totals(&registry).unwrap().unwrap().complete,
            "the record says its last measurement did not finish"
        );
        let line = report_line(FAMILY, &report);
        assert!(
            line.contains("view_store_bytes=unknown->unknown held_bytes=unknown")
                && line.contains("family_store_bytes=unknown->unknown"),
            "{line}"
        );
        assert!(report.over_hard.is_empty());

        // A later complete measurement replaces the record again. The family
        // store's files have grown meanwhile (registry and write-ahead log
        // writes), so only the view-store totals are compared.
        measure(&registry, None).unwrap();
        assert_eq!(recorded_bytes(&registry).0, before.0);
        assert!(recorded_totals(&registry).unwrap().unwrap().complete);
    }

    /// Admission still refuses a family whose last complete record is over
    /// the hard limit after a pass that stopped at its deadline, and says
    /// that the last measurement did not finish.
    #[test]
    fn admission_refuses_over_the_last_complete_record_after_an_incomplete_pass() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 64);
        }
        let usage = measure(&registry, None).unwrap();
        // No soft limit, so no pass evicts anything; the hard limit leaves
        // no room for a new view the size of the largest one.
        registry.set_disk_budget(budget(u64::MAX, usage.view_store_bytes));
        enforce(
            &registry,
            EnforceOptions {
                deadline: Some(Instant::now()),
                observer: None,
            },
        )
        .unwrap();
        let new_root = storage.path().join("c");
        fs::create_dir_all(&new_root).unwrap();

        let refused = registry
            .register_view_with_storage(registry.storage(), "c", &new_root)
            .unwrap_err();

        wait_for_background_enforcement(&registry);
        let RegistryError::AdmissionQueued(reason) = &refused else {
            panic!("unexpected error: {refused}");
        };
        assert!(reason.starts_with("queued: view store disk"), "{reason}");
        assert!(reason.contains("did not finish"), "{reason}");
        assert!(!registered(&registry, "c"));
    }

    /// `SUM(length(payload))` over every plane of the family, the logical
    /// total the occupancy report prints next to the file bytes.
    fn payload_total(registry: &FamilyRegistry) -> u64 {
        family_payload_bytes(registry).unwrap()
    }

    /// The usage rows the last recorded measurement left in the registry,
    /// without their timestamps.
    fn recorded_rows(registry: &FamilyRegistry) -> Vec<String> {
        registry
            .read(|connection| {
                let mut rows = Vec::new();
                let mut statement = connection
                    .prepare("SELECT scope, bytes, held FROM view_usage ORDER BY scope")?;
                let views = statement.query_map([], |row| {
                    Ok(format!(
                        "recorded view_usage scope={} bytes={} held={}",
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?
                    ))
                })?;
                for view in views {
                    rows.push(view?);
                }
                rows.push(format!(
                    "recorded family_usage file_bytes={}",
                    connection.query_row(
                        "SELECT file_bytes FROM family_usage WHERE singleton = 1",
                        [],
                        |row| row.get::<_, i64>(0)
                    )?
                ));
                rows.push(format!(
                    "recorded usage_record complete={}",
                    connection.query_row(
                        "SELECT complete FROM usage_record WHERE singleton = 1",
                        [],
                        |row| row.get::<_, bool>(0)
                    )?
                ));
                Ok(rows)
            })
            .unwrap()
    }

    /// The admission outcome with the refusal's "eviction pass" clause
    /// removed. That clause depends on whether another test's background
    /// eviction pass happens to be running, so goldens cannot include it.
    fn admission(registry: &FamilyRegistry) -> String {
        match admit_new_view(registry) {
            Ok(()) => "admitted".to_string(),
            Err(RegistryError::AdmissionQueued(reason)) => format!(
                "refused: {}",
                reason
                    .split("; ")
                    .filter(|part| !part.contains("eviction pass"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Err(error) => format!("error: {error}"),
        }
    }

    fn enforcement_values(label: &str, report: &EnforcementReport) -> String {
        let sweep = report.sweep.as_ref();
        format!(
            "{label} evicted={:?} sweep_byte_budget={:?} deleted_blobs={:?} deleted_bytes={:?} \
             deleted_segments={:?} sweep_error={:?} before_file_bytes={} after_file_bytes={} \
             after_complete={} over_hard={:?} stopped_early={}",
            report.evicted,
            LAST_SWEEP_BUDGET.with(|last| last.replace(None)),
            sweep.map(|sweep| sweep.deleted_blobs),
            sweep.map(|sweep| sweep.deleted_bytes),
            sweep.map(|sweep| sweep.deleted_segments),
            report.sweep_error,
            report.before.family_store.file_bytes,
            report.after.family_store.file_bytes,
            report.after.complete,
            report.over_hard,
            report.stopped_early
        )
    }

    /// Builds one fixed family (three idle views of their own content plus
    /// three rows no view references), then measures, accounts, records,
    /// admits and enforces it below and above its limits, and lists every
    /// value those steps produce.
    fn golden_fixture_values() -> Vec<String> {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b", "c"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 16);
        }
        set_last_bind(&registry, "a", 3_000);
        set_last_bind(&registry, "b", 1_000);
        set_last_bind(&registry, "c", 2_000);
        {
            let access = crate::blob_store::v2::StoreWriteAccess::for_registered_view(
                storage.path(),
                FAMILY,
            );
            let store =
                crate::blob_store::v2::FamilyStore::open(&access, FamilyPlane::Trigram).unwrap();
            for seed in 10..13 {
                let bytes = text(seed, 4_000);
                store
                    .put_or_touch(
                        &trigram_key(&bytes),
                        &TrigramPayload::extract(&bytes, &policy()).encode(),
                    )
                    .unwrap();
            }
        }
        wait_for_background_enforcement(&registry);
        LAST_SWEEP_BUDGET.with(|last| last.set(None));

        let mut values = Vec::new();
        let usage = measure(&registry, None).unwrap();
        values.push(format!(
            "measure family_store file_bytes={} complete={} view_store_bytes={} \
             held_view_bytes={} complete={}",
            usage.family_store.file_bytes,
            usage.family_store.complete,
            usage.view_store_bytes,
            usage.held_view_bytes,
            usage.complete
        ));
        values.push(format!("payload_total={}", payload_total(&registry)));
        values.push(format!("{:?}", account_keys(&registry, None).unwrap()));
        values.extend(recorded_rows(&registry));
        values.push(format!("admission default {}", admission(&registry)));

        let below = enforce(&registry, EnforceOptions::default()).unwrap();
        values.push(enforcement_values("below-limit", &below));
        values.extend(recorded_rows(&registry));

        registry.set_disk_budget(budget(view_bytes(&below.after, "a"), u64::MAX));
        let collect = enforce(&registry, EnforceOptions::default()).unwrap();
        values.push(enforcement_values("collect-all", &collect));
        values.extend(recorded_rows(&registry));
        values.push(format!("{:?}", account_keys(&registry, None).unwrap()));

        registry.set_disk_budget(budget(u64::MAX, 1));
        values.push(format!("admission over-hard {}", admission(&registry)));
        wait_for_background_enforcement(&registry);
        values
    }

    /// Values one fixed family produces through measurement, key accounting,
    /// recorded usage, admission and two enforcement passes, recorded on
    /// Linux on main before the disk-limit sweep measured the family's files
    /// to skip deletion. The collect-all pass's file bytes later grew by one
    /// 4 KiB page, the payload totals table that pass sets up in the trigram
    /// store. Every family file size then grew by one more 4 KiB page, the
    /// deletion counter table a writer sets up when it first opens a family
    /// store. Skipping the below-limit epoch update removes one 4,120-byte
    /// WAL frame from that pass and the next pass's starting file bytes. File
    /// sizes depend on the platform's SQLite page and write-ahead-log
    /// behaviour, so other platforms only run the fixture.
    const GOLDEN_VALUES: &[&str] = &[
        "measure family_store file_bytes=487085 complete=true view_store_bytes=124704 \
         held_view_bytes=0 complete=true",
        "payload_total=98874",
        "KeyAccounting { referenced_bytes: 159686, shared_bytes: 0, exclusive_bytes: \
         {\"a\": 53014, \"b\": 53322, \"c\": 53350}, unreferenced_bytes: 64737, complete: true }",
        "recorded view_usage scope=a bytes=41568 held=false",
        "recorded view_usage scope=b bytes=41568 held=false",
        "recorded view_usage scope=c bytes=41568 held=false",
        "recorded family_usage file_bytes=487085",
        "recorded usage_record complete=true",
        "admission default admitted",
        "below-limit evicted=[] sweep_byte_budget=Some(2147483648) deleted_blobs=Some(0) \
         deleted_bytes=Some(0) deleted_segments=Some(0) sweep_error=None \
         before_file_bytes=503565 after_file_bytes=528285 after_complete=true over_hard=[] \
         stopped_early=false",
        "recorded view_usage scope=a bytes=41568 held=false",
        "recorded view_usage scope=b bytes=41568 held=false",
        "recorded view_usage scope=c bytes=41568 held=false",
        "recorded family_usage file_bytes=528285",
        "recorded usage_record complete=true",
        "collect-all evicted=[(\"b\", 41568), (\"c\", 41568)] sweep_byte_budget=Some(0) \
         deleted_blobs=Some(5) deleted_bytes=Some(171409) deleted_segments=Some(2) \
         sweep_error=None before_file_bytes=544765 after_file_bytes=522673 \
         after_complete=true over_hard=[] stopped_early=false",
        "recorded view_usage scope=a bytes=41568 held=false",
        "recorded family_usage file_bytes=522673",
        "recorded usage_record complete=true",
        "KeyAccounting { referenced_bytes: 53014, shared_bytes: 0, exclusive_bytes: \
         {\"a\": 53014}, unreferenced_bytes: 0, complete: true }",
        "admission over-hard refused: queued: view store disk: 0 of 41568 bytes are held by \
         active sessions or protected generations, hard limit 1 bytes, a new view reserves \
         41568 bytes",
    ];

    /// The collect-all pass's file bytes after its sweep take one of two
    /// values, 4120 bytes (one 4 KiB write-ahead-log frame) apart, from run
    /// to run on unchanged main as well (2 of 30 runs there): the pass does
    /// not always write the same number of pages. These are the golden lines
    /// with the other value; a run must match one set or the other entirely.
    const GOLDEN_ALTERNATE_LINES: &[(usize, &str)] = &[
        (
            15,
            "collect-all evicted=[(\"b\", 41568), (\"c\", 41568)] sweep_byte_budget=Some(0) \
             deleted_blobs=Some(5) deleted_bytes=Some(171409) deleted_segments=Some(2) \
             sweep_error=None before_file_bytes=544765 after_file_bytes=518553 \
             after_complete=true over_hard=[] stopped_early=false",
        ),
        (17, "recorded family_usage file_bytes=518553"),
    ];

    #[test]
    fn measurement_sweep_and_admission_values_match_the_recorded_golden() {
        let actual = golden_fixture_values();
        eprintln!("golden values: {actual:#?}");
        if cfg!(target_os = "linux") {
            let mut alternate = GOLDEN_VALUES.to_vec();
            for (index, line) in GOLDEN_ALTERNATE_LINES {
                alternate[*index] = line;
            }
            if actual != alternate {
                assert_eq!(actual, GOLDEN_VALUES);
            }
        }
    }

    /// The hard limit is compared with live bytes as well: a family whose
    /// files are over it only because of free pages admits a new view and
    /// reports nothing over the limit, from the measurement and from the
    /// totals recorded for admission alike. A hard limit below the live
    /// bytes still refuses, and the refusal names the free pages it left out.
    #[test]
    fn free_pages_do_not_count_toward_the_hard_limit() {
        let storage = tempfile::tempdir().unwrap();
        let registry = two_view_family(storage.path());
        let access =
            crate::blob_store::v2::StoreWriteAccess::for_registered_view(storage.path(), FAMILY);
        let store =
            crate::blob_store::v2::FamilyStore::open(&access, FamilyPlane::Trigram).unwrap();
        // 4 MiB of free pages, made on the store's own connection.
        crate::blob_store::v2::read_tests::set_pragma(
            &store,
            "CREATE TABLE free_fixture (data BLOB);
             INSERT INTO free_fixture VALUES (zeroblob(4194304));
             DROP TABLE free_fixture;",
        );
        let free = store.reader().unused_file_bytes().unwrap();
        assert!(free >= 4 * 1024 * 1024, "{free}");
        let (file, complete) = family_file_bytes(storage.path(), FAMILY, None).unwrap();
        assert!(complete);
        let hard = file - free / 2;
        registry.set_disk_budget(DiskBudget {
            family_store_soft: u64::MAX,
            family_store_hard: hard,
            ..DiskBudget::default()
        });

        let usage = measure(&registry, None).unwrap();
        assert!(usage.family_store.file_bytes > hard, "{usage:?}");
        assert_eq!(usage.family_store.free_bytes, free);
        assert!(usage.family_store.live_bytes() < hard);
        let totals = recorded_totals(&registry).unwrap().unwrap();
        assert_eq!(totals.family_store_free_bytes, free);
        assert!(over_hard(totals, registry.disk_budget(), 0).is_empty());
        assert_eq!(admission(&registry), "admitted");

        let live = totals.family_store_live_bytes();
        registry.set_disk_budget(DiskBudget {
            family_store_soft: u64::MAX,
            family_store_hard: live - 1,
            ..DiskBudget::default()
        });
        let refused = admission(&registry);
        wait_for_background_enforcement(&registry);
        assert!(
            refused.starts_with("refused: queued: family store disk")
                && refused.contains(&format!("not counting {free} bytes of free pages")),
            "{refused}"
        );
    }

    /// Builds a family of two idle views and returns its registry, so tests
    /// can build identical copies of it in separate storage roots.
    fn two_view_family(storage: &Path) -> FamilyRegistry {
        let registry = FamilyRegistry::open(storage, FAMILY).unwrap();
        for (seed, scope) in ["a", "b"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 16);
            set_last_bind(&registry, scope, seed as u64 + 1);
        }
        wait_for_background_enforcement(&registry);
        registry
    }

    /// Measurement reads file sizes only, so a plane store that is not a
    /// SQLite file at all is measured like any other file. The family sweep,
    /// which has to open the store, reports why it could not run, and the
    /// enforcement pass still finishes. Before measurement stopped opening
    /// plane stores, the same family failed to measure at all.
    #[test]
    fn an_unopenable_plane_store_is_measured_by_size_and_reported_by_the_sweep() {
        let storage = tempfile::tempdir().unwrap();
        let broken = two_view_family(storage.path());
        let plane = crate::blob_store::v2::plane_path(storage.path(), FAMILY, FamilyPlane::Trigram)
            .unwrap();
        let (expected, complete) = family_file_bytes(storage.path(), FAMILY, None).unwrap();
        assert!(complete);
        // Same length, so the family's file bytes must not change.
        let length = fs::metadata(&plane).unwrap().len() as usize;
        fs::write(&plane, vec![b'x'; length]).unwrap();

        let measured = measure(&broken, None).expect("measurement opens no plane store");
        assert!(measured.family_store.complete);
        assert_eq!(measured.family_store.file_bytes, expected);

        let report = enforce(&broken, EnforceOptions::default()).unwrap();
        assert!(report.evicted.is_empty());
        assert!(report.sweep.is_none());
        let error = report
            .sweep_error
            .expect("the sweep could not open the store");
        assert!(error.contains("not a database"), "{error}");
        assert!(report.after.family_store.complete);
    }

    /// Runs one enforcement pass and counts, on this thread, the planes whose
    /// own checks its family sweep ran and the logical payload sums it ran.
    fn enforce_counting_aggregates(registry: &FamilyRegistry) -> (EnforcementReport, usize, usize) {
        let before = crate::gc::family::delete_aggregates_on_this_thread();
        crate::blob_store::v2::take_logical_aggregates();
        let report = enforce(registry, EnforceOptions::default()).unwrap();
        let after = crate::gc::family::delete_aggregates_on_this_thread();
        (
            report,
            after - before,
            crate::blob_store::v2::take_logical_aggregates(),
        )
    }

    fn plane_stores(storage: &Path) -> usize {
        FamilyPlane::ALL
            .into_iter()
            .filter(|plane| {
                crate::blob_store::v2::plane_path(storage, FAMILY, *plane)
                    .unwrap()
                    .is_file()
            })
            .count()
    }

    /// A pass below every limit skips before marking or per-plane checks:
    /// the family's files fit in the payload budget. An all-live family over
    /// that budget is settled by preflight, so only the collect-all pass after
    /// eviction reaches the deletion aggregates. Exact totals are initialized
    /// once by preflight and reused by deletion.
    #[test]
    fn a_below_limit_sweep_reads_no_logical_totals_and_every_other_pass_reads_them() {
        let storage = tempfile::tempdir().unwrap();
        let registry = two_view_family(storage.path());
        let planes = plane_stores(storage.path());
        assert_eq!(planes, 1);
        LAST_SWEEP_BUDGET.with(|last| last.set(None));

        let (report, checked, sums) = enforce_counting_aggregates(&registry);
        assert!(report.evicted.is_empty() && report.before.complete);
        assert_eq!(
            LAST_SWEEP_BUDGET.with(|last| last.replace(None)),
            Some(FAMILY_PAYLOAD_BUDGET_BYTES)
        );
        assert_eq!(report.sweep.as_ref().unwrap().deleted_blobs, 0);
        assert_eq!(report.sweep.as_ref().unwrap().marked_keys, 0);
        assert_eq!(
            report.sweep.as_ref().unwrap().skipped,
            Some("nothing to collect")
        );
        assert_eq!((checked, sums), (0, 0), "below-limit pass");

        // Over a payload budget of one byte, far under the soft limit.
        registry.set_disk_budget(DiskBudget {
            family_payload: 1,
            ..DiskBudget::default()
        });
        let (report, checked, sums) = enforce_counting_aggregates(&registry);
        assert!(report.evicted.is_empty());
        assert!(report.before.family_store.file_bytes < FAMILY_STORE_SOFT_BYTES);
        assert_eq!(LAST_SWEEP_BUDGET.with(|last| last.replace(None)), Some(1));
        assert_eq!(
            (checked, sums),
            (0, planes),
            "preflight settles an all-live family before deletion"
        );

        // A directory deeper than the walk goes leaves it incomplete.
        registry.set_disk_budget(DiskBudget::default());
        let deep = family_dir(storage.path(), FAMILY)
            .unwrap()
            .join("deeper/than/the-walk");
        fs::create_dir_all(&deep).unwrap();
        let (report, checked, sums) = enforce_counting_aggregates(&registry);
        assert!(report.evicted.is_empty() && !report.before.family_store.complete);
        assert_eq!(
            LAST_SWEEP_BUDGET.with(|last| last.replace(None)),
            Some(FAMILY_PAYLOAD_BUDGET_BYTES)
        );
        assert_eq!(
            (checked, sums),
            (0, 0),
            "preflight settles the incomplete walk by page capacity"
        );
        fs::remove_dir_all(family_dir(storage.path(), FAMILY).unwrap().join("deeper")).unwrap();

        registry.set_disk_budget(budget(0, u64::MAX));
        let (report, checked, sums) = enforce_counting_aggregates(&registry);
        assert_eq!(evicted(&report), ["a", "b"]);
        assert_eq!(LAST_SWEEP_BUDGET.with(|last| last.replace(None)), Some(0));
        assert!(report.sweep.as_ref().unwrap().deleted_blobs > 0);
        assert_eq!((checked, sums), (planes, 0), "collect-all pass");
    }

    /// A family whose files fit the sweep budget skips every plane's own
    /// checks, yet a large free list left in one of its plane stores is still
    /// returned to the file system, in bounded steps, with the remaining work
    /// reported.
    #[test]
    fn a_family_within_budget_still_reclaims_a_large_free_list() {
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        let access =
            crate::blob_store::v2::StoreWriteAccess::for_registered_view(storage.path(), FAMILY);
        drop(crate::blob_store::v2::FamilyStore::open(&access, FamilyPlane::Semantic).unwrap());
        let path = crate::blob_store::v2::plane_path(storage.path(), FAMILY, FamilyPlane::Semantic)
            .unwrap();
        {
            // The store's only connection closed above, so this one does not
            // share a live file set. 320 MiB of free pages clears the 256 MiB
            // threshold above which free pages are reclaimed.
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE reclaim_fixture (data BLOB);
                     INSERT INTO reclaim_fixture VALUES (zeroblob(335544320));
                     DELETE FROM reclaim_fixture;",
                )
                .unwrap();
        }
        let (before, complete) = family_file_bytes(storage.path(), FAMILY, None).unwrap();
        let byte_budget = 2 * before;
        assert!(complete && before > 320 * 1024 * 1024, "{before}");

        let checked = crate::gc::family::delete_aggregates_on_this_thread();
        let report = family::sweep_family_remeasured(
            &registry,
            None,
            FamilySweepPolicy { byte_budget },
            SweepBounds::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            crate::gc::family::delete_aggregates_on_this_thread(),
            checked,
            "the walk settled the family"
        );
        let (after, _) = family_file_bytes(storage.path(), FAMILY, None).unwrap();
        assert_eq!(report.deleted_blobs, 0);
        assert_eq!(report.space_reclaim.examined_stores, 1);
        assert!(report.space_reclaim.reclaimed_pages > 0, "{report:?}");
        assert!(report.reclaim_deferred, "eligible free pages remain");
        assert!(after < before, "{before} -> {after}");
    }

    /// Builds the two-view family, records its file bytes, then grows it with
    /// rows no view references, put before any sweep raises its epoch.
    fn grown_family(storage: &Path) -> (FamilyRegistry, u64) {
        let registry = two_view_family(storage);
        let (before_growth, complete) = family_file_bytes(storage, FAMILY, None).unwrap();
        assert!(complete);
        let access = crate::blob_store::v2::StoreWriteAccess::for_registered_view(storage, FAMILY);
        let store =
            crate::blob_store::v2::FamilyStore::open(&access, FamilyPlane::Trigram).unwrap();
        for seed in 100..140 {
            let bytes = text(seed, 8_000);
            store
                .put_or_touch(
                    &trigram_key(&bytes),
                    &TrigramPayload::extract(&bytes, &policy()).encode(),
                )
                .unwrap();
        }
        (registry, before_growth)
    }

    /// A family that grew past the sweep's budget after it was last measured
    /// is swept exactly like the sweep that always reads logical totals,
    /// because the skip decision measures the files again after marking. A
    /// sweep that trusted the measurement taken before the growth would skip
    /// and delete nothing.
    #[test]
    fn a_family_grown_past_the_budget_since_its_last_measurement_is_swept_in_full() {
        let remeasured_storage = tempfile::tempdir().unwrap();
        let aggregate_storage = tempfile::tempdir().unwrap();
        let stale_storage = tempfile::tempdir().unwrap();
        let (remeasured, byte_budget) = grown_family(remeasured_storage.path());
        let (aggregate, _) = grown_family(aggregate_storage.path());
        let (stale, stale_measurement) = grown_family(stale_storage.path());
        let policy = FamilySweepPolicy { byte_budget };
        let (grown, complete) = family_file_bytes(remeasured_storage.path(), FAMILY, None).unwrap();
        assert!(complete && grown > byte_budget, "{grown} <= {byte_budget}");

        let before = crate::gc::family::delete_aggregates_on_this_thread();
        let swept = family::sweep_family_remeasured(
            &remeasured,
            None,
            policy,
            SweepBounds::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            crate::gc::family::delete_aggregates_on_this_thread() - before,
            plane_stores(remeasured_storage.path())
        );
        let reference =
            family::sweep_family_bounded(&aggregate, None, policy, SweepBounds::default(), None)
                .unwrap();
        assert!(reference.deleted_blobs > 0, "{reference:?}");
        assert_eq!(
            (swept.deleted_blobs, swept.deleted_bytes),
            (reference.deleted_blobs, reference.deleted_bytes)
        );

        let trusted = family::sweep_with(
            &stale,
            None,
            FamilySweepPolicy {
                byte_budget: stale_measurement,
            },
            SweepBounds::default(),
            None,
            family::SmallFamilyCheck::Assumed(stale_measurement),
        )
        .unwrap();
        assert_eq!((trusted.deleted_blobs, trusted.deleted_bytes), (0, 0));
    }

    /// Receipt for the views performance gate G2a subset: the process-wide
    /// `metrics.views_work.logical_aggregates` counter across below-limit
    /// enforcement passes, with their wall time (reported, not gated). Other
    /// tests running at the same time would add to that counter, so this one
    /// is ignored and run alone:
    /// `cargo test -p agent-file-tools --lib -- --ignored --exact
    /// views::eviction::tests::g2a_below_limit_pass_logical_aggregates_receipt`.
    #[test]
    #[ignore = "reads a process-wide counter; run alone with --ignored --exact"]
    fn g2a_below_limit_pass_logical_aggregates_receipt() {
        use std::sync::atomic::Ordering;
        let storage = tempfile::tempdir().unwrap();
        let registry = FamilyRegistry::open(storage.path(), FAMILY).unwrap();
        for (seed, scope) in ["a", "b", "c"].into_iter().enumerate() {
            idle_view(&registry, scope, seed as u64, 16);
        }
        wait_for_background_enforcement(&registry);
        let planes = FamilyPlane::ALL
            .into_iter()
            .filter(|plane| {
                crate::blob_store::v2::plane_path(storage.path(), FAMILY, *plane)
                    .unwrap()
                    .is_file()
            })
            .count();
        let mut passes = Vec::new();
        for _ in 0..5 {
            let before = crate::views::io::WORK
                .logical_aggregates
                .load(Ordering::SeqCst);
            let started = Instant::now();
            let report = enforce(&registry, EnforceOptions::default()).unwrap();
            let wall_us = started.elapsed().as_micros() as u64;
            let aggregates = crate::views::io::WORK
                .logical_aggregates
                .load(Ordering::SeqCst)
                - before;
            assert!(report.evicted.is_empty() && report.sweep.is_some());
            assert_eq!(
                LAST_SWEEP_BUDGET.with(|last| last.replace(None)),
                Some(FAMILY_PAYLOAD_BUDGET_BYTES),
                "a below-limit pass keeps unreferenced rows under the payload budget"
            );
            passes.push(serde_json::json!({
                "logical_aggregates": aggregates,
                "enforce_wall_us": wall_us,
                "family_file_bytes": report.before.family_store.file_bytes,
            }));
        }
        eprintln!(
            "G2A_RECEIPT {}",
            serde_json::json!({
                "counter": "metrics.views_work.logical_aggregates",
                "plane_stores": planes,
                "passes": passes,
            })
        );
        // Measurement opens no plane store, and the family sweep's walk of
        // the family's files rules out deletion for a family this small.
        for pass in &passes {
            assert_eq!(
                pass["logical_aggregates"],
                serde_json::json!(0),
                "a below-limit pass ran a logical payload aggregate"
            );
        }
    }
}
