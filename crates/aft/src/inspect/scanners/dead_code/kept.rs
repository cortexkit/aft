//! The files a checkout's dead-code graph is kept in.
//!
//! A rollup that works out every file keeps its graph in a *base* file, read
//! with positioned reads so a later rollup reads only what it needs:
//!
//! - every scanned file's block (its edges and roots as numbers into a
//!   string table, and what rendering reads) and its *skeleton* (the facts
//!   the whole-project indexes are built from);
//! - tables of the indexes that are not one entry per file (which files
//!   export a name, and the Oxc, Rust public-API and Go dispatch analyses),
//!   by the hash of each key;
//! - for every index entry a file's materialization read, the files that
//!   read it, by the hash of the entry;
//! - a directory naming where each part is, with the small whole-project
//!   facts in it.
//!
//! A rollup patched after edits keeps an *overlay* beside the base instead
//! of writing a new base: the files it worked out again and the index
//! entries that differ from the base's, all since the base was written, so a
//! later rollup needs the base and its newest overlay only.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::engine::FileBlock;
use super::reads::Lookup;
use super::{DeadCodeContribution, OxcFileVerdicts, RustApiModule};
use crate::data_layer::checkout::GraphIdentity;

/// The first line of a base file. A change to what is kept or to how files
/// are materialized changes it, so a file of another format is never read.
pub(super) const BASE_HEADER: &str = "aft dead-code graph 8";
/// The first line of an overlay file.
pub(super) const OVERLAY_HEADER: &str = "aft dead-code overlay 8";
/// The last bytes of a complete base file.
const TRAILER_MAGIC: &[u8; 8] = b"AFTDCG07";
/// A base file's trailer: the directory's offset and length, the blake3
/// digest of every byte before the trailer, and [`TRAILER_MAGIC`].
const TRAILER_LEN: usize = 16 + 32 + 8;
pub(super) const BASE_EXTENSION: &str = "dead-code";
pub(super) const OVERLAY_EXTENSION: &str = "dead-code-overlay";

/// Bytes `offset..offset + len` of a kept file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Span {
    pub(super) offset: u64,
    pub(super) len: u64,
}

/// A 64-bit hash of `text`.
pub(super) fn key_hash(text: &str) -> u64 {
    let hash = blake3::hash(text.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes"))
}

/// The hash an index entry's readers are kept under. Two entries with one
/// hash share their readers, which only redoes more files than needed.
pub(super) fn read_hash(lookup: Lookup, key: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[lookup as u8]);
    hasher.update(key.as_bytes());
    u64::from_le_bytes(
        hasher.finalize().as_bytes()[..8]
            .try_into()
            .expect("eight bytes"),
    )
}

pub(super) fn u32_bytes(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub(super) fn u32s(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect()
}

fn u64_bytes(values: &[u64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn u64s(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| u64::from_le_bytes(chunk.try_into().expect("eight bytes")))
        .collect()
}

fn spans_bytes(spans: &[Span]) -> Vec<u8> {
    spans
        .iter()
        .flat_map(|span| [span.offset, span.len])
        .flat_map(u64::to_le_bytes)
        .collect()
}

fn spans(bytes: &[u8]) -> Vec<Span> {
    u64s(bytes)
        .chunks_exact(2)
        .map(|pair| Span {
            offset: pair[0],
            len: pair[1],
        })
        .collect()
}

/// A block as kept: its calls and roots as little-endian numbers, then the
/// rest as JSON.
pub(super) fn block_bytes(calls: &[u32], roots: &[u32], block: &FileBlock) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + 4 * (calls.len() + roots.len()));
    bytes.extend((calls.len() as u32).to_le_bytes());
    bytes.extend(u32_bytes(calls));
    bytes.extend((roots.len() as u32).to_le_bytes());
    bytes.extend(u32_bytes(roots));
    // Serializing a block of plain data does not fail.
    bytes.extend(serde_json::to_vec(block).unwrap_or_default());
    bytes
}

