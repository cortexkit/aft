//! Import of legacy (pre-view) index artifacts into the v3 family stores.
//!
//! The legacy artifacts of one key are `index/<key>/cache.bin` (trigram
//! postings), `semantic/<key>/semantic.bin` (vectors) and
//! `symbols/<key>/symbols.bin` (parsed symbols). This importer converts what
//! can be proven equal to a cold build into rows of the family's v3 planes
//! (`blobs/v3/<family>/{trigram,semantic,symbol}.sqlite`), so the base or
//! worker that later needs those files finds the rows by key instead of
//! re-tokenizing, re-embedding or re-parsing them.
//!
//! What it never does:
//!
//! - write a v1 or v2 row, manifest, pointer or schema (it opens no v1/v2
//!   store at all);
//! - move, change or delete a legacy file: only `aft cache prune-legacy`
//!   removes legacy sets, and only after the retention window;
//! - fall back to a legacy reader when v3 admission refuses a reservation:
//!   the plane reports a named storage gap and serves what v3 already holds;
//! - call an embedding model or the tree-sitter parser for an imported row
//!   of the trigram or symbol plane. The semantic plane re-chunks the
//!   checkout's bytes to prove the stored rows match, as the v2 import does,
//!   but never calls a model.
//!
//! Inputs of any size are streamed: memory is bounded by the per-file tables
//! of an artifact plus a working-set budget, never by the input size.
//!
//! Progress is recorded in the import ledger `blobs/v3/<family>/imports.sqlite`
//! (format in docs/design/views-v3-format.md, *Import ledger and symbol
//! plane*), one row per legacy key, artifact, producer and destination plane:
//!
//! ```text
//! (absent) → claimed → staged → validated → registered → done
//!                    ↘ rejected{reason} (from any non-terminal state)
//! ```
//!
//! Every transition is a compare-and-set on the row's state and owner token,
//! so a crash at any boundary leaves a row the next attempt resumes by
//! repeating at most one idempotent step.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::per_checkout::{convert_semantic_file, now_ms, small_summary_budget, BundleEntry};
use crate::blob_store::v2::{
    to_hex, ContentHash, FamilyKey, FamilyPlane, TrigramKey, TrigramPolicy,
};
use crate::blob_store::v3;
use crate::views::segment_store::{TrigramFlag, TrigramPayload, TrigramRecord};
use crate::views::semantic::SemanticProducer;
use crate::views::RelPath;

/// Format and schema of the v3 import ledger this build writes and reads.
pub const LEDGER_FORMAT_VERSION: i64 = 1;
/// `PRAGMA application_id` of the ledger ("AFTI").
pub const LEDGER_APPLICATION_ID: i64 = 0x4146_5449;
/// Working-set budget of the trigram inversion when the caller sets none.
pub const DEFAULT_WORKING_SET: usize = 64 << 20;
/// Payload bytes registered by one v3 publication.
const REGISTER_BATCH_BYTES: u64 = 32 << 20;
const STAGED_MAGIC: &[u8; 8] = b"AFTIMP03";
/// Size of one sequential read of a legacy source.
const READ_CHUNK: usize = 64 * 1024;
/// Longest path field accepted from a legacy artifact or a staged bundle.
const MAX_PATH_BYTES: usize = 64 * 1024;
/// Longest single non-path field (a snippet, an embedded text, a vector or a
/// staged payload) the decoders allocate.
const MAX_FIELD_BYTES: usize = 256 << 20;
/// Upper bound on files listed by one legacy artifact.
const MAX_LEGACY_FILES: usize = 2_000_000;

// Layout constants of the legacy trigram `cache.bin`, as written by the
// search index of the previous release (`search_index::write_cache_file_from_sources`).
const LEGACY_CACHE_MAGIC: u32 = 0x3144_4958;
const LEGACY_INDEX_MAGIC: &[u8; 8] = b"AFTIDX01";
const LEGACY_LOOKUP_MAGIC: &[u8; 8] = b"AFTLKP01";
const LEGACY_POSTING_BYTES: usize = 6;
const LEGACY_LOOKUP_ENTRY_BYTES: usize = 16;

/// Refusals of a v3 reservation that are a lack of storage, reported as the
/// plane's named storage gap rather than as an import failure.
const STORAGE_GAP_REFUSALS: [&str; 3] = [
    "v3_namespace_budget",
    "device_free_space_floor",
    "reservation_ledger_unreadable",
];

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub enum V3ImportError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Store(v3::Error),
    /// The ledger was written by a newer AFT.
    NewerLedger(i64),
    /// The legacy key or family cannot name a directory.
    InvalidKey(String),
    /// Another owner took the row over between two steps of this attempt.
    LostClaim(String),
    Corrupt(String),
}

impl fmt::Display for V3ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "v3 legacy import I/O error: {error}"),
            Self::Sqlite(error) => write!(f, "v3 legacy import ledger error: {error}"),
            Self::Store(error) => write!(f, "v3 legacy import store error: {error}"),
            Self::NewerLedger(version) => write!(
                f,
                "v3 import ledger format {version} is newer than this build reads ({LEDGER_FORMAT_VERSION})"
            ),
            Self::InvalidKey(key) => write!(f, "invalid legacy key or family `{key}`"),
            Self::LostClaim(row) => write!(f, "v3 import claim of {row} was taken over"),
            Self::Corrupt(reason) => write!(f, "v3 import state is corrupt: {reason}"),
        }
    }
}

impl std::error::Error for V3ImportError {}

impl From<io::Error> for V3ImportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for V3ImportError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<v3::Error> for V3ImportError {
    fn from(error: v3::Error) -> Self {
        Self::Store(error)
    }
}

pub type V3Result<T> = Result<T, V3ImportError>;

// ---------------------------------------------------------------------------
// Artifacts and paths

/// One legacy artifact the v3 importer reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum LegacyArtifact {
    Trigram,
    Semantic,
    Symbols,
}

