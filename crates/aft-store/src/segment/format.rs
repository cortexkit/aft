//! Byte layout of the segment header and section table rows.

use std::fmt;

use super::error::{ErrorKind, SegmentError};
use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
use crate::le::{array, u16_at, u32_at, u64_at};

pub(crate) const MAGIC: [u8; 8] = *b"AFTSEG\0\0";

/// Version of the segment format: the container layout and the required
/// sections it may hold.
///
/// It is the one number that tells an older build a segment is newer than
/// it reads, so a build that adds a required section kind, or changes the
/// version of a required section, must raise it along with the container
/// layout. An older build then refuses the segment by this version
/// ([`crate::family::FamilyError::NewerFormat`]) and leaves it as it is. A
/// required section it did not understand would instead make the segment
/// unusable, and an unusable segment is built over and reclaimed. The test
/// `the_segment_format_version_moves_with_the_required_sections` pins the
/// required section kinds and versions together with this number, and fails
/// when one changes without the other. Optional sections, which an older
/// build skips, need no bump.
pub const FORMAT_VERSION: u16 = 1;

pub(crate) const HEADER_LEN: usize = 128;
/// [`HEADER_LEN`] as the header records it.
const HEADER_LEN_FIELD: u16 = 128;
const _: () = assert!(HEADER_LEN_FIELD as usize == HEADER_LEN);
pub(crate) const ROW_LEN: usize = 64;

/// Checksum granularity. Each block of a section's bytes has its own digest,
/// verified the first time the block is read, so a read never has to verify
/// more than it uses.
pub const BLOCK_LEN: usize = 64 * 1024;

/// The largest segment a writer produces and a reader accepts. Small segments
/// are merged in the background, so no segment needs to be larger.
pub const MAX_SEGMENT_LEN: u64 = 256 * 1024 * 1024;

/// More sections than any plane writes; a table claiming more is corrupt.
pub(crate) const MAX_SECTIONS: u32 = 4096;

const FLAG_REQUIRED: u8 = 1;
const FLAG_VECTOR_ALIGNED: u8 = 2;
const KNOWN_FLAGS: u8 = FLAG_REQUIRED | FLAG_VECTOR_ALIGNED;

/// The only compression id in format version 1. Planes that compress do it
/// per record inside their sections, where a reader can decode one record
/// without the rest.
const COMPRESSION_NONE: u8 = 0;

/// What a section holds, as defined by the plane that writes it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SectionKind(u16);

impl SectionKind {
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// The key space a section indexes, such as trigrams or symbol names, or
/// [`KeySpace::NONE`] for sections that are not part of one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct KeySpace(u16);

impl KeySpace {
    pub const NONE: Self = Self(0);

    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// A section's identity within one segment: no two sections share it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SectionId {
    pub kind: SectionKind,
    pub key_space: KeySpace,
}

impl SectionId {
    #[must_use]
    pub const fn new(kind: SectionKind, key_space: KeySpace) -> Self {
        Self { kind, key_space }
    }
}

impl fmt::Display for SectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.0, self.key_space.0)
    }
}

/// Where a section starts in the file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alignment {
    /// An 8-byte boundary, enough for any integer field.
    Word,
    /// A 64-byte boundary, for arrays scanned with vector instructions.
    Vector,
}

impl Alignment {
    pub(crate) const fn bytes(self) -> u64 {
        match self {
            Self::Word => 8,
            Self::Vector => 64,
        }
    }
}

/// Whether a reader that does not understand a section may ignore it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Requirement {
    /// The segment cannot be used without this section.
    Required,
    /// Readers that do not understand the section skip it.
    Optional,
}

/// The decoded segment header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Header {
    pub(crate) family: FamilyId,
    pub(crate) segment: SegmentId,
    pub(crate) incarnation: WriterIncarnation,
    pub(crate) created_unix_ms: u64,
    pub(crate) section_count: u32,
    pub(crate) checksums_offset: u64,
    pub(crate) table_offset: u64,
    pub(crate) table_digest: Digest128,
    pub(crate) checksums_digest: Digest128,
}