/// A kept block: its calls, roots and the rest.
pub(super) fn parse_block(bytes: &[u8]) -> Result<(Vec<u32>, Vec<u32>, FileBlock), String> {
    let take = |at: usize| -> Result<(Vec<u32>, usize), String> {
        let count = bytes
            .get(at..at + 4)
            .map(|count| u32::from_le_bytes(count.try_into().expect("four bytes")) as usize)
            .ok_or("a kept block is cut short")?;
        let end = at + 4 + 4 * count;
        let values = bytes.get(at + 4..end).ok_or("a kept block is cut short")?;
        Ok((u32s(values), end))
    };
    let (calls, at) = take(0)?;
    let (roots, at) = take(at)?;
    let block = serde_json::from_slice(&bytes[at..]).map_err(|error| error.to_string())?;
    Ok((calls, roots, block))
}

/// A string table as kept: the count, each length, then the bytes.
fn strings_bytes(strings: &[Box<str>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend((strings.len() as u32).to_le_bytes());
    for string in strings {
        bytes.extend((string.len() as u32).to_le_bytes());
    }
    for string in strings {
        bytes.extend(string.as_bytes());
    }
    bytes
}

fn parse_strings(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    let short = || "a kept string table is cut short".to_string();
    let count =
        u32::from_le_bytes(bytes.get(..4).ok_or_else(short)?.try_into().expect("4")) as usize;
    let lengths = u32s(bytes.get(4..4 + 4 * count).ok_or_else(short)?);
    let mut at = 4 + 4 * count;
    let mut strings = Vec::with_capacity(count);
    for length in lengths {
        let end = at + length as usize;
        let text = std::str::from_utf8(bytes.get(at..end).ok_or_else(short)?)
            .map_err(|error| error.to_string())?;
        strings.push(text.into());
        at = end;
    }
    Ok(strings)
}

/// Where a table's parts are: its sorted key hashes and, per hash, the span
/// of a JSON list of the (key, value) entries with that hash.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(super) struct TableSpans {
    hashes: Span,
    records: Span,
}

/// Where the readers of index entries are: the sorted entry hashes, where
/// each hash's readers start in the list of readers, and that list (base
/// file numbers).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(super) struct ReadersSpans {
    hashes: Span,
    starts: Span,
    files: Span,
}

/// The rules for Rust module paths a file declares: whether it globs its
/// parent module, and its module declarations.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct ModuleInput {
    pub(super) globs_parent: bool,
    pub(super) modules: Vec<RustApiModule>,
}

/// The whole-project facts a kept graph was worked out from and gives, small
/// enough to read whole. A base and each overlay hold their own.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct State {
    /// The digest of everything the graph was worked out from that the
    /// patch does not check itself: the format and the facts' version.
    pub(super) config: String,
    /// The digest of everything it was worked out from: a rollup with the
    /// same reads the graph as it is.
    pub(super) exact: String,
    /// The scanned files, project-relative, in scan order.
    pub(super) scope: Vec<String>,
    /// Every call-graph file's identity, by store path.
    pub(super) identities: BTreeMap<String, GraphIdentity>,
    /// Every scanned file outside the call graph, with the modification time
    /// (nanoseconds) and size it had.
    pub(super) others: BTreeMap<String, (u128, u64)>,
    /// Every call-graph file's default-exported symbol, by project-relative
    /// file.
    pub(super) default_exports: BTreeMap<String, String>,
    /// The project's entry-point files, project-relative.
    pub(super) liveness_root_files: BTreeSet<String>,
    /// The project's executable roots' exports, by project-relative file.
    pub(super) executable_roots: BTreeMap<String, BTreeSet<String>>,
    /// The project's public-API files.
    pub(super) public_api_files: BTreeSet<String>,
    /// The call-graph files whose calls, and whose file-level facts, are
    /// filed under each project-relative file.
    pub(super) graph_calls: BTreeMap<String, Vec<String>>,
    pub(super) graph_facts: BTreeMap<String, Vec<String>>,
    /// Go package folders' files.
    pub(super) go_packages: BTreeMap<PathBuf, Vec<String>>,
    /// How many scanned files name each file as a `#[cfg(test)]` module.
    pub(super) test_module_counts: BTreeMap<String, u32>,
    /// How many scanned files use each Rust type name as a receiver.
    pub(super) receiver_counts: BTreeMap<String, u32>,
    /// How many scanned files refer to each type name.
    pub(super) type_ref_counts: BTreeMap<String, u32>,
    /// Each Rust file's module rules, for files with any.
    pub(super) module_inputs: BTreeMap<String, ModuleInput>,
    /// The scanned files with Oxc facts, Rust API facts and Go dispatch
    /// facts: the inputs of the whole-project analyses.
    pub(super) oxc_files: BTreeSet<String>,
    pub(super) rust_api_files: BTreeSet<String>,
    pub(super) go_files: BTreeSet<String>,
    /// The resolver configuration files the Oxc analysis read, with the
    /// hash of their bytes.
    pub(super) oxc_config: Vec<(PathBuf, String)>,
    /// A digest of each analysis's entry, by key.
    pub(super) oxc_digests: BTreeMap<String, u64>,
    pub(super) rust_public_digests: BTreeMap<String, u64>,
    pub(super) go_live_digests: BTreeMap<String, u64>,
    /// Roots kept alive in call-graph files no scanned file read, as (file,
    /// symbol) string numbers.
    pub(super) other_roots: Vec<u32>,
}

