use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::job::{canonicalize_normalized, normalize_path};
use crate::inspect::oxc_engine::{
    analyze_file_facts, AnalyzeOptions, DynamicImportFact, ExportFact, FileFacts, FileId,
    ImportFact, OxcEngineError, OxcEngineResult, OxcResolvedEdge, ReExportFact,
    FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, LangId};

const DRILL_DOWN_LIMIT: usize = 100;
const CYCLE_FACTS_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct CycleEdgeContribution {
    pub to: String,
    pub specifier: String,
    pub kind: String,
    pub line: u32,
    pub edge_kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CycleContribution {
    file: String,
    #[serde(default)]
    edges: Vec<CycleEdgeContribution>,
    #[serde(default)]
    oxc_facts: Option<OxcCycleFactsContribution>,
    #[serde(default)]
    parse_errors: Vec<Value>,
    #[serde(default)]
    skipped_files: Vec<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct OxcCycleFactsPayload<'a> {
    format_version: u32,
    content_hash: &'a str,
    exports: &'a [ExportFact],
    imports: &'a [ImportFact],
    re_exports: &'a [ReExportFact],
    dynamic_imports: &'a [DynamicImportFact],
    same_file_value_references: &'a BTreeSet<String>,
    used_import_bindings: &'a BTreeSet<String>,
    type_referenced_import_bindings: &'a BTreeSet<String>,
    value_referenced_import_bindings: &'a BTreeSet<String>,
    parse_error: &'a Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct OxcCycleFactsContribution {
    format_version: u32,
    content_hash: String,
    exports: Vec<ExportFact>,
    imports: Vec<ImportFact>,
    re_exports: Vec<ReExportFact>,
    dynamic_imports: Vec<DynamicImportFact>,
    same_file_value_references: BTreeSet<String>,
    used_import_bindings: BTreeSet<String>,
    type_referenced_import_bindings: BTreeSet<String>,
    value_referenced_import_bindings: BTreeSet<String>,
    #[serde(default)]
    parse_error: Option<String>,
}

pub fn run_cycles_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    InspectResult::failed(
        job,
        "cycles scanner requires the oxc import graph from Tier-2 reuse",
        started.elapsed(),
    )
}

