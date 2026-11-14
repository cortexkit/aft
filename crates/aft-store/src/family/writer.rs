//! Writing a family: building segments, publishing generations and
//! reclaiming files nothing can read any more.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::lease::{self, LeaseScan};
use super::manifest::{Manifest, SegmentEntry, StateRef};
use super::record::{self, Kind, RecordError};
use super::{Family, FamilyError, HORIZON_PREFIX, MANIFEST_PREFIX, STATE_PREFIX, numbered_desc};
use crate::checkout::CheckoutState;
use crate::ids::{CheckoutId, Digest128, Generation, SegmentId, WriterIncarnation};
use crate::segment::{SegmentMeta, SegmentWriter};

/// The one writer of a family, for as long as it holds `writer.lock`.
/// Dropping the session releases the lock.
#[derive(Debug)]
pub struct WriteSession<'f> {
    family: &'f Family,
    _lock: File,
    incarnation: WriterIncarnation,
    base: Manifest,
    next_generation: Generation,
    next_horizon: u64,
    next_segment: u64,
    next_state: HashMap<CheckoutId, u64>,
    /// Segments created in `tmp/` and not finished yet.
    open_segments: BTreeSet<SegmentId>,
    /// Finished segments in `tmp/`, with their lengths, waiting to publish.
    staged_segments: BTreeMap<SegmentId, u64>,
    /// State files written and not published yet.
    staged_states: BTreeMap<(CheckoutId, u64), StateRef>,
    poisoned: bool,
}

/// A segment being written in `tmp/`. Finish it with
/// [`WriteSession::finish_segment`].
#[derive(Debug)]
pub struct PendingSegment {
    id: SegmentId,
    writer: SegmentWriter,
}

/// Private build files that outlive a write session, but never a build.
/// The owner lock lets a later writer distinguish live work from a crash.
pub struct BuildScratch<E> {
    path: PathBuf,
    owner: Option<File>,
    admit: Box<dyn Fn(u64) -> Result<(), E> + Send + Sync>,
}

impl<E> std::fmt::Debug for BuildScratch<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildScratch")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<E> BuildScratch<E> {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Asks the caller's storage admission policy before writing a run.
    ///
    /// # Errors
    ///
    /// Returns the policy's refusal without writing anything.
    pub fn check_space(&self, additional_bytes: u64) -> Result<(), E> {
        (self.admit)(additional_bytes)
    }
}

impl<E> Drop for BuildScratch<E> {
    fn drop(&mut self) {
        // Close the lock before deleting its file, including on Windows.
        self.owner.take();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod build_scratch_tests {
    use super::*;
    use crate::ids::FamilyId;

    #[test]
    fn build_scratch_live_abandoned_and_torn_owners() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"scratch"))).unwrap();
        let mut scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(
                CheckoutId::from_digest(Digest128::of(b"checkout")),
                |bytes| {
                    if bytes <= 100 {
                        Ok(())
                    } else {
                        Err("disk floor")
                    }
                },
            )
            .unwrap();
        assert!(scratch.check_space(100).is_ok());
        assert_eq!(scratch.check_space(101), Err("disk floor"));
        let path = scratch.path().to_path_buf();
        assert_eq!(
            path.parent().unwrap().parent().unwrap(),
            family.dir().join("builds")
        );
        std::fs::write(path.join("partial.seg"), b"unfinished").unwrap();
        let mut session = family.try_write().unwrap().unwrap();
        assert!(
            session
                .create_scratch(
                    CheckoutId::from_digest(Digest128::of(b"checkout")),
                    |_| Ok::<_, ()>(())
                )
                .is_err()
        );
        let other = session
            .create_scratch(CheckoutId::from_digest(Digest128::of(b"other")), |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
        drop(session);
        assert!(
            other.path().exists(),
            "a different checkout can build concurrently"
        );
        assert!(
            path.join("partial.seg").exists(),
            "startup preserves a live build"
        );
        scratch.owner.take();
        drop(family.try_write().unwrap().unwrap());
        assert!(!path.exists(), "startup removes an abandoned build");
        let torn = path.parent().unwrap().join("torn");
        std::fs::create_dir(&torn).unwrap();
        drop(family.try_write().unwrap().unwrap());
        assert!(
            !torn.exists(),
            "startup removes a directory left before its owner was created"
        );
    }

    #[test]
    fn build_scratch_drop_removes_files_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"drop"))).unwrap();
        let mut path = PathBuf::new();
        let result = (|| -> Result<(), &str> {
            let scratch = family
                .try_write()
                .unwrap()
                .unwrap()
                .create_scratch(CheckoutId::from_digest(Digest128::of(b"checkout")), |_| {
                    Ok::<_, &str>(())
                })
                .unwrap();
            path = scratch.path().to_path_buf();
            std::fs::write(path.join("partial.seg"), b"unfinished").unwrap();
            Err("cancelled")
        })();
        assert!(result.is_err());
        assert!(!path.exists());
    }
}

