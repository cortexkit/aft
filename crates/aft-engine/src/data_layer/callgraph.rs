//! Call-graph resolution over a checkout's content records, one file at a
//! time and in parallel.
//!
//! Resolution has three passes, each a function of the resolver's (see
//! `callgraph_store::join`):
//!
//! 1. **Bind** every parsed file against the checkout's paths: its module
//!    targets and the rest of its resolver surface. Each file is bound on its
//!    own, so the pass runs on every thread at once.
//! 2. **Resolve** every file's references against all bound surfaces, which
//!    the threads share read-only, recording each surface query with its
//!    answer and each path or configuration field consulted.
//! 3. **Dispatch targets**: for Rust, TypeScript and JavaScript files, the
//!    files their type names resolve to, for method-call resolution.
//! 4. **Method calls**: every file's method-call sites against every file's
//!    method-call facts, including the import targets other files' pass 2
//!    found and the type targets their pass 3 found. Each file records the
//!    dependency keys its resolutions read.
//!
//! The resolver's memos and recorders are single-threaded, so each thread
//! has its own; they are caches and recorders, never inputs, so results do
//! not depend on which thread resolves which file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use aft_store::checkout::{CheckoutState, PathKind};
use rayon::prelude::*;

use crate::callgraph_store::facts::{BlobKey, DirEntry, EntryKind, ProjectFacts};
use crate::callgraph_store::join::dispatch::{
    associated_reference, dependency, DispatchFiles, MemoizedResolver, Resolution, Resolver,
    SiteHint,
};
use crate::callgraph_store::join::{
    bind_file, dispatch_type_targets_for, finish_binding, resolve_file, BlobRef, BlobRefKind,
    DerivedRow, DispatchTypeTargets, ManifestJoinError, ParseBlob, ViewBindingDependencies,
    ViewBindingFacts,
};
use crate::callgraph_store::method_call_facts::FileFacts;
use crate::callgraph_store::{
    DbFileIndex, LoadedFileIndexes, LoadedIndex, ProjectIndex, WorkspaceCratePrefixCache,
};

mod binding_batch;
mod key_order;
mod site_records;
mod spool;

pub(crate) use key_order::Budget as KeyOrderBudget;

use super::checkout::resume;
use super::checkout::staged_records::{StagedAt, StagedRecords};
use super::graph_rows::references_by_key;
use super::graph_store::StoredSlices;
use super::incremental::UpdateError;
use super::refresh_memory::{RefreshChecks, RefreshMemory, RefreshStop};

/// What resolving a checkout reads.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct CheckoutInputs {
    /// The checkout's paths.
    pub(crate) state: CheckoutState,
    /// Every parsed file's record key and parse output, by path.
    pub parses: Parses,
    /// The raw bytes of every resolver configuration file (`package.json`,
    /// `Cargo.toml`, `go.mod`, `tsconfig*.json`, `.gitignore`), by path.
    pub(crate) configs: HashMap<Vec<u8>, Arc<[u8]>>,
}

impl CheckoutInputs {
    fn csharp_projects(&self) -> crate::callgraph_store::join::dispatch::CSharpProjects {
        crate::callgraph_store::join::dispatch::CSharpProjects::new(
            self.state.directory_records(b"").map(|record| {
                (
                    record.path.as_slice(),
                    self.configs
                        .get(&record.path)
                        .filter(|_| record.language == super::content::CSHARP_PROJECT_CONFIG)
                        .map(|markers| markers.as_ref()),
                )
            }),
        )
    }
}

/// Every parsed file's record key and parse output, by path, in path order.
///
/// Built in memory, every entry holds its parse. Read from a publication,
/// an entry is decoded the first time it is read. A read that fails is
/// recorded in the publication's failures and reads as an empty parse;
/// whoever reads through a publication fails on any recorded failure.
///
/// A cold build keeps each entry as its encoded content record and decodes
/// it the first time it is read; [`Self::release_decoded`] lets the decoded
/// parses go between the build's stages, so a build never holds every
/// file's parse decoded at once.
#[derive(Clone, Default)]
pub(crate) struct Parses {
    map: BTreeMap<String, Arc<ParseSlot>>,
}

pub(crate) struct ParseSlot {
    value: std::sync::OnceLock<(BlobKey, Arc<ParseBlob>)>,
    stored: Option<Arc<StoredSlices>>,
    /// The encoded content record the parse is decoded from, with its
    /// parse key.
    encoded: Option<(BlobKey, Arc<aft_store::content::EncodedRecord>)>,
    /// The staged content record the parse is decoded from: a cold build's
    /// records are on disk (see `staged_records`).
    staged: Option<StagedParse>,
}

/// A parse kept as a staged content record.
pub(crate) struct StagedParse {
    pub(crate) key: BlobKey,
    pub(crate) at: StagedAt,
    /// [`list_kinds`] of the parse, noted when it was extracted.
    pub(crate) kinds: (bool, bool),
    pub(crate) records: Arc<StagedRecords>,
}

impl ParseSlot {
    /// Whether the parse can be decoded again once let go.
    fn redecodable(&self) -> bool {
        self.encoded.is_some() || self.stored.is_some() || self.staged.is_some()
    }

    /// The parse, decoded from its encoded record or the publication; its
    /// kinds are noted in [`parse_kinds`] on the way.
    fn decode(&self, path: &str) -> (BlobKey, Arc<ParseBlob>) {
        let decoded = self.decode_only(path);
        note_parse_kinds(&decoded.0, &decoded.1);
        decoded
    }

    fn decode_only(&self, path: &str) -> (BlobKey, Arc<ParseBlob>) {
        if let Some((key, record)) = &self.encoded {
            // The build checked that the record converts before keeping it
            // encoded (see `Parses::encoded`), so a failure here is a defect
            // in the codec; it reads as an empty parse like a failed read.
            return record
                .decode()
                .ok()
                .and_then(|record| match super::content::blob_from_record(&record) {
                    Ok(crate::callgraph_store::join::CallgraphBlob::Parse(parse)) => Some(parse),
                    _ => None,
                })
                .map_or_else(empty_parse, |parse| (key.clone(), Arc::new(parse)));
        }
        if let Some(StagedParse {
            key, at, records, ..
        }) = &self.staged
        {
            // A read or decode that fails is recorded, and the build fails
            // on it at its next stage boundary; until then it reads as an
            // empty parse, as a failed stored read does.
            let Some(record) = records.encoded(*at, path) else {
                return empty_parse();
            };
            return match record
                .decode()
                .map_err(|error| error.to_string())
                .and_then(|record| {
                    super::content::blob_from_record(&record).map_err(|error| error.to_string())
                }) {
                Ok(crate::callgraph_store::join::CallgraphBlob::Parse(parse)) => {
                    (key.clone(), Arc::new(parse))
                }
                Ok(_) => {
                    records.note_failure(format!("the staged record of {path} is not a parse"));
                    empty_parse()
                }
                Err(error) => {
                    records.note_failure(format!("the staged record of {path}: {error}"));
                    empty_parse()
                }
            };
        }
        self.stored
            .as_ref()
            .and_then(|stored| stored.parse_of(path))
            .unwrap_or_else(empty_parse)
    }
}

/// Above this many parses the memo of [`parse_kinds`] starts over, so it
/// never holds more than a few megabytes; one large repository's parses
/// take a tenth of it.
const PARSE_KINDS_LIMIT: usize = 1 << 16;

/// [`FileLists::kinds`] of every parse this process decoded, by its parse
/// key: whether it is Rust and whether it declares modules. Both follow
/// from the parsed content alone, which the key names, so they hold for
/// every checkout and engine of the process. An engine started from a
/// publication lists the module-declaring files from here rather than
/// decoding every stored parse; builds and engines fill it as they decode.
fn parse_kinds() -> &'static Mutex<HashMap<u128, (bool, bool)>> {
    static KINDS: std::sync::OnceLock<Mutex<HashMap<u128, (bool, bool)>>> =
        std::sync::OnceLock::new();
    KINDS.get_or_init(Default::default)
}

/// A parse key as [`parse_kinds`] files it: the record key digest it
/// spells in hex. `None` for anything else, such as the empty key of a
/// parse whose read failed.
fn parse_kinds_key(key: &str) -> Option<u128> {
    (key.len() == 32)
        .then(|| u128::from_str_radix(key, 16).ok())
        .flatten()
}

fn note_parse_kinds(key: &str, parse: &ParseBlob) {
    let Some(key) = parse_kinds_key(key) else {
        return;
    };
    let kinds = FileLists::kinds(parse);
    let mut memo = parse_kinds()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if memo.len() >= PARSE_KINDS_LIMIT && !memo.contains_key(&key) {
        memo.clear();
    }
    memo.insert(key, kinds);
}

/// A parse with nothing in it, for an entry whose stored read failed.
fn empty_parse() -> (BlobKey, Arc<ParseBlob>) {
    (
        BlobKey::new(),
        Arc::new(ParseBlob {
            entry_point_symbols: Vec::new(),
            dispatch: Default::default(),
            extractor_version: String::new(),
            language: String::new(),
            ast_nodes: Vec::new(),
            symbols: Vec::new(),
            default_export_symbol: None,
            exported_symbols: Vec::new(),
            callable_symbols: Vec::new(),
            imports: Vec::new(),
            python_bindings: Vec::new(),
            refs: Vec::new(),
        }),
    )
}

impl Parses {
    /// Every file `stored` has a resolution for, each decoded on first read.
    pub(crate) fn stored(stored: &Arc<StoredSlices>) -> Self {
        Self {
            map: stored
                .parsed_files()
                .map(|(path, _)| {
                    (
                        path.clone(),
                        Arc::new(ParseSlot {
                            value: std::sync::OnceLock::new(),
                            stored: Some(Arc::clone(stored)),
                            encoded: None,
                            staged: None,
                        }),
                    )
                })
                .collect(),
        }
    }

    /// Entries kept as encoded content records, each decoded on first read.
    pub(crate) fn encoded(
        entries: impl IntoIterator<Item = (String, BlobKey, Arc<aft_store::content::EncodedRecord>)>,
    ) -> Self {
        Self {
            map: entries
                .into_iter()
                .map(|(path, key, record)| {
                    (
                        path,
                        Arc::new(ParseSlot {
                            value: std::sync::OnceLock::new(),
                            stored: None,
                            encoded: Some((key, record)),
                            staged: None,
                        }),
                    )
                })
                .collect(),
        }
    }

    /// Entries kept as staged content records in `records`, each decoded on
    /// every read that does not find it decoded (see
    /// [`Self::release_decoded`]).
    pub(crate) fn staged(entries: impl IntoIterator<Item = (String, StagedParse)>) -> Self {
        Self {
            map: entries
                .into_iter()
                .map(|(path, staged)| {
                    (
                        path,
                        Arc::new(ParseSlot {
                            value: std::sync::OnceLock::new(),
                            stored: None,
                            encoded: None,
                            staged: Some(staged),
                        }),
                    )
                })
                .collect(),
        }
    }

    /// The staged records these parses read, when they are a cold build's.
    pub(crate) fn staged_records(&self) -> Option<&Arc<StagedRecords>> {
        self.map
            .values()
            .find_map(|slot| slot.staged.as_ref().map(|staged| &staged.records))
    }

    /// [`list_kinds`] of every parse, in path order, when each was noted as
    /// it was staged; `None` when any must be decoded to tell.
    fn staged_kinds(&self) -> Option<Vec<(bool, bool)>> {
        self.map
            .values()
            .map(|slot| slot.staged.as_ref().map(|staged| staged.kinds))
            .collect()
    }

    /// How many entries that can be decoded again are decoded now.
    pub(crate) fn decoded_count(&self) -> usize {
        self.map
            .values()
            .filter(|slot| slot.redecodable() && slot.value.get().is_some())
            .count()
    }

    /// Lets go of every decoded parse that can be decoded again, from its
    /// encoded record or the publication. An entry another copy of these
    /// parses shares stays decoded.
    pub(crate) fn release_decoded(&mut self) {
        for slot in self.map.values_mut() {
            if let Some(slot) = Arc::get_mut(slot) {
                if slot.redecodable() {
                    slot.value.take();
                }
            }
        }
    }

    /// These parses read from `stored` instead, each one decoded now kept
    /// decoded: `None` unless `stored` holds a parse of exactly these files,
    /// each under the key it has here. A parse key names the parsed content,
    /// so a parse decoded under a key is the one `stored` holds under it.
    /// For moving an engine onto the publication of what it resolved.
    pub(crate) fn rebased(&self, stored: &Arc<StoredSlices>) -> Option<Self> {
        let mut held = stored.parsed_files();
        let mut map = BTreeMap::new();
        for (path, slot) in &self.map {
            let (held_path, held_key) = held.next()?;
            if held_path != path || self.known_key(path)? != held_key {
                return None;
            }
            let value = std::sync::OnceLock::new();
            if let Some(decoded) = slot.value.get() {
                let _ = value.set(decoded.clone());
            }
            map.insert(
                path.clone(),
                Arc::new(ParseSlot {
                    value,
                    stored: Some(Arc::clone(stored)),
                    encoded: None,
                    staged: None,
                }),
            );
        }
        held.next().is_none().then_some(Self { map })
    }

    pub(crate) fn insert(&mut self, path: String, value: (BlobKey, Arc<ParseBlob>)) {
        let slot = ParseSlot {
            value: std::sync::OnceLock::from(value),
            stored: None,
            encoded: None,
            staged: None,
        };
        self.map.insert(path, Arc::new(slot));
    }

    pub(crate) fn remove(&mut self, path: &str) {
        self.map.remove(path);
    }

    pub(crate) fn get(&self, path: &str) -> Option<&(BlobKey, Arc<ParseBlob>)> {
        let slot = self.map.get(path)?;
        Some(slot.value.get_or_init(|| slot.decode(path)))
    }

    /// `path`'s record key and parse without keeping the parse: the one
    /// held when it is decoded already, otherwise one decoded now that only
    /// the caller holds. For work that reads a file once, such as binding
    /// it or listing its kinds: [`Self::get`] would keep every parse it
    /// read decoded until [`Self::release_decoded`], and an update that
    /// binds or lists most of a large checkout held over half a gigabyte of
    /// parses that way.
    pub(crate) fn read(&self, path: &str) -> Option<(BlobKey, Arc<ParseBlob>)> {
        let slot = self.map.get(path)?;
        Some(match slot.value.get() {
            Some((key, parse)) => (key.clone(), Arc::clone(parse)),
            None => slot.decode(path),
        })
    }

