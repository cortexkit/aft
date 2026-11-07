//! Dead code on an integer graph.
//!
//! Every (file, symbol) node the rules name gets a dense number once; edges
//! are compressed sparse rows of those numbers, each flagged with whether
//! production code makes it; Go package initialization stays one shared
//! target list per caller file rather than every caller-target pair;
//! production and test reachability are one bitset walk each; and the tests
//! that alone use a symbol are worked out only for the symbols no production
//! code reaches.
//!
//! Each file is materialized by the parent's per-file rules
//! ([`MaterializeContext::materialize_file`]) from what the call graph says
//! about it ([`CallSource`]), turned into numbers, and dropped before the
//! next batch. Read from the data layer ([`DeadCodeGraph`]), the call graph
//! itself is read one file at a time, so the strings of the whole graph
//! never exist at once. Findings are rendered by the parent's renderer.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde_json::Value;

use super::{
    caller_comparison_key, caller_comparison_path, group_outbound_calls_by_caller_file,
    is_test_code_file, merge_test_origin_ids, normalize_path, parse_dead_code_contributions,
    relative_initializer_group, relative_path, render_dead_code_findings, rust_export_identity,
    symbol_liveness_name, with_job_canonical_paths, CallgraphOutboundCall, CallgraphSnapshot,
    DeadCodeContribution, DispatchNamesForFile, GraphFileFacts, LivenessAnswers,
    MaterializeContext, MethodNamesByLanguage, RelativeInitializerGroup, RenderInputs,
};
use crate::data_layer::dead_code_graph::{CallGraphFiles, DeadCodeGraph};
use crate::inspect::entry_points::ProjectRoles;
use crate::inspect::FileContribution;

/// Files materialized together: their call-graph strings exist only while
/// their batch is turned into numbers.
const BATCH_FILES: usize = 256;

/// The dead-code aggregate of `contributions` over `snapshot`'s call graph:
/// the data layer's graph when the snapshot carries one, else the
/// snapshot's own rows.
///
/// # Errors
///
/// When a file of the data layer's call graph cannot be read.
pub(crate) fn aggregate_with_snapshot(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &ProjectRoles,
    timings: &mut Vec<(&'static str, f64)>,
) -> Result<Value, String> {
    with_job_canonical_paths(|| match snapshot.data_layer.as_ref() {
        Some(view) => {
            let started = std::time::Instant::now();
            let files = view.files();
            let graph = DeadCodeGraph::open(project_root, &files)?;
            let source = GraphSource::new(&graph);
            let snapshot = super::section::with_exports(snapshot, &graph);
            timings.push(("open graph", started.elapsed().as_secs_f64() * 1000.0));
            aggregate(
                &source,
                &snapshot,
                contributions,
                public_api_files,
                roles,
                timings,
            )
        }
        None => {
            let source = SnapshotSource::new(project_root, snapshot);
            aggregate(
                &source,
                snapshot,
                contributions,
                public_api_files,
                roles,
                timings,
            )
        }
    })
}

/// Times one phase into `timings`.
pub(super) fn timed<T>(
    timings: &mut Vec<(&'static str, f64)>,
    label: &'static str,
    phase: impl FnOnce() -> T,
) -> T {
    let started = std::time::Instant::now();
    let value = phase();
    timings.push((label, started.elapsed().as_secs_f64() * 1000.0));
    if let Some(mib) = resident_mib() {
        timings.push(("resident MiB", mib));
    }
    value
}

/// The process's resident memory in MiB, when profiling is asked for with
/// `AFT_DEAD_CODE_PROFILE` (Linux only).
pub(super) fn resident_mib() -> Option<f64> {
    std::env::var_os("AFT_DEAD_CODE_PROFILE")?;
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages = statm.split_whitespace().nth(1)?.parse::<f64>().ok()?;
    Some(pages * 4096.0 / (1024.0 * 1024.0))
}

/// Thread time spent projecting files' calls and materializing them, in
/// microseconds, summed over the threads that did it; read only when
/// profiling.
static PROJECT_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MATERIALIZE_MICROS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The stored syntax facts of project-relative `relative_file` in `files`:
/// `None` when the call graph has no such file.
///
/// # Errors
///
/// When its record cannot be read or holds no facts: every record of the
/// current extractor holds them.
pub(crate) fn stored_facts(
    files: &dyn CallGraphFiles,
    relative_file: &str,
) -> Option<Result<Vec<u8>, String>> {
    files.has_file(relative_file).then(|| {
        files.dead_code_facts(relative_file).and_then(|facts| {
            facts.ok_or_else(|| {
                format!("the content record of {relative_file} holds no dead-code facts")
            })
        })
    })
}

/// What the call graph says about one file whose facts are materialized.
#[derive(Default)]
pub(super) struct FileInputs<'s> {
    /// The calls made in the file.
    borrowed: Vec<&'s CallgraphOutboundCall>,
    owned: Vec<CallgraphOutboundCall>,
    /// Its Go package initialization group.
    go_initializers: Option<RelativeInitializerGroup>,
    /// The symbols the call graph marks as keeping themselves alive, when
    /// it lists the file's symbols at all.
    snapshot_roots: Option<BTreeSet<String>>,
    /// Members unknown receivers in the file keep alive, by project-relative
    /// file and symbol.
    unknown_live: Vec<(String, String)>,
    /// The call-graph files read for it.
    consumed: Vec<String>,
}

/// Where the engine reads the call graph from.
pub(super) trait CallSource: Sync {
    fn root(&self) -> &Path;
    /// What the call graph says about project-relative `file`; with
    /// `calls` false, only what its unknown receivers keep alive.
    fn file_inputs(&self, file: &str, calls: bool) -> Result<FileInputs<'_>, String>;
    /// Members unknown receivers keep alive in the call-graph files no
    /// materialized file `consumed`.
    fn other_unknown_live(
        &self,
        consumed: &BTreeSet<String>,
    ) -> Result<Vec<(String, String)>, String>;
}

/// The data layer's call graph, read one file at a time.
pub(super) struct GraphSource<'a, 'g> {
    graph: &'a DeadCodeGraph<'g>,
    /// Store paths whose calls are filed under each project-relative file.
    calls: HashMap<String, Vec<String>>,
    /// Store paths whose file-level facts (marked roots, Go initialization)
    /// are filed under each project-relative file.
    facts: HashMap<String, Vec<String>>,
}

impl<'a, 'g> GraphSource<'a, 'g> {
    /// Files each graph file the way the snapshot's rollup filed it: calls
    /// by the caller file's physical path, file-level facts by its
    /// project-relative path.
    pub(super) fn new(graph: &'a DeadCodeGraph<'g>) -> Self {
        let project_root = graph.root();
        let lexical_root = normalize_path(project_root);
        let canonical_root = super::canonicalize_normalized(project_root);
        let mut calls = HashMap::<String, Vec<String>>::new();
        let mut facts = HashMap::<String, Vec<String>>::new();
        for path in graph.file_paths() {
            let resolved = graph.resolve(path);
            let comparison =
                caller_comparison_path(project_root, &lexical_root, &canonical_root, &resolved);
            let (calls_key, _) = caller_comparison_key(&lexical_root, &canonical_root, comparison);
            calls.entry(calls_key).or_default().push(path.to_string());
            facts
                .entry(relative_path(project_root, &resolved))
                .or_default()
                .push(path.to_string());
        }
        Self {
            graph,
            calls,
            facts,
        }
    }

    /// The call-graph files whose calls, and whose file-level facts, are
    /// filed under each project-relative file.
    pub(super) fn graph_paths(&self, file: &str) -> (Option<&Vec<String>>, Option<&Vec<String>>) {
        (self.calls.get(file), self.facts.get(file))
    }

    /// Every project-relative file's call-graph files: those of its calls,
    /// and those of its file-level facts.
    pub(super) fn all_graph_paths(
        &self,
    ) -> (BTreeMap<String, Vec<String>>, BTreeMap<String, Vec<String>>) {
        (
            self.calls
                .iter()
                .map(|(file, paths)| (file.clone(), paths.clone()))
                .collect(),
            self.facts
                .iter()
                .map(|(file, paths)| (file.clone(), paths.clone()))
                .collect(),
        )
    }

    fn relative_unknown_live(&self, path: &str) -> Result<Vec<(String, String)>, String> {
        Ok(self
            .graph
            .unknown_live(path)?
            .into_iter()
            .map(|(file, symbol)| (relative_path(self.graph.root(), &file), symbol))
            .collect())
    }
}

impl CallSource for GraphSource<'_, '_> {
    fn root(&self) -> &Path {
        self.graph.root()
    }

    fn file_inputs(&self, file: &str, calls: bool) -> Result<FileInputs<'_>, String> {
        super::reads::record(super::reads::Lookup::GraphPaths, file);
        let calls_paths = self.calls.get(file).cloned().unwrap_or_default();
        let facts_paths = self.facts.get(file).cloned().unwrap_or_default();
        let mut inputs = FileInputs {
            consumed: calls_paths.iter().chain(&facts_paths).cloned().collect(),
            ..FileInputs::default()
        };
        inputs.consumed.sort();
        inputs.consumed.dedup();
        if !calls {
            for path in &inputs.consumed {
                let live = self.relative_unknown_live(path)?;
                inputs.unknown_live.extend(live);
            }
            return Ok(inputs);
        }
        let mut snapshot_roots = BTreeSet::new();
        for path in &inputs.consumed {
            let projection = self.graph.project_file(path)?;
            for (target, symbol) in projection.unknown_live {
                inputs
                    .unknown_live
                    .push((relative_path(self.graph.root(), &target), symbol));
            }
            if calls_paths.contains(path) {
                inputs.owned.extend(projection.outbound_calls);
            }
            if facts_paths.contains(path) {
                snapshot_roots.extend(self.graph.own_entry_point_symbols(path));
                // As the rollup keys groups by file, a later group of the
                // same file replaces an earlier one.
                if let Some(group) = projection.go_initializers {
                    inputs.go_initializers =
                        Some(relative_initializer_group(self.graph.root(), &group));
                }
            }
        }
        inputs.snapshot_roots = (!facts_paths.is_empty()).then_some(snapshot_roots);
        Ok(inputs)
    }

    fn other_unknown_live(
        &self,
        consumed: &BTreeSet<String>,
    ) -> Result<Vec<(String, String)>, String> {
        let others = self
            .graph
            .file_paths()
            .filter(|path| !consumed.contains(*path))
            .collect::<Vec<_>>();
        let live = others
            .par_iter()
            .map(|path| self.relative_unknown_live(path))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(live.into_iter().flatten().collect())
    }
}