fn clean_build_scratch(path: &Path) -> Result<(), FamilyError> {
    let owner = path.join("owner");
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&owner)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A crash can occur between directory creation and owner creation.
            return match std::fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(FamilyError::io(path)(error)),
            };
        }
        Err(error) => return Err(FamilyError::io(&owner)(error)),
    };
    match file.try_lock() {
        Ok(()) => {
            drop(file);
            match std::fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(FamilyError::io(path)(error)),
            }
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Err(std::fs::TryLockError::Error(error)) => Err(FamilyError::io(&owner)(error)),
    }
}

fn sweep_build_scratch(builds: &Path) -> Result<(), FamilyError> {
    if !builds.exists() {
        return Ok(());
    }
    for checkout in read_dir(builds)? {
        for workspace in read_dir(&checkout)? {
            clean_build_scratch(&workspace)?;
        }
    }
    Ok(())
}

impl PendingSegment {
    #[must_use]
    pub const fn id(&self) -> SegmentId {
        self.id
    }

    pub fn writer(&mut self) -> &mut SegmentWriter {
        &mut self.writer
    }
}

/// What one publication changes, relative to the session's current
/// generation.
#[derive(Clone, Debug, Default)]
pub struct Publication {
    /// Finished segments of this session to add.
    pub add_segments: Vec<SegmentId>,
    /// Segments of the current generation to leave out.
    pub remove_segments: Vec<SegmentId>,
    /// New checkout states written by this session.
    pub states: Vec<StateRef>,
    /// Checkouts to leave out of the new generation.
    pub retire_checkouts: Vec<CheckoutId>,
}

/// The result of one reclamation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimReport {
    pub outcome: ReclaimOutcome,
    /// The horizon in force after this reclamation: no generation below it
    /// can be read any more.
    pub horizon: Generation,
    pub removed_segments: usize,
    pub removed_states: usize,
    pub removed_manifests: usize,
    pub removed_horizons: usize,
    /// Lease records and owner files of ended processes.
    pub removed_leases: usize,
    /// Files that could not be removed this time, such as a segment another
    /// Windows process still has open. The next reclamation retries them.
    pub failed_removals: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimOutcome {
    /// Everything nothing can read was removed.
    Complete,
    /// A reader leased a generation below the new horizon while it was being
    /// written. Nothing was removed; a later reclamation will retry.
    Raced,
    /// A live process's lease could not be read, so what it protects is
    /// unknown. Nothing was removed.
    UnreadableLeases(usize),
}