    /// `path`'s parse key, without decoding the parse when the key is known
    /// without it.
    pub(crate) fn key(&self, path: &str) -> Option<BlobKey> {
        self.known_key(path)
            .or_else(|| self.get(path).map(|(key, _)| key.clone()))
    }

    /// `path`'s parse key when it is known without decoding the parse.
    fn known_key(&self, path: &str) -> Option<BlobKey> {
        let slot = self.map.get(path)?;
        if let Some((key, _)) = slot.value.get() {
            return Some(key.clone());
        }
        if let Some((key, _)) = &slot.encoded {
            return Some(key.clone());
        }
        if let Some(staged) = &slot.staged {
            return Some(staged.key.clone());
        }
        slot.stored
            .as_ref()
            .and_then(|stored| stored.record_key_of(path))
            .map(str::to_string)
    }

    pub(crate) fn contains_key(&self, path: &str) -> bool {
        self.map.contains_key(path)
    }

    #[doc(hidden)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Every path, in order, without reading any parse.
    pub(crate) fn keys(&self) -> std::collections::btree_map::Keys<'_, String, Arc<ParseSlot>> {
        self.map.keys()
    }

    /// Every path at or after `start`, in order, without reading any parse.
    pub(crate) fn keys_from<'p>(&'p self, start: &str) -> impl Iterator<Item = &'p String> {
        self.map
            .range::<str, _>((std::ops::Bound::Included(start), std::ops::Bound::Unbounded))
            .map(|(path, _)| path)
    }

    /// Every entry, reading each parse.
    #[doc(hidden)]
    pub fn iter(&self) -> ParsesIter<'_> {
        ParsesIter {
            parses: self,
            keys: self.map.keys(),
        }
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &(BlobKey, Arc<ParseBlob>)> {
        self.iter().map(|(_, value)| value)
    }

    /// Every entry in parallel, reading each parse.
    pub(crate) fn par_iter(
        &self,
    ) -> impl ParallelIterator<Item = (&String, &(BlobKey, Arc<ParseBlob>))> {
        let carry = crate::alloc_trace::carry();
        self.map
            .keys()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(move |path| {
                let _label = carry.enter();
                (path, self.get(path).expect("listed"))
            })
    }
}

/// [`Parses::iter`].
pub(crate) struct ParsesIter<'p> {
    parses: &'p Parses,
    keys: std::collections::btree_map::Keys<'p, String, Arc<ParseSlot>>,
}

impl<'p> Iterator for ParsesIter<'p> {
    type Item = (&'p String, &'p (BlobKey, Arc<ParseBlob>));

    fn next(&mut self) -> Option<Self::Item> {
        let path = self.keys.next()?;
        Some((path, self.parses.get(path).expect("listed")))
    }
}

impl<'p> IntoIterator for &'p Parses {
    type Item = (&'p String, &'p (BlobKey, Arc<ParseBlob>));
    type IntoIter = ParsesIter<'p>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::fmt::Debug for Parses {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Parses({} files)", self.map.len())
    }
}

impl std::ops::Index<&str> for Parses {
    type Output = (BlobKey, Arc<ParseBlob>);

    fn index(&self, path: &str) -> &Self::Output {
        self.get(path).expect("a parsed file")
    }
}

impl std::ops::Index<&String> for Parses {
    type Output = (BlobKey, Arc<ParseBlob>);

    fn index(&self, path: &String) -> &Self::Output {
        &self[path.as_str()]
    }
}

impl FromIterator<(String, (BlobKey, Arc<ParseBlob>))> for Parses {
    fn from_iter<I: IntoIterator<Item = (String, (BlobKey, Arc<ParseBlob>))>>(entries: I) -> Self {
        let mut parses = Self::default();
        for (path, value) in entries {
            parses.insert(path, value);
        }
        parses
    }
}

impl From<BTreeMap<String, (BlobKey, Arc<ParseBlob>)>> for Parses {
    fn from(map: BTreeMap<String, (BlobKey, Arc<ParseBlob>)>) -> Self {
        map.into_iter().collect()
    }
}

/// Reads one file's resolution: from a map built in memory, or from
/// [`Resolutions`].
pub(crate) trait ResolutionLookup {
    fn resolution(&self, path: &str) -> Option<&FileResolution>;

    /// `path`'s resolution for a caller that reads it once: the one
    /// [`Self::resolution`] gives, unless the lookup can decode it without
    /// keeping it ([`Resolutions::read`]).
    fn read_once(&self, path: &str) -> Option<ReadOnce<'_>> {
        self.resolution(path).map(ReadOnce::Held)
    }
}

/// A resolution [`ResolutionLookup::read_once`] gives: one the lookup
/// holds, or one decoded for the caller alone.
pub(crate) enum ReadOnce<'r> {
    Held(&'r FileResolution),
    Decoded(Arc<FileResolution>),
}

impl std::ops::Deref for ReadOnce<'_> {
    type Target = FileResolution;

    fn deref(&self) -> &FileResolution {
        match self {
            Self::Held(resolution) => resolution,
            Self::Decoded(resolution) => resolution,
        }
    }
}

impl ResolutionLookup for BTreeMap<String, FileResolution> {
    fn resolution(&self, path: &str) -> Option<&FileResolution> {
        self.get(path)
    }
}

/// Every parsed file's resolution, by path, in path order.
///
/// Built in memory, every entry holds its resolution. Read from a
/// publication, an entry is decoded the first time it is read; a read that
/// fails is recorded in the publication's failures and the entry reads as
/// absent, and whoever reads through a publication fails on any recorded
/// failure.
///
/// An entry an update resolves again or changes ([`Self::insert`],
/// [`Self::get_mut`]) is held decoded while the update runs.
/// [`Self::encode_changed`] then encodes it, in the fragment encoding a
/// publication writes, so that, like an entry read from the publication, it
/// can be let go ([`Self::release_decoded`]) and decoded again when next
/// read. Decoded, a resolution takes many times its encoded size, since
/// every reference row owns its strings; one broad edit re-resolves over a
/// thousand files.
///
/// Each decoded resolution sits behind an `Arc`, so letting it go never
/// copies it.
#[derive(Default)]
pub(crate) struct Resolutions {
    map: BTreeMap<String, ResolutionSlot>,
    /// Entries inserted or changed since the last [`Self::encode_changed`]:
    /// decoded, and not encoded yet.
    changed: BTreeSet<String>,
}

struct ResolutionSlot {
    value: std::sync::OnceLock<Option<Arc<FileResolution>>>,
    /// What `value` is decoded from again once it is let go.
    source: ResolutionSource,
}

/// Where an entry of [`Resolutions`] is decoded from.
enum ResolutionSource {
    /// Nowhere: the entry is built in memory, or changed and not encoded
    /// yet, so its decoded value is all there is and is never let go.
    Memory,
    /// The publication the entry was read from, unchanged since.
    Stored(Arc<StoredSlices>),
    /// The entry's encoding, made after an update changed it.
    Encoded(Arc<EncodedResolution>),
}

impl ResolutionSlot {
    fn read(&self, path: &str) -> Option<&FileResolution> {
        self.value.get_or_init(|| self.decode(path)).as_deref()
    }

    /// The value decoded from its source, without keeping it.
    fn decode(&self, path: &str) -> Option<Arc<FileResolution>> {
        match &self.source {
            // A slot built in memory is made with its value, and never
            // lets it go, so this is never reached.
            ResolutionSource::Memory => None,
            ResolutionSource::Stored(stored) => stored.resolution_of(path).map(Arc::new),
            ResolutionSource::Encoded(encoded) => Some(Arc::new(encoded.decode(path))),
        }
    }

    /// Whether the value can be let go and decoded again.
    fn releasable(&self) -> bool {
        !matches!(self.source, ResolutionSource::Memory)
    }
}

/// One file's resolution kept encoded in memory, in the fragment encoding
/// a publication writes ([`super::fragment`]), with the parse key its rows
/// are decoded with.
#[derive(Debug)]
pub(crate) struct EncodedResolution {
    record_key: Box<str>,
    bytes: Box<[u8]>,
}

impl EncodedResolution {
    /// Encodes `resolution`, a resolution of the file whose parse key is
    /// `record_key`.
    #[must_use]
    pub(crate) fn encode(record_key: &str, resolution: &FileResolution) -> Self {
        Self::encode_checked(record_key, resolution, None)
            .expect("an unguarded resolution cannot stop")
    }

    fn encode_checked(
        record_key: &str,
        resolution: &FileResolution,
        guard: Option<&RefreshMemory>,
    ) -> Result<Self, RefreshStop> {
        debug_assert!(
            resolution
                .rows
                .iter()
                .all(|row| row.caller_blob_key == record_key),
            "a resolution's rows carry its parse key"
        );
        Ok(Self {
            record_key: record_key.into(),
            bytes: super::fragment::encode_checked(resolution, guard)?.into_boxed_slice(),
        })
    }

    /// The resolution of the file at `path` this encodes.
    ///
    /// # Panics
    ///
    /// When the bytes do not decode. This process encoded them, so that is
    /// a defect in the fragment codec, which its tests check.
    #[must_use]
    pub(crate) fn decode(&self, path: &str) -> FileResolution {
        super::fragment::decode(&self.record_key, path, &self.bytes).unwrap_or_else(|error| {
            panic!(
                "{path}: a resolution encoded in memory does not decode: {}",
                error.0
            )
        })
    }

    /// The parse key of the file whose resolution this encodes.
    #[must_use]
    pub(crate) fn record_key(&self) -> &str {
        &self.record_key
    }

    /// The encoded bytes: exactly what [`super::fragment::encode`] writes for
    /// this resolution.
    #[must_use]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Resolutions {
    #[cfg(feature = "test-hooks")]
    pub(crate) fn profile_parts(&self) -> [usize; 5] {
        use aft_store::heap::HeapBytes as _;
        let mut seen = aft_store::heap::Seen::new();
        let mut parts = [0; 5];
        for slot in self.map.values() {
            if let Some(Some(file)) = slot.value.get() {
                parts[0] += file.rows.heap_bytes(&mut seen);
                parts[1] += file.binding.heap_bytes(&mut seen);
                parts[2] += file.dispatch.heap_bytes(&mut seen);
                parts[3] += file.sites.heap_bytes(&mut seen);
                parts[4] += file.site_dependencies.heap_bytes(&mut seen);
            }
        }
        parts
    }

    /// Every file `stored` has a resolution for, each decoded on first read.
    pub(crate) fn stored(stored: &Arc<StoredSlices>) -> Self {
        Self {
            map: stored
                .parsed_files()
                .map(|(path, _)| {
                    (
                        path.clone(),
                        ResolutionSlot {
                            value: std::sync::OnceLock::new(),
                            source: ResolutionSource::Stored(Arc::clone(stored)),
                        },
                    )
                })
                .collect(),
            changed: BTreeSet::new(),
        }
    }

    pub(crate) fn get(&self, path: &str) -> Option<&FileResolution> {
        self.map.get(path)?.read(path)
    }

    /// `path`'s resolution without keeping it: the one held when it is
    /// decoded already, otherwise one decoded now that only the caller
    /// holds. For checking a file an update most often leaves as it is,
    /// such as replaying its recorded answers: [`Self::get`] would keep
    /// each one decoded until the update ends, and a change to a widely
    /// read file has its readers' resolutions checked by the thousand.
    pub(crate) fn read(&self, path: &str) -> Option<Arc<FileResolution>> {
        let slot = self.map.get(path)?;
        match slot.value.get() {
            Some(value) => value.clone(),
            None => slot.decode(path),
        }
    }

    /// The encoding of `path`'s resolution, whose parse key is
    /// `record_key`: the one [`Self::encode_changed`] kept, shared, or for
    /// an entry that has none, its value encoded now.
    pub(crate) fn encoded(&self, path: &str, record_key: &str) -> Option<Arc<EncodedResolution>> {
        let slot = self.map.get(path)?;
        if let ResolutionSource::Encoded(encoded) = &slot.source {
            return Some(Arc::clone(encoded));
        }
        let resolution = slot.read(path)?;
        Some(Arc::new(EncodedResolution::encode(record_key, resolution)))
    }

    pub(crate) fn get_mut(&mut self, path: &str) -> Option<&mut FileResolution> {
        let Self { map, changed } = self;
        let slot = map.get_mut(path)?;
        slot.read(path);
        // Changed here, the entry no longer matches what it was decoded
        // from, so it stays decoded until `encode_changed` encodes it.
        slot.source = ResolutionSource::Memory;
        if !changed.contains(path) {
            changed.insert(path.to_string());
        }
        slot.value.get_mut()?.as_mut().map(Arc::make_mut)
    }

    /// Encodes every entry inserted or changed since the last call, reading
    /// each file's parse key from `record_key`, so that
    /// [`Self::release_decoded`] can let it go. Runs on the current rayon
    /// pool.
    pub(super) fn encode_changed(
        &mut self,
        record_key: impl Fn(&str) -> Option<String> + Sync,
        guard: Option<&RefreshMemory>,
    ) -> Result<(), RefreshStop> {
        let changed = std::mem::take(&mut self.changed);
        let map = &self.map;
        let carry = crate::alloc_trace::carry();
        let encoded = changed
            .par_iter()
            .map(|path| {
                guard.file()?;
                let _label = carry.enter();
                let Some(resolution) = map.get(path).and_then(|slot| slot.value.get()?.as_deref())
                else {
                    return Ok(None);
                };
                let Some(key) = record_key(path) else {
                    return Ok(None);
                };
                Ok(Some((
                    path,
                    EncodedResolution::encode_checked(&key, resolution, guard)?,
                )))
            })
            .collect::<Result<Vec<_>, RefreshStop>>()?;
        for (path, encoded) in encoded.into_iter().flatten() {
            guard.file()?;
            if let Some(slot) = self.map.get_mut(path) {
                slot.source = ResolutionSource::Encoded(Arc::new(encoded));
            }
        }
        Ok(())
    }

