//! Sorted tables stored inside segment sections, searched in place.
//!
//! A [`UnitTable`] maps fixed-width keys, such as content keys, to the byte
//! range of each unit in a data section. A unit's ordinal within the segment
//! is its position in the table. A [`LookupTable`] maps a 64-bit term key to
//! its posting list, with the entry count stored beside it so a query planner
//! can order terms rarest first without reading any postings.
//!
//! Both are binary-searched through the segment's block cache: a lookup reads
//! only the entries it compares.

use std::cmp::Ordering;

use crate::le::{u32_at, u64_at};
use crate::postings::PostingList;
use crate::segment::{ErrorKind, SectionHandle, Segment, SegmentError};

/// Where a unit's bytes lie in its data section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnitLocation {
    pub offset: u64,
    pub len: u32,
}

const UNIT_TABLE_PREAMBLE: usize = 8;
const UNIT_LOCATION_LEN: usize = 12;

/// Builds a unit table section. Keys must be pushed in strictly ascending
/// byte order.
#[derive(Debug)]
pub struct UnitTableBuilder {
    key_width: usize,
    bytes: Vec<u8>,
    count: u32,
}

impl UnitTableBuilder {
    /// A table whose keys are all `key_width` bytes long.
    ///
    /// # Panics
    ///
    /// When `key_width` is 0 or above 64.
    #[must_use]
    pub fn new(key_width: usize) -> Self {
        assert!(
            (1..=64).contains(&key_width),
            "unit table key width {key_width} is outside 1..=64"
        );
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            &u16::try_from(key_width)
                .expect("checked above")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Self {
            key_width,
            bytes,
            count: 0,
        }
    }

    /// Adds the next unit and returns its ordinal.
    ///
    /// # Panics
    ///
    /// When the key has the wrong width or does not sort after the previous
    /// one: producers sort their units, so either is a bug.
    pub fn push(&mut self, key: &[u8], location: UnitLocation) -> u32 {
        assert_eq!(key.len(), self.key_width, "unit key has the wrong width");
        if self.count > 0 {
            let entry = self.key_width + UNIT_LOCATION_LEN;
            let previous_start = self.bytes.len() - entry;
            let previous = &self.bytes[previous_start..previous_start + self.key_width];
            assert!(
                previous < key,
                "unit keys must be strictly ascending: {previous:02x?} then {key:02x?}"
            );
        }
        self.bytes.extend_from_slice(key);
        self.bytes.extend_from_slice(&location.offset.to_le_bytes());
        self.bytes.extend_from_slice(&location.len.to_le_bytes());
        let ordinal = self.count;
        self.count = self
            .count
            .checked_add(1)
            .expect("a unit table holds fewer than 2^32 units");
        ordinal
    }

    /// The section bytes and the item count to record for the section.
    #[must_use]
    pub fn finish(mut self) -> (Vec<u8>, u64) {
        self.bytes[4..8].copy_from_slice(&self.count.to_le_bytes());
        (self.bytes, u64::from(self.count))
    }
}

/// A unit table in an open segment.
#[derive(Debug)]
pub struct UnitTable<'s> {
    segment: &'s Segment,
    section: SectionHandle,
    key_width: usize,
    count: u32,
}