impl<'f> WriteSession<'f> {
    pub(super) fn start(family: &'f Family, lock: File) -> Result<Self, FamilyError> {
        let layout = &family.layout;
        // A generation a newer build wrote, or one naming a file it wrote,
        // is refused before anything here is removed or written: an older
        // build must never change what a newer one left.
        let (base, _) = family.newest_manifest()?;
        family.check_named_formats(&base)?;
        sweep_build_scratch(&layout.dir.join("builds"))?;
        // Only the lock holder writes `tmp/`, so whatever is there was left
        // by an earlier writer that ended before publishing it.
        let mut next_segment = 1;
        for entry in read_dir(&layout.tmp_dir())? {
            if let Some(id) = segment_id_of(&entry) {
                next_segment = next_segment.max(id + 1);
            }
            crate::fs::remove_file(&entry).map_err(FamilyError::io(&entry))?;
        }
        for entry in read_dir(&layout.segments_dir())? {
            if let Some(id) = segment_id_of(&entry) {
                next_segment = next_segment.max(id + 1);
            }
        }
        next_segment = next_segment.max(base.next_segment().get());
        let manifests = numbered_desc(&layout.dir, MANIFEST_PREFIX)?;
        let next_generation = manifests
            .first()
            .map_or(1, |(number, _)| number + 1)
            .max(base.generation().get() + 1);
        // A manifest above the newest valid one is a publication torn by an
        // earlier writer's crash. No reader could take it, and its number is
        // already behind `next_generation`, so it is never reused.
        for (number, path) in &manifests {
            if *number > base.generation().get() {
                crate::fs::remove_file(path).map_err(FamilyError::io(path))?;
            }
        }
        let next_horizon = numbered_desc(&layout.dir, HORIZON_PREFIX)?
            .first()
            .map_or(1, |(number, _)| number + 1);
        Ok(Self {
            family,
            _lock: lock,
            incarnation: WriterIncarnation::new(u64::from_le_bytes(crate::le::array(
                lease::fresh_id().as_bytes(),
                0,
            ))),
            base,
            next_generation: Generation::new(next_generation),
            next_horizon,
            next_segment,
            next_state: HashMap::new(),
            open_segments: BTreeSet::new(),
            staged_segments: BTreeMap::new(),
            staged_states: BTreeMap::new(),
            poisoned: false,
        })
    }

    /// The newest published generation: the one this session's next
    /// publication changes.
    #[must_use]
    pub fn base(&self) -> &Manifest {
        &self.base
    }

    #[must_use]
    pub const fn incarnation(&self) -> WriterIncarnation {
        self.incarnation
    }

    /// Starts a new segment in `tmp/`.
    ///
    /// # Errors
    ///
    /// When the file cannot be created.
    pub fn create_segment(&mut self) -> Result<PendingSegment, FamilyError> {
        let id = SegmentId::new(self.next_segment);
        self.next_segment += 1;
        let path = self.family.layout.tmp_segment(id);
        crate::fault::point("segment.create").map_err(FamilyError::io(&path))?;
        let writer = SegmentWriter::create(
            &path,
            SegmentMeta {
                family: self.family.id,
                segment: id,
                incarnation: self.incarnation,
                created_unix_ms: now_unix_ms(),
            },
        )?;
        self.open_segments.insert(id);
        Ok(PendingSegment { id, writer })
    }