    /// How many entries that can be decoded again, from the publication or
    /// their encoding, are decoded now.
    pub(crate) fn decoded_count(&self) -> usize {
        self.map
            .values()
            .filter(|slot| slot.releasable() && slot.value.get().is_some())
            .count()
    }

    /// Lets go of every decoded resolution that can be decoded again, from
    /// the publication or its encoding; the next read decodes it again. An
    /// entry built in memory, or changed and not encoded yet, stays.
    pub(crate) fn release_decoded(&mut self) {
        for slot in self.map.values_mut() {
            if slot.releasable() {
                slot.value.take();
            }
        }
    }

    /// Every entry read from `stored` instead, none decoded: `None` unless
    /// `stored` holds a resolution of exactly these files and no entry is
    /// changed and not encoded yet. Whether `stored` holds the same
    /// resolutions is the caller's to know: the publication of what these
    /// hold. A decoded one is not carried over, so every entry is read from
    /// `stored` alone.
    pub(crate) fn rebased(&self, stored: &Arc<StoredSlices>) -> Option<Self> {
        if !self.changed.is_empty() {
            return None;
        }
        let mut held = stored.parsed_files();
        let mut map = BTreeMap::new();
        for path in self.map.keys() {
            let (held_path, _) = held.next()?;
            if held_path != path {
                return None;
            }
            map.insert(
                path.clone(),
                ResolutionSlot {
                    value: std::sync::OnceLock::new(),
                    source: ResolutionSource::Stored(Arc::clone(stored)),
                },
            );
        }
        held.next().is_none().then_some(Self {
            map,
            changed: BTreeSet::new(),
        })
    }

    pub(crate) fn insert(&mut self, path: String, resolution: FileResolution) {
        if !self.changed.contains(&path) {
            self.changed.insert(path.clone());
        }
        self.map.insert(
            path,
            ResolutionSlot {
                value: std::sync::OnceLock::from(Some(Arc::new(resolution))),
                source: ResolutionSource::Memory,
            },
        );
    }

    /// How many files have a resolution slot, decoded or not.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn remove(&mut self, path: &str) -> Option<FileResolution> {
        self.changed.remove(path);
        let slot = self.map.remove(path)?;
        slot.read(path);
        let resolution = slot.value.into_inner().flatten()?;
        Some(Arc::try_unwrap(resolution).unwrap_or_else(|shared| (*shared).clone()))
    }

    pub(crate) fn contains_key(&self, path: &str) -> bool {
        self.map.contains_key(path)
    }

    /// Every path, in order, without decoding anything.
    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }

    /// Every entry, decoding each; one whose read failed is left out.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &FileResolution)> {
        self.map
            .iter()
            .filter_map(|(path, slot)| slot.read(path).map(|resolution| (path, resolution)))
    }

    /// Every entry, decoded and copied, for comparing with a cold build.
    pub(crate) fn to_map(&self) -> BTreeMap<String, FileResolution> {
        self.iter()
            .map(|(path, resolution)| (path.clone(), resolution.clone()))
            .collect()
    }
}

impl ResolutionLookup for Resolutions {
    fn resolution(&self, path: &str) -> Option<&FileResolution> {
        self.get(path)
    }

    fn read_once(&self, path: &str) -> Option<ReadOnce<'_>> {
        self.read(path).map(ReadOnce::Decoded)
    }
}

impl std::ops::Index<&str> for Resolutions {
    type Output = FileResolution;

    fn index(&self, path: &str) -> &FileResolution {
        self.get(path).expect("a resolved file")
    }
}

/// Entries built in memory, each held decoded for as long as it lasts: a
/// cold build's resolutions, which nothing encodes.
impl From<BTreeMap<String, FileResolution>> for Resolutions {
    fn from(map: BTreeMap<String, FileResolution>) -> Self {
        Self {
            map: map
                .into_iter()
                .map(|(path, resolution)| {
                    (
                        path,
                        ResolutionSlot {
                            value: std::sync::OnceLock::from(Some(Arc::new(resolution))),
                            source: ResolutionSource::Memory,
                        },
                    )
                })
                .collect(),
            changed: BTreeSet::new(),
        }
    }
}

/// One method-call site's resolution, as pass 4 keeps it. Sites with the
/// same answer share it: an unknown receiver's answer can name thousands of
/// methods, and every site calling that name gets it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiteResolution {
    pub(crate) site: SiteHint,
    pub(crate) resolution: Arc<Resolution>,
}

/// One file's resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct FileResolution {
    /// One row per reference.
    pub(crate) rows: Vec<DerivedRow>,
    /// What binding and resolution consulted, and the surface queries
    /// resolution asked with their answers.
    pub(crate) binding: ViewBindingDependencies,
    /// For Rust, TypeScript and JavaScript files, the targets of their type
    /// names.
    pub(crate) dispatch: Option<DispatchTypeTargets>,
    /// Pass 4: the method-call sites kept, in site order.
    pub(crate) sites: Vec<SiteResolution>,
    /// Pass 4: the dependency keys its site resolutions read.
    pub(crate) site_dependencies: BTreeSet<String>,
}

/// One file's pass-1 results: its bound surface, and its binding before
/// resolution.
#[derive(Debug)]
pub(crate) struct BoundFile {
    pub(crate) surface: Arc<DbFileIndex>,
    pub(crate) binding: ViewBindingDependencies,
}

/// The files some work read one at a time on a resolving thread, the first
/// time it read each, recorded in tests only: they check that an update did
/// that work beforehand, in parallel. Outside tests it holds and records
/// nothing.
#[derive(Debug, Default)]
pub(crate) struct FirstReads {
    #[cfg(test)]
    files: Mutex<Vec<String>>,
}

impl FirstReads {
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn note(&self, path: &str) {
        #[cfg(test)]
        self.files
            .lock()
            .expect("no panics while holding it")
            .push(path.to_string());
        #[cfg(not(test))]
        let _ = path;
    }

    #[cfg(test)]
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.files.lock().expect("no panics while holding it"))
    }
}

aft_store::heap_bytes_none!(FirstReads);

/// Lists over every parsed file that the resolver's whole-checkout lookups
/// read, each in path order.
#[derive(Clone, Debug, Default)]
struct FileLists {
    rust: Vec<String>,
    module_declaring: Vec<String>,
}

/// Whether `parse` is a Rust file, and whether it declares modules: as the
/// store's `module_declaring_files`, a module reference with a module path.
pub(crate) fn list_kinds(parse: &ParseBlob) -> (bool, bool) {
    FileLists::kinds(parse)
}

impl FileLists {
    /// See [`list_kinds`].
    fn kinds(parse: &ParseBlob) -> (bool, bool) {
        (
            parse.language == "rust",
            parse.refs.iter().any(|reference| {
                reference.kind == BlobRefKind::Module && reference.module_path.is_some()
            }),
        )
    }

    /// Puts `path` in `list`, kept in path order, or takes it out.
    fn place(list: &mut Vec<String>, path: &String, member: bool) {
        match (list.binary_search(path), member) {
            (Ok(_), true) | (Err(_), false) => {}
            (Ok(index), false) => {
                list.remove(index);
            }
            (Err(index), true) => list.insert(index, path.clone()),
        }
    }
}

/// The files of one checkout bound so far, shared by every resolver
/// thread. A file is bound when resolution first reaches it, or when an
/// update binds it again; an entry stays valid until an update binds the
/// file again or removes it, since a file whose binding can have changed is
/// always bound again (see `incremental`).
///
/// A cold build's cache is bounded ([`Self::bounded`]): past its byte limit
/// it lets go of the files least recently used, and a file read again is
/// bound again, which gives the same binding. The live engine's cache keeps
/// every file, since an update diffs a file's new binding against its old
/// one.
#[derive(Debug, Default)]
pub(crate) struct BindCache {
    files: Mutex<BoundSet>,
    lists: Mutex<Option<Arc<FileLists>>>,
    /// Every file a resolver bound on first use: what resolution waited on
    /// that nothing bound beforehand.
    bound_on_first_use: FirstReads,
    /// Each declared Rust module's parent over every module-declaring file,
    /// once a cold build has found them all (see [`Self::index_module_parents`]).
    module_parents: Mutex<Option<Arc<crate::callgraph_store::ModuleParents>>>,
    /// Whether the cache is bounded, readable without its lock.
    bounded: bool,
}

/// The bound files a [`BindCache`] holds, and what bounds them.
#[derive(Debug, Default)]
struct BoundSet {
    map: HashMap<String, BoundEntry>,
    /// The byte limit of a bounded cache, and its files in the order a
    /// second-chance sweep visits them; empty when unbounded.
    limit: Option<usize>,
    order: std::collections::VecDeque<String>,
    /// The bytes the files held count for, when bounded.
    held: usize,
    /// How many files were bound and how many let go, when bounded.
    bound: usize,
    evicted: usize,
}

#[derive(Debug)]
struct BoundEntry {
    bound: Arc<BoundFile>,
    /// The file's heap bytes, counted when bounded.
    bytes: usize,
    /// Read since the sweep last passed it.
    used: std::sync::atomic::AtomicBool,
}

impl BoundSet {
    fn insert(&mut self, path: String, bound: Arc<BoundFile>) {
        let bytes = match self.limit {
            Some(_) => {
                use aft_store::heap::HeapBytes as _;
                bound.heap_bytes(&mut aft_store::heap::Seen::new()) + path.len()
            }
            None => 0,
        };
        let entry = BoundEntry {
            bound,
            bytes,
            used: std::sync::atomic::AtomicBool::new(false),
        };
        if let Some(old) = self.map.insert(path.clone(), entry) {
            self.held -= old.bytes;
        } else if self.limit.is_some() {
            self.order.push_back(path);
        }
        self.held += bytes;
        if let Some(limit) = self.limit {
            self.bound += 1;
            self.evict_to(limit);
        }
    }

    /// Lets go of files until those held count for at most `limit` bytes,
    /// or one file is left. A file read since the sweep last passed it gets
    /// a second chance.
    fn evict_to(&mut self, limit: usize) {
        while self.held > limit && self.map.len() > 1 {
            let Some(path) = self.order.pop_front() else {
                break;
            };
            let Some(entry) = self.map.get(&path) else {
                continue;
            };
            if entry.used.swap(false, std::sync::atomic::Ordering::Relaxed) {
                self.order.push_back(path);
                continue;
            }
            let entry = self.map.remove(&path).expect("found above");
            self.held -= entry.bytes;
            self.evicted += 1;
        }
    }

    fn remove(&mut self, path: &str) {
        if let Some(entry) = self.map.remove(path) {
            self.held -= entry.bytes;
        }
    }
}

impl aft_store::heap::HeapBytes for BoundSet {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.map
            .iter()
            .map(|(path, entry)| {
                path.heap_bytes(seen)
                    + std::mem::size_of::<(String, BoundEntry)>()
                    + entry.bound.heap_bytes(seen)
            })
            .sum::<usize>()
            + self.order.capacity() * std::mem::size_of::<String>()
            + self
                .order
                .iter()
                .map(|path| path.heap_bytes(seen))
                .sum::<usize>()
    }
}

/// What a bounded [`BindCache`] did: how many files it bound and let go,
/// and the bytes its files count for now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BindCacheCounts {
    pub(crate) bound: usize,
    pub(crate) evicted: usize,
    pub(crate) held: usize,
}

impl BindCache {
    /// A cold build's cache, holding bound files that count for at most
    /// `bytes` (and always the last one bound).
    pub(crate) fn bounded(bytes: usize) -> Self {
        Self {
            files: Mutex::new(BoundSet {
                limit: Some(bytes),
                ..BoundSet::default()
            }),
            bounded: true,
            ..Self::default()
        }
    }

    fn is_bounded(&self) -> bool {
        self.bounded
    }

    /// The bounded cache's counts so far.
    pub(crate) fn counts(&self) -> BindCacheCounts {
        let files = self.files.lock().expect("no panics while holding it");
        BindCacheCounts {
            bound: files.bound,
            evicted: files.evicted,
            held: files.held,
        }
    }

    pub(crate) fn get(&self, path: &str) -> Option<Arc<BoundFile>> {
        let files = self.files.lock().expect("no panics while holding it");
        let entry = files.map.get(path)?;
        entry.used.store(true, std::sync::atomic::Ordering::Relaxed);
        Some(Arc::clone(&entry.bound))
    }

    /// Records `bound` as `path`'s binding, replacing any earlier one.
    pub(crate) fn set(&self, path: String, bound: BoundFile) -> Arc<BoundFile> {
        let bound = Arc::new(bound);
        self.files
            .lock()
            .expect("no panics while holding it")
            .insert(path, Arc::clone(&bound));
        bound
    }

    /// Records `bound` unless another thread bound `path` first; the two are
    /// equal, since binding reads only the file and the checkout.
    fn set_new(&self, path: &str, bound: BoundFile) -> Arc<BoundFile> {
        let mut files = self.files.lock().expect("no panics while holding it");
        if let Some(entry) = files.map.get(path) {
            return Arc::clone(&entry.bound);
        }
        let bound = Arc::new(bound);
        files.insert(path.to_string(), Arc::clone(&bound));
        bound
    }

    pub(crate) fn remove(&self, path: &str) {
        self.files
            .lock()
            .expect("no panics while holding it")
            .remove(path);
    }

    /// Binds every module-declaring file of the lists, in batches, and
    /// keeps each declared module's parent over all of them, so a resolver
    /// reading through a bounded cache finds a parent without binding every
    /// module-declaring file again (see `ProjectIndex::module_parent`). The
    /// parents are what that scan finds: the smallest `(declaring file,
    /// module name)` pair for each target.
    ///
    /// # Errors
    ///
    /// When binding fails on a file.
    fn index_module_parents(&self, inputs: &CheckoutInputs) -> Result<usize, ManifestJoinError> {
        let lists = self.lists(inputs);
        let mut parents = crate::callgraph_store::ModuleParents::new();
        for batch in lists.module_declaring.chunks(BATCH) {
            let missing = batch
                .iter()
                .filter(|path| self.get(path).is_none())
                .collect::<Vec<_>>();
            let mut bound = bind_all(inputs, &missing)?;
            for path in batch {
                let surface = match bound.remove(path) {
                    Some(fresh) => self.set_new(path, fresh).surface.clone(),
                    None => match self.get(path) {
                        Some(held) => held.surface.clone(),
                        None => continue,
                    },
                };
                crate::callgraph_store::note_module_parents(&mut parents, path, &surface);
            }
        }
        let count = parents.len();
        *self
            .module_parents
            .lock()
            .expect("no panics while holding it") = Some(Arc::new(parents));
        Ok(count)
    }

