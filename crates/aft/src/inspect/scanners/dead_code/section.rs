//! Each checkout's dead-code graph, kept between rollups and patched for
//! the files an edit affects.
//!
//! A rollup that works out every file keeps what it worked out in a base
//! file beside the family's store (see [`super::kept`]): every scanned
//! file's block and skeleton, the tables of the whole-project indexes, and
//! which files read each index entry.
//!
//! A later rollup over the same files and settings reads the blocks back
//! and only works out reachability. A rollup after edits patches the newest
//! kept graph instead of starting from nothing:
//! - a file whose call-graph facts or content changed is read and
//!   materialized again;
//! - each whole-project index is brought up to date from the changed files'
//!   old and new skeletons alone; the analyses over many files (the Oxc
//!   verdicts, the Rust public API, Go interface dispatch) are worked out
//!   again from their kept inputs when a changed file's inputs to them
//!   changed;
//! - every index entry that changed marks the files that read it to be
//!   materialized again;
//! - every other file keeps its block, and every other index entry is read
//!   from the base only if a redone file asks for it.
//!
//! The patched rollup keeps an overlay over the same base: what differs from
//! the base since it was written. Anything the patch cannot account for
//! (another format, a failed read, too many changes) works out every file.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use super::engine::{
    materialize_each, other_roots, resident_mib, stored_facts, timed, Assembler, EncodedBlock,
    GraphSource, MaterializedFile, Strings, Totals,
};
use super::index::{Index, KeptEntries};
use super::kept::{self, Base, ModuleInput, Overlay, OverlayFile, State, TableSpans};
use super::reads::{digest, pair_key, path_key, Lookup};
use super::{
    file_exported_symbols, file_rust_imports, file_rust_items, file_rust_type_declarations,
    go_interface_live_methods, oxc_verdicts_by_file, parse_dead_code_contributions, relative_path,
    rust_item_names_changed, rust_public_api_items, with_job_canonical_paths, CallgraphSnapshot,
    ContextIndexes, DeadCodeContribution, GoDispatchFacts, GraphFileFacts, MaterializeContext,
    OxcEngineResult, OxcFactsContribution, RustApiFacts, RustApiItem, RustApiModule, RustApiUse,
    RustModuleIndex,
};
use crate::data_layer::checkout::GraphIdentity;
use crate::data_layer::dead_code_graph::DeadCodeGraph;
use crate::inspect::entry_points::{EntryPointSet, ProjectRoles};
use crate::inspect::InspectJob;

/// When more than this share (in percent) of the scanned files, and more
/// than [`PATCH_LIMIT_FILES`], must be materialized again, a rollup works
/// out every file instead.
const PATCH_LIMIT_PERCENT: usize = 30;
const PATCH_LIMIT_FILES: usize = 64;

/// When an overlay would hold more files than this share (in percent) of
/// the scanned files, and more than [`PATCH_LIMIT_FILES`], the rollup works
/// out every file and keeps a new base, so overlays stay small.
const OVERLAY_LIMIT_PERCENT: usize = 10;

/// Whether redoing `files` of `scope` scanned files costs about as much as
/// working out every file.
fn too_many(files: usize, scope: usize) -> bool {
    files > PATCH_LIMIT_FILES && files * 100 > scope * PATCH_LIMIT_PERCENT
}

/// How many kept blocks are decoded at once: decoding is parallel, and only
/// a chunk's decoded blocks exist before they are added to the graph.
const BLOCK_CHUNK: usize = 512;

/// How a rollup may use kept graphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// Read a kept graph of the same files, else patch the newest one, else
    /// work out every file; keep the result.
    Kept,
    /// Work out every file and keep nothing: what the dead-code gate
    /// (`data_layer::dead_code_gate`) checks a patched rollup against.
    Nothing,
}