impl LegacyArtifact {
    pub const ALL: [Self; 3] = [Self::Trigram, Self::Semantic, Self::Symbols];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trigram => "trigram",
            Self::Semantic => "semantic",
            Self::Symbols => "symbols",
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|artifact| artifact.as_str() == value)
    }

    /// The v3 plane this artifact's rows go to.
    pub const fn plane(self) -> FamilyPlane {
        match self {
            Self::Trigram => FamilyPlane::Trigram,
            Self::Semantic => FamilyPlane::Semantic,
            Self::Symbols => FamilyPlane::Symbol,
        }
    }

    /// The legacy file this artifact is read from.
    pub fn source_path(self, storage: &Path, legacy_key: &str) -> PathBuf {
        match self {
            Self::Trigram => super::per_checkout::legacy_trigram_path(storage, legacy_key),
            Self::Semantic => super::per_checkout::legacy_semantic_path(storage, legacy_key),
            Self::Symbols => crate::symbol_cache_disk::cache_path(storage, legacy_key),
        }
    }
}

/// `<storage>/migration/v3/<family>/<legacy_key>`: staging bundles. It lies
/// outside `blobs/v3` and outside every directory older binaries walk.
pub fn staging_dir(storage: &Path, family: &str, legacy_key: &str) -> PathBuf {
    storage
        .join("migration")
        .join("v3")
        .join(family)
        .join(legacy_key)
}

fn hash_prefix(hash: &[u8; 32]) -> String {
    to_hex(hash)[..32].to_owned()
}

// ---------------------------------------------------------------------------
// Bounded reads of a legacy source

/// What a decode read and held, for the bounded-memory contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReadStats {
    /// Bytes read from the legacy source, summed over every pass.
    pub bytes_read: u64,
    /// Largest single read from the legacy source.
    pub largest_read: usize,
    /// Passes over the trigram posting blob after the checksum pass.
    pub posting_passes: u32,
    /// Largest number of bytes of decoded postings held at once.
    pub peak_working_set: u64,
}

/// The file identity a decode started from. A legacy writer replaces its
/// file by rename, so an unchanged identity on the open handle means the
/// bytes did not change under the decode.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SourceIdentity {
    len: u64,
    modified: Option<SystemTime>,
}

impl SourceIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

/// A legacy source opened once, read only through bounded reads. It is opened
/// read-only and never written, truncated or repaired.
pub(super) struct Source {
    file: fs::File,
    len: u64,
    identity: SourceIdentity,
    stats: ReadStats,
}

impl Source {
    fn open(path: &Path) -> Result<Self, String> {
        let file = fs::File::open(path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => "no legacy artifact".to_owned(),
            _ => format!("legacy artifact unreadable: {error}"),
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("legacy artifact unreadable: {error}"))?;
        if !metadata.is_file() {
            return Err("legacy artifact is not a file".to_owned());
        }
        Ok(Self {
            len: metadata.len(),
            identity: SourceIdentity::of(&metadata),
            file,
            stats: ReadStats::default(),
        })
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        offset
            .checked_add(buf.len() as u64)
            .filter(|end| *end <= self.len)
            .ok_or("unexpected end of legacy artifact")?;
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(buf))
            .map_err(|error| format!("legacy artifact read failed: {error}"))?;
        self.stats.bytes_read += buf.len() as u64;
        self.stats.largest_read = self.stats.largest_read.max(buf.len());
        Ok(())
    }

    /// A sequential reader over the whole source, through bounded reads.
    pub(super) fn sequential(&mut self) -> SourceRead<'_> {
        SourceRead {
            source: self,
            offset: 0,
        }
    }

    /// Whether the open file still has the identity it had when opened.
    fn unchanged(&self) -> bool {
        self.file
            .metadata()
            .is_ok_and(|metadata| SourceIdentity::of(&metadata) == self.identity)
    }

    /// One sequential pass over the whole file: its BLAKE3 and the CRC32 of
    /// each `[start, end)` range in `crc_ranges`.
    pub(super) fn checksum_pass(
        &mut self,
        crc_ranges: &[(u64, u64)],
    ) -> Result<([u8; 32], Vec<u32>), String> {
        let mut hasher = blake3::Hasher::new();
        let mut crcs = vec![crc32fast::Hasher::new(); crc_ranges.len()];
        let mut buffer = vec![0_u8; READ_CHUNK];
        let mut offset = 0_u64;
        while offset < self.len {
            let len = (self.len - offset).min(READ_CHUNK as u64) as usize;
            self.read_at(offset, &mut buffer[..len])?;
            let chunk = &buffer[..len];
            hasher.update(chunk);
            for ((start, end), crc) in crc_ranges.iter().zip(crcs.iter_mut()) {
                let from = (*start).max(offset);
                let to = (*end).min(offset + len as u64);
                if from < to {
                    crc.update(&chunk[(from - offset) as usize..(to - offset) as usize]);
                }
            }
            offset += len as u64;
        }
        Ok((
            *hasher.finalize().as_bytes(),
            crcs.into_iter().map(crc32fast::Hasher::finalize).collect(),
        ))
    }
}

/// `Read` over a [`Source`] from its start; every read is at most
/// [`READ_CHUNK`] bytes.
pub(super) struct SourceRead<'a> {
    source: &'a mut Source,
    offset: u64,
}

impl Read for SourceRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.source.len - self.offset;
        let len = (buf.len() as u64).min(remaining).min(READ_CHUNK as u64) as usize;
        if len == 0 {
            return Ok(0);
        }
        self.source
            .read_at(self.offset, &mut buf[..len])
            .map_err(io::Error::other)?;
        self.offset += len as u64;
        Ok(len)
    }
}

/// A little-endian cursor over `[offset, end)` of a source, refilled one
/// bounded window at a time.
struct SeqReader<'a> {
    source: &'a mut Source,
    window: Vec<u8>,
    window_start: u64,
    offset: u64,
    end: u64,
}

impl<'a> SeqReader<'a> {
    fn new(source: &'a mut Source, offset: u64, end: u64) -> Self {
        Self {
            source,
            window: Vec::new(),
            window_start: offset,
            offset,
            end,
        }
    }

    fn position(&self) -> u64 {
        self.offset
    }

    fn is_exhausted(&self) -> bool {
        self.offset == self.end
    }

