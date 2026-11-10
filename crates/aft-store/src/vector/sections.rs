//! The vector plane's sections in a segment.
//!
//! A segment's vector sections hold the vectors of one model and the chunk
//! maps that refer to them.
//!
//! | Section | Holds |
//! |---|---|
//! | fingerprint | the [`ModelIdentity`] every vector and chunk map here belongs to |
//! | vector units | a unit table: each embedded-text digest in ascending order, located at its f32 row; a vector's ordinal is its position |
//! | scan center | the mean of the segment's vectors (f32 each); every vector's codes are relative to it |
//! | scan rows | per vector, by ordinal: scale, residual, norm and largest component residual (f32 each), then the int8 codes padded to 16 bytes; 64-byte aligned |
//! | f32 rows | per vector, by ordinal: the vector as the embedder returned it, read only to rescore; 64-byte aligned |
//! | chunk maps | a unit table: each (content, path) key in ascending order, located at its map |
//! | chunk map data | per map: the path, then each chunk's text digest and flags, in chunk order |
//! | chunk details index | per map, by the map's ordinal: where its details are and how they are packed |
//! | chunk details | per map: what a search shows for each chunk, and why chunking or embedding failed, each map's bytes compressed on their own |
//!
//! A chunk map lists text digests, not ordinals, so a file whose chunks were
//! embedded earlier can name vectors that live in an older segment. A search
//! reads only chunk maps to choose its members; it reads a map's details only
//! for the files its answer names.
//!
//! A map's details unpack to one record ([`FileDetails::encode`]): a string
//! table, then the file's failure (an optional string), the chunk count,
//! and per chunk its kind code, start and end line, name, optional qualified
//! name, snippet and optional failure, all as varints and string indexes.
//! A map added without details has an empty record.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::VectorError;
use super::model::{ChunkMapKey, ModelIdentity, TextDigest};
use super::score::{ScanParams, mean, quantize};
use crate::content::DecodeError;
use crate::content::codec::{Decoder, Encoder};
use crate::ids::{ContentKey, Digest128};
use crate::le::u32_at;
use crate::packed::{self, INDEX_ENTRY_LEN, PackedBuilder};
use crate::segment::{
    Alignment, BLOCK_LEN, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentWriter,
};
use crate::table::{UnitLocation, UnitTable, UnitTableBuilder, UnitTableParts};

// The vector plane's section kinds are 0x04xx; each plane has its own
// range (text 0x01xx, content records 0x02xx, call graph 0x03xx), so planes
// can share a segment.
const FINGERPRINT: SectionKind = SectionKind::new(0x0400);
const UNITS: SectionKind = SectionKind::new(0x0401);
const SCAN: SectionKind = SectionKind::new(0x0402);
const VECTORS: SectionKind = SectionKind::new(0x0403);
const CHUNK_MAPS: SectionKind = SectionKind::new(0x0404);
const CHUNK_MAP_DATA: SectionKind = SectionKind::new(0x0405);
const CENTER: SectionKind = SectionKind::new(0x0406);
const DETAILS_INDEX: SectionKind = SectionKind::new(0x0407);
const DETAILS: SectionKind = SectionKind::new(0x0408);
const KINDS: [SectionKind; 9] = [
    FINGERPRINT,
    UNITS,
    CENTER,
    SCAN,
    VECTORS,
    CHUNK_MAPS,
    CHUNK_MAP_DATA,
    DETAILS_INDEX,
    DETAILS,
];
const VERSION: u16 = 1;
const KEY_SPACE: KeySpace = KeySpace::new(0);

/// Bytes of the per-vector values at the start of each scan row.
const SCAN_HEADER_LEN: usize = 16;
/// Bytes per chunk in a chunk map: its text digest and one flags byte.
const CHUNK_ENTRY_LEN: usize = Digest128::LEN + 1;
const FLAG_EXPORTED: u8 = 1;
const FLAG_FAILED: u8 = 2;

/// Dimensions above this are refused, as today's index refuses them.
pub const MAX_DIMENSIONS: u32 = 8192;