/// What a rollup did, for the dead-code gate.
#[derive(Debug, Default)]
pub(crate) struct Probe {
    /// `read`, `patched` or `full`.
    pub(crate) path: &'static str,
    /// Files whose facts or call graph changed since the kept graph.
    pub(crate) changed: usize,
    /// Files materialized again (the changed ones included).
    pub(crate) materialized: usize,
    /// A digest of each whole-project index the rollup built.
    pub(crate) indexes: Vec<(&'static str, u64)>,
    /// Each file's block as the graph reads it, by file.
    pub(crate) blocks: BTreeMap<String, String>,
    /// The bytes of what the rollup kept: a base or an overlay.
    pub(crate) kept_bytes: Option<u64>,
    /// Whether to gather `indexes` and `blocks`, which read every kept
    /// entry and block.
    pub(crate) details: bool,
    /// Whether a patch goes ahead however many files it must redo, so the
    /// gate checks the patch rather than a rollup that works out every file.
    pub(crate) force_patch: bool,
}

/// The Oxc engine's analysis of the files named, as the rollup asks for it.
pub(crate) type OxcFor<'a> = dyn Fn(&[PathBuf]) -> Result<Option<OxcEngineResult>, String> + 'a;

/// What every path of a rollup reads about the view and the project.
struct Rollup<'a, 'g> {
    job: &'a InspectJob,
    root: &'a Path,
    files: &'a crate::data_layer::checkout::ViewGraphFiles<'g>,
    graph: &'a DeadCodeGraph<'g>,
    source: GraphSource<'a, 'g>,
    snapshot: &'a CallgraphSnapshot,
    public_api_files: &'a BTreeSet<String>,
    entries: EntryPointSet,
    oxc: &'a OxcFor<'a>,
    /// The scanned files, project-relative, in scan order.
    scope: Vec<String>,
    identities: BTreeMap<String, GraphIdentity>,
    others: BTreeMap<String, (u128, u64)>,
    config: String,
    exact: String,
}

/// Dead code for `job`'s scope files over `snapshot`, which must carry the
/// data layer's call graph: read from a kept graph of the same files, else
/// patched from the newest kept graph, else worked out for every file (with
/// `oxc`, the Oxc engine's analysis of the files it is given); see the
/// module's documentation. `use_` says whether kept graphs are read and
/// written; `probe` gathers what the dead-code gate compares.
///
/// # Errors
///
/// When the snapshot carries no call graph, or a file of it or its facts
/// cannot be read.
#[allow(clippy::too_many_arguments)]
pub(crate) fn aggregate_from_view(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
    oxc: &OxcFor<'_>,
    public_api_files: &BTreeSet<String>,
    roles: &ProjectRoles,
    timings: &mut Vec<(&'static str, f64)>,
    use_: Use,
    mut probe: Option<&mut Probe>,
) -> Result<Value, String> {
    let root = job.project_root.as_path();
    let view = snapshot
        .data_layer
        .as_ref()
        .ok_or_else(|| "the dead-code snapshot carries no call graph".to_string())?;
    with_job_canonical_paths(|| {
        let files = view.files();
        // A file's symbols are read when first asked for: a rollup that
        // reads the kept graph or patches it parses only what it touches.
        let graph = DeadCodeGraph::open_lazy(root, &files);
        let source = timed(timings, "file the call graph", || GraphSource::new(&graph));
        let scope = job
            .scope_files
            .iter()
            .map(|file| relative_path(root, file))
            .collect::<Vec<_>>();
        let identities = timed(timings, "identify files", || identities(&files))?;
        let others = scope
            .iter()
            .filter(|file| source.graph_paths(file) == (None, None))
            .map(|file| (file.clone(), file_stamp(&root.join(file))))
            .collect::<BTreeMap<_, _>>();
        let config = text_digest(&format!(
            "{}\n{}",
            kept::BASE_HEADER,
            super::DEAD_CODE_FACTS_FORMAT_VERSION
        ));
        let entries = timed(timings, "entry points", || {
            crate::inspect::entry_points::resolve_entry_points(root)
        });
        let exact = text_digest(&format!(
            "{config}\n{scope:?}\n{public_api_files:?}\n{:?}\n{:?}\n{:?}\n{identities:?}\n{others:?}",
            snapshot.entry_points,
            entries.public_api_files(),
            entries.executable_root_exports(),
        ));
        let rollup = Rollup {
            job,
            root,
            files: &files,
            graph: &graph,
            source,
            snapshot,
            public_api_files,
            entries,
            oxc,
            scope,
            identities,
            others,
            config,
            exact,
        };
        let dir = match use_ {
            Use::Kept => view.section_dir().map(Path::to_path_buf),
            Use::Nothing => None,
        };
        let Some(dir) = dir else {
            return rollup.full(None, roles, timings, probe);
        };
        let started = std::time::Instant::now();
        let exact = [kept::BASE_EXTENSION, kept::OVERLAY_EXTENSION]
            .into_iter()
            .find_map(|extension| {
                Kept::open(&dir, &dir.join(format!("{}.{extension}", rollup.exact)))
            });
        if let Some(exact) = exact {
            timings.push((
                "open the kept graph",
                started.elapsed().as_secs_f64() * 1000.0,
            ));
            if let Some(probe) = probe {
                probe.path = "read";
            }
            return rollup.read(exact, roles, timings);
        }
        let bases = kept::kept_files(&dir)
            .into_iter()
            .map(|(_, path)| path)
            .filter(|path| kept::kept_config(path).as_deref() == Some(&rollup.config));
        for base in bases {
            let Some(kept) = Kept::open(&dir, &base) else {
                continue;
            };
            timings.push((
                "open the kept graph",
                started.elapsed().as_secs_f64() * 1000.0,
            ));
            match rollup.patch(kept, &dir, roles, timings, probe.as_deref_mut()) {
                Ok(Some(aggregate)) => return Ok(aggregate),
                Ok(None) => {}
                Err(error) => crate::slog_warn!(
                    "tier2 dead_code: could not patch the kept graph {}: {error}",
                    base.display()
                ),
            }
            break;
        }
        let keep = dir.join(format!("{}.{}", rollup.exact, kept::BASE_EXTENSION));
        rollup.full(Some(keep), roles, timings, probe)
    })
}

fn text_digest(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// `file`'s modification time (nanoseconds) and size; zeros when it cannot
/// be read.
fn file_stamp(file: &Path) -> (u128, u64) {
    std::fs::metadata(file)
        .map(|metadata| {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |time| time.as_nanos());
            (modified, metadata.len())
        })
        .unwrap_or((0, 0))
}

/// Every call-graph file's identity.
fn identities(
    files: &crate::data_layer::checkout::ViewGraphFiles<'_>,
) -> Result<BTreeMap<String, GraphIdentity>, String> {
    use crate::data_layer::dead_code_graph::CallGraphFiles;
    files
        .paths()
        .into_iter()
        .map(|path| {
            let identity = files
                .identity(&path)
                .ok_or_else(|| format!("no identity for {path}"))?;
            Ok((path, identity))
        })
        .collect()
}

/// Whether `path`'s facts are the same under identities `base` (from a
/// kept graph) and `current` (this view's): the same content record, and
/// the same resolution, told by its fragment key or by a digest of its
/// bytes.
fn same_identity(
    files: &crate::data_layer::checkout::ViewGraphFiles<'_>,
    path: &str,
    base: &GraphIdentity,
    current: &GraphIdentity,
) -> Result<bool, String> {
    if base.record != current.record {
        return Ok(false);
    }
    if base.fragment.is_some() && base.fragment == current.fragment {
        return Ok(true);
    }
    let published = files.published_identity(path)?;
    let digest_of = |identity: &GraphIdentity| match &identity.digest {
        Some(digest) => Some(digest.clone()),
        None => published
            .as_ref()
            .filter(|published| published.fragment == identity.fragment)
            .and_then(|published| published.digest.clone()),
    };
    let base_digest = digest_of(base);
    Ok(base_digest.is_some() && base_digest == digest_of(current))
}

/// `snapshot` listing `graph`'s exported symbols: a data-layer snapshot
/// lists none of its own.
pub(crate) fn with_exports(
    snapshot: &CallgraphSnapshot,
    graph: &DeadCodeGraph<'_>,
) -> CallgraphSnapshot {
    CallgraphSnapshot {
        exported_symbols: graph.all_exported_symbols(),
        ..snapshot.clone()
    }
}

/// A block as the graph reads it, with its strings written out, so two
/// rollups' blocks of one file compare.
fn canonical(encoded: &EncodedBlock, strings: &Strings) -> String {
    let calls = encoded
        .calls
        .chunks_exact(5)
        .map(|call| {
            serde_json::json!([
                strings.get(call[0]),
                strings.get(call[1]),
                strings.get(call[2]),
                call[3],
                call[4]
            ])
        })
        .collect::<Vec<_>>();
    let roots = encoded
        .roots
        .chunks_exact(2)
        .map(|root| serde_json::json!([strings.get(root[0]), strings.get(root[1])]))
        .collect::<Vec<_>>();
    serde_json::json!({"calls": calls, "roots": roots, "block": encoded.block}).to_string()
}

/// Adds `count` to `counts[key]` for each key, removing keys that reach
/// zero; returns the keys that appeared or disappeared.
fn count<'k>(
    counts: &mut BTreeMap<String, u32>,
    keys: impl IntoIterator<Item = &'k String>,
    add: bool,
    flipped: &mut BTreeSet<String>,
) {
    for key in keys {
        if add {
            let entry = counts.entry(key.clone()).or_insert(0);
            if *entry == 0 {
                flipped.insert(key.clone());
            }
            *entry += 1;
        } else if let Some(entry) = counts.get_mut(key) {
            *entry -= 1;
            if *entry == 0 {
                counts.remove(key);
                flipped.insert(key.clone());
            }
        }
    }
}

/// The receiver names of a skeleton.
fn receivers(skeleton: &DeadCodeContribution) -> impl Iterator<Item = &String> {
    skeleton
        .rust_api
        .iter()
        .flat_map(|facts| facts.receiver_references.iter())
}

/// The module rules of a skeleton, when it has any.
fn module_input(skeleton: &DeadCodeContribution) -> Option<ModuleInput> {
    let modules = skeleton
        .rust_api
        .as_ref()
        .map(|facts| facts.modules.clone())
        .unwrap_or_default();
    (skeleton.globs_parent_module || !modules.is_empty()).then_some(ModuleInput {
        globs_parent: skeleton.globs_parent_module,
        modules,
    })
}

/// The module index of `inputs` over the files of `scope`, read in scope
/// order as a rollup over every file reads them.
fn module_index(scope: &[String], inputs: &BTreeMap<String, ModuleInput>) -> RustModuleIndex {
    let known = scope.iter().map(String::as_str).collect::<BTreeSet<_>>();
    RustModuleIndex::from_inputs(
        &known,
        scope.iter().filter_map(|file| {
            let input = inputs.get(file)?;
            Some((file.as_str(), input.globs_parent, input.modules.as_slice()))
        }),
    )
}

/// The entries that differ between two module indexes, as read keys.
fn module_changes(old: &RustModuleIndex, new: &RustModuleIndex, keys: &mut Vec<(Lookup, String)>) {
    for key in old.files.keys().chain(new.files.keys()) {
        if old.files.get(key) != new.files.get(key) {
            keys.push((Lookup::RustModuleFiles, path_key(&key.0, &key.1)));
        }
    }
    for key in old.inline.symmetric_difference(&new.inline) {
        keys.push((Lookup::RustModuleInline, path_key(&key.0, &key.1)));
    }
    for key in old.parents.keys().chain(new.parents.keys()) {
        if old.parents.get(key) != new.parents.get(key) {
            keys.push((Lookup::RustModuleParents, key.clone()));
        }
    }
    if old.parents.len() != new.parents.len() {
        keys.push((Lookup::RustModuleParentCount, String::new()));
    }
    for key in old.globs_parent.symmetric_difference(&new.globs_parent) {
        keys.push((Lookup::RustModuleGlobsParent, key.clone()));
    }
}

/// The keys whose entries differ between two maps.
fn map_changes<'m, V: PartialEq + 'm>(
    old: &'m BTreeMap<String, V>,
    new: &'m BTreeMap<String, V>,
) -> impl Iterator<Item = &'m String> {
    old.keys()
        .chain(new.keys().filter(|key| !old.contains_key(*key)))
        .filter(|key| old.get(*key) != new.get(*key))
}

/// A digest of each entry of an analysis.
fn digests<V: std::fmt::Debug>(entries: &BTreeMap<String, V>) -> BTreeMap<String, u64> {
    entries
        .iter()
        .map(|(key, value)| (key.clone(), digest(value)))
        .collect()
}

/// The hash of each read of a materialized file.
fn read_hashes(reads: &[(Lookup, String)]) -> Vec<u64> {
    reads
        .iter()
        .map(|(lookup, key)| kept::read_hash(*lookup, key))
        .collect()
}

/// A kept graph opened for reading: a base, and the overlay over it when the
/// newest rollup patched it.
struct Kept {
    base: Arc<Base>,
    overlay: Option<Overlay>,
    /// Each base file's number.
    base_slots: Arc<HashMap<String, usize>>,
    /// The base's files, by number.
    base_scope: Arc<Vec<String>>,
}

impl Kept {
    /// The kept graph at `path` in `dir` (a base, or an overlay with its
    /// base); `None` when it is missing or cannot be read.
    fn open(dir: &Path, path: &Path) -> Option<Self> {
        let is_overlay = path
            .extension()
            .is_some_and(|extension| extension == kept::OVERLAY_EXTENSION);
        let (base, overlay) = if is_overlay {
            let (base, overlay) = kept::read_overlay(path)?;
            (Base::open(&dir.join(base))?, Some(overlay))
        } else {
            (Base::open(path)?, None)
        };
        if overlay
            .as_ref()
            .is_some_and(|overlay| overlay.state.config != base.directory.state.config)
        {
            return None;
        }
        let base_scope = Arc::new(base.directory.state.scope.clone());
        let base_slots = Arc::new(
            base_scope
                .iter()
                .enumerate()
                .map(|(slot, file)| (file.clone(), slot))
                .collect(),
        );
        Some(Self {
            base: Arc::new(base),
            overlay,
            base_slots,
            base_scope,
        })
    }

