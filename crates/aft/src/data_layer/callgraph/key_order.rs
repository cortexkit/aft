//! Pass 4 of a cold build in resolution-key order.
//!
//! [`Resolver::resolve`] gives every site with an equal [`ResolutionKey`]
//! the same answer and records the same dependency keys; the resolver's
//! memo (`MemoizedResolver`) already relies on that. So instead of
//! resolving each file's sites in turn, pass 4 resolves each distinct key
//! once:
//! 1. In file order, every site's key goes to a sorted run, member name
//!    first, so all keys of one name are read back together (a group).
//!    Groups are by name across languages: a caller's receiver type may be
//!    declared in a file of another language (a `.tsx` type of a `.ts`
//!    caller), and language stays a field of the key.
//! 2. Read back in key order, each key is resolved once, from the first site
//!    that has it, against the projected records alone. Groups declared by
//!    few files are packed per thread, up to the budget. A group declared by
//!    many files, or with more keys than one thread's share of the budget,
//!    is resolved by every thread at once, all reading one copy of its
//!    methods. The positions of the sites waiting for their key's answer go
//!    to a scratch file once they pass the budget, so no key, however many
//!    sites have it, holds more memory than that.
//! 3. Each distinct answer is kept once, in a table, and so is each large
//!    set of dependency keys. Every site's result (its file, its position
//!    among the file's sites, its answer's number and its key's dependency
//!    keys) goes to a second run, sorted by file and position.
//! 4. [`SiteResults`] reads that run back in file order, and [`Answers`]
//!    turns each file's results into the sites and dependency keys stored
//!    in its fragment.
//!
//! Both runs and the scratch files are written once into the build's
//! `runs/` folder and checksummed, and each says what it holds: the sorted
//! runs label their records with the file range or flush that wrote them,
//! and a scratch file's header and footer name its flush and keys.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::io::{BufReader, BufWriter, Read as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aft_store::family::ScratchDir;
use aft_store::spill::{Config, SortedRecords, Sorter, WriteGuard};
use rayon::prelude::*;

use crate::callgraph_store::join::dispatch::projected::FileRecord;
use crate::callgraph_store::join::dispatch::{
    dependency, Resolution, ResolutionKey, Resolver, SharedMethods,
};
use crate::callgraph_store::join::ManifestJoinError;

use super::super::checkout::DataError;
use super::site_records::{self, Pinned, RecordSource, Touches};
use super::SiteResolution;

/// The memory pass 4's key order may hold besides the record cache (whose
/// budget is the build cache's; see `site_records`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    /// Bytes of keys waiting for their answers, of their sites' positions,
    /// and of answers waiting to be written, at once.
    work_bytes: usize,
    /// Each sorted run's memory budget.
    sort_bytes: usize,
}

/// The default for both budgets.
const DEFAULT_BYTES: usize = 64 << 20;
/// The smallest budget a sorted run gets, so that a record of a few
/// kilobytes and a merge's buffers still fit.
const MIN_SORT_BYTES: usize = 256 << 10;
/// The memory one key waiting for its answer is counted for: its first
/// site, its site count, and its answer with dependency keys written in
/// line.
const KEY_BYTES: usize = 512;
/// The memory one waiting site's position takes.
const SITE_BYTES: usize = 8;
/// Groups whose name at least this many files declare are resolved by
/// every thread at once, reading one copy of the methods.
const SHARED_DECLARERS: usize = 256;
/// Dependency sets of up to this many keys are written with their sites;
/// larger ones, which are mostly the same for many keys (every declarer of
/// a name an unknown receiver calls), are kept once in a table.
const INLINE_DEPENDENCIES: usize = 32;
/// How many files' keys are computed at once in file order.
const KEY_BATCH: usize = 1024;

impl Budget {
    /// Both budgets set to `bytes`; the sorted runs get at least
    /// [`MIN_SORT_BYTES`].
    pub(crate) fn of_bytes(bytes: usize) -> Self {
        Self {
            work_bytes: bytes.max(1),
            sort_bytes: bytes.max(MIN_SORT_BYTES),
        }
    }

    /// The budget a build uses. With `test-hooks`, the exactness gates set
    /// it with `AFT_CALLGRAPH_SPILL_BYTES` to check that a tiny budget gives
    /// the same graph.
    pub(crate) fn configured() -> Self {
        #[cfg(feature = "test-hooks")]
        if let Some(bytes) = std::env::var("AFT_CALLGRAPH_SPILL_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        {
            return Self::of_bytes(bytes);
        }
        Self::of_bytes(DEFAULT_BYTES)
    }

    fn sort_config(self) -> Config {
        let mut config = Config::new(self.sort_bytes);
        config.buffer_bytes = (self.sort_bytes / 64).clamp(4 << 10, 64 << 10);
        config
    }

    /// How many keys wait for their answers at most.
    fn unit_keys(self) -> usize {
        (self.work_bytes / KEY_BYTES).max(1)
    }

    /// How many waiting sites' positions stay in memory; the rest go to a
    /// scratch file.
    fn memory_sites(self) -> usize {
        (self.work_bytes / 4 / SITE_BYTES).max(1)
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::of_bytes(DEFAULT_BYTES)
    }
}

fn sort_error(error: impl std::fmt::Display) -> ManifestJoinError {
    ManifestJoinError::Parse(format!("pass-4 key order: {error}"))
}

fn too_many(what: &str) -> ManifestJoinError {
    ManifestJoinError::Parse(format!("pass-4 key order: too many {what}"))
}

/// Appends `text` so that encodings compare as their texts do and none is
/// a prefix of another: each zero byte doubled as `0, 0xFF`, then `0, 1`.
fn put_text(out: &mut Vec<u8>, text: &str) {
    for &byte in text.as_bytes() {
        out.push(byte);
        if byte == 0 {
            out.push(0xFF);
        }
    }
    out.extend([0, 1]);
}

fn put_optional(out: &mut Vec<u8>, text: Option<&str>) {
    match text {
        None => out.push(0),
        Some(text) => {
            out.push(1);
            put_text(out, text);
        }
    }
}

/// Appends `key`, the key of a site of the file numbered `file`, so that
/// two keys encode the same exactly when they are equal, and keys of one
/// member name are adjacent in byte order. A key with a receiver is scoped
/// to its calling file, which the file's number names as well as its path.
fn encode_key(key: &ResolutionKey, file: u32, out: &mut Vec<u8>) {
    put_optional(out, key.member.as_deref());
    if key.receiver.is_some() {
        out.push(1);
        out.extend(file.to_be_bytes());
    } else {
        out.push(0);
        put_text(out, &key.scope);
    }
    put_optional(out, key.receiver.as_deref());
    out.push(u8::from(key.dynamic));
    out.push(u8::from(key.associated));
    put_optional(out, key.caller_scope.as_deref());
}

/// The length of the member name's encoding at the start of an encoded
/// key.
fn member_len(key: &[u8]) -> usize {
    if key.first() != Some(&1) {
        return 1;
    }
    let mut at = 1;
    while at + 1 < key.len() {
        if key[at] == 0 {
            if key[at + 1] == 1 {
                return at + 2;
            }
            at += 2;
        } else {
            at += 1;
        }
    }
    key.len()
}

