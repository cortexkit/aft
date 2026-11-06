//! Vectors the old meaning index already computed, which a fill takes
//! instead of asking the embedding model for them again.
//!
//! Before the data layer, AFT kept one meaning index per project key in
//! `semantic/<project key>/semantic.bin` under its storage folder: a base
//! snapshot (format versions 6 to 8), followed by the checksummed frames
//! later refreshes appended, each replacing the rows of the files it names.
//! Every row names its file by the path relative to the project root, and
//! holds a chunk's name, kind, line range, the exact text sent to the
//! embedder and the vector the model returned for it. The file table holds
//! the BLAKE3 digest of the bytes each file's chunks were cut from.
//!
//! A fill takes a chunk's vector from the snapshot only when all of these
//! hold, and embeds the chunk as it would without a snapshot otherwise:
//! - the snapshot was written for the configured model: its model
//!   fingerprint equals the configured one field for field, apart from the
//!   chunking version and two flags that record repairs of the snapshot
//!   itself;
//! - its chunking version equals the current one;
//! - the file's current bytes have the digest the snapshot recorded;
//! - a row of that file has the chunk's name, kind and line range, its
//!   qualified name too where the snapshot records one (version 7 on), and
//!   exactly the chunk's text.
//!
//! A file the snapshot lists as not fully embedded (version 8 keeps a table
//! of files with rows the embedder refused or that were too long for it)
//! gives nothing. A snapshot that cannot be read as a whole gives nothing
//! at all, except that a final frame cut short (a refresh killed while
//! appending) is left out, as the old index left it out.
//!
//! The snapshot is only ever read. It is read once, front to back, to learn
//! which file each row belongs to and where the row starts; a file's rows
//! are read again, one file at a time, only when a fill needs vectors for
//! that file. So memory grows with the number of files and rows, not with
//! the size of their texts and vectors. The file stays open while the plane
//! that reads it lives: a snapshot written in its place later is not seen,
//! and frames an older AFT appends later are not read.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::vectors::CutChunk;
use crate::semantic_index::SemanticIndexFingerprint;
use crate::symbols::SymbolKind;

/// The first format version that records content digests and paths
/// relative to the root, which the checks here need.
const FIRST_VERSION: u8 = 6;
/// The version that added qualified names to rows.
const QUALIFIED_VERSION: u8 = 7;
/// The version that added the tables of files not fully embedded; the
/// newest the old index wrote.
const LAST_VERSION: u8 = 8;

/// What starts each frame a refresh appended.
const FRAME_MAGIC: &[u8; 8] = b"AFTSEG01";
/// The only frame format the old index wrote.
const FRAME_VERSION: u8 = 1;
/// A frame's magic, payload length and payload checksum.
const FRAME_HEADER: u64 = 8 + 8 + 32;

/// Bounds the old index's own reader applied, so a damaged count is refused
/// before it is trusted.
const MAX_ROWS: u64 = 2_000_000;
const MAX_DIMENSION: usize = 16_384;

/// Fields of the stored fingerprint that record repairs of the snapshot
/// (summary texts capped, headers parsed in their own language), not which
/// model made its vectors. Any text they changed is caught by the text
/// check on each chunk.
const REPAIR_FLAGS: [&str; 2] = ["file_summary_budget_limited", "header_languages_resolved"];

/// The fingerprint field holding the chunking version, checked on its own.
const CHUNKING_VERSION: &str = "chunking_version";

/// Bytes buffered while reading the snapshot.
const READ_BUFFER: usize = 64 << 10;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The kind the old index stored as `code`; its codes differ from the
/// store's.
fn old_kind(code: u8) -> Option<SymbolKind> {
    Some(match code {
        0 => SymbolKind::Function,
        1 => SymbolKind::Class,
        2 => SymbolKind::Method,
        3 => SymbolKind::Struct,
        4 => SymbolKind::Interface,
        5 => SymbolKind::Enum,
        6 => SymbolKind::TypeAlias,
        7 => SymbolKind::Variable,
        8 => SymbolKind::Heading,
        9 => SymbolKind::FileSummary,
        10 => SymbolKind::Kernel,
        _ => return None,
    })
}

/// A path as the old index wrote it, in the store's spelling: relative to
/// the root and `/`-separated. Windows builds wrote `\`.
fn store_path(mut path: Vec<u8>) -> Vec<u8> {
    if cfg!(windows) {
        for byte in &mut path {
            if *byte == b'\\' {
                *byte = b'/';
            }
        }
    }
    path
}

/// Whether the fingerprint the snapshot recorded (`stored`, as JSON) and
/// its vectors' `dimension` belong to the `configured` model, and its
/// chunks to the current chunking version. `Err` says why not.
fn compatible(
    stored: &str,
    dimension: usize,
    configured: &SemanticIndexFingerprint,
) -> Result<(), String> {
    use serde_json::Value;
    if dimension != configured.dimension {
        return Err(format!(
            "its vectors have {dimension} dimensions; the model's have {}",
            configured.dimension
        ));
    }
    let Ok(Value::Object(mut stored)) = serde_json::from_str::<Value>(stored) else {
        return Err("its model fingerprint is missing or unreadable".to_string());
    };
    for flag in REPAIR_FLAGS {
        if stored.remove(flag).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("its fingerprint's {flag} is not a flag"));
        }
    }
    let Ok(Value::Object(mut current)) = serde_json::to_value(configured) else {
        return Err("the configured model fingerprint cannot be compared".to_string());
    };
    let stored_chunking = stored.remove(CHUNKING_VERSION);
    let current_chunking = current.remove(CHUNKING_VERSION);
    if stored != current {
        return Err("it was written for another model or model configuration".to_string());
    }
    match (
        stored_chunking.as_ref().and_then(Value::as_u64),
        current_chunking.as_ref().and_then(Value::as_u64),
    ) {
        (Some(stored), Some(current)) if stored == current => Ok(()),
        _ => Err("its chunks were cut by another chunking version".to_string()),
    }
}