    /// The whole-project facts of the kept graph.
    fn state(&self) -> &State {
        match &self.overlay {
            Some(overlay) => &overlay.state,
            None => &self.base.directory.state,
        }
    }

    fn overlay_file(&self, file: &str) -> Option<&OverlayFile> {
        self.overlay.as_ref()?.files.get(file)
    }

    /// The string table of the kept graph, and how many of its strings the
    /// base holds.
    fn strings(&self) -> Result<(Vec<Box<str>>, usize), String> {
        let mut strings = self.base.strings()?;
        let base = strings.len();
        if let Some(overlay) = &self.overlay {
            strings.extend(overlay.strings.iter().map(|string| string.as_str().into()));
        }
        Ok((strings, base))
    }

    /// The kept block of each of `files` (files of the kept graph), decoded
    /// in parallel, in their order.
    fn blocks(&self, files: &[&str]) -> Result<Vec<EncodedBlock>, String> {
        use rayon::prelude::*;
        files
            .par_iter()
            .map(|file| {
                if let Some(kept) = self.overlay_file(file) {
                    return Ok(EncodedBlock {
                        strings: Vec::new(),
                        calls: kept.calls.clone(),
                        roots: kept.roots.clone(),
                        block: kept.block.clone(),
                    });
                }
                let slot = *self
                    .base_slots
                    .get(*file)
                    .ok_or_else(|| format!("no kept block for {file}"))?;
                let (calls, roots, block) = kept::parse_block(&self.base.block(slot)?)?;
                Ok(EncodedBlock {
                    strings: Vec::new(),
                    calls,
                    roots,
                    block,
                })
            })
            .collect()
    }

    /// The skeleton of `file` in the kept graph, `None` when the graph has
    /// no such file.
    fn skeleton(
        &self,
        file: &str,
        scope: &HashSet<&str>,
    ) -> Result<Option<DeadCodeContribution>, String> {
        if let Some(kept) = self.overlay_file(file) {
            return Ok(Some(kept.skeleton.clone()));
        }
        if !scope.contains(file) {
            return Ok(None);
        }
        match self.base_slots.get(file) {
            Some(slot) => self.base.skeleton(*slot).map(Some),
            None => Ok(None),
        }
    }

