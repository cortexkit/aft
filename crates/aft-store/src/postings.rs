//! Posting lists: for one term, the ordinals of the units that contain it in
//! ascending order, each entry carrying the same fixed set of payload values.
//!
//! A list is stored as blocks of up to [`BLOCK_ENTRIES`] entries. Each block
//! starts with its first and last ordinal, so a reader can skip a block that
//! cannot hold a wanted ordinal without decoding it. Ordinals are stored as
//! bit-packed deltas; payload columns follow as fixed-width arrays, one value
//! per entry, so a reader decodes only what it needs from each block.
//!
//! ```text
//! block   first u32, last u32, entries u16, delta bits u8, reserved u8,
//!         (entries - 1) deltas at `delta bits` each, LSB first, byte-padded,
//!         then each payload column: `entries` values at its width
//! ```
//!
//! Ordinals may repeat: a token posting names a content and one symbol in it,
//! so a content with three matching symbols has three entries.

use crate::le::{u16_at, u32_at};
use crate::segment::{ErrorKind, SectionHandle, Segment, SegmentError};

/// Entries per block. Large enough that block headers are a small share of a
/// list, small enough that skipping by block is precise.
pub const BLOCK_ENTRIES: usize = 128;

const BLOCK_HEADER_LEN: usize = 12;
const MAX_COLUMNS: usize = 8;

/// The byte width of one payload column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Width {
    U8,
    U16,
    U32,
}

impl Width {
    const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }

    const fn max(self) -> u32 {
        match self {
            Self::U8 => u8::MAX as u32,
            Self::U16 => u16::MAX as u32,
            Self::U32 => u32::MAX,
        }
    }
}

/// The payload columns every posting of one key space carries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PostingLayout {
    columns: Vec<Width>,
}

impl PostingLayout {
    /// # Panics
    ///
    /// With more than eight columns; no key space needs that many.
    #[must_use]
    pub fn new(columns: &[Width]) -> Self {
        assert!(
            columns.len() <= MAX_COLUMNS,
            "{} payload columns is more than {MAX_COLUMNS}",
            columns.len()
        );
        Self {
            columns: columns.to_vec(),
        }
    }

    #[must_use]
    pub fn columns(&self) -> &[Width] {
        &self.columns
    }

    fn payload_len(&self, entries: usize) -> usize {
        self.columns
            .iter()
            .map(|width| width.bytes() * entries)
            .sum()
    }
}

/// Where a posting list starts in its section and how many entries it has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PostingList {
    pub offset: u64,
    pub count: u32,
}

/// Writes posting lists one after another into one section's bytes.
#[derive(Debug)]
pub struct PostingsBuilder {
    layout: PostingLayout,
    bytes: Vec<u8>,
    /// Bytes already handed over by [`Self::drain`]: the section offset at
    /// which `bytes` starts.
    drained: u64,
    entries: u64,
}

impl PostingsBuilder {
    #[must_use]
    pub fn new(layout: PostingLayout) -> Self {
        Self {
            layout,
            bytes: Vec::new(),
            drained: 0,
            entries: 0,
        }
    }

    /// Appends one list: `ordinals` in non-decreasing order, and one slice per
    /// payload column holding a value for every ordinal.
    ///
    /// # Panics
    ///
    /// When ordinals decrease, a column has the wrong length or count, or a
    /// value does not fit its column's width. Producers build these lists, so
    /// any of those is a bug.
    pub fn add_list(&mut self, ordinals: &[u32], columns: &[&[u32]]) -> PostingList {
        assert_eq!(
            columns.len(),
            self.layout.columns.len(),
            "posting list has the wrong number of payload columns"
        );
        for (column, width) in columns.iter().zip(&self.layout.columns) {
            assert_eq!(
                column.len(),
                ordinals.len(),
                "payload column length differs from ordinals"
            );
            assert!(
                column.iter().all(|value| *value <= width.max()),
                "a payload value does not fit its {width:?} column"
            );
        }
        assert!(
            ordinals.windows(2).all(|pair| pair[0] <= pair[1]),
            "posting ordinals must not decrease"
        );
        let list = PostingList {
            offset: self.drained + self.bytes.len() as u64,
            count: u32::try_from(ordinals.len())
                .expect("a posting list has fewer than 2^32 entries"),
        };
        for start in (0..ordinals.len()).step_by(BLOCK_ENTRIES) {
            let end = (start + BLOCK_ENTRIES).min(ordinals.len());
            let block_columns: Vec<&[u32]> =
                columns.iter().map(|column| &column[start..end]).collect();
            encode_block(
                &mut self.bytes,
                &self.layout,
                &ordinals[start..end],
                &block_columns,
            );
        }
        self.entries += ordinals.len() as u64;
        list
    }

