//! A family's files and the protocol that publishes and reclaims them.
//!
//! A family is every checkout, clone and fork of one repository on this
//! machine. Its directory holds:
//!
//! ```text
//! writer.lock                         held by the one process writing the family
//! MANIFEST-<generation>               one per published generation
//! HORIZON-<n>                         the reclamation horizon; the newest valid one counts
//! segments/<id>.seg                   published segments
//! tmp/<id>.seg                        segments the current writer is still building
//! checkouts/<checkout>/STATE-<n>      checkout states
//! leases/<owner>.owner                one per process reading the family, locked while it lives
//! leases/<owner>.<n>.lease            one per snapshot in use: the generation it reads
//! incoming/<owner>.owner              an owner file before it is locked into place
//! ```
//!
//! Every file is written once under a new name and never changed, renamed
//! over or reused. The rules:
//!
//! - **Writing.** One process at a time holds `writer.lock` and does all
//!   writing: it builds segments in `tmp/`, renames them into `segments/`,
//!   writes checkout states, then publishes a new manifest naming exactly the
//!   segments and states of the new generation. A crash before the manifest
//!   is complete leaves the previous one current; the files it left behind
//!   are never read and are removed later.
//! - **Reading.** A reader reads the newest valid manifest, records a lease
//!   naming its generation, then reads the horizon. If the horizon is above
//!   the generation, a reclamation may be removing it, and the reader starts
//!   again. Otherwise everything that manifest names stays on disk until the
//!   lease is released.
//! - **Reclaiming.** The writer computes the horizon as the lowest of the
//!   newest generation and every live lease, writes it, checks the leases
//!   again, and only then removes files that no manifest at or above the
//!   horizon names. A lease that appeared below the horizon in between stops
//!   the reclamation.
//!
//! A process is alive while it holds the lock on its owner file. The
//! operating system releases that lock when the process ends however it
//! ends, so a lease never outlives its process, and a reused process id
//! means nothing.

mod lease;
mod manifest;
pub(crate) mod record;
mod snapshot;
mod writer;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use manifest::{Manifest, SegmentEntry, StateRef};
pub use snapshot::Snapshot;
pub use writer::{PendingSegment, Publication, ReclaimOutcome, ReclaimReport, WriteSession};

use crate::ids::{CheckoutId, FamilyId, Generation, SegmentId};
use crate::segment::SegmentError;
use lease::Owner;
use record::{Kind, RecordError};

/// A failure in a family's files, naming the file.
#[derive(Debug, thiserror::Error)]
pub enum FamilyError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A record written by an older format version or for another family.
    #[error("{}: {reason}", path.display())]
    Foreign { path: PathBuf, reason: String },
    /// A file a manifest names is missing or fails its checks.
    #[error("{}: {reason}", path.display())]
    Damaged { path: PathBuf, reason: String },
    /// A record or segment a newer build wrote, in a format version above
    /// the highest this build reads. Unlike every other file this build
    /// cannot use, it is refused: never read past its version, treated as
    /// absent, rebuilt over, reclaimed or replaced. Rolling back to an older
    /// build must not destroy what the newer one wrote.
    #[error(transparent)]
    NewerFormat(NewerFormat),
    /// A segment failure. A segment in a newer format version is reported
    /// as [`Self::NewerFormat`] instead, never as this.
    #[error(transparent)]
    Segment(SegmentError),
    #[error(
        "no snapshot after {0} tries: each generation read was reclaimed before its lease took hold"
    )]
    SnapshotRetries(u32),
    #[error(
        "this write session failed while publishing and cannot publish again; start a new session"
    )]
    Poisoned,
    #[error("{0}")]
    Misuse(&'static str),
}

impl FamilyError {
    /// True when a file cannot be used as it stands: an older build's format
    /// version wrote it, or its bytes failed their checks. A reader treats
    /// such a file as absent, and what it held is built again. An I/O
    /// failure is not one: the file may read fine next time. Neither is a
    /// file in a newer format version ([`Self::newer_format`]): it is
    /// refused, and nothing is built in its place.
    #[must_use]
    pub fn is_unusable_file(&self) -> bool {
        match self {
            Self::Foreign { .. } | Self::Damaged { .. } => true,
            Self::Segment(error) => {
                error.is_corruption()
                    || match error.kind() {
                        crate::segment::ErrorKind::UnsupportedVersion { found, supported } => {
                            found < supported
                        }
                        crate::segment::ErrorKind::UnknownRequiredSection { .. }
                        | crate::segment::ErrorKind::NotASegment => true,
                        _ => false,
                    }
            }
            _ => false,
        }
    }

