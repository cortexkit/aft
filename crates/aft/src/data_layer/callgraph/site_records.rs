//! Pass 4 of a cold build, in two steps: each file's sites and
//! declarations projected in file order (see `dispatch::projected`), then
//! every site matched against the projected records alone (see
//! `key_order`).
//!
//! Between the two steps the records are kept on disk, not in memory. The
//! first step appends each file's record, in path order, to a scratch
//! segment in the build workspace's `pass4/` folder ([`RecordWriter`]); the
//! segment's blocks are checksummed, and each read checks the blocks it
//! touches. Memory keeps one table row per file (its path and where its
//! record lies) and the names each file declares. The second step reads
//! records back through a cache bounded in bytes ([`RecordSource`]), and
//! each resolver reads them through its own [`Pinned`] set, which keeps
//! every record the resolver has read alive, and out of the cache's
//! eviction, until the resolver is done with them. A record read again
//! after the cache let it go decodes equal, so eviction never changes an
//! answer.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use aft_store::cache::BlockCache;
use aft_store::family::{BuildScratch, ScratchDir};
use aft_store::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};
use aft_store::segment::{
    Alignment, KeySpace, KnownSection, Requirement, SectionHandle, SectionId, SectionKind,
    SectionSpec, Segment, SegmentMeta, SegmentWriter,
};
use rayon::prelude::*;

use crate::callgraph_store::join::dispatch::projected::{
    FileRecord, ProjectedMro, ProjectedType, Projections, Recorded,
};
use crate::callgraph_store::join::dispatch::{DispatchFiles, Resolver};
use crate::callgraph_store::join::{ManifestJoinError, ParseBlob};

use super::super::checkout::DataError;
use super::{CheckoutInputs, MethodCallInputs, NoResolutions, SiteSource};

/// The one section of a pass-4 record segment: every record's scratch
/// encoding (see [`FileRecord::encode_scratch`]), one after another. Only a
/// build's own scratch folder holds it; no published segment does.
const RECORDS: SectionKind = SectionKind::new(0x02F1);
const RECORDS_VERSION: u16 = 1;

/// A record segment is finished and a new one started past this many
/// bytes, well below the largest segment a file may hold.
const SEGMENT_BYTES: u64 = 4 << 30;

/// Disk admission is asked for this many bytes at a time, before the
/// appends that use them.
const ADMIT_BYTES: u64 = 64 << 20;

/// The block cache the record segments are read through. Records are read
/// mostly out of order, so it is kept small: it saves a second read of a
/// block only when neighbouring records are read together, as they are in
/// file order.
const BLOCK_CACHE_BYTES: usize = 8 << 20;

fn records_family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"aft pass-4 site records"))
}

fn records_section() -> SectionId {
    SectionId::new(RECORDS, KeySpace::new(0))
}

fn record_error(error: impl std::fmt::Display) -> ManifestJoinError {
    ManifestJoinError::Parse(format!("pass-4 records: {error}"))
}

/// Where one file's record lies, and the heap its decoded record holds.
#[derive(Clone, Copy, Debug)]
struct Placed {
    segment: u32,
    offset: u64,
    len: u32,
    heap: u32,
}

/// What the records hold, as the memory measurement reports it.
#[derive(Clone, Copy, Debug, Default)]
struct RecordCensus {
    /// The heap of every record, decoded.
    heap: usize,
    /// The heap of the projected answers alone.
    projection_bytes: usize,
    answers: usize,
    /// How many answers answer a Rust file's resolvable names only.
    rust_names: usize,
}

/// One record encoded on a projecting thread, ready to append.
struct EncodedRecord {
    path: String,
    bytes: Vec<u8>,
    heap: usize,
    methods: Vec<String>,
    go_required: Vec<String>,
    census: RecordCensus,
}

