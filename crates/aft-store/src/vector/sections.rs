//! The vector plane's sections in a segment.
//!
//! A segment's vector sections hold the vectors of one model and the chunk
//! maps that refer to them.
//!
//! | Section | Holds |
//! |---|---|
//! | fingerprint | the [`ModelIdentity`] every vector and chunk map here belongs to |
//! | vector units | a unit table: each embedded-text digest in ascending order, located at its f32 row; a vector's ordinal is its position |
//! | scan rows | per vector, by ordinal: scale, residual, norm and largest component residual (f32 each), then the int8 codes padded to 16 bytes; 64-byte aligned |
//! | f32 rows | per vector, by ordinal: the vector as the embedder returned it, read only to rescore; 64-byte aligned |
//! | chunk maps | a unit table: each (content, path) key in ascending order, located at its map |
//! | chunk map data | per map: the path, then each chunk's text digest and flags, in chunk order |
//!
//! A chunk map lists text digests, not ordinals, so a file whose chunks were
//! embedded earlier can name vectors that live in an older segment.

use std::collections::BTreeMap;

use super::VectorError;
use super::model::{ChunkMapKey, ModelIdentity, TextDigest};
use super::score::{ScanParams, quantize};
use crate::ids::{ContentKey, Digest128};
use crate::le::u32_at;
use crate::segment::{
    Alignment, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentWriter,
};
use crate::table::{UnitLocation, UnitTable, UnitTableBuilder};

const FINGERPRINT: SectionKind = SectionKind::new(0x0300);
const UNITS: SectionKind = SectionKind::new(0x0301);
const SCAN: SectionKind = SectionKind::new(0x0302);
const VECTORS: SectionKind = SectionKind::new(0x0303);
const CHUNK_MAPS: SectionKind = SectionKind::new(0x0304);
const CHUNK_MAP_DATA: SectionKind = SectionKind::new(0x0305);
const KINDS: [SectionKind; 6] = [
    FINGERPRINT,
    UNITS,
    SCAN,
    VECTORS,
    CHUNK_MAPS,
    CHUNK_MAP_DATA,
];
const VERSION: u16 = 1;
const KEY_SPACE: KeySpace = KeySpace::new(0);

/// Bytes of the per-vector values at the start of each scan row.
const SCAN_HEADER_LEN: usize = 16;
/// Bytes per chunk in a chunk map: its text digest and one flags byte.
const CHUNK_ENTRY_LEN: usize = Digest128::LEN + 1;
const FLAG_EXPORTED: u8 = 1;

/// Dimensions above this are refused, as today's index refuses them.
pub const MAX_DIMENSIONS: u32 = 8192;

/// The vector sections this build reads.
pub const KNOWN: [KnownSection; 6] = [
    KnownSection {
        kind: FINGERPRINT,
        version: VERSION,
    },
    KnownSection {
        kind: UNITS,
        version: VERSION,
    },
    KnownSection {
        kind: SCAN,
        version: VERSION,
    },
    KnownSection {
        kind: VECTORS,
        version: VERSION,
    },
    KnownSection {
        kind: CHUNK_MAPS,
        version: VERSION,
    },
    KnownSection {
        kind: CHUNK_MAP_DATA,
        version: VERSION,
    },
];

/// Bytes of one scan row for vectors of `dimensions`.
pub(crate) const fn scan_row_len(dimensions: usize) -> usize {
    SCAN_HEADER_LEN + dimensions.next_multiple_of(16)
}

/// Bytes of one segment's vector sections.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SectionBytes {
    /// The vector unit table.
    pub units: u64,
    /// The int8 scan rows every search reads.
    pub scan: u64,
    /// The f32 rows read only to rescore.
    pub vectors: u64,
    /// The chunk map table and data.
    pub chunk_maps: u64,
}

/// One chunk of a file, as its chunk map records it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chunk {
    /// The key of the text embedded for this chunk.
    pub text: TextDigest,
    /// Whether today's exported-chunk boost applies to its score.
    pub exported: bool,
}

/// Collects vectors and chunk maps for the vector sections of one segment.
#[derive(Debug)]
pub struct VectorSectionsBuilder {
    model: ModelIdentity,
    vectors: BTreeMap<TextDigest, Vec<f32>>,
    maps: BTreeMap<ChunkMapKey, (Vec<u8>, Vec<Chunk>)>,
}