    /// Sets the whole-checkout lists, built by a caller that read every
    /// parse anyway, so resolution does not read them all again.
    fn set_lists(&self, lists: FileLists) {
        *self.lists.lock().expect("no panics while holding it") = Some(Arc::new(lists));
    }

    /// Updates the whole-checkout lists, once built, for `paths`, whose
    /// parse in `inputs` changed, appeared or went away. Reading every parse
    /// to build them again would decode every stored one.
    pub(crate) fn update_lists<'p>(
        &self,
        inputs: &CheckoutInputs,
        paths: impl Iterator<Item = &'p String>,
    ) {
        let mut slot = self.lists.lock().expect("no panics while holding it");
        let Some(current) = slot.as_ref() else {
            return;
        };
        let mut lists = FileLists::clone(current);
        for path in paths {
            let (rust, declaring) = inputs
                .parses
                .get(path)
                .map_or((false, false), |(_, parse)| FileLists::kinds(parse));
            FileLists::place(&mut lists.rust, path, rust);
            FileLists::place(&mut lists.module_declaring, path, declaring);
        }
        *slot = Some(Arc::new(lists));
    }

    fn lists(&self, inputs: &CheckoutInputs) -> Arc<FileLists> {
        let mut slot = self.lists.lock().expect("no panics while holding it");
        Arc::clone(slot.get_or_insert_with(|| {
            let mut lists = FileLists::default();
            for (path, (_, parse)) in &inputs.parses {
                let (rust, declaring) = FileLists::kinds(parse);
                if rust {
                    lists.rust.push(path.clone());
                }
                if declaring {
                    lists.module_declaring.push(path.clone());
                }
            }
            Arc::new(lists)
        }))
    }

    /// Binds, in parallel, every file that declares Rust modules and is not
    /// bound yet, building the whole-checkout lists first, also in parallel,
    /// if no resolution has built them.
    ///
    /// Resolving a Rust file asks which file declares each of its modules.
    /// The resolver answers from a map it builds the first time it is asked,
    /// by binding every module-declaring file one after another on the
    /// thread that asked (see `ProjectIndex::lazy_module_parents`). An engine
    /// started from a publication has bound none of them, so without this
    /// its first Rust update waits on that serial pass, seconds on a large
    /// checkout. Binding reads only the file and the checkout, so these are
    /// the bindings the resolver would have made.
    ///
    /// # Errors
    ///
    /// When binding fails on a file.
    pub(super) fn bind_module_declaring_checked(
        &self,
        inputs: &CheckoutInputs,
        guard: Option<&RefreshMemory>,
    ) -> Result<(), UpdateError> {
        let lists = self.lists_in_parallel(inputs, guard)?;
        let paths = lists.module_declaring.iter().collect::<Vec<_>>();
        self.bind_missing_checked(inputs, &paths, guard)
    }

    /// [`Self::lists`], built in parallel when no resolution has built them.
    /// Each file's kinds come from [`parse_kinds`] when this process has
    /// decoded its parse before (a build of any checkout of the repository
    /// decodes every one); only the others are read, each without keeping
    /// it ([`Parses::read`]). An engine started from a publication has no
    /// parse decoded, and keeping every one held over 600 MB on AFT's own
    /// checkout. The lock is not held while they are read: a pool thread
    /// waiting on it could be the one a read needs.
    fn lists_in_parallel(
        &self,
        inputs: &CheckoutInputs,
        guard: Option<&RefreshMemory>,
    ) -> Result<Arc<FileLists>, RefreshStop> {
        if let Some(lists) = self
            .lists
            .lock()
            .expect("no panics while holding it")
            .as_ref()
        {
            return Ok(Arc::clone(lists));
        }
        let paths = inputs.parses.keys().collect::<Vec<_>>();
        let known = {
            let memo = parse_kinds()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            paths
                .iter()
                .map(|path| {
                    let key = inputs.parses.known_key(path)?;
                    memo.get(&parse_kinds_key(&key)?).copied()
                })
                .collect::<Vec<_>>()
        };
        let carry = crate::alloc_trace::carry();
        let kinds = paths
            .par_iter()
            .zip(known)
            .map(|(path, known)| {
                guard.file()?;
                let _label = carry.enter();
                Ok(known.unwrap_or_else(|| {
                    inputs
                        .parses
                        .read(path)
                        .map_or((false, false), |(_, parse)| FileLists::kinds(&parse))
                }))
            })
            .collect::<Result<Vec<_>, RefreshStop>>()?;
        let mut lists = FileLists::default();
        for (path, (rust, declaring)) in paths.into_iter().zip(kinds) {
            guard.file()?;
            if rust {
                lists.rust.push(path.clone());
            }
            if declaring {
                lists.module_declaring.push(path.clone());
            }
        }
        let mut slot = self.lists.lock().expect("no panics while holding it");
        Ok(Arc::clone(slot.get_or_insert_with(|| Arc::new(lists))))
    }

    /// The files resolution bound on first use since the last call.
    #[cfg(test)]
    pub(crate) fn take_bound_on_first_use(&self) -> Vec<String> {
        self.bound_on_first_use.take()
    }

    /// Binds, in parallel, every file of `paths` not bound yet.
    ///
    /// # Errors
    ///
    /// When binding fails on a file.
    pub(crate) fn bind_missing(
        &self,
        inputs: &CheckoutInputs,
        paths: &[&String],
    ) -> Result<(), ManifestJoinError> {
        self.bind_missing_checked(inputs, paths, None)
            .map_err(UpdateError::unguarded)
    }

    fn bind_missing_checked(
        &self,
        inputs: &CheckoutInputs,
        paths: &[&String],
        guard: Option<&RefreshMemory>,
    ) -> Result<(), UpdateError> {
        let missing = paths
            .iter()
            .filter(|path| self.get(path).is_none())
            .copied()
            .collect::<Vec<_>>();
        for (path, bound) in bind_all_checked(inputs, &missing, guard)? {
            guard.file()?;
            self.set_new(&path, bound);
        }
        Ok(())
    }
}

/// The bound files one resolver thread's index reads, binding a file the
/// first time resolution reaches it. A file bound here records its reads
/// into its own binding, never into the facts of the file being resolved.
pub(crate) struct BoundFiles<'a> {
    inputs: &'a CheckoutInputs,
    cache: &'a BindCache,
    facts: ViewBindingFacts<'a>,
    lists: std::cell::OnceCell<Arc<FileLists>>,
    error: std::cell::RefCell<Option<ManifestJoinError>>,
    /// With a bounded cache, the files read since [`Self::release_held`]:
    /// one file's resolution reads the same files again and again, and a
    /// cache that let one go would bind it again for each read.
    held: std::cell::RefCell<HashMap<String, Arc<BoundFile>>>,
}

impl<'a> BoundFiles<'a> {
    pub(crate) fn new(inputs: &'a CheckoutInputs, cache: &'a BindCache) -> Self {
        Self {
            inputs,
            cache,
            facts: thread_facts(inputs),
            lists: std::cell::OnceCell::new(),
            error: std::cell::RefCell::new(None),
            held: std::cell::RefCell::default(),
        }
    }

    /// Lets go of the files read since the last call, which a bounded cache
    /// may then let go of too. Called after each file is resolved.
    pub(crate) fn release_held(&self) {
        self.held.borrow_mut().clear();
    }

    /// `path`'s pass-1 results, bound now if no thread has bound it yet.
    pub(crate) fn bound(&self, path: &str) -> Option<Arc<BoundFile>> {
        let bound = self.bound_uncounted(path)?;
        if self.cache.is_bounded() {
            self.held
                .borrow_mut()
                .entry(path.to_string())
                .or_insert_with(|| Arc::clone(&bound));
        }
        Some(bound)
    }

    fn bound_uncounted(&self, path: &str) -> Option<Arc<BoundFile>> {
        if let Some(bound) = self.held.borrow().get(path) {
            return Some(Arc::clone(bound));
        }
        if let Some(bound) = self.cache.get(path) {
            return Some(bound);
        }
        // Read without keeping the parse: binding reads it once, and an
        // update's resolution reaches many files it binds this way.
        let (_, parse) = self.inputs.parses.read(path)?;
        self.cache.bound_on_first_use.note(path);
        match bind_file(path, &parse, &self.facts, None) {
            Ok((surface, binding)) => Some(self.cache.set_new(
                path,
                BoundFile {
                    surface: Arc::new(surface),
                    binding,
                },
            )),
            Err(error) => {
                self.error.borrow_mut().get_or_insert(error);
                None
            }
        }
    }

    /// The first bind that failed, which a lookup inside resolution cannot
    /// return; callers check after each file.
    pub(crate) fn take_error(&self) -> Option<ManifestJoinError> {
        self.error.borrow_mut().take()
    }

    fn lists(&self) -> &FileLists {
        self.lists.get_or_init(|| self.cache.lists(self.inputs))
    }
}

impl LoadedFileIndexes for BoundFiles<'_> {
    fn contains_file(&self, rel_path: &str) -> bool {
        self.inputs.parses.contains_key(rel_path)
    }

    fn file_index(&self, rel_path: &str) -> Option<LoadedIndex> {
        self.bound(rel_path)
            .map(|bound| LoadedIndex::Shared(Arc::clone(&bound.surface)))
    }

    fn rust_files(&self) -> Vec<String> {
        self.lists().rust.clone()
    }

    fn module_declaring_files(&self) -> Vec<String> {
        self.lists().module_declaring.clone()
    }

    fn module_parents(&self) -> Option<Arc<crate::callgraph_store::ModuleParents>> {
        self.cache
            .module_parents
            .lock()
            .expect("no panics while holding it")
            .clone()
    }
}

/// Resolves every parsed file of a checkout on `threads` threads.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_checkout(
    inputs: &CheckoutInputs,
    threads: usize,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    resolve_checkout_within(inputs, threads, KeyOrderBudget::configured())
}

/// [`resolve_checkout`], with pass 4's key order held to `budget`.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_checkout_within(
    inputs: &CheckoutInputs,
    threads: usize,
    budget: KeyOrderBudget,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    resolve_checkout_bounded(inputs, threads, budget, bind_cache_bytes())
}

/// [`resolve_checkout_within`], keeping bound files up to `bind_bytes`.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_checkout_bounded(
    inputs: &CheckoutInputs,
    threads: usize,
    budget: KeyOrderBudget,
    bind_bytes: usize,
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    // A copy shares every parse with `inputs`, so the stream below lets
    // none of them go.
    let mut copy = inputs.clone();
    let mut resolved = BTreeMap::new();
    // With no store to build into, the spool's workspace goes in the
    // system's temporary folder. Nothing sweeps it there, so it is deleted
    // whether or not resolution succeeds.
    let workspace = Arc::new(
        aft_store::family::BuildScratch::create_unattached(
            &std::env::temp_dir().join("aft-callgraph-builds"),
            |_| Ok::<_, super::checkout::DataError>(()),
        )
        .map_err(|error| ManifestJoinError::Parse(format!("spooled resolutions: {error}")))?,
    );
    let result = resolve_checkout_into(
        &mut copy,
        threads,
        None,
        &workspace,
        budget,
        bind_bytes,
        PassesCheckpoint::NONE,
        &mut |_, files| {
            resolved.extend(files);
            Ok(())
        },
    );
    let removed = workspace.remove();
    result?;
    removed.map_err(spool_error)?;
    Ok(resolved)
}

/// A spool that could not be written or read back fails the build: pass 4
/// must never run on a resolution that is missing or changed.
fn spool_error(error: std::io::Error) -> ManifestJoinError {
    ManifestJoinError::Parse(format!("spooled resolutions: {error}"))
}

/// What a cold build does with its `passes-2-3` checkpoint (see
/// `checkout::resume`).
#[derive(Clone, Copy)]
pub(crate) struct PassesCheckpoint<'m> {
    /// The build's identity digest and the digest of the scanned inputs,
    /// which every spooled range names.
    pub(crate) identity: resume::Digest,
    pub(crate) snapshot: resume::Digest,
    /// The complete stage an earlier attempt left for these inputs, if any.
    pub(crate) adopted: Option<&'m resume::Marker>,
    /// Set when that stage was adopted, so binding and passes 2-3 (each
    /// file's references and type names) were not resolved again.
    pub(crate) resumed: Option<&'m std::sync::atomic::AtomicBool>,
}

impl PassesCheckpoint<'_> {
    /// For a build no later attempt resumes.
    pub(crate) const NONE: Self = Self {
        identity: [0; 32],
        snapshot: [0; 32],
        adopted: None,
        resumed: None,
    };
}

/// Each file's pass-4 inputs, rebuilt from its parse and its spooled
/// pass-2/3 resolution exactly as passes 2-3 build them, a batch at a time.
///
/// # Errors
///
/// When a range cannot be read back or a resolution decoded.
fn sites_from_spool(
    inputs: &mut CheckoutInputs,
    spool: &spool::ResolutionSpool,
    paths: &[String],
) -> Result<Vec<(String, Arc<SiteFile>)>, ManifestJoinError> {
    let mut sites = Vec::with_capacity(paths.len());
    for (range, batch) in paths.chunks(BATCH).enumerate() {
        let encoded = spool.read_range(range).map_err(spool_error)?;
        let parses = &inputs.parses;
        let carry = crate::alloc_trace::carry();
        let built = batch
            .par_iter()
            .zip(encoded.into_par_iter())
            .map(|(path, bytes)| {
                let _label = carry.enter();
                let (key, parse) = parses.read(path).ok_or_else(|| {
                    ManifestJoinError::Parse(format!("no parse of {path} to resume from"))
                })?;
                let resolution = super::fragment::decode(&key, path, &bytes)
                    .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                Ok((path.clone(), Arc::new(SiteFile::new(&parse, &resolution))))
            })
            .collect::<Result<Vec<_>, ManifestJoinError>>()?;
        sites.extend(built);
        inputs.parses.release_decoded();
    }
    Ok(sites)
}