/// Writes the projected records of a cold build, in path order, to record
/// segments in the build workspace's `pass4/` folder.
pub(crate) struct RecordWriter {
    dir: ScratchDir<DataError>,
    writer: Option<SegmentWriter>,
    /// Finished record segments, in number order.
    finished: usize,
    /// The bytes and records in the open segment's section.
    segment_bytes: u64,
    segment_records: u64,
    /// Bytes appended, and bytes disk admission allowed so far.
    written: u64,
    admitted: u64,
    paths: Vec<String>,
    placed: Vec<Placed>,
    /// Method name to the files declaring it, in path order.
    methods: HashMap<String, Vec<String>>,
    /// Method name to the Go files whose interfaces require it.
    go_required: HashMap<String, Vec<String>>,
    census: RecordCensus,
}

impl RecordWriter {
    /// A writer into a new `pass4/` folder of `workspace`; a workspace
    /// holds one.
    ///
    /// # Errors
    ///
    /// When the folder cannot be made.
    pub(crate) fn new(workspace: &Arc<BuildScratch<DataError>>) -> Result<Self, ManifestJoinError> {
        Ok(Self {
            dir: workspace.subdir("pass4").map_err(record_error)?,
            writer: None,
            finished: 0,
            segment_bytes: 0,
            segment_records: 0,
            written: 0,
            admitted: 0,
            paths: Vec::new(),
            placed: Vec::new(),
            methods: HashMap::new(),
            go_required: HashMap::new(),
            census: RecordCensus::default(),
        })
    }

    fn segment_path(dir: &ScratchDir<DataError>, number: usize) -> std::path::PathBuf {
        dir.path().join(format!("records-{}", number + 1))
    }

    /// Appends `records`, whose paths must come after every path appended
    /// before, in ascending order. They are encoded in parallel and written
    /// in order; disk admission is asked before the bytes it has not yet
    /// allowed are written.
    ///
    /// # Errors
    ///
    /// When a path is out of order, a record cannot be encoded, admission
    /// refuses, or a write fails.
    pub(crate) fn append(
        &mut self,
        records: Vec<(String, FileRecord)>,
    ) -> Result<(), ManifestJoinError> {
        let carry = crate::alloc_trace::carry();
        let encoded = records
            .into_par_iter()
            .map(|(path, record)| {
                let _label = carry.enter();
                use aft_store::heap::HeapBytes as _;
                let mut bytes = Vec::new();
                record.encode_scratch(&mut bytes).map_err(record_error)?;
                let heap = record.heap_bytes(&mut aft_store::heap::Seen::new());
                let (answers, rust_names, projection_bytes) =
                    record.projection_census(&mut aft_store::heap::Seen::new());
                Ok(EncodedRecord {
                    path,
                    bytes,
                    heap,
                    methods: record
                        .method_names()
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    go_required: record
                        .go_required()
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    census: RecordCensus {
                        heap,
                        projection_bytes,
                        answers,
                        rust_names,
                    },
                })
            })
            .collect::<Result<Vec<_>, ManifestJoinError>>()?;
        for record in encoded {
            self.write(record)?;
        }
        Ok(())
    }

    fn write(&mut self, record: EncodedRecord) -> Result<(), ManifestJoinError> {
        if self
            .paths
            .last()
            .is_some_and(|last| last.as_str() >= record.path.as_str())
        {
            return Err(record_error(format!(
                "{} is written after {}, out of path order",
                record.path,
                self.paths.last().expect("checked above")
            )));
        }
        let len = record.bytes.len() as u64;
        if self.written + len > self.admitted {
            let more = len.max(ADMIT_BYTES);
            self.dir
                .workspace()
                .check_space(more)
                .map_err(record_error)?;
            self.admitted += more;
        }
        if self.writer.is_some()
            && self.segment_records > 0
            && self.segment_bytes + len > SEGMENT_BYTES
        {
            self.finish_segment()?;
        }
        if self.writer.is_none() {
            let mut writer = SegmentWriter::create(
                &Self::segment_path(&self.dir, self.finished),
                SegmentMeta {
                    family: records_family(),
                    segment: SegmentId::new(0),
                    incarnation: WriterIncarnation::new(0),
                    created_unix_ms: 0,
                },
            )
            .map_err(record_error)?;
            writer
                .begin_section(SectionSpec {
                    id: records_section(),
                    version: RECORDS_VERSION,
                    alignment: Alignment::Word,
                    requirement: Requirement::Required,
                })
                .map_err(record_error)?;
            self.writer = Some(writer);
        }
        let too_large =
            || record_error(format!("the record of {} is 4 GiB or larger", record.path));
        let placed = Placed {
            segment: u32::try_from(self.finished).map_err(|_| too_large())?,
            offset: self.segment_bytes,
            len: u32::try_from(len).map_err(|_| too_large())?,
            heap: u32::try_from(record.heap).unwrap_or(u32::MAX),
        };
        self.writer
            .as_mut()
            .expect("made above")
            .write(&record.bytes)
            .map_err(record_error)?;
        self.segment_bytes += len;
        self.segment_records += 1;
        self.written += len;
        for name in record.methods {
            self.methods
                .entry(name)
                .or_default()
                .push(record.path.clone());
        }
        for name in record.go_required {
            self.go_required
                .entry(name)
                .or_default()
                .push(record.path.clone());
        }
        let census = &mut self.census;
        census.heap += record.census.heap;
        census.projection_bytes += record.census.projection_bytes;
        census.answers += record.census.answers;
        census.rust_names += record.census.rust_names;
        self.paths.push(record.path);
        self.placed.push(placed);
        Ok(())
    }