impl Header {
    pub(crate) fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        out[0..8].copy_from_slice(&MAGIC);
        out[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[10..12].copy_from_slice(&HEADER_LEN_FIELD.to_le_bytes());
        // 12..16: flags, none defined.
        out[16..32].copy_from_slice(self.family.digest().as_bytes());
        out[32..40].copy_from_slice(&self.segment.get().to_le_bytes());
        out[40..48].copy_from_slice(&self.incarnation.get().to_le_bytes());
        out[48..56].copy_from_slice(&self.created_unix_ms.to_le_bytes());
        out[56..60].copy_from_slice(&self.section_count.to_le_bytes());
        // 60..64: reserved.
        out[64..72].copy_from_slice(&self.checksums_offset.to_le_bytes());
        out[72..80].copy_from_slice(&self.table_offset.to_le_bytes());
        out[80..96].copy_from_slice(self.table_digest.as_bytes());
        out[96..112].copy_from_slice(self.checksums_digest.as_bytes());
        let digest = Digest128::of(&out[..112]);
        out[112..128].copy_from_slice(digest.as_bytes());
        out
    }

    /// The refusal [`Self::decode`] gives a header naming a format version
    /// above [`FORMAT_VERSION`], which a newer build wrote, read from the
    /// file at `path` without reading anything past the version. `None` for
    /// any other file, or one that cannot be read: whoever opens it later
    /// reports why.
    pub(crate) fn newer_version(path: &std::path::Path) -> Option<SegmentError> {
        let file = crate::fs::open_read(path).ok()?;
        let mut bytes = [0; 10];
        crate::fs::read_exact_at(&file, &mut bytes, 0).ok()?;
        let found = u16_at(&bytes, 8);
        (bytes[0..8] == MAGIC && found > FORMAT_VERSION).then(|| {
            SegmentError::new(
                path,
                ErrorKind::UnsupportedVersion {
                    found,
                    supported: FORMAT_VERSION,
                },
            )
        })
    }

    /// Decodes and checks a header. The magic is checked first, so a file
    /// that is not a segment says so instead of failing a checksum. The
    /// version is checked before the checksum: a newer format version may
    /// lay out and check its header differently, so its file is refused by
    /// its version rather than called damaged.
    pub(crate) fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self, ErrorKind> {
        if bytes[0..8] != MAGIC {
            return Err(ErrorKind::NotASegment);
        }
        let version = u16_at(bytes, 8);
        if version != FORMAT_VERSION {
            return Err(ErrorKind::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        if Digest128::of(&bytes[..112]) != Digest128::from_bytes(array(bytes, 112)) {
            return Err(ErrorKind::Corrupt(super::Region::Header));
        }
        if u16_at(bytes, 10) != HEADER_LEN_FIELD || u32_at(bytes, 12) != 0 || u32_at(bytes, 60) != 0
        {
            return Err(ErrorKind::Malformed(
                "header fields this version does not define are set",
            ));
        }
        Ok(Self {
            family: FamilyId::from_digest(Digest128::from_bytes(array(bytes, 16))),
            segment: SegmentId::new(u64_at(bytes, 32)),
            incarnation: WriterIncarnation::new(u64_at(bytes, 40)),
            created_unix_ms: u64_at(bytes, 48),
            section_count: u32_at(bytes, 56),
            checksums_offset: u64_at(bytes, 64),
            table_offset: u64_at(bytes, 72),
            table_digest: Digest128::from_bytes(array(bytes, 80)),
            checksums_digest: Digest128::from_bytes(array(bytes, 96)),
        })
    }
}

/// One decoded section table row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Row {
    pub(crate) id: SectionId,
    pub(crate) version: u16,
    pub(crate) requirement: Requirement,
    pub(crate) alignment: Alignment,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) item_count: u64,
    pub(crate) checksums_offset: u64,
    pub(crate) digest: Digest128,
}

