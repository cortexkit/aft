//! A cold build's staging segment for content records.
//!
//! A build that extracts every file of a checkout appends each record here
//! as soon as it is extracted, instead of holding every record in memory
//! until the segment it publishes is written. Later stages read a record
//! back by its location, through the process's block cache, and the
//! published content sections are copied from here in key order
//! ([`write_staged`]).
//!
//! The staging segment is an ordinary segment file with one section, of a
//! kind no published segment holds. Each record is its surface part
//! followed by its body part, the body already packed as the published
//! body section stores it, so the copy into the published sections neither
//! packs nor unpacks anything. Where each record lies is kept by the
//! builder in memory ([`StagedUnit`]), not in the file.

use std::path::Path;
use std::sync::Arc;

use super::sections::{BODIES, BODY_INDEX, EncodedRecord, SURFACES, UNITS, spec as sections_spec};
use super::{ContentRecord, RecordKey};
use crate::cache::BlockCache;
use crate::ids::{Digest128, FamilyId};
use crate::packed::{self, StoredUnit};
use crate::segment::{
    Alignment, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentMeta, SegmentSummary, SegmentWriter,
};
use crate::table::{UnitLocation, UnitTableBuilder};

/// The one section of a staging segment. Never written into a published
/// segment.
const STAGED: SectionKind = SectionKind::new(0x02F0);
const STAGED_VERSION: u16 = 1;

fn staged_id() -> SectionId {
    SectionId::new(STAGED, KeySpace::new(0))
}

/// A record encoded and packed for the staging segment, built outside any
/// lock so extraction threads do the packing in parallel.
#[derive(Debug)]
pub struct PackedRecord {
    surface: Vec<u8>,
    body: StoredUnit,
}

impl PackedRecord {
    /// `record`'s surface part, and its body part packed as the published
    /// body section packs it.
    #[must_use]
    pub fn of(record: &ContentRecord) -> Self {
        let (surface, body) = record.encode();
        Self {
            surface,
            body: StoredUnit::packed(&body),
        }
    }

    /// The bytes it takes in the staging segment.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.surface.len() as u64 + self.body.parts().2.len() as u64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where one record lies in a staging segment, and how its body is packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedUnit {
    offset: u64,
    surface: u32,
    body: u32,
    unpacked: u32,
    codec: u8,
}

impl StagedUnit {
    /// The bytes [`Self::to_bytes`] writes.
    pub const BYTES: usize = 21;

    /// Size of the uncompressed record payload, without reading the segment.
    #[must_use]
    pub fn uncompressed_bytes(self) -> usize {
        (self.surface as usize).saturating_add(self.unpacked as usize)
    }

    /// The unit as fixed-width little-endian bytes, for a table kept on
    /// disk beside its staging segment so a later build can find each
    /// record again.
    #[must_use]
    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[0..8].copy_from_slice(&self.offset.to_le_bytes());
        out[8..12].copy_from_slice(&self.surface.to_le_bytes());
        out[12..16].copy_from_slice(&self.body.to_le_bytes());
        out[16..20].copy_from_slice(&self.unpacked.to_le_bytes());
        out[20] = self.codec;
        out
    }

    /// The unit [`Self::to_bytes`] wrote. Reading it back checks nothing:
    /// the codec and lengths are checked when the record is read.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; Self::BYTES]) -> Self {
        Self {
            offset: crate::le::u64_at(bytes, 0),
            surface: crate::le::u32_at(bytes, 8),
            body: crate::le::u32_at(bytes, 12),
            unpacked: crate::le::u32_at(bytes, 16),
            codec: bytes[20],
        }
    }
}

/// Appends records to a new staging segment.
#[derive(Debug)]
pub struct StageWriter {
    writer: SegmentWriter,
    len: u64,
    units: u64,
}

impl StageWriter {
    /// Creates the staging segment `path`, which must not exist.
    ///
    /// # Errors
    ///
    /// When the file cannot be created.
    pub fn create(path: &Path, family: FamilyId) -> Result<Self, SegmentError> {
        let created_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let mut writer = SegmentWriter::create(
            path,
            SegmentMeta {
                family,
                segment: crate::ids::SegmentId::new(0),
                incarnation: crate::ids::WriterIncarnation::new(0),
                created_unix_ms,
            },
        )?;
        writer.begin_section(SectionSpec {
            id: staged_id(),
            version: STAGED_VERSION,
            alignment: Alignment::Word,
            requirement: Requirement::Required,
        })?;
        Ok(Self {
            writer,
            len: 0,
            units: 0,
        })
    }