    fn finish_segment(&mut self) -> Result<(), ManifestJoinError> {
        if let Some(mut writer) = self.writer.take() {
            writer
                .end_section(self.segment_records)
                .map_err(record_error)?;
            writer.finish().map_err(record_error)?;
            self.finished += 1;
            self.segment_bytes = 0;
            self.segment_records = 0;
        }
        Ok(())
    }

    /// What the records written so far hold, as named measurements: their
    /// heap decoded, the heap of the projected answers alone, how many
    /// answers there are, and how many of those answer a Rust file's
    /// resolvable names only.
    pub(crate) fn census(&self) -> [(&'static str, usize); 4] {
        [
            ("resolve.site_records", self.census.heap),
            ("resolve.projection_bytes", self.census.projection_bytes),
            ("resolve.projected_answers", self.census.answers),
            ("resolve.rust_name_answers", self.census.rust_names),
        ]
    }

    /// Finishes the record segments and opens them for reading, through a
    /// cache keeping decoded records that count for at most `cache_bytes`
    /// besides those a [`Pinned`] set holds.
    ///
    /// # Errors
    ///
    /// When a segment cannot be finished or opened.
    pub(crate) fn finish(mut self, cache_bytes: usize) -> Result<RecordSource, ManifestJoinError> {
        self.finish_segment()?;
        let blocks = Arc::new(BlockCache::new(BLOCK_CACHE_BYTES));
        let known = [KnownSection {
            kind: RECORDS,
            version: RECORDS_VERSION,
        }];
        let segments = (0..self.finished)
            .map(|number| {
                let segment = Segment::open(
                    &Self::segment_path(&self.dir, number),
                    records_family(),
                    &known,
                    Arc::clone(&blocks),
                )
                .map_err(record_error)?;
                let section = segment
                    .section(records_section())
                    .ok_or_else(|| record_error("a record segment without its section"))?;
                Ok((segment, section))
            })
            .collect::<Result<Vec<_>, ManifestJoinError>>()?;
        let files = self.paths.len();
        Ok(RecordSource {
            _dir: self.dir,
            segments,
            paths: self.paths,
            placed: self.placed,
            methods: self.methods,
            go_required: self.go_required,
            disk_bytes: self.written,
            cache: Mutex::new(RecordCache::new(cache_bytes, files)),
            error: Mutex::new(None),
            count_touches: false,
            #[cfg(feature = "callgraph-shape-census")]
            census: Mutex::default(),
        })
    }
}

/// Projects the files of `batch`, whose method-call inputs `calls` keeps,
/// in parallel chunks, each with its own resolver, and appends their
/// records to `records`.
///
/// # Errors
///
/// When a file has no parse or no inputs, reading another file's inputs
/// failed, or a record cannot be written.
pub(super) fn project_batch(
    inputs: &CheckoutInputs,
    calls: &MethodCallInputs,
    batch: &[&String],
    records: &mut RecordWriter,
) -> Result<(), ManifestJoinError> {
    let source = SiteSource::kept(inputs, &NoResolutions, calls);
    let chunk = batch
        .len()
        .div_ceil(rayon::current_num_threads() * 4)
        .max(1);
    let carry = crate::alloc_trace::carry();
    let projected = batch
        .par_chunks(chunk)
        .map(|chunk| {
            let _label = carry.enter();
            let resolver = Resolver::over(&source);
            chunk
                .iter()
                .map(|path| {
                    let (_, parse) = inputs.parses.get(path).ok_or_else(|| {
                        ManifestJoinError::Parse(format!("missing parse for {path}"))
                    })?;
                    let site = source.site(path).ok_or_else(|| {
                        ManifestJoinError::Parse(format!("missing method-call inputs for {path}"))
                    })?;
                    let ordinary = &site.ordinary;
                    let record = resolver
                        .project_file(
                            path,
                            parse.bound_dispatch_sites(),
                            &mut site.facts.type_targets.keys().map(String::as_str),
                            &|ordinal| ordinary.associated.contains(&ordinal),
                            &|ordinal| {
                                ordinary.edges.contains(&ordinal)
                                    && ordinary.syntax.contains(&ordinal)
                            },
                        )
                        .ok_or_else(|| {
                            ManifestJoinError::Parse(format!(
                                "missing method-call facts for {path}"
                            ))
                        })?;
                    Ok(((*path).clone(), record))
                })
                .collect::<Result<Vec<_>, ManifestJoinError>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(error) = source
        .error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(ManifestJoinError::Parse(error));
    }
    records.append(projected.into_iter().flatten().collect())
}

/// Decoded records kept for the second step, up to a byte limit, and
/// counts of what reading them did.
struct RecordCache {
    limit: usize,
    map: HashMap<u32, Cached>,
    /// The files kept, in the order a second-chance sweep visits them.
    order: VecDeque<u32>,
    /// The bytes the kept records count for, and the most they counted
    /// for at once.
    held: usize,
    peak: usize,
    /// Records read from disk, how many of those had been read before, and
    /// how many the cache let go.
    reads: u64,
    rereads: u64,
    evicted: u64,
    /// Whether each file's record has been read from disk.
    read_before: Vec<bool>,
}

struct Cached {
    record: Arc<FileRecord>,
    bytes: usize,
    /// Read since the sweep last passed it.
    used: bool,
}

/// What the record cache did, for measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordCacheCounts {
    pub(crate) limit: usize,
    pub(crate) reads: u64,
    pub(crate) rereads: u64,
    pub(crate) evicted: u64,
    pub(crate) held: usize,
    pub(crate) peak: usize,
}

impl RecordCache {
    fn new(limit: usize, files: usize) -> Self {
        Self {
            limit,
            map: HashMap::new(),
            order: VecDeque::new(),
            held: 0,
            peak: 0,
            reads: 0,
            rereads: 0,
            evicted: 0,
            read_before: vec![false; files],
        }
    }

