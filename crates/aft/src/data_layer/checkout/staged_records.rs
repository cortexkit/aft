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
//! The staged records are also the build's `records` checkpoint (see
//! [`super::resume`]). Each staging segment is written as a `.part` file and
//! renamed, once finished, to a name derived from the record keys it holds;
//! beside it a table (`.keys`) lists each of those keys with where its
//! record lies and what the resolver needs of it without reading it. When
//! the scan ends, the stage's marker lists every segment and table. A build
//! that resumes reads the tables back ([`AdoptedRecords`]), and its scan
//! finds each record there by key instead of extracting the file again.
//!
//! A staged record that cannot be read back fails the build: the read is
//! recorded, and the build checks for recorded failures at each stage
//! boundary and before it publishes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aft_store::cache::BlockCache;
use aft_store::content::{
    ContentRecord, EncodedRecord, PackedRecord, RecordBody, RecordKey, Stage, StageWriter,
    StagedUnit,
};
use aft_store::family::ScratchDir;
use aft_store::ids::{Digest128, FamilyId};

use super::resume::{self, FileSum, Header, Marker};
use super::DataError;
use crate::callgraph_store::join::CallgraphBlob;

/// A staging segment is finished and a new one started past this many
/// bytes, well below the largest segment a file may hold.
const SEGMENT_BYTES: u64 = 4 << 30;

/// Disk admission is asked for this many bytes at a time, before the
/// appends that use them.
const ADMIT_BYTES: u64 = 64 << 20;

/// The name of the workspace's folder for staged records.
pub(crate) const FOLDER: &str = "records";

/// Where one staged record lies: its staging segment and its place there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StagedAt {
    stage: u32,
    unit: StagedUnit,
}

/// What reading the call graph's inputs needs of a staged record without
/// reading it back.
#[derive(Clone, Debug)]
pub(crate) enum StagedKind {
    /// A configuration file's record, with its bytes, which the resolver
    /// reads from memory.
    Config(Arc<[u8]>),
    /// A parsed file's record, and whether its parse converts for the call
    /// graph (one that does not is left out of the resolver's inputs), with
    /// the parse's kinds the resolver lists files by
    /// ([`super::super::callgraph::list_kinds`]).
    Parsed {
        converts: bool,
        kinds: (bool, bool),
    },
    Markers,
}