pub(crate) fn run_cycles_scan_with_oxc(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
) -> InspectResult {
    let started = Instant::now();
    let Some(oxc_result) = oxc_result else {
        return InspectResult::failed(
            job,
            "cycles scanner requires the oxc import graph from Tier-2 reuse",
            started.elapsed(),
        );
    };

    let project_root = canonicalize_normalized(&job.project_root);
    let mut contributions = Vec::new();
    let mut oxc_paths = BTreeSet::new();
    let parse_errors_by_file = parse_errors_by_file(oxc_result);
    let skipped_files = skipped_files_payload(&project_root, oxc_result);
    let mut edges_by_file = cycle_edges_by_file(&project_root, oxc_result);

    for facts in &oxc_result.facts {
        let path = normalize_path(&facts.path);
        oxc_paths.insert(path.clone());
        if let Some(contribution) = cycle_file_contribution(
            &project_root,
            facts,
            edges_by_file.remove(&path).unwrap_or_default(),
            parse_errors_by_file.get(&path),
            &skipped_files,
        ) {
            contributions.push(contribution);
        }
    }

    for error in &oxc_result.errors {
        let path = normalize_path(&error.file);
        if oxc_paths.contains(&path) {
            continue;
        }
        if let Some(contribution) = read_error_contribution(&project_root, error) {
            contributions.push(contribution);
        }
    }

    for path in job.scope_files.iter().map(|path| normalize_path(path)) {
        if oxc_paths.contains(&path) || is_js_ts_file(&path) {
            continue;
        }
        if let Some(contribution) = non_js_empty_contribution(&project_root, &path) {
            contributions.push(contribution);
        }
    }

    contributions.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    let aggregate = aggregate_cycle_contributions_with_limit(
        &project_root,
        &contributions,
        skipped_languages(&job.scope_files),
        Some(DRILL_DOWN_LIMIT),
    );

    let success = InspectScanSuccess {
        scanned_files: contributions
            .iter()
            .map(|contribution| contribution.file_path.clone())
            .collect(),
        contributions,
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

pub(crate) fn aggregate_cycle_contributions_with_limit(
    project_root: &Path,
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
) -> Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<CycleContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();

    let project_root = canonicalize_normalized(project_root);
    let graph = if parsed
        .iter()
        .any(|contribution| contribution.oxc_facts.is_some())
    {
        CycleGraph::from_oxc_facts(&project_root, &parsed)
    } else {
        CycleGraph::from_contributions(&parsed)
    };
    let mut cycles = graph.cycles();
    cycles.sort_by(|left, right| {
        right
            .files
            .len()
            .cmp(&left.files.len())
            .then_with(|| left.files.cmp(&right.files))
    });

    let count = cycles.len();
    let largest = cycles.first().map(|cycle| cycle.files.len()).unwrap_or(0);
    let limit = drill_down_limit.unwrap_or(usize::MAX);
    let items = cycles
        .iter()
        .take(limit)
        .map(CycleReport::to_value)
        .collect::<Vec<_>>();
    let (parse_errors, skipped_files) = honesty_fields(&parsed);

    let mut aggregate = json!({
        "count": count,
        "largest": largest,
        "items": items,
        "drill_down_capped": count > limit,
        "scanned_files": parsed.len(),
        "languages_skipped": languages_skipped,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
        "note": "TS/JS import cycles only; Rust module cycles are out of scope.",
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    aggregate
}

fn cycle_edges_by_file(
    project_root: &Path,
    oxc_result: &OxcEngineResult,
) -> BTreeMap<PathBuf, Vec<CycleEdgeContribution>> {
    let mut edges_by_file = BTreeMap::<PathBuf, Vec<CycleEdgeContribution>>::new();
    for edge in &oxc_result.edges {
        let Some(contribution) = cycle_edge_contribution(project_root, edge) else {
            continue;
        };
        edges_by_file
            .entry(normalize_path(&edge.from_file))
            .or_default()
            .push(contribution);
    }
    for edges in edges_by_file.values_mut() {
        edges.sort_by(|left, right| {
            left.to
                .cmp(&right.to)
                .then_with(|| left.specifier.cmp(&right.specifier))
                .then_with(|| left.line.cmp(&right.line))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        edges.dedup_by(|left, right| {
            left.to == right.to
                && left.specifier == right.specifier
                && left.kind == right.kind
                && left.line == right.line
                && left.edge_kind == right.edge_kind
        });
    }
    edges_by_file
}

fn cycle_edge_contribution(
    project_root: &Path,
    edge: &OxcResolvedEdge,
) -> Option<CycleEdgeContribution> {
    if edge.is_type_only {
        return None;
    }
    let resolved_file = edge.resolved_file.as_ref()?;
    Some(CycleEdgeContribution {
        to: relative_string(project_root, resolved_file),
        specifier: edge.specifier.clone(),
        kind: edge.kind.clone(),
        line: edge.line,
        edge_kind: if edge.kind == "dynamic_import" {
            "dynamic".to_string()
        } else {
            "static".to_string()
        },
    })
}

fn cycle_file_contribution(
    project_root: &Path,
    facts: &FileFacts,
    edges: Vec<CycleEdgeContribution>,
    parse_errors: Option<&Vec<String>>,
    skipped_files: &[Value],
) -> Option<FileContribution> {
    let path = &facts.path;
    let freshness = cache_freshness::collect(path).ok()?;
    let relative = relative_string(project_root, path);
    let facts_payload = OxcCycleFactsPayload {
        format_version: FACTS_FORMAT_VERSION,
        content_hash: &facts.content_hash,
        exports: &facts.exports,
        imports: &facts.imports,
        re_exports: &facts.re_exports,
        dynamic_imports: &facts.dynamic_imports,
        same_file_value_references: &facts.same_file_value_references,
        used_import_bindings: &facts.used_import_bindings,
        type_referenced_import_bindings: &facts.type_referenced_import_bindings,
        value_referenced_import_bindings: &facts.value_referenced_import_bindings,
        parse_error: &facts.parse_error,
    };
    let mut contribution = json!({
        "format_version": CYCLE_FACTS_FORMAT_VERSION,
        "file": relative,
        "edges": edges,
        "oxc_facts": facts_payload,
        "provenance": OXC_PROVENANCE,
    });
    if let Value::Object(object) = &mut contribution {
        if let Some(parse_errors) = parse_errors {
            object.insert(
                "parse_errors".to_string(),
                Value::Array(
                    parse_errors
                        .iter()
                        .map(|message| json!({ "file": relative_string(project_root, path), "message": message }))
                        .collect(),
                ),
            );
        }
        if !skipped_files.is_empty() {
            object.insert(
                "skipped_files".to_string(),
                Value::Array(skipped_files.to_vec()),
            );
        }
    }
    Some(FileContribution::new(
        InspectCategory::Cycles,
        path.to_path_buf(),
        freshness,
        contribution,
    ))
}

fn read_error_contribution(
    project_root: &Path,
    error: &OxcEngineError,
) -> Option<FileContribution> {
    let path = normalize_path(&absolute_path(project_root, &error.file));
    let freshness = freshness_for_existing_file(&path)?;
    let relative = relative_string(project_root, &path);
    let contribution = json!({
        "format_version": CYCLE_FACTS_FORMAT_VERSION,
        "file": relative,
        "edges": [],
        "parse_errors": [{
            "file": relative,
            "message": error.message,
        }],
    });
    Some(FileContribution::new(
        InspectCategory::Cycles,
        path,
        freshness,
        contribution,
    ))
}

fn non_js_empty_contribution(project_root: &Path, path: &Path) -> Option<FileContribution> {
    let freshness = freshness_for_existing_file(path)?;
    let contribution = json!({
        "format_version": CYCLE_FACTS_FORMAT_VERSION,
        "file": relative_string(project_root, path),
        "edges": [],
    });
    Some(FileContribution::new(
        InspectCategory::Cycles,
        path.to_path_buf(),
        freshness,
        contribution,
    ))
}

fn freshness_for_existing_file(path: &Path) -> Option<FileFreshness> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileFreshness {
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        size: metadata.len(),
        content_hash: cache_freshness::zero_hash(),
    })
}

#[derive(Debug, Clone)]
struct CycleReport {
    files: Vec<String>,
    chain: Vec<String>,
    edges: Vec<CycleEdgeReport>,
    edge_kind: String,
}

#[derive(Debug, Clone)]
struct CycleEdgeReport {
    from: String,
    to: String,
    imports: Vec<CycleEdgeContribution>,
    edge_kind: String,
}

impl CycleReport {
    fn to_value(&self) -> Value {
        json!({
            "size": self.files.len(),
            "files": &self.files,
            "chain": &self.chain,
            "cycle": self.chain.join(" -> "),
            "edge_kind": &self.edge_kind,
            "edges": self.edges.iter().map(CycleEdgeReport::to_value).collect::<Vec<_>>(),
        })
    }
}

impl CycleEdgeReport {
    fn to_value(&self) -> Value {
        json!({
            "from": &self.from,
            "to": &self.to,
            "edge_kind": &self.edge_kind,
            "imports": self.imports.iter().map(|import| {
                json!({
                    "specifier": &import.specifier,
                    "kind": &import.kind,
                    "line": import.line,
                    "edge_kind": &import.edge_kind,
                })
            }).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Default)]
struct CycleGraph {
    adjacency: BTreeMap<String, BTreeSet<String>>,
    reverse_adjacency: BTreeMap<String, BTreeSet<String>>,
    edges_by_pair: BTreeMap<(String, String), Vec<CycleEdgeContribution>>,
}

impl CycleGraph {
    fn from_contributions(contributions: &[CycleContribution]) -> Self {
        let mut graph = Self::default();
        for contribution in contributions {
            graph.add_node(contribution.file.clone());
            for edge in &contribution.edges {
                graph.add_edge(&contribution.file, edge.clone());
            }
        }
        graph.sort_edges();
        graph
    }

    fn from_oxc_facts(project_root: &Path, contributions: &[CycleContribution]) -> Self {
        let facts = contributions
            .iter()
            .filter_map(|contribution| {
                let oxc_facts = contribution.oxc_facts.as_ref()?;
                if oxc_facts.format_version != FACTS_FORMAT_VERSION {
                    return None;
                }
                Some(FileFacts {
                    file_id: FileId(0),
                    path: normalize_path(&project_root.join(&contribution.file)),
                    content_hash: oxc_facts.content_hash.clone(),
                    exports: oxc_facts.exports.clone(),
                    imports: oxc_facts.imports.clone(),
                    re_exports: oxc_facts.re_exports.clone(),
                    dynamic_imports: oxc_facts.dynamic_imports.clone(),
                    same_file_value_references: oxc_facts.same_file_value_references.clone(),
                    used_import_bindings: oxc_facts.used_import_bindings.clone(),
                    type_referenced_import_bindings: oxc_facts
                        .type_referenced_import_bindings
                        .clone(),
                    value_referenced_import_bindings: oxc_facts
                        .value_referenced_import_bindings
                        .clone(),
                    parse_error: oxc_facts.parse_error.clone(),
                })
            })
            .collect::<Vec<_>>();
        let oxc_result =
            analyze_file_facts(project_root, facts, AnalyzeOptions::default(), Vec::new());

        let mut graph = Self::default();
        let mut edges_by_file = cycle_edges_by_file(project_root, &oxc_result);
        for facts in &oxc_result.facts {
            let path = normalize_path(&facts.path);
            let from = relative_string(project_root, &path);
            graph.add_node(from.clone());
            for edge in edges_by_file.remove(&path).unwrap_or_default() {
                graph.add_edge(&from, edge);
            }
        }
        graph.sort_edges();
        graph
    }

    fn add_node(&mut self, file: String) {
        self.adjacency.entry(file.clone()).or_default();
        self.reverse_adjacency.entry(file).or_default();
    }

    fn add_edge(&mut self, from: &str, edge: CycleEdgeContribution) {
        self.adjacency
            .entry(from.to_string())
            .or_default()
            .insert(edge.to.clone());
        self.adjacency.entry(edge.to.clone()).or_default();
        self.reverse_adjacency
            .entry(edge.to.clone())
            .or_default()
            .insert(from.to_string());
        self.reverse_adjacency.entry(from.to_string()).or_default();
        self.edges_by_pair
            .entry((from.to_string(), edge.to.clone()))
            .or_default()
            .push(edge);
    }

    fn sort_edges(&mut self) {
        for edges in self.edges_by_pair.values_mut() {
            edges.sort_by(|left, right| {
                edge_rank(&left.edge_kind)
                    .cmp(&edge_rank(&right.edge_kind))
                    .then_with(|| left.specifier.cmp(&right.specifier))
                    .then_with(|| left.line.cmp(&right.line))
                    .then_with(|| left.kind.cmp(&right.kind))
            });
        }
    }

    fn cycles(&self) -> Vec<CycleReport> {
        self.strongly_connected_components()
            .into_iter()
            .filter(|component| {
                component.len() >= 2
                    || component.first().is_some_and(|node| {
                        self.adjacency
                            .get(node)
                            .is_some_and(|neighbors| neighbors.contains(node))
                    })
            })
            .filter_map(|component| self.report_for_component(component))
            .collect()
    }

    fn strongly_connected_components(&self) -> Vec<Vec<String>> {
        let mut visited = BTreeSet::new();
        let mut order = Vec::new();
        for node in self.adjacency.keys() {
            if visited.contains(node) {
                continue;
            }
            self.finish_order(node, &mut visited, &mut order);
        }

        let mut assigned = BTreeSet::new();
        let mut components = Vec::new();
        for node in order.into_iter().rev() {
            if !assigned.insert(node.clone()) {
                continue;
            }
            let mut component = Vec::new();
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                component.push(current.clone());
                if let Some(neighbors) = self.reverse_adjacency.get(&current) {
                    for neighbor in neighbors.iter().rev() {
                        if assigned.insert(neighbor.clone()) {
                            stack.push(neighbor.clone());
                        }
                    }
                }
            }
            component.sort();
            components.push(component);
        }
        components
    }

    fn finish_order(&self, start: &str, visited: &mut BTreeSet<String>, order: &mut Vec<String>) {
        visited.insert(start.to_string());
        let mut stack = vec![(start.to_string(), false)];
        while let Some((node, exiting)) = stack.pop() {
            if exiting {
                order.push(node);
                continue;
            }
            stack.push((node.clone(), true));
            if let Some(neighbors) = self.adjacency.get(&node) {
                for neighbor in neighbors.iter().rev() {
                    if visited.insert(neighbor.clone()) {
                        stack.push((neighbor.clone(), false));
                    }
                }
            }
        }
    }

    fn report_for_component(&self, files: Vec<String>) -> Option<CycleReport> {
        if files.len() == 1 {
            let file = files.first()?.clone();
            let edge = self.edge_report(&file, &file);
            let edge_kind = edge.edge_kind.clone();
            return Some(CycleReport {
                files,
                chain: vec![file.clone(), file],
                edges: vec![edge],
                edge_kind,
            });
        }

        let component_set = files.iter().cloned().collect::<BTreeSet<_>>();
        let mut chain = vec![files.first()?.clone()];
        let mut edges = Vec::new();

        for target in files.iter().skip(1).chain(files.first()) {
            let current = chain.last()?.clone();
            let path = self.shortest_path_in_component(&current, target, &component_set)?;
            for pair in path.windows(2) {
                let from = pair[0].clone();
                let to = pair[1].clone();
                if chain.last() != Some(&to) {
                    chain.push(to.clone());
                }
                edges.push(self.edge_report(&from, &to));
            }
        }

        let edge_kind = cycle_edge_kind(edges.iter().map(|edge| edge.edge_kind.as_str()));
        Some(CycleReport {
            files,
            chain,
            edges,
            edge_kind,
        })
    }

    fn shortest_path_in_component(
        &self,
        start: &str,
        target: &str,
        component: &BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if start == target {
            return Some(vec![start.to_string()]);
        }
        let mut queue = VecDeque::from([start.to_string()]);
        let mut seen = BTreeSet::from([start.to_string()]);
        let mut previous = BTreeMap::<String, String>::new();
        while let Some(node) = queue.pop_front() {
            for neighbor in self.adjacency.get(&node).into_iter().flatten() {
                if !component.contains(neighbor) || !seen.insert(neighbor.clone()) {
                    continue;
                }
                previous.insert(neighbor.clone(), node.clone());
                if neighbor == target {
                    return Some(reconstruct_path(start, target, &previous));
                }
                queue.push_back(neighbor.clone());
            }
        }
        None
    }

    fn edge_report(&self, from: &str, to: &str) -> CycleEdgeReport {
        let imports = self
            .edges_by_pair
            .get(&(from.to_string(), to.to_string()))
            .cloned()
            .unwrap_or_default();
        let edge_kind = cycle_edge_kind(imports.iter().map(|edge| edge.edge_kind.as_str()));
        CycleEdgeReport {
            from: from.to_string(),
            to: to.to_string(),
            imports,
            edge_kind,
        }
    }
}

fn reconstruct_path(start: &str, target: &str, previous: &BTreeMap<String, String>) -> Vec<String> {
    let mut path = vec![target.to_string()];
    let mut current = target;
    while current != start {
        let Some(prior) = previous.get(current) else {
            break;
        };
        path.push(prior.clone());
        current = prior;
    }
    path.reverse();
    path
}

fn edge_rank(edge_kind: &str) -> u8 {
    if edge_kind == "static" {
        0
    } else {
        1
    }
}

fn cycle_edge_kind<'a>(edge_kinds: impl Iterator<Item = &'a str>) -> String {
    let mut saw_static = false;
    let mut saw_dynamic = false;
    for edge_kind in edge_kinds {
        match edge_kind {
            "static" => saw_static = true,
            "dynamic" => saw_dynamic = true,
            _ => {}
        }
    }
    match (saw_static, saw_dynamic) {
        (true, true) => "mixed".to_string(),
        (true, false) => "static".to_string(),
        (false, true) => "dynamic-only".to_string(),
        (false, false) => "unknown".to_string(),
    }
}

fn parse_errors_by_file(oxc_result: &OxcEngineResult) -> BTreeMap<PathBuf, Vec<String>> {
    oxc_result.errors.iter().fold(
        BTreeMap::<PathBuf, Vec<String>>::new(),
        |mut errors, error| {
            errors
                .entry(normalize_path(&error.file))
                .or_default()
                .push(error.message.clone());
            errors
        },
    )
}

fn honesty_fields(contributions: &[CycleContribution]) -> (Vec<Value>, Vec<Value>) {
    let mut parse_error_keys = BTreeSet::new();
    let mut parse_errors = Vec::new();
    let mut skipped_file_keys = BTreeSet::new();
    let mut skipped_files = Vec::new();
    for contribution in contributions {
        for value in &contribution.parse_errors {
            if parse_error_keys.insert(value.to_string()) {
                parse_errors.push(value.clone());
            }
        }
        for value in &contribution.skipped_files {
            if skipped_file_keys.insert(value.to_string()) {
                skipped_files.push(value.clone());
            }
        }
    }
    (parse_errors, skipped_files)
}

fn skipped_files_payload(project_root: &Path, oxc_result: &OxcEngineResult) -> Vec<Value> {
    oxc_result
        .skipped_outside_root
        .iter()
        .map(|file| {
            json!({
                "file": relative_string(project_root, file),
                "message": "outside project root",
            })
        })
        .collect()
}

fn absolute_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    let normalized = normalize_path(&absolute_path(project_root, path));
    normalized
        .strip_prefix(project_root)
        .unwrap_or(&normalized)
        .to_string_lossy()
        .replace('\\', "/")
}

fn skipped_languages(files: &[PathBuf]) -> Vec<String> {
    files
        .iter()
        .filter(|file| !is_js_ts_file(file))
        .map(|file| {
            detect_language(file)
                .map(language_name)
                .unwrap_or("unknown")
                .to_string()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn is_js_ts_file(path: &Path) -> bool {
    detect_language(path).is_some_and(|language| {
        matches!(
            language,
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript
        )
    })
}

fn language_name(language: LangId) -> &'static str {
    match language {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Cuda => "cuda",
        LangId::Metal => "metal",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Yaml => "yaml",
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}
