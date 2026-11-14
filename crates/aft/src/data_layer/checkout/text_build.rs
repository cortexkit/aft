//! Bounded posting batches for both checkout text-building scans. Only content keys and
//! source ordinals survive a spill; resolution never holds a posting builder.

use super::*;
use aft_store::family::BuildScratch;
use aft_store::ids::{SegmentId, WriterIncarnation};
use aft_store::segment::{SegmentMeta, SegmentWriter};

/// Caps retained postings and unit overhead, not source bytes. One content
/// may exceed the cap; it is flushed immediately, so the overshoot is at
/// most one indexed content (itself bounded by the scan's file-size limit).
const BATCH_BYTES: usize = 32 * 1024 * 1024;
const UNIT_BYTES: usize = 128;

pub(super) struct TextBuild {
    scratch: BuildScratch<DataError>,
    family: FamilyId,
    current: TextSectionsBuilder,
    held: usize,
    cap: usize,
    units: BTreeMap<ContentKey, Option<aft_store::text::TextCopy>>,
    runs: Vec<(std::path::PathBuf, u64)>,
}

impl TextBuild {
    pub(super) fn new(scratch: BuildScratch<DataError>, family: FamilyId) -> Self {
        Self {
            scratch,
            family,
            current: TextSectionsBuilder::new(),
            held: 0,
            cap: BATCH_BYTES,
            units: BTreeMap::new(),
            runs: Vec::new(),
        }
    }

    pub(super) fn add_indexed(
        &mut self,
        key: ContentKey,
        len: u64,
        trigrams: &ContentTrigrams,
    ) -> Result<(), DataError> {
        if self.units.contains_key(&key) {
            return Ok(());
        }
        self.units.insert(key, None);
        self.held += UNIT_BYTES + std::mem::size_of_val(trigrams.entries());
        self.current.add_indexed(key, len, trigrams);
        self.flush_if_full()
    }

    pub(super) fn add_unindexed(
        &mut self,
        key: ContentKey,
        len: u64,
        status: TextStatus,
    ) -> Result<(), DataError> {
        if self.units.contains_key(&key) {
            return Ok(());
        }
        self.units.insert(key, None);
        self.held += UNIT_BYTES;
        self.current.add_unindexed(key, len, status);
        self.flush_if_full()
    }

    fn flush_if_full(&mut self) -> Result<(), DataError> {
        if self.held >= self.cap {
            self.spill()?;
        }
        Ok(())
    }

    pub(super) fn len(&self) -> usize {
        self.units.len()
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(ContentKey) -> bool) {
        self.units.retain(|key, _| keep(*key));
    }

    /// Writes the last batch before graph resolution starts, even if it
    /// did not fill the cap. The remaining build state contains no postings.
    pub(super) fn spill(&mut self) -> Result<(), DataError> {
        if self.current.is_empty() {
            return Ok(());
        }
        // A singleton posting can need a lookup row and a block header.
        // Eight times the batch estimate reserves more than that encoding.
        self.scratch
            .check_space((self.held as u64).saturating_mul(8))?;
        let source = self.runs.len();
        let path = self.scratch.path().join(format!("text-{source}.seg"));
        let mut writer = SegmentWriter::create(
            &path,
            SegmentMeta {
                family: self.family,
                segment: SegmentId::new(source as u64 + 1),
                incarnation: WriterIncarnation::new(0),
                created_unix_ms: 0,
            },
        )
        .map_err(DataError::store)?;
        let mut ordinal = 0;
        self.current.retain(|key| {
            if let Some(copy) = self.units.get_mut(&key) {
                *copy = Some(aft_store::text::TextCopy::Stored { source, ordinal });
            }
            ordinal += 1;
            true
        });
        std::mem::take(&mut self.current)
            .write(&mut writer)
            .map_err(DataError::store)?;
        let summary = writer.finish().map_err(DataError::store)?;
        self.runs.push((path, summary.len));
        self.held = 0;
        Ok(())
    }