    /// Allocates `builds/<checkout>/<build-id>/`, cleaned on drop or by the
    /// next writer after its process ends. Creation is under `writer.lock`;
    /// a checkout can have only one live workspace, shared by its builders.
    ///
    /// # Errors
    ///
    /// When the directory or its owner lock cannot be created, or the
    /// checkout already has a live workspace.
    pub fn create_scratch<E>(
        &mut self,
        checkout: CheckoutId,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<BuildScratch<E>, FamilyError> {
        let parent = self
            .family
            .layout
            .dir
            .join("builds")
            .join(checkout.to_string());
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        let mut parents = builder;
        parents
            .recursive(true)
            .create(&parent)
            .map_err(FamilyError::io(&parent))?;
        for workspace in read_dir(&parent)? {
            clean_build_scratch(&workspace)?;
            if workspace.exists() {
                return Err(FamilyError::Misuse(
                    "this checkout already has a live build workspace",
                ));
            }
        }
        let path = parent.join(lease::fresh_id().to_string());
        parents
            .recursive(false)
            .create(&path)
            .map_err(FamilyError::io(&path))?;
        let mut scratch = BuildScratch {
            path,
            owner: None,
            admit: Box::new(admit),
        };
        let owner_path = scratch.path.join("owner");
        let owner =
            crate::fs::create_new_private(&owner_path).map_err(FamilyError::io(&owner_path))?;
        owner.lock().map_err(FamilyError::io(&owner_path))?;
        scratch.owner = Some(owner);
        Ok(scratch)
    }

    /// Finishes a segment and stages it for publication.
    ///
    /// # Errors
    ///
    /// When the segment cannot be finished or was not created by this
    /// session.
    pub fn finish_segment(&mut self, pending: PendingSegment) -> Result<SegmentId, FamilyError> {
        if !self.open_segments.remove(&pending.id) {
            return Err(FamilyError::Misuse(
                "the segment was not created by this session",
            ));
        }
        crate::fault::point("segment.finish")
            .map_err(FamilyError::io(&self.family.layout.tmp_segment(pending.id)))?;
        let summary = pending.writer.finish()?;
        self.staged_segments.insert(pending.id, summary.len);
        Ok(pending.id)
    }

    /// Writes a new state file for `checkout`, durably, and stages it.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn write_state(
        &mut self,
        checkout: CheckoutId,
        payload: &[u8],
    ) -> Result<StateRef, FamilyError> {
        let layout = &self.family.layout;
        let dir = layout.checkout_dir(checkout);
        let number = match self.next_state.get(&checkout) {
            Some(number) => *number,
            None => {
                if !dir.is_dir() {
                    std::fs::create_dir_all(&dir).map_err(FamilyError::io(&dir))?;
                    // The manifest will name a file inside this new
                    // directory, so the directory's own entry must be
                    // durable first: sync the folder that holds it.
                    if let Some(parent) = dir.parent() {
                        crate::fs::sync_dir(parent).map_err(FamilyError::io(parent))?;
                    }
                }
                numbered_desc(&dir, STATE_PREFIX)?
                    .first()
                    .map_or(1, |(number, _)| number + 1)
            }
        };
        self.next_state.insert(checkout, number + 1);
        let path = layout.state(checkout, number);
        record::write(&path, Kind::State, self.family.id, number, payload)
            .map_err(FamilyError::io(&path))?;
        let state = StateRef {
            checkout,
            number,
            len: payload.len() as u64,
            digest: Digest128::of(payload),
        };
        self.staged_states.insert((checkout, number), state);
        Ok(state)
    }

    /// Writes a checkout state as a new state file and stages it.
    ///
    /// # Errors
    ///
    /// As [`write_state`](Self::write_state).
    pub fn write_checkout_state(
        &mut self,
        checkout: CheckoutId,
        state: &CheckoutState,
    ) -> Result<StateRef, FamilyError> {
        self.write_state(checkout, &state.encode())
    }

    /// Publishes a new generation: the current one with `change` applied.
    /// Returns the new generation.
    ///
    /// # Errors
    ///
    /// When `change` names something this session did not stage or the
    /// current generation does not hold, or a file step fails. After a failed
    /// file step the session cannot publish again.
    pub fn publish(&mut self, change: &Publication) -> Result<Generation, FamilyError> {
        if self.poisoned {
            return Err(FamilyError::Poisoned);
        }
        let mut segments = self
            .base
            .segments()
            .iter()
            .map(|entry| (entry.id, entry.len))
            .collect::<BTreeMap<_, _>>();
        for id in &change.remove_segments {
            if segments.remove(id).is_none() {
                return Err(FamilyError::Misuse(
                    "the publication removes a segment the current generation does not hold",
                ));
            }
        }
        for id in &change.add_segments {
            let Some(len) = self.staged_segments.get(id) else {
                return Err(FamilyError::Misuse(
                    "the publication adds a segment this session did not finish",
                ));
            };
            segments.insert(*id, *len);
        }
        let mut checkouts = self
            .base
            .checkouts()
            .iter()
            .map(|state| (state.checkout, *state))
            .collect::<BTreeMap<_, _>>();
        for checkout in &change.retire_checkouts {
            checkouts.remove(checkout);
        }
        for state in &change.states {
            if self.staged_states.get(&(state.checkout, state.number)) != Some(state) {
                return Err(FamilyError::Misuse(
                    "the publication names a state this session did not write",
                ));
            }
            if change.retire_checkouts.contains(&state.checkout) {
                return Err(FamilyError::Misuse(
                    "the publication both updates and retires a checkout",
                ));
            }
            checkouts.insert(state.checkout, *state);
        }

        self.poisoned = true;
        let layout = &self.family.layout;
        for id in &change.add_segments {
            let from = layout.tmp_segment(*id);
            let to = layout.segment(*id);
            crate::fs::rename_new_durable(&from, &to).map_err(FamilyError::io(&to))?;
            self.staged_segments.remove(id);
        }
        let generation = self.next_generation;
        let manifest = Manifest::new(
            generation,
            now_unix_ms(),
            SegmentId::new(self.next_segment),
            segments
                .into_iter()
                .map(|(id, len)| SegmentEntry { id, len })
                .collect(),
            checkouts.into_values().collect(),
        );
        let path = layout.manifest(generation);
        record::write(
            &path,
            Kind::Manifest,
            self.family.id,
            generation.get(),
            &manifest.encode(),
        )
        .map_err(FamilyError::io(&path))?;
        for state in &change.states {
            self.staged_states.remove(&(state.checkout, state.number));
        }
        self.base = manifest;
        self.next_generation = generation.next();
        self.poisoned = false;
        Ok(generation)
    }