    fn bytes(&mut self, out: &mut [u8]) -> Result<(), String> {
        if self.offset + out.len() as u64 > self.end {
            return Err("unexpected end of data".to_owned());
        }
        let mut written = 0;
        while written < out.len() {
            let window_end = self.window_start + self.window.len() as u64;
            if self.offset < self.window_start || self.offset >= window_end {
                let len = (self.end - self.offset).min(READ_CHUNK as u64) as usize;
                self.window.resize(len, 0);
                self.source.read_at(self.offset, &mut self.window)?;
                self.window_start = self.offset;
                continue;
            }
            let at = (self.offset - self.window_start) as usize;
            let take = (self.window.len() - at).min(out.len() - written);
            out[written..written + take].copy_from_slice(&self.window[at..at + take]);
            written += take;
            self.offset += take as u64;
        }
        Ok(())
    }

    fn skip(&mut self, len: u64) -> Result<(), String> {
        self.offset = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.end)
            .ok_or("unexpected end of data")?;
        Ok(())
    }

    fn vec(&mut self, len: usize, cap: usize) -> Result<Vec<u8>, String> {
        if len > cap || len as u64 > self.end - self.offset {
            return Err(format!("field of {len} bytes exceeds its bound"));
        }
        let mut out = vec![0_u8; len];
        self.bytes(&mut out)?;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        let mut out = [0_u8; 1];
        self.bytes(&mut out)?;
        Ok(out[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let mut out = [0_u8; 4];
        self.bytes(&mut out)?;
        Ok(u32::from_le_bytes(out))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let mut out = [0_u8; 8];
        self.bytes(&mut out)?;
        Ok(u64::from_le_bytes(out))
    }

    fn array32(&mut self) -> Result<[u8; 32], String> {
        let mut out = [0_u8; 32];
        self.bytes(&mut out)?;
        Ok(out)
    }

    /// A `u32`-length-prefixed field.
    fn field(&mut self, cap: usize) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        self.vec(len, cap)
    }

    fn string(&mut self, cap: usize) -> Result<String, String> {
        String::from_utf8(self.field(cap)?).map_err(|_| "legacy string is not UTF-8".to_owned())
    }
}

// ---------------------------------------------------------------------------
// Streaming conversion: trigram

struct TrigramFile {
    rel_path: RelPath,
    size: u64,
    content: [u8; 32],
    unindexed: bool,
}

/// Where the sections of a checksummed legacy `cache.bin` are.
struct TrigramLayout {
    files: Vec<TrigramFile>,
    blob_start: u64,
    blob_len: u64,
    lookup_entries_start: u64,
    entry_count: u64,
}

/// Verifies both checksums of a legacy `cache.bin` in one sequential pass and
/// reads its file table. Returns the layout and the file's BLAKE3.
fn trigram_layout(
    source: &mut Source,
    policy: &TrigramPolicy,
) -> Result<(TrigramLayout, [u8; 32]), String> {
    let source_len = source.len;
    let mut header = SeqReader::new(source, 0, 16.min(source_len));
    if header.u32()? != LEGACY_CACHE_MAGIC {
        return Err("not a trigram cache".to_owned());
    }
    let version = header.u32()?;
    if version != crate::search_index::INDEX_FORMAT_VERSION {
        return Err(format!(
            "trigram cache version {version} differs from this build's {}",
            crate::search_index::INDEX_FORMAT_VERSION
        ));
    }
    let postings_len_total = header.u64()?;
    let postings_end = 16_u64
        .checked_add(postings_len_total)
        .filter(|end| *end <= source.len && postings_len_total >= 4)
        .ok_or("trigram cache postings section overruns the file")?;
    let crc_at = postings_end - 4;
    let lookup_crc_at = source
        .len
        .checked_sub(4)
        .filter(|at| *at >= postings_end)
        .ok_or("trigram cache lookup section is truncated")?;
    let (source_hash, crcs) =
        source.checksum_pass(&[(16, crc_at), (postings_end, lookup_crc_at)])?;
    let mut stored = [0_u8; 4];
    source.read_at(crc_at, &mut stored)?;
    if crcs[0] != u32::from_le_bytes(stored) {
        return Err("trigram cache postings checksum mismatch".to_owned());
    }
    source.read_at(lookup_crc_at, &mut stored)?;
    if crcs[1] != u32::from_le_bytes(stored) {
        return Err("trigram cache lookup checksum mismatch".to_owned());
    }

    let mut reader = SeqReader::new(source, 16, crc_at);
    let mut magic = [0_u8; 8];
    reader.bytes(&mut magic)?;
    if &magic != LEGACY_INDEX_MAGIC || reader.u32()? != version {
        return Err("trigram cache postings header mismatch".to_owned());
    }
    let head_len = reader.u32()?;
    let root_len = reader.u32()?;
    let ignore_len = reader.u32()?;
    let max_file_size = reader.u64()?;
    if max_file_size != policy.max_file_size {
        return Err(format!(
            "trigram cache max_file_size {max_file_size} differs from the family policy {}",
            policy.max_file_size
        ));
    }
    let file_count = reader.u32()? as usize;
    if file_count > MAX_LEGACY_FILES {
        return Err(format!("trigram cache lists {file_count} files"));
    }
    reader.skip(u64::from(head_len))?;
    reader.skip(u64::from(root_len))?;
    reader.skip(u64::from(ignore_len))?;
    let mut files = Vec::with_capacity(file_count.min((crc_at / 57) as usize));
    let mut seen = HashSet::new();
    for _ in 0..file_count {
        let unindexed = match reader.u8()? {
            0 => false,
            1 => true,
            other => return Err(format!("trigram cache file flag {other}")),
        };
        let path_len = reader.u32()? as usize;
        let size = reader.u64()?;
        let _secs = reader.u64()?;
        let _nanos = reader.u32()?;
        let content = reader.array32()?;
        let path = reader.vec(path_len, MAX_PATH_BYTES)?;
        let path = std::str::from_utf8(&path).map_err(|_| "trigram cache path is not UTF-8")?;
        let path = Path::new(path);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "trigram cache path {} is not relative",
                path.display()
            ));
        }
        let rel_path = RelPath::from_os_path(path).map_err(|error| error.to_string())?;
        // A digest per path keeps the duplicate check smaller than the table.
        if !seen.insert(*blake3::hash(rel_path.as_bytes()).as_bytes()) {
            return Err("trigram cache lists a path twice".to_owned());
        }
        files.push(TrigramFile {
            rel_path,
            size,
            content,
            unindexed,
        });
    }
    drop(seen);
    let blob_len = reader.u64()?;
    let blob_start = reader.position();
    reader.skip(blob_len)?;
    if blob_len % LEGACY_POSTING_BYTES as u64 != 0 {
        return Err("trigram cache posting blob has a partial record".to_owned());
    }

    let mut lookup = SeqReader::new(source, postings_end, lookup_crc_at);
    lookup.bytes(&mut magic)?;
    if &magic != LEGACY_LOOKUP_MAGIC || lookup.u32()? != version {
        return Err("trigram cache lookup header mismatch".to_owned());
    }
    let entry_count = u64::from(lookup.u32()?);
    let lookup_entries_start = lookup.position();
    if entry_count
        .checked_mul(LEGACY_LOOKUP_ENTRY_BYTES as u64)
        .and_then(|len| lookup_entries_start.checked_add(len))
        != Some(lookup_crc_at)
    {
        return Err("trigram cache lookup table does not fill its section".to_owned());
    }
    Ok((
        TrigramLayout {
            files,
            blob_start,
            blob_len,
            lookup_entries_start,
            entry_count,
        },
        source_hash,
    ))
}

