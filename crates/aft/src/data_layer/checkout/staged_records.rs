//! A cold build's content records, staged on disk as they are extracted.
//!
//! The scan appends each record it extracts to a staging segment in the
//! build workspace's `records/` folder ([`RecordStage`]) and keeps only
//! where it lies. After the scan the staging segments are finished and
//! opened ([`StagedRecords`]): the resolver decodes each parse from there
//! when it reads it, through the process's block cache, and the published
//! content sections are copied from there in key order. So no stage holds
//! every record in memory.
//!
//! A staged record that cannot be read back fails the build: the read is
//! recorded, and the build checks for recorded failures at each stage
//! boundary and before it publishes.

use std::sync::{Arc, Mutex};

use aft_store::cache::BlockCache;
use aft_store::content::{
    ContentRecord, EncodedRecord, PackedRecord, RecordKey, Stage, StageWriter, StagedUnit,
};
use aft_store::family::ScratchDir;
use aft_store::ids::FamilyId;

use super::DataError;

/// A staging segment is finished and a new one started past this many
/// bytes, well below the largest segment a file may hold.
const SEGMENT_BYTES: u64 = 4 << 30;

/// Disk admission is asked for this many bytes at a time, before the
/// appends that use them.
const ADMIT_BYTES: u64 = 64 << 20;

/// The staging segment numbered `number` in `dir`.
fn stage_path(dir: &ScratchDir<DataError>, number: usize) -> std::path::PathBuf {
    dir.path().join(format!("records-{}", number + 1))
}

/// Where one staged record lies: its staging segment and its place there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagedAt {
    stage: u32,
    unit: StagedUnit,
}

/// The staging segments a scan appends to.
pub(crate) struct RecordStage {
    dir: ScratchDir<DataError>,
    family: FamilyId,
    out: Mutex<StageOut>,
}

struct StageOut {
    writer: Option<StageWriter>,
    /// Finished staging segments, in number order.
    finished: usize,
    /// Bytes appended, and bytes disk admission allowed so far.
    written: u64,
    admitted: u64,
}

impl RecordStage {
    /// Stages records in `dir`, the build workspace's own folder for them.
    pub(crate) fn new(dir: ScratchDir<DataError>, family: FamilyId) -> Self {
        Self {
            dir,
            family,
            out: Mutex::new(StageOut {
                writer: None,
                finished: 0,
                written: 0,
                admitted: 0,
            }),
        }
    }

    /// Appends `record`, encoded and packed on the calling thread, and
    /// returns where it lies. Disk admission is asked before the bytes it
    /// has not yet allowed are written.
    ///
    /// # Errors
    ///
    /// When admission refuses, or the write fails.
    pub(crate) fn append(&self, record: &ContentRecord) -> Result<StagedAt, DataError> {
        let packed = PackedRecord::of(record);
        let mut out = self.out.lock().expect("no panics while holding it");
        if out.written + packed.len() > out.admitted {
            let more = packed.len().max(ADMIT_BYTES);
            self.dir.workspace().check_space(more)?;
            out.admitted += more;
        }
        let full = out.writer.as_ref().is_some_and(|writer| {
            !writer.is_empty() && writer.len() + packed.len() > SEGMENT_BYTES
        });
        if full {
            let writer = out.writer.take().expect("checked above");
            writer.finish().map_err(DataError::store)?;
            out.finished += 1;
        }
        if out.writer.is_none() {
            let path = stage_path(&self.dir, out.finished);
            out.writer = Some(StageWriter::create(&path, self.family).map_err(DataError::store)?);
        }
        let unit = out
            .writer
            .as_mut()
            .expect("made above")
            .append(&packed)
            .map_err(DataError::store)?;
        out.written += packed.len();
        Ok(StagedAt {
            stage: u32::try_from(out.finished).expect("fewer than 4 billion staging segments"),
            unit,
        })
    }