impl<'s> UnitTable<'s> {
    /// Reads and checks the table's preamble.
    ///
    /// # Errors
    ///
    /// When the section cannot be read or its size does not match its entries.
    pub fn open(segment: &'s Segment, section: SectionHandle) -> Result<Self, SegmentError> {
        let malformed = |reason| SegmentError::new(segment.path(), ErrorKind::Malformed(reason));
        let info = segment.info(section);
        let mut preamble = [0; UNIT_TABLE_PREAMBLE];
        segment.read(section, 0, &mut preamble)?;
        let key_width = usize::from(crate::le::u16_at(&preamble, 0));
        let count = u32_at(&preamble, 4);
        let expected =
            (key_width + UNIT_LOCATION_LEN) as u64 * u64::from(count) + UNIT_TABLE_PREAMBLE as u64;
        if !(1..=64).contains(&key_width)
            || preamble[2..4] != [0, 0]
            || info.len != expected
            || info.item_count != u64::from(count)
        {
            return Err(malformed("unit table size does not match its entries"));
        }
        Ok(Self {
            segment,
            section,
            key_width,
            count,
        })
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The ordinal and location of the unit with `key`, if present.
    ///
    /// # Errors
    ///
    /// When a read fails or a block fails its checksum.
    pub fn get(&self, key: &[u8]) -> Result<Option<(u32, UnitLocation)>, SegmentError> {
        if key.len() != self.key_width {
            return Ok(None);
        }
        let (mut low, mut high) = (0, self.count);
        let mut entry = vec![0; self.key_width + UNIT_LOCATION_LEN];
        while low < high {
            let middle = low + (high - low) / 2;
            self.read_entry(middle, &mut entry)?;
            match entry[..self.key_width].cmp(key) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Ok(Some((middle, self.decode_location(&entry)))),
            }
        }
        Ok(None)
    }

    /// The key and location of the unit at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails or a block fails its checksum.
    ///
    /// # Panics
    ///
    /// When `ordinal` is not below [`len`](Self::len).
    pub fn entry(&self, ordinal: u32) -> Result<(Vec<u8>, UnitLocation), SegmentError> {
        assert!(
            ordinal < self.count,
            "unit ordinal {ordinal} is past {}",
            self.count
        );
        let mut entry = vec![0; self.key_width + UNIT_LOCATION_LEN];
        self.read_entry(ordinal, &mut entry)?;
        let location = self.decode_location(&entry);
        entry.truncate(self.key_width);
        Ok((entry, location))
    }

    fn read_entry(&self, ordinal: u32, entry: &mut [u8]) -> Result<(), SegmentError> {
        let offset = UNIT_TABLE_PREAMBLE as u64 + u64::from(ordinal) * entry.len() as u64;
        self.segment.read(self.section, offset, entry)
    }

    fn decode_location(&self, entry: &[u8]) -> UnitLocation {
        UnitLocation {
            offset: u64_at(entry, self.key_width),
            len: u32_at(entry, self.key_width + 8),
        }
    }

    /// What [`open`](Self::open) read and checked, so the table can be
    /// attached to the same open segment again without reading it.
    pub(crate) fn parts(&self) -> UnitTableParts {
        UnitTableParts {
            section: self.section,
            key_width: self.key_width,
            count: self.count,
        }
    }

    /// The table [`parts`](Self::parts) came from, attached to `segment`,
    /// which must be the very segment it was opened on: its section handle
    /// panics on any other.
    pub(crate) fn attach(segment: &'s Segment, parts: UnitTableParts) -> Self {
        Self {
            segment,
            section: parts.section,
            key_width: parts.key_width,
            count: parts.count,
        }
    }
}

/// A [`UnitTable`] detached from its segment (see [`UnitTable::parts`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct UnitTableParts {
    pub(crate) section: SectionHandle,
    key_width: usize,
    count: u32,
}

const LOOKUP_ENTRY_LEN: usize = 20;

/// Builds a lookup table section: term keys in strictly ascending order, each
/// with its posting list.
#[derive(Debug, Default)]
pub struct LookupTableBuilder {
    bytes: Vec<u8>,
    last: Option<u64>,
    count: u64,
}

impl LookupTableBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next key.
    ///
    /// # Panics
    ///
    /// When `key` does not sort after the previous one.
    pub fn push(&mut self, key: u64, postings: PostingList) {
        if let Some(last) = self.last {
            assert!(
                last < key,
                "lookup keys must be strictly ascending: {last} then {key}"
            );
        }
        self.bytes.extend_from_slice(&key.to_le_bytes());
        self.bytes.extend_from_slice(&postings.offset.to_le_bytes());
        self.bytes.extend_from_slice(&postings.count.to_le_bytes());
        self.last = Some(key);
        self.count += 1;
    }

    /// The section bytes and the item count to record for the section.
    #[must_use]
    pub fn finish(self) -> (Vec<u8>, u64) {
        (self.bytes, self.count)
    }
}