/// Files per on-disk range of encoded resolutions, in path order.
/// Passes 1–3 may process that range in smaller working batches.
const BATCH: usize = 1024;

/// How many decoded resolutions [`resolve_checkout_into`] hands to its sink
/// at once: enough to keep every thread busy building their fragments.
const SINK_CHUNK: usize = 128;

#[cfg(feature = "callgraph-shape-census")]
pub(crate) type BuildCensus = super::shape_census::Census;
#[cfg(not(feature = "callgraph-shape-census"))]
pub(crate) type BuildCensus = ();

/// [`resolve_checkout`], handing finished resolutions to `sink` a batch at a
/// time, in path order, instead of keeping them all, and adding to `memory`,
/// when given, the heap bytes and times of its stages. `sink` runs inside
/// the resolver's thread pool, so it can work on its batch in parallel.
///
/// Pass 4 reads every file's method-call inputs, built from that file's
/// pass-2 and pass-3 results, so it can start only once every file has
/// those. Until then each file's resolution is kept encoded, a small
/// fraction of its decoded size, and decoded again only to add its sites
/// and go to the sink.
///
/// Parses kept encoded (see [`Parses::encoded`]) are decoded when a stage
/// reads them and let go after each batch, so no stage holds every file's
/// parse decoded beyond binding, which reads them all.
///
/// # Errors
///
/// When the resolver fails on a file, or the sink does.
pub(crate) fn resolve_checkout_into(
    inputs: &mut CheckoutInputs,
    threads: usize,
    mut memory: Option<&mut Vec<(String, usize)>>,
    workspace: &Arc<aft_store::family::BuildScratch<super::checkout::DataError>>,
    budget: KeyOrderBudget,
    bind_bytes: usize,
    checkpoint: PassesCheckpoint<'_>,
    sink: &mut (dyn FnMut(&CheckoutInputs, Vec<(String, FileResolution)>) -> Result<(), ManifestJoinError>
              + Send),
) -> Result<BuildCensus, ManifestJoinError> {
    fn refs(batch: &[String]) -> Vec<&String> {
        batch.iter().collect()
    }
    let mut started = std::time::Instant::now();
    let mut lap = |memory: &mut Option<&mut Vec<(String, usize)>>, stage: &str| {
        if let Some(memory) = memory.as_deref_mut() {
            let ms = started.elapsed().as_millis();
            memory.push((
                format!("ms.{stage}"),
                usize::try_from(ms).unwrap_or(usize::MAX),
            ));
        }
        started = std::time::Instant::now();
    };
    // At each stage boundary: a staged record that could not be read fails
    // the build, and so does a disk that fell below the free-space floor
    // (the build then pauses; see `checkout::graph_spill_error`).
    let boundary = |inputs: &CheckoutInputs| -> Result<(), ManifestJoinError> {
        if let Some(records) = inputs.parses.staged_records() {
            records.check().map_err(ManifestJoinError::Parse)?;
        }
        workspace
            .check_space(0)
            .map_err(|error| ManifestJoinError::Parse(error.to_string()))
    };
    let bind_batch_bytes = crate::memory::profile::current().bind_batch_bytes;
    crate::alloc_trace::install(&pool(threads)?, || {
        let paths = inputs.parses.keys().cloned().collect::<Vec<_>>();
        // An earlier attempt of the build that resolved passes 2-3 from these
        // same inputs left them as a complete stage: adopted, binding and
        // passes 2-3 are skipped (see `checkout::resume`).
        let adopted = checkpoint.adopted.and_then(|marker| {
            match spool::ResolutionSpool::adopt(
                workspace,
                marker,
                checkpoint.identity,
                checkpoint.snapshot,
                paths.chunks(BATCH),
            ) {
                Ok(spool) => {
                    if let Some(resumed) = checkpoint.resumed {
                        resumed.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    Some(spool)
                }
                Err(error) => {
                    log::info!(
                        "callgraph build resume: discarded stage {}: {error}",
                        resume::PASSES
                    );
                    let _ = resume::discard_stage(workspace.path(), marker);
                    None
                }
            }
        });
        let (spool, sites) = if let Some(spool) = adopted {
            let sites = sites_from_spool(inputs, &spool, &paths)?;
            lap(&mut memory, "resume");
            boundary(inputs)?;
            (spool, sites)
        } else {
            // Bound files are kept up to a byte budget, and one read again after
            // it was let go is bound again (see `BindCache::bounded`).
            let bound = BindCache::bounded(bind_bytes);
            #[cfg(feature = "test-hooks")]
            super::refresh_probe::stage("binding");
            // The whole-checkout lists read every parse's kinds: noted as each
            // record was staged, or else read a batch at a time, each parse
            // decoded without keeping it.
            let kinds = match inputs.parses.staged_kinds() {
                Some(kinds) => kinds,
                None => {
                    let mut kinds = Vec::with_capacity(paths.len());
                    for batch in paths.chunks(BATCH) {
                        let parses = &inputs.parses;
                        let carry = crate::alloc_trace::carry();
                        kinds.par_extend(batch.par_iter().map(|path| {
                            let _label = carry.enter();
                            parses
                                .read(path)
                                .map_or((false, false), |(_, parse)| FileLists::kinds(&parse))
                        }));
                    }
                    kinds
                }
            };
            let mut lists = FileLists::default();
            for (path, (rust, declaring)) in paths.iter().zip(kinds) {
                if rust {
                    lists.rust.push(path.clone());
                }
                if declaring {
                    lists.module_declaring.push(path.clone());
                }
            }
            bound.set_lists(lists);
            // Resolving a Rust file asks each module's parent; with every
            // module-declaring file bound once here, resolvers read the parents
            // from one index instead of binding those files again.
            let module_parents = bound.index_module_parents(inputs)?;
            relieve_allocator();
            if let Some(memory) = memory.as_deref_mut() {
                memory.push(("resolve.module_parents".to_string(), module_parents));
            }
            lap(&mut memory, "bind");
            boundary(inputs)?;
            let mut spool =
                spool::ResolutionSpool::new(workspace, checkpoint.identity, checkpoint.snapshot)
                    .map_err(spool_error)?;
            let mut sites = Vec::with_capacity(paths.len());
            #[cfg(feature = "test-hooks")]
            super::refresh_probe::stage("resolution");
            for range in paths.chunks(BATCH) {
                let encoded =
                    binding_batch::resolve(inputs, &bound, range, bind_batch_bytes, &mut sites)?;
                spool.write_range(range, &encoded).map_err(spool_error)?;
            }
            if let Some(memory) = memory.as_deref_mut() {
                use aft_store::heap::HeapBytes as _;
                let mut seen = aft_store::heap::Seen::new();
                // The parses are counted by the caller; mark them seen so the
                // parts below count only what they hold besides.
                let _ = inputs.parses.heap_bytes(&mut seen);
                memory.push(("resolve.bound".to_string(), bound.heap_bytes(&mut seen)));
                let counts = bound.counts();
                memory.extend([
                    ("resolve.bind_cache_bound".to_string(), counts.bound),
                    ("resolve.bind_cache_evicted".to_string(), counts.evicted),
                    ("resolve.bind_cache_held".to_string(), counts.held),
                ]);
                memory.push((
                    "resolve.spooled_resolution_bytes".to_string(),
                    usize::try_from(spool.bytes()).unwrap_or(usize::MAX),
                ));
                memory.push(("resolve.spool_table".to_string(), spool.table_bytes()));
            }
            lap(&mut memory, "resolve");
            boundary(inputs)?;
            // Every resolution is spooled and no staged read failed: passes 2-3
            // are a complete stage a later attempt may adopt.
            spool
                .commit()
                .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
            // Pass 4 reads each file's own inputs, built above from its parse
            // and resolution, and never the bound surfaces.
            drop(bound);
            relieve_allocator();
            (spool, sites)
        };
        let calls = MethodCallInputs::of_files(sites);
        if let Some(memory) = memory.as_deref_mut() {
            use aft_store::heap::HeapBytes as _;
            let mut seen = aft_store::heap::Seen::new();
            let _ = inputs.parses.heap_bytes(&mut seen);
            memory.push((
                "resolve.method_call_inputs".to_string(),
                calls.heap_bytes(&mut seen),
            ));
        }
        // Pass 4, first step: in file order, what every site and declaration
        // means, with each answer read from the inputs above (see
        // `dispatch::projected`). The records go to disk, in the workspace's
        // `pass4/` folder; the second step reads them back through a
        // bounded cache. Its budget is the build cache's: the bound files
        // above were let go before it fills, so the two never add up.
        let mut records = site_records::RecordWriter::new(workspace)?;
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("projection");
        resume::kill_point("stage.projection");
        for batch in paths.chunks(BATCH) {
            site_records::project_batch(inputs, &calls, &refs(batch), &mut records)?;
            inputs.parses.release_decoded();
        }
        drop(calls);
        relieve_allocator();
        if let Some(memory) = memory.as_deref_mut() {
            memory.extend(
                records
                    .census()
                    .map(|(part, value)| (part.to_string(), value)),
            );
        }
        lap(&mut memory, "projection");
        boundary(inputs)?;
        let mut source = records.finish(bind_bytes)?;
        if memory.is_some() {
            source = source.counting_touches();
        }
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("sites");
        resume::kill_point("stage.sites");
        // Pass 4, second step: every distinct resolution key resolved once,
        // member name first, with each site's result going to a sorted run
        // (see `key_order`).
        let started = std::time::Instant::now();
        let (answers, mut results) = key_order::resolve(
            &source,
            &paths,
            &inputs.csharp_projects(),
            spool.dir(),
            budget,
            memory.as_deref_mut(),
        )?;
        let sites_ms = started.elapsed().as_millis();
        // Assembly reads each file's record once, from disk, in path order;
        // the records the cache kept are let go.
        source.release_cache();
        #[cfg(feature = "test-hooks")]
        super::refresh_probe::stage("assembly");
        // The key order's units and sorted keys are gone; only the answers
        // and the run of site results remain.
        relieve_allocator();
        // Then, in file order, each file's results become its sites and site
        // dependencies, joined to its spooled pass-2/3 resolution. They go to
        // the sink in small chunks, decoded in parallel, and the sink builds
        // their fragments in parallel too; a whole batch decoded and built
        // at once held about 750 MB more.
        // On glibc, trimming every four batches saved 1.3 GiB on VS Code and
        // 1.2 GiB on Kotlin versus streaming alone. Cold builds still beat the
        // builder that held every fragment until output: 229.6 vs 234.8 seconds
        // and 251.8 vs 277.6 seconds, respectively.
        // Other allocators keep their existing stage-boundary relief. The
        // internal cadence accepts 0 to disable the extra trims.
        let trim_cadence = batch_trim_cadence(
            std::env::var("AFT_INTERNAL_GRAPH_BATCH_TRIM")
                .ok()
                .as_deref(),
        );
        let mut trim_batches = 0usize;
        let mut trim_calls = 0usize;
        let mut trim_time = std::time::Duration::ZERO;
        for (range, batch) in paths.chunks(BATCH).enumerate() {
            let encoded = spool.read_range(range).map_err(spool_error)?;
            if encoded.len() != batch.len() {
                return Err(ManifestJoinError::Parse(format!(
                    "spooled range {range} holds {} resolutions for {} files",
                    encoded.len(),
                    batch.len()
                )));
            }
            let parses = &inputs.parses;
            let mut files = batch.iter().zip(encoded).enumerate();
            loop {
                let mut chunk = Vec::with_capacity(SINK_CHUNK);
                for (offset, (path, bytes)) in files.by_ref().take(SINK_CHUNK) {
                    let number = u32::try_from(range * BATCH + offset).map_err(|_| {
                        ManifestJoinError::Parse("too many files for pass 4".to_string())
                    })?;
                    chunk.push((path.clone(), bytes, results.take_file(number)?));
                }
                if chunk.is_empty() {
                    break;
                }
                let carry = crate::alloc_trace::carry();
                let (answers, source) = (&answers, &source);
                let files = chunk
                    .into_par_iter()
                    .map(|(path, bytes, site_results)| {
                        let _label = carry.enter();
                        let record = source.read_path(&path)?;
                        #[cfg(feature = "callgraph-shape-census")]
                        let mut census = super::shape_census::Census::default();
                        let (sites, dependencies) = answers.assemble(
                            &path,
                            &record,
                            site_results,
                            #[cfg(feature = "callgraph-shape-census")]
                            &mut census,
                        )?;
                        #[cfg(feature = "callgraph-shape-census")]
                        source
                            .census
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .merge(census);
                        let key = parses.key(&path).expect("every resolved file is parsed");
                        let mut file = super::fragment::decode(&key, &path, &bytes)
                            .map_err(|error| ManifestJoinError::Parse(error.to_string()))?;
                        file.sites = sites;
                        file.site_dependencies = dependencies;
                        #[cfg(feature = "test-hooks")]
                        super::checkout::exactness::flip_site_for_control(&path, &mut file);
                        Ok((path, file))
                    })
                    .collect::<Result<Vec<_>, ManifestJoinError>>()?;
                sink(&*inputs, files)?;
            }
            inputs.parses.release_decoded();
            trim_batches += 1;
            if trim_cadence != 0 && trim_batches % trim_cadence == 0 {
                let started = std::time::Instant::now();
                relieve_allocator();
                trim_time += started.elapsed();
                trim_calls += 1;
            }
        }
        results.finish()?;
        drop(answers);
        if trim_cadence != 0 {
            log::info!("pass-4 allocator relief: cadence={trim_cadence} batches={trim_batches} calls={trim_calls} elapsed_us={}", trim_time.as_micros());
        }
        #[cfg(feature = "callgraph-shape-census")]
        let census = std::mem::take(&mut *source.census.lock().unwrap_or_else(|e| e.into_inner()));
        drop(source);
        drop(spool);
        relieve_allocator();
        if let Some(memory) = memory.as_deref_mut() {
            memory.push((
                "ms.sites".to_string(),
                usize::try_from(sites_ms).unwrap_or(usize::MAX),
            ));
        }
        lap(&mut memory, "sites_and_fragments");
        boundary(inputs)?;
        #[cfg(feature = "callgraph-shape-census")]
        return Ok(census);
        #[cfg(not(feature = "callgraph-shape-census"))]
        Ok(())
    })
}

/// The byte budget of a cold build's bound files (see [`BindCache::bounded`]),
/// and later of its pass-4 record cache (see `site_records`), which fills
/// only after the bound files are let go.
/// VS Code's bound surfaces average about 58 KB a file, so the default keeps
/// about 4,500 files: a batch of 1,024 and the files it reaches. With
/// `test-hooks`, `AFT_CALLGRAPH_BUILD_CACHE_BYTES` sets it, so a tiny budget
/// can be checked to give the same graph.
pub(crate) fn bind_cache_bytes() -> usize {
    #[cfg(feature = "test-hooks")]
    if let Some(bytes) = std::env::var("AFT_CALLGRAPH_BUILD_CACHE_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        return bytes;
    }
    256 << 20
}

/// Hands the allocator's free pages back to the system between a build's
/// stages, so what a finished stage let go stops counting as the process's
/// memory while the next stage runs.
fn relieve_allocator() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    crate::memory::return_free_pages(crate::memory::AllocatorReliefSource::ColdBuildStage);
}