impl State {
    /// The project's test-module files: those any scanned file names.
    pub(super) fn test_module_files(&self) -> BTreeSet<String> {
        self.test_module_counts.keys().cloned().collect()
    }
}

/// A base file's directory.
#[derive(Serialize, Deserialize)]
pub(super) struct Directory {
    pub(super) state: State,
    strings: Span,
    blocks: Span,
    skeletons: Span,
    readers: ReadersSpans,
    pub(super) files_by_symbol: TableSpans,
    pub(super) oxc: TableSpans,
    pub(super) rust_public: TableSpans,
    pub(super) go_live: TableSpans,
}

/// Writes a base file to a temporary file beside where it is kept, and
/// moves it there once it is complete.
pub(super) struct BaseWriter {
    path: PathBuf,
    temporary: PathBuf,
    out: Option<std::io::BufWriter<std::fs::File>>,
    position: u64,
    failed: Option<std::io::Error>,
    blocks: Vec<Span>,
    /// Skeletons are written after every block, so the blocks stay one
    /// region a rollup reads at once; until then they wait here, with
    /// spans relative to the start of this buffer.
    skeleton_bytes: Vec<u8>,
    skeletons: Vec<Span>,
    /// Each read entry's readers, by entry hash.
    readers: HashMap<u64, Vec<u32>>,
    /// The digest of every byte written so far.
    digest: blake3::Hasher,
}

