//! Writing a family: building segments, publishing generations and
//! reclaiming files nothing can read any more.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

/// Private build files that outlive a write session and, when the build
/// fails, pauses or dies, the build too: dropping the workspace never
/// deletes it. The build deletes it with [`BuildScratch::remove`] once it
/// published or stood down. A later build of the same checkout may adopt it
/// (see [`ScratchClaim`]); a later writer's sweep deletes a dead one that no
/// build may adopt any more (see [`crate::family::workspace::worth_keeping`]).
///
/// A build is live while it holds its checkout's `checkout.lock`, which sits
/// beside the workspace, never inside it: the lock file must outlive every
/// folder that may be deleted, and Windows deletes no file held open.
pub struct BuildScratch<E> {
    path: PathBuf,
    /// The checkout's `checkout.lock`, locked for the workspace's life;
    /// `None` for a workspace no family sweeps.
    _lock: Option<File>,
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
    /// A workspace for a build that has no family to publish into, such as
    /// a test's or a measurement tool's, made under `parent` (created if
    /// missing). It takes no lock and no writer sweeps `parent`, so its
    /// builder must [`remove`](Self::remove) it whether the build succeeds
    /// or fails.
    ///
    /// # Errors
    ///
    /// When the directory cannot be created.
    pub fn create_unattached(
        parent: &Path,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<Self, FamilyError> {
        private_dir_builder()
            .recursive(true)
            .create(parent)
            .map_err(FamilyError::io(parent))?;
        Self::create_in(parent, None, admit)
    }

    /// Makes a fresh workspace folder under `parent`, which must exist.
    fn create_in(
        parent: &Path,
        lock: Option<File>,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<Self, FamilyError> {
        let path = parent.join(lease::fresh_id().to_string());
        private_dir_builder()
            .create(&path)
            .map_err(FamilyError::io(&path))?;
        Ok(BuildScratch {
            path,
            _lock: lock,
            admit: Box::new(admit),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Makes `name`, a folder of the workspace that one builder of the build
    /// keeps its files in. The returned handle keeps the workspace, and so
    /// its lock, alive.
    ///
    /// # Errors
    ///
    /// When the folder cannot be made, including when another builder of
    /// the build already made one of that name.
    pub fn subdir(self: &Arc<Self>, name: &str) -> std::io::Result<ScratchDir<E>> {
        let path = self.path.join(name);
        private_dir_builder().create(&path)?;
        Ok(ScratchDir {
            workspace: Arc::clone(self),
            path,
        })
    }

    /// [`Self::subdir`], or the folder an earlier build left when the
    /// workspace was adopted (see [`ScratchClaim::adopt`]).
    ///
    /// # Errors
    ///
    /// When the folder cannot be made.
    pub fn subdir_existing(self: &Arc<Self>, name: &str) -> std::io::Result<ScratchDir<E>> {
        let path = self.path.join(name);
        match private_dir_builder().create(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
        Ok(ScratchDir {
            workspace: Arc::clone(self),
            path,
        })
    }

    /// Records that the build stopped because the disk was short, so the
    /// workspace outlives other dead ones when disk is reclaimed (see
    /// [`WriteSession::reclaim_dead_workspaces`]). Adopting it clears the
    /// mark.
    ///
    /// # Errors
    ///
    /// When the mark cannot be written.
    pub fn mark_paused(&self) -> std::io::Result<()> {
        std::fs::File::create(self.path.join(crate::family::workspace::PAUSED)).map(drop)
    }

    /// Asks the caller's storage admission policy before writing a run.
    ///
    /// # Errors
    ///
    /// Returns the policy's refusal without writing anything.
    pub fn check_space(&self, additional_bytes: u64) -> Result<(), E> {
        (self.admit)(additional_bytes)
    }

    /// Deletes the workspace with everything in it. A build calls this once
    /// its publication is in the manifest, or when it stands down; every
    /// builder must have closed its files first, since Windows deletes no
    /// file held open. The checkout's lock stays held until the last
    /// holder drops the workspace.
    ///
    /// # Errors
    ///
    /// When the folder exists and cannot be deleted.
    pub fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// A builder's own folder inside a shared [`BuildScratch`]. Holding it keeps
/// the whole workspace alive.
pub struct ScratchDir<E> {
    workspace: Arc<BuildScratch<E>>,
    path: PathBuf,
}

impl<E> std::fmt::Debug for ScratchDir<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScratchDir")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl<E> ScratchDir<E> {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The workspace this folder is in, for its disk admission check.
    #[must_use]
    pub fn workspace(&self) -> &Arc<BuildScratch<E>> {
        &self.workspace
    }
}

/// Folders only their owner can enter, where the platform has such modes.
fn private_dir_builder() -> std::fs::DirBuilder {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder
}

#[cfg(test)]
mod build_scratch_tests {
    use super::*;
    use crate::ids::FamilyId;

    fn checkout(name: &[u8]) -> CheckoutId {
        CheckoutId::from_digest(Digest128::of(name))
    }

    #[test]
    fn build_scratch_live_abandoned_and_torn_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"scratch"))).unwrap();
        let scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(checkout(b"checkout"), |bytes| {
                if bytes <= 100 {
                    Ok(())
                } else {
                    Err("disk floor")
                }
            })
            .unwrap();
        assert!(scratch.check_space(100).is_ok());
        assert_eq!(scratch.check_space(101), Err("disk floor"));
        let path = scratch.path().to_path_buf();
        let checkout_dir = path.parent().unwrap().to_path_buf();
        assert_eq!(checkout_dir.parent().unwrap(), family.dir().join("builds"));
        assert_eq!(
            std::fs::read_dir(&path).unwrap().count(),
            0,
            "the lock is beside the workspace, not inside it"
        );
        assert!(checkout_dir.join("checkout.lock").is_file());
        std::fs::write(path.join("partial.seg"), b"unfinished").unwrap();
        let mut session = family.try_write().unwrap().unwrap();
        assert!(
            session
                .create_scratch(checkout(b"checkout"), |_| Ok::<_, ()>(()))
                .is_err()
        );
        let other = session
            .create_scratch(checkout(b"other"), |_| Ok::<_, ()>(()))
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
        drop(scratch);
        assert!(
            path.join("partial.seg").exists(),
            "dropping a workspace does not delete it"
        );
        drop(family.try_write().unwrap().unwrap());
        assert!(!path.exists(), "startup removes an abandoned build");
        assert!(
            checkout_dir.join("checkout.lock").is_file(),
            "the sweep keeps the lock file"
        );
        let torn = checkout_dir.join("torn");
        std::fs::create_dir(&torn).unwrap();
        drop(family.try_write().unwrap().unwrap());
        assert!(!torn.exists(), "startup removes a half-made workspace");
    }

    #[test]
    fn build_scratch_survives_an_error_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"drop"))).unwrap();
        let mut path = PathBuf::new();
        let result = (|| -> Result<(), &str> {
            let scratch = family
                .try_write()
                .unwrap()
                .unwrap()
                .create_scratch(checkout(b"checkout"), |_| Ok::<_, &str>(()))
                .unwrap();
            path = scratch.path().to_path_buf();
            std::fs::write(path.join("partial.seg"), b"unfinished").unwrap();
            Err("cancelled")
        })();
        assert!(result.is_err());
        assert!(
            path.join("partial.seg").exists(),
            "an error or pause leaves the build's work on disk"
        );
        let scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(checkout(b"checkout"), |_| Ok::<_, &str>(()))
            .unwrap();
        assert!(!path.exists(), "the next build of the checkout clears it");
        let next = scratch.path().to_path_buf();
        scratch.remove().unwrap();
        assert!(!next.exists());
        scratch.remove().unwrap();
    }

    #[test]
    fn a_workspace_with_an_identity_is_kept_for_its_checkout_to_adopt() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"adopt"))).unwrap();
        let root = dir.path().join("checkout-root");
        std::fs::create_dir(&root).unwrap();
        let scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(checkout(b"checkout"), |_| Ok::<_, ()>(()))
            .unwrap();
        let kept = scratch.path().to_path_buf();
        crate::family::workspace::write_identity(
            &kept,
            &crate::family::workspace::WorkspaceIdentity {
                root: root.clone(),
                payload: b"one".to_vec(),
            },
        )
        .unwrap();
        std::fs::write(kept.join("staged"), b"work").unwrap();
        scratch.mark_paused().unwrap();
        drop(scratch);
        drop(family.try_write().unwrap().unwrap());
        assert!(kept.join("staged").exists(), "the sweep keeps it");

        let claim = family
            .try_write()
            .unwrap()
            .unwrap()
            .claim_scratch(checkout(b"checkout"))
            .unwrap();
        let torn = claim.dir().join("torn");
        std::fs::create_dir(&torn).unwrap();
        assert_eq!(claim.leftovers().unwrap().len(), 2);
        assert!(
            family
                .try_write()
                .unwrap()
                .unwrap()
                .claim_scratch(checkout(b"checkout"))
                .is_err(),
            "one claim at a time"
        );
        let adopted = claim.adopt(&kept, |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(adopted.path(), kept);
        assert!(!torn.exists(), "adoption deletes the other leftovers");
        assert!(!crate::family::workspace::is_paused(&kept));
        assert!(kept.join(crate::family::workspace::ADOPTED).exists());
        let again = Arc::new(adopted);
        let records = again.subdir_existing("records").unwrap();
        assert!(again.subdir_existing("records").is_ok());
        drop(records);
        drop(again);

        // Its checkout gone, the next writer deletes it.
        std::fs::remove_dir(&root).unwrap();
        drop(family.try_write().unwrap().unwrap());
        assert!(!kept.exists());
    }

    #[test]
    fn disk_reclaim_deletes_other_dead_workspaces_oldest_first_and_paused_last() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"reclaim"))).unwrap();
        let root = dir.path().join("checkout-root");
        std::fs::create_dir(&root).unwrap();
        let mut made = Vec::new();
        for (name, paused) in [
            (b"old".as_slice(), false),
            (b"paused", true),
            (b"new", false),
        ] {
            let scratch = family
                .try_write()
                .unwrap()
                .unwrap()
                .create_scratch(checkout(name), |_| Ok::<_, ()>(()))
                .unwrap();
            crate::family::workspace::write_identity(
                scratch.path(),
                &crate::family::workspace::WorkspaceIdentity {
                    root: root.clone(),
                    payload: Vec::new(),
                },
            )
            .unwrap();
            if paused {
                scratch.mark_paused().unwrap();
            }
            made.push(scratch.path().to_path_buf());
            // Distinct modification times, oldest first.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let own = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(checkout(b"own"), |_| Ok::<_, ()>(()))
            .unwrap();
        let mut asked = 0;
        let removed = family
            .try_write()
            .unwrap()
            .unwrap()
            .reclaim_dead_workspaces(
                Some(checkout(b"own")),
                || {
                    asked += 1;
                    asked <= 1
                },
                || false,
            )
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!made[0].exists(), "the oldest dead one goes first");
        assert!(made[1].exists() && made[2].exists());
        let removed = family
            .try_write()
            .unwrap()
            .unwrap()
            .reclaim_dead_workspaces(Some(checkout(b"own")), || true, || false)
            .unwrap();
        assert_eq!(removed, 1);
        assert!(made[1].exists(), "a paused one stays above the floor");
        assert!(!made[2].exists());
        let removed = family
            .try_write()
            .unwrap()
            .unwrap()
            .reclaim_dead_workspaces(Some(checkout(b"own")), || true, || true)
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!made[1].exists(), "below the floor a paused one goes too");
        assert!(
            own.path().exists(),
            "a live build's workspace is never reclaimed"
        );
    }

    #[test]
    fn build_scratch_subdirs_share_one_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = Arc::new(
            BuildScratch::create_unattached(&dir.path().join("builds"), |bytes| {
                if bytes <= 10 {
                    Ok(())
                } else {
                    Err("disk floor")
                }
            })
            .unwrap(),
        );
        let root = scratch.path().to_path_buf();
        let text = scratch.subdir("text").unwrap();
        let graph = scratch.subdir("graph").unwrap();
        assert_eq!(text.path(), root.join("text"));
        assert_eq!(graph.path(), root.join("graph"));
        assert!(
            scratch.subdir("text").is_err(),
            "two builders never share one folder"
        );
        assert_eq!(graph.workspace().check_space(11), Err("disk floor"));
        std::fs::write(text.path().join("run"), b"partial").unwrap();
        drop(text);
        drop(graph);
        assert!(root.join("text").join("run").exists());
        scratch.remove().unwrap();
        assert!(!root.exists());
    }
}