/// The vector sections this build reads.
pub const KNOWN: [KnownSection; 9] = [
    KnownSection {
        kind: FINGERPRINT,
        version: VERSION,
    },
    KnownSection {
        kind: UNITS,
        version: VERSION,
    },
    KnownSection {
        kind: CENTER,
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
    KnownSection {
        kind: DETAILS_INDEX,
        version: VERSION,
    },
    KnownSection {
        kind: DETAILS,
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
    /// The chunk details index and their packed bytes.
    pub chunk_details: u64,
}

/// One chunk of a file, as its chunk map records it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chunk {
    /// The key of the text embedded for this chunk.
    pub text: TextDigest,
    /// Whether today's exported-chunk boost applies to its score.
    pub exported: bool,
    /// The embedder refused this chunk's text, so it has no vector and never
    /// will under this model; its details say why.
    pub failed: bool,
}

/// What a search shows for one chunk of a file, recorded by whoever cut the
/// file into chunks. The store keeps it as given.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ChunkDetail {
    /// The producer's code for the chunk's kind.
    pub kind: u8,
    pub start_line: u32,
    pub end_line: u32,
    pub name: String,
    pub qualified_name: Option<String>,
    pub snippet: String,
    /// Why the embedder refused the chunk's text; set exactly for a chunk
    /// whose map entry is [`Chunk::failed`].
    pub failure: Option<String>,
}

/// The details of one file's chunks, in chunk order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct FileDetails {
    /// Why the file could not be cut into chunks. Its chunk map is then
    /// empty: the file is accounted for, and has nothing to search.
    pub failure: Option<String>,
    pub chunks: Vec<ChunkDetail>,
}

impl FileDetails {
    /// The record a map's details are stored as, before packing.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut encoder = Encoder::default();
        encoder.opt_str(self.failure.as_deref());
        encoder.usize(self.chunks.len());
        for chunk in &self.chunks {
            encoder.byte(chunk.kind);
            encoder.u32(chunk.start_line);
            encoder.u32(chunk.end_line);
            encoder.str(&chunk.name);
            encoder.opt_str(chunk.qualified_name.as_deref());
            encoder.str(&chunk.snippet);
            encoder.opt_str(chunk.failure.as_deref());
        }
        encoder.finish()
    }

    /// Reads a record [`encode`](Self::encode) wrote.
    ///
    /// # Errors
    ///
    /// When the bytes are not such a record.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut decoder = Decoder::new(bytes)?;
        let failure = decoder.opt_string()?;
        // Each chunk takes at least its kind byte, two line varints and four
        // string indexes.
        let count = decoder.count_of(7)?;
        let mut chunks = Vec::with_capacity(count);
        for _ in 0..count {
            chunks.push(ChunkDetail {
                kind: decoder.byte()?,
                start_line: decoder.u32()?,
                end_line: decoder.u32()?,
                name: decoder.string()?,
                qualified_name: decoder.opt_string()?,
                snippet: decoder.string()?,
                failure: decoder.opt_string()?,
            });
        }
        decoder.finish()?;
        Ok(Self { failure, chunks })
    }
}

/// Collects vectors and chunk maps for the vector sections of one segment.
#[derive(Debug)]
pub struct VectorSectionsBuilder {
    model: ModelIdentity,
    vectors: BTreeMap<TextDigest, Vec<f32>>,
    maps: BTreeMap<ChunkMapKey, MapEntry>,
}