impl BaseWriter {
    /// A writer for `path` whose state digests to `config`; `None` (logged)
    /// when its folder or temporary file cannot be made.
    pub(super) fn create(path: &Path, config: &str) -> Option<Self> {
        let made = (|| {
            let dir = path
                .parent()
                .ok_or_else(|| std::io::Error::other("a kept graph needs a folder"))?;
            std::fs::create_dir_all(dir)?;
            let temporary = path.with_extension(format!("tmp{}", std::process::id()));
            let out = std::io::BufWriter::new(std::fs::File::create(&temporary)?);
            Ok::<_, std::io::Error>(Self {
                path: path.to_path_buf(),
                temporary,
                out: Some(out),
                position: 0,
                failed: None,
                blocks: Vec::new(),
                skeleton_bytes: Vec::new(),
                skeletons: Vec::new(),
                readers: HashMap::new(),
                digest: blake3::Hasher::new(),
            })
        })();
        match made {
            Ok(mut writer) => {
                writer.bytes(format!("{BASE_HEADER}\n{config}\n").as_bytes());
                Some(writer)
            }
            Err(error) => {
                crate::slog_warn!(
                    "tier2 dead_code: cannot keep the dead-code graph at {}: {error}",
                    path.display()
                );
                None
            }
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Span {
        let span = Span {
            offset: self.position,
            len: bytes.len() as u64,
        };
        if let (Some(out), None) = (self.out.as_mut(), self.failed.as_ref()) {
            if let Err(error) = out.write_all(bytes) {
                self.failed = Some(error);
            }
        }
        self.digest.update(bytes);
        self.position += bytes.len() as u64;
        span
    }

    fn json<T: Serialize + ?Sized>(&mut self, value: &T) -> Span {
        match serde_json::to_vec(value) {
            Ok(bytes) => self.bytes(&bytes),
            Err(error) => {
                self.failed = Some(error.into());
                Span::default()
            }
        }
    }

    /// Writes the next scanned file: its block, its skeleton (as JSON), and
    /// the hashes of the index entries its materialization read.
    pub(super) fn file(
        &mut self,
        calls: &[u32],
        roots: &[u32],
        block: &FileBlock,
        skeleton: &[u8],
        reads: impl IntoIterator<Item = u64>,
    ) {
        let number = self.blocks.len() as u32;
        let span = self.bytes(&block_bytes(calls, roots, block));
        self.blocks.push(span);
        self.skeletons.push(Span {
            offset: self.skeleton_bytes.len() as u64,
            len: skeleton.len() as u64,
        });
        self.skeleton_bytes.extend_from_slice(skeleton);
        for hash in reads {
            self.readers.entry(hash).or_default().push(number);
        }
    }

    /// Writes the next scanned file from a kept file's bytes: its block as
    /// kept, its skeleton (JSON), and the hashes of the index entries its
    /// materialization read.
    pub(super) fn raw_file(
        &mut self,
        block: &[u8],
        skeleton: &[u8],
        reads: impl IntoIterator<Item = u64>,
    ) {
        let number = self.blocks.len() as u32;
        let span = self.bytes(block);
        self.blocks.push(span);
        self.skeletons.push(Span {
            offset: self.skeleton_bytes.len() as u64,
            len: skeleton.len() as u64,
        });
        self.skeleton_bytes.extend_from_slice(skeleton);
        for hash in reads {
            self.readers.entry(hash).or_default().push(number);
        }
    }

    /// Writes a table of `entries`.
    pub(super) fn table<V: Serialize>(
        &mut self,
        entries: impl IntoIterator<Item = (String, V)>,
    ) -> TableSpans {
        let mut by_hash = BTreeMap::<u64, Vec<(String, V)>>::new();
        for (key, value) in entries {
            by_hash
                .entry(key_hash(&key))
                .or_default()
                .push((key, value));
        }
        let mut records = Vec::with_capacity(by_hash.len());
        let hashes = by_hash.keys().copied().collect::<Vec<_>>();
        for entries in by_hash.values() {
            records.push(self.json(entries));
        }
        TableSpans {
            hashes: self.bytes(&u64_bytes(&hashes)),
            records: self.bytes(&spans_bytes(&records)),
        }
    }

    /// Ends the file with `strings` and the directory, moves it where it is
    /// kept, and returns its size.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish(
        mut self,
        state: State,
        strings: &[Box<str>],
        files_by_symbol: TableSpans,
        oxc: TableSpans,
        rust_public: TableSpans,
        go_live: TableSpans,
    ) -> std::io::Result<u64> {
        let strings = self.bytes(&strings_bytes(strings));
        let skeleton_bytes = std::mem::take(&mut self.skeleton_bytes);
        let start = self.bytes(&skeleton_bytes).offset;
        drop(skeleton_bytes);
        let skeletons = std::mem::take(&mut self.skeletons)
            .into_iter()
            .map(|span| Span {
                offset: start + span.offset,
                len: span.len,
            })
            .collect::<Vec<_>>();
        let blocks = std::mem::take(&mut self.blocks);
        let blocks = self.bytes(&spans_bytes(&blocks));
        let skeletons = self.bytes(&spans_bytes(&skeletons));
        let mut readers = std::mem::take(&mut self.readers)
            .into_iter()
            .collect::<Vec<_>>();
        readers.sort_unstable_by_key(|(hash, _)| *hash);
        let mut starts = Vec::with_capacity(readers.len() + 1);
        let mut files = Vec::new();
        for (_, readers) in &readers {
            starts.push(files.len() as u32);
            files.extend(readers);
        }
        starts.push(files.len() as u32);
        let hashes = readers.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        drop(readers);
        let readers = ReadersSpans {
            hashes: self.bytes(&u64_bytes(&hashes)),
            starts: self.bytes(&u32_bytes(&starts)),
            files: self.bytes(&u32_bytes(&files)),
        };
        let directory = Directory {
            state,
            strings,
            blocks,
            skeletons,
            readers,
            files_by_symbol,
            oxc,
            rust_public,
            go_live,
        };
        let directory = self.json(&directory);
        let mut trailer = u64_bytes(&[directory.offset, directory.len]);
        trailer.extend(self.digest.finalize().as_bytes());
        trailer.extend(TRAILER_MAGIC);
        self.bytes(&trailer);
        if let Some(error) = self.failed.take() {
            return Err(error);
        }
        self.out
            .take()
            .ok_or_else(|| std::io::Error::other("the kept graph was closed"))?
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        std::fs::rename(&self.temporary, &self.path)?;
        Ok(self.position)
    }
}

impl Drop for BaseWriter {
    /// A file not finished leaves nothing behind.
    fn drop(&mut self) {
        // After a finished file was moved where it is kept, there is no
        // temporary file left to remove.
        let _ = std::fs::remove_file(&self.temporary);
    }
}

fn read_exact_at(file: &std::fs::File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buffer.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buffer[done..],
                offset + done as u64,
            )?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += read;
        }
        Ok(())
    }
}