/// A call-graph snapshot's rows, filed by project-relative file. The
/// snapshot's marked roots already include what unknown receivers keep
/// alive.
struct SnapshotSource<'s> {
    root: PathBuf,
    calls: BTreeMap<String, Vec<&'s CallgraphOutboundCall>>,
    roots: BTreeMap<String, &'s BTreeSet<String>>,
    groups: BTreeMap<String, &'s crate::inspect::job::GoInitializerGroup>,
}

impl<'s> SnapshotSource<'s> {
    fn new(project_root: &Path, snapshot: &'s CallgraphSnapshot) -> Self {
        Self {
            root: project_root.to_path_buf(),
            calls: group_outbound_calls_by_caller_file(project_root, &snapshot.outbound_calls)
                .by_file,
            roots: snapshot
                .entry_point_symbols
                .iter()
                .map(|(file, symbols)| (relative_path(project_root, file), symbols))
                .collect(),
            groups: snapshot
                .go_initializers
                .iter()
                .map(|group| (relative_path(project_root, &group.caller_file), group))
                .collect(),
        }
    }
}

impl CallSource for SnapshotSource<'_> {
    fn root(&self) -> &Path {
        &self.root
    }

    fn file_inputs(&self, file: &str, calls: bool) -> Result<FileInputs<'_>, String> {
        if !calls {
            return Ok(FileInputs::default());
        }
        Ok(FileInputs {
            borrowed: self.calls.get(file).cloned().unwrap_or_default(),
            go_initializers: self
                .groups
                .get(file)
                .map(|group| relative_initializer_group(&self.root, group)),
            snapshot_roots: self.roots.get(file).map(|roots| (*roots).clone()),
            ..FileInputs::default()
        })
    }

    fn other_unknown_live(
        &self,
        _consumed: &BTreeSet<String>,
    ) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }
}

/// One file's part of the dead-code graph: what materializing it produced,
/// reduced to what the graph and the findings read.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileBlock {
    contribution: DeadCodeContribution,
    /// Whether its cached facts could not be decoded (not serialized with
    /// the contribution).
    decode_failed: bool,
    /// Its Go package initialization group: callers, targets and whether
    /// the file is test code (not serialized with the contribution).
    go_initializers: Option<(Vec<String>, Vec<(String, String)>, bool)>,
    /// The call-graph files read for it.
    consumed: Vec<String>,
    /// The (file, node) roots that members unknown receivers in those files
    /// keep alive.
    unknown_live_roots: Vec<(String, String)>,
}

impl FileBlock {
    /// The project-relative file this block is of.
    pub(super) fn file(&self) -> &str {
        &self.contribution.file
    }

    /// The call-graph files read for it.
    pub(super) fn consumed(&self) -> &[String] {
        &self.consumed
    }

    fn new(
        context: &MaterializeContext<'_>,
        contribution: DeadCodeContribution,
        consumed: Vec<String>,
        unknown_live: Vec<(String, String)>,
    ) -> Self {
        let unknown_live_roots = live_roots(context, unknown_live);
        let mut contribution = graph_contribution(contribution);
        let go_initializers = contribution.go_initializers.take().map(|group| {
            (
                group.callers,
                group.targets.iter().cloned().collect(),
                group.test_origin,
            )
        });
        Self {
            decode_failed: contribution.decode_failed,
            contribution,
            go_initializers,
            consumed,
            unknown_live_roots,
        }
    }

    /// The contribution as the graph reads it.
    fn into_contribution(self) -> DeadCodeContribution {
        let mut contribution = self.contribution;
        contribution.decode_failed = self.decode_failed;
        contribution.go_initializers =
            self.go_initializers.map(|(callers, targets, test_origin)| {
                super::GoInitializerContribution {
                    callers,
                    targets: std::sync::Arc::new(targets.into_iter().collect()),
                    test_origin,
                }
            });
        contribution
    }
}

/// The roots that `unknown_live` (file, member) pairs name.
fn live_roots(
    context: &MaterializeContext<'_>,
    unknown_live: Vec<(String, String)>,
) -> Vec<(String, String)> {
    unknown_live
        .into_iter()
        .filter_map(|(file, symbol)| {
            let root = super::clean_symbol(&symbol)?;
            let root = context.root_node(&file, root);
            Some((file, root))
        })
        .collect()
}