    /// The refusal, when a newer build wrote the file in a format version
    /// above the highest this build reads.
    #[must_use]
    pub fn newer_format(&self) -> Option<&NewerFormat> {
        match self {
            Self::NewerFormat(refusal) => Some(refusal),
            _ => None,
        }
    }

    fn io(path: &Path) -> impl FnOnce(io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    fn record(path: &Path, error: RecordError) -> Self {
        match error {
            RecordError::Io(source) => Self::Io {
                path: path.to_path_buf(),
                source,
            },
            RecordError::Invalid(reason) => Self::Damaged {
                path: path.to_path_buf(),
                reason: format!("the record's bytes do not check ({reason})"),
            },
            RecordError::UnsupportedVersion { found, supported }
            | RecordError::NewerPayload { found, supported }
                if found > supported =>
            {
                Self::NewerFormat(NewerFormat {
                    path: path.to_path_buf(),
                    file: FileKind::Record,
                    found,
                    supported,
                })
            }
            foreign => Self::Foreign {
                path: path.to_path_buf(),
                reason: foreign.to_string(),
            },
        }
    }
}

impl From<SegmentError> for FamilyError {
    fn from(error: SegmentError) -> Self {
        match NewerFormat::of_segment(&error) {
            Some(refusal) => Self::NewerFormat(refusal),
            None => Self::Segment(error),
        }
    }
}

/// Which kind of family file a [`NewerFormat`] refusal names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A manifest, horizon, checkout state or lease record.
    Record,
    /// A segment.
    Segment,
}

/// A family file that a newer build wrote, in a format version above the
/// highest this build reads (see [`FamilyError::NewerFormat`]). For a
/// record, the version is the record's own or, for a checkout state, that
/// of the state layout its payload holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewerFormat {
    pub path: PathBuf,
    pub file: FileKind,
    /// The format version the file names.
    pub found: u16,
    /// The highest format version of its kind this build reads.
    pub supported: u16,
}

impl NewerFormat {
    /// The refusal a segment error carries, when the segment names a format
    /// version above the one this build reads.
    #[must_use]
    pub fn of_segment(error: &SegmentError) -> Option<Self> {
        match *error.kind() {
            crate::segment::ErrorKind::UnsupportedVersion { found, supported }
                if found > supported =>
            {
                Some(Self {
                    path: error.path().to_path_buf(),
                    file: FileKind::Segment,
                    found,
                    supported,
                })
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for NewerFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A record's refusal names either its own format version or that of
        // the layout its payload holds, so it says only "format version".
        let kind = match self.file {
            FileKind::Record => "",
            FileKind::Segment => "segment ",
        };
        write!(
            f,
            "{}: {kind}format version {} is not supported; this build reads version {}",
            self.path.display(),
            self.found,
            self.supported
        )
    }
}

impl std::error::Error for NewerFormat {}

/// One family's directory, opened by this process.
#[derive(Debug)]
pub struct Family {
    layout: Layout,
    id: FamilyId,
    owner: Mutex<Option<Arc<Owner>>>,
}

impl Family {
    /// Opens the family stored in `dir`, creating its directories if needed.
    ///
    /// # Errors
    ///
    /// When a directory cannot be created.
    pub fn open(dir: &Path, id: FamilyId) -> Result<Self, FamilyError> {
        let layout = Layout::new(dir);
        for sub in [
            layout.dir.clone(),
            layout.segments_dir(),
            layout.tmp_dir(),
            layout.checkouts_dir(),
            layout.leases_dir(),
            layout.incoming_dir(),
        ] {
            std::fs::create_dir_all(&sub).map_err(FamilyError::io(&sub))?;
        }
        Ok(Self {
            layout,
            id,
            owner: Mutex::new(None),
        })
    }

    #[must_use]
    pub const fn id(&self) -> FamilyId {
        self.id
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.layout.dir
    }

    /// Takes a consistent view of the newest generation. Everything the
    /// snapshot's manifest names stays on disk until the snapshot is dropped.
    ///
    /// # Errors
    ///
    /// When the family's files cannot be read, the newest manifest is from
    /// another format version or family, or reclamation kept outrunning the
    /// lease. A manifest or horizon a newer build wrote fails it with
    /// [`FamilyError::NewerFormat`].
    pub fn snapshot(&self) -> Result<Snapshot, FamilyError> {
        snapshot::take(self)
    }

    /// The newest generation a [`snapshot`](Self::snapshot) would read now,
    /// without taking a lease on it: for a reader that keeps a snapshot and
    /// wants to know, cheaply, whether a newer one has been published. The
    /// answer protects nothing; read the generation's files only through a
    /// snapshot.
    ///
    /// # Errors
    ///
    /// As [`snapshot`](Self::snapshot).
    pub fn newest_generation(&self) -> Result<Generation, FamilyError> {
        self.newest_manifest()
            .map(|(manifest, _)| manifest.generation())
    }

    /// Starts writing the family, if no other process is writing it.
    /// Returns `None` while another process holds the writer lock.
    ///
    /// # Errors
    ///
    /// When the lock file or the family's files cannot be read or cleaned,
    /// and with [`FamilyError::NewerFormat`], before anything is removed or
    /// written, when a newer build wrote the newest generation or a file it
    /// names (see [`check_formats`](Self::check_formats)).
    pub fn try_write(&self) -> Result<Option<WriteSession<'_>>, FamilyError> {
        let path = self.layout.writer_lock();
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&path).map_err(FamilyError::io(&path))?;
        match lock.try_lock() {
            Ok(()) => WriteSession::start(self, lock).map(Some),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(FamilyError::io(&path)(error)),
        }
    }