/// Whether the first `body` bytes of the base `file` (at `path`) hash to
/// `digest`. A daemon opens the same base on every rollup, so a base
/// verified once is remembered by its path, length and modification time
/// and not read whole again.
fn verified(path: &Path, file: &std::fs::File, body: u64, digest: &[u8]) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static VERIFIED: OnceLock<Mutex<HashSet<(PathBuf, u64, Option<std::time::SystemTime>)>>> =
        OnceLock::new();
    let key = (
        path.to_path_buf(),
        body,
        file.metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok()),
    );
    let seen = VERIFIED.get_or_init(|| Mutex::new(HashSet::new()));
    if seen.lock().is_ok_and(|seen| seen.contains(&key)) {
        return true;
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    while offset < body {
        let len = (body - offset).min(buffer.len() as u64) as usize;
        if read_exact_at(file, &mut buffer[..len], offset).is_err() {
            return false;
        }
        hasher.update(&buffer[..len]);
        offset += len as u64;
    }
    let good = hasher.finalize().as_bytes()[..] == *digest;
    if good {
        if let Ok(mut seen) = seen.lock() {
            seen.insert(key);
        }
    }
    good
}

/// The first two lines of a kept file: its header and its configuration
/// digest.
fn head(path: &Path) -> Option<(String, String)> {
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(std::fs::File::open(path).ok()?).lines();
    let header = lines.next()?.ok()?;
    let config = lines.next()?.ok()?;
    Some((header, config))
}

/// The configuration digest of the kept file at `path`, when it is a base
/// or overlay of this format.
pub(super) fn kept_config(path: &Path) -> Option<String> {
    let (header, config) = head(path)?;
    (header == BASE_HEADER || header == OVERLAY_HEADER).then_some(config)
}

/// A base file opened for positioned reads.
pub(super) struct Base {
    file: std::fs::File,
    pub(super) name: String,
    pub(super) directory: Directory,
    blocks: Vec<Span>,
    skeletons: Vec<Span>,
}

