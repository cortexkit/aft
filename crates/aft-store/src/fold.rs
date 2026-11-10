//! Folding: copying chosen units of several segments into a new one.
//!
//! A checkout that has published many small change segments reads its
//! units from all of them. Folding writes the units it reads into one new
//! segment, from the bytes the old segments store: nothing is extracted,
//! parsed or resolved again. Each plane writes its own sections (see
//! `text::write_folded`, `content::write_folded`, `graph::write_folded`);
//! this module holds what they share, the merge of posting lists.
//!
//! A folded segment's sections are the same bytes a builder writes for the
//! same units: units in ascending key order, so each takes the ordinal a
//! builder gives it, and each term's posting list holds the same ordinals
//! and payloads in the same blocks. Only the order of the sections in the
//! file differs, which readers do not depend on.
//!
//! Memory stays bounded by what the new segment indexes, not by what it
//! holds: unit tables, the posting-list index of each term, and one term's
//! posting list at a time. Unit bytes and posting blocks go to the file as
//! they are copied.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::postings::{PostingCursor, PostingLayout, PostingList, PostingsBuilder};
use crate::segment::{SectionHandle, SectionSpec, Segment, SegmentError, SegmentWriter};
use crate::table::{LookupTable, LookupTableBuilder};

/// Posting bytes held before they are written to the file.
const FLUSH_AT: usize = 1 << 20;

/// An ordinal no unit of the new segment takes: the old unit is not copied.
pub(crate) const NOT_COPIED: u32 = u32::MAX;

/// One old segment's term postings, and the new ordinal of each of its
/// units ([`NOT_COPIED`] for one left behind).
pub(crate) struct PostingSource<'s> {
    pub(crate) segment: &'s Segment,
    pub(crate) lookup: LookupTable<'s>,
    pub(crate) postings: SectionHandle,
    pub(crate) renumbered: &'s [u32],
}