/// A lookup table in an open segment.
#[derive(Debug)]
pub struct LookupTable<'s> {
    segment: &'s Segment,
    section: SectionHandle,
    count: u64,
}

impl<'s> LookupTable<'s> {
    /// Checks that the section holds whole entries.
    ///
    /// # Errors
    ///
    /// When the section's size does not match its item count.
    pub fn open(segment: &'s Segment, section: SectionHandle) -> Result<Self, SegmentError> {
        let info = segment.info(section);
        if info.item_count.checked_mul(LOOKUP_ENTRY_LEN as u64) != Some(info.len) {
            return Err(SegmentError::new(
                segment.path(),
                ErrorKind::Malformed("lookup table size does not match its entries"),
            ));
        }
        Ok(Self {
            segment,
            section,
            count: info.item_count,
        })
    }

    pub(crate) fn section(&self) -> SectionHandle {
        self.section
    }

    /// The table [`Self::open`] checked as `section` of the same `segment`,
    /// without checking it again.
    pub(crate) fn reopen(segment: &'s Segment, section: SectionHandle) -> Self {
        Self {
            segment,
            section,
            count: segment.info(section).item_count,
        }
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The posting list of `key`, if the table has it.
    ///
    /// # Errors
    ///
    /// When a read fails or a block fails its checksum.
    pub fn get(&self, key: u64) -> Result<Option<PostingList>, SegmentError> {
        let (mut low, mut high) = (0, self.count);
        let mut entry = [0; LOOKUP_ENTRY_LEN];
        while low < high {
            let middle = low + (high - low) / 2;
            self.segment
                .read(self.section, middle * LOOKUP_ENTRY_LEN as u64, &mut entry)?;
            match u64_at(&entry, 0).cmp(&key) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => {
                    return Ok(Some(PostingList {
                        offset: u64_at(&entry, 8),
                        count: u32_at(&entry, 16),
                    }));
                }
            }
        }
        Ok(None)
    }
}