/// What the findings read about every scanned file at once.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct Totals {
    scanned_files: usize,
    referenced_type_names: BTreeSet<String>,
    test_module_files: BTreeSet<String>,
}

/// Strings by number: the file paths and symbols of a dead-code graph's
/// calls and roots, each kept once.
#[derive(Default)]
pub(super) struct Strings {
    /// Each string's number by a hash of the string, so the table keeps each
    /// string once; strings whose hashes collide are numbered in
    /// `collided`. Built only when a string is numbered, since a graph read
    /// back numbers none.
    ids: Option<HashMap<u64, u32>>,
    collided: HashMap<Box<str>, u32>,
    pub(super) all: Vec<Box<str>>,
}

fn string_hash(string: &str) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault};
    BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().hash_one(string)
}

impl Strings {
    pub(super) fn id(&mut self, string: &str) -> u32 {
        let all = &self.all;
        let ids = self.ids.get_or_insert_with(|| {
            let mut ids = HashMap::with_capacity(all.len());
            for (id, string) in all.iter().enumerate() {
                ids.entry(string_hash(string)).or_insert(id as u32);
            }
            ids
        });
        let hash = string_hash(string);
        match ids.get(&hash) {
            Some(id) if &*self.all[*id as usize] == string => return *id,
            Some(_) => {
                if let Some(id) = self.collided.get(string) {
                    return *id;
                }
                // Built from a table read back, an earlier string with this
                // hash may hold the number while a later equal string is
                // the one asked for; any number of the string will do.
                let id = self.all.len() as u32;
                self.all.push(string.into());
                self.collided.insert(string.into(), id);
                return id;
            }
            None => {}
        }
        let id = self.all.len() as u32;
        self.all.push(string.into());
        ids.insert(hash, id);
        id
    }

    pub(super) fn get(&self, id: u32) -> &str {
        &self.all[id as usize]
    }

    /// The strings numbered from `first` on.
    pub(super) fn since(&self, first: usize) -> Vec<String> {
        self.all[first..]
            .iter()
            .map(|string| string.to_string())
            .collect()
    }

    /// The table of `strings`, numbered in their order.
    pub(super) fn from_all(strings: Vec<Box<str>>) -> Self {
        Self {
            ids: None,
            collided: HashMap::new(),
            all: strings,
        }
    }
}

/// A [`FileBlock`] whose calls and roots name files and symbols by string
/// number.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct EncodedBlock {
    /// The strings this block is first to use, numbered on from the
    /// strings before it.
    pub(super) strings: Vec<String>,
    /// Five numbers per call: caller symbol, target file, target symbol,
    /// line, and origin (0 not classified, 1 production, 2 test).
    pub(super) calls: Vec<u32>,
    /// (file, symbol) string numbers, two per root.
    pub(super) roots: Vec<u32>,
    /// The rest of the block, without its calls and roots.
    pub(super) block: FileBlock,
}

/// The dead-code graph as materialized files are added to it, one at a
/// time, so no more than a batch of whole files is held at once.
pub(super) struct Assembler {
    pub(super) totals: Totals,
    builder: GraphBuilder,
    pub(super) strings: Strings,
    /// Graph nodes by (file, symbol) string numbers.
    nodes: HashMap<(u32, u32), u32>,
    /// Each file's contribution with what rendering reads.
    slim: Vec<DeadCodeContribution>,
    /// Each language's dispatched method names. Every file's language is
    /// listed, also without names: the language is then known to have none.
    dispatched: MethodNamesByLanguage,
    /// Roots that members unknown receivers keep alive: (file, symbol)
    /// string numbers, two per root.
    roots: Vec<u32>,
    /// The same for call-graph files no scanned file read.
    pub(super) other_roots: Vec<u32>,
    /// Whether any file's cached facts could not be decoded.
    pub(super) decode_failed: bool,
}

impl Assembler {
    pub(super) fn new(totals: Totals) -> Self {
        Self {
            totals,
            builder: GraphBuilder::default(),
            strings: Strings::default(),
            nodes: HashMap::new(),
            slim: Vec::new(),
            dispatched: MethodNamesByLanguage::new(),
            roots: Vec::new(),
            other_roots: Vec::new(),
            decode_failed: false,
        }
    }

    /// `block` with its calls and roots numbered, numbering strings not
    /// seen before.
    pub(super) fn encode(&mut self, mut block: FileBlock) -> EncodedBlock {
        let first_new = self.strings.all.len();
        let calls = std::mem::take(&mut block.contribution.internal_calls);
        let mut numbers = Vec::with_capacity(calls.len() * 5);
        for call in &calls {
            numbers.extend([
                self.strings.id(&call.caller_symbol),
                self.strings.id(&call.file),
                self.strings.id(&call.symbol),
                call.line,
                match call.test_origin {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                },
            ]);
        }
        let roots = self.encode_roots(std::mem::take(&mut block.unknown_live_roots));
        // The caller file is numbered too: calls name their caller node by it.
        self.strings.id(&block.contribution.file);
        EncodedBlock {
            strings: self.strings.since(first_new),
            calls: numbers,
            roots,
            block,
        }
    }

    pub(super) fn encode_roots(&mut self, roots: Vec<(String, String)>) -> Vec<u32> {
        roots
            .iter()
            .flat_map(|(file, symbol)| [self.strings.id(file), self.strings.id(symbol)])
            .collect()
    }

    /// The node of (file, symbol) string numbers.
    fn node(&mut self, file: u32, symbol: u32) -> u32 {
        if let Some(node) = self.nodes.get(&(file, symbol)) {
            return *node;
        }
        let node = self
            .builder
            .ids
            .node(self.strings.get(file), self.strings.get(symbol));
        self.nodes.insert((file, symbol), node);
        node
    }

    /// Adds `block`, whose strings are all numbered.
    pub(super) fn add(&mut self, block: EncodedBlock) {
        let EncodedBlock {
            strings: _,
            calls,
            roots,
            block,
        } = block;
        self.decode_failed |= block.decode_failed;
        self.roots.extend(roots);
        let mut contribution = block.into_contribution();
        let file = self.strings.id(&contribution.file);
        for call in calls.chunks_exact(5) {
            let &[caller, target_file, target_symbol, line, origin] = call else {
                unreachable!("chunks of five");
            };
            let source = self.node(file, caller);
            let target = self.node(target_file, target_symbol);
            let origin = match origin {
                1 => Origin::Production,
                2 => Origin::Test,
                _ => Origin::Unknown,
            };
            let has_caller = !self.strings.get(caller).is_empty();
            self.builder.add_call(
                &contribution,
                &self.totals.test_module_files,
                source,
                target,
                has_caller,
                origin,
                line,
            );
        }
        self.builder
            .add_file(&contribution, &self.totals.test_module_files);
        self.dispatched
            .entry(super::language_for_file(&contribution.file).to_string())
            .or_default()
            .extend(std::mem::take(&mut contribution.dispatched_method_names));
        self.slim.push(slim_contribution(contribution));
    }