/// Opens `dir`'s `checkout.lock`, creating it, and takes it if no live
/// build holds it. `None` means a live build does.
fn try_checkout_lock(dir: &Path) -> Result<Option<File>, FamilyError> {
    let path = dir.join("checkout.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(FamilyError::io(&path))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(FamilyError::io(&path)(error)),
    }
}

/// Deletes the workspace folders of a checkout whose `checkout.lock` the
/// caller holds, so none of them belongs to a live build, except those a
/// later build of that checkout may still adopt (see
/// [`crate::family::workspace::worth_keeping`]). Only the build for that checkout
/// compares a workspace's identity with its own; any other process judges
/// it by liveness, its checkout's existence and its age. The lock file
/// itself stays: another process may be about to open it.
fn remove_dead_workspaces(dir: &Path) -> Result<(), FamilyError> {
    let now = SystemTime::now();
    for workspace in read_dir(dir)? {
        if !workspace.is_dir() || crate::family::workspace::worth_keeping(&workspace, now) {
            continue;
        }
        remove_workspace_dir(&workspace)?;
    }
    Ok(())
}

fn remove_workspace_dir(workspace: &Path) -> Result<(), FamilyError> {
    match std::fs::remove_dir_all(workspace) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(FamilyError::io(workspace)(error))
        }
        _ => Ok(()),
    }
}