    fn get(&mut self, file: u32) -> Option<Arc<FileRecord>> {
        let cached = self.map.get_mut(&file)?;
        cached.used = true;
        Some(Arc::clone(&cached.record))
    }

    /// Notes that `file`'s record was read from disk.
    fn note_read(&mut self, file: u32) {
        self.reads += 1;
        if let Some(read) = self.read_before.get_mut(file as usize) {
            if *read {
                self.rereads += 1;
            }
            *read = true;
        }
    }

    /// Keeps `record` as `file`'s, unless another thread kept one first,
    /// and returns the one kept. Then lets go of records until those kept
    /// count for at most the limit, or none left can go.
    fn insert(&mut self, file: u32, record: Arc<FileRecord>, bytes: usize) -> Arc<FileRecord> {
        if let Some(kept) = self.get(file) {
            return kept;
        }
        self.map.insert(
            file,
            Cached {
                record: Arc::clone(&record),
                bytes,
                used: false,
            },
        );
        self.order.push_back(file);
        self.held += bytes;
        self.peak = self.peak.max(self.held);
        self.evict();
        record
    }

    /// One sweep at most over the kept records, letting go of those not
    /// read since the sweep last passed them and held by no [`Pinned`]
    /// set, until the rest count for at most the limit. A record a pinned
    /// set holds stays: a resolver may still be reading it, and letting it
    /// go would only make the next reader read it again.
    fn evict(&mut self) {
        let mut visits = self.order.len();
        while self.held > self.limit && visits > 0 {
            visits -= 1;
            let Some(file) = self.order.pop_front() else {
                break;
            };
            let cached = self
                .map
                .get_mut(&file)
                .expect("every file in the sweep order is kept");
            if cached.used {
                cached.used = false;
                self.order.push_back(file);
                continue;
            }
            // The cache's own reference is the only one when no pinned
            // set holds the record. Another reference is taken only under
            // this lock, so the count cannot rise while it is checked.
            if Arc::strong_count(&cached.record) > 1 {
                self.order.push_back(file);
                continue;
            }
            let cached = self.map.remove(&file).expect("found above");
            self.held -= cached.bytes;
            self.evicted += 1;
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order = VecDeque::new();
        self.held = 0;
    }

    fn counts(&self) -> RecordCacheCounts {
        RecordCacheCounts {
            limit: self.limit,
            reads: self.reads,
            rereads: self.rereads,
            evicted: self.evicted,
            held: self.held,
            peak: self.peak,
        }
    }
}

/// The records of the second step, read from their segments by file
/// number: a file's number is its position among the paths, in path
/// order. Also files the records by method name, as the in-memory inputs
/// do.
pub(super) struct RecordSource {
    /// Held so the workspace, and so the files, stay while they are read.
    _dir: ScratchDir<DataError>,
    segments: Vec<(Segment, SectionHandle)>,
    paths: Vec<String>,
    placed: Vec<Placed>,
    /// Method name to the files declaring it, in path order.
    methods: HashMap<String, Vec<String>>,
    /// Method name to the Go files whose interfaces require it.
    go_required: HashMap<String, Vec<String>>,
    disk_bytes: u64,
    cache: Mutex<RecordCache>,
    error: Mutex<Option<String>>,
    /// Whether each record lookup is counted for the thread's
    /// [`Touches`], for measurement.
    count_touches: bool,
    #[cfg(feature = "callgraph-shape-census")]
    pub(super) census: Mutex<crate::data_layer::shape_census::Census>,
}

/// The record lookups one thread made while counting (see
/// [`RecordSource::counting_touches`]): how many, and of how many files.
#[derive(Debug, Default)]
pub(super) struct Touches {
    pub(super) lookups: u64,
    /// The numbers of the files whose records were looked up.
    pub(super) files: HashSet<u32>,
}

impl Touches {
    pub(super) fn merge(&mut self, other: Touches) {
        self.lookups += other.lookups;
        self.files.extend(other.files);
    }
}

thread_local! {
    /// What this thread's record lookups touched since [`start_touches`].
    static TOUCHES: RefCell<Option<Touches>> = const { RefCell::new(None) };
}

/// Starts counting this thread's record lookups, when the source counts
/// them at all.
pub(super) fn start_touches() {
    TOUCHES.with(|touches| *touches.borrow_mut() = Some(Touches::default()));
}

/// What this thread's record lookups touched since [`start_touches`], and
/// stops counting.
pub(super) fn take_touches() -> Touches {
    TOUCHES.with(|touches| touches.borrow_mut().take().unwrap_or_default())
}

impl RecordSource {
    /// Counts every record lookup for the looking thread's [`Touches`].
    pub(super) fn counting_touches(mut self) -> Self {
        self.count_touches = true;
        self
    }