    /// Fails with [`FamilyError::NewerFormat`] when a newer build wrote the
    /// newest manifest, or a segment or checkout state it names, in a format
    /// version above the highest this build reads (for a checkout state, the
    /// record's own version or that of the state layout its payload holds):
    /// this build can then
    /// neither read the family whole nor write it without harming what the
    /// newer build wrote. Reads only each named file's header unless the
    /// header names a newer version. A named file that cannot be read is left
    /// to the reader that needs it, which reports why.
    ///
    /// # Errors
    ///
    /// That refusal, or a failure to read the newest manifest.
    pub fn check_formats(&self) -> Result<(), FamilyError> {
        let (manifest, _) = self.newest_manifest()?;
        self.check_named_formats(&manifest)
    }

    /// The [`check_formats`](Self::check_formats) refusal for the files
    /// `manifest` names.
    fn check_named_formats(&self, manifest: &Manifest) -> Result<(), FamilyError> {
        for entry in manifest.segments() {
            if let Some(error) = crate::segment::newer_version(&self.layout.segment(entry.id)) {
                return Err(error.into());
            }
        }
        for state in manifest.checkouts() {
            let path = self.layout.state(state.checkout, state.number);
            if let Some(error) = record::newer_version(&path, Kind::State, self.id, state.number)
                .or_else(|| crate::checkout::newer_state_format(&path, self.id, state.number))
            {
                return Err(FamilyError::record(&path, error));
            }
        }
        Ok(())
    }

    /// This process's owner file in the family, created on first use.
    fn owner(&self) -> Result<Arc<Owner>, FamilyError> {
        let mut owner = self
            .owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = owner.as_ref() {
            return Ok(owner.clone());
        }
        let created = Arc::new(Owner::create(&self.layout)?);
        *owner = Some(created.clone());
        Ok(created)
    }

