use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::cache::BlockCache;
use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
use crate::test_rng::TestRng;

const KIND_A: SectionKind = SectionKind::new(1);
const KIND_B: SectionKind = SectionKind::new(2);

fn family(name: &str) -> FamilyId {
    FamilyId::from_digest(Digest128::of(name.as_bytes()))
}

fn meta() -> SegmentMeta {
    SegmentMeta {
        family: family("repo"),
        segment: SegmentId::new(9),
        incarnation: WriterIncarnation::new(3),
        created_unix_ms: 1_793_700_000_000,
    }
}

fn spec(kind: SectionKind, key_space: u16, alignment: Alignment) -> SectionSpec {
    SectionSpec {
        id: SectionId::new(kind, KeySpace::new(key_space)),
        version: 1,
        alignment,
        requirement: Requirement::Required,
    }
}

fn known() -> Vec<KnownSection> {
    vec![
        KnownSection {
            kind: KIND_A,
            version: 1,
        },
        KnownSection {
            kind: KIND_B,
            version: 1,
        },
    ]
}

fn cache() -> Arc<BlockCache> {
    Arc::new(BlockCache::new(8 * BLOCK_LEN))
}

/// Section lengths around block boundaries, each filled with distinct bytes.
fn sample_sections() -> Vec<(SectionSpec, Vec<u8>)> {
    let mut rng = TestRng::new(1);
    [
        0,
        1,
        BLOCK_LEN - 1,
        BLOCK_LEN,
        BLOCK_LEN + 1,
        3 * BLOCK_LEN + 17,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, len)| {
        let alignment = if index % 2 == 0 {
            Alignment::Word
        } else {
            Alignment::Vector
        };
        let kind = if index % 3 == 0 { KIND_B } else { KIND_A };
        let key_space = u16::try_from(index).unwrap();
        (spec(kind, key_space, alignment), rng.bytes(len))
    })
    .collect()
}

fn write_segment(dir: &Path, sections: &[(SectionSpec, Vec<u8>)]) -> PathBuf {
    let path = dir.join("1.seg");
    let mut writer = SegmentWriter::create(&path, meta()).unwrap();
    for (spec, bytes) in sections {
        writer.begin_section(*spec).unwrap();
        // Written in uneven pieces, so block boundaries fall inside writes.
        for piece in bytes.chunks(10_007) {
            writer.write(piece).unwrap();
        }
        writer.end_section(bytes.len() as u64 / 3).unwrap();
    }
    let summary = writer.finish().unwrap();
    assert_eq!(summary.sections, sections.len());
    assert_eq!(summary.len, std::fs::metadata(&path).unwrap().len());
    path
}

#[test]
fn every_byte_reads_back_through_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
    assert_eq!(segment.id(), SegmentId::new(9));
    assert_eq!(segment.incarnation(), WriterIncarnation::new(3));
    segment.verify().unwrap();

    let mut rng = TestRng::new(2);
    for (spec, bytes) in &sections {
        let handle = segment.section(spec.id).unwrap();
        let info = segment.info(handle);
        assert_eq!(info.len, bytes.len() as u64);
        assert_eq!(info.item_count, bytes.len() as u64 / 3);
        assert_eq!(segment.read_range(handle, 0..info.len).unwrap(), *bytes);
        for _ in 0..50 {
            if bytes.is_empty() {
                break;
            }
            let start = rng.below_usize(bytes.len());
            let end = start + rng.below_usize(bytes.len() - start + 1);
            let read = segment
                .read_range(handle, start as u64..end as u64)
                .unwrap();
            assert_eq!(
                read,
                bytes[start..end],
                "range {start}..{end} of {}",
                spec.id
            );
        }
    }
}

#[test]
fn sections_start_on_their_alignment() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let file = std::fs::read(&path).unwrap();
    let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
    // The reader checked alignment at open; this confirms the bytes sit
    // where the table says by locating each non-empty section in the file.
    for (spec, bytes) in sections.iter().filter(|(_, bytes)| bytes.len() > 64) {
        let handle = segment.section(spec.id).unwrap();
        let first = segment.read_range(handle, 0..64).unwrap();
        let offset = file
            .windows(64)
            .position(|window| window == first.as_slice())
            .unwrap();
        assert_eq!(offset as u64 % spec.alignment.bytes(), 0, "{}", spec.id);
        assert_eq!(&file[offset..offset + bytes.len()], bytes.as_slice());
    }
}

#[test]
fn reads_past_a_section_end_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
    let handle = segment.section(sections[1].0.id).unwrap();
    let error = segment.read_range(handle, 0..2).unwrap_err();
    assert!(
        matches!(error.kind(), ErrorKind::OutOfRange { len: 1, .. }),
        "{error}"
    );
}