    /// Counts a record lookup, of file `file` when it has a record, for the
    /// thread's [`Touches`] when counting.
    fn touch(&self, file: Option<u32>) {
        if !self.count_touches {
            return;
        }
        TOUCHES.with(|touches| {
            if let Some(touches) = touches.borrow_mut().as_mut() {
                touches.lookups += 1;
                if let Some(file) = file {
                    touches.files.insert(file);
                }
            }
        });
    }

    /// Every file with a record, in path order; a file's number is its
    /// position here.
    pub(super) fn paths(&self) -> &[String] {
        &self.paths
    }

    fn number(&self, path: &str) -> Option<u32> {
        let index = self
            .paths
            .binary_search_by(|candidate| candidate.as_str().cmp(path))
            .ok()?;
        u32::try_from(index).ok()
    }

    /// Every path from `start` on, in path order.
    fn paths_from<'s>(&'s self, start: &str) -> impl Iterator<Item = &'s str> + 's {
        let first = self
            .paths
            .partition_point(|candidate| candidate.as_str() < start);
        self.paths[first..].iter().map(String::as_str)
    }

    /// File `file`'s record, read and decoded from its segment, without the
    /// cache.
    fn read(&self, file: u32) -> Result<FileRecord, String> {
        let placed = self
            .placed
            .get(file as usize)
            .ok_or_else(|| format!("no pass-4 record numbered {file}"))?;
        let (segment, section) = self
            .segments
            .get(placed.segment as usize)
            .ok_or_else(|| format!("no pass-4 record segment {}", placed.segment))?;
        let bytes = segment
            .read_range(
                *section,
                placed.offset..placed.offset + u64::from(placed.len),
            )
            .map_err(|error| error.to_string())?;
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .note_read(file);
        FileRecord::decode_scratch(&bytes).map_err(|error| {
            format!(
                "the pass-4 record of {}: {error}",
                self.paths[file as usize]
            )
        })
    }