impl VectorSectionsBuilder {
    /// A builder for vectors of `model`.
    ///
    /// # Errors
    ///
    /// When the model's dimension is 0 or above [`MAX_DIMENSIONS`].
    pub fn new(model: ModelIdentity) -> Result<Self, VectorError> {
        if model.dimensions == 0 || model.dimensions > MAX_DIMENSIONS {
            return Err(VectorError::UnsupportedDimensions(model.dimensions));
        }
        Ok(Self {
            model,
            vectors: BTreeMap::new(),
            maps: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    /// Bytes one vector of `dimensions` takes in a segment, codes and f32
    /// row together, so callers can keep segments under their size limit.
    #[must_use]
    pub const fn bytes_per_vector(dimensions: usize) -> usize {
        scan_row_len(dimensions) + dimensions * 4 + Digest128::LEN + 12
    }

    /// Adds the vector embedded for `text`. Adding a text twice keeps the
    /// first vector: the same model given the same bytes returns the same
    /// vector.
    ///
    /// # Errors
    ///
    /// When the vector has the wrong dimension, a component that is not
    /// finite, or a norm that overflows f32.
    pub fn add_vector(&mut self, text: TextDigest, vector: &[f32]) -> Result<(), VectorError> {
        if vector.len() != self.model.dimensions as usize {
            return Err(VectorError::DimensionMismatch {
                expected: self.model.dimensions,
                found: vector.len(),
            });
        }
        if vector.iter().any(|value| !value.is_finite())
            || !super::score::vector_norm(vector).is_finite()
        {
            return Err(VectorError::NonFinite);
        }
        self.vectors.entry(text).or_insert_with(|| vector.to_vec());
        Ok(())
    }

    /// Records which texts the chunks of the file at `path` with `content`
    /// were embedded from, in chunk order. Adding a map twice keeps the
    /// first. The vectors may be in this segment or any other of the same
    /// model.
    pub fn add_chunk_map(&mut self, content: ContentKey, path: &[u8], chunks: &[Chunk]) {
        self.maps
            .entry(ChunkMapKey::new(content, path))
            .or_insert_with(|| (path.to_vec(), chunks.to_vec()));
    }

    /// How many vectors it holds.
    #[must_use]
    pub fn vectors(&self) -> usize {
        self.vectors.len()
    }

    /// How many chunk maps it holds.
    #[must_use]
    pub fn chunk_maps(&self) -> usize {
        self.maps.len()
    }

    /// Writes the six sections into `writer`.
    ///
    /// # Errors
    ///
    /// When a write fails or the segment would grow past its size limit.
    pub fn write(self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        let dimensions = self.model.dimensions as usize;
        let row_bytes = dimensions * 4;
        let row_len = scan_row_len(dimensions);
        let count = self.vectors.len() as u64;

        writer.write_section(spec(FINGERPRINT, Alignment::Word), &self.model.encode(), 1)?;

        let mut table = UnitTableBuilder::new(Digest128::LEN);
        let mut position = 0u64;
        let row_len_u32 = u32::try_from(row_bytes).expect("dimensions are limited to 8192");
        for text in self.vectors.keys() {
            table.push(
                text.digest().as_bytes(),
                UnitLocation {
                    offset: position,
                    len: row_len_u32,
                },
            );
            position += row_bytes as u64;
        }
        let (table, units) = table.finish();
        writer.write_section(spec(UNITS, Alignment::Word), &table, units)?;

        // Streamed one row at a time: neither section is ever whole in memory.
        writer.begin_section(spec(SCAN, Alignment::Vector))?;
        let mut row = Vec::with_capacity(row_len);
        let mut codes = Vec::with_capacity(dimensions);
        for vector in self.vectors.values() {
            codes.clear();
            let params = quantize(vector, &mut codes);
            row.clear();
            for value in [
                params.scale,
                params.residual,
                params.norm,
                params.max_residual,
            ] {
                row.extend_from_slice(&value.to_le_bytes());
            }
            row.extend_from_slice(&codes);
            row.resize(row_len, 0);
            writer.write(&row)?;
        }
        writer.end_section(count)?;

        writer.begin_section(spec(VECTORS, Alignment::Vector))?;
        let mut bytes = Vec::with_capacity(row_bytes);
        for vector in self.vectors.values() {
            bytes.clear();
            for value in vector {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            writer.write(&bytes)?;
        }
        writer.end_section(count)?;

        let mut table = UnitTableBuilder::new(ChunkMapKey::LEN);
        let mut data = Vec::new();
        for (key, (path, chunks)) in &self.maps {
            let start = data.len();
            let path_len = u32::try_from(path.len()).expect("a path is shorter than 4 GiB");
            data.extend_from_slice(&path_len.to_le_bytes());
            data.extend_from_slice(path);
            let chunk_count = u32::try_from(chunks.len()).expect("fewer than 2^32 chunks");
            data.extend_from_slice(&chunk_count.to_le_bytes());
            for chunk in chunks {
                data.extend_from_slice(chunk.text.digest().as_bytes());
                data.push(if chunk.exported { FLAG_EXPORTED } else { 0 });
            }
            table.push(
                &key.to_bytes(),
                UnitLocation {
                    offset: start as u64,
                    len: u32::try_from(data.len() - start).expect("a chunk map is below 4 GiB"),
                },
            );
        }
        let (table, maps) = table.finish();
        writer.write_section(spec(CHUNK_MAPS, Alignment::Word), &table, maps)?;
        writer.write_section(spec(CHUNK_MAP_DATA, Alignment::Word), &data, maps)
    }
}

fn spec(kind: SectionKind, alignment: Alignment) -> SectionSpec {
    SectionSpec {
        id: SectionId::new(kind, KEY_SPACE),
        version: VERSION,
        alignment,
        requirement: Requirement::Required,
    }
}

/// The vector sections of one open segment.
#[derive(Debug)]
pub struct VectorSegment<'s> {
    segment: &'s Segment,
    model: ModelIdentity,
    units: UnitTable<'s>,
    scan: SectionHandle,
    vectors: SectionHandle,
    maps: UnitTable<'s>,
    map_data: SectionHandle,
    dimensions: usize,
}

impl<'s> VectorSegment<'s> {
    /// The segment's vector sections, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// When the segment has only some of them, or they are malformed.
    pub fn open(segment: &'s Segment) -> Result<Option<Self>, SegmentError> {
        let handles = KINDS.map(|kind| segment.section(SectionId::new(kind, KEY_SPACE)));
        let [
            Some(fingerprint),
            Some(units),
            Some(scan),
            Some(vectors),
            Some(maps),
            Some(map_data),
        ] = handles
        else {
            if handles.iter().any(Option::is_some) {
                return Err(malformed(
                    segment,
                    "a segment has only some vector sections",
                ));
            }
            return Ok(None);
        };
        let info = segment.info(fingerprint);
        let len = usize::try_from(info.len)
            .ok()
            .filter(|len| *len <= 16 * 1024)
            .ok_or_else(|| malformed(segment, "the model fingerprint is too long"))?;
        let mut bytes = vec![0; len];
        segment.read(fingerprint, 0, &mut bytes)?;
        let model = ModelIdentity::decode(&bytes)
            .filter(|model| (1..=MAX_DIMENSIONS).contains(&model.dimensions))
            .ok_or_else(|| malformed(segment, "the model fingerprint does not decode"))?;
        let dimensions = model.dimensions as usize;
        let units = UnitTable::open(segment, units)?;
        let count = u64::from(units.len());
        let scan_info = segment.info(scan);
        let vectors_info = segment.info(vectors);
        if scan_info.len != count * scan_row_len(dimensions) as u64
            || scan_info.item_count != count
            || vectors_info.len != count * dimensions as u64 * 4
            || vectors_info.item_count != count
        {
            return Err(malformed(
                segment,
                "vector rows do not match the vector unit table",
            ));
        }
        let maps = UnitTable::open(segment, maps)?;
        if segment.info(map_data).item_count != u64::from(maps.len()) {
            return Err(malformed(
                segment,
                "chunk map data does not match the chunk map table",
            ));
        }
        Ok(Some(Self {
            segment,
            model,
            units,
            scan,
            vectors,
            maps,
            map_data,
            dimensions,
        }))
    }

    #[must_use]
    pub fn segment(&self) -> &'s Segment {
        self.segment
    }

    #[must_use]
    pub fn model(&self) -> &ModelIdentity {
        &self.model
    }

    #[must_use]
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// How many vectors the segment holds.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// How many chunk maps the segment holds.
    #[must_use]
    pub fn chunk_map_count(&self) -> u32 {
        self.maps.len()
    }

    /// Bytes of the segment's vector sections, for size reports.
    #[must_use]
    pub fn section_bytes(&self) -> SectionBytes {
        let len = |handle| self.segment.info(handle).len;
        let units = self
            .segment
            .section(SectionId::new(UNITS, KEY_SPACE))
            .map_or(0, len);
        let maps = self
            .segment
            .section(SectionId::new(CHUNK_MAPS, KEY_SPACE))
            .map_or(0, len);
        SectionBytes {
            units,
            scan: len(self.scan),
            vectors: len(self.vectors),
            chunk_maps: maps + len(self.map_data),
        }
    }

    /// The text digest of every vector, in ordinal order.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn texts(&self) -> Result<Vec<TextDigest>, SegmentError> {
        (0..self.units.len())
            .map(|ordinal| {
                let (key, _) = self.units.entry(ordinal)?;
                let bytes = <[u8; Digest128::LEN]>::try_from(key.as_slice())
                    .map_err(|_| self.malformed("a vector unit key is not a text digest"))?;
                Ok(TextDigest::from_digest(Digest128::from_bytes(bytes)))
            })
            .collect()
    }

    /// The ordinal of the vector embedded for `text`, if the segment has it.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn find(&self, text: TextDigest) -> Result<Option<u32>, SegmentError> {
        Ok(self
            .units
            .get(text.digest().as_bytes())?
            .map(|(ordinal, _)| ordinal))
    }