/// Reads the snapshot front to back within `end`, keeping track of where
/// it is. Every length is checked against what is left before it is used,
/// so a damaged length is refused rather than allocated.
struct Input<'r> {
    reader: &'r mut BufReader<File>,
    at: u64,
    end: u64,
}

impl Input<'_> {
    fn claim(&mut self, len: u64) -> Result<(), String> {
        if self.end.saturating_sub(self.at) < len {
            return Err(format!("it ends early, at byte {}", self.at));
        }
        self.at += len;
        Ok(())
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.claim(N as u64)?;
        let mut bytes = [0; N];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, String> {
        self.array().map(u64::from_le_bytes)
    }

    /// A count of at most [`MAX_ROWS`].
    fn count(&mut self) -> Result<u64, String> {
        let count = u64::from(self.u32()?);
        if count > MAX_ROWS {
            return Err(format!("it counts {count} rows, over {MAX_ROWS}"));
        }
        Ok(count)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = u64::from(self.u32()?);
        self.claim(len)?;
        let mut bytes = vec![0; len as usize];
        self.reader
            .read_exact(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    }

    fn skip(&mut self, len: u64) -> Result<(), String> {
        self.claim(len)?;
        let len = i64::try_from(len).map_err(|_| "a field is too long".to_string())?;
        self.reader
            .seek_relative(len)
            .map_err(|error| error.to_string())
    }

    fn skip_bytes(&mut self) -> Result<(), String> {
        let len = u64::from(self.u32()?);
        self.skip(len)
    }

    /// Moves to `at`, keeping what is buffered when `at` lies ahead.
    fn seek(&mut self, at: u64) -> Result<(), String> {
        if at >= self.at && at <= self.end {
            let ahead = i64::try_from(at - self.at).map_err(|_| "a row is too far".to_string())?;
            self.reader
                .seek_relative(ahead)
                .map_err(|error| error.to_string())?;
        } else {
            self.reader
                .seek(SeekFrom::Start(at))
                .map_err(|error| error.to_string())?;
        }
        self.at = at;
        Ok(())
    }
}

/// Where one file's rows are, and what its chunks were cut from.
#[derive(Debug)]
struct FileRows {
    /// The BLAKE3 digest of the bytes the rows were cut from; all zeros
    /// when the old index did not record one.
    digest: [u8; 32],
    /// Where each row starts, just past its path.
    rows: Vec<u64>,
    /// Whether the rows hold qualified names (version 7 on).
    qualified: bool,
    /// The file is listed twice, which can happen when two paths that are
    /// not UTF-8 were written the same way: nothing is taken from it.
    ambiguous: bool,
}

/// One base snapshot or the snapshot inside a frame, as far as matching
/// needs it.
struct Section {
    files: HashMap<Vec<u8>, FileRows>,
    /// Files with rows the embedder refused or that were too long for it.
    incomplete: HashSet<Vec<u8>>,
    /// Paths of rows whose file is not in the file table.
    unlisted: HashSet<Vec<u8>>,
}

/// Reads one snapshot (a base, or the one inside a frame) from `input`,
/// first checking that it belongs to the `configured` model.
fn read_section(
    input: &mut Input<'_>,
    configured: &SemanticIndexFingerprint,
) -> Result<Section, String> {
    let version = input.u8()?;
    if !(FIRST_VERSION..=LAST_VERSION).contains(&version) {
        return Err(format!("its format version {version} is not 6 to 8"));
    }
    let dimension = input.u32()? as usize;
    if dimension == 0 || dimension > MAX_DIMENSION {
        return Err(format!("its vectors have {dimension} dimensions"));
    }
    let rows = input.count()?;
    let identity =
        String::from_utf8(input.bytes()?).map_err(|_| "its fingerprint is not UTF-8".to_string())?;
    compatible(&identity, dimension, configured)?;

    let listed = input.count()?;
    let qualified = version >= QUALIFIED_VERSION;
    let mut files: HashMap<Vec<u8>, FileRows> = HashMap::new();
    for _ in 0..listed {
        let path = store_path(input.bytes()?);
        input.skip(8)?; // modified time, seconds
        let nanos = input.u32()?;
        if nanos >= 1_000_000_000 {
            return Err(format!("a modified time has {nanos} nanoseconds"));
        }
        input.skip(8)?; // size
        let digest = input.array::<32>()?;
        files
            .entry(path)
            .and_modify(|file| file.ambiguous = true)
            .or_insert(FileRows {
                digest,
                rows: Vec::new(),
                qualified,
                ambiguous: false,
            });
    }

    let vector_bytes = (dimension as u64) * 4;
    let mut unlisted = HashSet::new();
    for _ in 0..rows {
        let path = store_path(input.bytes()?);
        let at = input.at;
        input.skip_bytes()?; // name
        if qualified {
            input.skip_bytes()?;
        }
        input.skip(1 + 4 + 4 + 1)?; // kind, lines, exported
        input.skip_bytes()?; // snippet
        input.skip_bytes()?; // text
        input.skip(vector_bytes)?;
        match files.get_mut(&path) {
            Some(file) => file.rows.push(at),
            None => {
                unlisted.insert(path);
            }
        }
    }

    let mut incomplete = HashSet::new();
    if version >= LAST_VERSION {
        let total = input.u64()?;
        let mut counted = 0_u64;
        // Rows the embedder refused, then rows too long for it.
        for _ in 0..2 {
            let mut seen = HashSet::new();
            for _ in 0..input.count()? {
                let path = store_path(input.bytes()?);
                let skipped = input.u64()?;
                counted = counted
                    .checked_add(skipped)
                    .ok_or_else(|| "its skipped rows overflow".to_string())?;
                if skipped == 0 || !files.contains_key(&path) || !seen.insert(path.clone()) {
                    return Err("its table of skipped rows is malformed".to_string());
                }
                incomplete.insert(path);
            }
        }
        if counted > total {
            return Err("it lists more skipped rows than it counts".to_string());
        }
    }
    Ok(Section {
        files,
        incomplete,
        unlisted,
    })
}

/// Hashes the `len` bytes at the input's position, leaving it past them.
fn checksum(input: &mut Input<'_>, len: u64) -> Result<[u8; 32], String> {
    let mut hasher = blake3::Hasher::new();
    let mut left = len;
    let mut buffer = vec![0; 64 << 10];
    while left > 0 {
        let take = left.min(buffer.len() as u64) as usize;
        input.claim(take as u64)?;
        input
            .reader
            .read_exact(&mut buffer[..take])
            .map_err(|error| error.to_string())?;
        hasher.update(&buffer[..take]);
        left -= take as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// What reading a frame found.
enum Frame {
    /// The frame's replacement of some files' rows.
    Read {
        tombstones: HashSet<Vec<u8>>,
        section: Section,
    },
    /// No complete frame starts here: the file ends, or a refresh was
    /// killed while appending. What came before stands.
    End,
}

/// Reads the frame at the input's position, which must be the
/// `sequence`th.
fn read_frame(
    input: &mut Input<'_>,
    len: u64,
    sequence: u64,
    configured: &SemanticIndexFingerprint,
) -> Result<Frame, String> {
    let start = input.at;
    input.end = len;
    if len.saturating_sub(start) < FRAME_HEADER || input.array::<8>()? != *FRAME_MAGIC {
        return Ok(Frame::End);
    }
    let payload = input.u64()?;
    let expected = input.array::<32>()?;
    if payload > len - input.at {
        return Ok(Frame::End);
    }
    let payload_start = input.at;
    if checksum(input, payload)? != expected {
        return Ok(Frame::End);
    }
    input.seek(payload_start)?;
    input.end = payload_start + payload;

    let version = input.u8()?;
    if version != FRAME_VERSION {
        return Err(format!("a frame has format version {version}"));
    }
    if input.u64()? != sequence {
        return Err(format!("frame {sequence} is out of order"));
    }
    let identity =
        String::from_utf8(input.bytes()?).map_err(|_| "a frame's fingerprint is not UTF-8".to_string())?;
    let dimension = input.u32()? as usize;
    compatible(&identity, dimension, configured)?;
    let mut tombstones = HashSet::new();
    for _ in 0..input.count()? {
        if !tombstones.insert(store_path(input.bytes()?)) {
            return Err("a frame replaces one file twice".to_string());
        }
    }
    let inner = input.u64()?;
    if input.end.checked_sub(input.at) != Some(inner) {
        return Err("a frame's snapshot does not fill it".to_string());
    }
    let section = read_section(input, configured)?;
    if input.at != input.end {
        return Err("a frame's snapshot has bytes after it".to_string());
    }
    let replaced = section.files.keys().chain(&section.unlisted);
    if replaced
        .into_iter()
        .any(|path| !tombstones.contains(path))
    {
        return Err("a frame has rows of a file it does not replace".to_string());
    }
    Ok(Frame::Read {
        tombstones,
        section,
    })
}

/// One row of the old index, read back for matching.
struct Row {
    name: String,
    /// `None` where the snapshot records no qualified names (version 6);
    /// `Some(None)` for a row without one.
    qualified_name: Option<Option<String>>,
    kind: Option<SymbolKind>,
    start_line: u32,
    end_line: u32,
    text: String,
    vector: Vec<f32>,
}

impl Row {
    /// Whether this row is `chunk` as the fill cut it now.
    fn is(&self, chunk: &CutChunk) -> bool {
        let detail = &chunk.detail;
        self.kind.is_some()
            && self.kind == super::vectors::kind_of(detail.kind)
            && self.name == detail.name
            && self.start_line == detail.start_line
            && self.end_line == detail.end_line
            && self.qualified_name.as_ref().is_none_or(|qualified| {
                qualified.as_deref()
                    == detail
                        .qualified_name
                        .as_deref()
                        .filter(|name| !name.is_empty())
            })
            && self.text == chunk.text
    }
}

/// An old meaning index's snapshot, indexed: each usable file's digest and
/// where its rows are. See the module's documentation.
pub struct OldSnapshot {
    path: PathBuf,
    /// The open snapshot, and where its reader is.
    reader: Mutex<(BufReader<File>, u64)>,
    len: u64,
    dimension: usize,
    files: HashMap<Vec<u8>, FileRows>,
    rows: usize,
}

impl std::fmt::Debug for OldSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OldSnapshot")
            .field("path", &self.path)
            .field("files", &self.files.len())
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl OldSnapshot {
    /// Reads and indexes the snapshot at `path` for the `configured` model.
    /// `Ok(None)` when there is no file there.
    ///
    /// # Errors
    ///
    /// Why the snapshot cannot be used: another model, another chunking
    /// version, or a file that cannot be read as a whole.
    pub fn open(
        path: &Path,
        configured: &SemanticIndexFingerprint,
    ) -> Result<Option<Self>, String> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let len = file.metadata().map_err(|error| error.to_string())?.len();
        let mut reader = BufReader::with_capacity(READ_BUFFER, file);
        let mut input = Input {
            reader: &mut reader,
            at: 0,
            end: len,
        };
        let base = read_section(&mut input, configured)?;
        let mut files = base.files;
        let mut incomplete = base.incomplete;
        let mut sequence = 0;
        loop {
            sequence += 1;
            match read_frame(&mut input, len, sequence, configured)? {
                Frame::End => break,
                Frame::Read {
                    tombstones,
                    section,
                } => {
                    for path in &tombstones {
                        files.remove(path);
                        incomplete.remove(path);
                    }
                    files.extend(section.files);
                    incomplete.extend(section.incomplete);
                }
            }
        }
        let at = input.at;
        // Keep only what a fill may take from.
        files.retain(|path, file| {
            !file.ambiguous
                && !file.rows.is_empty()
                && file.digest != [0; 32]
                && !incomplete.contains(path)
        });
        files.shrink_to_fit();
        for file in files.values_mut() {
            file.rows.shrink_to_fit();
        }
        let rows = files.values().map(|file| file.rows.len()).sum();
        Ok(Some(Self {
            path: path.to_path_buf(),
            reader: Mutex::new((reader, at)),
            len,
            dimension: configured.dimension,
            files,
            rows,
        }))
    }

    /// Files the snapshot can give vectors for.
    #[must_use]
    pub fn files(&self) -> usize {
        self.files.len()
    }

    /// Rows of those files.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Where the snapshot is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the index of the snapshot holds in memory: each file's path,
    /// digest and row positions, and the read buffer.
    #[must_use]
    pub fn memory(&self) -> usize {
        let files = self.files.capacity()
            * (std::mem::size_of::<(Vec<u8>, FileRows)>() + std::mem::size_of::<u64>());
        let owned: usize = self
            .files
            .iter()
            .map(|(path, file)| path.capacity() + file.rows.capacity() * std::mem::size_of::<u64>())
            .sum();
        files + owned + READ_BUFFER
    }

    /// For each of `chunks`, cut from `bytes`, the content of the file at
    /// `path` (relative to the root, `/`-separated), the vector the old
    /// index computed for it, where it computed one for exactly this chunk
    /// of exactly these bytes (see the module's documentation).
    #[must_use]
    pub fn vectors(&self, path: &[u8], bytes: &[u8], chunks: &[CutChunk]) -> Vec<Option<Vec<f32>>> {
        let mut found = vec![None; chunks.len()];
        let Some(file) = self.files.get(path) else {
            return found;
        };
        if *blake3::hash(bytes).as_bytes() != file.digest {
            return found;
        }
        let rows = self.read_rows(file);
        for (slot, chunk) in found.iter_mut().zip(chunks) {
            *slot = rows
                .iter()
                .find(|row| row.is(chunk))
                .map(|row| row.vector.clone());
        }
        found
    }

    /// The rows of `file` that can be read back whole; one that cannot is
    /// left out, so its chunk is embedded.
    fn read_rows(&self, file: &FileRows) -> Vec<Row> {
        let mut guard = lock(&self.reader);
        let (reader, at) = &mut *guard;
        let mut input = Input {
            reader,
            at: *at,
            // The snapshot was checked as it was indexed; the bounds stay,
            // in case the file was cut short since.
            end: self.len,
        };
        let mut rows = Vec::with_capacity(file.rows.len());
        for &row in &file.rows {
            match self.read_row(&mut input, row, file.qualified) {
                Ok(row) => rows.push(row),
                // Where a failed read left the reader is unknown: the next
                // row is sought from the start of the file.
                Err(_) => input.at = u64::MAX,
            }
        }
        *at = input.at;
        rows
    }

    fn read_row(&self, input: &mut Input<'_>, at: u64, qualified: bool) -> Result<Row, String> {
        input.seek(at)?;
        let string = |input: &mut Input<'_>| {
            String::from_utf8(input.bytes()?).map_err(|_| "a row is not UTF-8".to_string())
        };
        let name = string(input)?;
        let qualified_name = if qualified {
            Some(Some(string(input)?).filter(|name| !name.is_empty()))
        } else {
            None
        };
        let kind = old_kind(input.u8()?);
        let start_line = input.u32()?;
        let end_line = input.u32()?;
        input.skip(1)?; // exported
        input.skip_bytes()?; // snippet
        let text = string(input)?;
        let mut vector = Vec::with_capacity(self.dimension);
        for _ in 0..self.dimension {
            let value = f32::from_le_bytes(input.array()?);
            if !value.is_finite() {
                return Err("a vector is not finite".to_string());
            }
            vector.push(value);
        }
        Ok(Row {
            name,
            qualified_name,
            kind,
            start_line,
            end_line,
            text,
            vector,
        })
    }
}

/// Where the old meaning index of the checkout rooted at `root` lies, under
/// AFT's storage folder `storage`: `semantic/<project key>/semantic.bin`,
/// the key worked out from the same identity records the old index's
/// configure used, read without writing any.
///
/// For a Git checkout at its top level the key is the repository's (from
/// its root commits), shared by the main checkout and every linked
/// worktree: the old index let only the main checkout write that snapshot,
/// and its worktrees read it. So a worktree consults the snapshot its main
/// checkout wrote, and no other snapshot need be looked for. A session
/// opened in a subfolder of a checkout had a key of its own, shared with
/// the same subfolder of the repository's other checkouts; a folder
/// outside Git had one derived from its path.
///
/// # Errors
///
/// When the root's identity cannot be worked out.
pub fn snapshot_path(storage: &Path, root: &Path) -> Result<PathBuf, String> {
    let key = crate::search_index::artifact_cache_key_read_only(root, storage)
        .map_err(|error| error.to_string())?;
    if key.is_empty() || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{key:?} is not a project key the old index wrote"));
    }
    Ok(storage.join("semantic").join(key).join("semantic.bin"))
}

