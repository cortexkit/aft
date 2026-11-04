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
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use super::{ContentRecord, DecodeError, RecordKey};
use crate::ids::Digest128;
use crate::le::{u32_at, u64_at};
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
const BODY_INDEX_LEN: usize = 17;

/// A body is stored compressed only when that saves at least this share.
const MIN_SAVING_PERCENT: usize = 10;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Codec {
    Raw = 0,
    Zlib = 1,
}

/// Collects records for the content sections of one segment.
#[derive(Debug, Default)]
pub struct ContentSectionsBuilder {
    records: BTreeMap<RecordKey, (Vec<u8>, Vec<u8>)>,
}

impl ContentSectionsBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a record under `key`. Adding a key twice keeps the first.
    pub fn add(&mut self, key: RecordKey, record: &ContentRecord) {
        self.records.entry(key).or_insert_with(|| record.encode());
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Writes the four sections into `writer`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub fn write(self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        let mut surfaces = Vec::new();
        let mut bodies = Vec::new();
        let mut index = Vec::with_capacity(self.records.len() * BODY_INDEX_LEN);
        for (key, (surface, body)) in self.records {
            table.push(
                key.digest().as_bytes(),
                UnitLocation {
                    offset: surfaces.len() as u64,
                    len: u32::try_from(surface.len()).expect("a surface is under 4 GiB"),
                },
            );
            surfaces.extend_from_slice(&surface);
            let (codec, stored) = pack(&body);
            index.extend_from_slice(&(bodies.len() as u64).to_le_bytes());
            index.extend_from_slice(
                &u32::try_from(stored.len())
                    .expect("a body is under 4 GiB")
                    .to_le_bytes(),
            );
            index.extend_from_slice(
                &u32::try_from(body.len())
                    .expect("a body is under 4 GiB")
                    .to_le_bytes(),
            );
            index.push(codec as u8);
            bodies.extend_from_slice(&stored);
        }
        let (table, units) = table.finish();
        writer.write_section(spec(UNITS), &table, units)?;
        writer.write_section(spec(SURFACES), &surfaces, units)?;
        writer.write_section(spec(BODIES), &bodies, units)?;
        writer.write_section(spec(BODY_INDEX), &index, units)
    }
}

fn pack(body: &[u8]) -> (Codec, Vec<u8>) {
    if body.is_empty() {
        return (Codec::Raw, Vec::new());
    }
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(body.len() / 3), Compression::fast());
    encoder
        .write_all(body)
        .expect("writing into a Vec cannot fail");
    let packed = encoder.finish().expect("writing into a Vec cannot fail");
    if packed.len() * 100 <= body.len() * (100 - MIN_SAVING_PERCENT) {
        (Codec::Zlib, packed)
    } else {
        (Codec::Raw, body.to_vec())
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
        self.segment.read_range(
            self.surfaces,
            location.offset..location.offset + u64::from(location.len),
        )
    }

    /// The body bytes of the record at `ordinal`, unpacked.
    ///
    /// # Errors
    ///
    /// When a read fails, a block fails its checksum, or the stored body
    /// does not unpack to its recorded length.
    pub fn body(&self, ordinal: u32) -> Result<Vec<u8>, SegmentError> {
        let mut entry = [0; BODY_INDEX_LEN];
        self.segment.read(
            self.index,
            u64::from(ordinal) * BODY_INDEX_LEN as u64,
            &mut entry,
        )?;
        let offset = u64_at(&entry, 0);
        let stored = u32_at(&entry, 8);
        let unpacked = u32_at(&entry, 12);
        let bytes = self
            .segment
            .read_range(self.bodies, offset..offset + u64::from(stored))?;
        match entry[16] {
            0 if stored == unpacked => Ok(bytes),
            1 => {
                let mut out = Vec::with_capacity(unpacked as usize);
                ZlibDecoder::new(bytes.as_slice())
                    .take(u64::from(unpacked) + 1)
                    .read_to_end(&mut out)
                    .map_err(|_| malformed(self.segment, "a record body does not unpack"))?;
                if out.len() == unpacked as usize {
                    Ok(out)
                } else {
                    Err(malformed(
                        self.segment,
                        "a record body unpacks to the wrong length",
                    ))
                }
            }
            _ => Err(malformed(self.segment, "unknown record body codec")),
        }
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