    /// How many bytes are held, not yet handed over by [`Self::drain`].
    #[must_use]
    pub fn held(&self) -> usize {
        self.bytes.len()
    }

    /// Hands over the bytes written so far, so a caller that writes the
    /// section as it goes holds no more than the lists since the last call.
    /// Later lists' offsets still count from the section's start.
    pub fn drain(&mut self) -> Vec<u8> {
        self.drained += self.bytes.len() as u64;
        std::mem::take(&mut self.bytes)
    }

    /// The section bytes not yet drained and its item count: the total
    /// number of postings.
    #[must_use]
    pub fn finish(self) -> (Vec<u8>, u64) {
        (self.bytes, self.entries)
    }
}

fn encode_block(out: &mut Vec<u8>, layout: &PostingLayout, ordinals: &[u32], columns: &[&[u32]]) {
    let first = ordinals[0];
    let last = ordinals[ordinals.len() - 1];
    let deltas: Vec<u32> = ordinals.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let bits = deltas
        .iter()
        .map(|delta| 32 - delta.leading_zeros())
        .max()
        .unwrap_or(0);
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(&last.to_le_bytes());
    out.extend_from_slice(
        &u16::try_from(ordinals.len())
            .expect("a block holds at most 128 entries")
            .to_le_bytes(),
    );
    out.push(u8::try_from(bits).expect("delta bits are at most 32"));
    out.push(0);
    pack(out, &deltas, bits);
    for (column, width) in columns.iter().zip(&layout.columns) {
        for value in *column {
            out.extend_from_slice(&value.to_le_bytes()[..width.bytes()]);
        }
    }
}

fn pack(out: &mut Vec<u8>, values: &[u32], bits: u32) {
    let mut accumulator: u64 = 0;
    let mut filled = 0;
    for value in values {
        accumulator |= u64::from(*value) << filled;
        filled += bits;
        while filled >= 8 {
            out.push(accumulator.to_le_bytes()[0]);
            accumulator >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        out.push(accumulator.to_le_bytes()[0]);
    }
}

fn packed_len(values: usize, bits: u32) -> usize {
    (values * bits as usize).div_ceil(8)
}

/// A block's header, enough to decide whether to skip it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockHeader {
    pub first: u32,
    pub last: u32,
    pub entries: u16,
    delta_bits: u8,
}

impl BlockHeader {
    fn decode(bytes: &[u8; BLOCK_HEADER_LEN]) -> Result<Self, &'static str> {
        let header = Self {
            first: u32_at(bytes, 0),
            last: u32_at(bytes, 4),
            entries: u16_at(bytes, 8),
            delta_bits: bytes[10],
        };
        if header.entries == 0
            || usize::from(header.entries) > BLOCK_ENTRIES
            || header.delta_bits > 32
            || bytes[11] != 0
            || header.first > header.last
        {
            return Err("posting block header is out of range");
        }
        Ok(header)
    }

    /// Length of the whole block, header included.
    fn encoded_len(&self, layout: &PostingLayout) -> usize {
        let entries = usize::from(self.entries);
        BLOCK_HEADER_LEN
            + packed_len(entries - 1, u32::from(self.delta_bits))
            + layout.payload_len(entries)
    }
}

/// One decoded block: its ordinals, and each payload column's values.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Block {
    pub ordinals: Vec<u32>,
    pub columns: Vec<Vec<u32>>,
}

