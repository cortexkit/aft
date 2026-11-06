//! The call graph's per-file sections in a segment.
//!
//! | Section | Holds |
//! |---|---|
//! | fragment units | a unit table: each fragment key in ascending order; a fragment's ordinal is its position |
//! | fragment data, fragment index | each fragment's bytes, packed (see `packed`) |
//! | term lookup, one key space per [`TermKind`] | each term's 64-bit hash to its posting list |
//! | term postings, same key spaces | posting lists of fragment ordinals |
//!
//! A fragment is one file's call-graph resolution, as bytes the caller
//! encodes; this plane does not interpret them. Terms let a query find the
//! fragments that can hold its answer (the callers of a target, the files
//! with a reference or symbol of some name, the files of a language)
//! without reading every fragment. Further terms name what each resolution
//! recorded as its inputs, so after an edit the files whose resolution may
//! have changed are found the same way. A term is stored as a hash, which
//! can collide, so a lookup returns candidates the caller confirms in the
//! fragment itself.

use std::collections::BTreeMap;

use crate::ids::Digest128;
use crate::packed::{self, PackedBuilder};
use crate::postings::{BLOCK_ENTRIES, PostingCursor, PostingLayout, PostingList, PostingsBuilder};
use crate::segment::{
    Alignment, ErrorKind, KeySpace, KnownSection, Requirement, SectionHandle, SectionId,
    SectionKind, SectionSpec, Segment, SegmentError, SegmentWriter,
};
use crate::table::{LookupTable, LookupTableBuilder, UnitLocation, UnitTable, UnitTableBuilder};

const UNITS: SectionKind = SectionKind::new(0x0300);
const DATA: SectionKind = SectionKind::new(0x0301);
const INDEX: SectionKind = SectionKind::new(0x0302);
const TERM_LOOKUP: SectionKind = SectionKind::new(0x0303);
const TERM_POSTINGS: SectionKind = SectionKind::new(0x0304);
/// Version 2 added the recorded-input terms; version 3 the method terms;
/// version 4 files folder reads under a folder's existence or its listing
/// separately. These sections are required, so raising this raises
/// [`crate::segment::FORMAT_VERSION`] too.
const VERSION: u16 = 4;
const UNIT_KEY_SPACE: KeySpace = KeySpace::new(0);

/// The graph sections this build reads.
pub const KNOWN: [KnownSection; 5] = [
    KnownSection {
        kind: UNITS,
        version: VERSION,
    },
    KnownSection {
        kind: DATA,
        version: VERSION,
    },
    KnownSection {
        kind: INDEX,
        version: VERSION,
    },
    KnownSection {
        kind: TERM_LOOKUP,
        version: VERSION,
    },
    KnownSection {
        kind: TERM_POSTINGS,
        version: VERSION,
    },
];

/// Identifies one fragment: the caller derives it from everything the
/// fragment's bytes depend on being the same.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FragmentKey(Digest128);

impl FragmentKey {
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

/// What a term names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TermKind {
    /// A call target: a file and a symbol in it.
    CallTarget,
    /// The short name of a reference.
    ReferenceName,
    /// A name a symbol can be looked up by.
    SymbolName,
    /// The language the file was parsed as.
    Language,
    /// An input key the file's binding recorded.
    BindingKey,
    /// An input key the file's resolution recorded.
    ResolutionKey,
    /// An input key the file's dispatch type targets recorded.
    DispatchKey,
    /// An input key the file's method-call sites recorded.
    SiteKey,
    /// A path or folder that a recorded checkout read of the file consulted.
    FactSubject,
    /// The name of a method the file declares.
    MethodName,
    /// The name of a method a Go interface of the file requires.
    GoRequired,
}

impl TermKind {
    const ALL: [Self; 11] = [
        Self::CallTarget,
        Self::ReferenceName,
        Self::SymbolName,
        Self::Language,
        Self::BindingKey,
        Self::ResolutionKey,
        Self::DispatchKey,
        Self::SiteKey,
        Self::FactSubject,
        Self::MethodName,
        Self::GoRequired,
    ];