    /// The section bytes written so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units == 0
    }

    /// Appends `record`, returning where it lies.
    ///
    /// # Errors
    ///
    /// When the write fails, or a part is 4 GiB or larger.
    pub fn append(&mut self, record: &PackedRecord) -> Result<StagedUnit, SegmentError> {
        let (codec, unpacked, body) = record.body.parts();
        let too_large = || {
            SegmentError::new(
                Path::new("staging segment"),
                ErrorKind::Malformed("a staged record part is 4 GiB or larger"),
            )
        };
        let unit = StagedUnit {
            offset: self.len,
            surface: u32::try_from(record.surface.len()).map_err(|_| too_large())?,
            body: u32::try_from(body.len()).map_err(|_| too_large())?,
            unpacked,
            codec,
        };
        self.writer.write(&record.surface)?;
        self.writer.write(body)?;
        self.len += record.len();
        self.units += 1;
        Ok(unit)
    }

    /// Ends the section and writes the segment's tables and header.
    ///
    /// # Errors
    ///
    /// When a write or the flush fails.
    pub fn finish(mut self) -> Result<SegmentSummary, SegmentError> {
        self.writer.end_section(self.units)?;
        self.writer.finish()
    }
}

/// A finished staging segment, open for reading through a block cache.
#[derive(Debug)]
pub struct Stage {
    segment: Segment,
    section: SectionHandle,
}

impl Stage {
    /// Opens the staging segment `path` of `family`.
    ///
    /// # Errors
    ///
    /// When the file is not a finished staging segment of `family`.
    pub fn open(
        path: &Path,
        family: FamilyId,
        cache: Arc<BlockCache>,
    ) -> Result<Self, SegmentError> {
        let segment = Segment::open(
            path,
            family,
            &[KnownSection {
                kind: STAGED,
                version: STAGED_VERSION,
            }],
            cache,
        )?;
        let section = segment.section(staged_id()).ok_or_else(|| {
            SegmentError::new(path, ErrorKind::Malformed("not a staging segment"))
        })?;
        Ok(Self { segment, section })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.segment.path()
    }

    fn surface(&self, unit: StagedUnit) -> Result<Vec<u8>, SegmentError> {
        let end = unit.offset + u64::from(unit.surface);
        self.segment.read_range(self.section, unit.offset..end)
    }

    fn stored_body(&self, unit: StagedUnit) -> Result<StoredUnit, SegmentError> {
        if !packed::entry_is_valid(unit.codec, unit.body, unit.unpacked) {
            return Err(SegmentError::new(
                self.path(),
                ErrorKind::Malformed("a staged body has an unknown codec"),
            ));
        }
        let start = unit.offset + u64::from(unit.surface);
        let bytes = self
            .segment
            .read_range(self.section, start..start + u64::from(unit.body))?;
        Ok(StoredUnit::from_parts(unit.codec, unit.unpacked, bytes))
    }

    /// The record at `unit`, as the content sections encode it.
    ///
    /// # Errors
    ///
    /// When a read fails, a block fails its checksum, or the body does not
    /// unpack.
    pub fn encoded(&self, unit: StagedUnit) -> Result<EncodedRecord, SegmentError> {
        let surface = self.surface(unit)?;
        let body = packed::unpack(self.path(), self.stored_body(unit)?)?;
        Ok(EncodedRecord { surface, body })
    }
}