    /// Finishes the staging segments and opens them for reading through
    /// `cache`.
    ///
    /// # Errors
    ///
    /// When a staging segment cannot be finished or opened.
    pub(crate) fn finish(self, cache: &Arc<BlockCache>) -> Result<StagedRecords, DataError> {
        let Self { dir, family, out } = self;
        let mut out = out.into_inner().expect("no panics while holding it");
        let mut count = out.finished;
        if let Some(writer) = out.writer.take() {
            writer.finish().map_err(DataError::store)?;
            count += 1;
        }
        let stages = (0..count)
            .map(|number| {
                Stage::open(&stage_path(&dir, number), family, Arc::clone(cache))
                    .map_err(DataError::store)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StagedRecords {
            _dir: dir,
            stages,
            bytes: out.written,
            failures: Mutex::default(),
        })
    }
}

/// The finished staging segments of a build, read by location.
pub(crate) struct StagedRecords {
    /// Held so the workspace, and so the files, stay while they are read.
    _dir: ScratchDir<DataError>,
    stages: Vec<Stage>,
    bytes: u64,
    /// Every staged read that failed, as an error message.
    failures: Mutex<Vec<String>>,
}

impl std::fmt::Debug for StagedRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedRecords")
            .field("stages", &self.stages.len())
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl StagedRecords {
    /// The record at `at`, as the content sections encode it. A read that
    /// fails is recorded (see [`Self::check`]) as well as returned.
    pub(crate) fn encoded(&self, at: StagedAt, what: &str) -> Option<EncodedRecord> {
        let read = self
            .stages
            .get(at.stage as usize)
            .ok_or_else(|| format!("no staging segment {}", at.stage))
            .and_then(|stage| stage.encoded(at.unit).map_err(|error| error.to_string()));
        match read {
            Ok(record) => Some(record),
            Err(error) => {
                self.note_failure(format!("the staged record of {what}: {error}"));
                None
            }
        }
    }

    /// Records that reading the staged record of something failed.
    pub(crate) fn note_failure(&self, failure: String) {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(failure);
    }

    /// Fails when any staged read failed so far.
    ///
    /// # Errors
    ///
    /// The first failure, with how many there were.
    pub(crate) fn check(&self) -> Result<(), String> {
        let failures = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match failures.first() {
            None => Ok(()),
            Some(first) => Err(format!(
                "{} staged record read(s) failed; the first: {first}",
                failures.len()
            )),
        }
    }

    /// Bytes staged, over every staging segment.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Writes the content sections of `units`, each `(key, location)`, into
    /// `writer`, as [`aft_store::content::ContentSectionsBuilder`] holding
    /// the same records writes them. `units` must be in ascending key
    /// order.
    ///
    /// # Errors
    ///
    /// When a read or a write fails.
    pub(crate) fn write_sections(
        &self,
        writer: &mut aft_store::segment::SegmentWriter,
        units: impl Iterator<Item = (RecordKey, StagedAt)>,
    ) -> Result<(), String> {
        let units = units
            .map(|(key, at)| (key, at.stage as usize, at.unit))
            .collect::<Vec<_>>();
        aft_store::content::write_staged(writer, &self.stages, &units)
            .map_err(|error| error.to_string())
    }
}

aft_store::heap_bytes_none!(StagedAt);

impl aft_store::heap::HeapBytes for StagedRecords {
    /// The failures recorded and the open segments' tables; the staged
    /// bytes are on disk, and the blocks read from them are the block
    /// cache's.
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.stages.capacity() * std::mem::size_of::<Stage>()
            + self
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .heap_bytes(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft_store::family::BuildScratch;

    fn record(language: &str) -> ContentRecord {
        ContentRecord {
            language: language.to_string(),
            extractor_version: "v1".to_string(),
            data: aft_store::content::RecordBody::Markers,
            todos: None,
        }
    }

    #[test]
    fn staged_records_read_back_and_a_refused_admission_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(aft_store::ids::Digest128::of(b"staging"));
        let workspace = Arc::new(
            BuildScratch::create_unattached(root.path(), |bytes| {
                if bytes <= ADMIT_BYTES {
                    Ok(())
                } else {
                    Err(DataError::DiskFloor("below the floor".to_string()))
                }
            })
            .unwrap(),
        );
        let stage = RecordStage::new(workspace.subdir("records").unwrap(), family);
        let first = stage.append(&record("rust")).unwrap();
        let second = stage.append(&record("go")).unwrap();
        let staged = stage.finish(&Arc::new(BlockCache::new(1 << 20))).unwrap();
        assert_eq!(
            staged.encoded(first, "a").unwrap().decode().unwrap(),
            record("rust")
        );
        assert_eq!(
            staged.encoded(second, "b").unwrap().decode().unwrap(),
            record("go")
        );
        assert!(staged.check().is_ok());
        let missing = StagedAt {
            stage: 7,
            unit: first.unit,
        };
        assert!(staged.encoded(missing, "c").is_none());
        let failure = staged.check().unwrap_err();
        assert!(failure.contains("the staged record of c"), "{failure}");
        drop(staged);

        // Under a workspace whose admission refuses everything, nothing is
        // written.
        let refused = Arc::new(
            BuildScratch::create_unattached(&root.path().join("refused"), |_| {
                Err(DataError::DiskFloor("below the floor".to_string()))
            })
            .unwrap(),
        );
        let stage = RecordStage::new(refused.subdir("records").unwrap(), family);
        assert!(matches!(
            stage.append(&record("rust")),
            Err(DataError::DiskFloor(_))
        ));
        assert_eq!(
            std::fs::read_dir(refused.path().join("records"))
                .unwrap()
                .count(),
            0,
            "nothing staged after a refusal"
        );
        workspace.remove().unwrap();
        refused.remove().unwrap();
    }
}