fn batch_trim_cadence(value: Option<&str>) -> usize {
    match value {
        None if cfg!(all(target_os = "linux", target_env = "gnu")) => 4,
        Some(value) => value.parse::<usize>().unwrap_or(0),
        None => 0,
    }
}

#[cfg(test)]
mod batch_trim_tests {
    #[test]
    fn batch_trim_defaults_to_four_on_glibc_and_can_be_disabled() {
        use super::batch_trim_cadence;
        assert_eq!(
            batch_trim_cadence(None),
            if cfg!(all(target_os = "linux", target_env = "gnu")) {
                4
            } else {
                0
            }
        );
        for value in [Some(""), Some("true"), Some("-1"), Some("0")] {
            assert_eq!(batch_trim_cadence(value), 0);
        }
        assert_eq!(batch_trim_cadence(Some("1")), 1);
        assert_eq!(batch_trim_cadence(Some("4")), 4);
    }
}

#[cfg(test)]
mod bounded_bind_tests {
    use std::rc::Rc;

    use super::*;
    use crate::callgraph_store::ResolverIndex as _;

    /// Each module target's parent, read through a resolver index over
    /// `bound`, for every Rust file of the fixture.
    fn parents(
        inputs: &CheckoutInputs,
        bound: &BindCache,
        targets: &[&str],
    ) -> Vec<Option<(String, String)>> {
        let facts = Rc::new(thread_facts(inputs));
        let (index, _files) = resolver_index(inputs, bound, &facts);
        targets
            .iter()
            .map(|target| index.module_parent(target))
            .collect()
    }

    /// A bounded cache that keeps one file finds every module's parent in
    /// the index built once over the module-declaring files, the parent the
    /// unbounded cache's scan finds, without binding those files again.
    #[test]
    fn a_bounded_cache_reads_module_parents_from_its_index() {
        let inputs = super::super::incremental_tests::checkout(
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
                ),
                ("src/lib.rs", "pub mod a;\npub mod b;\n"),
                ("src/a.rs", "pub mod c;\npub fn f() {}\n"),
                ("src/a/c.rs", "pub fn g() {}\n"),
                ("src/b.rs", "pub fn h() {}\n"),
            ],
            &[],
        );
        let targets = ["src/a.rs", "src/b.rs", "src/a/c.rs", "src/lib.rs"];
        let scanned = parents(&inputs, &BindCache::default(), &targets);
        assert_eq!(
            scanned[0],
            Some(("src/lib.rs".to_string(), "a".to_string())),
            "the fixture declares its modules"
        );
        let bounded = BindCache::bounded(1);
        assert_eq!(bounded.index_module_parents(&inputs).unwrap(), 3);
        let declaring = bounded.counts().bound;
        assert_eq!(declaring, 2, "lib.rs and a.rs declare modules");
        assert_eq!(parents(&inputs, &bounded, &targets), scanned);
        assert_eq!(
            bounded.counts().bound,
            declaring,
            "no module-declaring file is bound again to find a parent"
        );
        assert!(bounded.counts().evicted >= 1, "one byte keeps one file");
    }
}

/// A [`ResolutionLookup`] with nothing in it, for pass 4 when every file's
/// method-call inputs are already built.
struct NoResolutions;

impl ResolutionLookup for NoResolutions {
    fn resolution(&self, _path: &str) -> Option<&FileResolution> {
        None
    }
}

pub(crate) fn pool(threads: usize) -> Result<rayon::ThreadPool, ManifestJoinError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|index| format!("aft-resolve-{index}"))
        .build()
        .map_err(|error| ManifestJoinError::Parse(error.to_string()))
}

/// The thread pool every live call-graph engine (`LiveCheckout`) resolves
/// on: one for the whole process, started the first time an engine needs
/// it, with the current profile's resolver thread count. A pool per engine kept a full set
/// of resolver threads alive for as long as each edited checkout's engine
/// lived: with dozens of worktrees, hundreds of idle threads.
///
/// Engines run their parallel work through `ThreadPool::install`, so the
/// parallel iterators nested inside stay in this pool. Refreshes of
/// different checkouts share its threads; the runtime runs at most four of
/// them at once. Cold builds keep pools of their own ([`pool`]).
/// A profile change replaces the shared pool; running work keeps its old Arc.
///
/// # Errors
///
/// When the pool's threads cannot start.
pub(crate) fn engine_pool() -> Result<Arc<rayon::ThreadPool>, ManifestJoinError> {
    static POOL: Mutex<Option<(usize, Arc<rayon::ThreadPool>)>> = Mutex::new(None);
    let mut slot = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let threads = crate::memory::profile::current()
        .resolver_threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from));
    if let Some((size, pool)) = slot.as_ref() {
        if *size == threads {
            return Ok(Arc::clone(pool));
        }
    }
    let shared = Arc::new(pool(threads)?);
    *slot = Some((threads, Arc::clone(&shared)));
    Ok(shared)
}

/// The facts one resolver thread reads, recording every checkout read with
/// its answer so a stored resolution can be validated elsewhere.
pub(crate) fn thread_facts(inputs: &CheckoutInputs) -> ViewBindingFacts<'_> {
    ViewBindingFacts::typed(Rc::new(CheckoutFacts { inputs }))
}

/// Pass 1 for `paths`, in parallel: each file's bound surface, and its
/// binding before resolution. Records nothing in any cache, and keeps no
/// parse it decoded ([`Parses::read`]).
///
/// # Errors
///
/// When binding fails on a file.
pub(crate) fn bind_all(
    inputs: &CheckoutInputs,
    paths: &[&String],
) -> Result<HashMap<String, BoundFile>, ManifestJoinError> {
    bind_all_checked(inputs, paths, None).map_err(UpdateError::unguarded)
}

pub(super) fn bind_all_checked(
    inputs: &CheckoutInputs,
    paths: &[&String],
    guard: Option<&RefreshMemory>,
) -> Result<HashMap<String, BoundFile>, UpdateError> {
    let carry = crate::alloc_trace::carry();
    paths
        .par_iter()
        .map_init(
            || thread_facts(inputs),
            |facts, path| {
                guard.file()?;
                let _label = carry.enter();
                let (_, parse) = inputs
                    .parses
                    .read(path)
                    .unwrap_or_else(|| panic!("{path} is not a parsed file"));
                let (surface, binding) = bind_file(path, &parse, facts, None)?;
                Ok({
                    (
                        (*path).clone(),
                        BoundFile {
                            surface: Arc::new(surface),
                            binding,
                        },
                    )
                })
            },
        )
        .collect()
}

/// Passes 2 and 3 for `paths`, in parallel, against the surfaces of
/// `bound`, which binds each file resolution reaches the first time.
/// Method-call sites are left empty, for [`MethodCallInputs`].
///
/// # Errors
///
/// When resolution or binding fails on a file.
pub(crate) fn resolve_files(
    inputs: &CheckoutInputs,
    bound: &BindCache,
    paths: &[&String],
) -> Result<BTreeMap<String, FileResolution>, ManifestJoinError> {
    resolve_files_checked(inputs, bound, paths, None).map_err(UpdateError::unguarded)
}

pub(super) fn resolve_files_checked(
    inputs: &CheckoutInputs,
    bound: &BindCache,
    paths: &[&String],
    guard: Option<&RefreshMemory>,
) -> Result<BTreeMap<String, FileResolution>, UpdateError> {
    Ok(
        resolve_files_with_checked(inputs, bound, paths, &|_, resolution| resolution, guard)?
            .into_iter()
            .collect(),
    )
}

/// [`resolve_files`], passing each file's resolution through `keep` on the
/// thread that resolved it, so a caller can keep less than the whole
/// resolution. Returns the kept values in path order.
///
/// # Errors
///
/// As [`resolve_files`].
pub(crate) fn resolve_files_with<T: Send>(
    inputs: &CheckoutInputs,
    bound: &BindCache,
    paths: &[&String],
    keep: &(dyn Fn(&str, FileResolution) -> T + Sync),
) -> Result<Vec<(String, T)>, ManifestJoinError> {
    resolve_files_with_checked(inputs, bound, paths, keep, None).map_err(UpdateError::unguarded)
}

fn resolve_files_with_checked<T: Send>(
    inputs: &CheckoutInputs,
    bound: &BindCache,
    paths: &[&String],
    keep: &(dyn Fn(&str, FileResolution) -> T + Sync),
    guard: Option<&RefreshMemory>,
) -> Result<Vec<(String, T)>, UpdateError> {
    let carry = crate::alloc_trace::carry();
    paths
        .par_iter()
        .map_init(
            || {
                let _label = carry.enter();
                let facts = Rc::new(thread_facts(inputs));
                let (index, files) = resolver_index(inputs, bound, &facts);
                (facts, index, files)
            },
            |(facts, index, files), path| {
                guard.file()?;
                let _label = carry.enter();
                let (key, parse) = &inputs.parses[path.as_str()];
                let Some(own) = files.bound(path) else {
                    return Err(files
                        .take_error()
                        .unwrap_or_else(|| {
                            ManifestJoinError::Parse(format!("{path} is not a parsed file"))
                        })
                        .into());
                };
                let mut binding = own.binding.clone();
                let mut rows = BTreeSet::new();
                resolve_file(
                    path,
                    key,
                    parse,
                    &mut binding,
                    index,
                    facts,
                    None,
                    &mut rows,
                )?;
                finish_binding(&mut binding);
                // Copied from the bound file, which keeps it; a resolution
                // stores none, so one read back from a publication equals
                // one built here.
                binding.clear_surface();
                let dispatch = dispatch_targets(path, parse, index, facts)?;
                files.release_held();
                if let Some(error) = files.take_error() {
                    return Err(error.into());
                }
                let resolution = FileResolution {
                    rows: rows.into_iter().collect(),
                    binding,
                    dispatch,
                    ..FileResolution::default()
                };
                Ok(((*path).clone(), keep(path, resolution)))
            },
        )
        .collect()
}

/// A resolver index over the files of `bound`, binding each the first time
/// it is reached, and recording into `facts`. Also returns the index's
/// loader, whose bind errors callers check after each file.
pub(crate) fn resolver_index<'a>(
    inputs: &'a CheckoutInputs,
    bound: &'a BindCache,
    facts: &Rc<ViewBindingFacts<'a>>,
) -> (ProjectIndex<'a>, Rc<BoundFiles<'a>>) {
    let files = Rc::new(BoundFiles::new(inputs, bound));
    let index = ProjectIndex::from_loaded(
        std::path::Path::new("/"),
        Rc::clone(&files) as Rc<dyn LoadedFileIndexes + 'a>,
        WorkspaceCratePrefixCache::default(),
        facts.clone(),
    );
    (index, files)
}

/// Pass 3 for one file: for Rust, TypeScript and JavaScript, the targets of
/// its type names.
pub(crate) fn dispatch_targets(
    path: &str,
    parse: &ParseBlob,
    index: &ProjectIndex<'_>,
    facts: &ViewBindingFacts<'_>,
) -> Result<Option<DispatchTypeTargets>, ManifestJoinError> {
    matches!(
        parse.language.as_str(),
        "rust" | "typescript" | "tsx" | "javascript"
    )
    .then(|| dispatch_type_targets_for(path, parse, index, facts))
    .transpose()
}

/// What pass 4 reads about one file beyond its own facts: which of its
/// references Rust spells as an associated or `self.` call, and which of
/// its references already have an ordinary edge from pass 2.
///
/// References are kept by their key (`BlobRef::ordinal`) within the file,
/// not as the `view:<path>:<key>` text that names them in answers: every
/// lookup is for the same file, and a formatted string per reference was
/// one small allocation per call site of every file pass 4 read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OrdinaryCalls {
    /// Keys of call and construction references spelled as an associated
    /// function or a `self.` call.
    syntax: HashSet<u32>,
    /// Reference keys of associated-function calls.
    associated: Vec<u32>,
    /// Keys of references pass 2 linked from a caller symbol to a target
    /// symbol.
    edges: HashSet<u32>,
}

/// One file's pass-4 inputs: its method-call facts, the parse the resolver
/// reads them as, and its ordinary calls. A function of the file's parse
/// and resolution alone.
#[derive(Debug)]
pub(crate) struct SiteFile {
    facts: FileFacts,
    parse: ParseBlob,
    ordinary: OrdinaryCalls,
}

impl SiteFile {
    fn new(parse: &ParseBlob, resolution: &FileResolution) -> Self {
        let references = references_by_key(parse);
        let mut facts = FileFacts::new(parse);
        facts.import_targets = import_targets(&resolution.rows, &references);
        if let Some(dispatch) = &resolution.dispatch {
            facts.type_targets = dispatch.targets.clone();
        }
        let ordinary = ordinary_calls(parse, &resolution.rows, &references);
        let parse = facts.resolver_parse();
        Self {
            facts,
            parse,
            ordinary,
        }
    }

