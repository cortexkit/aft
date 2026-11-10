//! Reading one generation of a family.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::lease::LeaseFile;
use super::manifest::Manifest;
use super::record::{self, Kind};
use super::{Family, FamilyError, Layout};
use crate::cache::BlockCache;
use crate::checkout::CheckoutState;
use crate::ids::{CheckoutId, Digest128, FamilyId, Generation, SegmentId};
use crate::segment::{KnownSection, Segment};

/// One open segment per file, sections list and cache, for
/// [`Snapshot::open_shared_segment`].
#[derive(Clone, PartialEq, Eq, Hash)]
struct SharedKey {
    path: PathBuf,
    /// The cache's address: a segment caches its blocks in the cache it was
    /// opened with, so only callers sharing that cache share the segment.
    cache: usize,
    known: Vec<KnownSection>,
}

fn shared_segments() -> &'static Mutex<HashMap<SharedKey, Weak<Segment>>> {
    static SHARED: OnceLock<Mutex<HashMap<SharedKey, Weak<Segment>>>> = OnceLock::new();
    SHARED.get_or_init(Mutex::default)
}

/// The registry's size after its last sweep: the next sweep runs once it
/// has doubled.
static SWEEP_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(64);

/// How many times a reader starts again when the generation it read is
/// reclaimed before its lease takes hold. Each retry reads a newer
/// generation, so needing more than a few means something is wrong.
const MAX_TRIES: u32 = 16;

/// A consistent view of one generation. Everything its manifest names stays
/// on disk until the snapshot is dropped.
#[derive(Debug)]
pub struct Snapshot {
    manifest: Manifest,
    layout: Layout,
    family: FamilyId,
    skipped_manifests: usize,
    /// `None` for the empty generation, which names no files to protect.
    _lease: Option<LeaseFile>,
}

pub(super) fn take(family: &Family) -> Result<Snapshot, FamilyError> {
    for _ in 0..MAX_TRIES {
        let (manifest, skipped_manifests) = family.newest_manifest()?;
        let generation = manifest.generation();
        if generation == Generation::EMPTY {
            return Ok(Snapshot {
                manifest,
                layout: family.layout.clone(),
                family: family.id,
                skipped_manifests,
                _lease: None,
            });
        }
        let lease = family.owner()?.lease(family.id, generation)?;
        let horizon = family
            .newest_horizon()?
            .map_or(Generation::EMPTY, |(_, horizon)| horizon);
        if horizon > generation {
            // A reclamation may already be removing this generation's files.
            continue;
        }
        return Ok(Snapshot {
            manifest,
            layout: family.layout.clone(),
            family: family.id,
            skipped_manifests,
            _lease: Some(lease),
        });
    }
    Err(FamilyError::SnapshotRetries(MAX_TRIES))
}

impl Snapshot {
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    #[must_use]
    pub fn generation(&self) -> Generation {
        self.manifest.generation()
    }

    /// Newer manifests that were skipped because their bytes did not check.
    /// Nonzero means a write was torn by a crash or a file was damaged; the
    /// caller reports it in health.
    #[must_use]
    pub const fn skipped_manifests(&self) -> usize {
        self.skipped_manifests
    }

    /// The path of a segment this generation names.
    #[must_use]
    pub fn segment_path(&self, id: SegmentId) -> Option<PathBuf> {
        self.manifest.segment(id).map(|_| self.layout.segment(id))
    }

    /// Opens a segment this generation names and checks it is the file the
    /// manifest published.
    ///
    /// # Errors
    ///
    /// When the manifest does not name the segment, or the file is missing,
    /// has another length, or fails the segment's own checks.
    pub fn open_segment(
        &self,
        id: SegmentId,
        known: &[KnownSection],
        cache: Arc<BlockCache>,
    ) -> Result<Segment, FamilyError> {
        let entry = self.manifest.segment(id).ok_or(FamilyError::Misuse(
            "the snapshot's manifest does not name this segment",
        ))?;
        let path = self.layout.segment(id);
        let len = std::fs::metadata(&path)
            .map_err(FamilyError::io(&path))?
            .len();
        if len != entry.len {
            return Err(FamilyError::Damaged {
                path,
                reason: format!(
                    "the file is {len} bytes; generation {} published it at {} bytes",
                    self.manifest.generation(),
                    entry.len
                ),
            });
        }
        let segment = Segment::open(&path, self.family, known, cache)?;
        // An intact segment of the same family and length under the wrong
        // name (a restore or copy mix-up) passes every checksum.
        if segment.id() != id {
            return Err(FamilyError::Damaged {
                path,
                reason: format!(
                    "the file holds segment {}; generation {} names segment {}",
                    segment.id().get(),
                    self.manifest.generation(),
                    id.get()
                ),
            });
        }
        Ok(segment)
    }