    /// The newest manifest whose record checks, or the empty generation when
    /// there is none. Damaged or half-written manifests are skipped; their
    /// count is returned. A manifest from another format version or family
    /// fails the call instead: it is not this build's to ignore.
    fn newest_manifest(&self) -> Result<(Manifest, usize), FamilyError> {
        let mut skipped = 0;
        for (number, path) in numbered_desc(&self.layout.dir, MANIFEST_PREFIX)? {
            match record::read(&path, Kind::Manifest, self.id, number) {
                Ok(payload) => match Manifest::decode(Generation::new(number), &payload) {
                    Ok(manifest) => return Ok((manifest, skipped)),
                    Err(RecordError::Invalid(_)) => skipped += 1,
                    Err(error) => return Err(FamilyError::record(&path, error)),
                },
                Err(RecordError::Invalid(_)) => skipped += 1,
                Err(RecordError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(FamilyError::record(&path, error)),
            }
        }
        Ok((Manifest::empty(), skipped))
    }

    /// The newest valid horizon, as a generation, with its file number.
    fn newest_horizon(&self) -> Result<Option<(u64, Generation)>, FamilyError> {
        for (number, path) in numbered_desc(&self.layout.dir, HORIZON_PREFIX)? {
            match read_horizon(&path, self.id, number) {
                Ok(horizon) => return Ok(Some((number, horizon))),
                Err(RecordError::Invalid(_)) => {}
                Err(RecordError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(FamilyError::record(&path, error)),
            }
        }
        Ok(None)
    }
}

const MANIFEST_PREFIX: &str = "MANIFEST-";
const HORIZON_PREFIX: &str = "HORIZON-";
const STATE_PREFIX: &str = "STATE-";

fn read_horizon(path: &Path, family: FamilyId, number: u64) -> Result<Generation, RecordError> {
    let payload = record::read(path, Kind::Horizon, family, number)?;
    let mut input = record::Payload::new(&payload);
    let generation = Generation::new(input.u64()?);
    input.finish()?;
    Ok(generation)
}

/// Where each of a family's files lives.
#[derive(Clone, Debug)]
struct Layout {
    dir: PathBuf,
}

impl Layout {
    fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn writer_lock(&self) -> PathBuf {
        self.dir.join("writer.lock")
    }

    fn manifest(&self, generation: Generation) -> PathBuf {
        self.dir.join(format!("{MANIFEST_PREFIX}{generation}"))
    }

    fn horizon(&self, number: u64) -> PathBuf {
        self.dir.join(format!("{HORIZON_PREFIX}{number}"))
    }

    fn segments_dir(&self) -> PathBuf {
        self.dir.join("segments")
    }

    fn segment(&self, id: SegmentId) -> PathBuf {
        self.segments_dir().join(format!("{id}.seg"))
    }

    fn tmp_dir(&self) -> PathBuf {
        self.dir.join("tmp")
    }

    fn tmp_segment(&self, id: SegmentId) -> PathBuf {
        self.tmp_dir().join(format!("{id}.seg"))
    }

    fn checkouts_dir(&self) -> PathBuf {
        self.dir.join("checkouts")
    }

    fn checkout_dir(&self, checkout: CheckoutId) -> PathBuf {
        self.checkouts_dir().join(checkout.to_string())
    }

    fn state(&self, checkout: CheckoutId, number: u64) -> PathBuf {
        self.checkout_dir(checkout)
            .join(format!("{STATE_PREFIX}{number}"))
    }

    fn leases_dir(&self) -> PathBuf {
        self.dir.join("leases")
    }

    fn incoming_dir(&self) -> PathBuf {
        self.dir.join("incoming")
    }
}

/// The entries of `dir` named `<prefix><number>`, newest first. Other names
/// are ignored, and so is a number written with leading zeros or a sign, so
/// each number has exactly one spelling.
fn numbered_desc(dir: &Path, prefix: &str) -> Result<Vec<(u64, PathBuf)>, FamilyError> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(FamilyError::io(dir))? {
        let entry = entry.map_err(FamilyError::io(dir))?;
        let name = entry.file_name();
        let Some(number) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(canonical_number)
        else {
            continue;
        };
        found.push((number, entry.path()));
    }
    found.sort_by_key(|(number, _)| std::cmp::Reverse(*number));
    Ok(found)
}

/// Parses a decimal number written the one way this crate writes numbers.
fn canonical_number(text: &str) -> Option<u64> {
    let number = text.parse::<u64>().ok()?;
    (number.to_string() == text).then_some(number)
}

/// The names in `dir` that end with `suffix`, with that suffix removed.
fn names_with_suffix(dir: &Path, suffix: &str) -> Result<Vec<(String, PathBuf)>, FamilyError> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(FamilyError::io(dir))? {
        let entry = entry.map_err(FamilyError::io(dir))?;
        if let Some(stem) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(suffix))
        {
            found.push((stem.to_string(), entry.path()));
        }
    }
    Ok(found)
}