    /// The names of the methods the file declares.
    fn method_names(&self) -> BTreeSet<&str> {
        self.parse
            .dispatch
            .methods
            .iter()
            .map(|method| method.name.as_str())
            .collect()
    }

    /// For a Go file, the names of the methods its interfaces require.
    fn go_required(&self) -> BTreeSet<&str> {
        if self.parse.language != "go" {
            return BTreeSet::new();
        }
        let dispatch = &self.parse.dispatch;
        dispatch
            .methods
            .iter()
            .filter(|method| {
                dispatch
                    .types
                    .iter()
                    .any(|ty| ty.interface && ty.name == method.owner)
            })
            .map(|method| method.name.as_str())
            .collect()
    }
}

/// Every file's pass-4 inputs, as a live checkout keeps them.
///
/// Built from every file, the inputs are all in memory. Started from a
/// publication, they are computed per file the first time pass 4 reads a
/// file, and files are found by method name through the terms the
/// publication stored (see `graph_store::method_terms`); a file whose
/// inputs changed since is filed in memory instead.
#[derive(Default)]
pub(crate) struct MethodCallInputs {
    #[cfg(feature = "callgraph-shape-census")]
    census: Mutex<super::shape_census::Census>,
    /// Each file's inputs computed so far, for the current checkout.
    files: HashMap<String, Arc<SiteFile>>,
    /// The publication whose terms find every file not in `own`.
    stored: Option<Arc<super::graph_store::StoredSlices>>,
    /// Files filed in memory: every file when nothing is stored, otherwise
    /// those whose inputs changed since the publication, removed ones too.
    own: HashSet<String>,
    /// Method name to the files of `own` that declare it.
    methods: HashMap<String, BTreeSet<String>>,
    /// Method name to the Go files of `own` whose interfaces require it.
    go_required: HashMap<String, BTreeSet<String>>,
    static_types: HashMap<String, BTreeSet<String>>,
    /// Every file whose method-call inputs the resolver computed itself, on
    /// its own thread, the first time it read the file.
    computed_while_resolving: FirstReads,
}

/// Below this many files, pass 4 runs on one thread instead of in parallel
/// chunks.
const SMALL_SITE_RUN: usize = 64;

impl MethodCallInputs {
    /// Declaration changes already identify method readers before resolving
    /// any importer. Compare raw facts on both sides: import/type targets are
    /// filled by resolution and are admitted separately when they are known.
    pub(super) fn changed_parse_method_keys(
        old: &CheckoutInputs,
        next: &CheckoutInputs,
        changed: &BTreeSet<String>,
        guard: Option<&RefreshMemory>,
    ) -> Result<BTreeSet<String>, RefreshStop> {
        let mut keys = BTreeSet::new();
        for path in changed {
            guard.file()?;
            let before = old
                .parses
                .read(path)
                .map(|(_, parse)| FileFacts::new(&parse));
            let after = next
                .parses
                .read(path)
                .map(|(_, parse)| FileFacts::new(&parse));
            FileFacts::changed_keys_by_method(path, before.as_ref(), after.as_ref(), &mut keys);
        }
        Ok(keys)
    }

    /// The files whose inputs the resolver computed itself since the last
    /// call.
    #[cfg(test)]
    pub(crate) fn take_computed_while_resolving(&self) -> Vec<String> {
        self.computed_while_resolving.take()
    }

    /// Every file's inputs, from `resolved`.
    pub(crate) fn build(inputs: &CheckoutInputs, resolved: &(dyn ResolutionLookup + Sync)) -> Self {
        let carry = crate::alloc_trace::carry();
        let built = inputs
            .parses
            .par_iter()
            .filter_map(|(path, (_, parse))| {
                let _label = carry.enter();
                let resolution = resolved.resolution(path)?;
                Some((path.clone(), Arc::new(SiteFile::new(parse, resolution))))
            })
            .collect::<Vec<_>>();
        Self::from_site_files(built)
    }

    /// Every file's inputs, from inputs already built for every file,
    /// without the method-name filings: for reading files by path only, as
    /// the cold build's projection pass does.
    fn of_files(files: Vec<(String, Arc<SiteFile>)>) -> Self {
        Self {
            files: files.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Every file's inputs, from inputs already built for every file.
    fn from_site_files(files: Vec<(String, Arc<SiteFile>)>) -> Self {
        let mut calls = Self::default();
        for (path, file) in files {
            calls.file(&path, &file);
            calls.files.insert(path, file);
        }
        calls
    }

    /// Inputs found through `stored`'s terms and computed when first read.
    pub(crate) fn stored(stored: Arc<super::graph_store::StoredSlices>) -> Self {
        Self {
            stored: Some(stored),
            ..Self::default()
        }
    }

    /// These inputs found through `stored`'s method terms from now on (the
    /// hashed method names a publication files each file's fragment under),
    /// as [`Self::stored`] finds them, keeping every file's inputs computed so
    /// far: `stored` is the publication of the parses and resolutions they
    /// were computed from, so its terms file every file as the in-memory
    /// filing does. `None` for inputs built from every file, which find
    /// nothing through stored terms.
    pub(crate) fn rebased(self, stored: Arc<super::graph_store::StoredSlices>) -> Option<Self> {
        self.stored.as_ref()?;
        Some(Self {
            files: self.files,
            stored: Some(stored),
            ..Self::default()
        })
    }

    /// Files `path` in memory as declaring `file`'s methods.
    fn file(&mut self, path: &str, file: &SiteFile) {
        self.own.insert(path.to_string());
        if let Some(facts) = &file.parse.dispatch.static_facts {
            for key in facts.type_keys(&file.parse.language) {
                self.static_types
                    .entry(key)
                    .or_default()
                    .insert(path.to_string());
            }
        }
        for name in file.method_names() {
            self.methods
                .entry(name.to_string())
                .or_default()
                .insert(path.to_string());
        }
        for name in file.go_required() {
            self.go_required
                .entry(name.to_string())
                .or_default()
                .insert(path.to_string());
        }
    }

    fn unfile(&mut self, path: &str, file: &SiteFile) {
        if let Some(facts) = &file.parse.dispatch.static_facts {
            for key in facts.type_keys(&file.parse.language) {
                if let Some(files) = self.static_types.get_mut(&key) {
                    files.remove(path);
                }
            }
        }
        for name in file.method_names() {
            if let Some(files) = self.methods.get_mut(name) {
                files.remove(path);
            }
        }
        for name in file.go_required() {
            if let Some(files) = self.go_required.get_mut(name) {
                files.remove(path);
            }
        }
    }

    /// Keeps `path`'s inputs as `parse` and `resolution` give them, unless
    /// they are known already: called before an update replaces either, so
    /// [`Self::update`] can compare the old inputs with the new.
    pub(crate) fn remember(
        &mut self,
        path: &str,
        parse: Option<&ParseBlob>,
        resolution: Option<&FileResolution>,
    ) {
        if self.files.contains_key(path) {
            return;
        }
        if let (Some(parse), Some(resolution)) = (parse, resolution) {
            self.files
                .insert(path.to_string(), Arc::new(SiteFile::new(parse, resolution)));
        }
    }

    /// The inputs of one file as `resolution` leaves them, or none when the
    /// file is gone. Returns the dependency keys whose readers the change
    /// can affect, and whether the file's own pass-4 inputs changed. The old
    /// inputs are those [`Self::remember`] kept, or computed earlier.
    pub(crate) fn update(
        &mut self,
        inputs: &CheckoutInputs,
        path: &str,
        resolution: Option<&FileResolution>,
    ) -> (BTreeSet<String>, bool) {
        let new = resolution
            .zip(inputs.parses.get(path))
            .map(|(file, (_, parse))| Arc::new(SiteFile::new(parse, file)));
        let old = self.files.remove(path);
        let mut keys = BTreeSet::new();
        FileFacts::changed_keys_by_method(
            path,
            old.as_ref().map(|old| &old.facts),
            new.as_ref().map(|new| &new.facts),
            &mut keys,
        );
        // Own facts are pass-4 inputs even when the precise diff emits no whole-file key.
        // Content edits also seed pass 4 directly in the incremental engine.
        let own_changed = old.as_ref().map(|old| &old.facts) != new.as_ref().map(|new| &new.facts)
            || old.as_ref().map(|old| &old.ordinary) != new.as_ref().map(|new| &new.ordinary);
        if let Some(old) = &old {
            self.unfile(path, old);
        }
        // From here on the file is filed in memory, gone or not, so its
        // stored terms no longer apply.
        self.own.insert(path.to_string());
        if let Some(new) = new {
            self.file(path, &new);
            self.files.insert(path.to_string(), new);
        }
        (keys, own_changed)
    }

    /// Pass 4 for `paths`: each file's kept method-call sites and the
    /// dependency keys their resolutions read. A few files run on this
    /// thread; more run in parallel chunks, each with its own resolver.
    /// Inputs computed on the way are kept for later updates.
    pub(crate) fn resolve_sites(
        &mut self,
        inputs: &CheckoutInputs,
        resolved: &(dyn ResolutionLookup + Sync),
        paths: &[&String],
    ) -> Result<Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>, ManifestJoinError> {
        self.resolve_sites_checked(inputs, resolved, paths, None)
            .map_err(UpdateError::unguarded)
    }

    pub(super) fn resolve_sites_checked(
        &mut self,
        inputs: &CheckoutInputs,
        resolved: &(dyn ResolutionLookup + Sync),
        paths: &[&String],
        guard: Option<&RefreshMemory>,
    ) -> Result<Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>, UpdateError> {
        // Most updates reach no method-call site; the source below lists
        // every parsed file, so skip it then.
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let source = SiteSource::new(inputs, resolved, self, guard)?;
        let projects = inputs.csharp_projects();
        let results = if paths.len() <= SMALL_SITE_RUN {
            source.load_declarers(paths, guard)?;
            let resolver = Resolver::over(&source).with_csharp_projects(&projects);
            resolve_chunk(inputs, &source, &resolver, paths, guard)
        } else {
            let chunk = paths
                .len()
                .div_ceil(rayon::current_num_threads() * 4)
                .max(1);
            let carry = crate::alloc_trace::carry();
            paths
                .par_chunks(chunk)
                .map(|chunk| {
                    let _label = carry.enter();
                    // The resolver and its memo are single-threaded caches
                    // over the shared source: one per chunk of files.
                    let resolver = Resolver::over(&source).with_csharp_projects(&projects);
                    resolve_chunk(inputs, &source, &resolver, chunk, guard)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|chunks| chunks.into_iter().flatten().collect())
        }?;
        if let Some(error) = source
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return Err(ManifestJoinError::Parse(error).into());
        }
        let loaded = source.into_loaded(guard)?;
        for (path, file) in loaded {
            guard.file()?;
            self.files.insert(path, file);
        }
        Ok(results)
    }
}

/// The method-call resolver's view of a checkout's pass-4 inputs during one
/// run: the inputs kept, and any other file's computed the first time it is
/// read.
struct SiteSource<'s> {
    /// Resolver callbacks return `Option`; keep failed stored inputs so an
    /// unreadable file declaring a method cannot silently make calls unresolved.
    error: Mutex<Option<String>>,
    inputs: &'s CheckoutInputs,
    resolved: &'s (dyn ResolutionLookup + Sync),
    calls: &'s MethodCallInputs,
    /// Every parsed file whose inputs are not kept, computed on first read.
    loaded: HashMap<&'s str, std::sync::OnceLock<Option<Arc<SiteFile>>>>,
}

impl<'s> SiteSource<'s> {
    fn new(
        inputs: &'s CheckoutInputs,
        resolved: &'s (dyn ResolutionLookup + Sync),
        calls: &'s MethodCallInputs,
        guard: Option<&RefreshMemory>,
    ) -> Result<Self, RefreshStop> {
        let mut loaded = HashMap::new();
        for path in inputs.parses.keys() {
            guard.file()?;
            if !calls.files.contains_key(path.as_str()) {
                loaded.insert(path.as_str(), std::sync::OnceLock::new());
            }
        }
        Ok(Self {
            inputs,
            resolved,
            calls,
            loaded,
            error: Mutex::new(None),
        })
    }

    /// The source over `calls` when it keeps every parsed file's inputs, as
    /// a cold build's do: no file is computed on first read, so the parsed
    /// files are not listed.
    fn kept(
        inputs: &'s CheckoutInputs,
        resolved: &'s (dyn ResolutionLookup + Sync),
        calls: &'s MethodCallInputs,
    ) -> Self {
        Self {
            inputs,
            resolved,
            calls,
            loaded: HashMap::new(),
            error: Mutex::new(None),
        }
    }

    fn site(&self, path: &str) -> Option<&SiteFile> {
        if let Some(file) = self.calls.files.get(path) {
            return Some(file);
        }
        self.loaded
            .get(path)?
            .get_or_init(|| {
                self.calls.computed_while_resolving.note(path);
                self.compute(path)
            })
            .as_deref()
    }

    /// The file's pass-4 inputs, from its parse and resolution read without
    /// keeping either decoded: the inputs keep only what pass 4 reads, and
    /// a site calling a common method name reads every file declaring it.
    fn compute(&self, path: &str) -> Option<Arc<SiteFile>> {
        let computed = || {
            let (_, parse) = self.inputs.parses.read(path)?;
            let resolution = self.resolved.read_once(path)?;
            Some(Arc::new(SiteFile::new(&parse, &resolution)))
        };
        let file = computed();
        if file.is_none() {
            *self
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(format!(
                "stored method-call inputs of {path} could not be read"
            ));
        }
        file
    }

