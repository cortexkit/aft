//! The text plane's sections in a segment.
//!
//! | Section | Holds |
//! |---|---|
//! | text units | a unit table: each content key in ascending order; a content's ordinal is its position |
//! | text unit data | per content: byte length (u64), distinct trigram count (u32), status (u8) |
//! | trigram lookup | a lookup table: each trigram to its posting list |
//! | trigram postings | posting lists of content ordinals, each entry with its next-character mask (u8) |
//!
//! A content that is not indexed (binary, or too large) has a unit and no
//! postings, so readers know to verify it directly.

use std::collections::BTreeMap;

use crate::ids::{ContentKey, Digest128};
use crate::postings::{PostingCursor, PostingLayout, PostingsBuilder, Width};
use crate::segment::{
    Alignment, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentWriter,
};
use crate::table::{
    LookupTable, LookupTableBuilder, UnitLocation, UnitTable, UnitTableBuilder, UnitTableParts,
};

use super::trigrams::ContentTrigrams;

const UNITS: SectionKind = SectionKind::new(0x0100);
const UNIT_DATA: SectionKind = SectionKind::new(0x0101);
const TRIGRAM_LOOKUP: SectionKind = SectionKind::new(0x0102);
const TRIGRAM_POSTINGS: SectionKind = SectionKind::new(0x0103);
const VERSION: u16 = 1;
const KEY_SPACE: KeySpace = KeySpace::new(0);
const UNIT_DATA_LEN: usize = 13;
const UNIT_DATA_LEN_U32: u32 = 13;

/// The text sections this build reads.
pub const KNOWN: [KnownSection; 4] = [
    KnownSection {
        kind: UNITS,
        version: VERSION,
    },
    KnownSection {
        kind: UNIT_DATA,
        version: VERSION,
    },
    KnownSection {
        kind: TRIGRAM_LOOKUP,
        version: VERSION,
    },
    KnownSection {
        kind: TRIGRAM_POSTINGS,
        version: VERSION,
    },
];

fn posting_layout() -> PostingLayout {
    PostingLayout::new(&[Width::U8])
}

/// Whether a content's trigrams are indexed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextStatus {
    Indexed,
    /// Holds binary data. Grep reads it only if asked to.
    Binary,
    /// Larger than the indexing limit; verified by reading it.
    Oversize,
}

impl TextStatus {
    const fn code(self) -> u8 {
        match self {
            Self::Indexed => 0,
            Self::Binary => 1,
            Self::Oversize => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Indexed),
            1 => Some(Self::Binary),
            2 => Some(Self::Oversize),
            _ => None,
        }
    }
}

/// What the text plane records about one content.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TextUnit {
    pub len: u64,
    /// Distinct trigrams; the lexical score of `aft_search` divides by it.
    pub distinct: u32,
    pub status: TextStatus,
}

/// Collects contents for the text sections of one segment.
#[derive(Debug, Default)]
pub struct TextSectionsBuilder {
    units: BTreeMap<ContentKey, (TextUnit, Vec<(u32, u8)>)>,
}