/// Writes a postings section and then its lookup section, holding for each
/// term of the `sources`' lookup tables, in ascending order, the postings
/// of the copied units, renumbered. A term none of whose units is copied is
/// left out, as a builder never writes an empty list. `layout` has at most
/// one payload column, as the text and graph postings do.
///
/// # Errors
///
/// When a read or a write fails, or a posting block is malformed.
pub(crate) fn write_postings(
    writer: &mut SegmentWriter,
    postings_spec: SectionSpec,
    lookup_spec: SectionSpec,
    layout: &PostingLayout,
    sources: &[PostingSource<'_>],
) -> Result<(), SegmentError> {
    let columns = layout.columns().len();
    assert!(columns <= 1, "a fold merges postings of at most one column");
    // Each source's next lookup entry, smallest term first. A source none
    // of whose units is copied adds nothing and is not read.
    let mut next = BinaryHeap::new();
    let mut entries: Vec<(u64, Option<PostingList>)> = vec![(0, None); sources.len()];
    for (index, source) in sources.iter().enumerate() {
        if source.lookup.is_empty() || source.renumbered.iter().all(|&new| new == NOT_COPIED) {
            continue;
        }
        let (term, list) = source.lookup.entry(0)?;
        entries[index] = (1, Some(list));
        next.push(Reverse((term, index)));
    }
    let mut postings = PostingsBuilder::new(layout.clone());
    let mut lookup = LookupTableBuilder::new();
    let mut merged = Vec::new();
    writer.begin_section(postings_spec)?;
    while let Some(Reverse((term, _))) = next.peek().copied() {
        merged.clear();
        while let Some(Reverse((same, index))) = next.peek().copied() {
            if same != term {
                break;
            }
            next.pop();
            let source = &sources[index];
            let list = entries[index]
                .1
                .take()
                .expect("a queued source has its next list");
            copy_list(source, layout, list, &mut merged)?;
            let read = entries[index].0;
            if read < source.lookup.len() {
                let (term, list) = source.lookup.entry(read)?;
                entries[index] = (read + 1, Some(list));
                next.push(Reverse((term, index)));
            }
        }
        if merged.is_empty() {
            continue;
        }
        // Each source's renumbered ordinals ascend, and no two sources give
        // the same one, so a stable sort keeps any repeats in their order.
        merged.sort_by_key(|(ordinal, _)| *ordinal);
        let ordinals = merged
            .iter()
            .map(|(ordinal, _)| *ordinal)
            .collect::<Vec<_>>();
        let values = merged.iter().map(|(_, value)| *value).collect::<Vec<_>>();
        let list = if columns == 0 {
            postings.add_list(&ordinals, &[])
        } else {
            postings.add_list(&ordinals, &[&values])
        };
        lookup.push(term, list);
        if postings.held() >= FLUSH_AT {
            writer.write(&postings.drain())?;
        }
    }
    let (rest, count) = postings.finish();
    writer.write(&rest)?;
    writer.end_section(count)?;
    let (lookup, terms) = lookup.finish();
    writer.write_section(lookup_spec, &lookup, terms)
}

/// Appends to `merged` each posting of `list` in `source` whose unit is
/// copied, with its new ordinal and its payload value (zero without one).
fn copy_list(
    source: &PostingSource<'_>,
    layout: &PostingLayout,
    list: PostingList,
    merged: &mut Vec<(u32, u32)>,
) -> Result<(), SegmentError> {
    let mut cursor = PostingCursor::new(source.segment, source.postings, layout.clone(), list);
    while let Some(block) = cursor.next_block()? {
        for (position, ordinal) in block.ordinals.iter().enumerate() {
            let Some(&new) = source.renumbered.get(*ordinal as usize) else {
                return Err(SegmentError::new(
                    source.segment.path(),
                    crate::segment::ErrorKind::Malformed("a posting names a unit past the table"),
                ));
            };
            if new == NOT_COPIED {
                continue;
            }
            let value = block.columns.first().map_or(0, |column| column[position]);
            merged.push((new, value));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::cache::BlockCache;
    use crate::content::{ContentSectionsBuilder, ContentSegment, EncodedRecord, RecordKey};
    use crate::graph::{FragmentKey, GraphSectionsBuilder, GraphSegment, TermKind};
    use crate::ids::{ContentKey, Digest128, FamilyId, SegmentId, WriterIncarnation};
    use crate::segment::{KnownSection, Segment, SegmentMeta, SegmentWriter};
    use crate::test_rng::TestRng;
    use crate::text::{ContentTrigrams, TextCopy, TextSectionsBuilder, TextSegment, TextStatus};

    fn family() -> FamilyId {
        FamilyId::from_digest(Digest128::of(b"fold"))
    }

    fn known() -> Vec<KnownSection> {
        crate::text::KNOWN
            .into_iter()
            .chain(crate::content::KNOWN)
            .chain(crate::graph::KNOWN)
            .chain(crate::graph::KNOWN_OPTIONAL)
            .collect()
    }

    fn create(dir: &Path, id: u64) -> (PathBuf, SegmentWriter) {
        let path = dir.join(format!("{id}.seg"));
        let writer = SegmentWriter::create(
            &path,
            SegmentMeta {
                family: family(),
                segment: SegmentId::new(id),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 1_793_700_000_000,
            },
        )
        .unwrap();
        (path, writer)
    }

    fn open(path: &Path) -> Segment {
        Segment::open(path, family(), &known(), Arc::new(BlockCache::new(1 << 20))).unwrap()
    }

    /// The units of one test plane, by number: what each plane's builder is
    /// given for unit `n`. A unit has the same bytes in every segment that
    /// holds it, as content-keyed units do.
    struct Units {
        texts: Vec<Vec<u8>>,
        surfaces: Vec<Vec<u8>>,
        bodies: Vec<Vec<u8>>,
        fragments: Vec<Vec<u8>>,
        slices: Vec<Vec<u8>>,
        terms: Vec<Vec<(TermKind, Vec<u8>)>>,
    }

    impl Units {
        fn new(count: usize) -> Self {
            let mut rng = TestRng::new(7);
            let words = ["alpha", "beta", "gamma", "delta", "callee", "caller"];
            let mut units = Self {
                texts: Vec::new(),
                surfaces: Vec::new(),
                bodies: Vec::new(),
                fragments: Vec::new(),
                slices: Vec::new(),
                terms: Vec::new(),
            };
            for n in 0..count {
                let text = format!(
                    "fn {}_{n}() {{ {}(); }} // {}\n",
                    words[n % words.len()],
                    words[(n + 1) % words.len()],
                    "x".repeat(n * 7)
                );
                units.texts.push(text.into_bytes());
                units.surfaces.push(rng.bytes(16 + n * 3));
                // Some bodies compress, some do not, so both codecs fold.
                units.bodies.push(if n % 2 == 0 {
                    vec![u8::try_from(n).unwrap(); 600 + n]
                } else {
                    rng.bytes(40 + n)
                });
                units.fragments.push(rng.bytes(32 + n));
                units.slices.push(if n % 3 == 0 {
                    Vec::new()
                } else {
                    vec![b's'; 300 + n]
                });
                units.terms.push(vec![
                    (TermKind::Language, b"rust".to_vec()),
                    (
                        TermKind::SymbolName,
                        words[n % words.len()].as_bytes().to_vec(),
                    ),
                    (TermKind::CallTarget, format!("f{}", n / 2).into_bytes()),
                    (TermKind::FactSubject, format!("dir{}", n % 4).into_bytes()),
                ]);
            }
            units
        }

        fn text_key(n: usize) -> ContentKey {
            ContentKey::from_digest(Digest128::of(format!("text {n}").as_bytes()))
        }

        fn record_key(n: usize) -> RecordKey {
            RecordKey::from_digest(Digest128::of(format!("record {n}").as_bytes()))
        }

        fn fragment_key(n: usize) -> FragmentKey {
            FragmentKey::from_digest(Digest128::of(format!("fragment {n}").as_bytes()))
        }

        /// Writes a segment holding units `numbers` with the builders.
        fn build(&self, dir: &Path, id: u64, numbers: &[usize]) -> PathBuf {
            let mut text = TextSectionsBuilder::new();
            let mut content = ContentSectionsBuilder::new();
            let mut graph = GraphSectionsBuilder::new();
            for &n in numbers {
                if n % 5 == 4 {
                    // A content too large to index has a unit and no postings.
                    text.add_unindexed(
                        Self::text_key(n),
                        self.texts[n].len() as u64,
                        TextStatus::Oversize,
                    );
                } else {
                    text.add_indexed(
                        Self::text_key(n),
                        self.texts[n].len() as u64,
                        &ContentTrigrams::of(&self.texts[n]),
                    );
                }
                content.add_encoded(
                    Self::record_key(n),
                    Arc::new(EncodedRecord {
                        surface: self.surfaces[n].clone(),
                        body: self.bodies[n].clone(),
                    }),
                );
                graph.add_with_slice(
                    Self::fragment_key(n),
                    self.fragments[n].clone(),
                    self.slices[n].clone(),
                    self.terms[n]
                        .iter()
                        .map(|(kind, term)| (*kind, term.as_slice())),
                );
            }
            let (path, mut writer) = create(dir, id);
            text.write(&mut writer).unwrap();
            content.write(&mut writer).unwrap();
            graph.write(&mut writer).unwrap();
            writer.finish().unwrap();
            path
        }
    }

    /// Every section of `segment`: its id, version, item count and bytes,
    /// sorted by id.
    fn sections(segment: &Segment) -> Vec<(String, u16, u64, Vec<u8>)> {
        let mut sections = segment
            .sections()
            .map(|(handle, info)| {
                (
                    format!("{:?}", info.id),
                    info.version,
                    info.item_count,
                    segment.read_range(handle, 0..info.len).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        sections.sort();
        sections
    }

    /// Folding units out of two segments that share some of them writes the
    /// very sections the builders write for the same units, whichever
    /// segment each unit is copied from: the same unit tables, unit bytes,
    /// compressed bodies, slice records and posting lists.
    #[test]
    fn a_folded_segment_holds_the_sections_a_build_of_its_units_writes() {
        let dir = tempfile::tempdir().unwrap();
        let units = Units::new(40);
        let first = (0..30).collect::<Vec<_>>();
        let second = (20..40).collect::<Vec<_>>();
        let first_path = units.build(dir.path(), 1, &first);
        let second_path = units.build(dir.path(), 2, &second);
        // Every third unit of the first segment and every unit past 25,
        // shared ones taken from the second, as a newest holder would be.
        let wanted = (0..40)
            .filter(|n| n % 3 == 0 || *n > 25)
            .collect::<Vec<_>>();
        let from = |n: usize| usize::from(n >= 20);
        let expected_path = units.build(dir.path(), 3, &wanted);

        let sources = [open(&first_path), open(&second_path)];
        let texts = sources
            .iter()
            .map(|segment| TextSegment::open(segment).unwrap().unwrap())
            .collect::<Vec<_>>();
        let contents = sources
            .iter()
            .map(|segment| ContentSegment::open(segment).unwrap().unwrap())
            .collect::<Vec<_>>();
        let graphs = sources
            .iter()
            .map(|segment| GraphSegment::open(segment).unwrap().unwrap())
            .collect::<Vec<_>>();
        let text_keys = texts
            .iter()
            .map(|text| text.keys().unwrap())
            .collect::<Vec<_>>();
        let mut text_units = Vec::new();
        let mut record_units = Vec::new();
        let mut fragment_units = Vec::new();
        for &n in &wanted {
            let source = from(n);
            let at = text_keys[source]
                .iter()
                .position(|key| *key == Units::text_key(n))
                .unwrap();
            let at = u32::try_from(at).unwrap();
            let unit = texts[source].unit(at).unwrap();
            text_units.push((
                Units::text_key(n),
                if unit.status == TextStatus::Indexed {
                    TextCopy::Stored {
                        source,
                        ordinal: at,
                    }
                } else {
                    TextCopy::Unindexed {
                        len: unit.len,
                        status: unit.status,
                    }
                },
            ));
            let record = Units::record_key(n);
            let at = contents[source].find(record).unwrap().unwrap();
            record_units.push((record, source, at));
            let fragment = Units::fragment_key(n);
            let at = graphs[source].find(fragment).unwrap().unwrap();
            fragment_units.push((fragment, source, at));
        }
        text_units.sort_by_key(|(key, _)| *key);
        record_units.sort_by_key(|(key, ..)| *key);
        fragment_units.sort_by_key(|(key, ..)| *key);
        let (folded_path, mut writer) = create(dir.path(), 4);
        crate::text::write_folded(&mut writer, &texts, &text_units).unwrap();
        crate::content::write_folded(&mut writer, &contents, &record_units).unwrap();
        crate::graph::write_folded(&mut writer, &graphs, &fragment_units).unwrap();
        writer.finish().unwrap();

        let folded = sections(&open(&folded_path));
        let expected = sections(&open(&expected_path));
        assert_eq!(folded.len(), expected.len());
        for (folded, expected) in folded.iter().zip(&expected) {
            assert_eq!(
                (&folded.0, folded.1, folded.2),
                (&expected.0, expected.1, expected.2)
            );
            assert!(folded.3 == expected.3, "section {} differs", folded.0);
        }
    }
}