    /// The stored f32 vector at `ordinal`, exactly as it was written.
    ///
    /// # Errors
    ///
    /// When a read fails or a block fails its checksum.
    pub fn vector(&self, ordinal: u32) -> Result<Vec<f32>, SegmentError> {
        let mut bytes = vec![0; self.dimensions * 4];
        self.read_vector(ordinal, &mut bytes)?;
        Ok(f32s(&bytes))
    }

    /// Reads the f32 row at `ordinal` into `bytes`, which holds exactly one
    /// row.
    pub(crate) fn read_vector(&self, ordinal: u32, bytes: &mut [u8]) -> Result<(), SegmentError> {
        debug_assert_eq!(bytes.len(), self.dimensions * 4);
        self.segment.read(
            self.vectors,
            u64::from(ordinal) * (self.dimensions as u64 * 4),
            bytes,
        )
    }

    /// Reads consecutive f32 rows starting at `first` into `bytes`, whose
    /// length is a whole number of rows.
    pub(crate) fn read_vectors(&self, first: u32, bytes: &mut [u8]) -> Result<(), SegmentError> {
        self.segment.read(
            self.vectors,
            u64::from(first) * (self.dimensions as u64 * 4),
            bytes,
        )
    }

    /// Reads consecutive scan rows starting at `first` into `bytes`, whose
    /// length is a whole number of rows.
    pub(crate) fn read_scan_rows(&self, first: u32, bytes: &mut [u8]) -> Result<(), SegmentError> {
        self.segment.read(
            self.scan,
            u64::from(first) * scan_row_len(self.dimensions) as u64,
            bytes,
        )
    }