/// Calls `visit(trigram, postings)` for every posting list, in lookup order,
/// with each list delivered in bounded chunks of whole postings. Checks that
/// the lookup is strictly sorted and every list lies inside the blob.
fn for_each_posting_chunk(
    source: &mut Source,
    layout: &TrigramLayout,
    visit: &mut dyn FnMut(u32, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    const ENTRIES_PER_READ: usize = READ_CHUNK / LEGACY_LOOKUP_ENTRY_BYTES;
    const POSTINGS_PER_READ: usize = READ_CHUNK / LEGACY_POSTING_BYTES;
    let mut entries = vec![0_u8; ENTRIES_PER_READ * LEGACY_LOOKUP_ENTRY_BYTES];
    let mut postings = vec![0_u8; POSTINGS_PER_READ * LEGACY_POSTING_BYTES];
    let mut previous = None;
    let mut index = 0_u64;
    while index < layout.entry_count {
        let batch = (layout.entry_count - index).min(ENTRIES_PER_READ as u64) as usize;
        let bytes = &mut entries[..batch * LEGACY_LOOKUP_ENTRY_BYTES];
        source.read_at(
            layout.lookup_entries_start + index * LEGACY_LOOKUP_ENTRY_BYTES as u64,
            bytes,
        )?;
        for entry in bytes.chunks_exact(LEGACY_LOOKUP_ENTRY_BYTES) {
            let trigram = u32::from_le_bytes(entry[..4].try_into().expect("4"));
            let offset = u64::from_le_bytes(entry[4..12].try_into().expect("8"));
            let count = u64::from(u32::from_le_bytes(entry[12..16].try_into().expect("4")));
            if previous.is_some_and(|previous| previous >= trigram) {
                return Err("trigram cache lookup is not strictly sorted".to_owned());
            }
            previous = Some(trigram);
            count
                .checked_mul(LEGACY_POSTING_BYTES as u64)
                .and_then(|len| offset.checked_add(len))
                .filter(|end| *end <= layout.blob_len)
                .ok_or("trigram cache posting list overruns the blob")?;
            let mut done = 0_u64;
            while done < count {
                let take = (count - done).min(POSTINGS_PER_READ as u64) as usize;
                let chunk = &mut postings[..take * LEGACY_POSTING_BYTES];
                source.read_at(
                    layout.blob_start + offset + done * LEGACY_POSTING_BYTES as u64,
                    chunk,
                )?;
                visit(trigram, chunk)?;
                done += take as u64;
            }
        }
        index += batch as u64;
    }
    Ok(())
}

/// Converts a legacy `cache.bin` into canonical per-file trigram entries,
/// handing each to `emit`, with memory bounded by the file table plus
/// `working_set` bytes of decoded postings (or one file's postings, if a
/// single file needs more).
///
/// The legacy postings are the search index's own posting fold of each file,
/// so inverting them gives exactly the payload `TrigramPayload::extract`
/// computes from the same bytes; no source file is read. Anything that cannot
/// be proven equal is a rejection: another index or tokenizer version, another
/// size limit, a failed checksum, a path that is not relative, or a malformed
/// table. Files without a recorded content hash are left out.
///
/// The inversion runs in file-id ranges: one counting pass over the posting
/// blob sizes each file's postings, then one pass per range collects the
/// postings of that range's files only.
fn convert_trigram_streaming(
    source: &mut Source,
    policy: &TrigramPolicy,
    working_set: usize,
    emit: &mut dyn FnMut(BundleEntry) -> Result<(), String>,
) -> Result<[u8; 32], String> {
    let (layout, source_hash) = trigram_layout(source, policy)?;
    let file_count = layout.files.len();
    let mut counts = vec![0_u32; file_count];
    let mut last: Vec<Option<u32>> = vec![None; file_count];
    for_each_posting_chunk(source, &layout, &mut |trigram, chunk| {
        for posting in chunk.chunks_exact(LEGACY_POSTING_BYTES) {
            let file_id = u32::from_le_bytes(posting[..4].try_into().expect("4")) as usize;
            let slot = last
                .get_mut(file_id)
                .ok_or("trigram cache posting names an unknown file")?;
            if *slot == Some(trigram) {
                return Err("trigram cache lists one file twice under a trigram".to_owned());
            }
            *slot = Some(trigram);
            counts[file_id] += 1;
        }
        Ok(())
    })?;
    drop(last);
    source.stats.posting_passes += 1;

    let record_bytes = std::mem::size_of::<TrigramRecord>() as u64;
    let budget = working_set.max(1) as u64;
    let mut start = 0;
    while start < file_count {
        let mut end = start;
        let mut held = 0_u64;
        while end < file_count {
            let next = u64::from(counts[end]) * record_bytes;
            if end > start && held + next > budget {
                break;
            }
            held += next;
            end += 1;
        }
        source.stats.peak_working_set = source.stats.peak_working_set.max(held);
        let mut records: Vec<Vec<TrigramRecord>> = counts[start..end]
            .iter()
            .map(|count| Vec::with_capacity(*count as usize))
            .collect();
        for_each_posting_chunk(source, &layout, &mut |trigram, chunk| {
            for posting in chunk.chunks_exact(LEGACY_POSTING_BYTES) {
                let file_id = u32::from_le_bytes(posting[..4].try_into().expect("4")) as usize;
                if (start..end).contains(&file_id) {
                    records[file_id - start].push(TrigramRecord {
                        trigram,
                        next_mask: posting[4],
                        loc_mask: posting[5],
                    });
                }
            }
            Ok(())
        })?;
        source.stats.posting_passes += 1;
        for (file, records) in layout.files[start..end].iter().zip(records) {
            if file.content == [0; 32] {
                continue;
            }
            let flag = if !file.unindexed {
                TrigramFlag::Indexed
            } else if file.size > policy.max_file_size {
                TrigramFlag::UnindexedOversize
            } else {
                TrigramFlag::UnindexedBinary
            };
            if flag != TrigramFlag::Indexed && !records.is_empty() {
                return Err("trigram cache holds postings for an unindexed file".to_owned());
            }
            let content = ContentHash::from_bytes(file.content);
            emit(BundleEntry {
                rel_path: file.rel_path.clone(),
                content,
                size: file.size,
                key: *TrigramKey {
                    content,
                    policy: *policy,
                }
                .family_key()
                .as_bytes(),
                payload: TrigramPayload { flag, records }.encode(),
            })?;
        }
        start = end;
    }
    Ok(source_hash)
}

// ---------------------------------------------------------------------------
// Streaming conversion: semantic

struct SemanticHeader {
    version: u8,
    dimension: usize,
    entry_count: u64,
}

fn semantic_header(
    reader: &mut SeqReader<'_>,
    producer: &SemanticProducer,
) -> Result<SemanticHeader, String> {
    let version = reader.u8()?;
    if version != super::LEGACY_SEMANTIC_V6 && version != super::LEGACY_SEMANTIC_V7 {
        return Err(format!(
            "semantic.bin version {version} has no established producer stamps"
        ));
    }
    let dimension = reader.u32()? as usize;
    if dimension == 0 || dimension > super::MAX_LEGACY_DIMENSION {
        return Err(format!("invalid semantic dimension {dimension}"));
    }
    let entry_count = reader.u32()? as usize;
    if entry_count > super::MAX_LEGACY_ROWS {
        return Err(format!("too many semantic entries {entry_count}"));
    }
    let fingerprint = reader.string(MAX_FIELD_BYTES)?;
    if !super::fingerprint_is_compatible(&fingerprint, &producer.model_fingerprint) {
        return Err("model fingerprint or chunker version is incompatible".to_owned());
    }
    Ok(SemanticHeader {
        version,
        dimension,
        entry_count: entry_count as u64,
    })
}

/// Reads the metadata table: each path's recorded content hash.
fn semantic_metadata(reader: &mut SeqReader<'_>) -> Result<BTreeMap<Vec<u8>, [u8; 32]>, String> {
    let count = reader.u32()? as usize;
    if count > super::MAX_LEGACY_ROWS {
        return Err(format!("too many semantic metadata rows {count}"));
    }
    let mut metadata = BTreeMap::new();
    for _ in 0..count {
        let path = reader.field(MAX_PATH_BYTES)?;
        let _seconds = reader.u64()?;
        let nanos = reader.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(format!("invalid semantic mtime nanos {nanos}"));
        }
        let _size = reader.u64()?;
        let content_hash = reader.array32()?;
        if metadata.insert(path, content_hash).is_some() {
            return Err("duplicate semantic metadata path".to_owned());
        }
    }
    Ok(metadata)
}