    /// Adds the sizes of what the assembled graph holds to `timings`, when
    /// profiling: string bytes, the render data's JSON bytes, nodes.
    pub(super) fn profile_sizes(&self, timings: &mut Vec<(&'static str, f64)>) {
        let string_bytes = self
            .strings
            .all
            .iter()
            .map(|string| string.len())
            .sum::<usize>();
        timings.push(("strings", self.strings.all.len() as f64));
        timings.push(("string bytes", string_bytes as f64));
        let slim_bytes = self
            .slim
            .iter()
            .map(|contribution| serde_json::to_vec(contribution).map_or(0, |bytes| bytes.len()))
            .sum::<usize>();
        timings.push(("render data JSON bytes", slim_bytes as f64));
        let exports = self
            .slim
            .iter()
            .map(|contribution| contribution.exports.len())
            .sum::<usize>();
        timings.push(("render exports", exports as f64));
        timings.push(("assembled nodes", self.builder.ids.nodes.len() as f64));
        timings.push(("assembled edges", self.builder.edges.len() as f64));
        timings.push(("test labels", self.builder.labels.len() as f64));
    }

    /// The dead-code aggregate of every file added.
    pub(super) fn finish(
        self,
        project_root: &Path,
        public_api_files: &BTreeSet<String>,
        roles: &ProjectRoles,
        timings: &mut Vec<(&'static str, f64)>,
    ) -> Value {
        let Self {
            totals:
                Totals {
                    scanned_files,
                    referenced_type_names,
                    test_module_files,
                },
            mut builder,
            strings,
            nodes: _,
            mut slim,
            dispatched: dispatched_method_names,
            mut roots,
            other_roots,
            decode_failed: _,
        } = self;
        roots.extend(other_roots);
        let materialized_files = slim
            .iter()
            .filter(|contribution| !contribution.excluded_from_graph)
            .map(|contribution| contribution.file.as_str())
            .collect::<BTreeSet<_>>();
        let mut rooted_files = BTreeSet::new();
        for root in roots.chunks_exact(2) {
            let (file, symbol) = (strings.get(root[0]), strings.get(root[1]));
            if !materialized_files.contains(file) {
                continue;
            }
            builder.add_root(file, symbol);
            rooted_files.insert(file.to_string());
        }
        let graph = timed(timings, "reachability", || {
            builder.finish(&slim, &dispatched_method_names)
        });
        // A root added for a member an unknown receiver keeps alive marks the
        // export it names as an entry point, as the per-file rules would have.
        for contribution in &mut slim {
            if !rooted_files.contains(&contribution.file) {
                continue;
            }
            let file = contribution.file.clone();
            let rust_api = contribution.rust_api.take();
            for export in &mut contribution.exports {
                export.is_entry_point =
                    graph.is_root(&file, &rust_export_identity(export, rust_api.as_ref()));
            }
            contribution.rust_api = rust_api;
        }
        let answers = graph.answers();
        let started = std::time::Instant::now();
        let aggregate = render_dead_code_findings(
            project_root,
            &slim,
            public_api_files,
            roles,
            scanned_files,
            &answers,
            &RenderInputs {
                has_source_gaps: super::has_oversized_sources(&slim),
                referenced_type_names,
                test_module_files,
                dispatched_method_names: &dispatched_method_names,
            },
        );
        timings.push(("render", started.elapsed().as_secs_f64() * 1000.0));
        timings.push(("graph nodes", graph.ids.nodes.len() as f64));
        timings.push(("graph edges", graph.targets.len() as f64));
        aggregate
    }
}

/// The dead-code aggregate of `contributions` over `source`'s calls;
/// `snapshot` names the call graph's entry-point files and exports.
/// The dead-code aggregate of `contributions` over `source`'s calls;
/// `snapshot` names the call graph's entry-point files and exports.
fn aggregate(
    source: &dyn CallSource,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &ProjectRoles,
    timings: &mut Vec<(&'static str, f64)>,
) -> Result<Value, String> {
    let project_root = source.root().to_path_buf();
    let project_root = project_root.as_path();
    let parsed = timed(timings, "decode facts", || {
        parse_dead_code_contributions(project_root, contributions)
    });
    let context = timed(timings, "index facts and Oxc verdicts", || {
        MaterializeContext::new(
            project_root,
            GraphFileFacts::from_snapshot(project_root, snapshot),
            &parsed,
            public_api_files,
            true,
        )
    });
    let mut assembler = Assembler::new(Totals::of(&parsed, &context));
    let started = std::time::Instant::now();
    let consumed = materialize_each(&context, source, parsed, false, &mut |file| {
        let block = assembler.encode(file.block);
        assembler.add(block);
        Ok(())
    })?;
    timings.push((
        "project and materialize files",
        started.elapsed().as_secs_f64() * 1000.0,
    ));
    let other_roots = timed(timings, "other files' receivers", || {
        other_roots(&context, source, &consumed)
    })?;
    assembler.other_roots = assembler.encode_roots(other_roots);
    Ok(assembler.finish(project_root, public_api_files, roles, timings))
}

impl Totals {
    /// Totals of `scanned_files` files with these referenced type names and
    /// test-module files.
    pub(super) fn new(
        scanned_files: usize,
        referenced_type_names: BTreeSet<String>,
        test_module_files: BTreeSet<String>,
    ) -> Self {
        Self {
            scanned_files,
            referenced_type_names,
            test_module_files,
        }
    }

    /// The type names any scanned file refers to.
    pub(super) fn referenced_type_names(&self) -> &BTreeSet<String> {
        &self.referenced_type_names
    }

    /// What the findings read about all of `parsed` at once.
    pub(super) fn of(parsed: &[DeadCodeContribution], context: &MaterializeContext<'_>) -> Self {
        Self {
            scanned_files: parsed.len(),
            referenced_type_names: parsed
                .iter()
                .flat_map(|contribution| contribution.type_ref_names.iter().cloned())
                .collect(),
            test_module_files: context.test_module_files().clone(),
        }
    }
}

/// Members an unknown receiver keeps alive in call-graph files no scanned
/// file read: they are roots of their file too, also when it has no facts
/// of its own.
pub(super) fn other_roots(
    context: &MaterializeContext<'_>,
    source: &dyn CallSource,
    consumed: &BTreeSet<String>,
) -> Result<Vec<(String, String)>, String> {
    source
        .other_unknown_live(consumed)
        .map(|live| live_roots(context, live))
}

/// One file materialized: its block, and when asked for, every index entry
/// its materialization read and the facts the indexes are built from.
pub(super) struct MaterializedFile {
    pub(super) block: FileBlock,
    pub(super) reads: Vec<(super::reads::Lookup, String)>,
    pub(super) skeleton: DeadCodeContribution,
}

/// Materializes `contributions` over `source`'s calls, a batch at a time
/// in parallel, handing each file to `sink` in their order; `record` asks
/// for each file's reads and index facts. Returns the call-graph files read.
pub(super) fn materialize_each(
    context: &MaterializeContext<'_>,
    source: &dyn CallSource,
    contributions: Vec<DeadCodeContribution>,
    record: bool,
    sink: &mut dyn FnMut(MaterializedFile) -> Result<(), String>,
) -> Result<BTreeSet<String>, String> {
    let mut consumed = BTreeSet::<String>::new();
    let mut contributions = contributions.into_iter();
    loop {
        let batch = contributions.by_ref().take(BATCH_FILES).collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }
        let files = batch
            .into_par_iter()
            .map(|contribution| materialize(context, source, contribution, record))
            .collect::<Result<Vec<_>, String>>()?;
        for file in files {
            consumed.extend(file.block.consumed.iter().cloned());
            sink(file)?;
        }
    }
    if let Some(mib) = resident_mib() {
        for (label, counter) in [
            ("projection thread ms", &PROJECT_MICROS),
            ("materialize thread ms", &MATERIALIZE_MICROS),
        ] {
            crate::slog_info!(
                "perf dead_code {label}={} resident_mib={mib:.0}",
                counter.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0
            );
        }
    }
    Ok(consumed)
}

fn materialize(
    context: &MaterializeContext<'_>,
    source: &dyn CallSource,
    contribution: DeadCodeContribution,
    record: bool,
) -> Result<MaterializedFile, String> {
    let skeleton = if record {
        contribution.index_skeleton()
    } else {
        DeadCodeContribution::default()
    };
    let recording = record.then(super::reads::Recording::start);
    // Calls from a deliberately excluded file must not keep another file's
    // symbols live; only what its receivers keep alive elsewhere is read.
    let started = std::time::Instant::now();
    let inputs = source.file_inputs(&contribution.file, !contribution.excluded_from_graph)?;
    PROJECT_MICROS.fetch_add(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    let block = if contribution.excluded_from_graph {
        FileBlock::new(context, contribution, inputs.consumed, inputs.unknown_live)
    } else {
        let calls = inputs
            .borrowed
            .iter()
            .copied()
            .chain(&inputs.owned)
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        let contribution = context.materialize_file(
            contribution,
            &calls,
            inputs.go_initializers,
            inputs.snapshot_roots.as_ref(),
        );
        MATERIALIZE_MICROS.fetch_add(
            started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        FileBlock::new(context, contribution, inputs.consumed, inputs.unknown_live)
    };
    let reads = recording
        .map(super::reads::Recording::finish)
        .unwrap_or_default();
    Ok(MaterializedFile {
        block,
        reads,
        skeleton,
    })
}

fn graph_contribution(mut contribution: DeadCodeContribution) -> DeadCodeContribution {
    let internal_calls = std::mem::take(&mut contribution.internal_calls);
    let liveness_roots = std::mem::take(&mut contribution.liveness_roots);
    let imported_exports = std::mem::take(&mut contribution.imported_exports);
    let namespace_imported_exports = std::mem::take(&mut contribution.namespace_imported_exports);
    let dispatched_method_names = std::mem::take(&mut contribution.dispatched_method_names);
    let go_initializers = contribution.go_initializers.take();
    DeadCodeContribution {
        internal_calls,
        liveness_roots,
        imported_exports,
        namespace_imported_exports,
        dispatched_method_names,
        go_initializers,
        ..slim_contribution(contribution)
    }
}

/// `contribution` with only what rendering reads: its exports and the facts
/// that classify them.
fn slim_contribution(contribution: DeadCodeContribution) -> DeadCodeContribution {
    DeadCodeContribution {
        excluded_from_graph: contribution.excluded_from_graph,
        decode_failed: contribution.decode_failed,
        file: contribution.file,
        rust_api: contribution.rust_api.map(|facts| super::RustApiFacts {
            items: facts.items,
            ..Default::default()
        }),
        go_dispatch: contribution.go_dispatch,
        generated: contribution.generated,
        exports: contribution.exports,
        cfg_test_ranges: contribution.cfg_test_ranges,
        parse_errors: contribution.parse_errors,
        skipped_files: contribution.skipped_files,
        skipped_languages: contribution.skipped_languages,
        ..Default::default()
    }
}

/// Dense numbers for (file, symbol) nodes.
#[derive(Default)]
struct NodeIds {
    files: Vec<Box<str>>,
    file_ids: HashMap<Box<str>, u32>,
    /// Each file's nodes by symbol, so a lookup borrows the symbol rather
    /// than allocating a key.
    ids: Vec<HashMap<Box<str>, u32>>,
    /// Each node's file and symbol.
    nodes: Vec<(u32, Box<str>)>,
}

impl NodeIds {
    fn file(&mut self, file: &str) -> u32 {
        if let Some(id) = self.file_ids.get(file) {
            return *id;
        }
        let id = self.files.len() as u32;
        self.files.push(file.into());
        self.file_ids.insert(file.into(), id);
        self.ids.push(HashMap::new());
        id
    }

    fn node(&mut self, file: &str, symbol: &str) -> u32 {
        let file = self.file(file);
        let symbols = &mut self.ids[file as usize];
        if let Some(id) = symbols.get(symbol) {
            return *id;
        }
        let id = self.nodes.len() as u32;
        symbols.insert(symbol.into(), id);
        self.nodes.push((file, symbol.into()));
        id
    }

    fn find(&self, file: &str, symbol: &str) -> Option<u32> {
        let file = *self.file_ids.get(file)?;
        self.ids[file as usize].get(symbol).copied()
    }
}

/// How production code stands toward one call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Not classified: an edge to a type declaration.
    Unknown,
    Production,
    Test,
}

/// One Go file's package initialization: every caller reaches every target
/// but itself.
struct InitializerGroup {
    callers: Vec<u32>,
    /// Sorted and distinct.
    targets: Vec<u32>,
    test_origin: bool,
    /// The test-origin label its calls carry, when they are test calls.
    label: Option<u32>,
}

impl InitializerGroup {
    /// Whether `caller`'s group calls reach any node: the group has a target
    /// other than the caller itself.
    fn reaches_from(&self, caller: u32) -> bool {
        self.targets.iter().any(|target| *target != caller)
    }
}

/// The graph as files are added to it.
#[derive(Default)]
struct GraphBuilder {
    ids: NodeIds,
    /// Ordinary edges: source, target, made by production code (not a test
    /// call).
    edges: Vec<(u32, u32, bool)>,
    groups: Vec<InitializerGroup>,
    /// Each node's initializer group.
    node_group: HashMap<u32, u32>,
    roots: Vec<u32>,
    /// Targets of calls made by production code, with the caller (whose
    /// own test usage withdraws them as production evidence).
    production_seeds: Vec<(u32, u32)>,
    /// Targets of test calls, with the test that makes them.
    test_seeds: Vec<(u32, u32)>,
    labels: Labels,
    /// Each file's imported and namespace-imported exports.
    imports: HashMap<u32, Vec<u32>>,
}

/// The line of a label that names a whole test file.
const WHOLE_FILE: u32 = u32::MAX;

/// Test-origin labels by number. A label is a test file, or a line of a
/// `#[cfg(test)]` region (`file:line`); it is kept as numbers, since a large
/// project has tens of thousands of such lines and only the symbols a reply
/// reports need a label's text.
#[derive(Default)]
struct Labels {
    /// Each label's file (a number into `files`) and line, or
    /// [`WHOLE_FILE`].
    labels: Vec<(u32, u32)>,
    ids: HashMap<(u32, u32), u32>,
    files: Vec<Box<str>>,
    file_ids: HashMap<Box<str>, u32>,
}

impl Labels {
    fn id(&mut self, file: &str, line: u32) -> u32 {
        let file = match self.file_ids.get(file) {
            Some(id) => *id,
            None => {
                let id = self.files.len() as u32;
                self.files.push(file.into());
                self.file_ids.insert(file.into(), id);
                id
            }
        };
        let next = self.labels.len() as u32;
        let id = *self.ids.entry((file, line)).or_insert(next);
        if id == next {
            self.labels.push((file, line));
        }
        id
    }

    fn text(&self, id: usize) -> String {
        let (file, line) = self.labels[id];
        let file = &self.files[file as usize];
        if line == WHOLE_FILE {
            file.to_string()
        } else {
            format!("{file}:{line}")
        }
    }

    fn len(&self) -> usize {
        self.labels.len()
    }
}

impl GraphBuilder {
    /// The test-origin label of a test call at `line` of `contribution`: the
    /// test file, or the `#[cfg(test)]` line. A test call with neither is not
    /// a test origin.
    fn test_label(
        &mut self,
        contribution: &DeadCodeContribution,
        test_module_files: &BTreeSet<String>,
        line: u32,
    ) -> Option<u32> {
        if is_test_code_file(&contribution.file, test_module_files) {
            Some(self.labels.id(&contribution.file, WHOLE_FILE))
        } else if contribution.line_is_cfg_test(line) {
            Some(self.labels.id(&contribution.file, line))
        } else {
            None
        }
    }

    fn add_root(&mut self, file: &str, symbol: &str) {
        let node = self.ids.node(file, symbol);
        self.roots.push(node);
    }

    /// Adds one call of `contribution`'s file, from node `source` (a
    /// caller with a name when `has_caller`) to node `target`, made at
    /// `line`.
    #[allow(clippy::too_many_arguments)]
    fn add_call(
        &mut self,
        contribution: &DeadCodeContribution,
        test_module_files: &BTreeSet<String>,
        source: u32,
        target: u32,
        has_caller: bool,
        origin: Origin,
        line: u32,
    ) {
        if has_caller {
            self.edges.push((source, target, origin != Origin::Test));
        }
        match origin {
            Origin::Production => self.production_seeds.push((source, target)),
            Origin::Test => {
                if let Some(label) = self.test_label(contribution, test_module_files, line) {
                    self.test_seeds.push((target, label));
                }
            }
            Origin::Unknown => {}
        }
    }

    /// Adds `contribution`'s file's facts other than its calls.
    fn add_file(
        &mut self,
        contribution: &DeadCodeContribution,
        test_module_files: &BTreeSet<String>,
    ) {
        let file = contribution.file.as_str();
        if let Some(group) = &contribution.go_initializers {
            let callers = group
                .callers
                .iter()
                .map(|caller| self.ids.node(file, caller))
                .collect::<Vec<_>>();
            let mut targets = group
                .targets
                .iter()
                .map(|(target_file, symbol)| self.ids.node(target_file, symbol))
                .collect::<Vec<_>>();
            targets.sort_unstable();
            targets.dedup();
            // Initializer calls are made at line 1 of the caller file.
            let label = if group.test_origin {
                self.test_label(contribution, test_module_files, 1)
            } else {
                None
            };
            let index = self.groups.len() as u32;
            for (caller, name) in callers.iter().zip(&group.callers) {
                if !name.is_empty() {
                    self.node_group.insert(*caller, index);
                }
            }
            self.groups.push(InitializerGroup {
                callers,
                targets,
                test_origin: group.test_origin,
                label,
            });
        }
        for root in &contribution.liveness_roots {
            self.add_root(file, root);
        }
        for export in contribution
            .exports
            .iter()
            .filter(|export| export.is_entry_point)
        {
            let identity = rust_export_identity(export, contribution.rust_api.as_ref());
            self.add_root(file, &identity);
        }
        if !contribution.imported_exports.is_empty()
            || !contribution.namespace_imported_exports.is_empty()
        {
            let file_id = self.ids.file(file);
            let targets = contribution
                .imported_exports
                .iter()
                .chain(&contribution.namespace_imported_exports)
                .map(|root| self.ids.node(&root.file, &root.symbol))
                .collect::<Vec<_>>();
            self.imports.entry(file_id).or_default().extend(targets);
        }
    }

    fn finish(
        self,
        contributions: &[DeadCodeContribution],
        dispatched_method_names: &MethodNamesByLanguage,
    ) -> DeadCodeIntGraph {
        let Self {
            ids,
            mut edges,
            groups,
            node_group,
            roots,
            production_seeds,
            test_seeds,
            labels,
            mut imports,
        } = self;
        let node_count = ids.nodes.len();
        // One row per source; an edge made by production code anywhere is a
        // production edge.
        edges.sort_unstable();
        let mut offsets = vec![0u32; node_count + 1];
        let mut targets = Vec::with_capacity(edges.len());
        let mut production = Vec::with_capacity(edges.len());
        let mut index = 0;
        while index < edges.len() {
            let (source, target, _) = edges[index];
            let mut made_by_production = false;
            while index < edges.len() && edges[index].0 == source && edges[index].1 == target {
                made_by_production |= edges[index].2;
                index += 1;
            }
            offsets[source as usize + 1] += 1;
            targets.push(target);
            production.push(made_by_production);
        }
        drop(edges);
        for node in 0..node_count {
            offsets[node + 1] += offsets[node];
        }
        for targets in imports.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }
        let mut graph = DeadCodeIntGraph {
            ids,
            offsets,
            targets,
            production,
            groups,
            node_group,
            imports,
            roots: Bitset::new(node_count),
            reached: Bitset::new(node_count),
            production_reached: Bitset::new(node_count),
            test_only: HashMap::new(),
            labels,
        };
        for root in &roots {
            graph.roots.insert(*root);
        }
        let dispatch_names =
            dispatch_names_by_file(&graph.ids, contributions, dispatched_method_names);
        let all_seeds = roots
            .iter()
            .copied()
            .chain(graph.dispatch_roots(&dispatch_names, false))
            .collect::<Vec<_>>();
        graph.reached = graph.reach(all_seeds, false);
        let production_seeds_from_roots = roots
            .iter()
            .copied()
            .chain(graph.dispatch_roots(&dispatch_names, true))
            .collect::<Vec<_>>();
        graph.production_reached = graph.reach(production_seeds_from_roots, true);
        graph.test_only = graph.test_only_callers(&production_seeds, test_seeds);
        graph
    }
}

/// The dispatch names that can make a node of each file a dispatch root.
fn dispatch_names_by_file<'a>(
    ids: &NodeIds,
    contributions: &'a [DeadCodeContribution],
    dispatched_method_names: &'a MethodNamesByLanguage,
) -> HashMap<u32, DispatchNamesForFile<'a>> {
    super::dispatch_live_source_names_by_file(contributions, dispatched_method_names)
        .into_iter()
        .filter_map(|(file, names)| Some((*ids.file_ids.get(file)?, names)))
        .collect()
}