fn decode_body(
    header: BlockHeader,
    body: &[u8],
    layout: &PostingLayout,
) -> Result<Block, &'static str> {
    let entries = usize::from(header.entries);
    let bits = u32::from(header.delta_bits);
    let mask = if bits == 0 { 0 } else { (1u64 << bits) - 1 };
    let mut ordinals = Vec::with_capacity(entries);
    ordinals.push(header.first);
    let mut accumulator: u64 = 0;
    let mut available = 0;
    let mut position = 0;
    for _ in 1..entries {
        while available < bits {
            accumulator |= u64::from(body[position]) << available;
            position += 1;
            available += 8;
        }
        let delta = u32::try_from(accumulator & mask).expect("masked to at most 32 bits");
        accumulator >>= bits;
        available -= bits;
        let previous = ordinals[ordinals.len() - 1];
        ordinals.push(
            previous
                .checked_add(delta)
                .ok_or("posting ordinals overflow")?,
        );
    }
    if ordinals[entries - 1] != header.last {
        return Err("posting block's last ordinal does not match its deltas");
    }
    let mut position = packed_len(entries - 1, bits);
    let mut columns = Vec::with_capacity(layout.columns.len());
    for width in &layout.columns {
        let size = width.bytes();
        let values = body[position..position + size * entries]
            .chunks_exact(size)
            .map(|value| {
                let mut bytes = [0; 4];
                bytes[..size].copy_from_slice(value);
                u32::from_le_bytes(bytes)
            })
            .collect();
        position += size * entries;
        columns.push(values);
    }
    Ok(Block { ordinals, columns })
}

/// Reads one posting list block by block from a segment section.
#[derive(Debug)]
pub struct PostingCursor<'s> {
    segment: &'s Segment,
    section: SectionHandle,
    layout: PostingLayout,
    offset: u64,
    remaining: u32,
    peeked: Option<BlockHeader>,
}

impl<'s> PostingCursor<'s> {
    #[must_use]
    pub fn new(
        segment: &'s Segment,
        section: SectionHandle,
        layout: PostingLayout,
        list: PostingList,
    ) -> Self {
        Self {
            segment,
            section,
            layout,
            offset: list.offset,
            remaining: list.count,
            peeked: None,
        }
    }

    /// The next block's header, read without decoding the block.
    ///
    /// # Errors
    ///
    /// When a read fails or the header is malformed.
    pub fn peek(&mut self) -> Result<Option<BlockHeader>, SegmentError> {
        if self.peeked.is_none() && self.remaining > 0 {
            let mut bytes = [0; BLOCK_HEADER_LEN];
            self.segment.read(self.section, self.offset, &mut bytes)?;
            let header = BlockHeader::decode(&bytes).map_err(|reason| self.malformed(reason))?;
            if u32::from(header.entries) > self.remaining {
                return Err(self.malformed("posting block holds more entries than its list"));
            }
            self.peeked = Some(header);
        }
        Ok(self.peeked)
    }

    /// Moves past the next block without decoding it.
    ///
    /// # Errors
    ///
    /// As [`peek`](Self::peek).
    pub fn skip(&mut self) -> Result<(), SegmentError> {
        if let Some(header) = self.peek()? {
            self.advance(header);
        }
        Ok(())
    }

    /// Skips every block whose ordinals are all below `target`.
    ///
    /// # Errors
    ///
    /// As [`peek`](Self::peek).
    pub fn skip_below(&mut self, target: u32) -> Result<(), SegmentError> {
        while let Some(header) = self.peek()? {
            if header.last >= target {
                break;
            }
            self.advance(header);
        }
        Ok(())
    }

    /// Decodes and returns the next block.
    ///
    /// # Errors
    ///
    /// When a read fails or the block is malformed.
    pub fn next_block(&mut self) -> Result<Option<Block>, SegmentError> {
        let Some(header) = self.peek()? else {
            return Ok(None);
        };
        let len = header.encoded_len(&self.layout);
        let bytes = self
            .segment
            .read_range(self.section, self.offset..self.offset + len as u64)?;
        let block = decode_body(header, &bytes[BLOCK_HEADER_LEN..], &self.layout)
            .map_err(|reason| self.malformed(reason))?;
        self.advance(header);
        Ok(Some(block))
    }