fn semantic_entry(
    reader: &mut SeqReader<'_>,
    header: &SemanticHeader,
) -> Result<(Vec<u8>, super::LegacyEntry), String> {
    let path = reader.field(MAX_PATH_BYTES)?;
    let name = reader.string(MAX_FIELD_BYTES)?;
    let qualified_name = if header.version == super::LEGACY_SEMANTIC_V7 {
        let qualified = reader.string(MAX_FIELD_BYTES)?;
        (!qualified.is_empty()).then_some(qualified)
    } else {
        None
    };
    let kind = reader.u8()?;
    let start_line = reader.u32()?;
    let end_line = reader.u32()?;
    let exported = reader.u8()? != 0;
    let snippet = reader.string(MAX_FIELD_BYTES)?;
    let embed_text = reader.string(MAX_FIELD_BYTES)?;
    let vector_len = header
        .dimension
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or("semantic vector length overflow")?;
    let vector = reader.vec(vector_len, MAX_FIELD_BYTES)?;
    Ok((
        path,
        super::LegacyEntry {
            name,
            qualified_name,
            kind,
            start_line,
            end_line,
            exported,
            snippet,
            embed_text,
            vector,
        },
    ))
}

fn path_digest(path: &[u8]) -> [u8; 32] {
    *blake3::hash(path).as_bytes()
}