    /// Removes every file no reader can reach: segments, states, manifests
    /// and horizons that no manifest at or above the horizon names, and the
    /// leases of ended processes. Files this session staged and has not
    /// published are kept.
    ///
    /// # Errors
    ///
    /// When the family's files cannot be listed, read or removed.
    pub fn reclaim(&mut self) -> Result<ReclaimReport, FamilyError> {
        let layout = self.family.layout.clone();
        let family = self.family.id;
        let mut report = ReclaimReport {
            outcome: ReclaimOutcome::Complete,
            horizon: Generation::EMPTY,
            removed_segments: 0,
            removed_states: 0,
            removed_manifests: 0,
            removed_horizons: 0,
            removed_leases: 0,
            failed_removals: 0,
        };

        // Ended processes' files go first: they protect nothing.
        let mut scan = lease::scan(&layout, family)?;
        remove_dead(&mut scan, &mut report);
        let current = self.family.newest_horizon()?;
        report.horizon = current.map_or(Generation::EMPTY, |(_, horizon)| horizon);
        if scan.unreadable > 0 {
            report.outcome = ReclaimOutcome::UnreadableLeases(scan.unreadable);
            return Ok(report);
        }

        // The newest generation always survives, so with no lease at all
        // the horizon is the newest generation, never beyond it.
        let newest = self.base.generation();
        let horizon = scan
            .lowest_live()
            .map_or(newest, |lowest| lowest.min(newest));
        if current.is_none_or(|(_, written)| written != horizon) {
            if current.is_some_and(|(_, written)| written > horizon) {
                // A live lease below a horizon already written means a reader
                // raced an earlier reclamation; its generation is not safe to
                // read and it will start again. Never lower the horizon.
                report.outcome = ReclaimOutcome::Raced;
                return Ok(report);
            }
            let path = layout.horizon(self.next_horizon);
            record::write(
                &path,
                Kind::Horizon,
                family,
                self.next_horizon,
                &horizon.get().to_le_bytes(),
            )
            .map_err(FamilyError::io(&path))?;
            self.next_horizon += 1;
        }
        report.horizon = horizon;

        // A reader that read the old horizon must have its lease on disk by
        // now; one that reads the new horizon will start again.
        let mut rescan = lease::scan(&layout, family)?;
        if rescan.unreadable > 0 {
            report.outcome = ReclaimOutcome::UnreadableLeases(rescan.unreadable);
            return Ok(report);
        }
        if rescan.lowest_live().is_some_and(|lowest| lowest < horizon) {
            report.outcome = ReclaimOutcome::Raced;
            return Ok(report);
        }

        // Everything named by a generation at or above the horizon stays.
        let mut keep_segments = BTreeSet::new();
        let mut keep_states = BTreeSet::new();
        let mut manifests_to_remove = Vec::new();
        for (number, path) in numbered_desc(&layout.dir, MANIFEST_PREFIX)? {
            let generation = Generation::new(number);
            if generation > newest {
                // Only this session publishes, and it is not publishing now.
                continue;
            }
            if generation < horizon {
                manifests_to_remove.push(path);
                continue;
            }
            let manifest = match record::read(&path, Kind::Manifest, family, number)
                .and_then(|payload| Manifest::decode(generation, &payload))
            {
                Ok(manifest) => manifest,
                // Never valid, so never read: a torn publication.
                Err(RecordError::Invalid(_)) => {
                    manifests_to_remove.push(path);
                    continue;
                }
                Err(error) => return Err(FamilyError::record(&path, error)),
            };
            keep_segments.extend(manifest.segments().iter().map(|entry| entry.id));
            keep_states.extend(
                manifest
                    .checkouts()
                    .iter()
                    .map(|state| (state.checkout, state.number)),
            );
        }
        keep_states.extend(self.staged_states.keys().copied());

        for path in read_dir(&layout.segments_dir())? {
            let keep =
                segment_id_of(&path).is_some_and(|id| keep_segments.contains(&SegmentId::new(id)));
            if !keep && remove(&path, &mut report) {
                report.removed_segments += 1;
            }
        }
        for checkout_dir in read_dir(&layout.checkouts_dir())? {
            let Some(checkout) = checkout_id_of(&checkout_dir) else {
                continue;
            };
            let mut left = 0;
            for (number, path) in numbered_desc(&checkout_dir, STATE_PREFIX)? {
                if keep_states.contains(&(checkout, number)) {
                    left += 1;
                } else if remove(&path, &mut report) {
                    report.removed_states += 1;
                }
            }
            if left == 0 && !self.next_state.contains_key(&checkout) {
                // Only empty directories are removed; one gaining a file in
                // between keeps it.
                let _ = std::fs::remove_dir(&checkout_dir);
            }
        }
        for path in manifests_to_remove {
            if remove(&path, &mut report) {
                report.removed_manifests += 1;
            }
        }
        // Only the newest valid horizon is ever read.
        let newest_horizon = self.family.newest_horizon()?.map(|(number, _)| number);
        for (number, path) in numbered_desc(&layout.dir, HORIZON_PREFIX)? {
            if Some(number) != newest_horizon && remove(&path, &mut report) {
                report.removed_horizons += 1;
            }
        }
        remove_dead(&mut rescan, &mut report);
        Ok(report)
    }
}