    /// File `file`'s record, from the cache or else read and kept there.
    fn cached(&self, file: u32) -> Result<Arc<FileRecord>, String> {
        let lock = || {
            self.cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        };
        if let Some(record) = lock().get(file) {
            return Ok(record);
        }
        // Read outside the lock, so threads read different records at once.
        let record = Arc::new(self.read(file)?);
        let bytes = self.placed[file as usize].heap as usize + std::mem::size_of::<FileRecord>();
        Ok(lock().insert(file, record, bytes))
    }

    /// `path`'s record, read from disk for this one use: what assembly
    /// reads, once per file in path order.
    ///
    /// # Errors
    ///
    /// When `path` has no record, or reading it fails.
    pub(super) fn read_path(&self, path: &str) -> Result<FileRecord, ManifestJoinError> {
        let file = self.number(path).ok_or_else(|| {
            ManifestJoinError::Parse(format!("missing method-call record for {path}"))
        })?;
        self.read(file).map_err(ManifestJoinError::Parse)
    }

    /// How many files declare a method named `member`.
    pub(super) fn declarers(&self, member: &str) -> usize {
        self.methods.get(member).map_or(0, Vec::len)
    }

    /// The heap the decoded records of `files` hold together.
    pub(super) fn record_bytes(&self, files: impl Iterator<Item = u32>) -> usize {
        files
            .filter_map(|file| self.placed.get(file as usize))
            .map(|placed| placed.heap as usize)
            .sum()
    }

    /// The bytes the record segments hold on disk.
    pub(super) fn disk_bytes(&self) -> u64 {
        self.disk_bytes
    }

    /// What the cache did so far.
    pub(super) fn cache_counts(&self) -> RecordCacheCounts {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .counts()
    }

    /// Lets go of every cached record, once the second step no longer
    /// reads them by name.
    pub(super) fn release_cache(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Notes something the second step needed and could not read; the
    /// pass fails with the first.
    fn note_error(&self, error: String) {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert(error);
    }

    pub(super) fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// The records one resolver has read, each held until the set is dropped.
/// A resolver keeps references into the records it read (the methods it
/// loaded by name, the types it projected), so every record it reads must
/// stay alive while it does. The cache never lets go of a record a set
/// holds, so readers share one copy.
pub(super) struct Pinned<'s> {
    source: &'s RecordSource,
    /// Whether records are read through the cache, or read from disk for
    /// this set alone (for a pass over every file once, which a cache would
    /// only churn).
    through_cache: bool,
    held: RefCell<HashMap<u32, Arc<FileRecord>>>,
}

impl<'s> Pinned<'s> {
    /// A set reading records through the cache.
    pub(super) fn cached(source: &'s RecordSource) -> Self {
        Self {
            source,
            through_cache: true,
            held: RefCell::default(),
        }
    }