/// The member name whose encoding (see [`member_len`]) is `member`, or
/// `None` for a key with no member name.
fn member_name(member: &[u8]) -> Result<Option<String>, ManifestJoinError> {
    let Some((&1, mut rest)) = member.split_first() else {
        return Ok(None);
    };
    let mut name = Vec::with_capacity(rest.len());
    loop {
        match rest {
            [0, 1] => break,
            [0, 0xFF, after @ ..] => {
                name.push(0);
                rest = after;
            }
            [byte, after @ ..] if *byte != 0 => {
                name.push(*byte);
                rest = after;
            }
            _ => return Err(sort_error("a malformed member name in a key")),
        }
    }
    String::from_utf8(name)
        .map(Some)
        .map_err(|_| sort_error("a member name in a key is not UTF-8"))
}

fn site_value(file: u32, position: u32) -> [u8; 8] {
    let mut value = [0; 8];
    value[..4].copy_from_slice(&file.to_le_bytes());
    value[4..].copy_from_slice(&position.to_le_bytes());
    value
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, ManifestJoinError> {
    let end = *at + 4;
    let value = bytes
        .get(*at..end)
        .ok_or_else(|| sort_error("a truncated record"))?;
    *at = end;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

/// Values kept once each, numbered in the order first seen. Shared by the
/// threads resolving keys.
struct Interner<T> {
    table: Mutex<(HashMap<Arc<T>, u32>, Vec<Arc<T>>)>,
}

impl<T: Hash + Eq> Interner<T> {
    fn new() -> Self {
        Self {
            table: Mutex::new((HashMap::new(), Vec::new())),
        }
    }

    fn intern(&self, value: T) -> Result<u32, ManifestJoinError> {
        self.intern_all([value]).map(|ids| ids[0])
    }

    /// The numbers of `values`, in order, taking the lock once.
    fn intern_all(
        &self,
        values: impl IntoIterator<Item = T>,
    ) -> Result<Vec<u32>, ManifestJoinError> {
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (numbers, values_by_number) = &mut *table;
        values
            .into_iter()
            .map(|value| {
                if let Some(&id) = numbers.get(&value) {
                    return Ok(id);
                }
                let id = u32::try_from(values_by_number.len())
                    .map_err(|_| too_many("distinct answers or dependency keys"))?;
                let value = Arc::new(value);
                values_by_number.push(Arc::clone(&value));
                numbers.insert(value, id);
                Ok(id)
            })
            .collect()
    }

    fn into_values(self) -> Vec<Arc<T>> {
        self.table
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .1
    }
}

/// The dependency keys a key's resolution recorded.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Dependencies {
    None,
    /// A set kept once in [`Answers::dependencies`].
    Shared(u32),
    /// A small set, written with the key's first site in each file.
    Inline(Box<[String]>),
}

/// One key's resolution: its answer's number in [`Answers::answers`] and
/// the dependency keys it recorded.
struct KeyAnswer {
    answer: u32,
    dependencies: Dependencies,
    #[cfg(feature = "callgraph-shape-census")]
    exit: u8,
}

/// The resolver exits the shape census classifies sites by, numbered so a
/// site's result can carry its key's exit.
#[cfg(feature = "callgraph-shape-census")]
static EXITS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

#[cfg(feature = "callgraph-shape-census")]
fn exit_code(exit: &'static str) -> u8 {
    let mut exits = EXITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let code = exits
        .iter()
        .position(|known| *known == exit)
        .unwrap_or_else(|| {
            exits.push(exit);
            exits.len() - 1
        });
    u8::try_from(code).expect("fewer than 256 resolver exits")
}

#[cfg(feature = "callgraph-shape-census")]
fn exit_name(code: u8) -> &'static str {
    EXITS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())[usize::from(code)]
}

/// Appends a site's result: its key's answer and, for the first site of
/// that key in its file, its key's dependency keys.
fn encode_result(answer: &KeyAnswer, with_dependencies: bool, out: &mut Vec<u8>) {
    out.extend(answer.answer.to_le_bytes());
    match (&answer.dependencies, with_dependencies) {
        (Dependencies::None, _) | (_, false) => out.push(0),
        (Dependencies::Shared(id), true) => {
            out.push(1);
            out.extend(id.to_le_bytes());
        }
        (Dependencies::Inline(keys), true) => {
            out.push(2);
            out.extend(
                u32::try_from(keys.len())
                    .expect("at most INLINE_DEPENDENCIES keys")
                    .to_le_bytes(),
            );
            for key in keys.iter() {
                out.extend(
                    u32::try_from(key.len())
                        .expect("a dependency key under 4 GiB")
                        .to_le_bytes(),
                );
                out.extend(key.as_bytes());
            }
        }
    }
    #[cfg(feature = "callgraph-shape-census")]
    out.push(answer.exit);
}

/// What a scratch file of site positions starts with.
const SITES_MAGIC: &[u8; 8] = b"AFTSITE1";

/// The positions (file, position) of the sites waiting for their keys'
/// answers, in key order. Past `memory` positions the rest go to a scratch
/// file, written once and read back once.
struct SiteBuffer {
    memory: Vec<(u32, u32)>,
    cap: usize,
    overflow: Option<Overflow>,
}

/// A scratch file of site positions being written. Its header names the
/// flush it belongs to and the number of that flush's first key; its footer
/// the number of keys and sites it covers and the checksum of everything
/// before it.
struct Overflow {
    path: PathBuf,
    out: BufWriter<std::fs::File>,
    hasher: blake3::Hasher,
    sites: u64,
}

impl SiteBuffer {
    fn new(cap: usize) -> Self {
        Self {
            memory: Vec::new(),
            cap,
            overflow: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.memory.is_empty() && self.overflow.is_none()
    }

    fn push(
        &mut self,
        site: (u32, u32),
        runs: &ScratchDir<DataError>,
        names: &mut usize,
        membership: (u64, u64),
    ) -> std::io::Result<()> {
        if self.overflow.is_none() && self.memory.len() < self.cap {
            if self.memory.len() == self.memory.capacity() {
                // Grow on demand, but do not round a non-power-of-two site
                // budget up to the vector's next geometric capacity.
                let capacity = self
                    .memory
                    .capacity()
                    .saturating_mul(2)
                    .max(4)
                    .min(self.cap);
                self.memory.reserve_exact(capacity - self.memory.len());
            }
            self.memory.push(site);
            return Ok(());
        }
        let overflow = match &mut self.overflow {
            Some(overflow) => overflow,
            None => {
                *names += 1;
                let path = runs.path().join(format!("sites-{names}"));
                let mut out = BufWriter::new(std::fs::File::create_new(&path)?);
                let mut header = SITES_MAGIC.to_vec();
                header.extend(membership.0.to_le_bytes());
                header.extend(membership.1.to_le_bytes());
                out.write_all(&header)?;
                let mut hasher = blake3::Hasher::new();
                hasher.update(&header);
                self.overflow.insert(Overflow {
                    path,
                    out,
                    hasher,
                    sites: 0,
                })
            }
        };
        // Checks the build's free-disk policy before each further `cap`
        // positions are written.
        if overflow.sites % self.cap as u64 == 0 {
            runs.workspace()
                .check_space((self.cap * SITE_BYTES) as u64)
                .map_err(std::io::Error::other)?;
        }
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&site.0.to_le_bytes());
        bytes[4..].copy_from_slice(&site.1.to_le_bytes());
        overflow.out.write_all(&bytes)?;
        overflow.hasher.update(&bytes);
        overflow.sites += 1;
        Ok(())
    }