    /// The chunk map of the file at `path` with `content`, if the segment
    /// has one.
    ///
    /// # Errors
    ///
    /// When a read fails or the map is malformed.
    pub fn chunk_map(
        &self,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<Vec<Chunk>>, SegmentError> {
        let key = ChunkMapKey::new(content, path);
        let Some((_, location)) = self.maps.get(&key.to_bytes())? else {
            return Ok(None);
        };
        let bytes = self.segment.read_range(
            self.map_data,
            location.offset..location.offset + u64::from(location.len),
        )?;
        let (stored_path, chunks) = self.decode_map(&bytes)?;
        if stored_path != path {
            // Two paths with one digest: astronomically unlikely, and never
            // the other file's chunks.
            return Err(self.malformed("a chunk map is stored under another path"));
        }
        Ok(Some(chunks))
    }

    fn decode_map<'b>(&self, bytes: &'b [u8]) -> Result<(&'b [u8], Vec<Chunk>), SegmentError> {
        let fail = || self.malformed("a chunk map does not decode");
        if bytes.len() < 8 {
            return Err(fail());
        }
        let path_len = u32_at(bytes, 0) as usize;
        let count_at = 4usize.checked_add(path_len).ok_or_else(fail)?;
        if bytes.len() < count_at + 4 {
            return Err(fail());
        }
        let count = u32_at(bytes, count_at) as usize;
        let entries = &bytes[count_at + 4..];
        if Some(entries.len()) != count.checked_mul(CHUNK_ENTRY_LEN) {
            return Err(fail());
        }
        let chunks = entries
            .as_chunks::<CHUNK_ENTRY_LEN>()
            .0
            .iter()
            .map(|entry| {
                let flags = entry[Digest128::LEN];
                if flags & !FLAG_EXPORTED != 0 {
                    return Err(fail());
                }
                Ok(Chunk {
                    text: TextDigest::from_digest(Digest128::from_bytes(crate::le::array(
                        entry, 0,
                    ))),
                    exported: flags & FLAG_EXPORTED != 0,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok((&bytes[4..count_at], chunks))
    }

    fn malformed(&self, reason: &'static str) -> SegmentError {
        malformed(self.segment, reason)
    }
}

fn malformed(segment: &Segment, reason: &'static str) -> SegmentError {
    SegmentError::new(segment.path(), ErrorKind::Malformed(reason))
}

/// Decodes a scan row's per-vector values.
pub(crate) fn scan_params(row: &[u8]) -> ScanParams {
    let value = |at| f32::from_le_bytes(crate::le::array(row, at));
    ScanParams {
        scale: value(0),
        residual: value(4),
        norm: value(8),
        max_residual: value(12),
    }
}

/// The codes of a scan row.
pub(crate) fn scan_codes(row: &[u8], dimensions: usize) -> &[u8] {
    &row[SCAN_HEADER_LEN..SCAN_HEADER_LEN + dimensions]
}

/// Little-endian f32s from bytes whose length is a multiple of four.
pub(crate) fn f32s(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}