/// Where a plane looks for the old meaning index.
enum Place {
    /// The snapshot of the checkout rooted at `root` (see
    /// [`snapshot_path`]).
    Checkout { storage: PathBuf, root: PathBuf },
    /// The snapshot at this path.
    File(PathBuf),
}

/// The old meaning index a plane's fills take vectors from, opened by the
/// first fill that has chunks to embed and kept while the plane lives.
pub struct OldIndex {
    place: Place,
    configured: SemanticIndexFingerprint,
    /// `None` until a fill looked; then the snapshot, or `None` when there
    /// is none or it cannot be used.
    opened: Mutex<Option<Option<Arc<OldSnapshot>>>>,
}

impl OldIndex {
    /// The old meaning index of the checkout rooted at `root`, under AFT's
    /// storage folder `storage`, for the `configured` model.
    #[must_use]
    pub fn of_checkout(storage: &Path, root: &Path, configured: SemanticIndexFingerprint) -> Self {
        Self::new(
            Place::Checkout {
                storage: storage.to_path_buf(),
                root: root.to_path_buf(),
            },
            configured,
        )
    }

    /// The old meaning index snapshot at `path`, for the `configured` model.
    #[must_use]
    pub fn at(path: &Path, configured: SemanticIndexFingerprint) -> Self {
        Self::new(Place::File(path.to_path_buf()), configured)
    }