/// A checkout's build folder, `builds/<checkout>/`, with its
/// `checkout.lock` held: no other build of the checkout runs while the claim
/// or the workspace made from it lives. The claimant looks at the
/// workspaces earlier builds left ([`Self::leftovers`]) and either adopts
/// one ([`Self::adopt`]) or starts a new one ([`Self::fresh`]).
#[derive(Debug)]
pub struct ScratchClaim {
    dir: PathBuf,
    lock: File,
}

impl ScratchClaim {
    /// `builds/<checkout>/`, which holds the lock and the workspaces.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The workspace folders earlier builds of the checkout left, oldest
    /// first by when they were last made or adopted.
    ///
    /// # Errors
    ///
    /// When the folder cannot be listed.
    pub fn leftovers(&self) -> Result<Vec<PathBuf>, FamilyError> {
        let mut found = read_dir(&self.dir)?
            .into_iter()
            .filter(|path| path.is_dir())
            .map(|path| (crate::family::workspace::last_used(&path), path))
            .collect::<Vec<_>>();
        found.sort();
        Ok(found.into_iter().map(|(_, path)| path).collect())
    }

    /// Deletes `workspace`, one of [`Self::leftovers`].
    ///
    /// # Errors
    ///
    /// When it exists and cannot be deleted.
    pub fn remove(&self, workspace: &Path) -> Result<(), FamilyError> {
        if workspace.parent() != Some(self.dir.as_path()) {
            return Err(FamilyError::Misuse("not a workspace of this checkout"));
        }
        remove_workspace_dir(workspace)
    }