    /// The positions written so far, for reading back in order; the buffer
    /// is empty again. `keys` is how many keys they belong to, for the
    /// scratch file's footer.
    fn drain(&mut self, keys: u64) -> std::io::Result<SiteReader> {
        let file = match self.overflow.take() {
            None => None,
            Some(mut overflow) => {
                let mut footer = Vec::with_capacity(48);
                footer.extend(keys.to_le_bytes());
                footer.extend(overflow.sites.to_le_bytes());
                overflow.hasher.update(&footer);
                footer.extend(overflow.hasher.finalize().as_bytes());
                overflow.out.write_all(&footer)?;
                // Closed before it is read. Not synced: a build that does
                // not finish starts this flush again.
                drop(
                    overflow
                        .out
                        .into_inner()
                        .map_err(|error| error.into_error())?,
                );
                Some(OverflowReader::open(overflow.path, overflow.sites)?)
            }
        };
        Ok(SiteReader {
            memory: std::mem::take(&mut self.memory).into_iter(),
            file,
        })
    }
}

/// A scratch file of site positions read back, checked against its
/// header, length and checksum.
struct OverflowReader {
    path: PathBuf,
    input: BufReader<std::fs::File>,
    hasher: blake3::Hasher,
    sites: u64,
    remaining: u64,
}

impl OverflowReader {
    fn open(path: PathBuf, sites: u64) -> std::io::Result<Self> {
        let mut input = BufReader::new(std::fs::File::open(&path)?);
        let mut header = [0; 24];
        input.read_exact(&mut header)?;
        if &header[..8] != SITES_MAGIC {
            return Err(Self::changed(&path));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header);
        Ok(Self {
            path,
            input,
            hasher,
            sites,
            remaining: sites,
        })
    }

    fn changed(path: &std::path::Path) -> std::io::Error {
        std::io::Error::other(format!(
            "the site positions in {} changed after they were written",
            path.display()
        ))
    }

    fn next(&mut self) -> std::io::Result<Option<(u32, u32)>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut bytes = [0; 8];
        self.input.read_exact(&mut bytes)?;
        self.hasher.update(&bytes);
        self.remaining -= 1;
        Ok(Some((
            u32::from_le_bytes(bytes[..4].try_into().expect("four bytes")),
            u32::from_le_bytes(bytes[4..].try_into().expect("four bytes")),
        )))
    }

    /// Checks the footer and that nothing follows it, then deletes the file.
    fn finish(mut self, keys: u64) -> std::io::Result<()> {
        if self.remaining != 0 {
            return Err(Self::changed(&self.path));
        }
        let mut footer = [0; 48];
        self.input.read_exact(&mut footer)?;
        let mut expected = keys.to_le_bytes().to_vec();
        expected.extend(self.sites.to_le_bytes());
        self.hasher.update(&expected);
        let mut rest = Vec::new();
        self.input.read_to_end(&mut rest)?;
        if footer[..16] != expected[..]
            || footer[16..] != *self.hasher.finalize().as_bytes()
            || !rest.is_empty()
        {
            return Err(Self::changed(&self.path));
        }
        drop(self.input);
        std::fs::remove_file(&self.path)
    }
}

/// [`SiteBuffer::drain`]'s positions, in the order pushed.
struct SiteReader {
    memory: std::vec::IntoIter<(u32, u32)>,
    file: Option<OverflowReader>,
}

impl SiteReader {
    fn next(&mut self) -> std::io::Result<(u32, u32)> {
        if let Some(site) = self.memory.next() {
            return Ok(site);
        }
        match &mut self.file {
            Some(file) => file.next()?,
            None => None,
        }
        .ok_or_else(|| std::io::Error::other("fewer site positions than written"))
    }

    /// Checks every position was read, and the scratch file if there was
    /// one; `keys` is how many keys its footer must say it covers.
    fn finish(self, keys: u64) -> std::io::Result<()> {
        if self.memory.len() != 0 {
            return Err(std::io::Error::other("more site positions than read"));
        }
        match self.file {
            Some(file) => file.finish(keys),
            None => Ok(()),
        }
    }
}

/// One site's result as read back in file order.
#[derive(Debug)]
pub(super) struct SiteResult {
    file: u32,
    position: u32,
    answer: u32,
    dependencies: Dependencies,
    #[cfg(feature = "callgraph-shape-census")]
    exit: u8,
}

fn decode_result(key: &[u8], value: &[u8]) -> Result<SiteResult, ManifestJoinError> {
    let (file, position) = match key {
        [a, b, c, d, e, f, g, h] => (
            u32::from_be_bytes([*a, *b, *c, *d]),
            u32::from_be_bytes([*e, *f, *g, *h]),
        ),
        _ => return Err(sort_error("a result with a malformed key")),
    };
    let mut at = 0;
    let answer = read_u32(value, &mut at)?;
    let tag = *value
        .get(at)
        .ok_or_else(|| sort_error("a truncated record"))?;
    at += 1;
    let dependencies = match tag {
        0 => Dependencies::None,
        1 => Dependencies::Shared(read_u32(value, &mut at)?),
        2 => {
            let count = read_u32(value, &mut at)?;
            let mut keys = Vec::with_capacity(count.min(INLINE_DEPENDENCIES as u32) as usize);
            for _ in 0..count {
                let len = read_u32(value, &mut at)? as usize;
                let bytes = value
                    .get(at..at + len)
                    .ok_or_else(|| sort_error("a truncated record"))?;
                at += len;
                keys.push(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|_| sort_error("a dependency key that is not UTF-8"))?,
                );
            }
            Dependencies::Inline(keys.into_boxed_slice())
        }
        _ => return Err(sort_error("a result with an unknown dependency tag")),
    };
    #[cfg(feature = "callgraph-shape-census")]
    let exit = {
        let exit = *value
            .get(at)
            .ok_or_else(|| sort_error("a truncated record"))?;
        at += 1;
        exit
    };
    if at != value.len() {
        return Err(sort_error("a result with trailing bytes"));
    }
    Ok(SiteResult {
        file,
        position,
        answer,
        dependencies,
        #[cfg(feature = "callgraph-shape-census")]
        exit,
    })
}

/// Every site's result, sorted by file and site position, read one file
/// at a time in file order.
pub(super) struct SiteResults {
    records: SortedRecords,
    next: Option<SiteResult>,
}

impl SiteResults {
    fn pull(&mut self) -> Result<Option<SiteResult>, ManifestJoinError> {
        match self.records.next_record().map_err(sort_error)? {
            Some(record) => decode_result(record.key, record.value).map(Some),
            None => Ok(None),
        }
    }

    /// The results of the file numbered `file`, in site order. Files must
    /// be asked for in increasing order.
    pub(super) fn take_file(&mut self, file: u32) -> Result<Vec<SiteResult>, ManifestJoinError> {
        let mut results = Vec::new();
        loop {
            if self.next.is_none() {
                self.next = self.pull()?;
            }
            match &self.next {
                Some(next) if next.file == file => {
                    results.push(self.next.take().expect("just matched"));
                }
                Some(next) if next.file < file => {
                    return Err(sort_error(format!(
                        "a result for file {} read after file {file}",
                        next.file
                    )));
                }
                _ => return Ok(results),
            }
        }
    }