    /// Computes, in parallel, the inputs of every file that declares a
    /// method named like one a site of `paths` calls.
    ///
    /// The resolver reads every declarer of a called method's name, one
    /// after another on the thread resolving the site. A file not read since
    /// the engine started is computed then, from its stored parse and
    /// resolution, so the first update after a restart that reaches a
    /// common method name waits on decoding each of its declarers in turn.
    /// Computing them here first leaves the resolver finding them computed;
    /// what it reads, and what it records reading, is unchanged.
    fn load_declarers(
        &self,
        paths: &[&String],
        guard: Option<&RefreshMemory>,
    ) -> Result<(), RefreshStop> {
        let mut members = BTreeSet::new();
        for path in paths {
            guard.file()?;
            if let Some((_, parse)) = self.inputs.parses.get(path) {
                for site in parse.iter_bound_dispatch_sites() {
                    guard.site()?;
                    members.extend(site.member);
                }
            }
        }
        let mut declarers = BTreeSet::new();
        for member in members {
            for file in self.files_with_method(&member) {
                guard.file()?;
                if !self.calls.files.contains_key(file.as_str()) {
                    declarers.insert(file);
                }
            }
        }
        let declarers = declarers.into_iter().collect::<Vec<_>>();
        let carry = crate::alloc_trace::carry();
        declarers.par_iter().try_for_each(|file| {
            guard.file()?;
            let _label = carry.enter();
            if let Some(slot) = self.loaded.get(file.as_str()) {
                slot.get_or_init(|| self.compute(file));
            }
            Ok(())
        })
    }

    /// The stored files filed under `term` that are not filed in memory.
    fn stored_files(&self, kind: aft_store::graph::TermKind, term: &str) -> BTreeSet<String> {
        self.calls
            .stored
            .as_ref()
            .map(|stored| {
                stored
                    .files_recording(kind, term.as_bytes())
                    .into_iter()
                    .filter(|file| !self.calls.own.contains(file))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn into_loaded(
        self,
        guard: Option<&RefreshMemory>,
    ) -> Result<Vec<(String, Arc<SiteFile>)>, RefreshStop> {
        let mut loaded = Vec::new();
        for (path, slot) in self.loaded {
            guard.file()?;
            if let Some(Some(file)) = slot.into_inner() {
                loaded.push((path.to_string(), file));
            }
        }
        Ok(loaded)
    }
}

impl<'a, 's: 'a> DispatchFiles<'a> for SiteSource<'s> {
    fn file(&'a self, path: &str) -> Option<&'a ParseBlob> {
        self.site(path).map(|file| &file.parse)
    }

    fn files_with_method(&self, name: &str) -> Vec<String> {
        let mut files = self.stored_files(aft_store::graph::TermKind::MethodName, name);
        if let Some(own) = self.calls.methods.get(name) {
            files.extend(own.iter().cloned());
        }
        files.into_iter().collect()
    }

    fn files_with_static_type(&self, key: &str) -> Vec<String> {
        let mut files = self.stored_files(aft_store::graph::TermKind::StaticType, key);
        if let Some(own) = self.calls.static_types.get(key) {
            files.extend(own.iter().cloned());
        }
        files.into_iter().collect()
    }

    fn files_in(&self, directory: &str) -> Vec<String> {
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        self.inputs
            .parses
            .keys_from(&prefix)
            .take_while(|path| path.starts_with(&prefix))
            .filter(|path| !path[prefix.len()..].contains('/'))
            .cloned()
            .collect()
    }

    fn go_files_requiring(&self, name: &str) -> Vec<String> {
        let mut files = self.stored_files(aft_store::graph::TermKind::GoRequired, name);
        if let Some(own) = self.calls.go_required.get(name) {
            files.extend(own.iter().cloned());
        }
        files.into_iter().collect()
    }

    fn import_target(&self, file: &str, module: &str) -> Option<String> {
        self.site(file)?.facts.import_targets.get(module).cloned()
    }

    fn type_target(&self, file: &str, name: &str) -> Option<(String, String)> {
        self.site(file)?.facts.type_targets.get(name).cloned()
    }

    fn associated_site(&self, file: &str, ordinal: u32) -> bool {
        self.site(file)
            .is_some_and(|site| site.ordinary.associated.contains(&ordinal))
    }
}

fn resolve_chunk(
    inputs: &CheckoutInputs,
    source: &SiteSource<'_>,
    resolver: &Resolver<'_>,
    paths: &[&String],
    guard: Option<&RefreshMemory>,
) -> Result<Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>, UpdateError> {
    let memoized = MemoizedResolver::new(resolver);
    resolver.take_dependencies();
    #[cfg(feature = "callgraph-shape-census")]
    let mut census = super::shape_census::Census::default();
    let result = paths
        .iter()
        .map(|path| {
            guard.file()?;
            let (_, parse) = inputs
                .parses
                .get(path)
                .ok_or_else(|| ManifestJoinError::Parse(format!("missing parse for {path}")))?;
            let calls = &source
                .site(path)
                .ok_or_else(|| {
                    ManifestJoinError::Parse(format!("missing method-call inputs for {path}"))
                })?
                .ordinary;
            let mut sites = Vec::new();
            for site in parse.iter_bound_dispatch_sites() {
                guard.site()?;
                let resolution = memoized.resolve(path, &site);
                // This runs for memo hits too, and before Rust's ordinary-edge
                // preservation can omit a supplemental method-call row.
                #[cfg(feature = "callgraph-shape-census")]
                census.record(
                    &parse.language,
                    &site,
                    resolver.census_exit.get(),
                    &resolution,
                );
                // Keep proven self-call and scoped module-function bindings
                // when dispatch facts do not identify a project receiver type.
                if parse.language == "rust"
                    && resolution.external > 0
                    && resolution.targets.is_empty()
                    && calls.edges.contains(&site.ordinal)
                    && calls.syntax.contains(&site.ordinal)
                {
                    continue;
                }
                sites.push(SiteResolution { site, resolution });
            }
            // Every file depends on its own facts, which `Resolver::resolve`
            // reads without recording.
            let mut dependencies = resolver.take_dependencies();
            dependencies.insert(dependency::file(path));
            Ok(((*path).clone(), sites, dependencies))
        })
        .collect();
    #[cfg(feature = "callgraph-shape-census")]
    source
        .calls
        .census
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge(census);
    result
}

/// The target file of each module a file imports, from its pass-2 rows.
/// For a module imported from two targets, the greater path wins, as when
/// they were read back sorted.
fn import_targets(
    rows: &[DerivedRow],
    references: &HashMap<u32, &BlobRef>,
) -> BTreeMap<String, String> {
    let mut pairs = rows
        .iter()
        .filter(|row| row.kind == BlobRefKind::Import)
        .filter_map(|row| {
            let module = references.get(&row.ref_ordinal)?.module_path.clone()?;
            let target = String::from_utf8(row.target_path.clone()?).ok()?;
            Some((module, target))
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs.into_iter().collect()
}

fn ordinary_calls(
    parse: &ParseBlob,
    rows: &[DerivedRow],
    references: &HashMap<u32, &BlobRef>,
) -> OrdinaryCalls {
    let mut calls = OrdinaryCalls::default();
    let mut symbols = HashSet::new();
    for symbol in &parse.symbols {
        symbols.insert(symbol.scoped_name.as_str());
        symbols.insert(symbol.name.as_str());
    }
    for row in rows {
        let Some(reference) = references.get(&row.ref_ordinal) else {
            continue;
        };
        let full = reference.full_ref.as_deref().filter(|_| {
            parse.language == "rust"
                && matches!(row.kind, BlobRefKind::Call | BlobRefKind::Construction)
        });
        if let Some(full) = full {
            let associated = associated_reference(full);
            if associated {
                calls.associated.push(row.ref_ordinal);
            }
            if associated || full.starts_with("self.") {
                calls.syntax.insert(row.ref_ordinal);
            }
        }
        if matches!(
            row.kind,
            BlobRefKind::Call | BlobRefKind::Construction | BlobRefKind::CallbackRef
        ) && reference
            .caller_symbol
            .as_deref()
            .is_some_and(|symbol| symbols.contains(symbol))
            && row.target_path.is_some()
            && row.target_symbol.is_some()
        {
            calls.edges.insert(row.ref_ordinal);
        }
    }
    calls
}

/// The path `rel` names after following symlinks inside a checkout whose
/// symlinks, folders and paths `link`, `is_dir` and `exists` report, or
/// `None` when it leaves the checkout, loops, or names nothing. Shared by
/// [`CheckoutFacts`] and by the tracing that records which paths an answer
/// consulted, so the two can never walk differently.
pub(crate) fn canonical_walk(
    rel: &[u8],
    link: impl Fn(&[u8]) -> Option<Vec<u8>>,
    is_dir: impl Fn(&[u8]) -> bool,
    exists: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    if rel.starts_with(b"/") {
        return None;
    }
    let mut pending = rel
        .split(|b| *b == b'/')
        .map(<[u8]>::to_vec)
        .collect::<std::collections::VecDeque<_>>();
    let mut components = Vec::<Vec<u8>>::new();
    let mut seen = BTreeSet::new();
    let mut followed = 0;
    while let Some(component) = pending.pop_front() {
        match component.as_slice() {
            b"" | b"." => continue,
            b".." => {
                components.pop()?;
                continue;
            }
            _ => components.push(component),
        }
        let path = components.join(&b'/');
        if let Some(target) = link(&path) {
            // Bound expanding cycles such as a -> a/a as well as exact cycles.
            followed += 1;
            if target.starts_with(b"/") || followed > 40 || !seen.insert((path, pending.clone())) {
                return None;
            }
            components.pop();
            for component in target.split(|b| *b == b'/').rev() {
                pending.push_front(component.to_vec());
            }
        } else if !pending.is_empty() && !is_dir(&path) {
            return None;
        }
    }
    let path = components.join(&b'/');
    (exists(&path) || is_dir(&path)).then_some(path)
}

/// The checkout's paths and configuration, as the resolver asks for them.
/// Answers what the views manifest answered for the same checkout.
#[derive(Debug)]
pub(crate) struct CheckoutFacts<'a> {
    pub(crate) inputs: &'a CheckoutInputs,
}

impl CheckoutFacts<'_> {
    fn exists(&self, rel: &[u8]) -> bool {
        self.inputs.state.get(rel).is_some()
    }
}

impl ProjectFacts for CheckoutFacts<'_> {
    fn is_file(&self, rel: &[u8]) -> bool {
        self.inputs
            .state
            .get(rel)
            .is_some_and(|record| record.kind != PathKind::Symlink)
    }

    fn is_dir(&self, rel: &[u8]) -> bool {
        let state = &self.inputs.state;
        if rel.is_empty() {
            return !state.paths().is_empty();
        }
        !state.directory(rel).is_empty()
    }

    fn config_bytes(&self, rel: &[u8]) -> Option<Arc<[u8]>> {
        let path = self.canonical(rel)?;
        self.inputs.configs.get(&path).cloned()
    }

    fn symlink_target(&self, rel: &[u8]) -> Option<&[u8]> {
        self.inputs
            .state
            .get(rel)
            .filter(|record| record.kind == PathKind::Symlink)
            .map(|record| record.link.as_slice())
    }

    /// The path `rel` names after following symlinks inside the checkout,
    /// or `None` when it leaves the checkout, loops, or names nothing.
    fn canonical(&self, rel: &[u8]) -> Option<Vec<u8>> {
        canonical_walk(
            rel,
            |path| self.symlink_target(path).map(<[u8]>::to_vec),
            |path| self.is_dir(path),
            |path| self.exists(path),
        )
    }

    fn list_dir(&self, rel: &[u8]) -> Vec<DirEntry> {
        let mut prefix = rel.to_vec();
        if !prefix.is_empty() {
            prefix.push(b'/');
        }
        let state = &self.inputs.state;
        let range = if rel.is_empty() {
            0..state.paths().len()
        } else {
            state.directory(rel)
        };
        let mut entries = BTreeMap::new();
        for record in &state.paths()[range] {
            let Some(tail) = record.path.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            if tail.is_empty() {
                continue;
            }
            if let Some(slash) = tail.iter().position(|b| *b == b'/') {
                entries
                    .entry(tail[..slash].to_vec())
                    .or_insert(EntryKind::Directory);
            } else {
                let kind = if record.kind == PathKind::Symlink {
                    EntryKind::Symlink
                } else {
                    EntryKind::Regular
                };
                entries.insert(tail.to_vec(), kind);
            }
        }
        entries
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect()
    }
}

aft_store::heap_bytes_of_fields!(CheckoutInputs {
    state,
    parses,
    configs,
});
aft_store::heap_bytes_of_fields!(Parses { map });
impl aft_store::heap::HeapBytes for StagedParse {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.key.heap_bytes(seen) + self.records.heap_bytes(seen)
    }
}
aft_store::heap_bytes_of_fields!(ParseSlot {
    value,
    stored,
    encoded,
    staged
});
aft_store::heap_bytes_of_fields!(Resolutions { map, changed });
aft_store::heap_bytes_of_fields!(ResolutionSlot { value, source });
aft_store::heap_bytes_of_fields!(EncodedResolution { record_key, bytes });

impl aft_store::heap::HeapBytes for ResolutionSource {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Memory => 0,
            Self::Stored(stored) => stored.heap_bytes(seen),
            Self::Encoded(encoded) => encoded.heap_bytes(seen),
        }
    }
}
aft_store::heap_bytes_of_fields!(FileResolution {
    rows,
    binding,
    dispatch,
    sites,
    site_dependencies,
});
aft_store::heap_bytes_of_fields!(SiteResolution { site, resolution });
aft_store::heap_bytes_of_fields!(BoundFile { surface, binding });
aft_store::heap_bytes_of_fields!(FileLists {
    rust,
    module_declaring,
});
impl aft_store::heap::HeapBytes for BindCache {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        self.files.heap_bytes(seen)
            + self.lists.heap_bytes(seen)
            + self.bound_on_first_use.heap_bytes(seen)
            + self.module_parents.heap_bytes(seen)
    }
}
aft_store::heap_bytes_none!(BoundEntry);
aft_store::heap_bytes_of_fields!(OrdinaryCalls {
    syntax,
    associated,
    edges,
});
aft_store::heap_bytes_of_fields!(SiteFile {
    facts,
    parse,
    ordinary,
});
#[cfg(feature = "callgraph-shape-census")]
aft_store::heap_bytes_of_fields!(MethodCallInputs {
    census,
    files,
    stored,
    own,
    methods,
    go_required,
    static_types,
    computed_while_resolving,
});
#[cfg(not(feature = "callgraph-shape-census"))]
aft_store::heap_bytes_of_fields!(MethodCallInputs {
    files,
    stored,
    own,
    methods,
    go_required,
    static_types,
    computed_while_resolving,
});