    const fn key_space(self) -> KeySpace {
        KeySpace::new(match self {
            Self::CallTarget => 1,
            Self::ReferenceName => 2,
            Self::SymbolName => 3,
            Self::Language => 4,
            Self::BindingKey => 5,
            Self::ResolutionKey => 6,
            Self::DispatchKey => 7,
            Self::SiteKey => 8,
            Self::FactSubject => 9,
            Self::MethodName => 10,
            Self::GoRequired => 11,
        })
    }
}

/// The stored hash of a term.
fn term_hash(term: &[u8]) -> u64 {
    let digest = Digest128::of(term);
    let bytes = digest.as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn layout() -> PostingLayout {
    PostingLayout::new(&[])
}

/// The fewest bytes one posting block takes in [`layout`], which has no
/// payload columns: the 12-byte block header alone (first and last ordinal,
/// entry count, delta width, a reserved byte), when every delta in the block
/// is zero bits wide. A block holds at most [`BLOCK_ENTRIES`] entries.
const MIN_POSTING_BLOCK_LEN: u64 = 12;

/// Whether the posting section, `section_len` bytes long, can hold `list`:
/// the blocks its count needs, each at least [`MIN_POSTING_BLOCK_LEN`]
/// bytes, must fit between its offset and the section's end.
fn fits(list: PostingList, section_len: u64) -> bool {
    let needed = u64::from(list.count).div_ceil(BLOCK_ENTRIES as u64) * MIN_POSTING_BLOCK_LEN;
    list.offset
        .checked_add(needed)
        .is_some_and(|end| end <= section_len)
}

/// A fragment's bytes and the hashes of its terms.
type PendingFragment = (Vec<u8>, Vec<(TermKind, u64)>);

/// Collects fragments for the graph sections of one segment.
#[derive(Debug, Default)]
pub struct GraphSectionsBuilder {
    fragments: BTreeMap<FragmentKey, PendingFragment>,
}

impl GraphSectionsBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a fragment with the terms it answers for. Adding a key twice
    /// keeps the first.
    pub fn add<'t>(
        &mut self,
        key: FragmentKey,
        bytes: Vec<u8>,
        terms: impl IntoIterator<Item = (TermKind, &'t [u8])>,
    ) {
        self.fragments.entry(key).or_insert_with(|| {
            let mut hashed = terms
                .into_iter()
                .map(|(kind, term)| (kind, term_hash(term)))
                .collect::<Vec<_>>();
            hashed.sort_unstable();
            hashed.dedup();
            (bytes, hashed)
        });
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    /// Writes the graph sections into `writer`.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub fn write(self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        let mut data = PackedBuilder::with_capacity(self.fragments.len());
        let mut lists = BTreeMap::<(TermKind, u64), Vec<u32>>::new();
        for (key, (bytes, terms)) in self.fragments {
            // A fragment's bytes are found through the packed index by
            // ordinal, so the unit table carries no location.
            let ordinal = table.push(key.0.as_bytes(), UnitLocation { offset: 0, len: 0 });
            data.push(&bytes);
            for term in terms {
                lists.entry(term).or_default().push(ordinal);
            }
        }
        let (table, units) = table.finish();
        writer.write_section(spec(UNITS, UNIT_KEY_SPACE), &table, units)?;
        let (data, index) = data.finish();
        writer.write_section(spec(DATA, UNIT_KEY_SPACE), &data, units)?;
        writer.write_section(spec(INDEX, UNIT_KEY_SPACE), &index, units)?;
        for kind in TermKind::ALL {
            let mut postings = PostingsBuilder::new(layout());
            let mut lookup = LookupTableBuilder::new();
            for ((_, hash), ordinals) in lists.range((kind, 0)..=(kind, u64::MAX)) {
                lookup.push(*hash, postings.add_list(ordinals, &[]));
            }
            let (lookup, terms) = lookup.finish();
            writer.write_section(spec(TERM_LOOKUP, kind.key_space()), &lookup, terms)?;
            let (postings, entries) = postings.finish();
            writer.write_section(spec(TERM_POSTINGS, kind.key_space()), &postings, entries)?;
        }
        Ok(())
    }
}

