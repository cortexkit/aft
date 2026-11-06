//! Reading one generation of a family.

use std::path::PathBuf;
use std::sync::Arc;

use super::lease::LeaseFile;
use super::manifest::Manifest;
use super::record::{self, Kind};
use super::{Family, FamilyError, Layout};
use crate::cache::BlockCache;
use crate::checkout::CheckoutState;
use crate::ids::{CheckoutId, Digest128, FamilyId, Generation, SegmentId};
use crate::segment::{KnownSection, Segment};

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