    /// A set reading each record from disk, leaving the cache alone.
    pub(super) fn direct(source: &'s RecordSource) -> Self {
        Self {
            source,
            through_cache: false,
            held: RefCell::default(),
        }
    }

    /// How many records the set holds.
    pub(super) fn len(&self) -> usize {
        self.held.borrow().len()
    }

    /// File `file`'s record, held from now on until the set is dropped, and
    /// counted for the thread's [`Touches`] when counting. `None` when it
    /// cannot be read; the failure is noted for
    /// [`RecordSource::take_error`].
    pub(super) fn record_at(&self, file: u32) -> Option<&FileRecord> {
        self.source.touch(Some(file));
        if let Some(record) = self.held.borrow().get(&file) {
            // SAFETY: as below.
            return Some(unsafe { &*Arc::as_ptr(record) });
        }
        let read = if self.through_cache {
            self.source.cached(file)
        } else {
            self.source.read(file).map(Arc::new)
        };
        let record = match read {
            Ok(record) => record,
            Err(error) => {
                self.source.note_error(error);
                return None;
            }
        };
        let pointer = Arc::as_ptr(&record);
        self.held.borrow_mut().insert(file, record);
        // SAFETY: `held` keeps the record's `Arc` until the set is dropped:
        // nothing takes an entry out, and moving an `Arc` (when the map
        // grows) does not move the record it points to. So the record
        // outlives every borrow of `self`, which bounds the reference.
        Some(unsafe { &*pointer })
    }

    /// `path`'s record, as [`Self::record_at`]. `None` when `path` has no
    /// record, or reading it failed (noted for
    /// [`RecordSource::take_error`]).
    pub(super) fn record(&self, path: &str) -> Option<&FileRecord> {
        match self.source.number(path) {
            Some(file) => self.record_at(file),
            None => {
                self.source.touch(None);
                None
            }
        }
    }
}

impl Projections for Pinned<'_> {
    fn projected_type(&self, file: &str, name: &str) -> Recorded<'_, ProjectedType> {
        self.record(file)
            .map_or(Recorded::Missing, |record| record.projected_type(name))
    }

    fn projected_mro(&self, file: &str, owner: &str) -> Recorded<'_, ProjectedMro> {
        self.record(file)
            .map_or(Recorded::Missing, |record| record.projected_mro(owner))
    }

    fn missing(&self, what: String) {
        self.source
            .note_error(format!("pass 4 found no record of {what}"));
    }
}

