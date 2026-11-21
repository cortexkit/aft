//! A checkout's call graph between publications: the stored slices of its
//! last publication, with the files resolved since laid over them.
//!
//! Every row of the call graph belongs to the file whose resolution wrote
//! it, so an overlay works file by file: a file the overlay holds answers
//! from the overlay, a file it removed answers nothing, and every other file
//! answers from the base. A lookup takes the base's matching files that the
//! overlay does not replace, plus the overlay's own matches.
//!
//! One edit can resolve over a thousand files again, and the overlay holds
//! them until the next publication, so it holds each file's slice encoded,
//! as a publication stores it ([`FileSlice::encode`]), and decodes it when a
//! query reads it through the process's shared slice cache
//! ([`super::slice_cache`]), which bounds what decoded files every checkout
//! holds together. The lookups name each file and each term once, and file
//! the files under a term by number.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::query::{symbol_keys, SliceSource};
use super::slice::FileSlice;
use super::slice_cache::SourceToken;

/// Slices from `base`, with replaced and removed files laid over them.
///
/// A copy shares the replaced files with the overlay it was copied from;
/// the first change to either copies them (see [`Arc::make_mut`]).
#[derive(Clone)]
pub(crate) struct OverlaySlices<B> {
    base: B,
    /// The files resolved since `base` was published.
    replaced: Arc<Replaced>,
    /// The files removed since `base` was published.
    removed: BTreeSet<String>,
}

/// The files an overlay replaces, with their lookups.
#[derive(Clone, Default)]
struct Replaced {
    slices: BTreeMap<String, ReplacedSlice>,
    lookups: Lookups,
}

/// One replaced file's slice.
#[derive(Clone)]
enum ReplacedSlice {
    /// Decoded, as the caller gave it.
    Built(Arc<FileSlice>),
    /// Encoded, decoded when a query reads it.
    Encoded(Arc<EncodedSlice>),
}

/// A file's slice as a publication stores it, decoded through the shared
/// slice cache.
pub(crate) struct EncodedSlice {
    record_key: Box<str>,
    bytes: Box<[u8]>,
    /// What the shared cache files the decoded slice under, with the file's
    /// path.
    cached_as: Arc<CacheKey>,
}

/// A key of the shared slice cache, shared by every slice one refresh
/// encoded: each path is encoded at most once by one refresh, so the key
/// and a path name one slice. When the last slice holding it goes, the
/// cache lets go of what it decoded for them.
pub(crate) struct CacheKey(SourceToken);

impl CacheKey {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(SourceToken::next()))
    }
}

impl Drop for CacheKey {
    fn drop(&mut self) {
        super::slice_cache::shared().forget(self.0);
    }
}

/// A file's slice encoded, with the lookup terms it is filed under, made
/// off the overlay (in parallel, say) and laid over it with
/// [`OverlaySlices::replace_encoded`].
pub(crate) struct PreparedSlice {
    slice: EncodedSlice,
    terms: BTreeSet<Vec<u8>>,
}

impl PreparedSlice {
    /// Encodes `slice`, whose content record has the key `record_key`, for
    /// decoding under `cached_as`.
    #[must_use]
    pub(crate) fn of(slice: &FileSlice, record_key: &str, cached_as: &Arc<CacheKey>) -> Self {
        Self {
            slice: EncodedSlice {
                record_key: record_key.into(),
                bytes: slice.encode().into_boxed_slice(),
                cached_as: Arc::clone(cached_as),
            },
            terms: terms_of(slice),
        }
    }
}

impl EncodedSlice {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        let cache = super::slice_cache::shared();
        if let Some(slice) = cache.get(self.cached_as.0, path) {
            return Some(slice);
        }
        match FileSlice::decode(path, &self.record_key, &self.bytes) {
            Ok(slice) => Some(cache.insert(self.cached_as.0, path, Arc::new(slice))),
            // This process encoded the bytes, so this is a defect in the
            // slice codec, which its tests check.
            Err(error) => {
                crate::slog_warn!(
                    "{path}: a call-graph slice encoded in memory does not decode: {error}"
                );
                None
            }
        }
    }
}