/// Converts a legacy `semantic.bin` into view payloads under `producer`, one
/// file's rows at a time, handing each imported file to `emit`. Returns the
/// file's BLAKE3 and the number of files left for the plane to embed.
///
/// The decoder is read-only and strict: a version without producer stamps,
/// another model or chunker, or trailing bytes after the snapshot (appended
/// segments or a torn tail) reject the whole artifact. Nothing is repaired or
/// folded. Each file is then checked as the v2 import does
/// (`convert_semantic_file`): its current bytes must still hash to the
/// snapshot's hash and chunk into exactly the snapshot's rows. A path whose
/// rows are not contiguous in the snapshot is left for the plane.
fn convert_semantic_streaming(
    source: &mut Source,
    root: &Path,
    producer: &SemanticProducer,
    emit: &mut dyn FnMut(BundleEntry) -> Result<(), String>,
) -> Result<([u8; 32], u64), String> {
    let (source_hash, _) = source.checksum_pass(&[])?;
    let len = source.len;
    // Pass 1: validate the whole snapshot and find paths whose rows are split.
    let mut seen = HashSet::new();
    let mut split = HashSet::new();
    {
        let mut reader = SeqReader::new(source, 0, len);
        let header = semantic_header(&mut reader, producer)?;
        semantic_metadata(&mut reader)?;
        let mut current: Option<[u8; 32]> = None;
        for _ in 0..header.entry_count {
            let (path, _) = semantic_entry(&mut reader, &header)?;
            let digest = path_digest(&path);
            if current != Some(digest) {
                if !seen.insert(digest) {
                    split.insert(digest);
                }
                current = Some(digest);
            }
        }
        if !reader.is_exhausted() {
            return Err(
                "trailing bytes after semantic snapshot (segments or a torn tail are not repaired)"
                    .to_owned(),
            );
        }
    }
    // Pass 2: convert one contiguous group of rows at a time.
    let root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let summary_budget = small_summary_budget(producer);
    let mut reader = SeqReader::new(source, 0, len);
    let header = semantic_header(&mut reader, producer)?;
    let metadata = semantic_metadata(&mut reader)?;
    let mut skipped = 0_u64;
    let mut convert = |path: Vec<u8>, entries: Vec<super::LegacyEntry>| -> Result<(), String> {
        let digest = path_digest(&path);
        let Some(content_hash) = metadata.get(&path).copied() else {
            // Rows without metadata cannot prove they belong to this root.
            skipped += 1;
            return Ok(());
        };
        if split.contains(&digest) {
            skipped += 1;
            return Ok(());
        }
        let file = super::LegacyFile {
            path,
            content_hash,
            entries,
        };
        match convert_semantic_file(&root, producer, summary_budget, file) {
            Some(entry) => emit(entry),
            None => {
                skipped += 1;
                Ok(())
            }
        }
    };
    let mut group: Option<(Vec<u8>, Vec<super::LegacyEntry>)> = None;
    for _ in 0..header.entry_count {
        let (path, entry) = semantic_entry(&mut reader, &header)?;
        match &mut group {
            Some((current, rows)) if *current == path => rows.push(entry),
            _ => {
                if let Some((path, rows)) = group.take() {
                    convert(path, rows)?;
                }
                group = Some((path, vec![entry]));
            }
        }
    }
    if let Some((path, rows)) = group.take() {
        convert(path, rows)?;
    }
    // A file the snapshot lists without rows is imported when this release
    // also chunks it into no rows, as the v2 import does.
    for path in metadata.keys() {
        if !seen.contains(&path_digest(path)) {
            convert(path.clone(), Vec::new())?;
        }
    }
    Ok((source_hash, skipped))
}

// ---------------------------------------------------------------------------
// Staged bundles

/// Streams converted entries to a staging file. The final name is derived
/// from the hash of the whole file, so a resumed attempt finds exactly the
/// bytes its ledger row recorded.
struct StagedWriter {
    file: BufWriter<fs::File>,
    temporary: PathBuf,
    body: blake3::Hasher,
    whole: blake3::Hasher,
    count: u64,
}