    /// Checks that no result is left over for a file never asked for.
    pub(super) fn finish(mut self) -> Result<(), ManifestJoinError> {
        if self.next.is_some() || self.pull()?.is_some() {
            return Err(sort_error("a result for a file with no record"));
        }
        Ok(())
    }
}

/// The distinct answers and the large dependency sets that site results
/// name by number. A large set is kept as the numbers of its keys in
/// `keys`: sets of keys with the same member mostly share their keys, so
/// each key's text is kept once.
pub(super) struct Answers {
    answers: Vec<Arc<Resolution>>,
    dependencies: Vec<Arc<Vec<u32>>>,
    keys: Vec<Arc<String>>,
}

impl Answers {
    /// `path`'s kept sites and site dependencies, from `record`, its
    /// projected record, and `results`, its sites' results in site order:
    /// what pass 4 stores for the file. Every site's dependency keys count,
    /// including those of Rust sites left out because pass 2 already
    /// linked them, and every file depends on its own facts.
    pub(super) fn assemble(
        &self,
        path: &str,
        record: &FileRecord,
        results: Vec<SiteResult>,
        #[cfg(feature = "callgraph-shape-census")]
        census: &mut crate::data_layer::shape_census::Census,
    ) -> Result<(Vec<SiteResolution>, BTreeSet<String>), ManifestJoinError> {
        let records = record.sites();
        if results.len() != records.len() {
            return Err(sort_error(format!(
                "{path} has {} method-call sites and {} results",
                records.len(),
                results.len()
            )));
        }
        let language = record.facts().language.as_str();
        let mut dependencies = BTreeSet::new();
        let mut shared = HashSet::new();
        let mut shared_keys = Vec::new();
        let mut sites = Vec::new();
        for (position, (site, result)) in records.iter().zip(results).enumerate() {
            if result.position as usize != position {
                return Err(sort_error(format!(
                    "{path}: site {position} has the result of site {}",
                    result.position
                )));
            }
            match result.dependencies {
                Dependencies::None => {}
                Dependencies::Shared(id) => {
                    if shared.insert(id) {
                        let keys = self
                            .dependencies
                            .get(id as usize)
                            .ok_or_else(|| sort_error("an unknown dependency set"))?;
                        shared_keys.extend(keys.iter().copied());
                    }
                }
                Dependencies::Inline(keys) => dependencies.extend(keys.into_vec()),
            }
            let resolution = self
                .answers
                .get(result.answer as usize)
                .ok_or_else(|| sort_error("an unknown answer"))?;
            // Before Rust's ordinary-edge preservation can omit a
            // supplemental method-call row.
            #[cfg(feature = "callgraph-shape-census")]
            census.record(language, &site.site, exit_name(result.exit), resolution);
            // Keep proven self-call and scoped module-function bindings
            // when dispatch facts do not identify a project receiver type.
            if language == "rust"
                && resolution.external > 0
                && resolution.targets.is_empty()
                && site.keeps_ordinary_edge
            {
                continue;
            }
            sites.push(SiteResolution {
                site: site.site.clone(),
                resolution: Arc::clone(resolution),
            });
        }
        shared_keys.sort_unstable();
        shared_keys.dedup();
        for key in shared_keys {
            let key = self
                .keys
                .get(key as usize)
                .ok_or_else(|| sort_error("an unknown dependency key"))?;
            dependencies.insert(key.as_str().to_owned());
        }
        // `Resolver::resolve` reads the caller's own facts without
        // recording them.
        dependencies.insert(dependency::file(path));
        Ok((sites, dependencies))
    }
}

/// One key waiting for its answer: its first site, which it is resolved
/// from, and how many sites have it.
struct KeyWork {
    file: u32,
    position: u32,
    sites: u32,
}

/// Where a group's keys start among a flush's keys.
struct Segment {
    start: usize,
    group: u64,
}

/// The keys waiting for their answers, with their sites' positions.
struct Unit {
    keys: Vec<KeyWork>,
    /// Each group's first key, in key order. A group's keys may be split
    /// into several segments, so that threads can share them.
    segments: Vec<Segment>,
    /// Where the keys of a group every thread resolves at once start; they
    /// are the last ones.
    shared_from: Option<usize>,
    sites: SiteBuffer,
    /// The number, over the whole pass, of `keys[0]`.
    first_key: u64,
}

/// The group being read: the keys of one member name.
struct Group<'s> {
    /// The member name's encoding at the start of its keys.
    member: Vec<u8>,
    name: Option<String>,
    number: u64,
    /// The records declaring the group's methods, held while the group is
    /// read, when every thread resolves the group at once: each flush
    /// loads the methods from them once for every thread to read.
    shared: Option<Pinned<'s>>,
}

impl<'s> Group<'s> {
    /// The group's name and declaring records, when every thread resolves
    /// it at once.
    fn shared(&self) -> Option<(&str, &Pinned<'s>)> {
        Some((self.name.as_deref()?, self.shared.as_ref()?))
    }
}

/// What a group's resolution read, for measurement.
#[derive(Default)]
struct GroupTally {
    touches: Touches,
    keys: u64,
    sites: u64,
    methods: usize,
}

/// What the groups' resolutions read, summed up when the pass ends.
#[derive(Default)]
struct GroupStats {
    /// Per finished group: the files whose records it read, and how many
    /// record lookups it made.
    groups: Vec<(u64, u64)>,
    /// The group that read the most files: its name, tally and the
    /// records it read.
    largest: Option<(Option<String>, GroupTally)>,
    names: BTreeMap<u64, Option<String>>,
    shared: u64,
    largest_shared_methods: usize,
    /// The most records a shared group held for its methods.
    largest_shared_records: usize,
}

impl GroupStats {
    fn finish_group(&mut self, number: u64, tally: GroupTally) {
        let name = self.names.remove(&number).flatten();
        let files = tally.touches.files.len() as u64;
        self.groups.push((files, tally.touches.lookups));
        if self
            .largest
            .as_ref()
            .is_none_or(|(_, largest)| largest.touches.files.len() as u64 <= files)
        {
            self.largest = Some((name, tally));
        }
    }
}

/// What the threads resolving keys share.
struct Context<'s> {
    source: &'s RecordSource,
    paths: &'s [String],
    answers: Interner<Resolution>,
    dependencies: Interner<Vec<u32>>,
    dependency_keys: Interner<String>,
    counting: bool,
}