/// What a lookup term starts with: which lookup it belongs to.
const CALLER: u8 = 0;
const REFERENCE_NAME: u8 = 1;
const SYMBOL_NAME: u8 = 2;
const LANGUAGE: u8 = 3;

fn term(kind: u8, parts: &[&str]) -> Vec<u8> {
    let mut term = vec![kind];
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            // Paths and symbol names never hold a NUL.
            term.push(0);
        }
        term.extend_from_slice(part.as_bytes());
    }
    term
}

/// Every term `slice` is found under: the targets of its edges, the short
/// names of its references, its symbols' names, and its language.
fn terms_of(slice: &FileSlice) -> BTreeSet<Vec<u8>> {
    let mut terms = BTreeSet::new();
    for edge in slice.edges() {
        terms.insert(term(CALLER, &[edge.target_file(), edge.target_symbol()]));
    }
    for reference in slice.references() {
        if let Some(name) = reference.short_name() {
            terms.insert(term(REFERENCE_NAME, &[name]));
        }
    }
    for name in symbol_keys(slice) {
        terms.insert(term(SYMBOL_NAME, &[&name]));
    }
    terms.insert(term(LANGUAGE, &[slice.language()]));
    terms
}

/// Which replaced files each term finds. Each path and each term is held
/// once, and files are filed under terms by number.
#[derive(Clone, Default)]
struct Lookups {
    /// Every term ever filed, with its number.
    terms: HashMap<Box<[u8]>, u32>,
    /// The files filed under each term, by term number: file numbers in
    /// ascending order.
    files: Vec<Vec<u32>>,
    /// The terms each file is filed under, by file number; empty for a file
    /// no longer replaced.
    terms_of: Vec<Box<[u32]>>,
    /// Each file's path, by file number.
    paths: Vec<Box<str>>,
    /// Each file's number, by path.
    numbers: HashMap<Box<str>, u32>,
}

impl Lookups {
    fn number(&mut self, path: &str) -> u32 {
        if let Some(number) = self.numbers.get(path) {
            return *number;
        }
        let number = u32::try_from(self.paths.len()).expect("fewer than 2^32 files");
        self.paths.push(path.into());
        self.terms_of.push(Box::default());
        self.numbers.insert(path.into(), number);
        number
    }

    /// Files `path` under exactly `terms`.
    fn set(&mut self, path: &str, terms: BTreeSet<Vec<u8>>) {
        let file = self.number(path);
        self.unfile(file);
        let mut filed = Vec::with_capacity(terms.len());
        for term in terms {
            let number = match self.terms.get(term.as_slice()) {
                Some(number) => *number,
                None => {
                    let number = u32::try_from(self.files.len()).expect("fewer than 2^32 terms");
                    self.terms.insert(term.into_boxed_slice(), number);
                    self.files.push(Vec::new());
                    number
                }
            };
            let files = &mut self.files[number as usize];
            if let Err(at) = files.binary_search(&file) {
                files.insert(at, file);
            }
            filed.push(number);
        }
        self.terms_of[file as usize] = filed.into_boxed_slice();
    }

    /// Files `path` under nothing.
    fn remove(&mut self, path: &str) {
        if let Some(file) = self.numbers.get(path).copied() {
            self.unfile(file);
        }
    }

    fn unfile(&mut self, file: u32) {
        for number in std::mem::take(&mut self.terms_of[file as usize]).iter() {
            let files = &mut self.files[*number as usize];
            if let Ok(at) = files.binary_search(&file) {
                files.remove(at);
            }
        }
    }