#[test]
fn a_changed_block_fails_only_reads_that_touch_it() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let (spec, bytes) = &sections[5];
    let target = {
        let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
        let handle = segment.section(spec.id).unwrap();
        let block_one = segment
            .read_range(handle, BLOCK_LEN as u64..BLOCK_LEN as u64 + 32)
            .unwrap();
        let file = std::fs::read(&path).unwrap();
        file.windows(32)
            .position(|window| window == block_one.as_slice())
            .unwrap()
    };
    let mut file = std::fs::read(&path).unwrap();
    file[target + 5] ^= 0x40;
    std::fs::write(&path, &file).unwrap();

    let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
    let handle = segment.section(spec.id).unwrap();
    assert_eq!(
        segment.read_range(handle, 0..100).unwrap(),
        bytes[..100],
        "block 0 is intact"
    );
    let error = segment
        .read_range(handle, BLOCK_LEN as u64..BLOCK_LEN as u64 + 10)
        .unwrap_err();
    assert!(error.is_corruption());
    assert!(
        matches!(
            error.kind(),
            ErrorKind::Corrupt(Region::Block { block: 1, .. })
        ),
        "{error}"
    );
    assert!(segment.verify().unwrap_err().is_corruption());
}

#[test]
fn a_segment_of_another_family_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_segment(dir.path(), &sample_sections());
    let error = Segment::open(&path, family("other"), &known(), cache()).unwrap_err();
    assert!(
        matches!(error.kind(), ErrorKind::FamilyMismatch { .. }),
        "{error}"
    );
}

#[test]
fn unknown_required_sections_refuse_and_unknown_optional_ones_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut sections = sample_sections();
    sections.push((
        SectionSpec {
            id: SectionId::new(SectionKind::new(77), KeySpace::NONE),
            version: 1,
            alignment: Alignment::Word,
            requirement: Requirement::Optional,
        },
        b"later plane".to_vec(),
    ));
    let path = write_segment(dir.path(), &sections);
    let segment = Segment::open(&path, family("repo"), &known(), cache()).unwrap();
    assert_eq!(segment.sections().count(), sections.len() - 1);
    assert!(
        segment
            .section(SectionId::new(SectionKind::new(77), KeySpace::NONE))
            .is_none()
    );

    // A reader that knows kind A only at version 2 cannot use version 1.
    let only_newer = [KnownSection {
        kind: KIND_A,
        version: 2,
    }];
    let error = Segment::open(&path, family("repo"), &only_newer, cache()).unwrap_err();
    assert!(
        matches!(
            error.kind(),
            ErrorKind::UnknownRequiredSection { version: 1, .. }
        ),
        "{error}"
    );
}

#[test]
fn an_unfinished_segment_is_not_a_segment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.seg");
    let mut writer = SegmentWriter::create(&path, meta()).unwrap();
    writer
        .write_section(spec(KIND_A, 0, Alignment::Word), &[7; 5000], 1)
        .unwrap();
    // The writer is dropped without finishing, as a crash would leave it.
    drop(writer);
    let error = Segment::open(&path, family("repo"), &known(), cache()).unwrap_err();
    assert!(matches!(error.kind(), ErrorKind::NotASegment), "{error}");
}

#[test]
fn writers_refuse_misuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("misuse.seg");
    std::fs::write(&path, b"existing").unwrap();
    assert!(matches!(
        SegmentWriter::create(&path, meta()).unwrap_err().kind(),
        ErrorKind::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists
    ));

    let path = dir.path().join("fresh.seg");
    let mut writer = SegmentWriter::create(&path, meta()).unwrap();
    assert!(writer.write(b"loose").is_err(), "bytes outside a section");
    let a = spec(KIND_A, 0, Alignment::Word);
    writer.write_section(a, b"one", 1).unwrap();
    assert!(matches!(
        writer.write_section(a, b"two", 1).unwrap_err().kind(),
        ErrorKind::DuplicateSection(_)
    ));
    writer
        .begin_section(spec(KIND_B, 0, Alignment::Word))
        .unwrap();
    assert!(matches!(
        writer.finish().unwrap_err().kind(),
        ErrorKind::UnfinishedSection(_)
    ));
}

#[test]
fn closing_a_segment_releases_its_cached_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let shared = cache();
    let segment = Segment::open(&path, family("repo"), &known(), Arc::clone(&shared)).unwrap();
    let handle = segment.section(sections[5].0.id).unwrap();
    segment
        .read_range(handle, 0..segment.info(handle).len)
        .unwrap();
    assert_eq!(shared.stats().blocks, 4);
    drop(segment);
    assert_eq!(shared.stats().blocks, 0);
    assert_eq!(shared.stats().resident_bytes, 0);
}