/// Removes one file nothing can reach. A failure is counted, not returned:
/// the file stays unreachable, and the next reclamation tries again.
fn remove(path: &std::path::Path, report: &mut ReclaimReport) -> bool {
    if crate::fs::remove_file(path).is_ok() {
        true
    } else {
        report.failed_removals += 1;
        false
    }
}

/// Removes the files of ended processes found by `scan`. An owner file goes
/// after its leases, so a lease is never left without its owner's verdict.
/// The owner locks are released as their files go: on Windows a removed
/// file stays listed until its last handle closes.
fn remove_dead(scan: &mut LeaseScan, report: &mut ReclaimReport) {
    for path in scan
        .dead_leases
        .drain(..)
        .chain(scan.abandoned_incoming.drain(..))
    {
        if remove(&path, report) {
            report.removed_leases += 1;
        }
    }
    for owner in scan.dead_owners.drain(..) {
        if remove(&owner.path, report) {
            report.removed_leases += 1;
        }
    }
}

fn read_dir(dir: &std::path::Path) -> Result<Vec<PathBuf>, FamilyError> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(FamilyError::io(dir))? {
        paths.push(entry.map_err(FamilyError::io(dir))?.path());
    }
    paths.sort();
    Ok(paths)
}

fn segment_id_of(path: &std::path::Path) -> Option<u64> {
    path.file_name()?
        .to_str()?
        .strip_suffix(".seg")
        .and_then(super::canonical_number)
}

fn checkout_id_of(path: &std::path::Path) -> Option<CheckoutId> {
    let name = path.file_name()?.to_str()?;
    if name.len() != 2 * Digest128::LEN {
        return None;
    }
    let mut bytes = [0; Digest128::LEN];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(name.get(2 * index..2 * index + 2)?, 16).ok()?;
    }
    let checkout = CheckoutId::from_digest(Digest128::from_bytes(bytes));
    // Only the lowercase spelling this crate writes names a checkout.
    (checkout.to_string() == name).then_some(checkout)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