impl StagedKind {
    pub(crate) fn of(record: &ContentRecord) -> Self {
        match &record.data {
            RecordBody::Config { source } => Self::Config(Arc::from(source.as_slice())),
            RecordBody::Parsed { .. } => match super::super::content::blob_from_record(record) {
                Ok(CallgraphBlob::Parse(parse)) => Self::Parsed {
                    converts: true,
                    kinds: super::super::callgraph::list_kinds(&parse),
                },
                _ => Self::Parsed {
                    converts: false,
                    kinds: (false, false),
                },
            },
            RecordBody::Markers => Self::Markers,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) -> std::io::Result<()> {
        match self {
            Self::Config(source) => {
                out.push(0);
                let len = u32::try_from(source.len())
                    .map_err(|_| std::io::Error::other("a configuration file of 4 GiB"))?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(source);
            }
            Self::Parsed {
                converts,
                kinds: (rust, declaring),
            } => {
                out.extend_from_slice(&[
                    1,
                    u8::from(*converts),
                    u8::from(*rust),
                    u8::from(*declaring),
                ]);
            }
            Self::Markers => out.push(2),
        }
        Ok(())
    }

    fn decode(reader: &mut resume::Reader<'_>) -> std::io::Result<Self> {
        Ok(match reader.u8()? {
            0 => Self::Config(Arc::from(reader.part()?)),
            1 => {
                let flags = reader.take(3)?;
                Self::Parsed {
                    converts: flags[0] != 0,
                    kinds: (flags[1] != 0, flags[2] != 0),
                }
            }
            2 => Self::Markers,
            _ => return Err(std::io::Error::other("a staged record table is malformed")),
        })
    }
}

/// The staged records of an earlier attempt of the same build, from its
/// complete `records` stage: found by key, never extracted again.
#[derive(Default)]
pub(crate) struct AdoptedRecords {
    /// The stage's files as its marker lists them, listed again by this
    /// attempt's marker.
    files: Vec<(String, FileSum)>,
    /// The staging segments, in the order [`StagedAt`] numbers them.
    segments: Vec<PathBuf>,
    table: HashMap<RecordKey, (StagedAt, StagedKind)>,
}

impl AdoptedRecords {
    /// Reads the tables of the complete stage `marker` lists in
    /// `workspace`, whose files were already checked in full.
    ///
    /// # Errors
    ///
    /// When the marker does not list segment and table pairs, or a table
    /// cannot be read.
    pub(crate) fn load(workspace: &Path, marker: &Marker) -> std::io::Result<Self> {
        let malformed = |what: &str| std::io::Error::other(format!("the records stage {what}"));
        let files = marker
            .files()
            .map(|(name, sum)| (name.to_string(), sum))
            .collect::<Vec<_>>();
        if files.len() % 2 != 0 {
            return Err(malformed("does not list its files in pairs"));
        }
        let mut adopted = Self {
            files,
            ..Self::default()
        };
        for pair in adopted.files.chunks(2) {
            let (segment, table) = (&pair[0].0, &pair[1].0);
            let stem = segment
                .strip_suffix(".seg")
                .ok_or_else(|| malformed("lists a segment without its suffix"))?;
            if table.strip_suffix(".keys") != Some(stem) {
                return Err(malformed("lists a table that is not its segment's"));
            }
            let number = u32::try_from(adopted.segments.len())
                .map_err(|_| malformed("has too many segments"))?;
            let (header, body) = resume::read_framed(&workspace.join(table))?;
            if header.stage != resume::RECORDS || header.identity != marker.header.identity {
                return Err(malformed("lists a table of another stage"));
            }
            let mut reader = resume::Reader::new(&body);
            for member in &header.members {
                let digest: [u8; Digest128::LEN] = member
                    .as_slice()
                    .try_into()
                    .map_err(|_| malformed("names a malformed record key"))?;
                let unit = StagedUnit::from_bytes(
                    reader
                        .take(StagedUnit::BYTES)?
                        .try_into()
                        .expect("took that many"),
                );
                let kind = StagedKind::decode(&mut reader)?;
                adopted
                    .table
                    .entry(RecordKey::from_digest(Digest128::from_bytes(digest)))
                    .or_insert((
                        StagedAt {
                            stage: number,
                            unit,
                        },
                        kind,
                    ));
            }
            if !reader.is_done() {
                return Err(malformed("has a table with trailing bytes"));
            }
            adopted.segments.push(workspace.join(segment));
        }
        Ok(adopted)
    }

    /// How many records the stage holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.table.len()
    }
}

/// The staging segments a scan appends to.
pub(crate) struct RecordStage {
    dir: ScratchDir<DataError>,
    family: FamilyId,
    /// The build's identity digest, which the stage's files name.
    identity: resume::Digest,
    adopted: AdoptedRecords,
    out: Mutex<StageOut>,
}

struct StageOut {
    writer: Option<StageWriter>,
    /// What the segment being written holds, in the order appended.
    members: Vec<(RecordKey, StagedUnit, StagedKind)>,
    /// The segments this scan finished, each as its segment and its table
    /// with their sums, in number order.
    finished: Vec<[(String, FileSum); 2]>,
    /// Bytes appended, and bytes disk admission allowed so far.
    written: u64,
    admitted: u64,
}

impl RecordStage {
    /// Stages records in `dir`, the build workspace's own folder for them,
    /// for the build whose identity digest is `identity`, finding records
    /// an earlier attempt staged in `adopted`.
    pub(crate) fn new(
        dir: ScratchDir<DataError>,
        family: FamilyId,
        identity: resume::Digest,
        adopted: AdoptedRecords,
    ) -> Self {
        Self {
            dir,
            family,
            identity,
            adopted,
            out: Mutex::new(StageOut {
                writer: None,
                members: Vec::new(),
                finished: Vec::new(),
                written: 0,
                admitted: 0,
            }),
        }
    }

    /// The record with `key` an earlier attempt staged, if any.
    pub(crate) fn adopted(&self, key: RecordKey) -> Option<(StagedAt, StagedKind)> {
        self.adopted.table.get(&key).cloned()
    }