    pub(super) fn write(mut self, writer: &mut SegmentWriter) -> Result<(), DataError> {
        self.spill()?;
        self.scratch
            .check_space(self.runs.iter().map(|(_, len)| len).sum())?;
        // This cache is independent of the query cache: a sequential merge
        // must not leave every source run's posting blocks resident afterward.
        let cache = Arc::new(BlockCache::new(8 * 1024 * 1024));
        let opened = self
            .runs
            .iter()
            .map(|(path, _)| {
                Segment::open(
                    path,
                    self.family,
                    &aft_store::text::KNOWN,
                    Arc::clone(&cache),
                )
                .map_err(DataError::store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sources = opened
            .iter()
            .map(|segment| {
                TextSegment::open(segment)
                    .map_err(DataError::store)?
                    .ok_or_else(|| DataError::Store("a text spill has no text sections".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let units = self
            .units
            .iter()
            .map(|(key, copy)| (*key, copy.expect("every batch was spilled")))
            .collect::<Vec<_>>();
        aft_store::text::write_folded(writer, &sources, &units).map_err(DataError::store)
    }
}

impl aft_store::heap::HeapBytes for TextBuild {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.current.heap_bytes(seen)
            + aft_store::heap::btree_bytes(
                self.units.len(),
                std::mem::size_of::<(ContentKey, Option<aft_store::text::TextCopy>)>(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(dir: &Path, cap: usize) -> TextBuild {
        let family =
            Family::open(dir, FamilyId::from_digest(Digest128::of(b"text-build"))).unwrap();
        let scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(|_| Ok(()))
            .unwrap();
        let mut build = TextBuild::new(scratch, family.id());
        build.cap = cap;
        build
    }

    fn writer(path: &Path, family: FamilyId) -> SegmentWriter {
        SegmentWriter::create(
            path,
            SegmentMeta {
                family,
                segment: SegmentId::new(1),
                incarnation: WriterIncarnation::new(1),
                created_unix_ms: 0,
            },
        )
        .unwrap()
    }

    fn sections(path: &Path, family: FamilyId) -> Vec<Vec<u8>> {
        let segment = Segment::open(
            path,
            family,
            &aft_store::text::KNOWN,
            Arc::new(BlockCache::new(1 << 20)),
        )
        .unwrap();
        aft_store::text::KNOWN
            .iter()
            .map(|known| {
                let section = segment
                    .section(aft_store::segment::SectionId::new(
                        known.kind,
                        aft_store::segment::KeySpace::new(0),
                    ))
                    .unwrap();
                let mut bytes = vec![0; segment.info(section).len as usize];
                segment.read(section, 0, &mut bytes).unwrap();
                bytes
            })
            .collect()
    }

    #[test]
    fn text_build_many_flushes_are_byte_identical_in_any_input_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut bounded = build(&dir.path().join("family"), 2048);
        let scratch = bounded.scratch.path().to_path_buf();
        let family = bounded.family;
        let mut expected = TextSectionsBuilder::new();
        // More than two posting blocks for the common term, interleaved keys
        // across runs, different next-character masks, binary/oversize units,
        // and duplicate keys.
        for n in (0..401).rev() {
            let bytes = format!("common prefix fn item_{n}() {{ value_{n}; }}");
            let key = ContentKey::of(bytes.as_bytes());
            let trigrams = ContentTrigrams::of(bytes.as_bytes());
            if n % 17 == 0 {
                let status = if n % 2 == 0 {
                    TextStatus::Binary
                } else {
                    TextStatus::Oversize
                };
                bounded.add_unindexed(key, n, status).unwrap();
                expected.add_unindexed(key, n, status);
            } else {
                bounded
                    .add_indexed(key, bytes.len() as u64, &trigrams)
                    .unwrap();
                expected.add_indexed(key, bytes.len() as u64, &trigrams);
            }
            bounded
                .add_unindexed(key, 99999, TextStatus::Binary)
                .unwrap();
            assert!(bounded.held < bounded.cap);
        }
        assert!(bounded.runs.len() > 30);
        bounded.spill().unwrap();
        assert!(bounded.current.is_empty());
        assert_eq!(bounded.held, 0);
        let actual = dir.path().join("bounded.seg");
        let mut out = writer(&actual, family);
        bounded.write(&mut out).unwrap();
        out.finish().unwrap();
        assert!(!scratch.exists(), "successful merge removes every partial");
        let baseline = dir.path().join("baseline.seg");
        let mut out = writer(&baseline, family);
        expected.write(&mut out).unwrap();
        out.finish().unwrap();
        assert_eq!(sections(&actual, family), sections(&baseline, family));
    }

    #[test]
    fn text_build_retain_after_spill_and_empty_merge() {
        let dir = tempfile::tempdir().unwrap();
        for keep in [false, true] {
            let mut bounded = build(&dir.path().join(format!("family-{keep}")), 1);
            let family = bounded.family;
            let bytes = b"common postings retained only when requested";
            let key = ContentKey::of(bytes);
            let trigrams = ContentTrigrams::of(bytes);
            bounded
                .add_indexed(key, bytes.len() as u64, &trigrams)
                .unwrap();
            bounded.retain(|_| keep);
            let actual = dir.path().join(format!("actual-{keep}.seg"));
            let mut out = writer(&actual, family);
            bounded.write(&mut out).unwrap();
            out.finish().unwrap();
            let mut expected = TextSectionsBuilder::new();
            if keep {
                expected.add_indexed(key, bytes.len() as u64, &trigrams);
            }
            let baseline = dir.path().join(format!("expected-{keep}.seg"));
            let mut out = writer(&baseline, family);
            expected.write(&mut out).unwrap();
            out.finish().unwrap();
            assert_eq!(sections(&actual, family), sections(&baseline, family));
        }
    }

    #[test]
    fn text_build_spill_error_cleans_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut bounded = build(&dir.path().join("family"), 1);
        let scratch = bounded.scratch.path().to_path_buf();
        // Force create_new to refuse a run, rather than simulating success.
        std::fs::write(scratch.join("text-0.seg"), b"occupied").unwrap();
        assert!(bounded
            .add_unindexed(ContentKey::of(b"x"), 1, TextStatus::Binary)
            .is_err());
        drop(bounded);
        assert!(!scratch.exists());
    }

    #[test]
    fn text_build_disk_floor_reserves_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let family =
            Family::open(dir.path(), FamilyId::from_digest(Digest128::of(b"floor"))).unwrap();
        let path = family.dir().to_path_buf();
        let scratch = family
            .try_write()
            .unwrap()
            .unwrap()
            .create_scratch(move |bytes| Checkout::check_disk_space(&path, bytes))
            .unwrap();
        let mut bounded = TextBuild::new(scratch, family.id());
        bounded.cap = 1;
        let _free = crate::disk_floor::set_free_space_for_test(
            family.dir(),
            crate::disk_floor::MIN_FREE_BYTES + 1,
            10 * 1024 * 1024 * 1024,
        );
        let error = bounded
            .add_unindexed(ContentKey::of(b"x"), 1, TextStatus::Binary)
            .unwrap_err();
        assert!(matches!(error, DataError::DiskFloor(_)));
        assert_eq!(
            std::fs::read_dir(bounded.scratch.path()).unwrap().count(),
            1,
            "only the owner file exists"
        );
    }
}