impl<'s> Context<'s> {
    /// The record of the file numbered `file`, read through `pinned`.
    fn record<'p>(
        &self,
        pinned: &'p Pinned<'_>,
        file: u32,
    ) -> Result<(&'s str, &'p FileRecord), ManifestJoinError> {
        let path = self
            .paths
            .get(file as usize)
            .ok_or_else(|| sort_error(format!("no file numbered {file}")))?;
        let record = pinned.record_at(file).ok_or_else(|| {
            ManifestJoinError::Parse(
                self.source
                    .take_error()
                    .unwrap_or_else(|| format!("missing method-call record for {path}")),
            )
        })?;
        Ok((path.as_str(), record))
    }

    fn resolve_key<'p>(
        &self,
        pinned: &'p Pinned<'_>,
        resolver: &Resolver<'p>,
        key: &KeyWork,
    ) -> Result<KeyAnswer, ManifestJoinError> {
        let (path, record) = self.record(pinned, key.file)?;
        let site = &record
            .sites()
            .get(key.position as usize)
            .ok_or_else(|| sort_error(format!("{path} has no site {}", key.position)))?
            .site;
        resolver.take_dependencies();
        let resolution = resolver.resolve(path, site);
        let keys = resolver.take_dependencies();
        let dependencies = match keys.len() {
            0 => Dependencies::None,
            count if count <= INLINE_DEPENDENCIES => {
                Dependencies::Inline(keys.into_iter().collect())
            }
            _ => Dependencies::Shared(
                self.dependencies
                    .intern(self.dependency_keys.intern_all(keys)?)?,
            ),
        };
        Ok(KeyAnswer {
            answer: self.answers.intern(resolution)?,
            dependencies,
            #[cfg(feature = "callgraph-shape-census")]
            exit: exit_code(resolver.census_exit.get()),
        })
    }

    /// Resolves `keys` on this thread, reading `shared` when given.
    /// `segments` are where each group's keys start among `keys`; each
    /// segment gets its own resolver and its own set of held records, so a
    /// thread holds the records of one group at a time. When counting, each
    /// group's record lookups are tallied.
    fn resolve_keys(
        &self,
        keys: &[KeyWork],
        segments: &[(usize, u64)],
        shared: Option<&SharedMethods<'_>>,
    ) -> Result<(Vec<KeyAnswer>, Vec<(u64, Touches)>), ManifestJoinError> {
        let mut answers = Vec::with_capacity(keys.len());
        let mut tallies = Vec::new();
        for (index, (start, group)) in segments.iter().enumerate() {
            let end = segments.get(index + 1).map_or(keys.len(), |next| next.0);
            if self.counting {
                site_records::start_touches();
            }
            let pinned = Pinned::cached(self.source);
            let resolver = Resolver::over(&pinned);
            if let Some(shared) = shared {
                resolver.use_shared_methods(shared);
            }
            for key in &keys[*start..end] {
                answers.push(self.resolve_key(&pinned, &resolver, key)?);
            }
            if self.counting {
                tallies.push((*group, site_records::take_touches()));
            }
        }
        Ok((answers, tallies))
    }

    /// The records declaring `name`'s methods, read and held for every
    /// flush of the group to load the methods from, with how many methods
    /// there are and what reading them touched.
    fn share(&self, name: &str) -> (Pinned<'s>, usize, Touches) {
        if self.counting {
            site_records::start_touches();
        }
        let pinned = Pinned::cached(self.source);
        let methods = shared_methods(&pinned, name).len();
        let touches = if self.counting {
            site_records::take_touches()
        } else {
            Touches::default()
        };
        (pinned, methods, touches)
    }
}

/// The methods named `name`, loaded from the records `pinned` holds or
/// reads, for every thread to read.
fn shared_methods<'p>(pinned: &'p Pinned<'_>, name: &str) -> SharedMethods<'p> {
    Resolver::over(pinned)
        .share_methods(name)
        .expect("a resolver over records has a source")
}

/// Writes every site's key, in file order, to `sorter`, labelling each
/// file range's records with the range's number. Returns how many sites
/// there are.
fn collect_keys(context: &Context<'_>, sorter: &mut Sorter) -> Result<u64, ManifestJoinError> {
    let mut sites = 0u64;
    let threads = rayon::current_num_threads().max(1);
    let carry = crate::alloc_trace::carry();
    for (range, batch) in context.paths.chunks(KEY_BATCH).enumerate() {
        let first = range * KEY_BATCH;
        let chunk = batch.len().div_ceil(threads * 4).max(1);
        let keyed = batch
            .par_chunks(chunk)
            .enumerate()
            .map(|(index, files)| {
                let _label = carry.enter();
                // Every record is read once here, in file order, so each is
                // read from disk for this chunk alone, not cached.
                let pinned = Pinned::direct(context.source);
                let resolver = Resolver::over(&pinned);
                let mut keyed = Vec::new();
                for (offset, path) in files.iter().enumerate() {
                    let file = u32::try_from(first + index * chunk + offset)
                        .map_err(|_| too_many("files"))?;
                    let (_, record) = context.record(&pinned, file)?;
                    for (position, site) in record.sites().iter().enumerate() {
                        let key = resolver.resolution_key(path, &site.site);
                        let mut bytes = Vec::new();
                        encode_key(&key, file, &mut bytes);
                        let position =
                            u32::try_from(position).map_err(|_| too_many("sites in a file"))?;
                        keyed.push((bytes, site_value(file, position)));
                    }
                }
                Ok(keyed)
            })
            .collect::<Result<Vec<_>, ManifestJoinError>>()?;
        sorter.set_source_range(range as u64);
        for (key, value) in keyed.into_iter().flatten() {
            sorter.push(&key, &value).map_err(sort_error)?;
            sites += 1;
        }
    }
    Ok(sites)
}

/// The emitting side of the pass: the run of site results, and the counts
/// it reports.
struct Flusher<'a, 's> {
    context: &'a Context<'s>,
    results: Sorter,
    threads: usize,
    /// The most keys one thread's pack of small groups takes.
    pack_keys: usize,
    flushes: u64,
    scratch_files: usize,
    tallies: BTreeMap<u64, GroupTally>,
    stats: Option<GroupStats>,
}