    /// The `.part` file the segment numbered `number` is written to.
    fn part_path(&self, number: usize) -> PathBuf {
        self.dir
            .path()
            .join(format!("{FOLDER}-{}.part", number + 1))
    }

    /// Appends `record`, encoded and packed on the calling thread, with
    /// `key` and `kind`, and returns where it lies. Disk admission is asked
    /// before the bytes it has not yet allowed are written.
    ///
    /// # Errors
    ///
    /// When admission refuses, or the write fails.
    pub(crate) fn append(
        &self,
        key: RecordKey,
        record: &ContentRecord,
        kind: &StagedKind,
    ) -> Result<StagedAt, DataError> {
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
            self.finish_segment(&mut out)?;
        }
        if out.writer.is_none() {
            let path = self.part_path(out.finished.len());
            // A `.part` a killed attempt left holds nothing any stage lists.
            let _ = std::fs::remove_file(&path);
            out.writer = Some(StageWriter::create(&path, self.family).map_err(DataError::store)?);
        }
        let unit = out
            .writer
            .as_mut()
            .expect("made above")
            .append(&packed)
            .map_err(DataError::store)?;
        out.members.push((key, unit, kind.clone()));
        out.written += packed.len();
        let stage = self.adopted.segments.len() + out.finished.len();
        drop(out);
        resume::kill_point("records.append");
        Ok(StagedAt {
            stage: u32::try_from(stage).expect("fewer than 4 billion staging segments"),
            unit,
        })
    }

    /// Finishes the segment being written: renames it to a name derived
    /// from the keys it holds and writes its table beside it.
    fn finish_segment(&self, out: &mut StageOut) -> Result<(), DataError> {
        let Some(writer) = out.writer.take() else {
            return Ok(());
        };
        writer.finish().map_err(DataError::store)?;
        let members = std::mem::take(&mut out.members);
        let keys = members
            .iter()
            .map(|(key, _, _)| *key.digest().as_bytes())
            .collect::<Vec<_>>();
        let name = resume::member_name(FOLDER, keys.iter().map(<[u8; Digest128::LEN]>::as_slice));
        let failed = |error: std::io::Error| {
            DataError::Store(format!("the staged records' checkpoint: {error}"))
        };
        let segment = self.dir.path().join(format!("{name}.seg"));
        std::fs::rename(self.part_path(out.finished.len()), &segment).map_err(failed)?;
        let segment_sum = resume::sum_file(&segment).map_err(failed)?;
        let header = Header {
            identity: self.identity,
            snapshot: None,
            stage: resume::RECORDS.to_string(),
            members: keys.iter().map(|key| key.to_vec()).collect(),
        };
        let mut body = Vec::with_capacity(members.len() * (StagedUnit::BYTES + 4));
        for (_, unit, kind) in &members {
            body.extend_from_slice(&unit.to_bytes());
            kind.encode(&mut body).map_err(failed)?;
        }
        self.dir
            .workspace()
            .check_space(resume::framed_len(&header, body.len()))?;
        let table = self.dir.path().join(format!("{name}.keys"));
        let table_sum = resume::write_framed(&table, &header, &body).map_err(failed)?;
        out.finished.push([
            (format!("{FOLDER}/{name}.seg"), segment_sum),
            (format!("{FOLDER}/{name}.keys"), table_sum),
        ]);
        resume::kill_point("records.segment");
        Ok(())
    }

    /// Finishes the staging segments, completes the `records` stage with
    /// its marker, and opens every segment, an earlier attempt's first, for
    /// reading through `cache`.
    ///
    /// # Errors
    ///
    /// When a staging segment cannot be finished or opened, or the stage
    /// cannot be completed.
    pub(crate) fn finish(self, cache: &Arc<BlockCache>) -> Result<StagedRecords, DataError> {
        let mut out = self.out.lock().expect("no panics while holding it");
        self.finish_segment(&mut out)?;
        // A resumed scan that staged nothing new leaves the adopted marker
        // as it is: rewriting the same bytes over it gains nothing, and on
        // Windows a replace another process's reader holds up would fail
        // the build.
        if !out.finished.is_empty() || self.adopted.files.is_empty() {
            let mut files = self.adopted.files.clone();
            files.extend(out.finished.iter().flatten().cloned());
            resume::write_marker(
                self.dir.workspace(),
                resume::RECORDS,
                self.identity,
                None,
                &files,
            )?;
        }
        resume::kill_point("records.committed");
        let paths = self
            .adopted
            .segments
            .iter()
            .cloned()
            .chain(
                out.finished
                    .iter()
                    .map(|[(segment, _), _]| self.dir.workspace().path().join(segment)),
            )
            .collect::<Vec<_>>();
        let stages = paths
            .iter()
            .map(|path| Stage::open(path, self.family, Arc::clone(cache)).map_err(DataError::store))
            .collect::<Result<Vec<_>, _>>()?;
        let bytes = out.written;
        drop(out);
        Ok(StagedRecords {
            _dir: self.dir,
            stages,
            bytes,
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

    fn key(name: &[u8]) -> RecordKey {
        RecordKey::from_digest(Digest128::of(name))
    }

    fn append(stage: &RecordStage, name: &[u8], language: &str) -> Result<StagedAt, DataError> {
        let record = record(language);
        stage.append(key(name), &record, &StagedKind::of(&record))
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
        let stage = RecordStage::new(
            workspace.subdir("records").unwrap(),
            family,
            [1; 32],
            AdoptedRecords::default(),
        );
        let first = append(&stage, b"a", "rust").unwrap();
        let second = append(&stage, b"b", "go").unwrap();
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
        let stage = RecordStage::new(
            refused.subdir("records").unwrap(),
            family,
            [1; 32],
            AdoptedRecords::default(),
        );
        assert!(matches!(
            append(&stage, b"a", "rust"),
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

    #[test]
    fn a_finished_stage_is_adopted_by_key_and_a_resumed_scan_adds_to_it() {
        let root = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(Digest128::of(b"adopt"));
        let cache = Arc::new(BlockCache::new(1 << 20));
        let workspace = Arc::new(BuildScratch::create_unattached(root.path(), |_| Ok(())).unwrap());
        let stage = RecordStage::new(
            workspace.subdir(FOLDER).unwrap(),
            family,
            [1; 32],
            AdoptedRecords::default(),
        );
        let first = append(&stage, b"a", "rust").unwrap();
        let config = ContentRecord {
            data: RecordBody::Config {
                source: b"[package]".to_vec(),
            },
            ..record("toml")
        };
        stage
            .append(key(b"config"), &config, &StagedKind::of(&config))
            .unwrap();
        drop(stage.finish(&cache).unwrap());
        let marker = resume::read_marker(workspace.path(), resume::RECORDS)
            .unwrap()
            .unwrap();
        assert_eq!(marker.sums.len(), 2, "one segment and its table");
        assert!(
            std::fs::read_dir(workspace.path().join(FOLDER))
                .unwrap()
                .all(|entry| !entry.unwrap().path().to_string_lossy().ends_with(".part")),
            "every staging segment was renamed"
        );

        let adopted = AdoptedRecords::load(workspace.path(), &marker).unwrap();
        assert_eq!(adopted.len(), 2);
        let stage = RecordStage::new(
            workspace.subdir_existing(FOLDER).unwrap(),
            family,
            [1; 32],
            adopted,
        );
        let (again, kind) = stage.adopted(key(b"a")).unwrap();
        assert_eq!(again, first);
        assert!(matches!(kind, StagedKind::Markers));
        assert!(matches!(
            stage.adopted(key(b"config")),
            Some((_, StagedKind::Config(source))) if &*source == b"[package]"
        ));
        assert!(stage.adopted(key(b"new")).is_none());
        let added = append(&stage, b"new", "go").unwrap();
        assert_eq!(added.stage, 1, "after the adopted segment");
        let staged = stage.finish(&cache).unwrap();
        assert_eq!(
            staged.encoded(first, "a").unwrap().decode().unwrap(),
            record("rust")
        );
        assert_eq!(
            staged.encoded(added, "new").unwrap().decode().unwrap(),
            record("go")
        );
        let marker = resume::read_marker(workspace.path(), resume::RECORDS)
            .unwrap()
            .unwrap();
        assert_eq!(marker.sums.len(), 4, "both segments and their tables");
        drop(staged);
        workspace.remove().unwrap();
    }
}