impl Base {
    /// The base file at `path`; `None` when it is missing, of another
    /// format, or incomplete.
    pub(super) fn open(path: &Path) -> Option<Self> {
        let (header, _) = head(path)?;
        if header != BASE_HEADER {
            return None;
        }
        let file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let mut trailer = [0u8; TRAILER_LEN];
        let body = len.checked_sub(TRAILER_LEN as u64)?;
        read_exact_at(&file, &mut trailer, body).ok()?;
        if &trailer[48..] != TRAILER_MAGIC {
            return None;
        }
        if !verified(path, &file, body, &trailer[16..48]) {
            crate::slog_warn!(
                "tier2 dead_code: the kept dead-code graph {} is damaged; working out every file again",
                path.display()
            );
            return None;
        }
        let numbers = u64s(&trailer[..16]);
        let directory = Span {
            offset: numbers[0],
            len: numbers[1],
        };
        let mut base = Self {
            file,
            name: path.file_name()?.to_string_lossy().into_owned(),
            directory: Directory {
                state: State::default(),
                strings: Span::default(),
                blocks: Span::default(),
                skeletons: Span::default(),
                readers: ReadersSpans::default(),
                files_by_symbol: TableSpans::default(),
                oxc: TableSpans::default(),
                rust_public: TableSpans::default(),
                go_live: TableSpans::default(),
            },
            blocks: Vec::new(),
            skeletons: Vec::new(),
        };
        base.directory = base.json(directory).ok()?;
        base.blocks = spans(&base.read(base.directory.blocks).ok()?);
        base.skeletons = spans(&base.read(base.directory.skeletons).ok()?);
        (base.blocks.len() == base.directory.state.scope.len()
            && base.skeletons.len() == base.blocks.len())
        .then_some(base)
    }

    pub(super) fn read(&self, span: Span) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0; span.len as usize];
        read_exact_at(&self.file, &mut bytes, span.offset)
            .map_err(|error| format!("reading the kept dead-code graph: {error}"))?;
        Ok(bytes)
    }

    pub(super) fn json<T: DeserializeOwned>(&self, span: Span) -> Result<T, String> {
        serde_json::from_slice(&self.read(span)?)
            .map_err(|error| format!("decoding the kept dead-code graph: {error}"))
    }

    /// The base's string table.
    pub(super) fn strings(&self) -> Result<Vec<Box<str>>, String> {
        parse_strings(&self.read(self.directory.strings)?)
    }

    /// The block of base file number `slot`, as kept.
    pub(super) fn block(&self, slot: usize) -> Result<Vec<u8>, String> {
        let span = *self
            .blocks
            .get(slot)
            .ok_or_else(|| format!("no kept block {slot}"))?;
        self.read(span)
    }

    /// The skeleton of base file number `slot`, as kept (JSON).
    pub(super) fn skeleton_bytes(&self, slot: usize) -> Result<Vec<u8>, String> {
        let span = *self
            .skeletons
            .get(slot)
            .ok_or_else(|| format!("no kept skeleton {slot}"))?;
        self.read(span)
    }

    /// The skeleton of base file number `slot`.
    pub(super) fn skeleton(&self, slot: usize) -> Result<DeadCodeContribution, String> {
        let span = *self
            .skeletons
            .get(slot)
            .ok_or_else(|| format!("no kept skeleton {slot}"))?;
        self.json(span)
    }

    /// The skeletons of base files `slots`, read and decoded as `T` in
    /// parallel.
    pub(super) fn skeletons_as<T: DeserializeOwned + Send>(
        &self,
        slots: &[usize],
    ) -> Result<Vec<T>, String> {
        use rayon::prelude::*;
        slots
            .par_iter()
            .map(|slot| {
                let span = *self
                    .skeletons
                    .get(*slot)
                    .ok_or_else(|| format!("no kept skeleton {slot}"))?;
                self.json(span)
            })
            .collect()
    }

    /// A table's key hashes and record spans.
    pub(super) fn table(&self, spans: TableSpans) -> Result<Table, String> {
        Ok(Table {
            hashes: u64s(&self.read(spans.hashes)?),
            records: self::spans(&self.read(spans.records)?),
        })
    }

    /// The readers of index entries.
    pub(super) fn readers(&self) -> Result<Readers, String> {
        let spans = self.directory.readers;
        Ok(Readers {
            hashes: u64s(&self.read(spans.hashes)?),
            starts: u32s(&self.read(spans.starts)?),
            files: u32s(&self.read(spans.files)?),
        })
    }
}