/// A set of node numbers.
struct Bitset(Vec<u64>);

impl Bitset {
    fn new(len: usize) -> Self {
        Self(vec![0; len.div_ceil(64)])
    }

    fn contains(&self, index: u32) -> bool {
        self.0
            .get(index as usize / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    /// Adds `index`; whether it was new.
    fn insert(&mut self, index: u32) -> bool {
        let word = &mut self.0[index as usize / 64];
        let bit = 1 << (index % 64);
        let new = *word & bit == 0;
        *word |= bit;
        new
    }
}

/// The finished graph and what it answers.
struct DeadCodeIntGraph {
    ids: NodeIds,
    /// Compressed sparse rows: node `n`'s targets are
    /// `targets[offsets[n]..offsets[n + 1]]`.
    offsets: Vec<u32>,
    targets: Vec<u32>,
    /// Per edge: whether production code makes it.
    production: Vec<bool>,
    groups: Vec<InitializerGroup>,
    node_group: HashMap<u32, u32>,
    /// Each file's imported exports, reached with the file's first node.
    imports: HashMap<u32, Vec<u32>>,
    roots: Bitset,
    reached: Bitset,
    production_reached: Bitset,
    /// The tests that alone use each node only tests use.
    test_only: HashMap<u32, Vec<usize>>,
    labels: Labels,
}

impl DeadCodeIntGraph {
    fn is_root(&self, file: &str, symbol: &str) -> bool {
        self.ids
            .find(file, symbol)
            .is_some_and(|node| self.roots.contains(node))
    }

    /// The nodes `node` has an edge to, in the production graph only when
    /// `production`.
    fn successors(&self, node: u32, production: bool, mut visit: impl FnMut(u32)) {
        let start = self.offsets[node as usize] as usize;
        let end = self.offsets[node as usize + 1] as usize;
        for edge in start..end {
            if !production || self.production[edge] {
                visit(self.targets[edge]);
            }
        }
        if let Some(group) = self.node_group.get(&node) {
            let group = &self.groups[*group as usize];
            if !(production && group.test_origin) {
                for target in &group.targets {
                    if *target != node {
                        visit(*target);
                    }
                }
            }
        }
    }

    /// Whether `node` is a source of the (production) graph: it has an edge
    /// of that graph, or an initializer group that graph keeps.
    fn is_source(&self, node: u32, production: bool) -> bool {
        let start = self.offsets[node as usize] as usize;
        let end = self.offsets[node as usize + 1] as usize;
        (start..end).any(|edge| !production || self.production[edge])
            || self.node_group.get(&node).is_some_and(|group| {
                let group = &self.groups[*group as usize];
                !(production && group.test_origin) && group.reaches_from(node)
            })
    }

    /// Sources of the (production) graph whose name a dispatched method call
    /// in their language could reach.
    fn dispatch_roots(
        &self,
        names: &HashMap<u32, DispatchNamesForFile<'_>>,
        production: bool,
    ) -> Vec<u32> {
        (0..self.ids.nodes.len() as u32)
            .filter(|node| {
                let (file, symbol) = &self.ids.nodes[*node as usize];
                names
                    .get(file)
                    .is_some_and(|names| names.contains(symbol_liveness_name(symbol)))
                    && self.is_source(*node, production)
            })
            .collect()
    }

    /// Every node `seeds` reach in the (production) graph. Reaching a file's
    /// first node also reaches what that file imports.
    fn reach(&self, seeds: Vec<u32>, production: bool) -> Bitset {
        let mut reached = Bitset::new(self.ids.nodes.len());
        let mut files = Bitset::new(self.ids.files.len());
        let mut queue = VecDeque::from(seeds);
        while let Some(node) = queue.pop_front() {
            if !reached.insert(node) {
                continue;
            }
            let file = self.ids.nodes[node as usize].0;
            if files.insert(file) {
                if let Some(imports) = self.imports.get(&file) {
                    queue.extend(imports.iter().filter(|target| !reached.contains(**target)));
                }
            }
            self.successors(node, production, |target| {
                if !reached.contains(target) {
                    queue.push_back(target);
                }
            });
        }
        reached
    }

    /// Every node `seeds` reach through any edge, without file imports.
    fn closure(&self, seeds: impl IntoIterator<Item = u32>) -> Bitset {
        let mut reached = Bitset::new(self.ids.nodes.len());
        let mut stack = seeds
            .into_iter()
            .filter(|seed| reached.insert(*seed))
            .collect::<Vec<_>>();
        while let Some(node) = stack.pop() {
            self.successors(node, false, |target| {
                if reached.insert(target) {
                    stack.push(target);
                }
            });
        }
        reached
    }

    /// The targets a Go initializer group's `callers` call: every target
    /// some caller other than itself calls.
    fn group_targets_called_by<'s>(
        group: &'s InitializerGroup,
        callers: &[u32],
    ) -> impl Iterator<Item = u32> + 's {
        let mut distinct = callers.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let only = match distinct.as_slice() {
            [] => Some(None),
            [only] => Some(Some(*only)),
            _ => None,
        };
        group
            .targets
            .iter()
            .copied()
            .filter(move |target| match only {
                Some(None) => false,
                Some(Some(caller)) => *target != caller,
                None => true,
            })
    }