impl StagedWriter {
    fn create(dir: &Path, artifact: LegacyArtifact) -> io::Result<Self> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        crate::private_storage::create_dir_all(dir)?;
        let temporary = dir.join(format!(
            "{}.tmp.{}.{}.{}",
            artifact.as_str(),
            std::process::id(),
            now_ms(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let file = crate::private_storage::options()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = Self {
            file: BufWriter::new(file),
            temporary,
            body: blake3::Hasher::new(),
            whole: blake3::Hasher::new(),
            count: 0,
        };
        writer.write(STAGED_MAGIC)?;
        writer.write(&[artifact as u8])?;
        Ok(writer)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.body.update(bytes);
        self.whole.update(bytes);
        self.file.write_all(bytes)
    }

    fn entry(&mut self, entry: &BundleEntry) -> io::Result<()> {
        let path = entry.rel_path.as_bytes();
        let payload_len = u32::try_from(entry.payload.len())
            .ok()
            .filter(|len| *len as usize <= MAX_FIELD_BYTES)
            .ok_or_else(|| io::Error::other("staged payload too large"))?;
        self.write(&[1])?;
        self.write(&(path.len() as u32).to_le_bytes())?;
        self.write(path)?;
        self.write(entry.content.as_bytes())?;
        self.write(&entry.size.to_le_bytes())?;
        self.write(&entry.key)?;
        self.write(&payload_len.to_le_bytes())?;
        self.write(&entry.payload)?;
        self.count += 1;
        Ok(())
    }

    /// Writes the trailer, makes the file durable under its final name and
    /// returns that name and the file's hash.
    fn finish(mut self, dir: &Path, artifact: LegacyArtifact) -> io::Result<(PathBuf, [u8; 32])> {
        let digest = *self.body.finalize().as_bytes();
        self.write(&[0])?;
        let count = self.count.to_le_bytes();
        self.write(&count)?;
        self.write(&digest)?;
        let hash = *self.whole.finalize().as_bytes();
        let file = self.file.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        drop(file);
        let path = staged_path(dir, artifact, &hash);
        crate::fs_lock::rename_over(&self.temporary, &path)?;
        crate::fs_lock::sync_parent(&path);
        Ok((path, hash))
    }

    fn abandon(self) {
        let temporary = self.temporary.clone();
        drop(self);
        let _ = fs::remove_file(temporary);
    }
}

pub(super) fn staged_path(dir: &Path, artifact: LegacyArtifact, hash: &[u8; 32]) -> PathBuf {
    dir.join(format!(
        "{}-{}-{}.bundle",
        artifact.as_str(),
        artifact.plane().as_str(),
        hash_prefix(hash)
    ))
}

/// Streams the entries of a staged bundle to `visit`, then checks its
/// trailer, entry count and whole-file hash against `expected`. `visit` may
/// see entries of a bundle that later fails these checks, so a caller that
/// must not act on such entries runs a checking pass first.
fn read_staged(
    path: &Path,
    artifact: LegacyArtifact,
    expected: &[u8; 32],
    visit: &mut dyn FnMut(BundleEntry) -> Result<(), String>,
) -> Result<u64, String> {
    struct Hashing<R> {
        inner: R,
        body: blake3::Hasher,
        whole: blake3::Hasher,
        in_body: bool,
    }
    impl<R: Read> Hashing<R> {
        fn take(&mut self, out: &mut [u8]) -> Result<(), String> {
            self.inner
                .read_exact(out)
                .map_err(|error| format!("staged bundle unreadable: {error}"))?;
            if self.in_body {
                self.body.update(out);
            }
            self.whole.update(out);
            Ok(())
        }
        fn u32(&mut self) -> Result<u32, String> {
            let mut out = [0_u8; 4];
            self.take(&mut out)?;
            Ok(u32::from_le_bytes(out))
        }
        fn u64(&mut self) -> Result<u64, String> {
            let mut out = [0_u8; 8];
            self.take(&mut out)?;
            Ok(u64::from_le_bytes(out))
        }
        fn array32(&mut self) -> Result<[u8; 32], String> {
            let mut out = [0_u8; 32];
            self.take(&mut out)?;
            Ok(out)
        }
        fn vec(&mut self, len: usize, cap: usize) -> Result<Vec<u8>, String> {
            if len > cap {
                return Err("staged bundle field exceeds its bound".to_owned());
            }
            let mut out = vec![0_u8; len];
            self.take(&mut out)?;
            Ok(out)
        }
    }
    let file =
        fs::File::open(path).map_err(|error| format!("staged bundle unreadable: {error}"))?;
    let mut reader = Hashing {
        inner: BufReader::with_capacity(READ_CHUNK, file),
        body: blake3::Hasher::new(),
        whole: blake3::Hasher::new(),
        in_body: true,
    };
    let mut magic = [0_u8; 9];
    reader.take(&mut magic)?;
    if &magic[..8] != STAGED_MAGIC || magic[8] != artifact as u8 {
        return Err("staged bundle has another format or artifact".to_owned());
    }
    let mut count = 0_u64;
    loop {
        let mut marker = [0_u8; 1];
        // The end marker is the first trailer byte: it is not part of the
        // body digest the writer recorded.
        reader.in_body = false;
        reader.take(&mut marker)?;
        match marker[0] {
            0 => break,
            1 => {
                reader.body.update(&marker);
                reader.in_body = true;
            }
            other => return Err(format!("staged bundle marker {other}")),
        }
        let path_len = reader.u32()? as usize;
        let rel_path = RelPath::new(reader.vec(path_len, MAX_PATH_BYTES)?)
            .map_err(|error| error.to_string())?;
        let content = ContentHash::from_bytes(reader.array32()?);
        let size = reader.u64()?;
        let key = reader.array32()?;
        let payload_len = reader.u32()? as usize;
        let payload = reader.vec(payload_len, MAX_FIELD_BYTES)?;
        count += 1;
        visit(BundleEntry {
            rel_path,
            content,
            size,
            key,
            payload,
        })?;
    }
    let body_digest = *reader.body.finalize().as_bytes();
    let recorded_count = reader.u64()?;
    let recorded_digest = reader.array32()?;
    if recorded_count != count || recorded_digest != body_digest {
        return Err("staged bundle trailer does not match its entries".to_owned());
    }
    let mut extra = [0_u8; 1];
    if reader
        .inner
        .read(&mut extra)
        .map_err(|error| error.to_string())?
        != 0
    {
        return Err("staged bundle has trailing bytes".to_owned());
    }
    if reader.whole.finalize().as_bytes() != expected {
        return Err("staged bundle does not match its recorded hash".to_owned());
    }
    Ok(count)
}

/// The result of converting one legacy artifact to a staged bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Staged {
    pub path: PathBuf,
    pub hash: [u8; 32],
    pub source_hash: [u8; 32],
    pub source_len: u64,
    pub entries: u64,
    /// Files the artifact lists that were left for the plane to build.
    pub skipped: u64,
    pub stats: ReadStats,
}

/// What a staging attempt needs besides the artifact.
pub(super) struct StageInput<'a> {
    pub storage: &'a Path,
    pub legacy_key: &'a str,
    pub root: &'a Path,
    pub dir: &'a Path,
    pub trigram: &'a TrigramPolicy,
    pub semantic: Option<&'a SemanticProducer>,
    pub working_set: usize,
}

/// Converts `artifact` into a durable staged bundle. `Ok(Err(reason))` is a
/// rejection of the artifact: absent, incompatible, malformed, or changed
/// while it was read. `Err` is a failure to write the staging file.
pub(super) fn stage(
    artifact: LegacyArtifact,
    input: &StageInput<'_>,
) -> V3Result<Result<Staged, String>> {
    let mut source = match Source::open(&artifact.source_path(input.storage, input.legacy_key)) {
        Ok(source) => source,
        Err(reason) => return Ok(Err(reason)),
    };
    let mut writer = StagedWriter::create(input.dir, artifact)?;
    let mut write_error = None;
    let mut emit = |entry: BundleEntry| -> Result<(), String> {
        writer.entry(&entry).map_err(|error| {
            let message = error.to_string();
            write_error = Some(error);
            message
        })
    };
    let converted = match artifact {
        LegacyArtifact::Trigram => {
            convert_trigram_streaming(&mut source, input.trigram, input.working_set, &mut emit)
                .map(|hash| (hash, 0))
        }
        LegacyArtifact::Semantic => match input.semantic {
            Some(producer) => {
                convert_semantic_streaming(&mut source, input.root, producer, &mut emit)
            }
            None => Err("the request carries no semantic producer".to_owned()),
        },
        LegacyArtifact::Symbols => {
            super::v3_symbols::convert_symbols_streaming(&mut source, input.root, &mut emit)
        }
    };
    if let Some(error) = write_error {
        writer.abandon();
        return Err(error.into());
    }
    let (source_hash, skipped) = match converted {
        Ok(converted) => converted,
        Err(reason) => {
            writer.abandon();
            return Ok(Err(reason));
        }
    };
    if !source.unchanged() {
        writer.abandon();
        return Ok(Err(
            "the legacy artifact changed during the import".to_owned()
        ));
    }
    let entries = writer.count;
    let (path, hash) = writer.finish(input.dir, artifact)?;
    Ok(Ok(Staged {
        path,
        hash,
        source_hash,
        source_len: source.len,
        entries,
        skipped,
        stats: source.stats,
    }))
}