    /// The model the index is read for.
    #[must_use]
    pub fn configured(&self) -> &SemanticIndexFingerprint {
        &self.configured
    }

    fn new(place: Place, configured: SemanticIndexFingerprint) -> Self {
        Self {
            place,
            configured,
            opened: Mutex::new(None),
        }
    }

    /// The snapshot, read and indexed now if no fill has looked for it yet.
    /// `None` when there is none or it cannot be used; the log says why.
    pub fn snapshot(&self) -> Option<Arc<OldSnapshot>> {
        let mut opened = lock(&self.opened);
        if let Some(snapshot) = &*opened {
            return snapshot.clone();
        }
        let started = Instant::now();
        let path = match &self.place {
            Place::File(path) => Ok(path.clone()),
            Place::Checkout { storage, root } => snapshot_path(storage, root),
        };
        let snapshot = match path {
            Err(error) => {
                crate::slog_info!(
                    "data layer meaning fill: no old meaning index to reuse: {error}"
                );
                None
            }
            Ok(path) => match OldSnapshot::open(&path, &self.configured) {
                Ok(Some(snapshot)) => {
                    crate::slog_info!(
                        "data layer meaning fill: reusing vectors of the old meaning index {}: {} files, {} rows, read in {} ms",
                        path.display(),
                        snapshot.files(),
                        snapshot.rows(),
                        started.elapsed().as_millis()
                    );
                    Some(Arc::new(snapshot))
                }
                Ok(None) => None,
                Err(reason) => {
                    crate::slog_info!(
                        "data layer meaning fill: not reusing the old meaning index {}: {reason}",
                        path.display()
                    );
                    None
                }
            },
        };
        *opened = Some(snapshot.clone());
        snapshot
    }
}