    /// The tests that alone use each node only tests use.
    ///
    /// A node is used by tests when a test call reaches it through any
    /// edges. Production code uses a node when a production call, made by a
    /// caller tests do not use, reaches it; that disqualifies everything
    /// downstream. Since production use is closed under edges, every path
    /// from a test to a node production code does not use runs through such
    /// nodes alone, so labels are carried only among them.
    ///
    /// Go package initialization calls count as calls of their group's
    /// file: a test file's are test calls, any other file's production
    /// calls.
    fn test_only_callers(
        &self,
        production_seeds: &[(u32, u32)],
        mut test_seeds: Vec<(u32, u32)>,
    ) -> HashMap<u32, Vec<usize>> {
        for group in &self.groups {
            if let (true, Some(label)) = (group.test_origin, group.label) {
                test_seeds.extend(
                    Self::group_targets_called_by(group, &group.callers)
                        .map(|target| (target, label)),
                );
            }
        }
        let test_used = self.closure(test_seeds.iter().map(|(target, _)| *target));
        let mut production_targets = production_seeds
            .iter()
            .filter(|(source, _)| !test_used.contains(*source))
            .map(|(_, target)| *target)
            .collect::<Vec<_>>();
        for group in self.groups.iter().filter(|group| !group.test_origin) {
            let callers = group
                .callers
                .iter()
                .copied()
                .filter(|caller| !test_used.contains(*caller))
                .collect::<Vec<_>>();
            production_targets.extend(Self::group_targets_called_by(group, &callers));
        }
        let production_used = self.closure(production_targets);
        let mut labels = HashMap::<u32, Vec<usize>>::new();
        let mut pending = Bitset::new(self.ids.nodes.len());
        let mut queue = VecDeque::new();
        for (target, label) in &test_seeds {
            if production_used.contains(*target) {
                continue;
            }
            let entry = labels.entry(*target).or_default();
            let label = *label as usize;
            if let Err(index) = entry.binary_search(&label) {
                entry.insert(index, label);
            }
            if pending.insert(*target) {
                queue.push_back(*target);
            }
        }
        while let Some(node) = queue.pop_front() {
            pending.0[node as usize / 64] &= !(1 << (node % 64));
            let carried = labels.get(&node).cloned().unwrap_or_default();
            let mut next = Vec::new();
            self.successors(node, false, |target| {
                if !production_used.contains(target) {
                    next.push(target);
                }
            });
            for target in next {
                let entry = labels.entry(target).or_default();
                if merge_test_origin_ids(entry, &carried) && pending.insert(target) {
                    queue.push_back(target);
                }
            }
        }
        labels
    }