fn spec(kind: SectionKind, key_space: KeySpace) -> SectionSpec {
    SectionSpec {
        id: SectionId::new(kind, key_space),
        version: VERSION,
        alignment: Alignment::Word,
        requirement: Requirement::Required,
    }
}

/// The graph sections of one open segment.
#[derive(Debug)]
pub struct GraphSegment<'s> {
    segment: &'s Segment,
    units: UnitTable<'s>,
    data: SectionHandle,
    index: SectionHandle,
    terms: Vec<(TermKind, LookupTable<'s>, SectionHandle)>,
}

impl<'s> GraphSegment<'s> {
    /// The segment's graph sections, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// When the segment has only some of them, or they are malformed.
    pub fn open(segment: &'s Segment) -> Result<Option<Self>, SegmentError> {
        let section = |kind, key_space| segment.section(SectionId::new(kind, key_space));
        let mut handles = vec![
            section(UNITS, UNIT_KEY_SPACE),
            section(DATA, UNIT_KEY_SPACE),
            section(INDEX, UNIT_KEY_SPACE),
        ];
        for kind in TermKind::ALL {
            handles.push(section(TERM_LOOKUP, kind.key_space()));
            handles.push(section(TERM_POSTINGS, kind.key_space()));
        }
        if handles.iter().all(Option::is_none) {
            return Ok(None);
        }
        let Some(handles) = handles.into_iter().collect::<Option<Vec<_>>>() else {
            return Err(malformed(segment, "a segment has only some graph sections"));
        };
        let units = UnitTable::open(segment, handles[0])?;
        if segment.info(handles[2]).len != u64::from(units.len()) * packed::INDEX_ENTRY_LEN as u64 {
            return Err(malformed(
                segment,
                "the fragment index does not match the unit table",
            ));
        }
        let mut terms = Vec::with_capacity(TermKind::ALL.len());
        let (pairs, _) = handles[3..].as_chunks::<2>();
        for (kind, [lookup, postings]) in TermKind::ALL.into_iter().zip(pairs) {
            terms.push((kind, LookupTable::open(segment, *lookup)?, *postings));
        }
        Ok(Some(Self {
            segment,
            units,
            data: handles[1],
            index: handles[2],
            terms,
        }))
    }

    /// How many fragments the segment holds.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// The ordinal of the fragment with `key`.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn find(&self, key: FragmentKey) -> Result<Option<u32>, SegmentError> {
        Ok(self
            .units
            .get(key.0.as_bytes())?
            .map(|(ordinal, _)| ordinal))
    }

    /// The fragment key at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails.
    pub fn key(&self, ordinal: u32) -> Result<FragmentKey, SegmentError> {
        let (key, _) = self.units.entry(ordinal)?;
        let bytes = <[u8; Digest128::LEN]>::try_from(key.as_slice())
            .map_err(|_| malformed(self.segment, "a fragment key has the wrong width"))?;
        Ok(FragmentKey(Digest128::from_bytes(bytes)))
    }

    /// The bytes of the fragment at `ordinal`.
    ///
    /// # Errors
    ///
    /// When a read fails, a block fails its checksum, or the bytes do not
    /// unpack.
    pub fn fragment(&self, ordinal: u32) -> Result<Vec<u8>, SegmentError> {
        packed::read(self.segment, self.data, self.index, ordinal)
    }

    /// The ordinals of the fragments added with `term` of `kind`, ascending.
    /// May include fragments whose term only shares its hash.
    ///
    /// # Errors
    ///
    /// When a read fails or a posting block is malformed.
    pub fn candidates(&self, kind: TermKind, term: &[u8]) -> Result<Vec<u32>, SegmentError> {
        let (_, lookup, postings) = self
            .terms
            .iter()
            .find(|(each, ..)| *each == kind)
            .expect("every term kind has its sections");
        let Some(list) = lookup.get(term_hash(term))? else {
            return Ok(Vec::new());
        };
        // The count is stored, so it is checked against the bytes its blocks
        // would occupy before room is reserved for that many ordinals.
        if !fits(list, self.segment.info(*postings).len) {
            return Err(malformed(
                self.segment,
                "a term's posting count needs more bytes than its section holds",
            ));
        }
        let mut cursor = PostingCursor::new(self.segment, *postings, layout(), list);
        let mut ordinals = Vec::with_capacity(list.count as usize);
        while let Some(block) = cursor.next_block()? {
            ordinals.extend_from_slice(&block.ordinals);
        }
        Ok(ordinals)
    }
}

