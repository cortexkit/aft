//! Owners and leases: how a reclaimer knows which generations live readers
//! still need.
//!
//! Each process that reads a family has one owner file,
//! `leases/<owner>.owner`, locked by that process for as long as it lives.
//! The file is created and locked under `incoming/`, then renamed into
//! `leases/`, so a reclaimer never sees an owner file that is not yet
//! locked: finding one unlocked proves its process has ended.
//!
//! Each snapshot in use adds a lease record, `leases/<owner>.<n>.lease`,
//! naming the generation it reads. Dropping the snapshot removes the record.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use super::record::{self, Kind, Payload, RecordError};
use super::{FamilyError, Layout};
use crate::ids::{Digest128, FamilyId, Generation};

const OWNER_SUFFIX: &str = ".owner";
const LEASE_SUFFIX: &str = ".lease";

/// An `incoming/` owner file this old whose lock is free was left by a
/// process that died between creating and locking it. Younger ones may
/// belong to a process about to lock them.
const INCOMING_GRACE: Duration = Duration::from_secs(60);

#[cfg(test)]
thread_local! {
    /// A test runs this between the scan's lease and owner listings, to
    /// place a new reader exactly there.
    pub(super) static BETWEEN_LISTINGS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// This process's presence in one family.
#[derive(Debug)]
pub(super) struct Owner {
    id: Digest128,
    /// Holds the lock that marks this process alive.
    _file: File,
    leases_dir: PathBuf,
    next: AtomicU64,
    /// Lease records whose removal failed. A leftover record of a live
    /// process would pin its generation, so each new lease retries them.
    stale: Mutex<Vec<PathBuf>>,
}

impl Owner {
    pub(super) fn create(layout: &Layout) -> Result<Self, FamilyError> {
        let mut last_error = None;
        // A reclaimer may remove an incoming file it judges abandoned; the
        // creator then starts again with a new name.
        for _ in 0..3 {
            let id = fresh_id();
            let incoming = layout.incoming_dir().join(format!("{id}{OWNER_SUFFIX}"));
            let target = layout.leases_dir().join(format!("{id}{OWNER_SUFFIX}"));
            crate::fault::point("owner.create").map_err(FamilyError::io(&incoming))?;
            let file =
                crate::fs::create_new_private(&incoming).map_err(FamilyError::io(&incoming))?;
            match file.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(FamilyError::Misuse("a new owner file was already locked"));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(FamilyError::io(&incoming)(error));
                }
            }
            match crate::fs::rename_new_durable(&incoming, &target) {
                Ok(()) => {
                    return Ok(Self {
                        id,
                        _file: file,
                        leases_dir: layout.leases_dir(),
                        next: AtomicU64::new(1),
                        stale: Mutex::new(Vec::new()),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    last_error = Some(FamilyError::io(&incoming)(error));
                }
                Err(error) => return Err(FamilyError::io(&target)(error)),
            }
        }
        Err(last_error.unwrap_or(FamilyError::Misuse("owner file creation kept failing")))
    }

    /// Records a lease on `generation`, durably, before returning.
    pub(super) fn lease(
        self: &std::sync::Arc<Self>,
        family: FamilyId,
        generation: Generation,
    ) -> Result<LeaseFile, FamilyError> {
        self.retry_stale();
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let path = self
            .leases_dir
            .join(format!("{}.{number}{LEASE_SUFFIX}", self.id));
        if let Err(error) = record::write(
            &path,
            Kind::Lease,
            family,
            number,
            &generation.get().to_le_bytes(),
        ) {
            self.remove_or_remember(path.clone());
            return Err(FamilyError::io(&path)(error));
        }
        Ok(LeaseFile {
            path: Some(path),
            owner: self.clone(),
        })
    }

    fn retry_stale(&self) {
        let stale = std::mem::take(
            &mut *self
                .stale
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for path in stale {
            self.remove_or_remember(path);
        }
    }

    fn remove_or_remember(&self, path: PathBuf) {
        if crate::fs::remove_file(&path).is_err() {
            self.stale
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path);
        }
    }
}

/// A lease record held by a snapshot. Dropping it releases the lease.
#[derive(Debug)]
pub(super) struct LeaseFile {
    path: Option<PathBuf>,
    owner: std::sync::Arc<Owner>,
}

impl Drop for LeaseFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            self.owner.remove_or_remember(path);
        }
    }
}

/// An owner file whose lock was free: its process has ended. Holding the
/// lock keeps a new process from reusing the name while it is removed.
#[derive(Debug)]
pub(super) struct DeadOwner {
    pub(super) path: PathBuf,
    pub(super) _lock: File,
}