/// A table of a base file: sorted key hashes, and per hash the span of its
/// entries.
pub(super) struct Table {
    hashes: Vec<u64>,
    records: Vec<Span>,
}

impl Table {
    pub(super) fn len(&self) -> usize {
        self.hashes.len()
    }

    pub(super) fn slot(&self, key: &str) -> Option<usize> {
        self.hashes.binary_search(&key_hash(key)).ok()
    }

    /// The entries of `slot`.
    pub(super) fn load<V: DeserializeOwned>(
        &self,
        base: &Base,
        slot: usize,
    ) -> Result<Vec<(String, V)>, String> {
        base.json(self.records[slot])
    }
}

/// The base files that read each index entry, by entry hash.
pub(super) struct Readers {
    hashes: Vec<u64>,
    starts: Vec<u32>,
    files: Vec<u32>,
}

impl Readers {
    pub(super) fn of(&self, hash: u64) -> &[u32] {
        match self.hashes.binary_search(&hash) {
            Ok(slot) => &self.files[self.starts[slot] as usize..self.starts[slot + 1] as usize],
            Err(_) => &[],
        }
    }

    /// The hashes each of `files` base files read, by base file number.
    pub(super) fn by_file(&self, files: usize) -> Vec<Vec<u64>> {
        let mut by_file = vec![Vec::new(); files];
        for (slot, hash) in self.hashes.iter().enumerate() {
            for file in &self.files[self.starts[slot] as usize..self.starts[slot + 1] as usize] {
                if let Some(reads) = by_file.get_mut(*file as usize) {
                    reads.push(*hash);
                }
            }
        }
        by_file
    }
}

/// One file an overlay worked out again.
#[derive(Serialize, Deserialize)]
pub(super) struct OverlayFile {
    pub(super) calls: Vec<u32>,
    pub(super) roots: Vec<u32>,
    pub(super) block: FileBlock,
    pub(super) skeleton: DeadCodeContribution,
    /// The hashes of the index entries its materialization read.
    pub(super) reads: Vec<u64>,
}

/// What a patched rollup keeps beside the base: everything that differs
/// from the base since the base was written.
#[derive(Serialize, Deserialize)]
pub(super) struct Overlay {
    pub(super) state: State,
    /// Strings numbered on from the base's.
    pub(super) strings: Vec<String>,
    /// Files worked out again since the base, by project-relative file.
    pub(super) files: BTreeMap<String, OverlayFile>,
    /// Index entries that differ from the base's: `None` for an entry the
    /// base has and the graph no longer does.
    pub(super) files_by_symbol: BTreeMap<String, Option<BTreeSet<String>>>,
    pub(super) oxc: BTreeMap<String, Option<OxcFileVerdicts>>,
    pub(super) rust_public: BTreeMap<String, Option<BTreeSet<(String, u32)>>>,
    pub(super) go_live: BTreeMap<String, Option<BTreeSet<String>>>,
}

/// Reads the overlay at `path`: the base file name it is over, and the
/// overlay; `None` when it is missing, of another format, cannot be read,
/// or does not hash to the digest written with it.
pub(super) fn read_overlay(path: &Path) -> Option<(String, Overlay)> {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = String::new();
    for expected in [Some(OVERLAY_HEADER), None] {
        line.clear();
        reader.read_line(&mut line).ok()?;
        if let Some(expected) = expected {
            if line.trim_end() != expected {
                return None;
            }
        }
    }
    line.clear();
    reader.read_line(&mut line).ok()?;
    let base = line.trim_end().to_string();
    line.clear();
    reader.read_line(&mut line).ok()?;
    let digest = line.trim_end().to_string();
    let mut body = Vec::new();
    reader.read_to_end(&mut body).ok()?;
    if blake3::hash(&body).to_hex().as_str() != digest {
        crate::slog_warn!(
            "tier2 dead_code: the kept dead-code overlay {} is damaged; not reading it",
            path.display()
        );
        return None;
    }
    let overlay = serde_json::from_slice(&body).ok()?;
    Some((base, overlay))
}