impl TextSectionsBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an indexed content. Adding a content twice keeps the first.
    pub fn add_indexed(&mut self, key: ContentKey, len: u64, trigrams: &ContentTrigrams) {
        let unit = TextUnit {
            len,
            distinct: u32::try_from(trigrams.distinct()).unwrap_or(u32::MAX),
            status: TextStatus::Indexed,
        };
        self.units
            .entry(key)
            .or_insert_with(|| (unit, trigrams.entries().to_vec()));
    }

    /// Adds a content whose trigrams are not indexed.
    pub fn add_unindexed(&mut self, key: ContentKey, len: u64, status: TextStatus) {
        debug_assert_ne!(status, TextStatus::Indexed);
        let unit = TextUnit {
            len,
            distinct: 0,
            status,
        };
        self.units.entry(key).or_insert((unit, Vec::new()));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Writes the four sections into `writer`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub fn write(self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        let mut data = Vec::with_capacity(self.units.len() * UNIT_DATA_LEN);
        let mut lists = BTreeMap::<u32, (Vec<u32>, Vec<u32>)>::new();
        for (key, (unit, trigrams)) in self.units {
            let ordinal = table.push(
                key.digest().as_bytes(),
                UnitLocation {
                    offset: data.len() as u64,
                    len: UNIT_DATA_LEN_U32,
                },
            );
            data.extend_from_slice(&unit.len.to_le_bytes());
            data.extend_from_slice(&unit.distinct.to_le_bytes());
            data.push(unit.status.code());
            for (trigram, mask) in trigrams {
                let (ordinals, masks) = lists.entry(trigram).or_default();
                ordinals.push(ordinal);
                masks.push(u32::from(mask));
            }
        }
        let mut postings = PostingsBuilder::new(posting_layout());
        let mut lookup = LookupTableBuilder::new();
        for (trigram, (ordinals, masks)) in lists {
            let list = postings.add_list(&ordinals, &[&masks]);
            lookup.push(u64::from(trigram), list);
        }
        let (table, units) = table.finish();
        writer.write_section(spec(UNITS), &table, units)?;
        writer.write_section(spec(UNIT_DATA), &data, units)?;
        let (lookup, keys) = lookup.finish();
        writer.write_section(spec(TRIGRAM_LOOKUP), &lookup, keys)?;
        let (postings, entries) = postings.finish();
        writer.write_section(spec(TRIGRAM_POSTINGS), &postings, entries)
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

/// The text sections of one open segment.
#[derive(Debug)]
pub struct TextSegment<'s> {
    segment: &'s Segment,
    units: UnitTable<'s>,
    data: SectionHandle,
    lookup: LookupTable<'s>,
    postings: SectionHandle,
}

/// A segment's text sections as [`TextSegment::open`] found and checked
/// them, kept without the segment so a reader that holds the segment can
/// rebuild the [`TextSegment`] for each query without reading it again
/// (see [`TextSegment::with_sections`]).
#[derive(Clone, Copy, Debug)]
pub struct TextSections {
    units: UnitTableParts,
    data: SectionHandle,
    lookup: SectionHandle,
    postings: SectionHandle,
}

impl<'s> TextSegment<'s> {
    /// The segment's text sections, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// When the segment has only some of them, or they are malformed.
    pub fn open(segment: &'s Segment) -> Result<Option<Self>, SegmentError> {
        let handles = [UNITS, UNIT_DATA, TRIGRAM_LOOKUP, TRIGRAM_POSTINGS]
            .map(|kind| segment.section(SectionId::new(kind, KEY_SPACE)));
        let [Some(units), Some(data), Some(lookup), Some(postings)] = handles else {
            if handles.iter().any(Option::is_some) {
                return Err(SegmentError::new(
                    segment.path(),
                    ErrorKind::Malformed("a segment has only some text sections"),
                ));
            }
            return Ok(None);
        };
        let units = UnitTable::open(segment, units)?;
        if segment.info(data).len != u64::from(units.len()) * UNIT_DATA_LEN as u64 {
            return Err(SegmentError::new(
                segment.path(),
                ErrorKind::Malformed("text unit data does not match the unit table"),
            ));
        }
        Ok(Some(Self {
            segment,
            units,
            data,
            lookup: LookupTable::open(segment, lookup)?,
            postings,
        }))
    }

    /// What [`Self::open`] found and checked.
    #[must_use]
    pub fn sections(&self) -> TextSections {
        TextSections {
            units: self.units.parts(),
            data: self.data,
            lookup: self.lookup.section(),
            postings: self.postings,
        }
    }

    /// The text sections of `segment`, from what [`Self::sections`] took
    /// from the same segment, without reading or checking them again.
    ///
    /// `sections` must come from `segment` itself: a section handle panics
    /// on any other segment.
    #[must_use]
    pub fn with_sections(segment: &'s Segment, sections: TextSections) -> Self {
        Self {
            segment,
            units: UnitTable::attach(segment, sections.units),
            data: sections.data,
            lookup: LookupTable::reopen(segment, sections.lookup),
            postings: sections.postings,
        }
    }

    /// How many contents the segment holds.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The content key of every unit, in ordinal order.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn keys(&self) -> Result<Vec<ContentKey>, SegmentError> {
        (0..self.units.len())
            .map(|ordinal| {
                let (key, _) = self.units.entry(ordinal)?;
                let bytes = <[u8; Digest128::LEN]>::try_from(key.as_slice())
                    .map_err(|_| self.malformed("a text unit key is not a content key"))?;
                Ok(ContentKey::from_digest(Digest128::from_bytes(bytes)))
            })
            .collect()
    }

    /// What the segment records about the content at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails or the record is malformed.
    pub fn unit(&self, ordinal: u32) -> Result<TextUnit, SegmentError> {
        let mut bytes = [0; UNIT_DATA_LEN];
        self.segment.read(
            self.data,
            u64::from(ordinal) * UNIT_DATA_LEN as u64,
            &mut bytes,
        )?;
        let status = TextStatus::from_code(bytes[12])
            .ok_or_else(|| self.malformed("unknown text status"))?;
        Ok(TextUnit {
            len: crate::le::u64_at(&bytes, 0),
            distinct: crate::le::u32_at(&bytes, 8),
            status,
        })
    }

    /// How many contents hold `trigram`.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn count(&self, trigram: u32) -> Result<u32, SegmentError> {
        Ok(self
            .lookup
            .get(u64::from(trigram))?
            .map_or(0, |list| list.count))
    }

    /// The ordinals in `within` (ascending) whose content holds `trigram`
    /// followed by a byte in `next` (zero: by anything), ascending.
    ///
    /// # Errors
    ///
    /// When a read fails or a posting block is malformed.
    pub fn ordinals(
        &self,
        trigram: u32,
        next: u8,
        within: &[u32],
    ) -> Result<Vec<u32>, SegmentError> {
        let Some(list) = self.lookup.get(u64::from(trigram))? else {
            return Ok(Vec::new());
        };
        let mut cursor = PostingCursor::new(self.segment, self.postings, posting_layout(), list);
        let mut found = Vec::new();
        let mut wanted = within.iter().copied().peekable();
        while let Some(&target) = wanted.peek() {
            cursor.skip_below(target)?;
            let Some(block) = cursor.next_block()? else {
                break;
            };
            let masks = &block.columns[0];
            for (ordinal, mask) in block.ordinals.iter().zip(masks) {
                while wanted.peek().is_some_and(|target| target < ordinal) {
                    wanted.next();
                }
                if wanted.peek() == Some(ordinal) {
                    if next == 0 || u32::from(next) & mask != 0 {
                        found.push(*ordinal);
                    }
                    wanted.next();
                }
            }
        }
        Ok(found)
    }

    /// Every ordinal whose content holds `trigram`, ascending: the whole
    /// posting list, for a caller that holds most of the segment's contents.
    ///
    /// # Errors
    ///
    /// When a read fails or a posting block is malformed.
    pub fn all_ordinals(&self, trigram: u32) -> Result<Vec<u32>, SegmentError> {
        let Some(list) = self.lookup.get(u64::from(trigram))? else {
            return Ok(Vec::new());
        };
        let mut found = Vec::with_capacity(list.count as usize);
        let mut cursor = PostingCursor::new(self.segment, self.postings, posting_layout(), list);
        while let Some(block) = cursor.next_block()? {
            found.extend_from_slice(&block.ordinals);
        }
        Ok(found)
    }

    fn malformed(&self, reason: &'static str) -> SegmentError {
        SegmentError::new(self.segment.path(), ErrorKind::Malformed(reason))
    }
}

crate::heap_bytes_none!(TextSections);