    fn files(&self, term: &[u8]) -> BTreeSet<String> {
        self.terms
            .get(term)
            .map(|number| {
                self.files[*number as usize]
                    .iter()
                    .map(|file| self.paths[*file as usize].to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Replaced {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        match self.slices.get(path)? {
            ReplacedSlice::Built(slice) => Some(Arc::clone(slice)),
            ReplacedSlice::Encoded(encoded) => encoded.slice(path),
        }
    }
}

impl<B: SliceSource> OverlaySlices<B> {
    pub(crate) fn new(base: B) -> Self {
        Self {
            base,
            replaced: Arc::default(),
            removed: BTreeSet::new(),
        }
    }

    /// Lays a file's new slice over the base.
    pub(crate) fn replace(&mut self, slice: FileSlice) {
        let path = slice.path().to_string();
        self.removed.remove(&path);
        let replaced = Arc::make_mut(&mut self.replaced);
        replaced.lookups.set(&path, terms_of(&slice));
        replaced
            .slices
            .insert(path, ReplacedSlice::Built(Arc::new(slice)));
    }

    /// Lays `path`'s new slice, encoded, over the base.
    pub(crate) fn replace_encoded(&mut self, path: &str, prepared: PreparedSlice) {
        self.removed.remove(path);
        let replaced = Arc::make_mut(&mut self.replaced);
        replaced.lookups.set(path, prepared.terms);
        replaced.slices.insert(
            path.to_string(),
            ReplacedSlice::Encoded(Arc::new(prepared.slice)),
        );
    }

    /// Removes a file from the checkout.
    pub(crate) fn remove(&mut self, path: &str) {
        let replaced = Arc::make_mut(&mut self.replaced);
        replaced.lookups.remove(path);
        replaced.slices.remove(path);
        self.removed.insert(path.to_string());
    }

    /// How many files the overlay replaces or removes.
    pub(crate) fn len(&self) -> usize {
        self.replaced.slices.len() + self.removed.len()
    }

    /// The slices the overlay lies over.
    pub(crate) fn base(&self) -> &B {
        &self.base
    }

    /// Whether the base still answers for `path`.
    fn answers_from_base(&self, path: &str) -> bool {
        !self.removed.contains(path) && !self.replaced.slices.contains_key(path)
    }

    fn merge(&self, base: BTreeSet<String>, own: BTreeSet<String>) -> BTreeSet<String> {
        let mut files = base
            .into_iter()
            .filter(|path| self.answers_from_base(path))
            .collect::<BTreeSet<_>>();
        files.extend(own);
        files
    }
}

impl<B: SliceSource> SliceSource for OverlaySlices<B> {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        if self.removed.contains(path) {
            return None;
        }
        if self.replaced.slices.contains_key(path) {
            return self.replaced.slice(path);
        }
        self.base.slice(path)
    }

    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String> {
        self.merge(
            self.base.caller_files(target_file, target_symbol),
            self.replaced
                .lookups
                .files(&term(CALLER, &[target_file, target_symbol])),
        )
    }

    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_with_ref_name(short_name),
            self.replaced
                .lookups
                .files(&term(REFERENCE_NAME, &[short_name])),
        )
    }

    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_with_symbol_name(name),
            self.replaced.lookups.files(&term(SYMBOL_NAME, &[name])),
        )
    }

    fn files_of_language(&self, language: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_of_language(language),
            self.replaced.lookups.files(&term(LANGUAGE, &[language])),
        )
    }

    fn files(&self) -> Vec<String> {
        let mut files = self
            .base
            .files()
            .into_iter()
            .filter(|path| !self.removed.contains(path))
            .collect::<BTreeSet<_>>();
        files.extend(self.replaced.slices.keys().cloned());
        files.into_iter().collect()
    }
}

// A cache key is a plain number; the slices decoded under it are counted
// by the cache.
aft_store::heap_bytes_none!(CacheKey);
aft_store::heap_bytes_of_fields!(EncodedSlice {
    record_key,
    bytes,
    cached_as,
});
aft_store::heap_bytes_of_fields!(Lookups {
    terms,
    files,
    terms_of,
    paths,
    numbers,
});
aft_store::heap_bytes_of_fields!(Replaced { slices, lookups });

impl aft_store::heap::HeapBytes for ReplacedSlice {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        match self {
            Self::Built(slice) => slice.heap_bytes(seen),
            Self::Encoded(encoded) => encoded.heap_bytes(seen),
        }
    }
}