impl aft_store::heap::HeapBytes for OldIndex {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        let snapshot = match self.opened.try_lock() {
            Ok(opened) => opened.clone().flatten(),
            Err(std::sync::TryLockError::Poisoned(opened)) => opened.into_inner().clone().flatten(),
            Err(std::sync::TryLockError::WouldBlock) => {
                seen.contend();
                return 0;
            }
        };
        snapshot.map_or(0, |snapshot| {
            if seen.first(Arc::as_ptr(&snapshot)) {
                std::mem::size_of::<OldSnapshot>() + snapshot.memory()
            } else {
                0
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use aft_store::ids::{Digest128, FamilyId};

    use super::*;
    use crate::config::SemanticBackendConfig;
    use crate::data_layer::checkout::{Checkout, DataLayer};
    use crate::data_layer::vectors::{
        cut_file, kind_of, Cut, FillBudget, FillReport, Producer, VectorPlane,
    };

    const DIMENSIONS: usize = 8;

    /// The model the tests configure.
    fn configured() -> SemanticIndexFingerprint {
        SemanticIndexFingerprint::for_config_dimension(&SemanticBackendConfig::default(), DIMENSIONS)
    }

    /// The fingerprint as the old index stored it: the model's, with the
    /// flag recording that its headers were parsed in their own language.
    fn stored_identity(fingerprint: &SemanticIndexFingerprint) -> serde_json::Value {
        let mut value = serde_json::to_value(fingerprint).unwrap();
        value["header_languages_resolved"] = serde_json::Value::Bool(true);
        value
    }

    /// What the model returns for `text` in these tests.
    fn model_vector(text: &str) -> Vec<f32> {
        let hash = blake3::hash(text.as_bytes());
        (0..DIMENSIONS)
            .map(|i| f32::from(hash.as_bytes()[i]) / 255.0)
            .collect()
    }

    /// What the old index stored for `text`: unlike [`model_vector`], so a
    /// stored vector tells where it came from.
    fn old_vector(text: &str) -> Vec<f32> {
        model_vector(text).iter().map(|value| -1.0 - value).collect()
    }

    fn old_code(kind: &SymbolKind) -> u8 {
        (0..=10).find(|code| old_kind(*code).as_ref() == Some(kind)).unwrap()
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A small project whose files each cut into a few chunks.
    fn project(root: &Path) {
        write(
            root,
            "src/lib.rs",
            "mod util;\n\n/// Opens the store.\npub fn open_store(path: &str) -> usize {\n    path.len() * 2\n}\n\npub struct Config {\n    pub name: String,\n}\n\nimpl Config {\n    pub fn new(name: &str) -> Self {\n        Self { name: name.to_string() }\n    }\n}\n",
        );
        write(
            root,
            "src/util.rs",
            "pub fn callee() {\n    println!(\"called\");\n}\n\npub fn twin() {\n    println!(\"twin\");\n}\n",
        );
        write(
            root,
            "tool/main.py",
            "import os\n\nclass Tool:\n    def run(self):\n        return os.getcwd()\n\ndef helper(value):\n    return value + 1\n",
        );
    }

    /// One row of an old snapshot.
    #[derive(Clone)]
    struct OldRow {
        path: String,
        name: String,
        qualified_name: Option<String>,
        kind: u8,
        start_line: u32,
        end_line: u32,
        text: String,
        vector: Vec<f32>,
    }

    /// What an old snapshot holds.
    #[derive(Clone)]
    struct Old {
        version: u8,
        identity: serde_json::Value,
        /// Each file with the digest of the bytes its rows were cut from.
        files: Vec<(String, [u8; 32])>,
        rows: Vec<OldRow>,
        /// Files with rows the embedder refused (version 8).
        refused: Vec<String>,
    }

    /// The rows the old index wrote for the files of `root`, each with the
    /// text cut from its bytes now and an [`old_vector`].
    fn old_of(root: &Path) -> Old {
        let mut files = Vec::new();
        let mut rows = Vec::new();
        for path in ["src/lib.rs", "src/util.rs", "tool/main.py"] {
            let bytes = std::fs::read(root.join(path)).unwrap();
            files.push((path.to_string(), *blake3::hash(&bytes).as_bytes()));
            let Cut::Chunks(chunks) =
                cut_file(root, path.as_bytes(), &bytes, configured().embed_text_caps)
            else {
                panic!("{path} cannot be cut");
            };
            assert!(chunks.len() >= 2, "{path}");
            for chunk in chunks {
                rows.push(OldRow {
                    path: path.to_string(),
                    name: chunk.detail.name,
                    qualified_name: chunk.detail.qualified_name,
                    kind: old_code(&kind_of(chunk.detail.kind).unwrap()),
                    start_line: chunk.detail.start_line,
                    end_line: chunk.detail.end_line,
                    vector: old_vector(&chunk.text),
                    text: chunk.text,
                });
            }
        }
        Old {
            version: LAST_VERSION,
            identity: stored_identity(&configured()),
            files,
            rows,
            refused: Vec::new(),
        }
    }

    fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }

    /// `old` in the old index's format.
    fn snapshot_bytes(old: &Old) -> Vec<u8> {
        let mut out = vec![old.version];
        out.extend_from_slice(&(DIMENSIONS as u32).to_le_bytes());
        out.extend_from_slice(&(old.rows.len() as u32).to_le_bytes());
        put_bytes(&mut out, old.identity.to_string().as_bytes());
        out.extend_from_slice(&(old.files.len() as u32).to_le_bytes());
        for (path, digest) in &old.files {
            put_bytes(&mut out, path.as_bytes());
            out.extend_from_slice(&1_700_000_000_u64.to_le_bytes());
            out.extend_from_slice(&0_u32.to_le_bytes());
            out.extend_from_slice(&64_u64.to_le_bytes());
            out.extend_from_slice(digest);
        }
        for row in &old.rows {
            put_bytes(&mut out, row.path.as_bytes());
            put_bytes(&mut out, row.name.as_bytes());
            if old.version >= QUALIFIED_VERSION {
                put_bytes(
                    &mut out,
                    row.qualified_name.as_deref().unwrap_or_default().as_bytes(),
                );
            }
            out.push(row.kind);
            out.extend_from_slice(&row.start_line.to_le_bytes());
            out.extend_from_slice(&row.end_line.to_le_bytes());
            out.push(1);
            put_bytes(&mut out, b"snippet");
            put_bytes(&mut out, row.text.as_bytes());
            for value in &row.vector {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        if old.version >= LAST_VERSION {
            out.extend_from_slice(&(old.refused.len() as u64).to_le_bytes());
            out.extend_from_slice(&(old.refused.len() as u32).to_le_bytes());
            for path in &old.refused {
                put_bytes(&mut out, path.as_bytes());
                out.extend_from_slice(&1_u64.to_le_bytes());
            }
            out.extend_from_slice(&0_u32.to_le_bytes());
        }
        out
    }

    /// A frame a refresh appends: the `sequence`th, replacing the rows of
    /// the files `replaced` names with those of `old`.
    fn frame_bytes(sequence: u64, replaced: &[&str], old: &Old) -> Vec<u8> {
        let mut payload = vec![FRAME_VERSION];
        payload.extend_from_slice(&sequence.to_le_bytes());
        put_bytes(&mut payload, configured().as_string().as_bytes());
        payload.extend_from_slice(&(DIMENSIONS as u32).to_le_bytes());
        payload.extend_from_slice(&(replaced.len() as u32).to_le_bytes());
        for path in replaced {
            put_bytes(&mut payload, path.as_bytes());
        }
        let inner = snapshot_bytes(old);
        payload.extend_from_slice(&(inner.len() as u64).to_le_bytes());
        payload.extend_from_slice(&inner);
        let mut frame = FRAME_MAGIC.to_vec();
        frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        frame.extend_from_slice(blake3::hash(&payload).as_bytes());
        frame.extend_from_slice(&payload);
        frame
    }

    /// What one fill over an old snapshot did.
    struct Filled {
        report: FillReport,
        /// Every chunk text of the project, with where its stored vector
        /// came from: `true` for the old index, `false` for the model.
        sources: Vec<(String, bool)>,
    }

    impl Filled {
        fn imported(&self) -> BTreeSet<&str> {
            self.sources
                .iter()
                .filter(|(_, old)| *old)
                .map(|(text, _)| text.as_str())
                .collect()
        }

        fn embedded(&self) -> BTreeSet<&str> {
            self.sources
                .iter()
                .filter(|(_, old)| !*old)
                .map(|(text, _)| text.as_str())
                .collect()
        }
    }

    /// Fills a fresh store for the project at `root` with the old snapshot
    /// `bytes` in place, for the `configured()` model, and checks the
    /// snapshot is left as it was.
    fn fill_over(root: &Path, bytes: &[u8]) -> Filled {
        let storage = tempfile::tempdir().unwrap();
        let path = storage.path().join("semantic/0123456789abcdef/semantic.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();

        let store = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(Digest128::of(b"old index tests"));
        let checkout =
            Checkout::bind(Arc::new(DataLayer::new(8 << 20, 2)), store.path(), family, root)
                .unwrap();
        checkout.build().unwrap();
        let plane = VectorPlane::new(
            Arc::new(checkout),
            Producer::from_fingerprint(&configured()),
            100,
        )
        .reusing_old_index(OldIndex::at(&path, configured()));
        let mut sent = Vec::new();
        let report = plane
            .fill(
                &mut |texts| {
                    let vectors = texts.iter().map(|text| model_vector(text)).collect();
                    sent.extend(texts);
                    Ok(vectors)
                },
                FillBudget {
                    max_batch: 4,
                    files_per_round: 2,
                },
                &mut |_| {},
            )
            .unwrap();
        assert!(report.error.is_none(), "{report:?}");
        assert_eq!(report.files_left, 0, "{report:?}");

        let mut sources = Vec::new();
        for row in old_of(root).rows {
            let stored = plane.stored_vector(&row.text).unwrap().unwrap();
            let old = if stored == old_vector(&row.text) {
                true
            } else {
                assert_eq!(stored, model_vector(&row.text), "{}", row.text);
                false
            };
            sources.push((row.text, old));
        }
        let embedded: BTreeSet<&str> = sent.iter().map(String::as_str).collect();
        assert_eq!(embedded.len(), sent.len(), "a text was embedded twice");
        let filled = Filled { report, sources };
        assert_eq!(embedded, filled.embedded(), "the model got what was not imported");
        assert_eq!(filled.report.chunks_imported, filled.imported().len());
        assert_eq!(filled.report.chunks_embedded, filled.embedded().len());

        // The old snapshot is only read.
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        let listed: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(listed, ["semantic.bin"]);
        filled
    }

    /// Every chunk text of the project at `root`.
    fn all_texts(root: &Path) -> BTreeSet<String> {
        old_of(root).rows.into_iter().map(|row| row.text).collect()
    }

    /// The chunk texts of one file.
    fn texts_of(root: &Path, path: &str) -> BTreeSet<String> {
        old_of(root)
            .rows
            .into_iter()
            .filter(|row| row.path == path)
            .map(|row| row.text)
            .collect()
    }

    fn owned(texts: BTreeSet<&str>) -> BTreeSet<String> {
        texts.into_iter().map(str::to_string).collect()
    }

    /// A snapshot the configured model wrote, of the bytes on disk, gives
    /// every chunk's vector bit for bit, and the model is asked for none.
    #[test]
    fn a_compatible_chunk_is_imported_and_the_old_snapshot_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let filled = fill_over(root.path(), &snapshot_bytes(&old_of(root.path())));
        assert_eq!(owned(filled.imported()), all_texts(root.path()));
        assert!(filled.embedded().is_empty());
        assert_eq!(filled.report.chunks_embedded, 0);
        assert_eq!(filled.report.embed_calls, 0);
    }

    /// A snapshot of version 6 has no qualified names to check; the other
    /// checks still let its chunks through.
    #[test]
    fn a_snapshot_without_qualified_names_is_imported() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let mut old = old_of(root.path());
        old.version = FIRST_VERSION;
        let filled = fill_over(root.path(), &snapshot_bytes(&old));
        assert_eq!(owned(filled.imported()), all_texts(root.path()));
    }

    /// A snapshot another model wrote gives nothing, though every chunk and
    /// every file's bytes match.
    #[test]
    fn another_model_embeds_every_chunk() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let mut old = old_of(root.path());
        old.identity["model"] = serde_json::Value::String("another/model".to_string());
        let filled = fill_over(root.path(), &snapshot_bytes(&old));
        assert!(filled.imported().is_empty());
        assert_eq!(owned(filled.embedded()), all_texts(root.path()));
    }

    /// A snapshot whose chunks another chunking version cut gives nothing,
    /// though the model and every chunk match.
    #[test]
    fn another_chunking_version_embeds_every_chunk() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let mut old = old_of(root.path());
        let current = configured().chunking_version;
        old.identity["chunking_version"] = serde_json::Value::from(current + 1);
        let filled = fill_over(root.path(), &snapshot_bytes(&old));
        assert!(filled.imported().is_empty());
        assert_eq!(owned(filled.embedded()), all_texts(root.path()));
    }

    /// A file whose bytes are not those the snapshot recorded is embedded,
    /// even where its rows match its chunks; the other files are imported.
    #[test]
    fn changed_file_bytes_embed_that_file() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let mut old = old_of(root.path());
        old.files[1].1 = *blake3::hash(b"other bytes").as_bytes();
        assert_eq!(old.files[1].0, "src/util.rs");
        let filled = fill_over(root.path(), &snapshot_bytes(&old));
        assert_eq!(owned(filled.embedded()), texts_of(root.path(), "src/util.rs"));
        let mut rest = all_texts(root.path());
        rest.retain(|text| !texts_of(root.path(), "src/util.rs").contains(text));
        assert_eq!(owned(filled.imported()), rest);
    }

    /// A row that differs from the chunk in any one of its name, kind,
    /// lines, qualified name or text gives nothing for it; every other
    /// chunk is imported.
    #[test]
    fn a_changed_chunk_is_embedded() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let pristine = old_of(root.path());
        let at = pristine
            .rows
            .iter()
            .position(|row| row.name == "new" && row.qualified_name.is_some())
            .expect("Config::new has a qualified name");
        let changes: [(&str, fn(&mut OldRow)); 6] = [
            ("name", |row| row.name.push('x')),
            ("kind", |row| row.kind = if row.kind == 0 { 2 } else { 0 }),
            ("start line", |row| row.start_line += 1),
            ("end line", |row| row.end_line += 1),
            ("qualified name", |row| {
                row.qualified_name.as_mut().unwrap().push('x');
            }),
            ("text", |row| row.text.push(' ')),
        ];
        for (what, change) in changes {
            let mut old = pristine.clone();
            change(&mut old.rows[at]);
            let filled = fill_over(root.path(), &snapshot_bytes(&old));
            let chunk = pristine.rows[at].text.clone();
            assert_eq!(owned(filled.embedded()), BTreeSet::from([chunk.clone()]), "{what}");
            let mut rest = all_texts(root.path());
            rest.remove(&chunk);
            assert_eq!(owned(filled.imported()), rest, "{what}");
        }
    }