impl Row {
    pub(crate) fn encode(&self) -> [u8; ROW_LEN] {
        let mut out = [0; ROW_LEN];
        out[0..2].copy_from_slice(&self.id.kind.get().to_le_bytes());
        out[2..4].copy_from_slice(&self.id.key_space.get().to_le_bytes());
        out[4..6].copy_from_slice(&self.version.to_le_bytes());
        out[6] = COMPRESSION_NONE;
        let mut flags = 0;
        if self.requirement == Requirement::Required {
            flags |= FLAG_REQUIRED;
        }
        if self.alignment == Alignment::Vector {
            flags |= FLAG_VECTOR_ALIGNED;
        }
        out[7] = flags;
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.len.to_le_bytes());
        // The uncompressed length equals the stored length: format version 1
        // has no section compression. The field keeps room for it.
        out[24..32].copy_from_slice(&self.len.to_le_bytes());
        out[32..40].copy_from_slice(&self.item_count.to_le_bytes());
        out[40..48].copy_from_slice(&self.checksums_offset.to_le_bytes());
        out[48..64].copy_from_slice(self.digest.as_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8; ROW_LEN]) -> Result<Self, ErrorKind> {
        let flags = bytes[7];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(ErrorKind::Malformed(
                "a section has flags this version does not define",
            ));
        }
        let len = u64_at(bytes, 16);
        if bytes[6] != COMPRESSION_NONE || u64_at(bytes, 24) != len {
            return Err(ErrorKind::Malformed(
                "a section uses compression this version does not define",
            ));
        }
        Ok(Self {
            id: SectionId::new(
                SectionKind::new(u16_at(bytes, 0)),
                KeySpace::new(u16_at(bytes, 2)),
            ),
            version: u16_at(bytes, 4),
            requirement: if flags & FLAG_REQUIRED == 0 {
                Requirement::Optional
            } else {
                Requirement::Required
            },
            alignment: if flags & FLAG_VECTOR_ALIGNED == 0 {
                Alignment::Word
            } else {
                Alignment::Vector
            },
            offset: u64_at(bytes, 8),
            len,
            item_count: u64_at(bytes, 32),
            checksums_offset: u64_at(bytes, 40),
            digest: Digest128::from_bytes(array(bytes, 48)),
        })
    }

    /// Number of checksum blocks the section's bytes are split into.
    pub(crate) fn block_count(&self) -> u64 {
        self.len.div_ceil(BLOCK_LEN as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            family: FamilyId::from_digest(Digest128::of(b"family")),
            segment: SegmentId::new(42),
            incarnation: WriterIncarnation::new(7),
            created_unix_ms: 1_793_700_000_000,
            section_count: 3,
            checksums_offset: 4096,
            table_offset: 8192,
            table_digest: Digest128::of(b"table"),
            checksums_digest: Digest128::of(b"checksums"),
        }
    }

    #[test]
    fn header_round_trips() {
        let encoded = header().encode();
        assert_eq!(Header::decode(&encoded).unwrap(), header());
    }

    #[test]
    fn a_changed_header_byte_fails_its_checksum() {
        let mut encoded = header().encode();
        encoded[33] ^= 1;
        assert!(matches!(
            Header::decode(&encoded),
            Err(ErrorKind::Corrupt(super::super::Region::Header))
        ));
    }

    #[test]
    fn foreign_files_are_not_segments() {
        let mut encoded = header().encode();
        encoded[0] = b'S';
        assert!(matches!(
            Header::decode(&encoded),
            Err(ErrorKind::NotASegment)
        ));
    }

    #[test]
    fn row_round_trips() {
        let row = Row {
            id: SectionId::new(SectionKind::new(3), KeySpace::new(9)),
            version: 2,
            requirement: Requirement::Optional,
            alignment: Alignment::Vector,
            offset: 192,
            len: 70_000,
            item_count: 11,
            checksums_offset: 70_400,
            digest: Digest128::of(b"section"),
        };
        assert_eq!(Row::decode(&row.encode()).unwrap(), row);
        assert_eq!(row.block_count(), 2);
    }

    #[test]
    fn unknown_row_flags_are_refused() {
        let row = Row {
            id: SectionId::new(SectionKind::new(1), KeySpace::NONE),
            version: 1,
            requirement: Requirement::Required,
            alignment: Alignment::Word,
            offset: 128,
            len: 0,
            item_count: 0,
            checksums_offset: 128,
            digest: Digest128::of(b""),
        };
        let mut encoded = row.encode();
        encoded[7] |= 0x80;
        assert!(matches!(
            Row::decode(&encoded),
            Err(ErrorKind::Malformed(_))
        ));
    }
}