crate::heap_bytes_none!(UnitTableParts);

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

    const UNITS: SectionKind = SectionKind::new(1);
    const LOOKUP: SectionKind = SectionKind::new(2);

    fn open_with(sections: &[(SectionKind, Vec<u8>, u64)]) -> (tempfile::TempDir, Segment) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.seg");
        let family = FamilyId::from_digest(Digest128::of(b"t"));
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
        for (kind, bytes, items) in sections {
            writer
                .write_section(
                    SectionSpec {
                        id: SectionId::new(*kind, KeySpace::NONE),
                        version: 1,
                        alignment: Alignment::Word,
                        requirement: Requirement::Required,
                    },
                    bytes,
                    *items,
                )
                .unwrap();
        }
        writer.finish().unwrap();
        let known = [
            KnownSection {
                kind: UNITS,
                version: 1,
            },
            KnownSection {
                kind: LOOKUP,
                version: 1,
            },
        ];
        let segment = Segment::open(
            &path,
            family,
            &known,
            Arc::new(BlockCache::new(4 * BLOCK_LEN)),
        )
        .unwrap();
        (dir, segment)
    }

    #[test]
    fn unit_tables_find_every_key_and_no_other() {
        let mut rng = TestRng::new(10);
        let mut keys: Vec<[u8; 16]> = (0..20_000)
            .map(|_| {
                let mut key = [0; 16];
                key.copy_from_slice(&rng.bytes(16));
                key
            })
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let mut builder = UnitTableBuilder::new(16);
        for (index, key) in keys.iter().enumerate() {
            let location = UnitLocation {
                offset: index as u64 * 100,
                len: u32::try_from(index).unwrap(),
            };
            assert_eq!(builder.push(key, location), u32::try_from(index).unwrap());
        }
        let (bytes, items) = builder.finish();
        let (_dir, segment) = open_with(&[(UNITS, bytes, items)]);
        let handle = segment
            .section(SectionId::new(UNITS, KeySpace::NONE))
            .unwrap();
        let table = UnitTable::open(&segment, handle).unwrap();
        assert_eq!(table.len() as usize, keys.len());
        for (index, key) in keys.iter().enumerate().step_by(7) {
            let (ordinal, location) = table.get(key).unwrap().unwrap();
            assert_eq!(ordinal as usize, index);
            assert_eq!(location.offset, index as u64 * 100);
            assert_eq!(table.entry(ordinal).unwrap(), (key.to_vec(), location));
        }
        for _ in 0..500 {
            let probe = rng.bytes(16);
            let expected = keys.binary_search_by(|key| key.as_slice().cmp(&probe)).ok();
            let found = table
                .get(&probe)
                .unwrap()
                .map(|(ordinal, _)| ordinal as usize);
            assert_eq!(found, expected);
        }
        assert_eq!(
            table.get(&[0; 15]).unwrap(),
            None,
            "wrong-width keys never match"
        );
    }

    #[test]
    fn an_empty_unit_table_finds_nothing() {
        let (bytes, items) = UnitTableBuilder::new(8).finish();
        let (_dir, segment) = open_with(&[(UNITS, bytes, items)]);
        let handle = segment
            .section(SectionId::new(UNITS, KeySpace::NONE))
            .unwrap();
        let table = UnitTable::open(&segment, handle).unwrap();
        assert!(table.is_empty());
        assert_eq!(table.get(&[0; 8]).unwrap(), None);
    }

    #[test]
    fn a_unit_table_whose_size_lies_is_refused() {
        let mut builder = UnitTableBuilder::new(4);
        builder.push(b"aaaa", UnitLocation { offset: 0, len: 1 });
        let (bytes, _) = builder.finish();
        let (_dir, segment) = open_with(&[(UNITS, bytes, 2)]);
        let handle = segment
            .section(SectionId::new(UNITS, KeySpace::NONE))
            .unwrap();
        assert!(
            UnitTable::open(&segment, handle)
                .unwrap_err()
                .is_corruption()
        );
    }

    #[test]
    #[should_panic(expected = "strictly ascending")]
    fn unit_keys_out_of_order_are_a_bug() {
        let mut builder = UnitTableBuilder::new(1);
        builder.push(b"b", UnitLocation { offset: 0, len: 0 });
        builder.push(b"a", UnitLocation { offset: 0, len: 0 });
    }

    #[test]
    fn lookup_tables_find_every_key_and_no_other() {
        let mut rng = TestRng::new(11);
        let mut keys: Vec<u64> = (0..30_000).map(|_| rng.next_u64()).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut builder = LookupTableBuilder::new();
        for (index, key) in keys.iter().enumerate() {
            builder.push(
                *key,
                PostingList {
                    offset: index as u64 * 3,
                    count: u32::try_from(index % 1000 + 1).unwrap(),
                },
            );
        }
        let (bytes, items) = builder.finish();
        let (_dir, segment) = open_with(&[(LOOKUP, bytes, items)]);
        let handle = segment
            .section(SectionId::new(LOOKUP, KeySpace::NONE))
            .unwrap();
        let table = LookupTable::open(&segment, handle).unwrap();
        assert_eq!(table.len(), keys.len() as u64);
        for (index, key) in keys.iter().enumerate().step_by(11) {
            let list = table.get(*key).unwrap().unwrap();
            assert_eq!(list.offset, index as u64 * 3);
            assert_eq!(list.count as usize, index % 1000 + 1);
        }
        for _ in 0..500 {
            let probe = rng.next_u64();
            assert_eq!(
                table.get(probe).unwrap().is_some(),
                keys.binary_search(&probe).is_ok()
            );
        }
    }
}