    fn advance(&mut self, header: BlockHeader) {
        self.offset += header.encoded_len(&self.layout) as u64;
        self.remaining -= u32::from(header.entries);
        self.peeked = None;
    }

    fn malformed(&self, reason: &'static str) -> SegmentError {
        SegmentError::new(self.segment.path(), ErrorKind::Malformed(reason))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cache::BlockCache;
    use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::{
        Alignment, BLOCK_LEN, KeySpace, KnownSection, Requirement, SectionId, SectionKind,
        SectionSpec, SegmentMeta, SegmentWriter,
    };
    use crate::test_rng::TestRng;

    const POSTINGS: SectionKind = SectionKind::new(5);

    struct Case {
        ordinals: Vec<u32>,
        columns: Vec<Vec<u32>>,
    }

    fn random_case(rng: &mut TestRng, len: usize, layout: &PostingLayout, gap: u64) -> Case {
        let mut ordinal = u32::try_from(rng.below(1000)).unwrap();
        let ordinals = (0..len)
            .map(|_| {
                ordinal += u32::try_from(rng.below(gap)).unwrap();
                ordinal
            })
            .collect();
        let columns = layout
            .columns()
            .iter()
            .map(|width| {
                (0..len)
                    .map(|_| u32::try_from(rng.below(u64::from(width.max()) + 1)).unwrap())
                    .collect()
            })
            .collect();
        Case { ordinals, columns }
    }

    fn write(
        layout: &PostingLayout,
        cases: &[Case],
    ) -> (tempfile::TempDir, Segment, Vec<PostingList>) {
        let mut builder = PostingsBuilder::new(layout.clone());
        let lists = cases
            .iter()
            .map(|case| {
                let columns: Vec<&[u32]> = case.columns.iter().map(Vec::as_slice).collect();
                builder.add_list(&case.ordinals, &columns)
            })
            .collect();
        let (bytes, items) = builder.finish();
        assert_eq!(
            items,
            cases
                .iter()
                .map(|case| case.ordinals.len() as u64)
                .sum::<u64>()
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.seg");
        let family = FamilyId::from_digest(Digest128::of(b"p"));
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
        writer
            .write_section(
                SectionSpec {
                    id: SectionId::new(POSTINGS, KeySpace::new(1)),
                    version: 1,
                    alignment: Alignment::Word,
                    requirement: Requirement::Required,
                },
                &bytes,
                items,
            )
            .unwrap();
        writer.finish().unwrap();
        let segment = Segment::open(
            &path,
            family,
            &[KnownSection {
                kind: POSTINGS,
                version: 1,
            }],
            Arc::new(BlockCache::new(4 * BLOCK_LEN)),
        )
        .unwrap();
        (dir, segment, lists)
    }

    fn read_all(segment: &Segment, layout: &PostingLayout, list: PostingList) -> Case {
        let handle = segment
            .section(SectionId::new(POSTINGS, KeySpace::new(1)))
            .unwrap();
        let mut cursor = PostingCursor::new(segment, handle, layout.clone(), list);
        let mut ordinals = Vec::new();
        let mut columns = vec![Vec::new(); layout.columns().len()];
        while let Some(block) = cursor.next_block().unwrap() {
            ordinals.extend(block.ordinals);
            for (column, values) in columns.iter_mut().zip(block.columns) {
                column.extend(values);
            }
        }
        Case { ordinals, columns }
    }

    #[test]
    fn lists_round_trip_across_block_boundaries() {
        let layouts = [
            PostingLayout::new(&[]),
            PostingLayout::new(&[Width::U8]),
            PostingLayout::new(&[Width::U32, Width::U8, Width::U16]),
        ];
        let mut rng = TestRng::new(20);
        for layout in &layouts {
            let cases: Vec<Case> = [0, 1, 2, 127, 128, 129, 255, 256, 1000, 5000]
                .iter()
                .zip([1, 2, 4, 1_000, 70_000, 500_000].iter().cycle())
                .map(|(len, gap)| random_case(&mut rng, *len, layout, *gap))
                .collect();
            let (_dir, segment, lists) = write(layout, &cases);
            for (case, list) in cases.iter().zip(lists) {
                let read = read_all(&segment, layout, list);
                assert_eq!(
                    read.ordinals,
                    case.ordinals,
                    "{layout:?} len {}",
                    case.ordinals.len()
                );
                assert_eq!(read.columns, case.columns);
            }
        }
    }

    #[test]
    fn full_width_deltas_round_trip() {
        let layout = PostingLayout::new(&[Width::U32]);
        let case = Case {
            ordinals: vec![0, 1, u32::MAX - 1, u32::MAX],
            columns: vec![vec![u32::MAX, 0, 7, u32::MAX]],
        };
        let (_dir, segment, lists) = write(&layout, &[case]);
        let read = read_all(&segment, &layout, lists[0]);
        assert_eq!(read.ordinals, vec![0, 1, u32::MAX - 1, u32::MAX]);
        assert_eq!(read.columns, vec![vec![u32::MAX, 0, 7, u32::MAX]]);
    }

    #[test]
    fn repeated_ordinals_are_kept() {
        let layout = PostingLayout::new(&[Width::U16]);
        let case = Case {
            ordinals: vec![4, 4, 4, 9, 9, 200],
            columns: vec![vec![1, 2, 3, 1, 2, 1]],
        };
        let (_dir, segment, lists) = write(&layout, &[case]);
        let read = read_all(&segment, &layout, lists[0]);
        assert_eq!(read.ordinals, vec![4, 4, 4, 9, 9, 200]);
        assert_eq!(read.columns, vec![vec![1, 2, 3, 1, 2, 1]]);
    }

    #[test]
    fn skipping_reads_block_headers_only_and_lands_on_the_target() {
        let layout = PostingLayout::new(&[Width::U8]);
        let ordinals: Vec<u32> = (0..1000).map(|index| index * 10).collect();
        let case = Case {
            columns: vec![ordinals.iter().map(|ordinal| ordinal % 251).collect()],
            ordinals,
        };
        let (_dir, segment, lists) = write(&layout, &[case]);
        let handle = segment
            .section(SectionId::new(POSTINGS, KeySpace::new(1)))
            .unwrap();
        let mut cursor = PostingCursor::new(&segment, handle, layout.clone(), lists[0]);
        cursor.skip_below(5_000).unwrap();
        let header = cursor.peek().unwrap().unwrap();
        assert!(header.first <= 5_000 && header.last >= 5_000, "{header:?}");
        let block = cursor.next_block().unwrap().unwrap();
        assert!(block.ordinals.contains(&5_000));
        cursor.skip_below(u32::MAX).unwrap();
        assert_eq!(cursor.next_block().unwrap(), None);
    }

    #[test]
    fn a_block_whose_deltas_disagree_with_its_last_ordinal_is_refused() {
        let layout = PostingLayout::new(&[]);
        let mut bytes = Vec::new();
        encode_block(&mut bytes, &layout, &[1, 2, 3], &[]);
        bytes[4] = 9; // last ordinal claims 9, deltas reach 3
        let header = BlockHeader::decode(&crate::le::array(&bytes, 0)).unwrap();
        assert!(decode_body(header, &bytes[BLOCK_HEADER_LEN..], &layout).is_err());
    }

    #[test]
    #[should_panic(expected = "must not decrease")]
    fn decreasing_ordinals_are_a_bug() {
        PostingsBuilder::new(PostingLayout::new(&[])).add_list(&[3, 2], &[]);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn payload_values_must_fit_their_width() {
        PostingsBuilder::new(PostingLayout::new(&[Width::U8])).add_list(&[1], &[&[256]]);
    }
}
