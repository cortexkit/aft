//! The content plane's sections in a segment.
//!
//! | Section | Holds |
//! |---|---|
//! | record units | a unit table: each record key in ascending order, located at its surface |
//! | record surfaces | each record's surface part, uncompressed, read in place |
//! | record bodies | each record's body part, compressed when that saves space |
//! | body index | per unit, by ordinal: body offset (u64), stored length (u32), length when unpacked (u32), codec (u8) |
//!
//! Surfaces stay uncompressed because resolving any file reads the surfaces
//! of the files it depends on, often many and often small; a body is read
//! only when its own file is resolved.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{ContentRecord, DecodeError, RecordKey};
use crate::ids::Digest128;
use crate::packed::{self, PackedBuilder};
use crate::segment::{
    Alignment, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentWriter,
};
use crate::table::{UnitLocation, UnitTable, UnitTableBuilder};

const UNITS: SectionKind = SectionKind::new(0x0200);
const SURFACES: SectionKind = SectionKind::new(0x0201);
const BODIES: SectionKind = SectionKind::new(0x0202);
const BODY_INDEX: SectionKind = SectionKind::new(0x0203);
const VERSION: u16 = 1;
const KEY_SPACE: KeySpace = KeySpace::new(0);
const BODY_INDEX_LEN: usize = packed::INDEX_ENTRY_LEN;

/// The content sections this build reads.
pub const KNOWN: [KnownSection; 4] = [
    KnownSection {
        kind: UNITS,
        version: VERSION,
    },
    KnownSection {
        kind: SURFACES,
        version: VERSION,
    },
    KnownSection {
        kind: BODIES,
        version: VERSION,
    },
    KnownSection {
        kind: BODY_INDEX,
        version: VERSION,
    },
];

/// A record's two parts as the sections store them, before the body is
/// compressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRecord {
    pub surface: Vec<u8>,
    pub body: Vec<u8>,
}

impl crate::heap::HeapBytes for EncodedRecord {
    fn heap_bytes(&self, _seen: &mut crate::heap::Seen) -> usize {
        self.surface.capacity() + self.body.capacity()
    }
}

impl EncodedRecord {
    #[must_use]
    pub fn of(record: &ContentRecord) -> Self {
        let (surface, body) = record.encode();
        Self { surface, body }
    }

    /// The record again.
    ///
    /// # Errors
    ///
    /// When the bytes are not a record this build encodes.
    pub fn decode(&self) -> Result<ContentRecord, DecodeError> {
        ContentRecord::decode(&self.surface, &self.body)
    }
}

/// Collects records for the content sections of one segment.
#[derive(Debug, Default)]
pub struct ContentSectionsBuilder {
    records: BTreeMap<RecordKey, Arc<EncodedRecord>>,
}

impl crate::heap::HeapBytes for ContentSectionsBuilder {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        let entry = std::mem::size_of::<RecordKey>() + std::mem::size_of::<Arc<EncodedRecord>>();
        crate::heap::btree_bytes(self.records.len(), entry)
            + self
                .records
                .values()
                .filter(|record| seen.first(Arc::as_ptr(record)))
                .map(|record| {
                    std::mem::size_of::<EncodedRecord>()
                        + record.surface.capacity()
                        + record.body.capacity()
                })
                .sum::<usize>()
    }
}

impl ContentSectionsBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a record under `key`. Adding a key twice keeps the first.
    pub fn add(&mut self, key: RecordKey, record: &ContentRecord) {
        self.records
            .entry(key)
            .or_insert_with(|| Arc::new(EncodedRecord::of(record)));
    }

    /// Adds a record already encoded. Adding a key twice keeps the first.
    pub fn add_encoded(&mut self, key: RecordKey, record: Arc<EncodedRecord>) {
        self.records.entry(key).or_insert(record);
    }

    /// The encoded record under `key`, shared.
    #[must_use]
    pub fn get(&self, key: &RecordKey) -> Option<&Arc<EncodedRecord>> {
        self.records.get(key)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Keeps only the records `keep` accepts: a build leaves out those
    /// another segment of the family already holds.
    pub fn retain(&mut self, mut keep: impl FnMut(RecordKey) -> bool) {
        self.records.retain(|key, _| keep(*key));
    }

    /// Writes the four sections into `writer`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub fn write(self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        let mut surfaces = Vec::new();
        let mut bodies = PackedBuilder::with_capacity(self.records.len());
        for (key, record) in self.records {
            table.push(
                key.digest().as_bytes(),
                UnitLocation {
                    offset: surfaces.len() as u64,
                    len: u32::try_from(record.surface.len()).expect("a surface is under 4 GiB"),
                },
            );
            surfaces.extend_from_slice(&record.surface);
            bodies.push(&record.body);
        }
        let (bodies, index) = bodies.finish();
        let (table, units) = table.finish();
        writer.write_section(spec(UNITS), &table, units)?;
        writer.write_section(spec(SURFACES), &surfaces, units)?;
        writer.write_section(spec(BODIES), &bodies, units)?;
        writer.write_section(spec(BODY_INDEX), &index, units)
    }
}

fn spec(kind: SectionKind) -> SectionSpec {
    SectionSpec {
        id: SectionId::new(kind, KEY_SPACE),
        version: VERSION,
        alignment: Alignment::Word,
        requirement: Requirement::Required,
    }
}

/// The content sections of one open segment.
#[derive(Debug)]
pub struct ContentSegment<'s> {
    segment: &'s Segment,
    units: UnitTable<'s>,
    surfaces: SectionHandle,
    bodies: SectionHandle,
    index: SectionHandle,
}