    /// Part of the skeleton of each of `files` (files of the kept graph),
    /// decoded as `T` from the base, or `from_overlay` for files the overlay
    /// holds; in their order.
    fn skeletons_as<T: DeserializeOwned + Send>(
        &self,
        files: &[&String],
        from_overlay: impl Fn(&DeadCodeContribution) -> T,
    ) -> Result<Vec<T>, String> {
        let base_slots = files
            .iter()
            .filter(|file| self.overlay_file(file).is_none())
            .map(|file| {
                self.base_slots
                    .get(file.as_str())
                    .copied()
                    .ok_or_else(|| format!("no kept skeleton for {file}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut from_base = self.base.skeletons_as::<T>(&base_slots)?.into_iter();
        files
            .iter()
            .map(|file| match self.overlay_file(file) {
                Some(kept) => Ok(from_overlay(&kept.skeleton)),
                None => from_base
                    .next()
                    .ok_or_else(|| format!("no kept skeleton for {file}")),
            })
            .collect()
    }

    /// The files that read any of `keys`, by name.
    fn readers(&self, keys: &[(Lookup, String)]) -> Result<BTreeSet<String>, String> {
        let readers = self.base.readers()?;
        let mut overlay_reads = HashMap::<u64, Vec<&str>>::new();
        if let Some(overlay) = &self.overlay {
            for (file, kept) in &overlay.files {
                for hash in &kept.reads {
                    overlay_reads.entry(*hash).or_default().push(file);
                }
            }
        }
        let mut files = BTreeSet::new();
        for (lookup, key) in keys {
            let hash = kept::read_hash(*lookup, key);
            for slot in readers.of(hash) {
                let file = &self.base_scope[*slot as usize];
                // A file the overlay worked out again read what its own
                // reads say, not what its base block's did.
                if self.overlay_file(file).is_none() {
                    files.insert(file.clone());
                }
            }
            for file in overlay_reads.get(&hash).into_iter().flatten() {
                files.insert((*file).to_string());
            }
        }
        Ok(files)
    }
}

/// The parts of a kept skeleton the per-file indexes derive from (see
/// [`per_file_patched`]): decoding only these keeps a patch that reads many
/// files' index entries from holding every such file's whole skeleton.
#[derive(Deserialize)]
struct IndexSkeleton {
    file: String,
    exports: Vec<super::ExportContribution>,
    #[serde(default)]
    rust_api: Option<IndexRustFacts>,
    #[serde(default)]
    rust_imports: Vec<super::RawImportContribution>,
}

#[derive(Deserialize)]
struct IndexRustFacts {
    #[serde(default)]
    initializers: Vec<super::RustInitializer>,
    items: Vec<RustApiItem>,
}

impl IndexSkeleton {
    fn into_contribution(self) -> DeadCodeContribution {
        DeadCodeContribution {
            file: self.file,
            exports: self.exports,
            rust_api: self.rust_api.map(|facts| RustApiFacts {
                initializers: facts.initializers,
                items: facts.items,
                ..Default::default()
            }),
            rust_imports: self.rust_imports,
            ..Default::default()
        }
    }
}

/// The base's skeletons as the per-file indexes read them, each decoded when
/// first asked for.
struct BaseSkeletons {
    base: Arc<Base>,
    cells: Vec<OnceLock<Result<Arc<DeadCodeContribution>, String>>>,
    /// How many skeletons were decoded, and how many bytes were read from
    /// the base for them, for profiling.
    decoded: std::sync::atomic::AtomicUsize,
    decoded_bytes: std::sync::atomic::AtomicUsize,
}

impl BaseSkeletons {
    fn new(base: &Arc<Base>, files: usize) -> Arc<Self> {
        Arc::new(Self {
            base: Arc::clone(base),
            cells: (0..files).map(|_| OnceLock::new()).collect(),
            decoded: std::sync::atomic::AtomicUsize::new(0),
            decoded_bytes: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn get(&self, slot: usize) -> Result<Arc<DeadCodeContribution>, String> {
        self.cells
            .get(slot)
            .ok_or_else(|| format!("no kept skeleton {slot}"))?
            .get_or_init(|| {
                let bytes = self.base.skeleton_bytes(slot)?;
                self.decoded
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.decoded_bytes
                    .fetch_add(bytes.len(), std::sync::atomic::Ordering::Relaxed);
                serde_json::from_slice::<IndexSkeleton>(&bytes)
                    .map(|skeleton| Arc::new(skeleton.into_contribution()))
                    .map_err(|error| format!("kept skeleton {slot}: {error}"))
            })
            .clone()
    }

    /// Adds how many skeletons were decoded, and their bytes, to `timings`.
    fn profile(&self, timings: &mut Vec<(&'static str, f64)>) {
        use std::sync::atomic::Ordering::Relaxed;
        timings.push(("kept skeletons decoded", self.decoded.load(Relaxed) as f64));
        timings.push((
            "kept skeleton bytes decoded",
            self.decoded_bytes.load(Relaxed) as f64,
        ));
    }
}

/// A per-file index derived by `derive` from each file's skeleton: the
/// changed and overlay files' values in memory, every other file's read
/// from the base when asked for. `new_values` are the touched files' new
/// values; `overlay` the overlay's files with their skeletons; `stale` the
/// base files whose base values no longer hold.
fn per_file_patched<V: Send + Sync + 'static>(
    kept: &Kept,
    skeletons: &Arc<BaseSkeletons>,
    derive: fn(&DeadCodeContribution) -> Option<V>,
    new_values: BTreeMap<String, Option<V>>,
    stale: &BTreeSet<String>,
) -> Index<V> {
    let mut memory = BTreeMap::new();
    if let Some(overlay) = &kept.overlay {
        for (file, kept_file) in &overlay.files {
            if !new_values.contains_key(file) && !stale.contains(file) {
                if let Some(value) = derive(&kept_file.skeleton) {
                    memory.insert(file.clone(), value);
                }
            }
        }
    }
    let mut removed = stale.clone();
    for (file, value) in new_values {
        removed.insert(file.clone());
        if let Some(value) = value {
            memory.insert(file, value);
        }
    }
    if let Some(overlay) = &kept.overlay {
        removed.extend(overlay.files.keys().cloned());
    }
    let slots = Arc::clone(&kept.base_slots);
    let names = Arc::clone(&kept.base_scope);
    let skeletons = Arc::clone(skeletons);
    Index::patched(
        memory,
        removed,
        KeptEntries::new(
            names.len(),
            Box::new(move |key| slots.get(key).copied()),
            Box::new(move |slot| {
                let skeleton = skeletons.get(slot)?;
                Ok(derive(&skeleton)
                    .map(|value| vec![(names[slot].clone(), value)])
                    .unwrap_or_default())
            }),
        ),
    )
}

/// A table index: `changes` (the overlay's, then this patch's) over the
/// base's table at `spans`.
fn table_patched<V: DeserializeOwned + Send + Sync + 'static>(
    base: &Arc<Base>,
    spans: TableSpans,
    changes: &BTreeMap<String, Option<V>>,
) -> Result<Index<V>, String>
where
    V: Clone,
{
    let table = Arc::new(base.table(spans)?);
    let (slots, loads, base) = (Arc::clone(&table), table, Arc::clone(base));
    let mut memory = BTreeMap::new();
    let mut removed = BTreeSet::new();
    for (key, value) in changes {
        match value {
            Some(value) => {
                memory.insert(key.clone(), value.clone());
            }
            None => {
                removed.insert(key.clone());
            }
        }
    }
    Ok(Index::patched(
        memory,
        removed,
        KeptEntries::new(
            slots.len(),
            Box::new(move |key| slots.slot(key)),
            Box::new(move |slot| loads.load(&base, slot)),
        ),
    ))
}

/// What an index differs from the base by, as overlay entries.
fn index_changes<V: Clone>(index: &Index<V>) -> BTreeMap<String, Option<V>> {
    let (memory, removed) = index.changes();
    let mut changes = removed
        .into_iter()
        .flatten()
        .map(|key| (key.clone(), None))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in memory {
        changes.insert(key.clone(), Some(value.clone()));
    }
    changes
}

/// What an analysis worked out again differs from the base's, by the
/// base's digests, as overlay entries.
fn analysis_changes<V: Clone + std::fmt::Debug>(
    entries: &BTreeMap<String, V>,
    base_digests: &BTreeMap<String, u64>,
) -> BTreeMap<String, Option<V>> {
    let mut changes = base_digests
        .keys()
        .filter(|key| !entries.contains_key(*key))
        .map(|key| (key.clone(), None))
        .collect::<BTreeMap<_, _>>();
    for (key, value) in entries {
        if base_digests.get(key) != Some(&digest(value)) {
            changes.insert(key.clone(), Some(value.clone()));
        }
    }
    changes
}

/// A skeleton read for its Oxc facts only.
#[derive(Deserialize)]
struct OxcSkeleton {
    file: String,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
}

/// A skeleton read for the Rust public-API analysis's inputs only.
#[derive(Deserialize)]
struct ApiSkeleton {
    file: String,
    #[serde(default)]
    rust_api: Option<ApiFacts>,
}

#[derive(Deserialize)]
struct ApiFacts {
    items: Vec<RustApiItem>,
    modules: Vec<RustApiModule>,
    uses: Vec<RustApiUse>,
}

/// A skeleton read for its Go dispatch facts only.
#[derive(Deserialize)]
struct GoSkeleton {
    file: String,
    #[serde(default)]
    go_dispatch: Option<GoDispatchFacts>,
}

/// The Oxc facts of a skeleton, without the content hash no verdict reads.
fn oxc_inputs(skeleton: Option<&DeadCodeContribution>) -> Option<u64> {
    let facts = skeleton?.oxc_facts.as_ref()?;
    Some(digest(&(
        facts.format_version,
        &facts.exports,
        &facts.imports,
        &facts.re_exports,
        &facts.dynamic_imports,
        &facts.same_file_value_references,
        &facts.used_import_bindings,
        &facts.type_referenced_import_bindings,
        &facts.value_referenced_import_bindings,
        &facts.parse_error,
    )))
}

/// The Rust public-API analysis's inputs of a skeleton.
fn api_inputs(skeleton: Option<&DeadCodeContribution>) -> Option<u64> {
    let facts = skeleton?.rust_api.as_ref()?;
    Some(digest(&(&facts.items, &facts.modules, &facts.uses)))
}

/// The Go dispatch analysis's inputs of a skeleton.
fn go_inputs(skeleton: Option<&DeadCodeContribution>) -> Option<u64> {
    skeleton?.go_dispatch.as_ref().map(digest)
}

/// Whether the resolver configuration files `config` names still hash as
/// they did.
fn config_unchanged(config: &[(PathBuf, String)]) -> bool {
    config.iter().all(|(path, hash)| {
        crate::project_content::read(path)
            .is_ok_and(|bytes| blake3::hash(&bytes).to_hex().to_string() == *hash)
    })
}

/// Stored facts a test damages: (project root, project-relative file).
#[cfg(test)]
pub(crate) static DAMAGED_FACTS_FOR_TEST: std::sync::Mutex<Vec<(PathBuf, String)>> =
    std::sync::Mutex::new(Vec::new());

/// The stored facts of `relative_file` in the project at `root`, as every
/// rollup reads them.
fn stored_facts_of(
    root: &Path,
    files: &crate::data_layer::checkout::ViewGraphFiles<'_>,
    relative_file: &str,
) -> Option<Result<Vec<u8>, String>> {
    if facts_damaged_for_test(root, relative_file) {
        return Some(Ok(b"damaged facts".to_vec()));
    }
    stored_facts(files, relative_file)
}

/// Whether a test damaged `relative_file`'s stored facts in the project at
/// `root`.
#[cfg(test)]
fn facts_damaged_for_test(root: &Path, relative_file: &str) -> bool {
    DAMAGED_FACTS_FOR_TEST.lock().is_ok_and(|damaged| {
        damaged
            .iter()
            .any(|(damaged_root, file)| damaged_root.as_path() == root && file == relative_file)
    })
}

#[cfg(not(test))]
fn facts_damaged_for_test(_root: &Path, _relative_file: &str) -> bool {
    false
}

impl Rollup<'_, '_> {
    /// The facts of `files` (project-relative), read from their content
    /// records with the Oxc engine's facts for those it analyzes. `None`
    /// when the Oxc engine skipped a file outside the project (whose list
    /// every analyzed file carries) or a file's facts could not be decoded.
    fn facts_of(
        &self,
        files: &BTreeSet<String>,
    ) -> Result<Option<BTreeMap<String, DeadCodeContribution>>, String> {
        let mut job = self.job.clone();
        job.scope_files = self
            .job
            .scope_files
            .iter()
            .filter(|file| files.contains(&relative_path(self.root, file)))
            .cloned()
            .collect();
        if job.scope_files.is_empty() {
            return Ok(Some(BTreeMap::new()));
        }
        let oxc_result = (self.oxc)(&job.scope_files)?;
        if oxc_result
            .as_ref()
            .is_some_and(|result| !result.skipped_outside_root.is_empty())
        {
            return Ok(None);
        }
        // A file's exports come from the call-graph files whose file-level
        // facts are filed under it, as a full rollup takes them from every
        // file's.
        let exported_symbols = files
            .iter()
            .filter_map(|file| self.source.graph_paths(file).1)
            .flatten()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .flat_map(|path| self.graph.exported_symbols(path))
            .collect::<Vec<_>>();
        let lookup = |relative_file: &str| stored_facts_of(self.root, self.files, relative_file);
        let contributions = super::scan_dead_code_facts_stored(
            &job,
            oxc_result.as_ref(),
            &exported_symbols,
            &lookup,
        )?
        .ok_or_else(|| "dead-code scan cancelled".to_string())?
        .0;
        let parsed = parse_dead_code_contributions(self.root, &contributions);
        if parsed.iter().any(|contribution| contribution.decode_failed) {
            return Ok(None);
        }
        Ok(Some(
            parsed
                .into_iter()
                .map(|contribution| (contribution.file.clone(), contribution))
                .collect(),
        ))
    }

    /// The whole-project facts of this rollup that do not come from files'
    /// skeletons.
    fn base_state(&self, default_exports: BTreeMap<String, String>) -> State {
        let (graph_calls, graph_facts) = self.source.all_graph_paths();
        State {
            config: self.config.clone(),
            exact: self.exact.clone(),
            scope: self.scope.clone(),
            identities: self.identities.clone(),
            others: self.others.clone(),
            default_exports,
            liveness_root_files: self
                .snapshot
                .entry_points
                .iter()
                .map(|file| relative_path(self.root, file))
                .collect(),
            executable_roots: super::executable_root_exports_by_file(self.root, &self.entries),
            public_api_files: self.public_api_files.clone(),
            graph_calls,
            graph_facts,
            ..State::default()
        }
    }

    /// Reads the kept graph `kept`, which was worked out from the same
    /// inputs as this rollup.
    fn read(
        &self,
        kept: Kept,
        roles: &ProjectRoles,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Result<Value, String> {
        let state = kept.state();
        let totals = Totals::new(
            self.scope.len(),
            state.type_ref_counts.keys().cloned().collect(),
            state.test_module_files(),
        );
        let mut assembler = Assembler::new(totals);
        assembler.strings = timed(timings, "read kept strings", || {
            kept.strings()
                .map(|(strings, _)| Strings::from_all(strings))
        })?;
        let files = self.scope.iter().map(String::as_str).collect::<Vec<_>>();
        timed(timings, "read and add kept blocks", || {
            for chunk in files.chunks(BLOCK_CHUNK) {
                for block in kept.blocks(chunk)? {
                    assembler.add(block);
                }
            }
            Ok::<_, String>(())
        })?;
        assembler.other_roots = state.other_roots.clone();
        Ok(assembler.finish(self.root, self.public_api_files, roles, timings))
    }

    /// Works out every file; keeps the graph at `keep` when given.
    fn full(
        &self,
        keep: Option<PathBuf>,
        roles: &ProjectRoles,
        timings: &mut Vec<(&'static str, f64)>,
        mut probe: Option<&mut Probe>,
    ) -> Result<Value, String> {
        let root = self.root;
        // Every file is projected, so every file is read in full.
        self.graph.compact_targets(false);
        let snapshot = timed(timings, "read every file's symbols", || {
            with_exports(self.snapshot, self.graph)
        });
        if let Some(failure) = self.graph.failure() {
            return Err(failure);
        }
        let oxc_result = timed(timings, "Oxc analysis", || {
            (self.oxc)(&self.job.scope_files)
        })?;
        // Every analyzed file carries the list of files the Oxc engine
        // skipped outside the project; a patch cannot keep it current.
        let keep = keep.filter(|_| {
            oxc_result
                .as_ref()
                .is_none_or(|result| result.skipped_outside_root.is_empty())
        });
        let contributions = timed(timings, "read stored facts", || {
            let lookup =
                |relative_file: &str| stored_facts_of(self.root, self.files, relative_file);
            super::scan_dead_code_facts_stored(
                self.job,
                oxc_result.as_ref(),
                &snapshot.exported_symbols,
                &lookup,
            )
        })?
        .ok_or_else(|| "dead-code scan cancelled".to_string())?
        .0;
        drop(oxc_result);
        let parsed = timed(timings, "decode facts", || {
            parse_dead_code_contributions(root, &contributions)
        });
        drop(contributions);
        let graph_facts = GraphFileFacts::from_snapshot(root, &snapshot);
        let default_exports = graph_facts.default_export_symbols_by_file.clone();
        drop(snapshot);
        let mut indexes = timed(timings, "index facts and Oxc verdicts", || {
            ContextIndexes::build(
                root,
                graph_facts,
                &parsed,
                self.public_api_files,
                &self.entries,
                true,
            )
        });
        let oxc_config = std::mem::take(&mut indexes.oxc_config);
        let context = MaterializeContext::with_indexes(root, self.public_api_files, indexes);
        let totals = Totals::of(&parsed, &context);
        // What the kept graph's state counts and lists, from every file's
        // facts before they are materialized.
        let mut state = self.base_state(default_exports);
        let mut writer = keep
            .as_deref()
            .and_then(|path| kept::BaseWriter::create(path, &self.config));
        if writer.is_some() {
            let mut flipped = BTreeSet::new();
            for contribution in &parsed {
                count(
                    &mut state.test_module_counts,
                    &contribution.cfg_test_module_files,
                    true,
                    &mut flipped,
                );
                count(
                    &mut state.receiver_counts,
                    receivers(contribution),
                    true,
                    &mut flipped,
                );
                count(
                    &mut state.type_ref_counts,
                    &contribution.type_ref_names,
                    true,
                    &mut flipped,
                );
                if let Some(input) = module_input(contribution) {
                    state.module_inputs.insert(contribution.file.clone(), input);
                }
                if contribution.oxc_facts.is_some() {
                    state.oxc_files.insert(contribution.file.clone());
                }
                if contribution.rust_api.is_some() {
                    state.rust_api_files.insert(contribution.file.clone());
                }
                if contribution.go_dispatch.is_some() {
                    state.go_files.insert(contribution.file.clone());
                }
            }
            state.go_packages = self.graph.go_packages().clone();
            state.oxc_config = oxc_config;
            state.oxc_digests = digests(&context.oxc_by_file.all().entries());
            state.rust_public_digests = digests(&context.rust_public_items.all().entries());
            state.go_live_digests = digests(&context.go_live_methods.all().entries());
        }
        let mut assembler = Assembler::new(totals);
        let started = std::time::Instant::now();
        let record = writer.is_some();
        let details = probe.as_ref().is_some_and(|probe| probe.details);
        let consumed = materialize_each(&context, &self.source, parsed, record, &mut |file| {
            let MaterializedFile {
                block,
                reads,
                skeleton,
            } = file;
            let file = block.file().to_string();
            let encoded = assembler.encode(block);
            if details {
                if let Some(probe) = probe.as_deref_mut() {
                    probe
                        .blocks
                        .insert(file, canonical(&encoded, &assembler.strings));
                }
            }
            if let Some(writer) = &mut writer {
                let skeleton = serde_json::to_vec(&skeleton).map_err(|error| error.to_string())?;
                writer.file(
                    &encoded.calls,
                    &encoded.roots,
                    &encoded.block,
                    &skeleton,
                    read_hashes(&reads),
                );
            }
            assembler.add(encoded);
            Ok(())
        })?;
        timings.push((
            "project and materialize files",
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        if let Some(mib) = resident_mib() {
            timings.push(("resident MiB", mib));
        }
        let roots = timed(timings, "other files' receivers", || {
            other_roots(&context, &self.source, &consumed)
        })?;
        assembler.other_roots = assembler.encode_roots(roots);
        // A file whose facts could not be decoded is a gap of this rollup
        // only; keeping the graph would keep the gap.
        if let (Some(mut writer), false) = (writer, assembler.decode_failed) {
            let started = std::time::Instant::now();
            state.other_roots = assembler.other_roots.clone();
            let files_by_symbol = writer.table(context.files_by_exported_symbol.all().entries());
            let oxc = writer.table(context.oxc_by_file.all().entries());
            let rust_public = writer.table(context.rust_public_items.all().entries());
            let go_live = writer.table(context.go_live_methods.all().entries());
            match writer.finish(
                state,
                &assembler.strings.all,
                files_by_symbol,
                oxc,
                rust_public,
                go_live,
            ) {
                Ok(bytes) => {
                    if let Some(probe) = probe.as_deref_mut() {
                        probe.kept_bytes = Some(bytes);
                    }
                    if let (Some(keep), Some(dir)) =
                        (keep.as_deref(), keep.as_deref().and_then(Path::parent))
                    {
                        kept::prune(dir, keep);
                    }
                }
                Err(error) => crate::slog_warn!(
                    "tier2 dead_code: could not keep the dead-code graph: {error}"
                ),
            }
            timings.push(("keep the graph", started.elapsed().as_secs_f64() * 1000.0));
        }
        if let Some(probe) = probe {
            probe.path = "full";
            probe.changed = self.scope.len();
            probe.materialized = self.scope.len();
            if probe.details {
                probe.indexes = context.index_digests();
                probe.indexes.push((
                    "referenced_type_names",
                    digest(assembler.totals.referenced_type_names()),
                ));
            }
        }
        Ok(assembler.finish(root, self.public_api_files, roles, timings))
    }
}

/// The export symbols of a skeleton.
fn export_symbols(skeleton: Option<&DeadCodeContribution>) -> BTreeSet<String> {
    skeleton
        .into_iter()
        .flat_map(|skeleton| skeleton.exports.iter().map(|export| export.symbol.clone()))
        .collect()
}

/// The keys of `map` whose presence flipped between `old` and `new` among
/// `candidates`.
fn flipped<'k>(
    old: &BTreeMap<String, u32>,
    new: &BTreeMap<String, u32>,
    candidates: impl IntoIterator<Item = &'k String>,
) -> BTreeSet<String> {
    candidates
        .into_iter()
        .filter(|key| old.contains_key(*key) != new.contains_key(*key))
        .cloned()
        .collect()
}

/// A file a patched rollup materialized again.
struct Redone {
    calls: Vec<u32>,
    roots: Vec<u32>,
    block: super::engine::FileBlock,
    skeleton: DeadCodeContribution,
    reads: Vec<u64>,
}

impl Rollup<'_, '_> {
    /// Patches the kept graph `kept` for what changed since, and keeps the
    /// result in `dir`: an overlay over the same base, or a new base when
    /// the overlay would grow large. `None` when the patch cannot account
    /// for the change and every file must be worked out instead.
    #[allow(clippy::too_many_lines)]
    fn patch(
        &self,
        mut kept: Kept,
        dir: &Path,
        roles: &ProjectRoles,
        timings: &mut Vec<(&'static str, f64)>,
        mut probe: Option<&mut Probe>,
    ) -> Result<Option<Value>, String> {
        let root = self.root;
        let force = probe.as_ref().is_some_and(|probe| probe.force_patch);
        let too_many = |files: usize| !force && too_many(files, self.scope.len());
        // Only a few files are projected; the many files they call into are
        // read for their lookups alone.
        self.graph.compact_targets(true);
        let started = std::time::Instant::now();
        let old = kept.state();

        // The call-graph files whose facts changed, and the scanned files
        // they, the file's own content, or its filing under call-graph
        // files change.
        let mut changed_paths = BTreeSet::new();
        for (path, identity) in &self.identities {
            let same = match old.identities.get(path) {
                Some(kept_identity) => same_identity(self.files, path, kept_identity, identity)?,
                None => false,
            };
            if !same {
                changed_paths.insert(path.clone());
            }
        }
        changed_paths.extend(
            old.identities
                .keys()
                .filter(|path| !self.identities.contains_key(*path))
                .cloned(),
        );
        let (graph_calls, graph_facts) = self.source.all_graph_paths();
        let old_scope = old.scope.iter().map(String::as_str).collect::<HashSet<_>>();
        let scope = self
            .scope
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut changed = BTreeSet::new();
        for file in &self.scope {
            let (calls, facts) = self.source.graph_paths(file);
            let graph_changed = calls
                .into_iter()
                .chain(facts)
                .flatten()
                .any(|path| changed_paths.contains(path));
            if !old_scope.contains(file.as_str())
                || self.others.get(file) != old.others.get(file)
                || graph_changed
                || graph_calls.get(file) != old.graph_calls.get(file)
                || graph_facts.get(file) != old.graph_facts.get(file)
            {
                changed.insert(file.clone());
            }
        }
        let removed = old
            .scope
            .iter()
            .filter(|file| !scope.contains(file.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();
        if too_many(changed.len()) {
            return Ok(None);
        }
        let Some(mut fresh) = timed(timings, "read changed files' facts", || {
            self.facts_of(&changed)
        })?
        else {
            return Ok(None);
        };
        let touched = changed.iter().chain(&removed).cloned().collect::<Vec<_>>();
        let mut old_skeletons = BTreeMap::new();
        let mut new_skeletons = BTreeMap::new();
        for file in &touched {
            old_skeletons.insert(file.clone(), kept.skeleton(file, &old_scope)?);
            let new = match fresh.get(file) {
                Some(contribution) => Some(contribution.index_skeleton()),
                None if changed.contains(file) => {
                    return Err(format!("no facts read for {file}"));
                }
                None => None,
            };
            new_skeletons.insert(file.clone(), new);
        }
        let old_of = |file: &str| old_skeletons.get(file).and_then(Option::as_ref);
        let new_of = |file: &str| new_skeletons.get(file).and_then(Option::as_ref);
        timings.push((
            "find changed files",
            started.elapsed().as_secs_f64() * 1000.0,
        ));

        // Each whole-project index brought up to date, and the entries that
        // changed.
        let started = std::time::Instant::now();
        let mut keys = Vec::<(Lookup, String)>::new();
        keys.extend(
            changed_paths
                .iter()
                .map(|path| (Lookup::GraphFile, path.clone())),
        );
        for file in map_changes(&old.graph_calls, &graph_calls)
            .chain(map_changes(&old.graph_facts, &graph_facts))
        {
            keys.push((Lookup::GraphPaths, file.clone()));
        }
        let go_packages = self
            .graph
            .patch_go_packages(old.go_packages.clone(), &changed_paths);
        for dir in old.go_packages.keys().chain(go_packages.keys()) {
            if old.go_packages.get(dir) != go_packages.get(dir) {
                keys.push((Lookup::GoPackage, dir.to_string_lossy().into_owned()));
            }
        }

        // The indexes of one entry per file.
        let stale = kept
            .base_scope
            .iter()
            .filter(|file| !scope.contains(file.as_str()))
            .cloned()
            .collect::<BTreeSet<_>>();
        let base_skeletons = BaseSkeletons::new(&kept.base, kept.base_scope.len());
        macro_rules! per_file {
            ($derive:expr, $lookup:expr) => {{
                let mut values = BTreeMap::new();
                for file in &touched {
                    let new = new_of(file).and_then($derive);
                    if digest(&old_of(file).and_then($derive)) != digest(&new) {
                        keys.push(($lookup, file.clone()));
                    }
                    values.insert(file.clone(), new);
                }
                per_file_patched(&kept, &base_skeletons, $derive, values, &stale)
            }};
        }
        let exported_symbols_by_file =
            per_file!(file_exported_symbols, Lookup::ExportedSymbolsByFile);
        let rust_items_by_file = per_file!(file_rust_items, Lookup::RustItemsByFile);
        for file in &touched {
            let old_items = old_of(file).and_then(file_rust_items).unwrap_or_default();
            let new_items = new_of(file).and_then(file_rust_items).unwrap_or_default();
            for name in rust_item_names_changed(&old_items, &new_items) {
                keys.push((Lookup::RustItemsNamed, pair_key(file, name)));
            }
        }
        let rust_imports_by_file = per_file!(file_rust_imports, Lookup::RustImportsByFile);
        let rust_type_declarations = {
            let mut values = BTreeMap::new();
            for file in &touched {
                let old_set = old_of(file)
                    .and_then(file_rust_type_declarations)
                    .unwrap_or_default();
                let new = new_of(file).and_then(file_rust_type_declarations);
                for symbol in old_set.symmetric_difference(new.as_ref().unwrap_or(&BTreeSet::new()))
                {
                    keys.push((Lookup::RustTypeDeclarations, pair_key(file, symbol)));
                }
                values.insert(file.clone(), new);
            }
            per_file_patched(
                &kept,
                &base_skeletons,
                file_rust_type_declarations,
                values,
                &stale,
            )
        };

        // The files exporting each symbol.
        let overlay_files_by_symbol = kept
            .overlay
            .as_ref()
            .map(|overlay| overlay.files_by_symbol.clone())
            .unwrap_or_default();
        let before = table_patched(
            &kept.base,
            kept.base.directory.files_by_symbol,
            &overlay_files_by_symbol,
        )?;
        let mut updates = BTreeMap::<String, BTreeSet<String>>::new();
        for file in &touched {
            let (old_symbols, new_symbols) =
                (export_symbols(old_of(file)), export_symbols(new_of(file)));
            for symbol in old_symbols.symmetric_difference(&new_symbols) {
                let files = updates
                    .entry(symbol.clone())
                    .or_insert_with(|| before.peek(symbol).cloned().unwrap_or_default());
                if new_symbols.contains(symbol) {
                    files.insert(file.clone());
                } else {
                    files.remove(file);
                }
            }
        }
        if let Some(failure) = before.failure() {
            return Err(failure);
        }
        drop(before);
        let mut files_by_symbol_changes = overlay_files_by_symbol;
        for (symbol, files) in updates {
            keys.push((Lookup::FilesByExportedSymbol, symbol.clone()));
            files_by_symbol_changes.insert(symbol, (!files.is_empty()).then_some(files));
        }
        let files_by_exported_symbol = table_patched(
            &kept.base,
            kept.base.directory.files_by_symbol,
            &files_by_symbol_changes,
        )?;

        // Sets over every file, kept as how many files name each member.
        let mut test_module_counts = old.test_module_counts.clone();
        let mut receiver_counts = old.receiver_counts.clone();
        let mut type_ref_counts = old.type_ref_counts.clone();
        let mut candidates = (BTreeSet::new(), BTreeSet::new());
        let mut ignored = BTreeSet::new();
        for file in &touched {
            for (skeleton, add) in [(old_of(file), false), (new_of(file), true)] {
                let Some(skeleton) = skeleton else {
                    continue;
                };
                count(
                    &mut test_module_counts,
                    &skeleton.cfg_test_module_files,
                    add,
                    &mut candidates.0,
                );
                count(
                    &mut receiver_counts,
                    receivers(skeleton),
                    add,
                    &mut candidates.1,
                );
                count(
                    &mut type_ref_counts,
                    &skeleton.type_ref_names,
                    add,
                    &mut ignored,
                );
            }
        }
        for file in flipped(&old.test_module_counts, &test_module_counts, &candidates.0) {
            keys.push((Lookup::TestModuleFiles, file));
        }
        for name in flipped(&old.receiver_counts, &receiver_counts, &candidates.1) {
            keys.push((Lookup::RustUsedReceivers, name));
        }

        // Rust module paths.
        let mut module_inputs = old.module_inputs.clone();
        for file in &touched {
            module_inputs.remove(file);
            if let Some(input) = new_of(file).and_then(module_input) {
                module_inputs.insert(file.clone(), input);
            }
        }
        let old_modules = module_index(&old.scope, &old.module_inputs);
        let rust_modules = module_index(&self.scope, &module_inputs);
        module_changes(&old_modules, &rust_modules, &mut keys);
        drop(old_modules);

        // Facts of the project and of whole files.
        let mut default_exports = old.default_exports.clone();
        for path in &changed_paths {
            let file = relative_path(root, &self.graph.resolve(path));
            default_exports.remove(&file);
            for export in self.graph.exported_symbols(path) {
                if export.kind == super::DEFAULT_EXPORT_MARKER_KIND {
                    default_exports.insert(file.clone(), export.symbol);
                }
            }
        }
        for file in map_changes(&old.default_exports, &default_exports) {
            keys.push((Lookup::DefaultExportByFile, file.clone()));
        }
        let mut state = self.base_state(default_exports);
        for file in state
            .liveness_root_files
            .symmetric_difference(&old.liveness_root_files)
        {
            keys.push((Lookup::LivenessRootFiles, file.clone()));
        }
        for file in map_changes(&old.executable_roots, &state.executable_roots) {
            keys.push((Lookup::ExecutableRootExports, file.clone()));
        }
        for file in state
            .public_api_files
            .symmetric_difference(&old.public_api_files)
        {
            keys.push((Lookup::PublicApiFiles, file.clone()));
        }
        let roots_changed = state.liveness_root_files != old.liveness_root_files
            || state.executable_roots != old.executable_roots
            || state.public_api_files != old.public_api_files;
        let files_added_or_removed = !removed.is_empty()
            || changed
                .iter()
                .any(|file| !old_scope.contains(file.as_str()));

        // The analyses over many files, worked out again from every file's
        // inputs when any of their inputs changed.
        let update = |set: &BTreeSet<String>, has: fn(&DeadCodeContribution) -> bool| {
            let mut set = set.clone();
            for file in &touched {
                set.remove(file);
                if new_of(file).is_some_and(has) {
                    set.insert(file.clone());
                }
            }
            set
        };
        state.oxc_files = update(&old.oxc_files, |skeleton| skeleton.oxc_facts.is_some());
        state.rust_api_files = update(&old.rust_api_files, |skeleton| skeleton.rust_api.is_some());
        state.go_files = update(&old.go_files, |skeleton| skeleton.go_dispatch.is_some());
        let inputs_changed = |inputs: fn(Option<&DeadCodeContribution>) -> Option<u64>| {
            touched
                .iter()
                .any(|file| inputs(old_of(file)) != inputs(new_of(file)))
        };
        // Every file's inputs to an analysis, in scan order: from the
        // changed files' new skeletons, else from the kept graph.
        let inputs_of = |files: &BTreeSet<String>| -> Vec<&String> {
            self.scope
                .iter()
                .filter(|file| files.contains(*file))
                .collect()
        };
        let base_digests = &kept.base.directory.state;
        let overlay = kept.overlay.as_ref();
        let (oxc_by_file, oxc_changes) = if inputs_changed(oxc_inputs)
            || roots_changed
            || files_added_or_removed
            || !config_unchanged(&old.oxc_config)
        {
            let files = inputs_of(&state.oxc_files);
            let untouched = files
                .iter()
                .filter(|file| !new_skeletons.contains_key(**file))
                .copied()
                .collect::<Vec<_>>();
            let mut kept_inputs = kept
                .skeletons_as::<OxcSkeleton>(&untouched, |skeleton| OxcSkeleton {
                    file: skeleton.file.clone(),
                    oxc_facts: skeleton.oxc_facts.clone(),
                })?
                .into_iter();
            let contributions = files
                .iter()
                .map(|file| match new_of(file) {
                    Some(skeleton) => DeadCodeContribution {
                        file: skeleton.file.clone(),
                        oxc_facts: skeleton.oxc_facts.clone(),
                        ..Default::default()
                    },
                    None => {
                        let kept = kept_inputs.next().unwrap_or(OxcSkeleton {
                            file: String::new(),
                            oxc_facts: None,
                        });
                        DeadCodeContribution {
                            file: kept.file,
                            oxc_facts: kept.oxc_facts,
                            ..Default::default()
                        }
                    }
                })
                .collect::<Vec<_>>();
            let (verdicts, config) = oxc_verdicts_by_file(
                root,
                &self.snapshot.entry_points,
                &contributions,
                self.public_api_files,
                &self.entries,
            );
            state.oxc_config = config;
            state.oxc_digests = digests(&verdicts);
            let changes = analysis_changes(&verdicts, &base_digests.oxc_digests);
            (Index::memory(verdicts), changes)
        } else {
            state.oxc_config = old.oxc_config.clone();
            state.oxc_digests = old.oxc_digests.clone();
            let changes = overlay
                .map(|overlay| overlay.oxc.clone())
                .unwrap_or_default();
            (
                table_patched(&kept.base, kept.base.directory.oxc, &changes)?,
                changes,
            )
        };
        for key in old.oxc_digests.keys().chain(state.oxc_digests.keys()) {
            if old.oxc_digests.get(key) != state.oxc_digests.get(key) {
                keys.push((Lookup::OxcByFile, key.clone()));
            }
        }
        let (rust_public_items, rust_public_changes) =
            if inputs_changed(api_inputs) || state.public_api_files != old.public_api_files {
                let files = inputs_of(&state.rust_api_files);
                let untouched = files
                    .iter()
                    .filter(|file| !new_skeletons.contains_key(**file))
                    .copied()
                    .collect::<Vec<_>>();
                let mut kept_inputs = kept
                    .skeletons_as::<ApiSkeleton>(&untouched, |skeleton| ApiSkeleton {
                        file: skeleton.file.clone(),
                        rust_api: skeleton.rust_api.as_ref().map(|facts| ApiFacts {
                            items: facts.items.clone(),
                            modules: facts.modules.clone(),
                            uses: facts.uses.clone(),
                        }),
                    })?
                    .into_iter();
                let contributions = files
                    .iter()
                    .map(|file| match new_of(file) {
                        Some(skeleton) => DeadCodeContribution {
                            file: skeleton.file.clone(),
                            rust_api: skeleton.rust_api.clone(),
                            ..Default::default()
                        },
                        None => {
                            let kept = kept_inputs.next().unwrap_or(ApiSkeleton {
                                file: String::new(),
                                rust_api: None,
                            });
                            DeadCodeContribution {
                                file: kept.file,
                                rust_api: kept.rust_api.map(|facts| RustApiFacts {
                                    items: facts.items,
                                    modules: facts.modules,
                                    uses: facts.uses,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }
                        }
                    })
                    .collect::<Vec<_>>();
                let items = rust_public_api_items(&contributions, self.public_api_files);
                state.rust_public_digests = digests(&items);
                let changes = analysis_changes(&items, &base_digests.rust_public_digests);
                (Index::memory(items), changes)
            } else {
                state.rust_public_digests = old.rust_public_digests.clone();
                let changes = overlay
                    .map(|overlay| overlay.rust_public.clone())
                    .unwrap_or_default();
                (
                    table_patched(&kept.base, kept.base.directory.rust_public, &changes)?,
                    changes,
                )
            };
        for key in old
            .rust_public_digests
            .keys()
            .chain(state.rust_public_digests.keys())
        {
            if old.rust_public_digests.get(key) != state.rust_public_digests.get(key) {
                keys.push((Lookup::RustPublicItems, key.clone()));
            }
        }
        let (go_live_methods, go_live_changes) = if inputs_changed(go_inputs) {
            let files = inputs_of(&state.go_files);
            let untouched = files
                .iter()
                .filter(|file| !new_skeletons.contains_key(**file))
                .copied()
                .collect::<Vec<_>>();
            let mut kept_inputs = kept
                .skeletons_as::<GoSkeleton>(&untouched, |skeleton| GoSkeleton {
                    file: skeleton.file.clone(),
                    go_dispatch: skeleton.go_dispatch.clone(),
                })?
                .into_iter();
            let contributions = files
                .iter()
                .map(|file| match new_of(file) {
                    Some(skeleton) => DeadCodeContribution {
                        file: skeleton.file.clone(),
                        go_dispatch: skeleton.go_dispatch.clone(),
                        ..Default::default()
                    },
                    None => {
                        let kept = kept_inputs.next().unwrap_or(GoSkeleton {
                            file: String::new(),
                            go_dispatch: None,
                        });
                        DeadCodeContribution {
                            file: kept.file,
                            go_dispatch: kept.go_dispatch,
                            ..Default::default()
                        }
                    }
                })
                .collect::<Vec<_>>();
            let methods = go_interface_live_methods(&contributions);
            state.go_live_digests = digests(&methods);
            let changes = analysis_changes(&methods, &base_digests.go_live_digests);
            (Index::memory(methods), changes)
        } else {
            state.go_live_digests = old.go_live_digests.clone();
            let changes = overlay
                .map(|overlay| overlay.go_live.clone())
                .unwrap_or_default();
            (
                table_patched(&kept.base, kept.base.directory.go_live, &changes)?,
                changes,
            )
        };
        for key in old
            .go_live_digests
            .keys()
            .chain(state.go_live_digests.keys())
        {
            if old.go_live_digests.get(key) != state.go_live_digests.get(key) {
                keys.push((Lookup::GoLiveMethods, key.clone()));
            }
        }
        state.test_module_counts = test_module_counts;
        state.receiver_counts = receiver_counts;
        state.type_ref_counts = type_ref_counts;
        state.module_inputs = module_inputs;
        state.go_packages = go_packages;
        let old_state_files = old.scope.len();
        let indexes = ContextIndexes {
            liveness_root_files: state.liveness_root_files.clone(),
            executable_root_exports_by_file: Index::memory(state.executable_roots.clone()),
            exported_symbols_by_file,
            files_by_exported_symbol,
            default_export_symbols_by_file: state.default_exports.clone(),
            oxc_by_file,
            test_module_files: state.test_module_files(),
            rust_public_items,
            rust_used_receivers: state.receiver_counts.keys().cloned().collect(),
            go_live_methods,
            rust_items_by_file,
            rust_modules,
            rust_type_declarations,
            rust_imports_by_file,
            oxc_config: Vec::new(),
        };
        let context = MaterializeContext::with_indexes(root, self.public_api_files, indexes);
        timings.push((
            "bring indexes up to date",
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        if let Some(mib) = resident_mib() {
            timings.push(("resident MiB", mib));
        }

        // The files that read a changed entry are materialized again too.
        let started = std::time::Instant::now();
        let readers = kept.readers(&keys)?;
        let dirty = self
            .scope
            .iter()
            .filter(|file| changed.contains(*file) || readers.contains(*file))
            .cloned()
            .collect::<BTreeSet<_>>();
        timings.push((
            "find files to redo",
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        if too_many(dirty.len()) {
            return Ok(None);
        }
        let others = dirty
            .iter()
            .filter(|file| !fresh.contains_key(*file))
            .cloned()
            .collect::<BTreeSet<_>>();
        let Some(more) = timed(timings, "read affected files' facts", || {
            self.facts_of(&others)
        })?
        else {
            return Ok(None);
        };
        fresh.extend(more);
        let started = std::time::Instant::now();
        let contributions = self
            .scope
            .iter()
            .filter(|file| dirty.contains(*file))
            .map(|file| {
                fresh
                    .remove(file)
                    .ok_or_else(|| format!("no facts read for {file}"))
            })
            .collect::<Result<Vec<_>, String>>()?;
        drop(fresh);
        let mut redone = HashMap::new();
        let mut consumed =
            materialize_each(&context, &self.source, contributions, true, &mut |file| {
                redone.insert(file.block.file().to_string(), file);
                Ok(())
            })?;
        timings.push((
            "project and materialize files",
            started.elapsed().as_secs_f64() * 1000.0,
        ));
        if let Some(mib) = resident_mib() {
            timings.push(("resident MiB", mib));
            base_skeletons.profile(timings);
        }
        if let Some(failure) = context.failure().or_else(|| self.graph.failure()) {
            return Err(failure);
        }

        // The new graph: kept blocks as they are, redone ones new.
        let started = std::time::Instant::now();
        let totals = Totals::new(
            self.scope.len(),
            state.type_ref_counts.keys().cloned().collect(),
            state.test_module_files(),
        );
        let mut assembler = Assembler::new(totals);
        let (strings, base_strings) = kept.strings()?;
        assembler.strings = Strings::from_all(strings);
        let details = probe.as_ref().is_some_and(|probe| probe.details);
        let mut redone_files = BTreeMap::new();
        for chunk in self.scope.chunks(BLOCK_CHUNK) {
            let kept_files = chunk
                .iter()
                .filter(|file| !redone.contains_key(*file))
                .map(String::as_str)
                .collect::<Vec<_>>();
            let mut kept_blocks = kept.blocks(&kept_files)?.into_iter();
            for file in chunk {
                let encoded = match redone.remove(file) {
                    Some(MaterializedFile {
                        block,
                        reads,
                        skeleton,
                    }) => {
                        let encoded = assembler.encode(block);
                        redone_files.insert(
                            file.clone(),
                            Redone {
                                calls: encoded.calls.clone(),
                                roots: encoded.roots.clone(),
                                block: encoded.block.clone(),
                                skeleton,
                                reads: read_hashes(&reads),
                            },
                        );
                        encoded
                    }
                    None => {
                        let encoded = kept_blocks
                            .next()
                            .ok_or_else(|| format!("no kept block for {file}"))?;
                        consumed.extend(encoded.block.consumed().iter().cloned());
                        encoded
                    }
                };
                if details {
                    if let Some(probe) = probe.as_deref_mut() {
                        probe
                            .blocks
                            .insert(file.clone(), canonical(&encoded, &assembler.strings));
                    }
                }
                assembler.add(encoded);
            }
        }
        let roots = other_roots(&context, &self.source, &consumed)?;
        assembler.other_roots = assembler.encode_roots(roots);
        state.other_roots = assembler.other_roots.clone();
        timings.push(("assemble", started.elapsed().as_secs_f64() * 1000.0));
        if let Some(mib) = resident_mib() {
            timings.push(("resident MiB", mib));
            assembler.profile_sizes(timings);
        }

        // Keep the result: an overlay of what differs from the base, or a
        // new base when that has grown large.
        let started = std::time::Instant::now();
        let mut overlay_files = kept
            .overlay
            .take()
            .map(|overlay| overlay.files)
            .unwrap_or_default();
        overlay_files
            .retain(|file, _| scope.contains(file.as_str()) && !redone_files.contains_key(file));
        let overlay_size = overlay_files.len() + redone_files.len();
        let rebase = overlay_size > PATCH_LIMIT_FILES
            && overlay_size * 100 > self.scope.len() * OVERLAY_LIMIT_PERCENT;
        let kept_bytes = if rebase {
            let keep = dir.join(format!("{}.{}", self.exact, kept::BASE_EXTENSION));
            self.rebase(
                &kept,
                &keep,
                state,
                &assembler.strings.all,
                &overlay_files,
                &redone_files,
                &context,
            )
        } else {
            let files = overlay_files
                .into_iter()
                .chain(redone_files.into_iter().map(|(file, redone)| {
                    (
                        file,
                        OverlayFile {
                            calls: redone.calls,
                            roots: redone.roots,
                            block: redone.block,
                            skeleton: redone.skeleton,
                            reads: redone.reads,
                        },
                    )
                }))
                .collect();
            let overlay = Overlay {
                state,
                strings: assembler.strings.since(base_strings),
                files,
                files_by_symbol: index_changes(context.files_by_exported_symbol.all()),
                oxc: oxc_changes,
                rust_public: rust_public_changes,
                go_live: go_live_changes,
            };
            let keep = dir.join(format!("{}.{}", self.exact, kept::OVERLAY_EXTENSION));
            let written = kept::write_overlay(&keep, &kept.base.name, &overlay);
            if written.is_ok() {
                kept::prune(dir, &keep);
            }
            written.map_err(|error| error.to_string())
        };
        let kept_bytes = match kept_bytes {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                crate::slog_warn!("tier2 dead_code: could not keep the dead-code graph: {error}");
                None
            }
        };
        timings.push(("keep", started.elapsed().as_secs_f64() * 1000.0));
        if let Some(mib) = resident_mib() {
            timings.push(("resident MiB", mib));
        }
        timings.push(("changed call-graph files", changed_paths.len() as f64));
        timings.push(("index entries changed", keys.len() as f64));
        timings.push(("changed files", changed.len() as f64));
        timings.push(("files redone", dirty.len() as f64));
        timings.push(("kept files before", old_state_files as f64));
        if let Some(probe) = probe {
            probe.path = "patched";
            probe.changed = changed.len();
            probe.materialized = dirty.len();
            probe.kept_bytes = kept_bytes;
            if probe.details {
                probe.indexes = context.index_digests();
                probe.indexes.push((
                    "referenced_type_names",
                    digest(assembler.totals.referenced_type_names()),
                ));
            }
        }
        Ok(Some(assembler.finish(
            root,
            self.public_api_files,
            roles,
            timings,
        )))
    }
}

impl Rollup<'_, '_> {
    /// Keeps a patched rollup as a new base at `keep`, from the kept graph
    /// `kept`, the files an overlay would hold (`overlay_files`, `redone`),
    /// the rollup's `strings`, its `state` and its indexes (`context`);
    /// returns the base's size.
    #[allow(clippy::too_many_arguments)]
    fn rebase(
        &self,
        kept: &Kept,
        keep: &Path,
        state: State,
        strings: &[Box<str>],
        overlay_files: &BTreeMap<String, OverlayFile>,
        redone: &BTreeMap<String, Redone>,
        context: &MaterializeContext<'_>,
    ) -> Result<u64, String> {
        let mut writer = kept::BaseWriter::create(keep, &self.config)
            .ok_or_else(|| "cannot create a kept dead-code graph".to_string())?;
        let reads = kept.base.readers()?.by_file(kept.base_scope.len());
        let json = |skeleton: &DeadCodeContribution| {
            serde_json::to_vec(skeleton).map_err(|error| error.to_string())
        };
        for file in &self.scope {
            if let Some(file) = redone.get(file) {
                writer.file(
                    &file.calls,
                    &file.roots,
                    &file.block,
                    &json(&file.skeleton)?,
                    file.reads.iter().copied(),
                );
            } else if let Some(file) = overlay_files.get(file) {
                writer.file(
                    &file.calls,
                    &file.roots,
                    &file.block,
                    &json(&file.skeleton)?,
                    file.reads.iter().copied(),
                );
            } else {
                let slot = *kept
                    .base_slots
                    .get(file)
                    .ok_or_else(|| format!("no kept block for {file}"))?;
                writer.raw_file(
                    &kept.base.block(slot)?,
                    &kept.base.skeleton_bytes(slot)?,
                    reads[slot].iter().copied(),
                );
            }
        }
        drop(reads);
        let files_by_symbol = writer.table(context.files_by_exported_symbol.all().entries());
        let oxc = writer.table(context.oxc_by_file.all().entries());
        let rust_public = writer.table(context.rust_public_items.all().entries());
        let go_live = writer.table(context.go_live_methods.all().entries());
        if let Some(failure) = context.failure() {
            return Err(failure);
        }
        let bytes = writer
            .finish(state, strings, files_by_symbol, oxc, rust_public, go_live)
            .map_err(|error| error.to_string())?;
        if let Some(dir) = keep.parent() {
            kept::prune(dir, keep);
        }
        Ok(bytes)
    }
}