    /// Continues in `workspace`, one of [`Self::leftovers`], deleting every
    /// other one. Marks it adopted now and clears its pause mark.
    ///
    /// # Errors
    ///
    /// When `workspace` is not one of this checkout's, or a file cannot be
    /// written or deleted.
    pub fn adopt<E>(
        self,
        workspace: &Path,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<BuildScratch<E>, FamilyError> {
        if workspace.parent() != Some(self.dir.as_path()) || !workspace.is_dir() {
            return Err(FamilyError::Misuse("not a workspace of this checkout"));
        }
        for other in self.leftovers()? {
            if other != workspace {
                remove_workspace_dir(&other)?;
            }
        }
        crate::family::workspace::mark_adopted(workspace).map_err(FamilyError::io(workspace))?;
        let paused = workspace.join(crate::family::workspace::PAUSED);
        match std::fs::remove_file(&paused) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(FamilyError::io(&paused)(error));
            }
            _ => {}
        }
        Ok(BuildScratch {
            path: workspace.to_path_buf(),
            _lock: Some(self.lock),
            admit: Box::new(admit),
        })
    }

    /// Starts a new workspace, after deleting every leftover no later build
    /// may adopt (see [`remove_dead_workspaces`]); a caller that adopts
    /// none of them deletes them with [`Self::remove`] first.
    ///
    /// # Errors
    ///
    /// When a folder cannot be made or deleted.
    pub fn fresh<E>(
        self,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<BuildScratch<E>, FamilyError> {
        remove_dead_workspaces(&self.dir)?;
        BuildScratch::create_in(&self.dir, Some(self.lock), admit)
    }
}