/// Writes `overlay` over the base named `base` to `path`, through a
/// temporary file, with the digest of its body; returns its size.
pub(super) fn write_overlay(path: &Path, base: &str, overlay: &Overlay) -> std::io::Result<u64> {
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    let written = (|| {
        let body = serde_json::to_vec(overlay)?;
        let mut out = std::io::BufWriter::new(std::fs::File::create(&temporary)?);
        write!(
            out,
            "{OVERLAY_HEADER}\n{}\n{base}\n{}\n",
            overlay.state.config,
            blake3::hash(&body).to_hex()
        )?;
        out.write_all(&body)?;
        let file = out.into_inner().map_err(|error| error.into_error())?;
        let size = file.metadata()?.len();
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(size)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// The kept files in `dir` of either kind, newest first.
pub(super) fn kept_files(dir: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut kept = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().extension().is_some_and(|extension| {
                extension == BASE_EXTENSION || extension == OVERLAY_EXTENSION
            })
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect::<Vec<_>>();
    kept.sort_by(|left, right| right.cmp(left));
    kept
}

/// How many overlays, and bases no kept overlay is over, a folder keeps.
const OVERLAYS_KEPT: usize = 4;
const BASES_KEPT: usize = 2;

/// Removes kept files beyond the newest few overlays and bases, never a
/// base a kept overlay is over, nor `keep`.
pub(super) fn prune(dir: &Path, keep: &Path) {
    let kept = kept_files(dir);
    let is_overlay = |path: &Path| {
        path.extension()
            .is_some_and(|extension| extension == OVERLAY_EXTENSION)
    };
    let mut needed = BTreeSet::new();
    let mut overlays = 0;
    let mut bases = 0;
    let mut remove = Vec::new();
    for (_, path) in &kept {
        if is_overlay(path) {
            overlays += 1;
            if overlays <= OVERLAYS_KEPT || path == keep {
                if let Some(base) = overlay_base(path) {
                    needed.insert(dir.join(base));
                }
            } else {
                remove.push(path.clone());
            }
        }
    }
    for (_, path) in &kept {
        if is_overlay(path) {
            continue;
        }
        bases += 1;
        if bases > BASES_KEPT && !needed.contains(path) && path != keep {
            remove.push(path.clone());
        }
    }
    for path in remove {
        let _ = std::fs::remove_file(path);
    }
}

/// The base file name the overlay at `path` is over.
fn overlay_base(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let mut lines = std::io::BufReader::new(std::fs::File::open(path).ok()?).lines();
    (lines.next()?.ok()? == OVERLAY_HEADER).then_some(())?;
    lines.next()?.ok()?;
    lines.next()?.ok()
}

#[cfg(test)]
mod tests {
    // Proof for the open kept-graph integrity fix: metadata-only verification
    // caching can accept changed bytes when both length and mtime are unchanged.
    #[test]
    #[ignore = "proof for the open kept-graph integrity fix"]
    fn verification_rechecks_same_length_content_with_unchanged_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kept-body");
        let bytes = b"the verified graph body";
        std::fs::write(&path, bytes).unwrap();
        let digest = blake3::hash(bytes);
        let file = std::fs::File::open(&path).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        assert!(super::verified(
            &path,
            &file,
            bytes.len() as u64,
            digest.as_bytes()
        ));
        drop(file);

        let mut damaged = bytes.to_vec();
        damaged[0] ^= 1;
        std::fs::write(&path, damaged).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().modified().unwrap(), modified);
        assert_eq!(file.metadata().unwrap().len(), bytes.len() as u64);
        assert!(
            !super::verified(&path, &file, bytes.len() as u64, digest.as_bytes()),
            "unchanged length and mtime must not hide damaged graph bytes"
        );
    }
}