/// What a reclaimer learns from the `leases/` directory.
#[derive(Debug, Default)]
pub(super) struct LeaseScan {
    /// The generation of every lease held by a live process.
    pub(super) live: Vec<Generation>,
    /// Leases of live processes whose records could not be read yet: one is
    /// being written, or its write failed. The reclaimer cannot know what
    /// they protect, so it removes nothing this time.
    pub(super) unreadable: usize,
    pub(super) dead_owners: Vec<DeadOwner>,
    /// Lease records whose owner has ended or is gone.
    pub(super) dead_leases: Vec<PathBuf>,
    /// Abandoned `incoming/` owner files.
    pub(super) abandoned_incoming: Vec<PathBuf>,
}

impl LeaseScan {
    pub(super) fn lowest_live(&self) -> Option<Generation> {
        self.live.iter().copied().min()
    }
}

pub(super) fn scan(layout: &Layout, family: FamilyId) -> Result<LeaseScan, FamilyError> {
    let leases_dir = layout.leases_dir();
    let mut scan = LeaseScan::default();
    // Leases are listed before owners. A process renames its owner file into
    // `leases/` before it writes any lease, so the owner of every lease in
    // this listing either appears in the owner listing below or has been
    // removed, which only happens once its process has ended. Listing owners
    // first would miss an owner that appeared in between, and count its new
    // lease as dead.
    let leases = super::names_with_suffix(&leases_dir, LEASE_SUFFIX)?;
    #[cfg(test)]
    BETWEEN_LISTINGS.with(|slot| {
        if let Some(callback) = slot.borrow_mut().take() {
            callback();
        }
    });
    let mut alive = BTreeMap::<String, bool>::new();
    for (stem, path) in super::names_with_suffix(&leases_dir, OWNER_SUFFIX)? {
        match free_lock(&path) {
            Some(lock) => {
                alive.insert(stem, false);
                scan.dead_owners.push(DeadOwner { path, _lock: lock });
            }
            None => {
                alive.insert(stem, true);
            }
        }
    }
    for (stem, path) in leases {
        let Some((owner, number)) = stem
            .rsplit_once('.')
            .and_then(|(owner, number)| Some((owner, super::canonical_number(number)?)))
        else {
            continue;
        };
        // Absent from the owner listing: the owner file was removed after
        // this lease was written, so its process has ended.
        if !alive.get(owner).copied().unwrap_or(false) {
            scan.dead_leases.push(path);
            continue;
        }
        match read_lease(&path, family, number) {
            Ok(generation) => scan.live.push(generation),
            // Released between the listing and the read.
            Err(RecordError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => scan.unreadable += 1,
        }
    }
    let incoming = layout.incoming_dir();
    for (_, path) in super::names_with_suffix(&incoming, OWNER_SUFFIX)? {
        let old_enough = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= INCOMING_GRACE);
        if old_enough && free_lock(&path).is_some() {
            scan.abandoned_incoming.push(path);
        }
    }
    Ok(scan)
}

/// Takes the lock on an owner file if no process holds it. `None` means the
/// owner is alive, or that the answer is unknown, which counts the same: a
/// file that cannot be opened (removed since the listing, or on Windows a
/// file being removed) proves nothing about its process.
fn free_lock(path: &Path) -> Option<File> {
    let file = crate::fs::open_read(path).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(std::fs::TryLockError::WouldBlock | std::fs::TryLockError::Error(_)) => None,
    }
}

fn read_lease(path: &Path, family: FamilyId, number: u64) -> Result<Generation, RecordError> {
    let payload = record::read(path, Kind::Lease, family, number)?;
    let mut input = Payload::new(&payload);
    let generation = Generation::new(input.u64()?);
    input.finish()?;
    Ok(generation)
}

/// A new name for an owner or a writer run. Unique without coordination:
/// the process id, the time, a process-wide counter and an address that
/// differs between runs all go into one digest.
pub(super) fn fresh_id() -> Digest128 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.update(&now.as_nanos().to_le_bytes());
    hasher.update(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let local = 0u8;
    hasher.update(&(std::ptr::from_ref(&local) as usize).to_le_bytes());
    Digest128::from_hash(&hasher.finalize())
}

/// A lease's path. Its owner is shared by every lease of the process and
/// counted with none of them.
impl crate::heap::HeapBytes for LeaseFile {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        let Self { path, owner: _ } = self;
        crate::heap::HeapBytes::heap_bytes(path, seen)
    }
}