impl<B: aft_store::heap::HeapBytes> aft_store::heap::HeapBytes for OverlaySlices<B> {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        let Self {
            base,
            replaced,
            removed,
        } = self;
        base.heap_bytes(seen) + replaced.heap_bytes(seen) + removed.heap_bytes(seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_layer::query::MemorySlices;

    /// The slices of a checkout of `files`, resolved cold.
    fn slices(files: &[(&str, &str)]) -> Vec<FileSlice> {
        let inputs = crate::data_layer::incremental_tests::checkout(files, &[]);
        let resolved = crate::data_layer::callgraph::resolve_checkout(&inputs, 1).unwrap();
        resolved
            .iter()
            .map(|(path, resolution)| {
                let (key, parse) = &inputs.parses[path];
                FileSlice::build(path, key, parse, resolution)
            })
            .collect()
    }

    fn memory(slices: &[FileSlice]) -> MemorySlices {
        let mut memory = MemorySlices::default();
        for slice in slices {
            memory.insert(slice.clone());
        }
        memory
    }

    /// Checks that `overlay` answers every lookup and slice as `expected`
    /// does, for every term a slice of either `slices` is filed under.
    fn same_answers(
        overlay: &OverlaySlices<MemorySlices>,
        expected: &MemorySlices,
        slices: &[&FileSlice],
    ) {
        assert_eq!(overlay.files(), expected.files());
        for path in overlay.files() {
            assert_eq!(
                overlay.slice(&path).map(|slice| slice.encode()),
                expected.slice(&path).map(|slice| slice.encode()),
                "{path}"
            );
        }
        for slice in slices {
            for edge in slice.edges() {
                let (file, symbol) = (edge.target_file(), edge.target_symbol());
                assert_eq!(
                    overlay.caller_files(file, symbol),
                    expected.caller_files(file, symbol),
                    "callers of {file} {symbol}"
                );
            }
            for name in slice
                .references()
                .filter_map(|reference| reference.short_name())
            {
                assert_eq!(
                    overlay.files_with_ref_name(name),
                    expected.files_with_ref_name(name),
                    "references named {name}"
                );
            }
            for name in symbol_keys(slice) {
                assert_eq!(
                    overlay.files_with_symbol_name(&name),
                    expected.files_with_symbol_name(&name),
                    "symbols named {name}"
                );
            }
            assert_eq!(
                overlay.files_of_language(slice.language()),
                expected.files_of_language(slice.language())
            );
        }
    }

    #[test]
    fn an_encoded_overlay_answers_as_the_slices_it_holds() {
        let before = slices(&[
            (
                "a.ts",
                "import { run } from './b';\nexport function main() { run(); }\n",
            ),
            ("b.ts", "export function run() { return 1; }\n"),
            (
                "c.ts",
                "import { run } from './b';\nexport function other() { run(); }\n",
            ),
        ]);
        let after = slices(&[
            (
                "a.ts",
                "import { stop } from './b';\nexport function main() { stop(); }\n",
            ),
            (
                "b.ts",
                "export function run() { return 1; }\nexport function stop() { return 2; }\n",
            ),
        ]);
        let every = before.iter().chain(&after).collect::<Vec<_>>();
        let cached_as = CacheKey::new();
        let prepared = |slice: &FileSlice| PreparedSlice::of(slice, "key", &cached_as);
        let mut overlay = OverlaySlices::new(memory(&before));
        same_answers(&overlay, &memory(&before), &every);

        // A file laid over, then laid over again, is filed only under its
        // newest slice's terms.
        overlay.replace_encoded("a.ts", prepared(&before[1]));
        overlay.replace_encoded("a.ts", prepared(&after[0]));
        overlay.replace_encoded("b.ts", prepared(&after[1]));
        overlay.remove("c.ts");
        let mut expected = memory(&after);
        same_answers(&overlay, &expected, &every);

        // A copy keeps answering as it did when its original changes.
        let served = overlay.clone();
        overlay.remove("a.ts");
        same_answers(&served, &expected, &every);
        expected.remove("a.ts");
        same_answers(&overlay, &expected, &every);
        assert_eq!(overlay.len(), 3);
    }
}