    fn answers(&self) -> GraphAnswers<'_> {
        GraphAnswers { graph: self }
    }
}

struct GraphAnswers<'g> {
    graph: &'g DeadCodeIntGraph,
}

impl LivenessAnswers for GraphAnswers<'_> {
    fn reachable(&self, file: &str, identity: &str) -> bool {
        self.graph
            .ids
            .find(file, identity)
            .is_some_and(|node| self.graph.reached.contains(node))
    }

    fn production_reachable(&self, file: &str, identity: &str) -> bool {
        self.graph
            .ids
            .find(file, identity)
            .is_some_and(|node| self.graph.production_reached.contains(node))
    }

    fn test_only_callers(&self, file: &str, identity: &str) -> Option<Vec<String>> {
        let node = self.graph.ids.find(file, identity)?;
        let labels = self.graph.test_only.get(&node)?;
        let mut names = labels
            .iter()
            .map(|label| self.graph.labels.text(*label))
            .collect::<Vec<_>>();
        names.sort();
        // A test file whose name ends in `:<line>` spells the same label as
        // that line of a `#[cfg(test)]` region; the label is listed once.
        names.dedup();
        Some(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A file's facts: its exports (symbol, kind) and its calls (caller,
    /// target file, target symbol, made by a test).
    fn facts(
        file: &str,
        exports: &[(&str, &str)],
        calls: &[(&str, &str, &str, bool)],
    ) -> DeadCodeContribution {
        serde_json::from_value(json!({
            "file": file,
            "exports": exports
                .iter()
                .map(|(symbol, kind)| json!({"symbol": symbol, "kind": kind, "line": 1}))
                .collect::<Vec<_>>(),
            "internal_calls": calls
                .iter()
                .map(|(caller, target, symbol, test_origin)| json!({
                    "caller_symbol": caller, "file": target, "symbol": symbol,
                    "line": 1, "test_origin": test_origin,
                }))
                .collect::<Vec<_>>(),
        }))
        .expect("contribution facts")
    }

    /// The graph of already materialized `contributions`, their calls added
    /// as [`Assembler::add`] adds a file's numbered calls.
    fn graph(
        contributions: &[DeadCodeContribution],
        dispatched_method_names: &MethodNamesByLanguage,
    ) -> DeadCodeIntGraph {
        let mut builder = GraphBuilder::default();
        let test_module_files = BTreeSet::new();
        for contribution in contributions {
            for call in &contribution.internal_calls {
                let source = builder.ids.node(&contribution.file, &call.caller_symbol);
                let target = builder.ids.node(&call.file, &call.symbol);
                let origin = match call.test_origin {
                    None => Origin::Unknown,
                    Some(false) => Origin::Production,
                    Some(true) => Origin::Test,
                };
                builder.add_call(
                    contribution,
                    &test_module_files,
                    source,
                    target,
                    !call.caller_symbol.is_empty(),
                    origin,
                    call.line,
                );
            }
            builder.add_file(contribution, &test_module_files);
        }
        builder.finish(contributions, dispatched_method_names)
    }

    /// The tests that alone use each node, by (file, symbol).
    fn test_origins(
        contributions: &[DeadCodeContribution],
    ) -> BTreeMap<(String, String), Vec<String>> {
        let graph = graph(contributions, &MethodNamesByLanguage::new());
        let answers = graph.answers();
        graph
            .ids
            .nodes
            .iter()
            .filter_map(|(file, symbol)| {
                let file = graph.ids.files[*file as usize].to_string();
                let origins = answers.test_only_callers(&file, symbol)?;
                (!origins.is_empty()).then(|| ((file, symbol.to_string()), origins))
            })
            .collect()
    }

    #[test]
    fn test_origins_coalesce_high_fanin_and_keep_cycle_and_production_semantics() {
        // 128 tests call into a cycle; production code calls `cut`, so
        // neither it nor what it calls is test-only.
        let mut contributions = (0..128)
            .map(|n| {
                facts(
                    &format!("test{n}_test.go"),
                    &[],
                    &[
                        ("Test", "helper.go", "entry", true),
                        ("Test", "helper.go", "entry", true),
                        ("Test", "helper.go", "cut", true),
                    ],
                )
            })
            .collect::<Vec<_>>();
        contributions.push(facts(
            "helper.go",
            &[],
            &[
                ("entry", "helper.go", "left", false),
                ("entry", "helper.go", "right", false),
                ("left", "helper.go", "join", false),
                ("right", "helper.go", "join", false),
                ("join", "helper.go", "entry", false),
                ("cut", "helper.go", "sink", false),
                ("production", "helper.go", "cut", false),
            ],
        ));
        let origins = test_origins(&contributions);
        let mut tests = (0..128)
            .map(|n| format!("test{n}_test.go"))
            .collect::<Vec<_>>();
        tests.sort();
        let expected = ["entry", "left", "right", "join"]
            .into_iter()
            .map(|symbol| (("helper.go".to_string(), symbol.to_string()), tests.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(origins, expected);
    }

    #[test]
    fn test_origins_requeue_previously_visited_nodes_for_late_labels() {
        // `join` is reached by TestA first and again, through `relay`, by
        // TestB; `sink` must carry both.
        let contributions = [
            facts("a_test.go", &[], &[("TestA", "helper.go", "join", true)]),
            facts("b_test.go", &[], &[("TestB", "helper.go", "relay", true)]),
            facts(
                "helper.go",
                &[],
                &[
                    ("join", "helper.go", "sink", false),
                    ("relay", "helper.go", "join", false),
                ],
            ),
        ];
        let origins = test_origins(&contributions);
        assert_eq!(
            origins[&("helper.go".into(), "sink".into())],
            vec!["a_test.go", "b_test.go"]
        );
        assert_eq!(
            origins[&("helper.go".into(), "relay".into())],
            vec!["b_test.go"]
        );
    }

    #[test]
    fn interned_test_origins_preserve_distinct_sorted_transitive_labels() {
        let contributions = [
            facts("z_test.go", &[], &[("TestZ", "helper.go", "relay", true)]),
            facts("a_test.go", &[], &[("TestA", "helper.go", "relay", true)]),
            facts("helper.go", &[], &[("relay", "product.go", "Live", false)]),
        ];
        let origins = test_origins(&contributions);
        assert_eq!(
            origins[&("product.go".into(), "Live".into())],
            vec!["a_test.go", "z_test.go"]
        );
        assert_eq!(
            origins[&("helper.go".into(), "relay".into())],
            vec!["a_test.go", "z_test.go"]
        );
    }

    #[test]
    fn dispatch_roots_require_a_contribution_and_a_dispatched_name() {
        // Each source calls a sink of its own, so a source is reached only
        // when it is itself a dispatch root.
        let call = |symbol: &'static str, file: &'static str| (symbol, file, "sink", false);
        let contributions = [
            facts(
                "src/service.ts",
                &[("render", "method")],
                &[
                    call("render", "src/service.ts"),
                    call("handle", "src/service.ts"),
                    call("missing", "src/service.ts"),
                ],
            ),
            facts(
                "src/worker.py",
                &[("process", "function")],
                &[call("process", "src/worker.py")],
            ),
            facts(
                "src/server.go",
                &[
                    ("Serve", "method"),
                    ("Handle", "function"),
                    ("helper", "method"),
                ],
                &[
                    call("Serve", "src/server.go"),
                    call("Handle", "src/server.go"),
                    call("helper", "src/server.go"),
                ],
            ),
            // The file's language has no dispatched names at all.
            facts(
                "src/plain.zig",
                &[("render", "method")],
                &[call("render", "src/plain.zig")],
            ),
        ];
        let dispatched_method_names = MethodNamesByLanguage::from([
            (
                "typescript".to_string(),
                BTreeSet::from(["render".to_string(), "handle".to_string()]),
            ),
            (
                "python".to_string(),
                BTreeSet::from(["process".to_string()]),
            ),
            (
                "go".to_string(),
                BTreeSet::from(["Serve".to_string(), "Handle".to_string()]),
            ),
        ]);
        let graph = graph(&contributions, &dispatched_method_names);
        let answers = graph.answers();
        let reached = [
            // Non-Go: membership is the language's name set, with no
            // method-kind gate.
            ("src/service.ts", "render", true),
            ("src/service.ts", "handle", true),
            ("src/service.ts", "missing", false),
            ("src/worker.py", "process", true),
            // Go: explicit dispatched names apply only to method exports.
            ("src/server.go", "Serve", true),
            ("src/server.go", "Handle", false),
            ("src/server.go", "helper", false),
            ("src/plain.zig", "render", false),
        ];
        for (file, symbol, expected) in reached {
            assert_eq!(
                answers.reachable(file, symbol),
                expected,
                "{file}::{symbol}"
            );
        }
    }
}
