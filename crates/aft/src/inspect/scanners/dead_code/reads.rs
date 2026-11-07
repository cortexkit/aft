//! What materializing one file reads about every other file.
//!
//! A file's dead-code block (its edges, roots and imports) depends on its own
//! facts and call graph, and on entries of whole-project indexes: which file
//! exports a name, where a Rust module lives, whether the Oxc analysis of
//! JavaScript and TypeScript finds each of a file's exports used, and
//! so on. To redo only the files an edit affects, each file's
//! materialization records every index entry it reads (an index and a key).
//! After an edit, an entry whose answer changed marks every file that read
//! it for materializing again; every other file keeps its block.
//!
//! Reads are recorded per thread while [`Recording`] is active, so the
//! indexes need no recorder passed through every helper that reads them.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// An index materializing a file can read. Each names a map or set over the
/// whole project; a key selects one entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum Lookup {
    /// A file's exported (and Rust-declared) symbols, by file.
    ExportedSymbolsByFile,
    /// The files exporting a symbol, by symbol.
    FilesByExportedSymbol,
    /// A file's default-exported symbol, by file.
    DefaultExportByFile,
    /// A file's Oxc verdicts (whether the Oxc analysis of JavaScript and
    /// TypeScript finds each of its exports used), by file.
    OxcByFile,
    /// Whether a file is compiled only for tests by a `#[cfg(test)]` module.
    TestModuleFiles,
    /// A Rust file's public-API items, by file.
    RustPublicItems,
    /// Whether a short type name is used as a receiver anywhere.
    RustUsedReceivers,
    /// A Go file's methods kept alive by interface dispatch, by file.
    GoLiveMethods,
    /// A Rust file's items, by file.
    RustItemsByFile,
    /// An out-of-line Rust module's file, by declaring file and module path.
    RustModuleFiles,
    /// Whether an inline Rust module exists, by file and module path.
    RustModuleInline,
    /// An out-of-line Rust module file's declaring file and scope.
    RustModuleParents,
    /// How many out-of-line Rust modules there are (a bound when walking
    /// up to the crate root).
    RustModuleParentCount,
    /// Whether a Rust file globs its parent module (`use super::*`).
    RustModuleGlobsParent,
    /// Whether a (file, symbol) node is a Rust type declaration.
    RustTypeDeclarations,
    /// A Rust file's imports, by file.
    RustImportsByFile,
    /// Whether a file is an entry point of the project.
    LivenessRootFiles,
    /// An executable root's exports, by file.
    ExecutableRootExports,
    /// Whether a file is public API.
    PublicApiFiles,
    /// A call-graph file whose parse or resolution was read while turning
    /// the call graph's rows into a file's edges.
    GraphFile,
    /// The Go files of a package folder, by folder.
    GoPackage,
    /// The call-graph files whose calls and file-level facts are filed
    /// under a project-relative file.
    GraphPaths,
    /// A Rust file's items with one name, by file and name. Resolving a
    /// call into another file looks only at the items named like the
    /// callee, so an item added under a new name redoes no caller.
    RustItemsNamed,
}

type Frame = HashMap<Lookup, HashSet<Box<str>>>;

thread_local! {
    static READS: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Records `index` read at `key` in this thread's innermost recording, if
/// any.
pub(crate) fn record(index: Lookup, key: &str) {
    READS.with(|reads| {
        if let Some(frame) = reads.borrow_mut().last_mut() {
            let keys = frame.entry(index).or_default();
            if !keys.contains(key) {
                keys.insert(key.into());
            }
        }
    });
}

/// Records reads on this thread from creation until [`Self::finish`].
/// Recordings stack: a thread that waits inside one file's materialization
/// may run another file's (rayon steals work while it waits), and that
/// file's reads go to its own recording.
pub(crate) struct Recording {
    /// The depth of this recording's frame, to check frames end in order.
    depth: usize,
    finished: bool,
}

impl Recording {
    pub(crate) fn start() -> Self {
        let depth = READS.with(|reads| {
            let mut reads = reads.borrow_mut();
            reads.push(Frame::new());
            reads.len()
        });
        Self {
            depth,
            finished: false,
        }
    }

    /// Every (index, key) read since the recording started, sorted.
    pub(crate) fn finish(mut self) -> Vec<(Lookup, String)> {
        self.finished = true;
        let frame = READS.with(|reads| {
            let mut reads = reads.borrow_mut();
            assert_eq!(
                reads.len(),
                self.depth,
                "dead-code read recordings end in order"
            );
            reads.pop()
        });
        let mut reads = frame
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(index, keys)| keys.into_iter().map(move |key| (index, key.into_string())))
            .collect::<Vec<_>>();
        reads.sort();
        reads
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if !self.finished {
            READS.with(|reads| reads.borrow_mut().truncate(self.depth.saturating_sub(1)));
        }
    }
}

/// An index whose reads are recorded as [`Lookup`] `index`. It offers only
/// keyed reads, so every read names the entry it depends on.
#[derive(Debug, Default)]
pub(crate) struct Tracked<M> {
    index: Option<Lookup>,
    map: M,
}

impl<M> Tracked<M> {
    pub(crate) fn new(index: Lookup, map: M) -> Self {
        Self {
            index: Some(index),
            map,
        }
    }

    /// The whole index, for building and comparing it rather than reading
    /// it while materializing a file.
    pub(crate) fn all(&self) -> &M {
        &self.map
    }

    /// Records a read of `key` in this index.
    pub(crate) fn record_key(&self, key: &str) {
        if let Some(index) = self.index {
            record(index, key);
        }
    }

    /// Records a read of `key` as a read of `lookup`, a finer-grained view
    /// of this index, when this index's reads are recorded.
    pub(crate) fn record_as(&self, lookup: Lookup, key: &str) {
        if self.index.is_some() {
            record(lookup, key);
        }
    }
}

impl<V> Tracked<BTreeMap<String, V>> {
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.record_key(key);
        self.map.get(key)
    }
}

impl Tracked<BTreeSet<String>> {
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.record_key(key);
        self.map.contains(key)
    }
}

impl Tracked<&BTreeSet<String>> {
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.record_key(key);
        self.map.contains(key)
    }
}

/// The key of a (file, symbol) pair.
pub(crate) fn pair_key(first: &str, second: &str) -> String {
    format!("{first}\0{second}")
}

/// The key of a (file, module path) pair.
pub(crate) fn path_key(file: &str, path: &[String]) -> String {
    let mut key = file.to_string();
    for segment in path {
        key.push('\0');
        key.push_str(segment);
    }
    key
}

/// The answer an index gives at a key, as a digest: equal digests mean the
/// same answer.
pub(crate) fn digest<T: std::fmt::Debug + ?Sized>(answer: &T) -> u64 {
    let text = format!("{answer:?}");
    let hash = blake3::hash(text.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("eight bytes"))
}