    /// [`Self::open_segment`], shared by every caller in this process that
    /// reads the same segment file with the same sections and cache: the
    /// segment is opened once and stays open while any of them holds it.
    ///
    /// Every checkout of a repository reads the same segment files. Opened
    /// once per checkout, a busy daemon held one descriptor per checkout per
    /// segment (10,966 descriptors with about 90 checkouts bound); past
    /// macOS's OPEN_MAX of 10,240, new pipes for shell commands got numbers
    /// `posix_spawn` refuses, and spawns failed with EBADF. Shared, the
    /// process holds at most one descriptor per segment file, and one copy
    /// of each cached block. Descriptors are capped as well
    /// ([`crate::segment::MAX_OPEN_SEGMENT_FILES`]): past the cap, a segment
    /// nobody read lately has its file closed, and opened again on its next
    /// read.
    ///
    /// A shared segment was checked when it was first opened; its file never
    /// changes. The manifest must still name it, and each caller's own
    /// snapshot keeps the file on disk, as for an unshared segment.
    ///
    /// # Errors
    ///
    /// As [`Self::open_segment`].
    pub fn open_shared_segment(
        &self,
        id: SegmentId,
        known: &[KnownSection],
        cache: &Arc<BlockCache>,
    ) -> Result<Arc<Segment>, FamilyError> {
        if self.manifest.segment(id).is_none() {
            return Err(FamilyError::Misuse(
                "the snapshot's manifest does not name this segment",
            ));
        }
        let key = SharedKey {
            path: self.layout.segment(id),
            cache: Arc::as_ptr(cache) as usize,
            known: known.to_vec(),
        };
        if let Some(segment) = shared_segments()
            .lock()
            .expect("shared segment registry poisoned")
            .get(&key)
            .and_then(Weak::upgrade)
        {
            return Ok(segment);
        }
        // Opened without the registry's lock held: opening reads the file.
        // Two threads opening the same segment at once both open it, and the
        // second keeps the first one's.
        let opened = Arc::new(self.open_segment(id, known, Arc::clone(cache))?);
        let mut registry = shared_segments()
            .lock()
            .expect("shared segment registry poisoned");
        if let Some(segment) = registry.get(&key).and_then(Weak::upgrade) {
            return Ok(segment);
        }
        // Entries whose segments every holder dropped are cleared here, so
        // the registry holds about as many entries as there are open files.
        if registry.len() >= 2 * SWEEP_AT.load(std::sync::atomic::Ordering::Relaxed) {
            registry.retain(|_, segment| segment.strong_count() > 0);
            SWEEP_AT.store(registry.len().max(64), std::sync::atomic::Ordering::Relaxed);
        }
        registry.insert(key, Arc::downgrade(&opened));
        Ok(opened)
    }

    /// The payload of a checkout's state at this generation, or `None` when
    /// the generation has no state for the checkout.
    ///
    /// # Errors
    ///
    /// When the state file is missing or is not the one the manifest named.
    pub fn read_state(&self, checkout: CheckoutId) -> Result<Option<Vec<u8>>, FamilyError> {
        let Some(state) = self.manifest.checkout(checkout) else {
            return Ok(None);
        };
        let path = self.layout.state(checkout, state.number);
        let payload = record::read(&path, Kind::State, self.family, state.number)
            .map_err(|error| FamilyError::record(&path, error))?;
        if payload.len() as u64 != state.len || Digest128::of(&payload) != state.digest {
            return Err(FamilyError::Damaged {
                path,
                reason: format!(
                    "the state's payload differs from the one generation {} published",
                    self.manifest.generation()
                ),
            });
        }
        Ok(Some(payload))
    }

    /// A checkout's state at this generation, decoded.
    ///
    /// # Errors
    ///
    /// As [`read_state`](Self::read_state), and when the payload is not a
    /// well-formed state. A payload holding a state layout a newer build
    /// wrote fails with [`FamilyError::NewerFormat`].
    pub fn checkout_state(
        &self,
        checkout: CheckoutId,
    ) -> Result<Option<CheckoutState>, FamilyError> {
        let Some(payload) = self.read_state(checkout)? else {
            return Ok(None);
        };
        CheckoutState::decode(&payload).map(Some).map_err(|error| {
            let path = self
                .manifest
                .checkout(checkout)
                .map_or_else(PathBuf::new, |state| {
                    self.layout.state(checkout, state.number)
                });
            FamilyError::record(&path, error)
        })
    }
}

crate::heap_bytes_of_fields!(Snapshot {
    manifest,
    layout,
    family,
    skipped_manifests,
    _lease,
});
