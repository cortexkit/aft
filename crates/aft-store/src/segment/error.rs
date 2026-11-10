//! Errors from writing and reading segments.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use super::format::SectionId;
use crate::ids::FamilyId;

/// A failure to write or read one segment file, naming the file.
#[derive(Debug)]
pub struct SegmentError {
    path: PathBuf,
    kind: ErrorKind,
}

impl SegmentError {
    pub(crate) fn new(path: &Path, kind: ErrorKind) -> Self {
        Self {
            path: path.to_path_buf(),
            kind,
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// True when the file's bytes failed a check: it was torn, truncated or
    /// changed after it was written. Callers count these and rebuild the
    /// affected data.
    #[must_use]
    pub fn is_corruption(&self) -> bool {
        matches!(self.kind, ErrorKind::Corrupt(_) | ErrorKind::Malformed(_))
    }
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.kind)
    }
}

impl std::error::Error for SegmentError {}

/// What went wrong with a segment.
#[derive(Debug, thiserror::Error)]
pub enum ErrorKind {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("not a segment file")]
    NotASegment,
    #[error(
        "segment format version {found} is not supported; this build reads version {supported}"
    )]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("segment belongs to family {found}, expected {expected}")]
    FamilyMismatch { found: FamilyId, expected: FamilyId },
    #[error("{0} failed its checksum")]
    Corrupt(Region),
    #[error("malformed segment: {0}")]
    Malformed(&'static str),
    #[error("required section {id} version {version} is not understood by this build")]
    UnknownRequiredSection { id: SectionId, version: u16 },
    #[error("section {id} has no bytes at {end} (its length is {len})")]
    OutOfRange { id: SectionId, end: u64, len: u64 },
    #[error("segment would exceed {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("section {0} was written twice")]
    DuplicateSection(SectionId),
    #[error("section {0} was left unfinished")]
    UnfinishedSection(SectionId),
}

/// The part of a segment whose checksum failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Region {
    Header,
    SectionTable,
    ChecksumTable,
    /// One 64 KiB block of a section.
    Block {
        section: SectionId,
        block: u64,
    },
    /// A section's bytes as a whole.
    Section(SectionId),
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => f.write_str("the header"),
            Self::SectionTable => f.write_str("the section table"),
            Self::ChecksumTable => f.write_str("the checksum table"),
            Self::Block { section, block } => write!(f, "block {block} of section {section}"),
            Self::Section(section) => write!(f, "section {section}"),
        }
    }
}