/// Writes into `writer` the content sections of a segment holding `units`,
/// each `(key, stage, unit)` the record at `unit` of the staging segment
/// `stages[stage]`: the same bytes, in the same section order, as a
/// [`super::ContentSectionsBuilder`] holding the same records writes, with
/// one record in memory at a time. `units` must be in strictly ascending key
/// order.
///
/// # Errors
///
/// When a read or a write fails.
///
/// # Panics
///
/// When `units` is not in strictly ascending key order or names a stage
/// that is not in `stages`.
pub fn write_staged(
    writer: &mut SegmentWriter,
    stages: &[Stage],
    units: &[(RecordKey, usize, StagedUnit)],
) -> Result<(), SegmentError> {
    assert!(
        units.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "staged units are written in strictly ascending key order"
    );
    // The unit table locates each surface by the surfaces written before
    // it, which the staged lengths give without reading anything.
    let mut table = UnitTableBuilder::new(Digest128::LEN);
    let mut offset = 0u64;
    for (key, _, unit) in units {
        table.push(
            key.digest().as_bytes(),
            UnitLocation {
                offset,
                len: unit.surface,
            },
        );
        offset += u64::from(unit.surface);
    }
    let (table, count) = table.finish();
    writer.write_section(sections_spec(UNITS), &table, count)?;
    drop(table);
    writer.begin_section(sections_spec(SURFACES))?;
    for (_, stage, unit) in units {
        writer.write(&stages[*stage].surface(*unit)?)?;
    }
    writer.end_section(count)?;
    writer.begin_section(sections_spec(BODIES))?;
    let mut bodies = packed::PackedCopy::with_capacity(units.len());
    for (_, stage, unit) in units {
        bodies.push(writer, &stages[*stage].stored_body(*unit)?)?;
    }
    writer.end_section(count)?;
    writer.write_section(sections_spec(BODY_INDEX), &bodies.finish(), count)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentSectionsBuilder;
    use crate::content::sections::tests::record;
    use crate::ids::{ContentKey, SegmentId, WriterIncarnation};
    use crate::segment::BLOCK_LEN;

    fn key(index: u32) -> RecordKey {
        RecordKey::new(
            ContentKey::of(format!("content {index}").as_bytes()),
            "rust",
            "v1",
        )
    }

    fn published(dir: &Path, name: &str, family: FamilyId) -> SegmentWriter {
        SegmentWriter::create(
            &dir.join(name),
            SegmentMeta {
                family,
                segment: SegmentId::new(1),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap()
    }

    /// Records staged across two staging segments, in extraction order,
    /// read back whole and copied into content sections byte for byte as a
    /// builder holding them writes them; a changed staging file fails its
    /// read.
    #[test]
    fn staged_records_read_back_and_write_the_builders_sections() {
        let dir = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(Digest128::of(b"family"));
        let cache = Arc::new(BlockCache::new(8 * BLOCK_LEN));
        let mut writers = [
            StageWriter::create(&dir.path().join("stage-1"), family).unwrap(),
            StageWriter::create(&dir.path().join("stage-2"), family).unwrap(),
        ];
        let mut units = Vec::new();
        let mut builder = ContentSectionsBuilder::new();
        // Extraction order is not key order.
        for index in (0..300).rev() {
            let stage = (index % 2) as usize;
            let unit = writers[stage]
                .append(&PackedRecord::of(&record(index)))
                .unwrap();
            units.push((key(index), stage, unit));
            builder.add(key(index), &record(index));
        }
        for writer in writers {
            writer.finish().unwrap();
        }
        let stages = ["stage-1", "stage-2"]
            .map(|name| Stage::open(&dir.path().join(name), family, Arc::clone(&cache)).unwrap());
        for (index, (_, stage, unit)) in (0..300).rev().zip(&units) {
            let encoded = stages[*stage].encoded(*unit).unwrap();
            assert_eq!(encoded.decode().unwrap(), record(index));
        }
        units.sort_by_key(|(key, _, _)| *key);
        let mut staged = published(dir.path(), "staged.seg", family);
        write_staged(&mut staged, &stages, &units).unwrap();
        staged.finish().unwrap();
        let mut built = published(dir.path(), "built.seg", family);
        builder.write(&mut built).unwrap();
        built.finish().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("staged.seg")).unwrap(),
            std::fs::read(dir.path().join("built.seg")).unwrap(),
            "the staged copy writes the builder's bytes"
        );

        drop(stages);
        let path = dir.path().join("stage-2");
        let mut bytes = std::fs::read(&path).unwrap();
        // Inside the section's bytes: the tables at the end are far smaller.
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();
        let stage = Stage::open(&path, family, Arc::new(BlockCache::new(8 * BLOCK_LEN))).unwrap();
        assert!(
            units
                .iter()
                .filter(|(_, stage, _)| *stage == 1)
                .any(|(_, _, unit)| stage.encoded(*unit).is_err()),
            "a changed block fails its checksum"
        );
    }
}