/// Re-reads a staged bundle and re-derives what can be derived from each
/// entry: a trigram entry's key from its content and the policy, and that
/// every payload decodes for its plane and producer.
pub(super) fn check_staged(
    artifact: LegacyArtifact,
    path: &Path,
    hash: &[u8; 32],
    input: &StageInput<'_>,
) -> Result<u64, String> {
    read_staged(path, artifact, hash, &mut |entry| match artifact {
        LegacyArtifact::Trigram => {
            TrigramPayload::decode(&entry.payload).map_err(|error| error.to_string())?;
            let key = TrigramKey {
                content: entry.content,
                policy: *input.trigram,
            }
            .family_key();
            (key.as_bytes() == &entry.key)
                .then_some(())
                .ok_or_else(|| "staged trigram key differs from its content".to_owned())
        }
        LegacyArtifact::Semantic => {
            let producer = input
                .semantic
                .ok_or("the request carries no semantic producer")?;
            let relative = crate::views::segment_store::rel_path_to_os(&entry.rel_path)
                .map_err(|error| error.to_string())?;
            crate::semantic_index::SemanticVectors::decode_view_payload(
                &entry.payload,
                &relative,
                &crate::semantic_index::ViewPayloadProducer {
                    chunker_version: &producer.chunker_version,
                    template_version: &producer.template_version,
                    model_fingerprint: &producer.model_fingerprint,
                },
            )
            .map(|_| ())
        }
        LegacyArtifact::Symbols => super::v3_symbols::check_staged_entry(&entry),
    })
}

/// Re-hashes a legacy source in one bounded sequential pass.
pub(super) fn source_identity(
    artifact: LegacyArtifact,
    storage: &Path,
    legacy_key: &str,
) -> Result<([u8; 32], u64), String> {
    let mut source = Source::open(&artifact.source_path(storage, legacy_key))?;
    let (hash, _) = source.checksum_pass(&[])?;
    Ok((hash, source.len))
}

// ---------------------------------------------------------------------------
// Registration into v3

/// What putting a staged bundle into its plane did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Registered {
    pub inserted: u64,
    /// Rows already present with an equal payload; only touched.
    pub reused: u64,
    /// Rows already present with another payload for the same key. The stored
    /// row is kept: semantic keys address the input and producer, not the
    /// model's output floats, and a stored trigram row came from the same bytes.
    pub kept_existing: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegisterOutcome {
    Registered(Registered),
    /// A reservation was refused for lack of storage. The plane serves what
    /// v3 already holds; nothing falls back to the legacy artifact.
    StorageGap(String),
}

fn storage_gap(error: &v3::Error) -> Option<String> {
    match error {
        v3::Error::Refused(reason) if STORAGE_GAP_REFUSALS.contains(reason) => {
            Some(format!("storage_gap:{reason}"))
        }
        _ => None,
    }
}

/// Puts every entry of a checked staged bundle into the destination plane,
/// in batches of at most [`REGISTER_BATCH_BYTES`] payload bytes, each one
/// v3 publication under its own reservation. Every put is idempotent, so a
/// resumed attempt repeats the batches a crash interrupted.
pub(super) fn register_staged(
    storage: &Path,
    family: &str,
    artifact: LegacyArtifact,
    path: &Path,
    hash: &[u8; 32],
    after_batch: &mut dyn FnMut(u64),
) -> V3Result<Result<RegisterOutcome, String>> {
    let plane = artifact.plane();
    let schema = plane.payload_schema();
    let mut totals = Registered::default();
    let mut batch: Vec<(FamilyKey, Vec<u8>)> = Vec::new();
    let mut batch_bytes = 0_u64;
    let mut batches = 0_u64;
    let mut failure: Option<V3ImportError> = None;
    let mut gap: Option<String> = None;
    let mut flush = |batch: &mut Vec<(FamilyKey, Vec<u8>)>,
                     batch_bytes: &mut u64,
                     totals: &mut Registered|
     -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }
        let request = batch_bytes.saturating_mul(2).saturating_add(1 << 20);
        let result = v3::publish(storage, family, request, None, |publication| {
            let mut done = Registered::default();
            for (key, payload) in batch.iter() {
                match publication.put_or_touch(key, payload, schema) {
                    Ok(true) => done.inserted += 1,
                    Ok(false) => done.reused += 1,
                    Err(v3::Error::Refused("payload_mismatch")) => {
                        publication.touch(&[*key])?;
                        done.kept_existing += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(done)
        });
        match result {
            Ok(done) => {
                totals.inserted += done.inserted;
                totals.reused += done.reused;
                totals.kept_existing += done.kept_existing;
                batches += 1;
                after_batch(batches);
                batch.clear();
                *batch_bytes = 0;
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                match storage_gap(&error) {
                    Some(name) => gap = Some(name),
                    None => failure = Some(error.into()),
                }
                Err(message)
            }
        }
    };
    let read = read_staged(path, artifact, hash, &mut |entry| {
        batch_bytes += entry.payload.len() as u64;
        batch.push((FamilyKey::new(plane, entry.key), entry.payload));
        if batch_bytes >= REGISTER_BATCH_BYTES {
            flush(&mut batch, &mut batch_bytes, &mut totals)?;
        }
        Ok(())
    });
    let read = read.and_then(|_| flush(&mut batch, &mut batch_bytes, &mut totals));
    if let Some(name) = gap {
        return Ok(Ok(RegisterOutcome::StorageGap(name)));
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(read.map(|_| RegisterOutcome::Registered(totals)))
}

#[cfg(test)]
#[path = "v3_import_tests.rs"]
pub(crate) mod tests;