impl Flusher<'_, '_> {
    /// Resolves the waiting keys and writes their sites' results. Small
    /// groups are packed per thread; a shared group's keys, which come last,
    /// are then split among every thread, all reading the methods loaded
    /// once from `shared`: the shared group's name and its declaring
    /// records.
    fn flush(
        &mut self,
        unit: &mut Unit,
        shared: Option<(&str, &Pinned<'_>)>,
    ) -> Result<(), ManifestJoinError> {
        if unit.keys.is_empty() {
            return Ok(());
        }
        let context = self.context;
        let small_end = unit.shared_from.unwrap_or(unit.keys.len());
        // Packs of whole segments, each about a quarter of one thread's
        // share, so that the threads finish together.
        let target = small_end
            .div_ceil(self.threads * 4)
            .clamp(1, self.pack_keys);
        let mut packs: Vec<(std::ops::Range<usize>, Vec<(usize, u64)>)> = Vec::new();
        for (index, segment) in unit.segments.iter().enumerate() {
            if segment.start >= small_end {
                break;
            }
            let end = unit
                .segments
                .get(index + 1)
                .map_or(small_end, |next| next.start.min(small_end));
            match packs.last_mut() {
                Some((range, segments)) if range.end - range.start < target => {
                    segments.push((segment.start - range.start, segment.group));
                    range.end = end;
                }
                _ => packs.push((segment.start..end, vec![(0, segment.group)])),
            }
        }
        let carry = crate::alloc_trace::carry();
        let mut answers = Vec::with_capacity(unit.keys.len());
        let mut tallies = Vec::new();
        let packed = packs
            .par_iter()
            .map(|(range, segments)| {
                let _label = carry.enter();
                context.resolve_keys(&unit.keys[range.clone()], segments, None)
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (pack, touched) in packed {
            answers.extend(pack);
            tallies.extend(touched);
        }
        if small_end < unit.keys.len() {
            let (name, declarers) =
                shared.ok_or_else(|| sort_error("a shared group without its methods"))?;
            // Loaded from records the group holds, so nothing is read again.
            let methods = shared_methods(declarers, name);
            let group = unit
                .segments
                .iter()
                .rev()
                .find(|segment| segment.start <= small_end)
                .map_or(0, |segment| segment.group);
            let keys = &unit.keys[small_end..];
            let chunk = keys.len().div_ceil(self.threads * 4).max(1);
            let resolved = keys
                .par_chunks(chunk)
                .map(|keys| {
                    let _label = carry.enter();
                    context.resolve_keys(keys, &[(0, group)], Some(&methods))
                })
                .collect::<Result<Vec<_>, _>>()?;
            for (chunk, touched) in resolved {
                answers.extend(chunk);
                tallies.extend(touched);
            }
        }
        if let Some(error) = context.source.take_error() {
            return Err(ManifestJoinError::Parse(error));
        }
        if answers.len() != unit.keys.len() {
            return Err(sort_error(format!(
                "{} answers for {} keys",
                answers.len(),
                unit.keys.len()
            )));
        }
        let keys = unit.keys.len() as u64;
        let mut sites = unit.sites.drain(keys).map_err(sort_error)?;
        self.results.set_source_range(self.flushes);
        let mut value = Vec::new();
        for (key, answer) in unit.keys.iter().zip(&answers) {
            let mut last_file = None;
            for _ in 0..key.sites {
                let (file, position) = sites.next().map_err(sort_error)?;
                value.clear();
                encode_result(answer, last_file != Some(file), &mut value);
                last_file = Some(file);
                let mut key = [0; 8];
                key[..4].copy_from_slice(&file.to_be_bytes());
                key[4..].copy_from_slice(&position.to_be_bytes());
                self.results.push(&key, &value).map_err(sort_error)?;
            }
        }
        sites.finish(keys).map_err(sort_error)?;
        for (group, touches) in tallies {
            self.tallies
                .entry(group)
                .or_default()
                .touches
                .merge(touches);
        }
        self.flushes += 1;
        unit.first_key += keys;
        unit.keys.clear();
        unit.segments.clear();
        unit.shared_from = None;
        Ok(())
    }

    /// Counts the tallies of the groups before `current` (all of them when
    /// `None`) as finished.
    fn finish_groups(&mut self, current: Option<u64>) {
        let Some(stats) = &mut self.stats else {
            return;
        };
        let finished = match current {
            Some(current) => {
                let rest = self.tallies.split_off(&current);
                std::mem::replace(&mut self.tallies, rest)
            }
            None => std::mem::take(&mut self.tallies),
        };
        for (group, tally) in finished {
            stats.finish_group(group, tally);
        }
    }
}

/// Resolves every site of `paths` (every file with a record, in path
/// order) once per distinct key, as the module comment describes. Writes
/// its runs in `runs`; adds to `memory`, when given, what it held and read.
///
/// # Errors
///
/// When a file has no record, a record the resolver needed is missing, or
/// a run cannot be written or read back as written.
pub(super) fn resolve(
    source: &RecordSource,
    paths: &[String],
    runs: &ScratchDir<DataError>,
    budget: Budget,
    memory: Option<&mut Vec<(String, usize)>>,
) -> Result<(Answers, SiteResults), ManifestJoinError> {
    // A file's number is its position in `paths` and among the records.
    if source.paths() != paths {
        return Err(sort_error("the records are not of the files resolved"));
    }
    let guard = {
        let workspace = Arc::clone(runs.workspace());
        let bytes = budget.sort_bytes as u64;
        WriteGuard::new(move || workspace.check_space(bytes).map_err(std::io::Error::other))
    };
    let context = Context {
        source,
        paths,
        answers: Interner::new(),
        dependencies: Interner::new(),
        dependency_keys: Interner::new(),
        counting: memory.is_some(),
    };
    let mut keys = Sorter::new(runs.path(), budget.sort_config()).map_err(sort_error)?;
    keys.set_write_guard(guard.clone()).map_err(sort_error)?;
    let site_count = collect_keys(&context, &mut keys)?;
    let mut sorted = keys.finish().map_err(sort_error)?;
    let mut results = Sorter::new(runs.path(), budget.sort_config()).map_err(sort_error)?;
    results.set_write_guard(guard).map_err(sort_error)?;
    let threads = rayon::current_num_threads().max(1);
    let unit_keys = budget.unit_keys();
    let mut flusher = Flusher {
        context: &context,
        results,
        threads,
        pack_keys: (unit_keys / threads).max(1),
        flushes: 0,
        scratch_files: 0,
        tallies: BTreeMap::new(),
        stats: context.counting.then(GroupStats::default),
    };
    let mut unit = Unit {
        keys: Vec::new(),
        segments: Vec::new(),
        shared_from: None,
        sites: SiteBuffer::new(budget.memory_sites()),
        first_key: 0,
    };
    let mut last = Vec::new();
    let mut group: Option<Group<'_>> = None;
    let mut groups = 0u64;
    let mut key_count = 0u64;
    while let Some(record) = sorted.next_record().map_err(sort_error)? {
        let mut at = 0;
        let file = read_u32(record.value, &mut at)?;
        let position = read_u32(record.value, &mut at)?;
        if group.is_none() || record.key != last.as_slice() {
            last.clear();
            last.extend_from_slice(record.key);
            let member = &last[..member_len(&last)];
            if group.as_ref().is_none_or(|group| group.member != member) {
                // A new group. A shared group's keys must be the last of a
                // flush, so the one before goes first.
                let number = groups;
                groups += 1;
                if group.as_ref().is_some_and(|group| group.shared.is_some())
                    || unit.keys.len() >= unit_keys
                {
                    flusher.flush(&mut unit, group.as_ref().and_then(Group::shared))?;
                    flusher.finish_groups(Some(number));
                }
                // The group before lets go of the records it held, before
                // the next group may read its own.
                drop(group.take());
                let mut next = Group {
                    member: member.to_vec(),
                    name: member_name(member)?,
                    number,
                    shared: None,
                };
                if next
                    .name
                    .as_deref()
                    .is_some_and(|name| source.declarers(name) >= SHARED_DECLARERS)
                {
                    share(&context, &mut flusher, &mut next);
                    unit.shared_from = Some(unit.keys.len());
                }
                if let Some(stats) = &mut flusher.stats {
                    stats.names.insert(number, next.name.clone());
                }
                unit.segments.push(Segment {
                    start: unit.keys.len(),
                    group: number,
                });
                group = Some(next);
            } else if unit.keys.len() >= unit_keys {
                let current = group.as_ref().expect("a key continues its group");
                flusher.flush(&mut unit, current.shared())?;
                flusher.finish_groups(Some(current.number));
                unit.segments.push(Segment {
                    start: 0,
                    group: current.number,
                });
                if current.shared.is_some() {
                    unit.shared_from = Some(0);
                }
            }
            let current = group.as_mut().expect("a key has its group");
            let start = unit.segments.last().expect("the group's segment").start;
            if current.shared.is_none() && unit.keys.len() - start >= flusher.pack_keys {
                // More keys than one thread's share: every thread resolves
                // the group at once, reading one copy of its methods. A
                // group with no name has no methods to share; its keys are
                // split into more segments instead.
                if current.name.is_some() {
                    share(&context, &mut flusher, current);
                    unit.shared_from = Some(start);
                } else {
                    unit.segments.push(Segment {
                        start: unit.keys.len(),
                        group: current.number,
                    });
                }
            }
            unit.keys.push(KeyWork {
                file,
                position,
                sites: 0,
            });
            key_count += 1;
            if flusher.stats.is_some() {
                flusher.tallies.entry(current.number).or_default().keys += 1;
            }
        }
        let membership = (flusher.flushes, unit.first_key);
        unit.sites
            .push(
                (file, position),
                runs,
                &mut flusher.scratch_files,
                membership,
            )
            .map_err(sort_error)?;
        let key = unit.keys.last_mut().expect("a site has its key");
        key.sites = key
            .sites
            .checked_add(1)
            .ok_or_else(|| too_many("sites of one key"))?;
        if flusher.stats.is_some() {
            let number = group.as_ref().expect("a site has its group").number;
            flusher.tallies.entry(number).or_default().sites += 1;
        }
    }
    flusher.flush(&mut unit, group.as_ref().and_then(Group::shared))?;
    flusher.finish_groups(None);
    drop(group);
    drop(sorted);
    if !unit.sites.is_empty() {
        return Err(sort_error("site positions left after the last flush"));
    }
    let Flusher {
        results,
        flushes,
        scratch_files,
        stats,
        ..
    } = flusher;
    let result_stats = results.stats();
    let answers = Answers {
        answers: context.answers.into_values(),
        dependencies: context.dependencies.into_values(),
        keys: context.dependency_keys.into_values(),
    };
    if let Some(memory) = memory {
        report(
            memory,
            source,
            &answers,
            stats.unwrap_or_default(),
            [
                ("pass4.sites", site_count),
                ("pass4.keys", key_count),
                ("pass4.groups", groups),
                ("pass4.flushes", flushes),
                ("pass4.site_scratch_files", scratch_files as u64),
                ("pass4.result_run_bytes_written", result_stats.bytes_written),
            ],
        );
    }
    let records = results.finish().map_err(sort_error)?;
    Ok((
        answers,
        SiteResults {
            records,
            next: None,
        },
    ))
}

/// Reads and holds the records declaring `group`'s methods, for every
/// thread to read the methods from, and tallies what reading them touched.
fn share<'s>(context: &Context<'s>, flusher: &mut Flusher<'_, 's>, group: &mut Group<'s>) {
    let name = group.name.as_deref().expect("a shared group has a name");
    let (declarers, methods, touches) = context.share(name);
    if let Some(stats) = &mut flusher.stats {
        stats.shared += 1;
        stats.largest_shared_methods = stats.largest_shared_methods.max(methods);
        stats.largest_shared_records = stats.largest_shared_records.max(declarers.len());
        let tally = flusher.tallies.entry(group.number).or_default();
        tally.touches.merge(touches);
        tally.methods = methods;
    }
    group.shared = Some(declarers);
}

/// Adds what the pass held and read to `memory`.
fn report(
    memory: &mut Vec<(String, usize)>,
    source: &RecordSource,
    answers: &Answers,
    stats: GroupStats,
    counts: [(&str, u64); 6],
) {
    use aft_store::heap::HeapBytes as _;
    let size = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
    for (name, value) in counts {
        memory.push((name.to_string(), size(value)));
    }
    let cache = source.cache_counts();
    memory.extend([
        (
            "pass4.record_disk_bytes".to_string(),
            size(source.disk_bytes()),
        ),
        ("pass4.record_cache_limit".to_string(), cache.limit),
        ("pass4.record_cache_peak".to_string(), cache.peak),
        (
            "pass4.record_peak_with_read_through".to_string(),
            cache.peak_with_outside,
        ),
        ("pass4.record_cache_held".to_string(), cache.held),
        ("pass4.record_reads".to_string(), size(cache.reads)),
        ("pass4.record_rereads".to_string(), size(cache.rereads)),
        (
            "pass4.record_read_through".to_string(),
            size(cache.read_through),
        ),
        (
            "pass4.record_direct_reads".to_string(),
            size(cache.direct_reads),
        ),
        ("pass4.record_evictions".to_string(), size(cache.evicted)),
        (
            "pass4.largest_shared_records".to_string(),
            stats.largest_shared_records,
        ),
    ]);
    let mut seen = aft_store::heap::Seen::new();
    memory.push(("pass4.answers".to_string(), answers.answers.len()));
    memory.push((
        "pass4.answer_bytes".to_string(),
        answers.answers.heap_bytes(&mut seen),
    ));
    memory.push((
        "pass4.shared_dependency_sets".to_string(),
        answers.dependencies.len(),
    ));
    memory.push((
        "pass4.shared_dependency_bytes".to_string(),
        answers.dependencies.heap_bytes(&mut seen),
    ));
    memory.push(("pass4.dependency_keys".to_string(), answers.keys.len()));
    memory.push((
        "pass4.dependency_key_bytes".to_string(),
        answers.keys.heap_bytes(&mut seen),
    ));
    memory.push(("pass4.shared_groups".to_string(), size(stats.shared)));
    memory.push((
        "pass4.largest_shared_methods".to_string(),
        stats.largest_shared_methods,
    ));
    let mut files = stats
        .groups
        .iter()
        .map(|(files, _)| *files)
        .collect::<Vec<_>>();
    let mut lookups = stats
        .groups
        .iter()
        .map(|(_, lookups)| *lookups)
        .collect::<Vec<_>>();
    files.sort_unstable();
    lookups.sort_unstable();
    let percentile = |values: &[u64], share: usize| {
        values
            .get((values.len() * share / 100).min(values.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    };
    memory.push((
        "pass4.record_lookups".to_string(),
        size(lookups.iter().sum()),
    ));
    memory.push((
        "pass4.group_files_max".to_string(),
        size(files.last().copied().unwrap_or(0)),
    ));
    memory.push((
        "pass4.group_files_p95".to_string(),
        size(percentile(&files, 95)),
    ));
    memory.push((
        "pass4.group_lookups_max".to_string(),
        size(lookups.last().copied().unwrap_or(0)),
    ));
    memory.push((
        "pass4.group_lookups_p95".to_string(),
        size(percentile(&lookups, 95)),
    ));
    if let Some((name, largest)) = stats.largest {
        // The heap of the records the largest group read: what a bounded
        // record cache must hold to resolve it without reading any twice.
        let record_bytes = source.record_bytes(largest.touches.files.iter().copied());
        log::info!(
            "pass-4 largest group: {} read {} files' records ({record_bytes} bytes) with {} lookups, {} keys, {} sites, {} methods",
            name.as_deref().unwrap_or("(no member)"),
            largest.touches.files.len(),
            largest.touches.lookups,
            largest.keys,
            largest.sites,
            largest.methods,
        );
        // The name rides in the measurement's label, which callers without
        // a logger still print.
        memory.push((
            format!(
                "pass4.largest_group_name={}",
                name.as_deref().unwrap_or("(no member)")
            ),
            1,
        ));
        memory.push((
            "pass4.largest_group_files".to_string(),
            largest.touches.files.len(),
        ));
        memory.push(("pass4.largest_group_record_bytes".to_string(), record_bytes));
        memory.push(("pass4.largest_group_keys".to_string(), size(largest.keys)));
        memory.push(("pass4.largest_group_sites".to_string(), size(largest.sites)));
        memory.push(("pass4.largest_group_methods".to_string(), largest.methods));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(member: Option<&str>, receiver: Option<&str>, scope: &str) -> ResolutionKey {
        ResolutionKey {
            scope: scope.to_string(),
            receiver: receiver.map(str::to_string),
            member: member.map(str::to_string),
            dynamic: false,
            associated: false,
            caller_scope: None,
        }
    }

    fn encoded(key: &ResolutionKey, file: u32) -> Vec<u8> {
        let mut out = Vec::new();
        encode_key(key, file, &mut out);
        out
    }

    #[test]
    fn keys_encode_equal_exactly_when_equal_and_group_by_member() {
        let untyped = key(Some("get"), None, "typescript");
        // An untyped key is scoped to the caller's language, not its file.
        assert_eq!(encoded(&untyped, 1), encoded(&untyped, 2));
        // A typed key is scoped to its calling file.
        let typed = key(Some("get"), Some("Map"), "src/a.ts");
        assert_ne!(encoded(&typed, 1), encoded(&typed, 2));
        let others = [
            key(Some("get"), None, "javascript"),
            key(Some("get\0"), None, "typescript"),
            key(Some("ge"), None, "typescript"),
            key(None, None, "typescript"),
            ResolutionKey {
                dynamic: true,
                ..key(Some("get"), None, "typescript")
            },
            ResolutionKey {
                associated: true,
                ..key(Some("get"), None, "typescript")
            },
            ResolutionKey {
                caller_scope: Some(String::new()),
                ..key(Some("get"), None, "typescript")
            },
        ];
        for other in &others {
            assert_ne!(encoded(&untyped, 1), encoded(other, 1), "{other:?}");
        }
        // Every key of a member sorts before any key of a member after it,
        // and its member's encoding is its prefix.
        let mut all = vec![
            encoded(&key(Some("get"), Some("Z"), "src/z.ts"), 9),
            encoded(&key(Some("ge"), None, "typescript"), 0),
            encoded(&key(Some("get"), None, "javascript"), 0),
            encoded(&key(Some("get\0x"), None, "typescript"), 0),
            encoded(&key(Some("geta"), None, "typescript"), 0),
            encoded(&key(None, None, "rust"), 0),
        ];
        all.sort();
        let members = all
            .iter()
            .map(|key| key[..member_len(key)].to_vec())
            .collect::<Vec<_>>();
        let mut grouped = members.clone();
        grouped.dedup();
        assert_eq!(grouped.len(), 5, "{members:?}");
        let mut get = Vec::new();
        put_optional(&mut get, Some("get"));
        assert_eq!(members.iter().filter(|member| **member == get).count(), 2);
        // Each group's name reads back from its member's encoding.
        let names = members
            .iter()
            .map(|member| member_name(member).unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            [None, Some("ge"), Some("get"), Some("get\0x"), Some("geta")]
                .into_iter()
                .map(|name| name.map(str::to_string))
                .collect()
        );
        assert!(member_name(&[1, b'g', 0]).is_err());
    }

    fn runs() -> (tempfile::TempDir, ScratchDir<DataError>) {
        let root = tempfile::tempdir().unwrap();
        let workspace = Arc::new(
            aft_store::family::BuildScratch::create_unattached(root.path(), |_| Ok(())).unwrap(),
        );
        let runs = workspace.subdir("runs").unwrap();
        (root, runs)
    }

    #[test]
    fn site_position_capacity_stays_within_its_budget_and_grows_on_demand() {
        let (_root, runs) = runs();
        let mut names = 0;
        assert_eq!(SiteBuffer::new(usize::MAX).memory.capacity(), 0);
        for cap in [1, 3, 5, 9, 2049] {
            let mut buffer = SiteBuffer::new(cap);
            for index in 0..cap + 2 {
                buffer
                    .push((0, index as u32), &runs, &mut names, (0, 0))
                    .unwrap();
                assert!(buffer.memory.capacity() <= cap, "cap={cap}");
                assert_eq!(buffer.memory.len(), (index + 1).min(cap));
            }
            let mut reader = buffer.drain(1).unwrap();
            for index in 0..cap + 2 {
                assert_eq!(reader.next().unwrap(), (0, index as u32));
            }
            reader.finish(1).unwrap();
        }
    }

    #[test]
    fn site_positions_past_memory_go_through_a_checked_scratch_file() {
        let (_root, runs) = runs();
        let mut names = 0;
        let mut buffer = SiteBuffer::new(2);
        let sites = (0..7).map(|index| (index, index * 10)).collect::<Vec<_>>();
        for site in &sites {
            buffer.push(*site, &runs, &mut names, (3, 40)).unwrap();
        }
        assert_eq!(names, 1);
        let mut reader = buffer.drain(5).unwrap();
        assert!(buffer.is_empty());
        for site in &sites {
            assert_eq!(reader.next().unwrap(), *site);
        }
        assert!(reader.next().is_err());
        reader.finish(5).unwrap();
        assert!(
            !runs.path().join("sites-1").exists(),
            "a scratch file read back is deleted"
        );

        // A scratch file changed after it was written fails the pass, as
        // does a footer naming other keys.
        for (change, keys) in [(true, 5), (false, 6)] {
            let mut buffer = SiteBuffer::new(1);
            for site in &sites {
                buffer.push(*site, &runs, &mut names, (3, 40)).unwrap();
            }
            let path = runs.path().join(format!("sites-{names}"));
            if change {
                // The reader opened by draining has already buffered the
                // small file, so the changed file is opened again.
                drop(buffer.drain(5).unwrap());
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[30] ^= 1;
                std::fs::write(&path, bytes).unwrap();
                let mut reader = OverflowReader::open(path, 6).unwrap();
                for _ in 0..6 {
                    reader.next().unwrap();
                }
                assert!(reader.finish(5).is_err());
            } else {
                let mut reader = buffer.drain(5).unwrap();
                for _ in &sites {
                    reader.next().unwrap();
                }
                assert!(reader.finish(keys).is_err());
            }
        }
    }

    #[test]
    fn site_results_decode_as_encoded() {
        let answers = [
            KeyAnswer {
                answer: 7,
                dependencies: Dependencies::None,
                #[cfg(feature = "callgraph-shape-census")]
                exit: 1,
            },
            KeyAnswer {
                answer: 8,
                dependencies: Dependencies::Shared(3),
                #[cfg(feature = "callgraph-shape-census")]
                exit: 2,
            },
            KeyAnswer {
                answer: 9,
                dependencies: Dependencies::Inline(
                    ["m:get".to_string(), "r:\u{e9}\0.rs".to_string()].into(),
                ),
                #[cfg(feature = "callgraph-shape-census")]
                exit: 3,
            },
        ];
        for answer in &answers {
            for with_dependencies in [true, false] {
                let mut value = Vec::new();
                encode_result(answer, with_dependencies, &mut value);
                let mut key = [0; 8];
                key[..4].copy_from_slice(&5u32.to_be_bytes());
                key[4..].copy_from_slice(&6u32.to_be_bytes());
                let decoded = decode_result(&key, &value).unwrap();
                assert_eq!((decoded.file, decoded.position), (5, 6));
                assert_eq!(decoded.answer, answer.answer);
                let expected = match (&answer.dependencies, with_dependencies) {
                    (_, false) | (Dependencies::None, _) => Dependencies::None,
                    (Dependencies::Shared(id), true) => Dependencies::Shared(*id),
                    (Dependencies::Inline(keys), true) => Dependencies::Inline(keys.clone()),
                };
                assert_eq!(decoded.dependencies, expected);
                assert!(decode_result(&key, &value[..value.len() - 1]).is_err());
            }
        }
    }
}