fn malformed(segment: &Segment, reason: &'static str) -> SegmentError {
    SegmentError::new(segment.path(), ErrorKind::Malformed(reason))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::cache::BlockCache;
    use crate::ids::{FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::{BLOCK_LEN, SegmentMeta};

    fn key(n: u8) -> FragmentKey {
        FragmentKey(Digest128::of(&[n]))
    }

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"repo"))
    }

    fn create(path: &Path) -> SegmentWriter {
        SegmentWriter::create(
            path,
            SegmentMeta {
                family: family(),
                segment: SegmentId::new(1),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap()
    }

    fn open(path: &Path) -> Segment {
        Segment::open(
            path,
            family(),
            &KNOWN,
            Arc::new(BlockCache::new(16 * BLOCK_LEN)),
        )
        .unwrap()
    }

    fn write(path: &Path, builder: Option<GraphSectionsBuilder>) -> Segment {
        let mut writer = create(path);
        if let Some(builder) = builder {
            builder.write(&mut writer).unwrap();
        }
        writer.finish().unwrap();
        open(path)
    }

    /// Graph sections holding one fragment and one call-target term, `t`,
    /// whose lookup entry says `list` while its postings hold one block with
    /// one ordinal. The segment's checksums are valid: only a writer could
    /// have stored such a list.
    fn write_forged(path: &Path, list: PostingList) -> Segment {
        let mut writer = create(path);
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        table.push(key(1).0.as_bytes(), UnitLocation { offset: 0, len: 0 });
        let (table, units) = table.finish();
        writer
            .write_section(spec(UNITS, UNIT_KEY_SPACE), &table, units)
            .unwrap();
        let mut data = PackedBuilder::with_capacity(1);
        data.push(b"one");
        let (data, index) = data.finish();
        writer
            .write_section(spec(DATA, UNIT_KEY_SPACE), &data, units)
            .unwrap();
        writer
            .write_section(spec(INDEX, UNIT_KEY_SPACE), &index, units)
            .unwrap();
        for kind in TermKind::ALL {
            let mut postings = PostingsBuilder::new(layout());
            let mut lookup = LookupTableBuilder::new();
            if kind == TermKind::CallTarget {
                postings.add_list(&[0], &[]);
                lookup.push(term_hash(b"t"), list);
            }
            let (lookup, terms) = lookup.finish();
            writer
                .write_section(spec(TERM_LOOKUP, kind.key_space()), &lookup, terms)
                .unwrap();
            let (postings, entries) = postings.finish();
            writer
                .write_section(spec(TERM_POSTINGS, kind.key_space()), &postings, entries)
                .unwrap();
        }
        writer.finish().unwrap();
        open(path)
    }

    #[test]
    fn fragments_and_terms_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = GraphSectionsBuilder::new();
        let repeated = b"abcabcabc".repeat(100);
        builder.add(
            key(1),
            repeated.clone(),
            [
                (TermKind::CallTarget, b"lib.rs\0run".as_slice()),
                (TermKind::Language, b"rust".as_slice()),
            ],
        );
        builder.add(
            key(2),
            b"two".to_vec(),
            [
                (TermKind::CallTarget, b"lib.rs\0run".as_slice()),
                (TermKind::ReferenceName, b"run".as_slice()),
                (TermKind::Language, b"rust".as_slice()),
            ],
        );
        builder.add(key(3), Vec::new(), []);
        let segment = write(&dir.path().join("graph.seg"), Some(builder));
        let graph = GraphSegment::open(&segment).unwrap().unwrap();
        assert_eq!(graph.len(), 3);
        let one = graph.find(key(1)).unwrap().unwrap();
        let two = graph.find(key(2)).unwrap().unwrap();
        let three = graph.find(key(3)).unwrap().unwrap();
        assert_eq!(graph.key(one).unwrap(), key(1));
        assert_eq!(graph.fragment(one).unwrap(), repeated);
        assert_eq!(graph.fragment(two).unwrap(), b"two");
        assert!(graph.fragment(three).unwrap().is_empty());
        let mut both = vec![one, two];
        both.sort_unstable();
        assert_eq!(
            graph
                .candidates(TermKind::CallTarget, b"lib.rs\0run")
                .unwrap(),
            both
        );
        assert_eq!(graph.candidates(TermKind::Language, b"rust").unwrap(), both);
        assert_eq!(
            graph.candidates(TermKind::ReferenceName, b"run").unwrap(),
            vec![two]
        );
        // The same term under another kind is a different term.
        assert!(
            graph
                .candidates(TermKind::SymbolName, b"run")
                .unwrap()
                .is_empty()
        );
        assert!(
            graph
                .candidates(TermKind::CallTarget, b"lib.rs\0other")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_segment_without_graph_sections_has_no_graph() {
        let dir = tempfile::tempdir().unwrap();
        let segment = write(&dir.path().join("empty.seg"), None);
        assert!(GraphSegment::open(&segment).unwrap().is_none());
    }

    #[test]
    fn a_posting_count_its_section_cannot_hold_is_refused_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        // The honest list: the forged segment is otherwise well formed.
        let segment = write_forged(
            &dir.path().join("honest.seg"),
            PostingList {
                offset: 0,
                count: 1,
            },
        );
        let graph = GraphSegment::open(&segment).unwrap().unwrap();
        assert_eq!(graph.candidates(TermKind::CallTarget, b"t").unwrap(), [0]);
        for (name, list) in [
            // 2^32 - 1 ordinals, 16 GiB, from a 12-byte section.
            (
                "huge",
                PostingList {
                    offset: 0,
                    count: u32::MAX,
                },
            ),
            // One entry more than a block holds needs a second block.
            (
                "two blocks",
                PostingList {
                    offset: 0,
                    count: u32::try_from(BLOCK_ENTRIES).unwrap() + 1,
                },
            ),
            // An offset whose end does not fit in 64 bits.
            (
                "overflow",
                PostingList {
                    offset: u64::MAX - 4,
                    count: 1,
                },
            ),
        ] {
            let segment = write_forged(&dir.path().join(format!("{name}.seg")), list);
            let graph = GraphSegment::open(&segment).unwrap().unwrap();
            let error = graph.candidates(TermKind::CallTarget, b"t").unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    ErrorKind::Malformed(
                        "a term's posting count needs more bytes than its section holds"
                    )
                ),
                "{name}: {error}"
            );
            assert!(error.is_corruption(), "{name}");
        }
    }

    #[test]
    fn the_smallest_block_is_the_bound_the_count_check_assumes() {
        // A full block of equal ordinals has zero-width deltas: the shortest
        // block the layout can store. If the format ever stored less, the
        // count check would refuse real lists.
        let mut postings = PostingsBuilder::new(layout());
        let list = postings.add_list(&[7; BLOCK_ENTRIES], &[]);
        let (bytes, _) = postings.finish();
        assert_eq!(bytes.len() as u64, MIN_POSTING_BLOCK_LEN);
        assert!(fits(list, MIN_POSTING_BLOCK_LEN));
        assert!(!fits(list, MIN_POSTING_BLOCK_LEN - 1));
    }
}