    /// A file version 8 lists as having rows the embedder refused gives
    /// nothing.
    #[test]
    fn a_file_not_fully_embedded_is_embedded_again() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let mut old = old_of(root.path());
        old.refused.push("tool/main.py".to_string());
        let filled = fill_over(root.path(), &snapshot_bytes(&old));
        assert_eq!(owned(filled.embedded()), texts_of(root.path(), "tool/main.py"));
    }

    /// Frames a refresh appended replace their files' rows; a final frame
    /// cut short is left out, and a frame out of order makes the snapshot
    /// unusable.
    #[test]
    fn frames_replace_the_rows_of_their_files() {
        let root = tempfile::tempdir().unwrap();
        project(root.path());
        let current = old_of(root.path());
        // The base holds util.rs as it was before an edit.
        let mut base = current.clone();
        base.files[1].1 = *blake3::hash(b"util.rs before the edit").as_bytes();
        for row in &mut base.rows {
            if row.path == "src/util.rs" {
                row.text.push_str(" before the edit");
            }
        }
        let mut refresh = current.clone();
        refresh.files.retain(|(path, _)| path == "src/util.rs");
        refresh.rows.retain(|row| row.path == "src/util.rs");
        let util = texts_of(root.path(), "src/util.rs");

        let base_bytes = snapshot_bytes(&base);
        let alone = fill_over(root.path(), &base_bytes);
        assert_eq!(owned(alone.embedded()), util);

        let mut refreshed = base_bytes.clone();
        refreshed.extend(frame_bytes(1, &["src/util.rs"], &refresh));
        let filled = fill_over(root.path(), &refreshed);
        assert_eq!(owned(filled.imported()), all_texts(root.path()));

        let mut torn = refreshed.clone();
        let cut = frame_bytes(2, &["src/lib.rs"], &current);
        torn.extend(&cut[..cut.len() / 2]);
        let filled = fill_over(root.path(), &torn);
        assert_eq!(owned(filled.imported()), all_texts(root.path()));

        let mut disordered = base_bytes;
        disordered.extend(frame_bytes(2, &["src/util.rs"], &refresh));
        let filled = fill_over(root.path(), &disordered);
        assert!(filled.imported().is_empty());
    }
}