impl<'s> ContentSegment<'s> {
    /// The segment's content sections, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// When the segment has only some of them, or they are malformed.
    pub fn open(segment: &'s Segment) -> Result<Option<Self>, SegmentError> {
        let handles = [UNITS, SURFACES, BODIES, BODY_INDEX]
            .map(|kind| segment.section(SectionId::new(kind, KEY_SPACE)));
        let [Some(units), Some(surfaces), Some(bodies), Some(index)] = handles else {
            if handles.iter().any(Option::is_some) {
                return Err(malformed(
                    segment,
                    "a segment has only some content sections",
                ));
            }
            return Ok(None);
        };
        let units = UnitTable::open(segment, units)?;
        if segment.info(index).len != u64::from(units.len()) * BODY_INDEX_LEN as u64 {
            return Err(malformed(
                segment,
                "the body index does not match the unit table",
            ));
        }
        Ok(Some(Self {
            segment,
            units,
            surfaces,
            bodies,
            index,
        }))
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The ordinal of the record with `key`.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn find(&self, key: RecordKey) -> Result<Option<u32>, SegmentError> {
        Ok(self
            .units
            .get(key.digest().as_bytes())?
            .map(|(ordinal, _)| ordinal))
    }

    /// The record key at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn key(&self, ordinal: u32) -> Result<RecordKey, SegmentError> {
        let (key, _) = self.units.entry(ordinal)?;
        let bytes = <[u8; Digest128::LEN]>::try_from(key.as_slice())
            .map_err(|_| malformed(self.segment, "a record key has the wrong width"))?;
        Ok(RecordKey::from_digest(Digest128::from_bytes(bytes)))
    }

    /// The surface bytes of the record at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails or a block fails its checksum.
    pub fn surface(&self, ordinal: u32) -> Result<Vec<u8>, SegmentError> {
        let (_, location) = self.units.entry(ordinal)?;
        let end = location
            .offset
            .checked_add(u64::from(location.len))
            .ok_or_else(|| malformed(self.segment, "a surface's range overflows"))?;
        self.segment.read_range(self.surfaces, location.offset..end)
    }

    /// The body bytes of the record at `ordinal`, unpacked.
    ///
    /// # Errors
    ///
    /// When a read fails, a block fails its checksum, or the stored body
    /// does not unpack to its recorded length.
    pub fn body(&self, ordinal: u32) -> Result<Vec<u8>, SegmentError> {
        packed::read(self.segment, self.bodies, self.index, ordinal)
    }

    /// The whole record at `ordinal`.
    ///
    /// # Errors
    ///
    /// As [`surface`](Self::surface) and [`body`](Self::body), and when the
    /// parts do not decode.
    pub fn record(&self, ordinal: u32) -> Result<ContentRecord, ContentReadError> {
        let surface = self.surface(ordinal)?;
        let body = self.body(ordinal)?;
        Ok(ContentRecord::decode(&surface, &body)?)
    }
}

/// Why a stored record could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ContentReadError {
    #[error(transparent)]
    Segment(#[from] SegmentError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

fn malformed(segment: &Segment, reason: &'static str) -> SegmentError {
    SegmentError::new(segment.path(), ErrorKind::Malformed(reason))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cache::BlockCache;
    use crate::content::{Body, RecordBody, RefKind, Reference, Surface};
    use crate::ids::{ContentKey, FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::{BLOCK_LEN, SegmentMeta};

    fn record(index: u32) -> ContentRecord {
        let reference = Reference {
            ordinal: index,
            kind: RefKind::Call,
            caller_symbol: Some(format!("caller_{index}")),
            short_name: Some("target".into()),
            full_ref: Some(format!("module::target_{}", index % 7)),
            module_path: None,
            line: index,
            byte_start: 10,
            byte_end: 20,
            path_override: None,
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard: false,
            import_kind: None,
        };
        ContentRecord {
            language: "rust".into(),
            extractor_version: "v1".into(),
            data: RecordBody::Parsed {
                surface: Box::new(Surface {
                    exported_symbols: vec![format!("item_{index}")],
                    ..Surface::default()
                }),
                // Bodies of all sizes: some compress, some do not.
                body: Box::new(Body {
                    references: vec![reference; index as usize % 40],
                    ..Body::default()
                }),
            },
        }
    }

    #[test]
    fn records_are_found_by_key_and_read_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(Digest128::of(b"family"));
        let path = dir.path().join("1.seg");
        let mut writer = SegmentWriter::create(
            &path,
            SegmentMeta {
                family,
                segment: SegmentId::new(1),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap();
        let key = |index: u32| {
            RecordKey::new(
                ContentKey::of(format!("content {index}").as_bytes()),
                "rust",
                "v1",
            )
        };
        let mut builder = ContentSectionsBuilder::new();
        for index in 0..300 {
            builder.add(key(index), &record(index));
        }
        builder.write(&mut writer).unwrap();
        writer.finish().unwrap();
        let segment = Segment::open(
            &path,
            family,
            &KNOWN,
            Arc::new(BlockCache::new(8 * BLOCK_LEN)),
        )
        .unwrap();
        let content = ContentSegment::open(&segment).unwrap().unwrap();
        assert_eq!(content.len(), 300);
        for index in 0..300 {
            let ordinal = content.find(key(index)).unwrap().unwrap();
            assert_eq!(content.key(ordinal).unwrap(), key(index));
            assert_eq!(content.record(ordinal).unwrap(), record(index));
        }
        assert_eq!(
            content
                .find(RecordKey::new(ContentKey::of(b"content 1"), "rust", "v2"))
                .unwrap(),
            None,
            "another extractor version is another record"
        );
    }
}
