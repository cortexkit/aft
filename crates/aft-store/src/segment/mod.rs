//! The segment container: one immutable file holding sections of any plane.
//!
//! A segment file has this shape. All integers are little-endian, all
//! references are byte offsets from the start of the file, and the bytes are
//! the same on every operating system.
//!
//! ```text
//! header          128 bytes: magic, format version, family, segment id,
//!                 writer incarnation, creation time, section count, the
//!                 offsets and digests of the two regions below, and a
//!                 digest of the header itself
//! sections        each section's bytes, starting on an 8-byte boundary, or
//!                 a 64-byte boundary for arrays meant for vector scans
//! checksums       a digest of every 64 KiB block of every section, grouped
//!                 by section
//! section table   one 64-byte row per section: kind, key space, version,
//!                 flags, offset, length, item count, where its block
//!                 digests are, and a digest of its whole bytes
//! ```
//!
//! A reader verifies the header, the table and the checksum region when it
//! opens a segment, and each block's digest the first time the block is read
//! into the [`BlockCache`](crate::BlockCache). Any failed check makes the read
//! fail with the cause; nothing is guessed or silently skipped.
//!
//! Sections are opaque to the container. Each plane gives its sections a kind
//! and a version; a reader passes the kinds and versions it understands, and a
//! segment with a required section outside that list cannot be opened. A
//! build that adds or changes a required section raises [`FORMAT_VERSION`]
//! as well, so an older build refuses such a segment by its version rather
//! than finding it unusable.

mod error;
mod format;
mod reader;
mod writer;

pub use error::{ErrorKind, Region, SegmentError};
pub use format::{
    Alignment, BLOCK_LEN, FORMAT_VERSION, KeySpace, MAX_SEGMENT_LEN, Requirement, SectionId,
    SectionKind,
};
pub use reader::{KnownSection, SectionHandle, SectionInfo, Segment};
pub use writer::{SectionSpec, SegmentMeta, SegmentSummary, SegmentWriter};

/// The refusal [`Segment::open`] would give the file at `path` because a
/// newer build wrote it in a format version above [`FORMAT_VERSION`], read
/// from the header's magic and version alone. `None` for any other file.
pub(crate) fn newer_version(path: &std::path::Path) -> Option<SegmentError> {
    format::Header::newer_version(path)
}

#[cfg(test)]
mod tests;