impl<'a, 's: 'a> DispatchFiles<'a> for Pinned<'s> {
    fn file(&'a self, path: &str) -> Option<&'a ParseBlob> {
        self.record(path).map(FileRecord::facts)
    }

    fn files_with_method(&self, name: &str) -> Vec<String> {
        self.source.methods.get(name).cloned().unwrap_or_default()
    }

    fn files_in(&self, directory: &str) -> Vec<String> {
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        self.source
            .paths_from(&prefix)
            .take_while(|path| path.starts_with(&prefix))
            .filter(|path| !path[prefix.len()..].contains('/'))
            .map(str::to_string)
            .collect()
    }

    fn go_files_requiring(&self, name: &str) -> Vec<String> {
        self.source
            .go_required
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    // Import and type targets are read only while projecting a type, which
    // the records answer instead.
    fn import_target(&self, file: &str, module: &str) -> Option<String> {
        self.missing(format!("the import target of {module} in {file}"));
        None
    }

    fn type_target(&self, file: &str, name: &str) -> Option<(String, String)> {
        self.missing(format!("the type target of {name} in {file}"));
        None
    }

    fn associated_site(&self, file: &str, ordinal: u32) -> bool {
        self.record(file)
            .is_some_and(|record| record.associated(ordinal))
    }

    fn projections(&'a self) -> Option<&'a (dyn Projections + 'a)> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &std::path::Path) -> Arc<BuildScratch<DataError>> {
        Arc::new(BuildScratch::create_unattached(root, |_| Ok(())).unwrap())
    }

    fn path(index: u32) -> String {
        format!("src/{index:03}.ts")
    }

    /// A writer holding `count` sample records, in path order, written in
    /// batches of seven.
    fn written(workspace: &Arc<BuildScratch<DataError>>, count: u32) -> RecordWriter {
        let mut writer = RecordWriter::new(workspace).unwrap();
        let all = (0..count).collect::<Vec<_>>();
        for batch in all.chunks(7) {
            writer
                .append(
                    batch
                        .iter()
                        .map(|&index| (path(index), FileRecord::sample(index)))
                        .collect(),
                )
                .unwrap();
        }
        writer
    }

    #[test]
    fn records_read_back_equal_after_the_cache_lets_them_go() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let source = written(&workspace, 60).finish(1).unwrap();
        assert_eq!(source.paths().len(), 60);
        assert_eq!(source.declarers("shared"), 60);
        assert_eq!(source.declarers("m1"), 20);
        assert_eq!(source.read_path(&path(17)).unwrap(), FileRecord::sample(17));
        assert!(source.read_path("src/missing.ts").is_err());

        // Each record read through its own set, which lets it go when
        // dropped: a one-byte cache keeps none of them, so the second round
        // reads every record from disk again, and each decodes equal.
        for _round in 0..2 {
            for index in 0..60 {
                let pinned = Pinned::cached(&source);
                assert_eq!(
                    pinned.record(&path(index)),
                    Some(&FileRecord::sample(index))
                );
            }
        }
        let counts = source.cache_counts();
        assert!(counts.evicted >= 59, "{counts:?}");
        assert!(counts.rereads >= 59, "{counts:?}");

        // A record a set holds is kept however small the cache: reading it
        // again, through another set, reads nothing from disk, while other
        // records go.
        let holding = Pinned::cached(&source);
        let held = holding.record(&path(3)).unwrap();
        for index in 10..20 {
            let pinned = Pinned::cached(&source);
            assert!(pinned.record(&path(index)).is_some());
        }
        let reads = source.cache_counts().reads;
        let again = Pinned::cached(&source);
        assert!(std::ptr::eq(again.record(&path(3)).unwrap(), held));
        assert_eq!(source.cache_counts().reads, reads);
        assert_eq!(held, &FileRecord::sample(3));
        // A set reading from disk leaves the cache alone.
        let direct = Pinned::direct(&source);
        assert_eq!(direct.record_at(3), Some(&FileRecord::sample(3)));
        assert!(!std::ptr::eq(direct.record_at(3).unwrap(), held));
        assert!(source.take_error().is_none());
        drop((holding, again, direct));
        drop(source);
        workspace.remove().unwrap();
    }

    #[test]
    fn a_large_cache_keeps_records_and_out_of_order_paths_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let mut writer = written(&workspace, 20);
        assert!(writer
            .append(vec![(path(5), FileRecord::sample(5))])
            .is_err());
        let source = writer.finish(64 << 20).unwrap();
        for _round in 0..2 {
            for index in 0..20 {
                let pinned = Pinned::cached(&source);
                assert_eq!(pinned.record_at(index), Some(&FileRecord::sample(index)));
            }
        }
        let counts = source.cache_counts();
        assert_eq!((counts.reads, counts.rereads, counts.evicted), (20, 0, 0));
        source.release_cache();
        assert_eq!(source.cache_counts().held, 0);
        drop(source);
        workspace.remove().unwrap();
    }

    #[test]
    fn a_changed_record_block_fails_its_checksum() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        let mut writer = written(&workspace, 60);
        writer.finish_segment().unwrap();
        let file = RecordWriter::segment_path(&writer.dir, 0);
        let mut bytes = std::fs::read(&file).unwrap();
        // Inside the section's bytes: the tables at the end are far smaller.
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;
        std::fs::write(&file, bytes).unwrap();
        let source = writer.finish(64 << 20).unwrap();
        let pinned = Pinned::cached(&source);
        let failed = (0..60)
            .filter(|&index| pinned.record_at(index).is_none())
            .count();
        assert!(failed > 0, "a changed block reads back");
        let error = source.take_error().expect("the failed read is noted");
        assert!(error.contains("records-1"), "{error}");
        assert!((0..60).any(|index| source.read_path(&path(index)).is_err()));
        drop(pinned);
        drop(source);
        workspace.remove().unwrap();
    }
}