/// A chunk map being written: its path, its chunks and their details.
type MapEntry = (Vec<u8>, Vec<Chunk>, Option<FileDetails>);

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
            .or_insert_with(|| (path.to_vec(), chunks.to_vec(), None));
    }

    /// [`add_chunk_map`](Self::add_chunk_map), with what a search shows for
    /// each chunk and why any part failed.
    ///
    /// # Errors
    ///
    /// [`VectorError::DetailsMismatch`] when the details do not describe
    /// these chunks: another count, a failure on a chunk not flagged failed
    /// or none on one that is, or chunks beside a file failure.
    pub fn add_chunk_map_with_details(
        &mut self,
        content: ContentKey,
        path: &[u8],
        chunks: &[Chunk],
        details: FileDetails,
    ) -> Result<(), VectorError> {
        let matches = details.chunks.len() == chunks.len()
            && (details.failure.is_none() || chunks.is_empty())
            && chunks
                .iter()
                .zip(&details.chunks)
                .all(|(chunk, detail)| chunk.failed == detail.failure.is_some());
        if !matches {
            return Err(VectorError::DetailsMismatch);
        }
        self.maps
            .entry(ChunkMapKey::new(content, path))
            .or_insert_with(|| (path.to_vec(), chunks.to_vec(), Some(details)));
        Ok(())
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

    /// Writes the nine sections into `writer`.
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

        let center = mean(self.vectors.values().map(Vec::as_slice), dimensions);
        let center_bytes: Vec<u8> = center
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        writer.write_section(
            spec(CENTER, Alignment::Word),
            &center_bytes,
            dimensions as u64,
        )?;

        // Streamed one row at a time: neither section is ever whole in memory.
        writer.begin_section(spec(SCAN, Alignment::Vector))?;
        let mut row = Vec::with_capacity(row_len);
        let mut codes = Vec::with_capacity(dimensions);
        for vector in self.vectors.values() {
            codes.clear();
            let params = quantize(vector, &center, &mut codes);
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
        let mut details = PackedBuilder::with_capacity(self.maps.len());
        for (key, (path, chunks, file_details)) in &self.maps {
            details.push(
                &file_details
                    .as_ref()
                    .map_or_else(Vec::new, FileDetails::encode),
            );
            let start = data.len();
            let path_len = u32::try_from(path.len()).expect("a path is shorter than 4 GiB");
            data.extend_from_slice(&path_len.to_le_bytes());
            data.extend_from_slice(path);
            let chunk_count = u32::try_from(chunks.len()).expect("fewer than 2^32 chunks");
            data.extend_from_slice(&chunk_count.to_le_bytes());
            for chunk in chunks {
                data.extend_from_slice(chunk.text.digest().as_bytes());
                let mut flags = 0;
                if chunk.exported {
                    flags |= FLAG_EXPORTED;
                }
                if chunk.failed {
                    flags |= FLAG_FAILED;
                }
                data.push(flags);
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
        writer.write_section(spec(CHUNK_MAP_DATA, Alignment::Word), &data, maps)?;
        let (details, index) = details.finish();
        writer.write_section(spec(DETAILS_INDEX, Alignment::Word), &index, maps)?;
        writer.write_section(spec(DETAILS, Alignment::Word), &details, maps)
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
    /// Decoded once: every search needs all of it, and it is one vector.
    center: Arc<[f32]>,
    scan: SectionHandle,
    vectors: SectionHandle,
    maps: UnitTable<'s>,
    map_data: SectionHandle,
    details_index: SectionHandle,
    details: SectionHandle,
    dimensions: usize,
}

/// What opening a segment's vector sections read and checked, kept so the
/// same open segment can be read again without repeating any of it (see
/// [`VectorSegment::reopen`]).
#[derive(Clone, Debug)]
pub struct VectorSegmentParts {
    model: ModelIdentity,
    units: UnitTableParts,
    center: Arc<[f32]>,
    scan: SectionHandle,
    vectors: SectionHandle,
    maps: UnitTableParts,
    map_data: SectionHandle,
    details_index: SectionHandle,
    details: SectionHandle,
}

impl<'s> VectorSegment<'s> {
    /// The segment's vector sections, or `None` when it has none. Opening
    /// reads every scan row once, to check the values each search relies on.
    ///
    /// # Errors
    ///
    /// When the segment has only some of them, or they are malformed.
    pub fn open(segment: &'s Segment) -> Result<Option<Self>, SegmentError> {
        let handles = KINDS.map(|kind| segment.section(SectionId::new(kind, KEY_SPACE)));
        let [
            Some(fingerprint),
            Some(units),
            Some(center),
            Some(scan),
            Some(vectors),
            Some(maps),
            Some(map_data),
            Some(details_index),
            Some(details),
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
        let center_info = segment.info(center);
        if center_info.len != dimensions as u64 * 4 || center_info.item_count != dimensions as u64 {
            return Err(malformed(
                segment,
                "the scan center does not match the model",
            ));
        }
        let mut center_bytes = vec![0; dimensions * 4];
        segment.read(center, 0, &mut center_bytes)?;
        let center: Arc<[f32]> = f32s(&center_bytes).into();
        if center.iter().any(|value| !value.is_finite()) {
            return Err(malformed(segment, "the scan center is not finite"));
        }
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
        check_scan_rows(segment, scan, count, dimensions)?;
        let maps = UnitTable::open(segment, maps)?;
        if segment.info(map_data).item_count != u64::from(maps.len()) {
            return Err(malformed(
                segment,
                "chunk map data does not match the chunk map table",
            ));
        }
        let index_info = segment.info(details_index);
        if index_info.item_count != u64::from(maps.len())
            || index_info.len != u64::from(maps.len()) * INDEX_ENTRY_LEN as u64
            || segment.info(details).item_count != u64::from(maps.len())
        {
            return Err(malformed(
                segment,
                "chunk details do not match the chunk map table",
            ));
        }
        Ok(Some(Self {
            segment,
            model,
            units,
            center,
            scan,
            vectors,
            maps,
            map_data,
            details_index,
            details,
            dimensions,
        }))
    }

    /// What this open read and checked, to [`reopen`](Self::reopen) the same
    /// open segment later at no cost.
    #[must_use]
    pub fn parts(&self) -> VectorSegmentParts {
        VectorSegmentParts {
            model: self.model.clone(),
            units: self.units.parts(),
            center: Arc::clone(&self.center),
            scan: self.scan,
            vectors: self.vectors,
            maps: self.maps.parts(),
            map_data: self.map_data,
            details_index: self.details_index,
            details: self.details,
        }
    }

    /// The vector sections of `segment` as an earlier [`open`](Self::open)
    /// of this same open segment found them, without reading or checking
    /// anything again. `None` when `parts` came from another open segment,
    /// even one of the same file.
    #[must_use]
    pub fn reopen(segment: &'s Segment, parts: &VectorSegmentParts) -> Option<Self> {
        // A section handle names the open segment it came from, so this
        // compares instances, not files.
        if segment.section(SectionId::new(UNITS, KEY_SPACE)) != Some(parts.units.section) {
            return None;
        }
        Some(Self {
            segment,
            model: parts.model.clone(),
            units: UnitTable::attach(segment, parts.units),
            center: Arc::clone(&parts.center),
            scan: parts.scan,
            vectors: parts.vectors,
            maps: UnitTable::attach(segment, parts.maps),
            map_data: parts.map_data,
            details_index: parts.details_index,
            details: parts.details,
            dimensions: parts.model.dimensions as usize,
        })
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

    /// The mean vector the segment's codes are relative to.
    #[must_use]
    pub fn center(&self) -> &[f32] {
        &self.center
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
        let details_index = len(self.details_index);
        SectionBytes {
            units,
            scan: len(self.scan),
            vectors: len(self.vectors),
            chunk_maps: maps + len(self.map_data),
            chunk_details: details_index + len(self.details),
        }
    }

    /// The key of every chunk map, in ordinal order.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn chunk_map_keys(&self) -> Result<Vec<ChunkMapKey>, SegmentError> {
        (0..self.maps.len())
            .map(|ordinal| {
                let (key, _) = self.maps.entry(ordinal)?;
                ChunkMapKey::from_bytes(&key)
                    .ok_or_else(|| self.malformed("a chunk map key is not a content and path"))
            })
            .collect()
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
        Ok(self.locate_map(content, path)?.map(|(_, chunks)| chunks))
    }

    /// The details recorded with the chunk map of the file at `path` with
    /// `content`: `None` when the segment has no such map, or the map was
    /// added without details.
    ///
    /// # Errors
    ///
    /// When a read fails, or the details are malformed or do not describe
    /// the map's chunks.
    pub fn chunk_details(
        &self,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<FileDetails>, SegmentError> {
        let Some((ordinal, chunks)) = self.locate_map(content, path)? else {
            return Ok(None);
        };
        let bytes = packed::read(self.segment, self.details, self.details_index, ordinal)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let details = FileDetails::decode(&bytes)
            .map_err(|_| self.malformed("chunk details do not decode"))?;
        let matches = details.chunks.len() == chunks.len()
            && chunks
                .iter()
                .zip(&details.chunks)
                .all(|(chunk, detail)| chunk.failed == detail.failure.is_some());
        if !matches {
            return Err(self.malformed("chunk details do not match their chunk map"));
        }
        Ok(Some(details))
    }

    /// The ordinal and chunks of the map of the file at `path` with
    /// `content`, if the segment has one.
    fn locate_map(
        &self,
        content: ContentKey,
        path: &[u8],
    ) -> Result<Option<(u32, Vec<Chunk>)>, SegmentError> {
        let key = ChunkMapKey::new(content, path);
        let Some((ordinal, location)) = self.maps.get(&key.to_bytes())? else {
            return Ok(None);
        };
        let end = location
            .offset
            .checked_add(u64::from(location.len))
            .ok_or_else(|| self.malformed("a chunk map's range overflows"))?;
        let bytes = self
            .segment
            .read_range(self.map_data, location.offset..end)?;
        let (stored_path, chunks) = self.decode_map(&bytes)?;
        if stored_path != path {
            // Two paths with one digest: astronomically unlikely, and never
            // the other file's chunks.
            return Err(self.malformed("a chunk map is stored under another path"));
        }
        Ok(Some((ordinal, chunks)))
    }

    fn decode_map<'b>(&self, bytes: &'b [u8]) -> Result<(&'b [u8], Vec<Chunk>), SegmentError> {
        let fail = || self.malformed("a chunk map does not decode");
        if bytes.len() < 8 {
            return Err(fail());
        }
        let path_len = u32_at(bytes, 0) as usize;
        let count_at = 4usize.checked_add(path_len).ok_or_else(fail)?;
        let entries_at = count_at
            .checked_add(4)
            .filter(|at| *at <= bytes.len())
            .ok_or_else(fail)?;
        let count = u32_at(bytes, count_at) as usize;
        let entries = &bytes[entries_at..];
        if Some(entries.len()) != count.checked_mul(CHUNK_ENTRY_LEN) {
            return Err(fail());
        }
        let chunks = entries
            .as_chunks::<CHUNK_ENTRY_LEN>()
            .0
            .iter()
            .map(|entry| {
                let flags = entry[Digest128::LEN];
                if flags & !(FLAG_EXPORTED | FLAG_FAILED) != 0 {
                    return Err(fail());
                }
                Ok(Chunk {
                    text: TextDigest::from_digest(Digest128::from_bytes(crate::le::array(
                        entry, 0,
                    ))),
                    exported: flags & FLAG_EXPORTED != 0,
                    failed: flags & FLAG_FAILED != 0,
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

/// Refuses a segment any of whose `count` scan rows holds a value no writer
/// produces (see [`ScanParams::is_sound`]). Every search trusts those values
/// to bound its scores, so they are checked once when the segment is opened,
/// a block at a time.
fn check_scan_rows(
    segment: &Segment,
    scan: SectionHandle,
    count: u64,
    dimensions: usize,
) -> Result<(), SegmentError> {
    let row_len = scan_row_len(dimensions);
    let rows_per_read = (BLOCK_LEN / row_len).max(1);
    let mut rows = Vec::new();
    let mut first = 0u64;
    while first < count {
        let take = rows_per_read.min(usize::try_from(count - first).unwrap_or(usize::MAX));
        rows.resize(take * row_len, 0);
        segment.read(scan, first * row_len as u64, &mut rows)?;
        if rows
            .chunks_exact(row_len)
            .any(|row| !scan_params(row).is_sound())
        {
            return Err(malformed(
                segment,
                "a scan row holds a negative or non-finite value",
            ));
        }
        first += take as u64;
    }
    Ok(())
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

crate::heap_bytes_of_fields!(VectorSegmentParts {
    model,
    units,
    center,
    scan,
    vectors,
    maps,
    map_data,
    details_index,
    details,
});

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::cache::BlockCache;
    use crate::ids::{FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::SegmentMeta;

    const DIMENSIONS: u32 = 4;
    const PATH: &[u8] = b"src/a.rs";

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"forged vector sections"))
    }

    fn model() -> ModelIdentity {
        ModelIdentity {
            name: "test/forged".to_string(),
            revision: "r1".to_string(),
            dimensions: DIMENSIONS,
            normalized: true,
            text_template: 1,
            chunk_template: 1,
        }
    }

    fn content() -> ContentKey {
        ContentKey::of(b"fn a() {}")
    }

    fn le_bytes(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    /// Sound scan values for the forged vector: what a writer would store.
    const SOUND: [f32; 4] = [0.01, 0.001, 1.0, 0.0005];

    /// A segment holding one vector whose scan row carries `params` and one
    /// chunk map, for `PATH`, located at `map` (`None`: where it really is).
    /// Its checksums are valid, so only a buggy or hostile writer could have
    /// stored these values.
    fn write_forged(path: &Path, params: [f32; 4], map: Option<UnitLocation>) -> Segment {
        let dimensions = DIMENSIONS as usize;
        let mut writer = SegmentWriter::create(
            path,
            SegmentMeta {
                family: family(),
                segment: SegmentId::new(1),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap();
        writer
            .write_section(spec(FINGERPRINT, Alignment::Word), &model().encode(), 1)
            .unwrap();
        let mut units = UnitTableBuilder::new(Digest128::LEN);
        units.push(
            TextDigest::of(b"chunk").digest().as_bytes(),
            UnitLocation {
                offset: 0,
                len: DIMENSIONS * 4,
            },
        );
        let (units, count) = units.finish();
        writer
            .write_section(spec(UNITS, Alignment::Word), &units, count)
            .unwrap();
        writer
            .write_section(
                spec(CENTER, Alignment::Word),
                &le_bytes(&[0.0; DIMENSIONS as usize]),
                u64::from(DIMENSIONS),
            )
            .unwrap();
        let mut row = le_bytes(&params);
        row.resize(scan_row_len(dimensions), 0);
        writer
            .write_section(spec(SCAN, Alignment::Vector), &row, 1)
            .unwrap();
        writer
            .write_section(
                spec(VECTORS, Alignment::Vector),
                &le_bytes(&[1.0, 0.0, 0.0, 0.0]),
                1,
            )
            .unwrap();
        // The map: the path, then no chunks.
        let mut data = u32::try_from(PATH.len()).unwrap().to_le_bytes().to_vec();
        data.extend_from_slice(PATH);
        data.extend_from_slice(&0u32.to_le_bytes());
        let mut maps = UnitTableBuilder::new(ChunkMapKey::LEN);
        maps.push(
            &ChunkMapKey::new(content(), PATH).to_bytes(),
            map.unwrap_or(UnitLocation {
                offset: 0,
                len: u32::try_from(data.len()).unwrap(),
            }),
        );
        let (maps, count) = maps.finish();
        writer
            .write_section(spec(CHUNK_MAPS, Alignment::Word), &maps, count)
            .unwrap();
        writer
            .write_section(spec(CHUNK_MAP_DATA, Alignment::Word), &data, count)
            .unwrap();
        let mut details = PackedBuilder::with_capacity(1);
        details.push(&[]);
        let (details, index) = details.finish();
        writer
            .write_section(spec(DETAILS_INDEX, Alignment::Word), &index, count)
            .unwrap();
        writer
            .write_section(spec(DETAILS, Alignment::Word), &details, count)
            .unwrap();
        writer.finish().unwrap();
        Segment::open(
            path,
            family(),
            &KNOWN,
            Arc::new(BlockCache::new(4 * BLOCK_LEN)),
        )
        .unwrap()
    }

    #[test]
    fn a_scan_row_no_writer_produces_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let sound = write_forged(&dir.path().join("sound.seg"), SOUND, None);
        let opened = VectorSegment::open(&sound).unwrap().unwrap();
        assert_eq!(opened.chunk_map(content(), PATH).unwrap(), Some(Vec::new()));
        // Each value in turn made negative, then not finite. A negative
        // residual or norm would shrink the vector's score interval.
        let mut cases = Vec::new();
        for field in 0..4 {
            for bad in [-0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut params = SOUND;
                params[field] = bad;
                cases.push(params);
            }
        }
        for (index, params) in cases.into_iter().enumerate() {
            let segment = write_forged(&dir.path().join(format!("{index}.seg")), params, None);
            let error = VectorSegment::open(&segment).unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    ErrorKind::Malformed("a scan row holds a negative or non-finite value")
                ),
                "{params:?}: {error}"
            );
            assert!(error.is_corruption(), "{params:?}");
        }
    }

    #[test]
    fn a_chunk_map_range_past_u64_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let segment = write_forged(
            &dir.path().join("overflow.seg"),
            SOUND,
            Some(UnitLocation {
                offset: u64::MAX - 3,
                len: 16,
            }),
        );
        let opened = VectorSegment::open(&segment).unwrap().unwrap();
        let error = opened.chunk_map(content(), PATH).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                ErrorKind::Malformed("a chunk map's range overflows")
            ),
            "{error}"
        );
        assert!(error.is_corruption());
    }

    #[test]
    fn a_chunk_map_whose_path_runs_past_its_bytes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let segment = write_forged(&dir.path().join("short.seg"), SOUND, None);
        let opened = VectorSegment::open(&segment).unwrap().unwrap();
        // A path length that leaves no room for the chunk count, and one that
        // reaches the top of the address range.
        for path_len in [5u32, u32::MAX] {
            let mut bytes = path_len.to_le_bytes().to_vec();
            bytes.extend_from_slice(&[0; 8]);
            assert!(opened.decode_map(&bytes).unwrap_err().is_corruption());
        }
    }
}