fn sweep_build_scratch(builds: &Path) -> Result<(), FamilyError> {
    if !builds.exists() {
        return Ok(());
    }
    for checkout in read_dir(builds)? {
        if !checkout.is_dir() {
            continue;
        }
        if let Some(_lock) = try_checkout_lock(&checkout)? {
            remove_dead_workspaces(&checkout)?;
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

    /// Allocates `builds/<checkout>/<build-id>/` and takes the checkout's
    /// `builds/<checkout>/checkout.lock`, first deleting every workspace an
    /// earlier build of the checkout left that no later build may adopt
    /// (see [`ScratchClaim::fresh`]). The build deletes the new one with
    /// [`BuildScratch::remove`]; one it leaves is judged by the next writer
    /// that finds the lock free. Creation is under `writer.lock`; a checkout
    /// can have only one live workspace, shared by its builders.
    ///
    /// # Errors
    ///
    /// When the directory or the lock file cannot be created, or the
    /// checkout already has a live workspace.
    pub fn create_scratch<E>(
        &mut self,
        checkout: CheckoutId,
        admit: impl Fn(u64) -> Result<(), E> + Send + Sync + 'static,
    ) -> Result<BuildScratch<E>, FamilyError> {
        self.claim_scratch(checkout)?.fresh(admit)
    }

    /// Takes `builds/<checkout>/checkout.lock` for a build that may adopt
    /// a workspace an earlier build of the checkout left (see
    /// [`ScratchClaim`]). Creation is under `writer.lock`; the claim
    /// outlives the session.
    ///
    /// # Errors
    ///
    /// When the directory or the lock file cannot be created, or the
    /// checkout already has a live build.
    pub fn claim_scratch(&mut self, checkout: CheckoutId) -> Result<ScratchClaim, FamilyError> {
        let parent = self
            .family
            .layout
            .dir
            .join("builds")
            .join(checkout.to_string());
        private_dir_builder()
            .recursive(true)
            .create(&parent)
            .map_err(FamilyError::io(&parent))?;
        let Some(lock) = try_checkout_lock(&parent)? else {
            return Err(FamilyError::Misuse(
                "this checkout already has a live build workspace",
            ));
        };
        // Holding the lock, no workspace left here belongs to a live build.
        Ok(ScratchClaim { dir: parent, lock })
    }

    /// Deletes dead workspaces of every checkout but `keep`, oldest first,
    /// while `short` says the disk is still short of what the next build
    /// needs. A workspace a build left when it stopped for lack of disk goes
    /// only after every other one, and only while `below_floor` says the
    /// disk is below the free-space floor itself. Returns how many were
    /// deleted.
    ///
    /// # Errors
    ///
    /// When a folder cannot be listed or deleted.
    pub fn reclaim_dead_workspaces(
        &mut self,
        keep: Option<CheckoutId>,
        mut short: impl FnMut() -> bool,
        mut below_floor: impl FnMut() -> bool,
    ) -> Result<usize, FamilyError> {
        let builds = self.family.layout.dir.join("builds");
        if !builds.exists() {
            return Ok(0);
        }
        let keep = keep.map(|checkout| checkout.to_string());
        let mut dead = Vec::new();
        for checkout in read_dir(&builds)? {
            if !checkout.is_dir()
                || (keep.is_some()
                    && checkout.file_name().and_then(|name| name.to_str()) == keep.as_deref())
            {
                continue;
            }
            // Held only while listing: a build of that checkout that starts
            // meanwhile is noticed again before each deletion.
            if try_checkout_lock(&checkout)?.is_none() {
                continue;
            }
            for workspace in read_dir(&checkout)? {
                if workspace.is_dir() {
                    let paused = crate::family::workspace::is_paused(&workspace);
                    dead.push((
                        paused,
                        crate::family::workspace::last_used(&workspace),
                        checkout.clone(),
                        workspace,
                    ));
                }
            }
        }
        dead.sort();
        let mut removed = 0;
        for (paused, _, checkout, workspace) in dead {
            let wanted = if paused { below_floor() } else { short() };
            if !wanted {
                continue;
            }
            let Some(_lock) = try_checkout_lock(&checkout)? else {
                continue;
            };
            remove_workspace_dir(&workspace)?;
            removed += 1;
        }
        Ok(removed)
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