/// Damage anywhere in a segment must never panic, and must never let a read
/// return bytes other than the ones written: either the read fails or the
/// damage was in bytes no read uses, such as alignment padding.
#[test]
fn damaged_files_fail_cleanly_or_read_the_written_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let sections = sample_sections();
    let path = write_segment(dir.path(), &sections);
    let original = std::fs::read(&path).unwrap();
    let mut rng = TestRng::new(3);
    let (mut opened, mut refused_reads) = (0, 0);
    for round in 0..300 {
        let mut damaged = original.clone();
        match round % 3 {
            0 => {
                let at = rng.below_usize(damaged.len());
                damaged[at] ^= 1 << rng.below(8);
            }
            1 => damaged.truncate(rng.below_usize(damaged.len())),
            _ => {
                let at = rng.below_usize(damaged.len());
                let len = rng.below_usize(64).min(damaged.len() - at);
                damaged[at..at + len].fill(0);
            }
        }
        let damaged_path = dir.path().join(format!("damaged-{round}.seg"));
        std::fs::write(&damaged_path, &damaged).unwrap();
        let Ok(segment) = Segment::open(&damaged_path, family("repo"), &known(), cache()) else {
            continue;
        };
        opened += 1;
        for (spec, bytes) in &sections {
            let Some(handle) = segment.section(spec.id) else {
                continue;
            };
            match segment.read_range(handle, 0..bytes.len() as u64) {
                Ok(read) => {
                    assert_eq!(read, *bytes, "round {round}: a read returned changed bytes");
                }
                Err(error) => {
                    assert!(error.is_corruption(), "round {round}: {error}");
                    refused_reads += 1;
                }
            }
        }
    }
    // Damage inside section bytes leaves the tables intact, so some damaged
    // files open and only their reads fail; both paths must have run.
    assert!(
        opened > 0 && refused_reads > 0,
        "opened {opened}, refused reads {refused_reads}"
    );
}

/// The segment format version and the required sections it was pinned with
/// last; see [`FORMAT_VERSION`]. Each entry is a section kind and its
/// version, from the text, content, call-graph and vector planes.
const PINNED_FORMAT_VERSION: u16 = 1;
const PINNED_REQUIRED_SECTIONS: [(u16, u16); 22] = [
    // Text.
    (0x0100, 1),
    (0x0101, 1),
    (0x0102, 1),
    (0x0103, 1),
    // Content.
    (0x0200, 1),
    (0x0201, 1),
    (0x0202, 1),
    (0x0203, 1),
    // Call graph.
    (0x0300, 4),
    (0x0301, 4),
    (0x0302, 4),
    (0x0303, 4),
    (0x0304, 4),
    // Vectors.
    (0x0400, 1),
    (0x0401, 1),
    (0x0402, 1),
    (0x0403, 1),
    (0x0404, 1),
    (0x0405, 1),
    (0x0406, 1),
    (0x0407, 1),
    (0x0408, 1),
];

/// Every section kind the planes read, with its version. Each plane writes
/// all of its sections as required, so a build that does not know one of
/// them cannot open a segment holding it.
fn required_sections() -> std::collections::BTreeSet<(u16, u16)> {
    crate::text::KNOWN
        .iter()
        .chain(&crate::content::KNOWN)
        .chain(&crate::graph::KNOWN)
        .chain(&crate::vector::KNOWN)
        .map(|known| (known.kind.get(), known.version))
        .collect()
}

/// An older build must meet a newer segment format version, which it
/// refuses, rather than a required section it does not know, which makes
/// the segment unusable, so it is built over and reclaimed. So the
/// required sections may only change together with [`FORMAT_VERSION`].
#[test]
fn the_segment_format_version_moves_with_the_required_sections() {
    let sections = required_sections();
    let pinned = PINNED_REQUIRED_SECTIONS
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        FORMAT_VERSION != PINNED_FORMAT_VERSION || sections == pinned,
        "the required sections changed (added {:?}, removed {:?}) while the segment format \
         version stayed {FORMAT_VERSION}: bump FORMAT_VERSION in segment/format.rs, so an older \
         build refuses these segments by their version instead of meeting a required section \
         it does not know and rebuilding over them. Then pin the new version and sections \
         here. Only an optional section, which an older build skips, needs no bump.",
        sections.difference(&pinned).collect::<Vec<_>>(),
        pinned.difference(&sections).collect::<Vec<_>>()
    );
    assert!(
        FORMAT_VERSION == PINNED_FORMAT_VERSION && sections == pinned,
        "the segment format version is {FORMAT_VERSION} but this guard pins version \
         {PINNED_FORMAT_VERSION}: pin the new version here together with the required sections \
         it holds, {sections:?}"
    );
}
