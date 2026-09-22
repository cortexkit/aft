use std::collections::{
    btree_map::Entry as BTreeMapEntry, hash_map::Entry, BTreeMap, BTreeSet, HashMap, VecDeque,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache_freshness::{self, FileFreshness};
use crate::callgraph::{resolve_module_path, resolve_reexported_symbol_target};
use crate::calls::extract_type_references;
use crate::imports::{parse_imports, specifier_imported_name, specifier_local_name};
use crate::inspect::job::{
    canonicalize_normalized, dead_code_skipped_language, is_test_file, is_test_support_file,
    language_name, CALLGRAPH_PROVENANCE_REEXPORT, CALLGRAPH_PROVENANCE_TREESITTER,
    DISPATCHED_CALLEE_SEPARATOR,
};
use crate::inspect::oxc_engine::{
    analyze_file_facts, AnalyzeOptions, DynamicImportFact, ExportFact, FileFacts, FileId,
    ImportFact, LivenessVerdict, OxcEngineResult, OxcFileVerdicts, OxcReExportContext,
    ReExportFact, ReExportKind, FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::inspect::{
    CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob,
    InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, grammar_for, LangId};

use super::DEFAULT_EXPORT_MARKER_KIND;

const MAX_DRILL_DOWN_ITEMS: usize = 100;
pub(crate) const DEAD_CODE_FACTS_FORMAT_VERSION: u32 = 4;
const MACRO_TOKEN_LIVENESS_PROVENANCE: &str = "macro_token_liveness";
const RUST_MACRO_REF_SHAPE_CALL: &str = "call";
const RUST_MACRO_REF_SHAPE_METHOD: &str = "method";
const RUST_MACRO_REF_SHAPE_STRUCT: &str = "struct";
const TOP_LEVEL_SYMBOL: &str = "<top-level>";

type ExportNode = (String, String);
type OutboundCallsByCallerFile<'a> = BTreeMap<PathBuf, Vec<&'a CallgraphOutboundCall>>;
type MethodNamesByLanguage = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollupKind {
    Incremental,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RollupVerdict {
    pub kind: RollupKind,
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub(crate) struct DeadCodeRollupState {
    all: Arc<ReachabilityState>,
    production: Arc<ReachabilityState>,
    materialized: Arc<Vec<DeadCodeContribution>>,
    contribution_hashes: BTreeMap<String, String>,
    callgraph_hashes: BTreeMap<String, String>,
    public_api_files: BTreeSet<String>,
    roles_fingerprint: String,
    fragments: BTreeMap<String, DeadCodeFileFragment>,
    aggregate: Value,
    drill_down_limit: Option<usize>,
    cache_key: Option<String>,
}

#[derive(Debug, Clone)]
struct DeadCodeFileFragment {
    contribution_hash: String,
    reachable_exports: BTreeSet<String>,
    production_reachable_exports: BTreeSet<String>,
    public_api: bool,
    roles_fingerprint: String,
    headline_items: Vec<Value>,
    generated_items: Vec<Value>,
    test_only_items: Vec<Value>,
    uncertain_items: Vec<Value>,
    by_language: BTreeMap<String, usize>,
}

impl DeadCodeFileFragment {
    fn rendered_items(&self) -> usize {
        self.headline_items.len()
            + self.generated_items.len()
            + self.test_only_items.len()
            + self.uncertain_items.len()
    }
}

#[derive(Debug, Clone)]
struct ReachabilityState {
    edges: BTreeMap<ExportNode, BTreeSet<ExportNode>>,
    imported_by_file: BTreeMap<String, BTreeSet<ExportNode>>,
    namespace_by_file: BTreeMap<String, BTreeSet<ExportNode>>,
    roots: BTreeSet<ExportNode>,
    dispatch_roots: BTreeSet<ExportNode>,
    reachable: BTreeSet<ExportNode>,
}

#[derive(Debug, Default)]
struct ImportedExportLiveness {
    root_exports: Vec<ImportedExportContribution>,
    namespace_exports: Vec<ImportedExportContribution>,
}

#[derive(Debug, Default)]
struct FileAnalysis {
    raw_imports: Vec<RawImportContribution>,
    rust_imports: Vec<RawImportContribution>,
    raw_reexports: Vec<RawReexportContribution>,
    attribute_entry_points: Vec<String>,
    macro_token_refs: Vec<MacroTokenRefContribution>,
    cfg_test_ranges: Vec<RustCfgTestRange>,
    type_ref_names: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct RustMacroToken<'a> {
    text: &'a str,
    kind: &'a str,
    line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct RustCfgTestRange {
    start_line: u32,
    end_line: u32,
}

impl RustCfgTestRange {
    fn contains(self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }
}

#[derive(Debug, Clone)]
struct RustImportedSymbolSpec {
    local_name: String,
    module_segments: Vec<String>,
    imported_name: String,
}

#[derive(Default)]
struct DeadCodeFileAnalyzer {
    parsers: HashMap<LangId, tree_sitter::Parser>,
}

#[derive(Debug, Serialize)]
struct OxcDeadCodeFactsPayload<'a> {
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

impl DeadCodeFileAnalyzer {
    fn analyze_file(&mut self, file: &Path, has_oxc_file: bool) -> FileAnalysis {
        let Some(lang) = detect_language(file) else {
            return FileAnalysis::default();
        };
        let needs_type_refs = supports_type_refs(lang);
        let is_ts_js = matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript);
        // Oxc FileFacts are the raw TS/JS import/re-export/dynamic-import facts.
        // Only the legacy non-oxc TS/JS path needs tree-sitter import/re-export facts here.
        let needs_ts_raw_facts = is_ts_js && !has_oxc_file;
        let needs_rust_reexports = matches!(lang, LangId::Rust);
        let needs_rust_attribute_entry_points = matches!(lang, LangId::Rust);
        let needs_rust_macro_token_refs = matches!(lang, LangId::Rust);

        if !needs_type_refs
            && !needs_ts_raw_facts
            && !needs_rust_reexports
            && !needs_rust_attribute_entry_points
            && !needs_rust_macro_token_refs
        {
            return FileAnalysis::default();
        }

        let Ok(source) = fs::read_to_string(file) else {
            return FileAnalysis::default();
        };
        let needs_tree = needs_type_refs
            || needs_ts_raw_facts
            || needs_rust_attribute_entry_points
            || needs_rust_macro_token_refs;
        let tree = needs_tree
            .then(|| self.parse_source(lang, &source))
            .flatten();

        let type_ref_names = if needs_type_refs {
            tree.as_ref()
                .map(|tree| extract_type_references(&source, tree.root_node(), lang))
                .unwrap_or_default()
        } else {
            BTreeSet::new()
        };

        let raw_imports = if needs_ts_raw_facts {
            tree.as_ref()
                .map(|tree| raw_imports_from_tree(&source, tree, lang))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let rust_imports = if needs_rust_macro_token_refs {
            tree.as_ref()
                .map(|tree| rust_raw_import_contributions(&source, tree))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let raw_reexports = if needs_ts_raw_facts {
            tree.as_ref()
                .map(|tree| ts_raw_reexport_contributions(&source, tree.root_node()))
                .unwrap_or_default()
        } else if needs_rust_reexports {
            rust_raw_reexport_contributions(&source)
        } else {
            Vec::new()
        };

        let attribute_entry_points = if needs_rust_attribute_entry_points {
            tree.as_ref()
                .map(|tree| {
                    let mut roots = BTreeSet::new();
                    for entry in
                        crate::parser::rust_attribute_entry_points(&source, tree.root_node())
                    {
                        roots.insert(entry.name);
                        roots.insert(entry.scoped_name);
                    }
                    roots.into_iter().collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let macro_token_refs = if needs_rust_macro_token_refs {
            tree.as_ref()
                .map(|tree| rust_macro_token_refs(&source, tree.root_node()))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let cfg_test_ranges = if lang == LangId::Rust {
            tree.as_ref()
                .map(|tree| rust_cfg_test_ranges(&source, tree.root_node()))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        FileAnalysis {
            raw_imports,
            rust_imports,
            raw_reexports,
            attribute_entry_points,
            macro_token_refs,
            cfg_test_ranges,
            type_ref_names,
        }
    }

    fn parse_source(&mut self, lang: LangId, source: &str) -> Option<tree_sitter::Tree> {
        let parser = match self.parsers.entry(lang) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let grammar = grammar_for(lang);
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&grammar).is_err() {
                    return None;
                }
                entry.insert(parser)
            }
        };

        parser.parse(source, None)
    }
}

pub fn run_dead_code_scan(job: &InspectJob) -> InspectResult {
    run_dead_code_scan_with_oxc_started(job, None, Instant::now())
}

pub(crate) fn run_dead_code_scan_with_oxc(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
) -> InspectResult {
    run_dead_code_scan_with_oxc_started(job, oxc_result, Instant::now())
}

fn run_dead_code_scan_with_oxc_started(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    started: Instant,
) -> InspectResult {
    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        let success = InspectScanSuccess {
            scanned_files: job.scope_files.clone(),
            contributions: Vec::new(),
            aggregate: callgraph_unavailable_aggregate(job.scope_files.len()),
        };
        return InspectResult::success(job, success, started.elapsed());
    };

    let fallback_exports_by_file = fallback_export_contributions_by_file(job, snapshot);
    let oxc_facts_by_file = oxc_result
        .map(|result| {
            result
                .facts
                .iter()
                .cloned()
                .map(|facts| (relative_path(&job.project_root, &facts.path), facts))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let oxc_parse_errors_by_file = oxc_result
        .map(|result| {
            result.errors.iter().fold(
                BTreeMap::<String, Vec<String>>::new(),
                |mut errors, error| {
                    errors
                        .entry(relative_path(&job.project_root, &error.file))
                        .or_default()
                        .push(error.message.clone());
                    errors
                },
            )
        })
        .unwrap_or_default();
    let oxc_skipped_files = oxc_result
        .map(|result| oxc_skipped_files_payload(&job.project_root, result))
        .unwrap_or_default();

    let cancellation = crate::executor::current_job_cancellation();
    let contributions = job
        .scope_files
        .par_iter()
        .filter_map(|file| {
            if cancellation
                .as_ref()
                .is_some_and(|token| token.cancel_requested_before_commit())
            {
                return None;
            }
            let mut file_analyzer = DeadCodeFileAnalyzer::default();
            Some(gather_file_contribution(
                job,
                file,
                &fallback_exports_by_file,
                &oxc_facts_by_file,
                &oxc_parse_errors_by_file,
                &oxc_skipped_files,
                &mut file_analyzer,
            ))
        })
        .collect::<Vec<_>>();
    if crate::executor::current_job_cancelled() {
        return InspectResult::failed(job, "dead-code scan cancelled", started.elapsed());
    }

    let public_api_files = collect_public_api_files(&job.project_root);
    if crate::executor::current_job_cancelled() {
        return InspectResult::failed(job, "dead-code scan cancelled", started.elapsed());
    }
    let roles = crate::inspect::entry_points::resolve_project_roles(&job.project_root);
    let aggregate = aggregate_dead_code_contributions_with_snapshot(
        &job.project_root,
        snapshot,
        &contributions,
        &public_api_files,
        &roles,
        Some(MAX_DRILL_DOWN_ITEMS),
    );
    let success = InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions,
        aggregate,
    };

    InspectResult::success(job, success, started.elapsed())
}

fn fallback_export_contributions_by_file(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
) -> BTreeMap<String, Vec<ExportContribution>> {
    let mut by_file: BTreeMap<String, Vec<ExportContribution>> = BTreeMap::new();
    for export in &snapshot.exported_symbols {
        if export.kind == DEFAULT_EXPORT_MARKER_KIND {
            continue;
        }
        by_file
            .entry(relative_path(&job.project_root, &export.file))
            .or_default()
            .push(ExportContribution {
                symbol: export.symbol.clone(),
                kind: export.kind.clone(),
                line: export.line,
                is_type_like: is_type_like_kind(&export.kind),
                is_entry_point: false,
                has_references: false,
                test_only_reference_files: Vec::new(),
                verdict: None,
                reason: None,
                provenance: None,
                also_reexported: Vec::new(),
            });
    }
    by_file
}

fn group_outbound_calls_by_caller_file<'a>(
    project_root: &Path,
    outbound_calls: &'a [CallgraphOutboundCall],
) -> OutboundCallsByCallerFile<'a> {
    let mut by_file: OutboundCallsByCallerFile<'a> = BTreeMap::new();
    for call in outbound_calls {
        by_file
            .entry(normalize_absolute(project_root, &call.caller_file))
            .or_default()
            .push(call);
    }
    by_file
}

fn gather_file_contribution(
    job: &InspectJob,
    file: &Path,
    fallback_exports_by_file: &BTreeMap<String, Vec<ExportContribution>>,
    oxc_facts_by_file: &BTreeMap<String, FileFacts>,
    oxc_parse_errors_by_file: &BTreeMap<String, Vec<String>>,
    oxc_skipped_files: &[Value],
    file_analyzer: &mut DeadCodeFileAnalyzer,
) -> FileContribution {
    let file_name = relative_path(&job.project_root, file);
    let generated = crate::inspect::generated::is_generated_file(&job.project_root, file);
    if let Some(language) = dead_code_skipped_language(file) {
        return FileContribution::new(
            InspectCategory::DeadCode,
            file.to_path_buf(),
            collect_freshness(file),
            json!({
                "file": file_name,
                "facts_format_version": DEAD_CODE_FACTS_FORMAT_VERSION,
                "generated": generated,
                "exports": [],
                "skipped_languages": [language],
            }),
        );
    }

    let oxc_facts = oxc_facts_by_file.get(&file_name);
    let exports = oxc_facts
        .map(oxc_fact_export_contributions)
        .unwrap_or_else(|| {
            fallback_exports_by_file
                .get(&file_name)
                .cloned()
                .unwrap_or_default()
        });
    let FileAnalysis {
        raw_imports,
        rust_imports,
        raw_reexports,
        attribute_entry_points,
        macro_token_refs,
        cfg_test_ranges,
        type_ref_names,
    } = file_analyzer.analyze_file(file, oxc_facts.is_some());

    let mut payload = json!({
        "file": file_name,
        "facts_format_version": DEAD_CODE_FACTS_FORMAT_VERSION,
        "generated": generated,
        "exports": exports
            .iter()
            .map(|export| {
                let mut value = json!({
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                });
                if export.is_type_like {
                    value["is_type_like"] = json!(true);
                }
                value
            })
            .collect::<Vec<_>>(),
    });

    if !raw_imports.is_empty() {
        payload["raw_imports"] = json!(raw_imports);
    }
    if !raw_reexports.is_empty() {
        payload["raw_reexports"] = json!(raw_reexports);
    }
    if !rust_imports.is_empty() {
        payload["rust_imports"] = json!(rust_imports);
    }
    if !macro_token_refs.is_empty() {
        payload["macro_token_refs"] = json!(macro_token_refs);
    }
    if !attribute_entry_points.is_empty() {
        payload["attribute_entry_points"] = json!(attribute_entry_points);
    }
    if !cfg_test_ranges.is_empty() {
        payload["cfg_test_ranges"] = json!(cfg_test_ranges);
    }
    if let Some(facts) = oxc_facts {
        payload["provenance"] = json!(OXC_PROVENANCE);
        payload["oxc_facts"] = json!(OxcDeadCodeFactsPayload {
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
        });
    }
    if let Some(parse_errors) = oxc_parse_errors_by_file.get(&file_name) {
        payload["parse_errors"] = json!(parse_errors
            .iter()
            .map(|message| json!({
                "file": file_name,
                "message": message,
            }))
            .collect::<Vec<_>>());
    }
    if oxc_facts.is_some() && !oxc_skipped_files.is_empty() {
        payload["skipped_files"] = Value::Array(oxc_skipped_files.to_vec());
    }

    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        collect_freshness(file),
        payload,
    )
    .with_type_ref_names(type_ref_names)
}

fn oxc_fact_export_contributions(facts: &FileFacts) -> Vec<ExportContribution> {
    facts
        .exports
        .iter()
        .map(|export| ExportContribution {
            symbol: export.name.as_symbol(),
            kind: export.kind.clone(),
            line: export.line,
            is_type_like: export.is_type_only || is_type_like_kind(&export.kind),
            is_entry_point: false,
            has_references: false,
            test_only_reference_files: Vec::new(),
            verdict: None,
            reason: None,
            provenance: None,
            also_reexported: Vec::new(),
        })
        .collect()
}

fn oxc_export_contributions(file: &OxcFileVerdicts) -> Vec<ExportContribution> {
    file.exports
        .iter()
        .map(|export| ExportContribution {
            symbol: export.symbol.clone(),
            kind: export.kind.clone(),
            line: export.line,
            is_type_like: is_type_like_kind(&export.kind),
            is_entry_point: matches!(export.verdict, LivenessVerdict::Used),
            has_references: export.has_references,
            test_only_reference_files: export.test_only_reference_files.clone(),
            verdict: Some(export.verdict),
            reason: Some(export.reason.clone()),
            provenance: Some(export.provenance.clone()),
            also_reexported: export.also_reexported.clone(),
        })
        .collect()
}

fn oxc_skipped_files_payload(project_root: &Path, oxc_result: &OxcEngineResult) -> Vec<Value> {
    oxc_result
        .skipped_outside_root
        .iter()
        .map(|path| {
            json!({
                "file": relative_path(project_root, path),
                "reason": "outside_project_root",
            })
        })
        .collect()
}

pub(crate) fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    callgraph_unavailable_aggregate_with_reason(scanned_files, None)
}

/// Report a terminal callgraph capability gap without inventing a dead-code
/// count. Path-identity gaps include the raw path so an operator can correct a
/// mount or alias mismatch instead of retrying the same unavailable store.
pub(crate) fn callgraph_unavailable_aggregate_with_reason(
    scanned_files: usize,
    reason: Option<&str>,
) -> serde_json::Value {
    let mut aggregate = json!({
        "items": [],
        "by_language": {},
        "languages_skipped": [],
        "drill_down_capped": false,
        "uncertain_count": 0,
        "uncertain_items": [],
        "callgraph_available": false,
        "scanned_files": scanned_files,
        "notes": ["callgraph_unavailable"],
    });
    if let Some(reason) = reason {
        aggregate["notes"] = json!(["callgraph_unavailable", "callgraph_path_identity_mismatch"]);
        aggregate["callgraph_unavailable_reason"] = json!(reason);
    }
    aggregate
}

pub(crate) fn aggregate_dead_code_contributions_with_snapshot(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    aggregate_dead_code_contributions_incremental(
        project_root,
        snapshot,
        contributions,
        public_api_files,
        roles,
        drill_down_limit,
        None,
        None,
        &BTreeSet::new(),
    )
    .0
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn aggregate_dead_code_contributions_incremental(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
    cache_key: Option<&str>,
    previous: Option<&DeadCodeRollupState>,
    changed_files: &BTreeSet<String>,
) -> (serde_json::Value, DeadCodeRollupState, RollupVerdict) {
    let contribution_hashes = contribution_hashes(
        previous.map(|state| &state.contribution_hashes),
        contributions,
        changed_files,
    );
    let callgraph_hashes = callgraph_hashes(
        project_root,
        snapshot,
        previous.map(|state| &state.callgraph_hashes),
        changed_files,
    );
    let roles_fingerprint = format!("{roles:?}");
    let changed_graph_files = previous
        .map(|state| changed_map_keys(&state.callgraph_hashes, &callgraph_hashes))
        .unwrap_or_default();
    if let Some(previous) = previous {
        let contribution_files = contribution_hashes.keys().cloned().collect::<BTreeSet<_>>();
        let _retained_rendered_items = previous
            .fragments
            .values()
            .map(DeadCodeFileFragment::rendered_items)
            .sum::<usize>();
        let fragments_match = previous.fragments.iter().all(|(file, fragment)| {
            contribution_hashes.get(file) == Some(&fragment.contribution_hash)
                && fragment.public_api == public_api_files.contains(file)
                && fragment.roles_fingerprint == roles_fingerprint
                && fragment.reachable_exports
                    == reachable_symbols_for_file(&previous.all.reachable, file)
                && fragment.production_reachable_exports
                    == reachable_symbols_for_file(&previous.production.reachable, file)
        });
        if previous.contribution_hashes == contribution_hashes
            && previous.public_api_files == *public_api_files
            && previous.roles_fingerprint == roles_fingerprint
            && previous.drill_down_limit == drill_down_limit
            && previous.cache_key.as_deref() == cache_key
            && previous.materialized.len() <= contribution_hashes.len()
            && fragments_match
            && changed_files
                .iter()
                .all(|file| !rollup_semantics_file(file))
            && changed_graph_files.is_disjoint(&contribution_files)
        {
            let mut state = previous.clone();
            state.callgraph_hashes = callgraph_hashes;
            return (
                state.aggregate.clone(),
                state,
                RollupVerdict {
                    kind: RollupKind::Incremental,
                    reason: None,
                },
            );
        }
    }

    let parsed = parse_dead_code_contributions(contributions);
    let mut affected_files = changed_files
        .union(&changed_graph_files)
        .cloned()
        .collect::<BTreeSet<_>>();
    if previous.is_none()
        || previous.is_some_and(|state| {
            state.public_api_files != *public_api_files
                || state.roles_fingerprint != roles_fingerprint
        })
        || affected_files
            .iter()
            .any(|file| rollup_semantics_file(file))
        || changed_export_surface(previous, &parsed, &affected_files)
        || parsed.iter().any(|contribution| {
            affected_files.contains(&contribution.file) && contribution.oxc_facts.is_some()
        })
    {
        affected_files = parsed
            .iter()
            .map(|contribution| contribution.file.clone())
            .collect();
    }
    let materialized = Arc::new(materialize_dead_code_contributions(
        project_root,
        snapshot,
        parsed,
        public_api_files,
        previous.map(|state| state.materialized.as_slice()),
        &affected_files,
    ));
    let all_edges = edges_by_source(materialized.as_ref(), false);
    let production_edges = edges_by_source(materialized.as_ref(), true);
    let dispatched_method_names =
        collect_dispatched_method_names_by_language(materialized.as_ref());
    let (all, all_incremental) = build_reachability_state(
        materialized.as_ref(),
        all_edges,
        &dispatched_method_names,
        previous.map(|state| state.all.as_ref()),
        changed_files,
    );
    let (production, production_incremental) = build_reachability_state(
        materialized.as_ref(),
        production_edges,
        &dispatched_method_names,
        previous.map(|state| state.production.as_ref()),
        changed_files,
    );
    let verdict = if all_incremental && production_incremental {
        RollupVerdict {
            kind: RollupKind::Incremental,
            reason: None,
        }
    } else {
        RollupVerdict {
            kind: RollupKind::Full,
            reason: Some("cold"),
        }
    };
    if let Some(previous) = previous {
        affected_files.extend(
            all.reachable
                .symmetric_difference(&previous.all.reachable)
                .map(|node| node.0.clone()),
        );
        affected_files.extend(
            production
                .reachable
                .symmetric_difference(&previous.production.reachable)
                .map(|node| node.0.clone()),
        );
    }
    let all = Arc::new(all);
    let production = Arc::new(production);
    let mut fragments = previous
        .map(|state| state.fragments.clone())
        .unwrap_or_default();
    fragments.retain(|file, _| contribution_hashes.contains_key(file));
    let rendered = materialized
        .iter()
        .filter(|contribution| previous.is_none() || affected_files.contains(&contribution.file))
        .cloned()
        .collect::<Vec<_>>();
    let rendered_aggregate = aggregate_materialized_dead_code_contributions(
        project_root,
        materialized.as_ref(),
        &rendered,
        public_api_files,
        roles,
        None,
        rendered.len(),
        &all.reachable,
        &production.reachable,
        &dispatched_method_names,
    );
    fragments.extend(fragments_from_aggregate(
        &rendered,
        &contribution_hashes,
        public_api_files,
        &roles_fingerprint,
        &all.reachable,
        &production.reachable,
        &rendered_aggregate,
    ));
    let aggregate = fold_dead_code_fragments(
        &fragments,
        materialized.as_ref(),
        roles,
        drill_down_limit,
        contributions.len(),
    );
    (
        aggregate.clone(),
        DeadCodeRollupState {
            all,
            production,
            materialized,
            contribution_hashes,
            callgraph_hashes,
            public_api_files: public_api_files.clone(),
            roles_fingerprint,
            fragments,
            aggregate,
            drill_down_limit,
            cache_key: cache_key.map(str::to_owned),
        },
        verdict,
    )
}

fn rollup_semantics_file(file: &str) -> bool {
    let name = Path::new(file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(file);
    name == "package.json"
        || name == "Cargo.toml"
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
        || (name.starts_with("jsconfig") && name.ends_with(".json"))
        || name.ends_with(".config.js")
        || name.ends_with(".config.ts")
}

fn changed_export_surface(
    previous: Option<&DeadCodeRollupState>,
    parsed: &[DeadCodeContribution],
    affected_files: &BTreeSet<String>,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let old = previous
        .materialized
        .iter()
        .filter(|contribution| affected_files.contains(&contribution.file))
        .map(|contribution| {
            (
                contribution.file.as_str(),
                contribution
                    .exports
                    .iter()
                    .map(|export| export.symbol.as_str())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let new = parsed
        .iter()
        .filter(|contribution| affected_files.contains(&contribution.file))
        .map(|contribution| {
            (
                contribution.file.as_str(),
                contribution
                    .exports
                    .iter()
                    .map(|export| export.symbol.as_str())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    old != new
}

fn contribution_hashes(
    previous: Option<&BTreeMap<String, String>>,
    contributions: &[FileContribution],
    changed_files: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    if let Some(previous) = previous {
        // A contribution that is no longer present must lose its hash (and with
        // it its retained fragment) even when the caller's changed-file set did
        // not name it: a forced deletion arrives spelled by the manager
        // (backslashes on Windows) while these keys use the contribution's own
        // `file` field, so membership in `changed_files` cannot be the only
        // thing that removes a stale key.
        let current_files = contributions
            .iter()
            .map(contribution_file_key)
            .collect::<BTreeSet<_>>();
        let mut hashes = previous.clone();
        hashes.retain(|file, _| current_files.contains(file));
        for file in changed_files {
            hashes.remove(file);
        }
        for contribution in contributions {
            let file = contribution_file_key(contribution);
            if changed_files.contains(&file) || !hashes.contains_key(&file) {
                let bytes = serde_json::to_vec(&contribution.contribution).unwrap_or_default();
                hashes.insert(file, blake3::hash(&bytes).to_hex().to_string());
            }
        }
        return hashes;
    }

    contributions
        .iter()
        .map(|contribution| {
            let file = contribution_file_key(contribution);
            let bytes = serde_json::to_vec(&contribution.contribution).unwrap_or_default();
            (file, blake3::hash(&bytes).to_hex().to_string())
        })
        .collect()
}

fn contribution_file_key(contribution: &FileContribution) -> String {
    contribution
        .contribution
        .get("file")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| contribution.file_path.to_string_lossy().replace('\\', "/"))
}

fn callgraph_hashes(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    previous: Option<&BTreeMap<String, String>>,
    changed_files: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut hashes = previous.cloned().unwrap_or_default();
    let files = if previous.is_some() {
        for file in changed_files {
            hashes.remove(file);
        }
        changed_files.clone()
    } else {
        snapshot
            .outbound_calls
            .iter()
            .map(|call| relative_path(project_root, &call.caller_file))
            .collect()
    };
    let absolute_files = files
        .iter()
        .map(|file| (project_root.join(file), file))
        .collect::<BTreeMap<_, _>>();
    let mut hashers = BTreeMap::<String, blake3::Hasher>::new();
    for call in &snapshot.outbound_calls {
        let Some(file) = absolute_files.get(&call.caller_file) else {
            continue;
        };
        let hasher = hashers.entry((*file).clone()).or_default();
        hasher.update(call.caller_symbol.as_bytes());
        hasher.update(&[0]);
        hasher.update(call.target.as_bytes());
        hasher.update(&call.line.to_le_bytes());
        hasher.update(call.provenance.as_bytes());
    }
    hashes.extend(
        hashers
            .into_iter()
            .map(|(file, hasher)| (file, hasher.finalize().to_hex().to_string())),
    );
    hashes
}

fn changed_map_keys(
    previous: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    previous
        .keys()
        .chain(current.keys())
        .filter(|key| previous.get(*key) != current.get(*key))
        .cloned()
        .collect()
}

fn reachable_symbols_for_file(reachable: &BTreeSet<ExportNode>, file: &str) -> BTreeSet<String> {
    reachable
        .iter()
        .filter(|node| node.0 == file)
        .map(|node| node.1.clone())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn fragments_from_aggregate(
    materialized: &[DeadCodeContribution],
    contribution_hashes: &BTreeMap<String, String>,
    public_api_files: &BTreeSet<String>,
    roles_fingerprint: &str,
    reachable: &BTreeSet<ExportNode>,
    production_reachable: &BTreeSet<ExportNode>,
    aggregate: &Value,
) -> BTreeMap<String, DeadCodeFileFragment> {
    let items_for_file = |key: &str, file: &str| {
        aggregate[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["file"].as_str() == Some(file))
            .cloned()
            .collect::<Vec<_>>()
    };
    materialized
        .iter()
        .map(|contribution| {
            let file = contribution.file.clone();
            (
                file.clone(),
                DeadCodeFileFragment {
                    contribution_hash: contribution_hashes.get(&file).cloned().unwrap_or_default(),
                    reachable_exports: reachable_symbols_for_file(reachable, &file),
                    production_reachable_exports: reachable_symbols_for_file(
                        production_reachable,
                        &file,
                    ),
                    public_api: public_api_files.contains(&file),
                    roles_fingerprint: roles_fingerprint.to_string(),
                    headline_items: items_for_file("items", &file)
                        .into_iter()
                        .filter(|item| item.get("generated").is_none())
                        .collect(),
                    generated_items: items_for_file("generated_items", &file),
                    test_only_items: items_for_file("test_only_items", &file),
                    uncertain_items: items_for_file("uncertain_items", &file),
                    by_language: [(
                        language_for_file(&file).to_string(),
                        aggregate["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|item| {
                                item["file"].as_str() == Some(file.as_str())
                                    && item.get("generated").is_none()
                            })
                            .count(),
                    )]
                    .into_iter()
                    .filter(|(_, count)| *count > 0)
                    .collect(),
                },
            )
        })
        .collect()
}

fn fold_dead_code_fragments(
    fragments: &BTreeMap<String, DeadCodeFileFragment>,
    materialized: &[DeadCodeContribution],
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
    scanned_files: usize,
) -> Value {
    let count = fragments
        .values()
        .map(|fragment| fragment.headline_items.len())
        .sum::<usize>();
    let generated_count = fragments
        .values()
        .map(|fragment| fragment.generated_items.len())
        .sum::<usize>();
    let test_only_count = fragments
        .values()
        .map(|fragment| fragment.test_only_items.len())
        .sum::<usize>();
    let uncertain_count = fragments
        .values()
        .map(|fragment| fragment.uncertain_items.len())
        .sum::<usize>();
    let mut by_language = BTreeMap::<String, usize>::new();
    for fragment in fragments.values() {
        for (language, value) in &fragment.by_language {
            *by_language.entry(language.clone()).or_default() += value;
        }
    }
    let headline_items = crate::inspect::entry_points::rank_and_truncate_items(
        fragments
            .values()
            .flat_map(|fragment| fragment.headline_items.iter().cloned())
            .collect(),
        roles,
        drill_down_limit,
    );
    let generated_items = crate::inspect::entry_points::rank_and_truncate_items(
        fragments
            .values()
            .flat_map(|fragment| fragment.generated_items.iter().cloned())
            .collect(),
        roles,
        drill_down_limit,
    );
    let test_only_items = crate::inspect::entry_points::rank_and_truncate_items(
        fragments
            .values()
            .flat_map(|fragment| fragment.test_only_items.iter().cloned())
            .collect(),
        roles,
        drill_down_limit,
    );
    let mut uncertain_items = fragments
        .values()
        .flat_map(|fragment| fragment.uncertain_items.iter().cloned())
        .collect::<Vec<_>>();
    if let Some(limit) = drill_down_limit {
        uncertain_items.truncate(limit);
    }
    let top = crate::inspect::entry_points::top_preview_symbols(&headline_items);
    let mut dead_items = headline_items;
    dead_items.extend(generated_items.iter().cloned());
    if let Some(limit) = drill_down_limit {
        dead_items.truncate(limit);
    }
    let generated_top = generated_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let test_only_top = test_only_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let (parse_errors, skipped_files, languages_skipped) = dead_code_honesty_fields(materialized);
    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": dead_items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "by_language": by_language,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count + generated_count > limit),
        "generated_drill_down_capped": drill_down_limit.is_some_and(|limit| generated_count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "languages_skipped": languages_skipped,
        "callgraph_available": true,
        "scanned_files": scanned_files,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    aggregate
}

fn parse_dead_code_contributions(contributions: &[FileContribution]) -> Vec<DeadCodeContribution> {
    contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DeadCodeContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>()
}

fn materialize_dead_code_contributions(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    parsed: Vec<DeadCodeContribution>,
    public_api_files: &BTreeSet<String>,
    previous: Option<&[DeadCodeContribution]>,
    affected_files: &BTreeSet<String>,
) -> Vec<DeadCodeContribution> {
    let liveness_root_files = snapshot
        .entry_points
        .iter()
        .map(|file| relative_path(project_root, file))
        .collect::<BTreeSet<_>>();
    let executable_root_exports_by_file =
        crate::inspect::entry_points::resolve_entry_points(project_root)
            .executable_root_exports()
            .into_iter()
            .map(|(file, exports)| (relative_path(project_root, &file), exports))
            .collect::<BTreeMap<_, _>>();
    let attribute_roots_from_snapshot = snapshot
        .entry_point_symbols
        .iter()
        .map(|(file, symbols)| (relative_path(project_root, file), symbols.clone()))
        .collect::<BTreeMap<_, _>>();
    let (exported_symbols_by_file, files_by_exported_symbol, default_export_symbols_by_file) =
        exported_symbol_indexes_from_contributions(project_root, snapshot, &parsed);
    let outbound_calls_by_caller_file =
        group_outbound_calls_by_caller_file(project_root, &snapshot.outbound_calls);
    let full_materialization = previous.is_none() || affected_files.len() >= parsed.len();
    let oxc_by_file = if full_materialization {
        oxc_verdicts_by_file(project_root, snapshot, &parsed, public_api_files)
    } else {
        BTreeMap::new()
    };
    let previous_by_file = previous
        .into_iter()
        .flatten()
        .map(|contribution| (contribution.file.as_str(), contribution))
        .collect::<BTreeMap<_, _>>();

    parsed
        .into_iter()
        .map(|mut contribution| {
            if !affected_files.contains(&contribution.file) {
                if let Some(previous) = previous_by_file.get(contribution.file.as_str()) {
                    return (*previous).clone();
                }
            }
            let _facts_format_version = contribution.facts_format_version;
            let absolute_file = project_root.join(&contribution.file);
            let normalized_file = normalize_absolute(project_root, &absolute_file);
            let outbound_calls_for_file = outbound_calls_by_caller_file
                .get(&normalized_file)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut exports = oxc_by_file
                .get(&contribution.file)
                .map(oxc_export_contributions)
                .unwrap_or_else(|| contribution.exports.clone());

            let mut internal_calls = outbound_calls_for_file
                .iter()
                .copied()
                .filter_map(|call| {
                    project_internal_call(
                        project_root,
                        call,
                        &contribution.file,
                        is_test_file(&contribution.file)
                            || contribution
                                .cfg_test_ranges
                                .iter()
                                .any(|range| range.contains(call.line)),
                        &exported_symbols_by_file,
                        &files_by_exported_symbol,
                    )
                })
                .collect::<Vec<_>>();
            internal_calls.extend(resolve_raw_reexport_liveness_edges(
                project_root,
                &contribution.file,
                &contribution.raw_reexports,
                &exported_symbols_by_file,
                &default_export_symbols_by_file,
            ));
            if let Some(oxc_facts) = &contribution.oxc_facts {
                internal_calls.extend(resolve_oxc_reexport_liveness_edges(
                    project_root,
                    &contribution.file,
                    oxc_facts,
                    &exported_symbols_by_file,
                    &default_export_symbols_by_file,
                ));
            }
            internal_calls.extend(resolve_macro_token_liveness_edges(
                project_root,
                &contribution.file,
                &contribution.macro_token_refs,
                &contribution.rust_imports,
                &exported_symbols_by_file,
            ));
            sort_dedup_internal_calls(&mut internal_calls);

            let dispatched_method_names = outbound_calls_for_file
                .iter()
                .copied()
                .flat_map(|call| dispatched_method_names_from_call(call, &contribution.file))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let imported_export_liveness = resolve_raw_imported_export_liveness_roots(
                project_root,
                &contribution.file,
                &contribution.raw_imports,
                &exported_symbols_by_file,
                &default_export_symbols_by_file,
            );
            let mut attribute_entry_points = contribution
                .attribute_entry_points
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if let Some(snapshot_roots) = attribute_roots_from_snapshot.get(&contribution.file) {
                attribute_entry_points.extend(snapshot_roots.iter().cloned());
            }
            let liveness_roots = liveness_roots_for_file(
                &contribution.file,
                &exports,
                &internal_calls,
                &attribute_entry_points,
                executable_root_exports_by_file.get(&contribution.file),
                liveness_root_files.contains(&contribution.file),
                public_api_files.contains(&contribution.file),
            );
            for export in &mut exports {
                export.is_entry_point = liveness_roots.contains(&export.symbol);
            }

            contribution.exports = exports;
            contribution.internal_calls = internal_calls
                .into_iter()
                .map(InternalCallContribution::from)
                .collect();
            contribution.liveness_roots = liveness_roots;
            contribution.imported_exports = imported_export_liveness.root_exports;
            contribution.namespace_imported_exports = imported_export_liveness.namespace_exports;
            contribution.dispatched_method_names = dispatched_method_names;
            contribution
        })
        .collect()
}

fn exported_symbol_indexes_from_contributions(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[DeadCodeContribution],
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, String>,
) {
    let mut exported_symbols_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files_by_exported_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut default_export_symbols_by_file: BTreeMap<String, String> = BTreeMap::new();

    for contribution in contributions {
        for export in &contribution.exports {
            exported_symbols_by_file
                .entry(contribution.file.clone())
                .or_default()
                .insert(export.symbol.clone());
            files_by_exported_symbol
                .entry(export.symbol.clone())
                .or_default()
                .insert(contribution.file.clone());
        }
    }

    for export in &snapshot.exported_symbols {
        let file = relative_path(project_root, &export.file);
        if export.kind == DEFAULT_EXPORT_MARKER_KIND {
            default_export_symbols_by_file.insert(file, export.symbol.clone());
        }
    }

    (
        exported_symbols_by_file,
        files_by_exported_symbol,
        default_export_symbols_by_file,
    )
}

fn oxc_verdicts_by_file(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[DeadCodeContribution],
    public_api_files: &BTreeSet<String>,
) -> BTreeMap<String, OxcFileVerdicts> {
    let facts = contributions
        .iter()
        .filter_map(|contribution| {
            let oxc_facts = contribution.oxc_facts.as_ref()?;
            if oxc_facts.format_version != FACTS_FORMAT_VERSION {
                return None;
            }
            Some(FileFacts {
                file_id: FileId(0),
                path: canonical_or_normalized(project_root, &project_root.join(&contribution.file)),
                content_hash: oxc_facts.content_hash.clone(),
                exports: oxc_facts.exports.clone(),
                imports: oxc_facts.imports.clone(),
                re_exports: oxc_facts.re_exports.clone(),
                dynamic_imports: oxc_facts.dynamic_imports.clone(),
                same_file_value_references: oxc_facts.same_file_value_references.clone(),
                used_import_bindings: oxc_facts.used_import_bindings.clone(),
                type_referenced_import_bindings: oxc_facts.type_referenced_import_bindings.clone(),
                value_referenced_import_bindings: oxc_facts
                    .value_referenced_import_bindings
                    .clone(),
                parse_error: oxc_facts.parse_error.clone(),
            })
        })
        .collect::<Vec<_>>();
    if facts.is_empty() {
        return BTreeMap::new();
    }

    let entry_points = crate::inspect::entry_points::resolve_entry_points(project_root);
    analyze_file_facts(
        project_root,
        facts,
        AnalyzeOptions {
            entry_points: snapshot.entry_points.iter().cloned().collect(),
            public_api_files: public_api_files
                .iter()
                .map(|file| project_root.join(file))
                .collect(),
            executable_root_exports: entry_points.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: true,
        },
        Vec::new(),
    )
    .files
    .into_iter()
    .map(|file| (file.relative_file.clone(), file))
    .collect()
}

fn sort_dedup_internal_calls(internal_calls: &mut Vec<InternalCall>) {
    internal_calls.sort_by(|left, right| {
        left.caller_symbol
            .cmp(&right.caller_symbol)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.symbol.cmp(&right.symbol))
            .then_with(|| left.line.cmp(&right.line))
            .then_with(|| left.provenance.cmp(&right.provenance))
            .then_with(|| left.test_origin.cmp(&right.test_origin))
    });
    internal_calls.dedup_by(|left, right| {
        left.caller_symbol == right.caller_symbol
            && left.file == right.file
            && left.symbol == right.symbol
            && left.line == right.line
            && left.provenance == right.provenance
            && left.test_origin == right.test_origin
    });
}

fn aggregate_materialized_dead_code_contributions(
    project_root: &Path,
    facts: &[DeadCodeContribution],
    parsed: &[DeadCodeContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
    scanned_files: usize,
    reachable: &BTreeSet<ExportNode>,
    production_reachable: &BTreeSet<ExportNode>,
    dispatched_method_names: &MethodNamesByLanguage,
) -> serde_json::Value {
    let test_only_callers = test_only_callers_by_target(facts);
    let referenced_type_names = collect_referenced_type_names(facts);

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut count = 0usize;
    let mut headline_items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let mut test_only_count = 0usize;
    let mut test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items: Vec<serde_json::Value> = Vec::new();
    for contribution in parsed {
        let generated_file = crate::inspect::generated::is_generated_file_with_cached_hint(
            project_root,
            &contribution.file,
            contribution.generated,
        );
        // Test-support files (fixtures, corpora, mock data) are consumed by
        // path, never imported, so their exports always look dead. Skip
        // REPORTING them — their edges already kept real code live above.
        if is_test_support_file(&contribution.file) {
            continue;
        }
        let is_public_api_file = public_api_files.contains(&contribution.file);
        for export in &contribution.exports {
            if export_uses_oxc(export) {
                match export.verdict.unwrap_or(LivenessVerdict::Unused) {
                    LivenessVerdict::Used => {
                        if !is_test_file(&contribution.file)
                            && !export.test_only_reference_files.is_empty()
                        {
                            let mut item = json!({
                                "file": contribution.file,
                                "symbol": export.symbol,
                                "kind": export.kind,
                                "line": export.line,
                                "provenance": export.provenance.as_deref().unwrap_or(OXC_PROVENANCE),
                                "used_by": export.test_only_reference_files,
                            });
                            add_reexport_contexts(&mut item, &export.also_reexported);
                            if generated_file {
                                item["generated"] = json!(true);
                                generated_count += 1;
                                generated_items.push(item);
                            } else {
                                test_only_count += 1;
                                test_only_items.push(item);
                            }
                        }
                        continue;
                    }
                    LivenessVerdict::Uncertain => {
                        uncertain_count += 1;
                        if drill_down_limit.is_none_or(|limit| uncertain_items.len() < limit) {
                            let mut item = json!({
                                "file": contribution.file,
                                "symbol": export.symbol,
                                "kind": export.kind,
                                "line": export.line,
                                "reason": export.reason.as_deref().unwrap_or("oxc_uncertain"),
                                "provenance": export.provenance.as_deref().unwrap_or(OXC_PROVENANCE),
                            });
                            add_reexport_contexts(&mut item, &export.also_reexported);
                            uncertain_items.push(item);
                        }
                        continue;
                    }
                    LivenessVerdict::Unused => {
                        if !is_test_file(&contribution.file)
                            && !export.test_only_reference_files.is_empty()
                        {
                            let mut item = json!({
                                "file": contribution.file,
                                "symbol": export.symbol,
                                "kind": export.kind,
                                "line": export.line,
                                "provenance": export.provenance.as_deref().unwrap_or(OXC_PROVENANCE),
                                "used_by": export.test_only_reference_files,
                            });
                            add_reexport_contexts(&mut item, &export.also_reexported);
                            if generated_file {
                                item["generated"] = json!(true);
                                generated_count += 1;
                                generated_items.push(item);
                            } else {
                                test_only_count += 1;
                                test_only_items.push(item);
                            }
                            continue;
                        }
                        if export.has_references {
                            continue;
                        }
                    }
                }
            } else {
                let node = (contribution.file.clone(), export.symbol.clone());
                if !is_test_file(&contribution.file)
                    && !is_public_api_file
                    && !export.is_entry_point
                    && !production_reachable.contains(&node)
                    && test_only_callers.contains_key(&node)
                {
                    let item = json!({
                        "file": contribution.file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "provenance": CALLGRAPH_PROVENANCE_TREESITTER,
                        "used_by": test_only_callers.get(&node).cloned().unwrap_or_default(),
                    });
                    if generated_file {
                        let mut item = item;
                        item["generated"] = json!(true);
                        generated_count += 1;
                        generated_items.push(item);
                    } else {
                        test_only_count += 1;
                        test_only_items.push(item);
                    }
                    continue;
                }
                if reachable.contains(&node)
                    || is_public_api_file
                    || dispatch_liveness_keeps_export_live(
                        contribution,
                        export,
                        &dispatched_method_names,
                    )
                {
                    continue;
                }

                if (export.is_type_like || is_type_like_kind(&export.kind))
                    && referenced_type_names.contains(symbol_liveness_name(&export.symbol))
                {
                    continue;
                }
            }

            let mut item = json!({
                "file": contribution.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            });
            if let Some(provenance) = &export.provenance {
                item["provenance"] = json!(provenance);
            }
            add_reexport_contexts(&mut item, &export.also_reexported);
            if generated_file {
                item["generated"] = json!(true);
                generated_count += 1;
                generated_items.push(item);
            } else {
                count += 1;
                *by_language
                    .entry(language_for_file(&contribution.file).to_string())
                    .or_default() += 1;
                headline_items.push(item);
            }
        }
    }

    let headline_items = crate::inspect::entry_points::rank_and_truncate_items(
        headline_items,
        roles,
        drill_down_limit,
    );
    let generated_items = crate::inspect::entry_points::rank_and_truncate_items(
        generated_items,
        roles,
        drill_down_limit,
    );
    let top = crate::inspect::entry_points::top_preview_symbols(&headline_items);
    let mut dead_items = headline_items;
    dead_items.extend(generated_items.iter().cloned());
    if let Some(limit) = drill_down_limit {
        dead_items.truncate(limit);
    }
    let generated_top = generated_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let test_only_items = crate::inspect::entry_points::rank_and_truncate_items(
        test_only_items,
        roles,
        drill_down_limit,
    );
    let test_only_top = test_only_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();

    let (parse_errors, skipped_files, languages_skipped) = dead_code_honesty_fields(parsed);
    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": dead_items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "by_language": by_language,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count + generated_count > limit),
        "generated_drill_down_capped": drill_down_limit.is_some_and(|limit| generated_count > limit),
        "test_only_drill_down_capped": drill_down_limit.is_some_and(|limit| test_only_count > limit),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "languages_skipped": languages_skipped,
        "callgraph_available": true,
        "scanned_files": scanned_files,
        "complete": parse_errors.is_empty() && skipped_files.is_empty(),
    });
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    aggregate
}

fn add_reexport_contexts(item: &mut Value, contexts: &[OxcReExportContext]) {
    if !contexts.is_empty() {
        item["also_reexported"] = json!(contexts);
    }
}

fn export_uses_oxc(export: &ExportContribution) -> bool {
    export.verdict.is_some() || export.provenance.as_deref() == Some(OXC_PROVENANCE)
}

fn dead_code_honesty_fields(
    parsed: &[DeadCodeContribution],
) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let mut parse_error_keys = BTreeSet::new();
    let mut parse_errors = Vec::new();
    let mut skipped_file_keys = BTreeSet::new();
    let mut skipped_files = Vec::new();
    let mut languages_skipped = BTreeSet::new();
    for contribution in parsed {
        for value in &contribution.parse_errors {
            let key = value.to_string();
            if parse_error_keys.insert(key) {
                parse_errors.push(value.clone());
            }
        }
        for value in &contribution.skipped_files {
            let key = value.to_string();
            if skipped_file_keys.insert(key) {
                skipped_files.push(value.clone());
            }
        }
        languages_skipped.extend(contribution.skipped_languages.iter().cloned());
    }
    (
        parse_errors,
        skipped_files,
        languages_skipped.into_iter().collect(),
    )
}

fn edges_by_source(
    contributions: &[DeadCodeContribution],
    exclude_test_origins: bool,
) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
    let mut edges: BTreeMap<ExportNode, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        for call in &contribution.internal_calls {
            if exclude_test_origins && call.test_origin == Some(true) {
                continue;
            }
            // Keep EVERY resolved edge, regardless of whether the target is an
            // exported symbol. Liveness must traverse through private
            // intermediaries (a private router/helper that forwards a root to a
            // public handler). Restricting targets to exports severed the chain
            // at the first private hop and made every handler reachable only via
            // a private function look dead. Node identity is (file, symbol);
            // private and exported symbols share the same node space.
            if call.caller_symbol.is_empty() {
                continue;
            }
            let target = (call.file.clone(), call.symbol.clone());
            let source = (contribution.file.clone(), call.caller_symbol.clone());
            edges.entry(source).or_default().insert(target);
        }
    }

    edges
}

fn test_only_callers_by_target(
    contributions: &[DeadCodeContribution],
) -> BTreeMap<ExportNode, Vec<String>> {
    let mut callers: BTreeMap<ExportNode, (bool, BTreeSet<String>)> = BTreeMap::new();
    for contribution in contributions {
        for call in &contribution.internal_calls {
            let Some(test_origin) = call.test_origin else {
                continue;
            };
            let target = (call.file.clone(), call.symbol.clone());
            let summary = callers
                .entry(target)
                .or_insert_with(|| (true, BTreeSet::new()));
            if test_origin {
                summary.1.insert(contribution.file.clone());
            } else {
                summary.0 = false;
            }
        }
    }
    callers
        .into_iter()
        .filter_map(|(target, (all_test, files))| {
            (all_test && !files.is_empty()).then(|| (target, files.into_iter().collect()))
        })
        .collect()
}

fn collect_dispatched_method_names_by_language(
    contributions: &[DeadCodeContribution],
) -> MethodNamesByLanguage {
    let mut by_language: MethodNamesByLanguage = BTreeMap::new();
    for contribution in contributions {
        let language = language_for_file(&contribution.file).to_string();
        by_language
            .entry(language)
            .or_default()
            .extend(contribution.dispatched_method_names.iter().cloned());
    }
    by_language
}

fn collect_referenced_type_names(contributions: &[DeadCodeContribution]) -> BTreeSet<String> {
    // A type-like export is live if it is referenced in type position ANYWHERE
    // in the project — not only from call-reachable files. Filtering by
    // call-reachability under-approximates
    // liveness: the cross-file call graph is incomplete (constructor/method
    // edges, workspace-package boundaries), so genuinely-used types referenced
    // from files the call graph fails to mark reachable were flagged dead.
    // This mirrors `collect_dispatched_method_names`, which is also unfiltered,
    // and keeps dead_code biased toward under-reporting (it is a hint, not
    // authority): a type with zero type-references anywhere is still precise
    // dead.
    contributions
        .iter()
        .flat_map(|contribution| contribution.type_ref_names.iter().cloned())
        .collect()
}

fn build_reachability_state(
    contributions: &[DeadCodeContribution],
    edges: BTreeMap<ExportNode, BTreeSet<ExportNode>>,
    dispatched_method_names: &MethodNamesByLanguage,
    previous: Option<&ReachabilityState>,
    changed_files: &BTreeSet<String>,
) -> (ReachabilityState, bool) {
    let mut current = reachability_inputs(contributions, edges, dispatched_method_names);
    let Some(previous) = previous else {
        current.reachable = traverse_reachable(
            &current,
            BTreeSet::new(),
            current.roots.iter().chain(&current.dispatch_roots).cloned(),
        );
        return (current, false);
    };

    current.reachable = incremental_reachable(previous, &current, changed_files);
    (current, true)
}

fn reachability_inputs(
    contributions: &[DeadCodeContribution],
    edges: BTreeMap<ExportNode, BTreeSet<ExportNode>>,
    dispatched_method_names: &MethodNamesByLanguage,
) -> ReachabilityState {
    let mut roots = BTreeSet::new();
    for contribution in contributions {
        roots.extend(
            contribution
                .liveness_roots
                .iter()
                .map(|root| (contribution.file.clone(), root.clone())),
        );
        roots.extend(
            contribution
                .exports
                .iter()
                .filter(|export| export.is_entry_point)
                .map(|export| (contribution.file.clone(), export.symbol.clone())),
        );
    }

    let dispatch_live_source_names_by_file =
        dispatch_live_source_names_by_file(contributions, dispatched_method_names);
    let dispatch_roots = edges
        .keys()
        .filter(|source| {
            dispatch_live_source_names_by_file
                .get(source.0.as_str())
                .is_some_and(|entry| entry.contains(symbol_liveness_name(&source.1)))
        })
        .cloned()
        .collect();

    ReachabilityState {
        edges,
        imported_by_file: imported_exports_by_file(contributions),
        namespace_by_file: namespace_imported_exports_by_file(contributions),
        roots,
        dispatch_roots,
        reachable: BTreeSet::new(),
    }
}

fn incremental_reachable(
    previous: &ReachabilityState,
    current: &ReachabilityState,
    changed_files: &BTreeSet<String>,
) -> BTreeSet<ExportNode> {
    if changed_files.is_empty()
        && previous.edges == current.edges
        && previous.imported_by_file == current.imported_by_file
        && previous.namespace_by_file == current.namespace_by_file
        && previous.roots == current.roots
        && previous.dispatch_roots == current.dispatch_roots
    {
        return previous.reachable.clone();
    }

    let mut frontier = BTreeSet::new();
    for source in previous.edges.keys().chain(current.edges.keys()) {
        if changed_files.contains(&source.0)
            || previous.edges.get(source) != current.edges.get(source)
        {
            frontier.insert(source.clone());
            frontier.extend(previous.edges.get(source).into_iter().flatten().cloned());
            frontier.extend(current.edges.get(source).into_iter().flatten().cloned());
        }
    }
    frontier.extend(previous.roots.symmetric_difference(&current.roots).cloned());
    frontier.extend(
        previous
            .dispatch_roots
            .symmetric_difference(&current.dispatch_roots)
            .cloned(),
    );
    let import_files = previous
        .imported_by_file
        .keys()
        .chain(current.imported_by_file.keys())
        .chain(previous.namespace_by_file.keys())
        .chain(current.namespace_by_file.keys())
        .collect::<BTreeSet<_>>();
    for file in import_files {
        if previous.imported_by_file.get(file) != current.imported_by_file.get(file) {
            frontier.extend(
                previous
                    .imported_by_file
                    .get(file)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            frontier.extend(
                current
                    .imported_by_file
                    .get(file)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        if previous.namespace_by_file.get(file) != current.namespace_by_file.get(file) {
            frontier.extend(
                previous
                    .namespace_by_file
                    .get(file)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            frontier.extend(
                current
                    .namespace_by_file
                    .get(file)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
    }

    // A removed edge can orphan its complete downstream component, including a
    // cycle. Invalidate that old component before seeding it again from roots or
    // unaffected live inbound edges in the new graph.
    expand_frontier(previous, &mut frontier);
    expand_frontier(current, &mut frontier);

    let mut retained = previous
        .reachable
        .difference(&frontier)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut seeds = current
        .roots
        .iter()
        .chain(&current.dispatch_roots)
        .filter(|node| frontier.contains(*node))
        .cloned()
        .collect::<Vec<_>>();

    for (source, targets) in &current.edges {
        if retained.contains(source) {
            seeds.extend(
                targets
                    .iter()
                    .filter(|target| frontier.contains(*target))
                    .cloned(),
            );
        }
    }
    let retained_files = retained
        .iter()
        .map(|node| node.0.as_str())
        .collect::<BTreeSet<_>>();
    for file in retained_files {
        seeds.extend(
            current
                .imported_by_file
                .get(file)
                .into_iter()
                .chain(current.namespace_by_file.get(file))
                .flatten()
                .filter(|target| frontier.contains(*target))
                .cloned(),
        );
    }

    retained = traverse_reachable_in_frontier(current, retained, seeds, &frontier);
    retained
}

fn expand_frontier(state: &ReachabilityState, frontier: &mut BTreeSet<ExportNode>) {
    let mut queue = frontier.iter().cloned().collect::<VecDeque<_>>();
    let mut expanded_files = BTreeSet::new();
    while let Some(node) = queue.pop_front() {
        if expanded_files.insert(node.0.clone()) {
            for target in state
                .imported_by_file
                .get(&node.0)
                .into_iter()
                .chain(state.namespace_by_file.get(&node.0))
                .flatten()
            {
                if frontier.insert(target.clone()) {
                    queue.push_back(target.clone());
                }
            }
        }
        if let Some(targets) = state.edges.get(&node) {
            for target in targets {
                if frontier.insert(target.clone()) {
                    queue.push_back(target.clone());
                }
            }
        }
    }
}

fn traverse_reachable(
    state: &ReachabilityState,
    reachable: BTreeSet<ExportNode>,
    seeds: impl IntoIterator<Item = ExportNode>,
) -> BTreeSet<ExportNode> {
    traverse_reachable_inner(state, reachable, seeds, None)
}

fn traverse_reachable_in_frontier(
    state: &ReachabilityState,
    reachable: BTreeSet<ExportNode>,
    seeds: impl IntoIterator<Item = ExportNode>,
    frontier: &BTreeSet<ExportNode>,
) -> BTreeSet<ExportNode> {
    traverse_reachable_inner(state, reachable, seeds, Some(frontier))
}

fn traverse_reachable_inner(
    state: &ReachabilityState,
    mut reachable: BTreeSet<ExportNode>,
    seeds: impl IntoIterator<Item = ExportNode>,
    frontier: Option<&BTreeSet<ExportNode>>,
) -> BTreeSet<ExportNode> {
    let mut queue = seeds.into_iter().collect::<VecDeque<_>>();
    let mut expanded_file_imports = reachable
        .iter()
        .map(|node| node.0.clone())
        .collect::<BTreeSet<_>>();
    while let Some(node) = queue.pop_front() {
        if frontier.is_some_and(|nodes| !nodes.contains(&node)) || !reachable.insert(node.clone()) {
            continue;
        }
        if expanded_file_imports.insert(node.0.clone()) {
            queue.extend(
                state
                    .imported_by_file
                    .get(&node.0)
                    .into_iter()
                    .chain(state.namespace_by_file.get(&node.0))
                    .flatten()
                    .filter(|target| !reachable.contains(*target))
                    .cloned(),
            );
        }
        queue.extend(
            state
                .edges
                .get(&node)
                .into_iter()
                .flatten()
                .filter(|target| !reachable.contains(*target))
                .cloned(),
        );
    }
    reachable
}

/// Per-file dispatch membership for the reachability projection.
///
/// A non-Go file's dispatch roots are exactly the names in its language's
/// dispatched-name set, so that set is borrowed once per language instead of
/// being copied into every file's entry. Go is genuinely per-file: only the
/// methods that file itself exports can be dispatch roots.
enum DispatchNamesForFile<'a> {
    Language(&'a BTreeSet<String>),
    GoMethods(BTreeSet<String>),
}

impl DispatchNamesForFile<'_> {
    fn contains(&self, name: &str) -> bool {
        match self {
            DispatchNamesForFile::Language(names) => names.contains(name),
            DispatchNamesForFile::GoMethods(methods) => methods.contains(name),
        }
    }

    /// How many names this entry actually stores. A borrowed language set
    /// stores none; only Go's per-file method set owns names. This is what
    /// keeps the index O(files) instead of O(files x names).
    fn materialized_name_count(&self) -> usize {
        match self {
            DispatchNamesForFile::Language(_) => 0,
            DispatchNamesForFile::GoMethods(methods) => methods.len(),
        }
    }
}

/// Indexes each contributing file to the dispatch names that can make it a
/// dispatch root.
///
/// The previous shape copied a language's whole dispatched-name set into every
/// file of that language, so its cost was files x names — 62.7 million entries
/// at reporter scale. This index stores one entry per contributing file and
/// answers non-Go membership from the language's set on demand.
fn dispatch_live_source_names_by_file<'a>(
    contributions: &'a [DeadCodeContribution],
    dispatched_method_names: &'a MethodNamesByLanguage,
) -> BTreeMap<&'a str, DispatchNamesForFile<'a>> {
    let mut by_file: BTreeMap<&'a str, DispatchNamesForFile<'a>> = BTreeMap::new();
    for contribution in contributions {
        let language = language_for_file(&contribution.file);
        let Some(language_method_names) = dispatched_method_names.get(language) else {
            continue;
        };
        if language != "go" {
            by_file.insert(
                contribution.file.as_str(),
                DispatchNamesForFile::Language(language_method_names),
            );
            continue;
        }

        let entry = match by_file.entry(contribution.file.as_str()) {
            BTreeMapEntry::Occupied(entry) => entry.into_mut(),
            BTreeMapEntry::Vacant(entry) => {
                entry.insert(DispatchNamesForFile::GoMethods(BTreeSet::new()))
            }
        };
        let DispatchNamesForFile::GoMethods(methods) = entry else {
            // A file's language is a function of its path, so a Go file cannot
            // already be indexed as a non-Go one.
            continue;
        };
        for export in &contribution.exports {
            if export_is_method(export)
                && language_method_names.contains(symbol_liveness_name(&export.symbol))
            {
                methods.insert(symbol_liveness_name(&export.symbol).to_string());
            }
        }
    }
    by_file
}

fn dispatch_liveness_keeps_export_live(
    contribution: &DeadCodeContribution,
    export: &ExportContribution,
    dispatched_method_names: &MethodNamesByLanguage,
) -> bool {
    let language = language_for_file(&contribution.file);
    let Some(method_names) = dispatched_method_names.get(language) else {
        return false;
    };
    let name_is_dispatched = method_names.contains(symbol_liveness_name(&export.symbol));
    if language == "go" {
        export_is_method(export) && name_is_dispatched
    } else {
        name_is_dispatched
    }
}

fn export_is_method(export: &ExportContribution) -> bool {
    export.kind == "method"
}

fn imported_exports_by_file(
    contributions: &[DeadCodeContribution],
) -> BTreeMap<String, BTreeSet<ExportNode>> {
    let mut by_file: BTreeMap<String, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        if contribution.imported_exports.is_empty() {
            continue;
        }
        by_file
            .entry(contribution.file.clone())
            .or_default()
            .extend(
                contribution
                    .imported_exports
                    .iter()
                    .map(|root| (root.file.clone(), root.symbol.clone())),
            );
    }

    by_file
}

fn namespace_imported_exports_by_file(
    contributions: &[DeadCodeContribution],
) -> BTreeMap<String, BTreeSet<ExportNode>> {
    let mut by_file: BTreeMap<String, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        if contribution.namespace_imported_exports.is_empty() {
            continue;
        }
        by_file
            .entry(contribution.file.clone())
            .or_default()
            .extend(
                contribution
                    .namespace_imported_exports
                    .iter()
                    .map(|root| (root.file.clone(), root.symbol.clone())),
            );
    }

    by_file
}

fn project_internal_call(
    project_root: &Path,
    call: &CallgraphOutboundCall,
    caller_file: &str,
    test_origin: bool,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<InternalCall> {
    let target = parse_target(project_root, &call.target);
    let symbol = target.symbol?;
    let file = match target.file {
        // Qualified target (file::symbol). The snapshot builder already
        // resolved and validated this edge — cross-file targets are confirmed
        // exports of the target file, and same-file targets are confirmed
        // definitions (private functions included, e.g. `main.rs::dispatch`).
        // Keep the edge regardless of the target's export visibility: liveness
        // must flow THROUGH private intermediaries, otherwise a public handler
        // reached only via a private router/helper looks unreachable.
        Some(file) => file,
        None => resolve_unqualified_target(
            caller_file,
            &symbol,
            exported_symbols_by_file,
            files_by_exported_symbol,
        )?,
    };

    Some(InternalCall {
        caller_symbol: call.caller_symbol.clone(),
        file,
        symbol,
        line: call.line,
        provenance: call.provenance.clone(),
        test_origin: Some(test_origin),
    })
}

fn resolve_macro_token_liveness_edges(
    _project_root: &Path,
    caller_file: &str,
    refs: &[MacroTokenRefContribution],
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<InternalCall> {
    let mut calls = Vec::new();
    for reference in refs {
        let Some((file, symbol)) = resolve_macro_token_ref_target(
            caller_file,
            reference,
            rust_imports,
            exported_symbols_by_file,
        ) else {
            continue;
        };
        calls.push(InternalCall {
            caller_symbol: reference.caller_symbol.clone(),
            file,
            symbol,
            line: reference.line,
            provenance: MACRO_TOKEN_LIVENESS_PROVENANCE.to_string(),
            test_origin: None,
        });
    }
    sort_dedup_internal_calls(&mut calls);
    calls
}

fn resolve_macro_token_ref_target(
    caller_file: &str,
    reference: &MacroTokenRefContribution,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Option<ExportNode> {
    let path = reference.path.as_deref().unwrap_or(&[]);
    match reference.shape.as_str() {
        RUST_MACRO_REF_SHAPE_CALL => resolve_macro_call_or_struct_ref(
            caller_file,
            path,
            &reference.name,
            rust_imports,
            exported_symbols_by_file,
        ),
        RUST_MACRO_REF_SHAPE_STRUCT => resolve_macro_call_or_struct_ref(
            caller_file,
            path,
            &reference.name,
            rust_imports,
            exported_symbols_by_file,
        ),
        RUST_MACRO_REF_SHAPE_METHOD => resolve_macro_method_ref(
            caller_file,
            path,
            &reference.name,
            rust_imports,
            exported_symbols_by_file,
        ),
        _ => None,
    }
}

fn resolve_macro_call_or_struct_ref(
    caller_file: &str,
    path: &[String],
    name: &str,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Option<ExportNode> {
    if path.is_empty() {
        if let Some(target) = exported_symbol_target(caller_file, name, exported_symbols_by_file) {
            return Some(target);
        }
        return unique_macro_target(imported_macro_targets_for_local(
            caller_file,
            name,
            rust_imports,
            exported_symbols_by_file,
        ));
    }

    let scoped_symbol = macro_scoped_symbol(path, name);
    if let Some(target) =
        exported_symbol_target(caller_file, &scoped_symbol, exported_symbols_by_file)
    {
        return Some(target);
    }

    unique_macro_target(resolve_macro_module_targets(
        caller_file,
        path,
        name,
        rust_imports,
        exported_symbols_by_file,
    ))
}

fn resolve_macro_method_ref(
    caller_file: &str,
    path: &[String],
    name: &str,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Option<ExportNode> {
    let (type_name, module_path) = path.split_last()?;
    let scoped_symbol = macro_scoped_symbol(path, name);
    if let Some(target) =
        exported_symbol_target(caller_file, &scoped_symbol, exported_symbols_by_file)
    {
        return Some(target);
    }

    let target_symbol = format!("{type_name}::{name}");
    let mut targets = BTreeSet::new();
    if module_path.is_empty() {
        for (file, imported_type) in imported_macro_targets_for_local(
            caller_file,
            type_name,
            rust_imports,
            exported_symbols_by_file,
        ) {
            let imported_method = format!("{imported_type}::{name}");
            if let Some(target) =
                exported_symbol_target(&file, &imported_method, exported_symbols_by_file)
            {
                targets.insert(target);
            }
        }
    } else {
        targets.extend(resolve_macro_module_targets(
            caller_file,
            module_path,
            &target_symbol,
            rust_imports,
            exported_symbols_by_file,
        ));
    }
    unique_macro_target(targets)
}

fn resolve_macro_module_targets(
    caller_file: &str,
    module_path: &[String],
    target_symbol: &str,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<ExportNode> {
    let mut targets = BTreeSet::new();
    for candidate in rust_macro_module_path_candidates(module_path, rust_imports) {
        let segment_refs = candidate.iter().map(String::as_str).collect::<Vec<_>>();
        let Some(resolved_segments) = rust_resolve_segments_for_macro(caller_file, &segment_refs)
        else {
            continue;
        };
        let Some(file) = rust_file_for_segments_from_contributions(
            caller_file,
            &resolved_segments,
            exported_symbols_by_file,
        ) else {
            continue;
        };
        if let Some(target) = exported_symbol_target(&file, target_symbol, exported_symbols_by_file)
        {
            targets.insert(target);
        }
    }
    targets
}

fn imported_macro_targets_for_local(
    caller_file: &str,
    local_name: &str,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeSet<ExportNode> {
    let mut targets = BTreeSet::new();
    for import in rust_imports {
        for imported in rust_imported_symbol_specs(import) {
            if imported.local_name != local_name {
                continue;
            }
            let segment_refs = imported
                .module_segments
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>();
            let Some(resolved_segments) =
                rust_resolve_segments_for_macro(caller_file, &segment_refs)
            else {
                continue;
            };
            let Some(file) = rust_file_for_segments_from_contributions(
                caller_file,
                &resolved_segments,
                exported_symbols_by_file,
            ) else {
                continue;
            };
            if let Some(target) =
                exported_symbol_target(&file, &imported.imported_name, exported_symbols_by_file)
            {
                targets.insert(target);
            }
        }
    }
    targets
}

fn rust_macro_module_path_candidates(
    path: &[String],
    rust_imports: &[RawImportContribution],
) -> Vec<Vec<String>> {
    let mut candidates = Vec::new();
    if let Some(first) = path.first() {
        for import in rust_imports {
            let Some((local_name, mut import_segments)) = rust_import_module_alias_segments(import)
            else {
                continue;
            };
            if &local_name == first {
                import_segments.extend(path[1..].iter().cloned());
                push_unique_macro_path_candidate(&mut candidates, import_segments);
            }
        }
    }
    push_unique_macro_path_candidate(&mut candidates, path.to_vec());
    candidates
}

fn rust_import_module_alias_segments(
    import: &RawImportContribution,
) -> Option<(String, Vec<String>)> {
    let path = import.source.trim().trim_end_matches(';').trim();
    if path.contains("::{") || path.contains('{') || path.contains('*') {
        return None;
    }
    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments = rust_path_segments(path_without_alias);
    let local_name = alias.or_else(|| segments.last().map(String::as_str))?;
    if rust_macro_name_is_upper_camel(local_name) {
        return None;
    }
    Some((local_name.to_string(), segments))
}

fn rust_imported_symbol_specs(import: &RawImportContribution) -> Vec<RustImportedSymbolSpec> {
    let path = import.source.trim().trim_end_matches(';').trim();
    if let Some((prefix, rest)) = path.split_once("::{") {
        let list = rest.trim_end_matches('}');
        return list
            .split(',')
            .filter_map(|specifier| rust_imported_symbol_spec(prefix, specifier))
            .collect();
    }

    rust_imported_symbol_spec("", path).into_iter().collect()
}

fn rust_imported_symbol_spec(prefix: &str, specifier: &str) -> Option<RustImportedSymbolSpec> {
    let specifier = specifier.trim();
    if specifier.is_empty() || specifier == "*" || specifier.contains('{') {
        return None;
    }
    let (path_without_alias, alias) = specifier
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((specifier, None));
    let mut segments = rust_path_segments(path_without_alias);
    let imported_name = segments.pop()?;
    let local_name = alias.unwrap_or(imported_name.as_str()).trim();
    if local_name.is_empty() || local_name == "_" {
        return None;
    }

    let mut module_segments = rust_path_segments(prefix);
    module_segments.extend(segments);
    Some(RustImportedSymbolSpec {
        local_name: local_name.to_string(),
        module_segments,
        imported_name,
    })
}

fn rust_path_segments(path: &str) -> Vec<String> {
    path.split("::")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn push_unique_macro_path_candidate(candidates: &mut Vec<Vec<String>>, candidate: Vec<String>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn rust_resolve_segments_for_macro(caller_file: &str, segments: &[&str]) -> Option<Vec<String>> {
    if segments.is_empty() {
        return Some(Vec::new());
    }
    let caller_segments = rust_module_segments_for_rel(caller_file);
    match segments[0] {
        "crate" => Some(
            segments[1..]
                .iter()
                .map(|item| (*item).to_string())
                .collect(),
        ),
        "self" => {
            let mut resolved = caller_segments;
            resolved.extend(segments[1..].iter().map(|item| (*item).to_string()));
            Some(resolved)
        }
        "super" => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments[1..].iter().map(|item| (*item).to_string()));
            Some(resolved)
        }
        _ => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(segments.iter().map(|item| (*item).to_string()));
            Some(resolved)
        }
    }
}

fn rust_file_for_segments_from_contributions(
    caller_file: &str,
    segments: &[String],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    let src_prefix = rust_src_prefix_for_rel(caller_file);
    if segments.is_empty() {
        let lib = format!("{src_prefix}/lib.rs");
        if exported_symbols_by_file.contains_key(&lib) {
            return Some(lib);
        }
        let main = format!("{src_prefix}/main.rs");
        if exported_symbols_by_file.contains_key(&main) {
            return Some(main);
        }
    }

    let candidate = if segments.is_empty() {
        format!("{src_prefix}/lib.rs")
    } else {
        format!("{}/{}.rs", src_prefix, segments.join("/"))
    };
    if exported_symbols_by_file.contains_key(&candidate) {
        return Some(candidate);
    }
    if !segments.is_empty() {
        let mod_candidate = format!("{}/{}/mod.rs", src_prefix, segments.join("/"));
        if exported_symbols_by_file.contains_key(&mod_candidate) {
            return Some(mod_candidate);
        }
    }
    None
}

fn rust_src_prefix_for_rel(rel_path: &str) -> String {
    rel_path
        .split_once("/src/")
        .map(|(prefix, _)| format!("{prefix}/src"))
        .unwrap_or_else(|| "src".to_string())
}

fn rust_module_segments_for_rel(rel_path: &str) -> Vec<String> {
    let after_src = rel_path
        .split_once("/src/")
        .map(|(_, rest)| rest)
        .or_else(|| rel_path.strip_prefix("src/"))
        .unwrap_or(rel_path);
    if matches!(after_src, "lib.rs" | "main.rs") {
        return Vec::new();
    }
    if let Some(prefix) = after_src.strip_suffix("/mod.rs") {
        return prefix.split('/').map(|item| item.to_string()).collect();
    }
    after_src
        .strip_suffix(".rs")
        .unwrap_or(after_src)
        .split('/')
        .map(|item| item.to_string())
        .collect()
}

fn macro_scoped_symbol(path: &[String], name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{}::{name}", path.join("::"))
    }
}

fn exported_symbol_target(
    file: &str,
    symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> Option<ExportNode> {
    exported_symbols_by_file
        .get(file)
        .is_some_and(|symbols| symbols.contains(symbol))
        .then(|| (file.to_string(), symbol.to_string()))
}

fn unique_macro_target(targets: BTreeSet<ExportNode>) -> Option<ExportNode> {
    if targets.len() == 1 {
        targets.into_iter().next()
    } else {
        None
    }
}

fn raw_imports_from_tree(
    source: &str,
    tree: &tree_sitter::Tree,
    lang: LangId,
) -> Vec<RawImportContribution> {
    parse_imports(source, tree, lang)
        .imports
        .into_iter()
        .map(|import| RawImportContribution {
            source: import.module_path,
            names: import.names,
            default_import: import.default_import,
            namespace_import: import.namespace_import,
        })
        .collect()
}

fn rust_raw_import_contributions(
    source: &str,
    tree: &tree_sitter::Tree,
) -> Vec<RawImportContribution> {
    parse_imports(source, tree, LangId::Rust)
        .imports
        .into_iter()
        .map(|import| RawImportContribution {
            source: import.module_path,
            names: import.names,
            default_import: None,
            namespace_import: None,
        })
        .collect()
}

fn rust_cfg_test_ranges(source: &str, root: tree_sitter::Node) -> Vec<RustCfgTestRange> {
    let mut ranges = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "mod_item" | "function_item" | "impl_item")
            && rust_node_has_cfg_test_attribute(source, node)
        {
            ranges.push(RustCfgTestRange {
                start_line: node.start_position().row as u32 + 1,
                end_line: node.end_position().row as u32 + 1,
            });
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
    ranges.sort_by_key(|range| (range.start_line, range.end_line));
    ranges.dedup();
    ranges
}

fn rust_node_has_cfg_test_attribute(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(attribute) = previous {
        match attribute.kind() {
            "attribute_item" => {
                let compact = source[attribute.byte_range()]
                    .chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>();
                if compact
                    .strip_prefix("#[cfg(")
                    .and_then(|inner| inner.strip_suffix(")]"))
                    .is_some_and(cfg_predicate_requires_test)
                {
                    return true;
                }
                previous = attribute.prev_sibling();
            }
            "line_comment" | "block_comment" => previous = attribute.prev_sibling(),
            _ => break,
        }
    }
    false
}

fn cfg_predicate_requires_test(predicate: &str) -> bool {
    if predicate == "test" {
        return true;
    }
    if let Some(inner) = predicate
        .strip_prefix("all(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        return split_cfg_predicates(inner)
            .into_iter()
            .any(cfg_predicate_requires_test);
    }
    if let Some(inner) = predicate
        .strip_prefix("any(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let predicates = split_cfg_predicates(inner);
        return !predicates.is_empty() && predicates.into_iter().all(cfg_predicate_requires_test);
    }
    false
}

fn split_cfg_predicates(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(input[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let tail = input[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn rust_macro_token_refs(source: &str, root: tree_sitter::Node) -> Vec<MacroTokenRefContribution> {
    let mut refs = BTreeSet::new();
    let mut scope_stack = Vec::new();
    collect_rust_macro_token_refs(source, root, &mut scope_stack, &mut refs);
    refs.into_iter().collect()
}

fn collect_rust_macro_token_refs(
    source: &str,
    node: tree_sitter::Node,
    scope_stack: &mut Vec<String>,
    refs: &mut BTreeSet<MacroTokenRefContribution>,
) {
    let scope_len = scope_stack.len();
    if node.kind() == "function_item" {
        if let Some(symbol) = rust_function_symbol_name(source, &node) {
            scope_stack.push(symbol);
        }
    }

    if node.kind() == "macro_invocation" {
        if let Some(token_tree) = find_child_by_kind(node, "token_tree") {
            let caller_symbol = scope_stack
                .last()
                .cloned()
                .unwrap_or_else(|| TOP_LEVEL_SYMBOL.to_string());
            let mut tokens = Vec::new();
            collect_rust_macro_tokens(source, token_tree, &mut tokens);
            extract_rust_macro_token_refs(&tokens, &caller_symbol, refs);
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_rust_macro_token_refs(source, cursor.node(), scope_stack, refs);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    scope_stack.truncate(scope_len);
}

fn collect_rust_macro_tokens<'a>(
    source: &'a str,
    node: tree_sitter::Node,
    tokens: &mut Vec<RustMacroToken<'a>>,
) {
    if rust_macro_token_node_is_opaque(node.kind()) {
        return;
    }

    if node.child_count() == 0 {
        let text = node_text(source, node).trim();
        if !text.is_empty() {
            tokens.push(RustMacroToken {
                text,
                kind: node.kind(),
                line: node.start_position().row as u32 + 1,
            });
        }
        return;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_rust_macro_tokens(source, cursor.node(), tokens);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn rust_macro_token_node_is_opaque(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal" | "raw_string_literal" | "char_literal" | "line_comment" | "block_comment"
    )
}

fn extract_rust_macro_token_refs(
    tokens: &[RustMacroToken<'_>],
    caller_symbol: &str,
    refs: &mut BTreeSet<MacroTokenRefContribution>,
) {
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if !rust_macro_token_is_identifier(token) || rust_macro_token_is_keyword(token.text) {
            continue;
        }
        if index > 0 && tokens[index - 1].text == "." {
            continue;
        }
        if tokens.get(index + 1).is_some_and(|next| next.text == "!") {
            continue;
        }

        let path = rust_macro_path_before(tokens, index);
        let next = rust_macro_next_after_optional_turbofish(tokens, index + 1);
        if tokens.get(next).is_some_and(|next| next.text == "(") {
            let shape = if path
                .last()
                .is_some_and(|segment| rust_macro_name_is_upper_camel(segment))
            {
                RUST_MACRO_REF_SHAPE_METHOD
            } else {
                RUST_MACRO_REF_SHAPE_CALL
            };
            refs.insert(MacroTokenRefContribution {
                caller_symbol: caller_symbol.to_string(),
                line: token.line,
                name: token.text.to_string(),
                path: macro_ref_path(path),
                shape: shape.to_string(),
            });
            continue;
        }

        if rust_macro_name_is_upper_camel(token.text)
            && tokens.get(index + 1).is_some_and(|next| next.text == "{")
        {
            refs.insert(MacroTokenRefContribution {
                caller_symbol: caller_symbol.to_string(),
                line: token.line,
                name: token.text.to_string(),
                path: macro_ref_path(path),
                shape: RUST_MACRO_REF_SHAPE_STRUCT.to_string(),
            });
        }
    }
}

fn rust_macro_path_before(tokens: &[RustMacroToken<'_>], index: usize) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cursor = index;
    while cursor >= 2
        && tokens[cursor - 1].text == "::"
        && rust_macro_token_is_path_segment(&tokens[cursor - 2])
    {
        segments.push(tokens[cursor - 2].text.to_string());
        cursor -= 2;
    }
    segments.reverse();
    segments
}

fn rust_macro_next_after_optional_turbofish(tokens: &[RustMacroToken<'_>], index: usize) -> usize {
    if tokens.get(index).is_none_or(|token| token.text != "::")
        || tokens.get(index + 1).is_none_or(|token| token.text != "<")
    {
        return index;
    }

    let mut depth = 0usize;
    let mut cursor = index + 1;
    while let Some(token) = tokens.get(cursor) {
        match token.text {
            "<" => depth += 1,
            ">" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return cursor + 1;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    index
}

fn macro_ref_path(path: Vec<String>) -> Option<Vec<String>> {
    (!path.is_empty()).then_some(path)
}

fn rust_macro_token_is_identifier(token: &RustMacroToken<'_>) -> bool {
    matches!(token.kind, "identifier" | "type_identifier")
        || rust_macro_text_is_identifier(token.text)
}

fn rust_macro_token_is_path_segment(token: &RustMacroToken<'_>) -> bool {
    rust_macro_token_is_identifier(token)
        && (!rust_macro_token_is_keyword(token.text)
            || matches!(token.text, "crate" | "self" | "super"))
}

fn rust_macro_text_is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn rust_macro_name_is_upper_camel(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn rust_macro_token_is_keyword(text: &str) -> bool {
    matches!(
        text,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

fn rust_function_symbol_name(
    source: &str,
    function_node: &tree_sitter::Node<'_>,
) -> Option<String> {
    let name_node = function_node.child_by_field_name("name")?;
    let name = node_text(source, name_node).to_string();
    let declaration_list_owner = rust_function_declaration_list_owner(function_node);

    match declaration_list_owner.as_ref().map(tree_sitter::Node::kind) {
        Some("impl_item") => {
            let scope_name = rust_impl_scope_name(declaration_list_owner.as_ref().unwrap(), source);
            if scope_name.is_empty() {
                Some(name)
            } else {
                Some(format!("{scope_name}::{name}"))
            }
        }
        Some(owner_kind) if owner_kind != "mod_item" => None,
        _ => {
            let scope_chain = rust_mod_scope_chain(function_node, source);
            if scope_chain.is_empty() {
                Some(name)
            } else {
                Some(format!("{}::{name}", scope_chain.join("::")))
            }
        }
    }
}

fn rust_function_declaration_list_owner<'a>(
    function_node: &tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    function_node
        .parent()
        .filter(|parent| parent.kind() == "declaration_list")
        .and_then(|parent| parent.parent())
}

fn rust_mod_scope_chain(node: &tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                scopes.push(node_text(source, name_node).to_string());
            }
        }
        current = parent.parent();
    }
    scopes.reverse();
    scopes
}

fn rust_impl_scope_name(impl_node: &tree_sitter::Node<'_>, source: &str) -> String {
    let mut type_names: Vec<String> = Vec::new();
    let mut child_cursor = impl_node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "type_identifier" || child.kind() == "generic_type" {
                type_names.push(node_text(source, child).to_string());
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if type_names.len() >= 2 {
        format!("{} for {}", type_names[0], type_names[1])
    } else if type_names.len() == 1 {
        type_names[0].clone()
    } else {
        String::new()
    }
}

fn ts_raw_reexport_contributions(
    source: &str,
    root: tree_sitter::Node,
) -> Vec<RawReexportContribution> {
    let mut reexports = Vec::new();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return reexports;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            if let Some(module_path) = export_source_module(source, node) {
                let line = (node.start_position().row + 1) as u32;
                let raw_export = node_text(source, node).trim();
                for specifier in ts_reexport_specifiers(raw_export) {
                    reexports.push(RawReexportContribution {
                        language: "ts".to_string(),
                        source: module_path.clone(),
                        kind: "named".to_string(),
                        imported: Some(specifier.imported),
                        exported: Some(specifier.exported),
                        line,
                    });
                }
                if raw_export.contains('*') {
                    if let Some(namespace_export) = ts_namespace_reexport_name(raw_export) {
                        reexports.push(RawReexportContribution {
                            language: "ts".to_string(),
                            source: module_path.clone(),
                            kind: "namespace".to_string(),
                            imported: Some("*".to_string()),
                            exported: Some(namespace_export),
                            line,
                        });
                    } else {
                        reexports.push(RawReexportContribution {
                            language: "ts".to_string(),
                            source: module_path.clone(),
                            kind: "star".to_string(),
                            imported: Some("*".to_string()),
                            exported: None,
                            line,
                        });
                    }
                }
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    reexports
}

fn rust_raw_reexport_contributions(source: &str) -> Vec<RawReexportContribution> {
    rust_pub_use_statements(source)
        .into_iter()
        .flat_map(|(statement, line)| {
            rust_reexport_specifiers(&statement)
                .into_iter()
                .map(move |specifier| RawReexportContribution {
                    language: "rust".to_string(),
                    source: specifier.module_path.join("::"),
                    kind: if specifier.imported == "*" {
                        "star".to_string()
                    } else {
                        "named".to_string()
                    },
                    imported: Some(specifier.imported),
                    exported: Some(specifier.exported),
                    line,
                })
        })
        .collect()
}

fn resolve_raw_reexport_liveness_edges(
    project_root: &Path,
    file_name: &str,
    raw_reexports: &[RawReexportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Vec<InternalCall> {
    let mut edges = Vec::new();
    let file = project_root.join(file_name);
    let from_dir = file.parent().unwrap_or_else(|| Path::new("."));

    for raw in raw_reexports {
        match raw.language.as_str() {
            "ts" => {
                let Some(module_entry) = resolve_import_module_path(from_dir, &raw.source) else {
                    continue;
                };
                edges.extend(resolve_reexport_fact_edge(
                    project_root,
                    file_name,
                    &module_entry,
                    raw.kind.as_str(),
                    raw.imported.as_deref(),
                    raw.exported.as_deref(),
                    raw.line,
                    exported_symbols_by_file,
                    default_export_symbols_by_file,
                ));
            }
            "rust" => {
                let module_path = raw
                    .source
                    .split("::")
                    .filter(|segment| !segment.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let Some(module_entry) =
                    rust_module_entry_from_file(project_root, file_name, &module_path)
                else {
                    continue;
                };
                edges.extend(resolve_reexport_fact_edge(
                    project_root,
                    file_name,
                    &module_entry,
                    raw.kind.as_str(),
                    raw.imported.as_deref(),
                    raw.exported.as_deref(),
                    raw.line,
                    exported_symbols_by_file,
                    default_export_symbols_by_file,
                ));
            }
            _ => {}
        }
    }

    edges
}

fn resolve_oxc_reexport_liveness_edges(
    project_root: &Path,
    file_name: &str,
    oxc_facts: &OxcFactsContribution,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Vec<InternalCall> {
    let file = project_root.join(file_name);
    let from_dir = file.parent().unwrap_or_else(|| Path::new("."));
    let mut edges = Vec::new();
    for fact in &oxc_facts.re_exports {
        let Some(module_entry) = resolve_import_module_path(from_dir, &fact.source) else {
            continue;
        };
        let kind = match fact.kind {
            ReExportKind::Named => "named",
            ReExportKind::Star => "star",
            ReExportKind::Namespace => "namespace",
        };
        edges.extend(resolve_reexport_fact_edge(
            project_root,
            file_name,
            &module_entry,
            kind,
            fact.imported_name.as_deref(),
            fact.exported_name.as_deref(),
            fact.line,
            exported_symbols_by_file,
            default_export_symbols_by_file,
        ));
    }
    edges
}

#[allow(clippy::too_many_arguments)]
fn resolve_reexport_fact_edge(
    project_root: &Path,
    file_name: &str,
    module_entry: &Path,
    kind: &str,
    imported: Option<&str>,
    exported: Option<&str>,
    line: u32,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Vec<InternalCall> {
    match kind {
        "star" => reexport_edges_for_all_target_symbols(
            project_root,
            file_name,
            "",
            module_entry,
            line,
            exported_symbols_by_file,
            default_export_symbols_by_file,
            true,
        ),
        "namespace" => {
            let namespace_export = exported.unwrap_or_default();
            if namespace_export.is_empty()
                || !file_exports_symbol(file_name, namespace_export, exported_symbols_by_file)
            {
                return Vec::new();
            }
            reexport_edges_for_all_target_symbols(
                project_root,
                file_name,
                namespace_export,
                module_entry,
                line,
                exported_symbols_by_file,
                default_export_symbols_by_file,
                false,
            )
        }
        _ => {
            let imported = imported.unwrap_or_default();
            let exported = exported.unwrap_or(imported);
            if imported.is_empty()
                || exported.is_empty()
                || !file_exports_symbol(file_name, exported, exported_symbols_by_file)
            {
                return Vec::new();
            }
            resolve_imported_export_liveness_root(
                project_root,
                module_entry,
                imported,
                exported_symbols_by_file,
                default_export_symbols_by_file,
            )
            .map(|(target_file, target_symbol)| {
                vec![InternalCall {
                    caller_symbol: exported.to_string(),
                    file: target_file,
                    symbol: target_symbol,
                    line,
                    provenance: CALLGRAPH_PROVENANCE_REEXPORT.to_string(),
                    test_origin: None,
                }]
            })
            .unwrap_or_default()
        }
    }
}

fn rust_module_entry_from_file(
    project_root: &Path,
    file_name: &str,
    module_path: &[String],
) -> Option<PathBuf> {
    let first = module_path.first()?;
    let file = project_root.join(file_name);
    let base_dir = file.parent().unwrap_or_else(|| Path::new("."));
    resolve_rust_module_file(base_dir, first)
}

fn resolve_raw_imported_export_liveness_roots(
    project_root: &Path,
    file_name: &str,
    raw_imports: &[RawImportContribution],
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> ImportedExportLiveness {
    let file = project_root.join(file_name);
    let from_dir = file.parent().unwrap_or_else(|| Path::new("."));
    let mut root_exports: BTreeSet<ExportNode> = BTreeSet::new();
    let mut namespace_exports: BTreeSet<ExportNode> = BTreeSet::new();

    for import in raw_imports {
        if import.namespace_import.is_some() {
            if let Some(module_entry) = resolve_import_module_path(from_dir, &import.source) {
                namespace_exports.extend(resolve_namespace_import_liveness_roots(
                    project_root,
                    &module_entry,
                    exported_symbols_by_file,
                    default_export_symbols_by_file,
                ));
            }
        }

        let Some(module_entry) = resolve_import_module_path(from_dir, &import.source) else {
            continue;
        };

        for imported_name in import
            .names
            .iter()
            .map(|name| specifier_imported_name(name))
        {
            if let Some(root) = resolve_imported_export_liveness_root(
                project_root,
                &module_entry,
                imported_name,
                exported_symbols_by_file,
                default_export_symbols_by_file,
            ) {
                root_exports.insert(root);
            }
        }

        if import.default_import.is_some() {
            if let Some(root) = resolve_imported_export_liveness_root(
                project_root,
                &module_entry,
                "default",
                exported_symbols_by_file,
                default_export_symbols_by_file,
            ) {
                root_exports.insert(root);
            }
        }
    }

    ImportedExportLiveness {
        root_exports: root_exports
            .into_iter()
            .map(|(file, symbol)| ImportedExportContribution { file, symbol })
            .collect(),
        namespace_exports: namespace_exports
            .into_iter()
            .map(|(file, symbol)| ImportedExportContribution { file, symbol })
            .collect(),
    }
}

fn ts_reexport_specifiers(raw_export: &str) -> Vec<ReexportSpecifier> {
    let Some(start) = raw_export.find('{').map(|index| index + 1) else {
        return Vec::new();
    };
    let Some(end) = raw_export[start..].find('}').map(|index| start + index) else {
        return Vec::new();
    };

    raw_export[start..end]
        .split(',')
        .filter_map(|specifier| {
            let specifier = specifier.trim();
            if specifier.is_empty() {
                return None;
            }
            let imported = specifier_imported_name(specifier).trim();
            let exported = specifier_local_name(specifier).trim();
            if imported.is_empty() || exported.is_empty() {
                return None;
            }
            Some(ReexportSpecifier {
                imported: imported.to_string(),
                exported: exported.to_string(),
            })
        })
        .collect()
}

fn ts_namespace_reexport_name(raw_export: &str) -> Option<String> {
    let after_star = raw_export.split_once('*')?.1.trim_start();
    let after_as = after_star.strip_prefix("as")?.trim_start();
    let name = after_as
        .split_whitespace()
        .next()?
        .trim_matches(|ch: char| ch == '{' || ch == '}' || ch == ';' || ch == ',');
    (!name.is_empty()).then(|| name.to_string())
}

fn reexport_edges_for_all_target_symbols(
    project_root: &Path,
    file_name: &str,
    namespace_export: &str,
    module_entry: &Path,
    line: u32,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
    match_current_export_names: bool,
) -> Vec<InternalCall> {
    let Some((_, target_symbols)) =
        exported_symbols_for_resolved_file(project_root, module_entry, exported_symbols_by_file)
    else {
        return Vec::new();
    };

    let mut edges = Vec::new();
    for target_symbol in target_symbols {
        let caller_symbol = if match_current_export_names {
            if !file_exports_symbol(file_name, target_symbol, exported_symbols_by_file) {
                continue;
            }
            target_symbol.clone()
        } else {
            namespace_export.to_string()
        };

        if let Some((target_file, resolved_symbol)) = resolve_imported_export_liveness_root(
            project_root,
            module_entry,
            target_symbol,
            exported_symbols_by_file,
            default_export_symbols_by_file,
        ) {
            edges.push(InternalCall {
                caller_symbol,
                file: target_file,
                symbol: resolved_symbol,
                line,
                provenance: CALLGRAPH_PROVENANCE_REEXPORT.to_string(),
                test_origin: None,
            });
        }
    }

    edges
}

fn resolve_rust_module_file(base_dir: &Path, module: &str) -> Option<PathBuf> {
    let flat = base_dir.join(format!("{module}.rs"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = base_dir.join(module).join("mod.rs");
    nested.is_file().then_some(nested)
}

fn rust_pub_use_statements(source: &str) -> Vec<(String, u32)> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut start_line = 0u32;

    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if current.is_empty() {
            if !(trimmed.starts_with("pub use ") || trimmed.starts_with("pub(crate) use ")) {
                continue;
            }
            start_line = (index + 1) as u32;
        }

        current.push(' ');
        current.push_str(trimmed);
        if trimmed.ends_with(';') {
            statements.push((current.trim().to_string(), start_line));
            current.clear();
        }
    }

    statements
}

fn rust_reexport_specifiers(statement: &str) -> Vec<RustReexportSpecifier> {
    let statement = statement
        .trim()
        .trim_end_matches(';')
        .strip_prefix("pub(crate) use ")
        .or_else(|| {
            statement
                .trim()
                .trim_end_matches(';')
                .strip_prefix("pub use ")
        })
        .unwrap_or("")
        .trim();
    if statement.is_empty() {
        return Vec::new();
    }

    if let Some((module_path, grouped)) = statement.split_once("::{") {
        let grouped = grouped.trim_end_matches('}');
        return grouped
            .split(',')
            .filter_map(|specifier| rust_reexport_specifier(module_path.trim(), specifier.trim()))
            .collect();
    }

    let Some((module_path, imported)) = statement.rsplit_once("::") else {
        return Vec::new();
    };
    rust_reexport_specifier(module_path.trim(), imported.trim())
        .into_iter()
        .collect()
}

fn rust_reexport_specifier(module_path: &str, specifier: &str) -> Option<RustReexportSpecifier> {
    if specifier.is_empty() {
        return None;
    }
    let (imported, exported) = specifier
        .split_once(" as ")
        .map(|(imported, exported)| (imported.trim(), exported.trim()))
        .unwrap_or((specifier.trim(), specifier.trim()));
    if imported.is_empty() || exported.is_empty() {
        return None;
    }
    Some(RustReexportSpecifier {
        module_path: rust_normalize_module_path(module_path),
        imported: imported.to_string(),
        exported: exported.to_string(),
    })
}

fn rust_normalize_module_path(module_path: &str) -> Vec<String> {
    module_path
        .split("::")
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() || matches!(segment, "self" | "crate") {
                None
            } else {
                Some(segment.to_string())
            }
        })
        .collect()
}

fn file_exports_symbol(
    file_name: &str,
    symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
) -> bool {
    exported_symbols_by_file
        .get(file_name)
        .is_some_and(|symbols| symbols.contains(symbol))
}

fn export_source_module(source: &str, node: tree_sitter::Node) -> Option<String> {
    node.child_by_field_name("source")
        .or_else(|| find_child_by_kind(node, "string"))
        .and_then(|source_node| string_literal_content(source, source_node))
}

fn find_child_by_kind<'tree>(
    node: tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(descendant) = find_child_by_kind(child, kind) {
            return Some(descendant);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn string_literal_content(source: &str, node: tree_sitter::Node) -> Option<String> {
    let raw = node_text(source, node).trim();
    let quote = raw.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    raw.strip_prefix(quote)
        .and_then(|value| value.strip_suffix(quote))
        .map(ToOwned::to_owned)
}

fn node_text<'a>(source: &'a str, node: tree_sitter::Node) -> &'a str {
    &source[node.byte_range()]
}

fn resolve_import_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    if is_relative_module_path(module_path) {
        return resolve_js_ts_module_path(from_dir, module_path);
    }
    resolve_workspace_package_import(from_dir, module_path)
}

fn resolve_js_ts_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    resolve_module_path(from_dir, module_path)
        .or_else(|| resolve_esm_source_module_path(from_dir, module_path))
}

fn resolve_esm_source_module_path(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    if !is_relative_module_path(module_path) {
        return None;
    }
    let base = from_dir.join(module_path);
    let ext = base.extension().and_then(|extension| extension.to_str())?;
    let candidates: &[&str] = match ext {
        "js" => &["ts", "tsx"],
        "jsx" => &["tsx", "ts"],
        "mjs" => &["mts", "ts"],
        "cjs" => &["cts", "ts"],
        _ => return None,
    };

    candidates
        .iter()
        .map(|extension| base.with_extension(extension))
        .find(|candidate| candidate.is_file())
}

fn is_relative_module_path(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

#[derive(Debug)]
struct ReexportSpecifier {
    imported: String,
    exported: String,
}

#[derive(Debug)]
struct RustReexportSpecifier {
    module_path: Vec<String>,
    imported: String,
    exported: String,
}

fn resolve_workspace_package_import(from_dir: &Path, module_path: &str) -> Option<PathBuf> {
    let package_name = package_name_from_import(module_path)?;
    let module_entry = resolve_module_path(from_dir, module_path)?;
    let resolved_package_name = package_name_for_file(&module_entry)?;
    (resolved_package_name == package_name).then_some(module_entry)
}

fn package_name_from_import(module_path: &str) -> Option<String> {
    if module_path.starts_with('.') || module_path.starts_with('/') || module_path.starts_with('#')
    {
        return None;
    }

    let mut parts = module_path.split('/');
    let first = parts.next()?;
    if first.is_empty() {
        return None;
    }

    if first.starts_with('@') {
        let second = parts.next()?;
        (!second.is_empty()).then(|| format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

fn package_name_for_file(file: &Path) -> Option<String> {
    let mut current = file.parent();
    while let Some(dir) = current {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            if let Ok(source) = fs::read_to_string(&manifest) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&source) {
                    if let Some(name) = value.get("name").and_then(serde_json::Value::as_str) {
                        return Some(name.to_string());
                    }
                }
            }
        }
        current = dir.parent();
    }
    None
}

fn resolve_namespace_import_liveness_roots(
    project_root: &Path,
    module_entry: &Path,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Vec<ExportNode> {
    let Some((_, symbols)) =
        exported_symbols_for_resolved_file(project_root, module_entry, exported_symbols_by_file)
    else {
        return Vec::new();
    };
    let mut roots = BTreeSet::new();

    for symbol in symbols {
        if let Some(root) = resolve_imported_export_liveness_root(
            project_root,
            module_entry,
            symbol,
            exported_symbols_by_file,
            default_export_symbols_by_file,
        ) {
            roots.insert(root);
        }
    }

    if default_export_symbol_for_resolved_file(
        project_root,
        module_entry,
        default_export_symbols_by_file,
    )
    .is_some()
    {
        if let Some(root) = resolve_imported_export_liveness_root(
            project_root,
            module_entry,
            "default",
            exported_symbols_by_file,
            default_export_symbols_by_file,
        ) {
            roots.insert(root);
        }
    }

    roots.into_iter().collect()
}

fn resolve_imported_export_liveness_root(
    project_root: &Path,
    module_entry: &Path,
    imported_symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Option<ExportNode> {
    let mut file_exports_symbol = |path: &Path, symbol_name: &str| {
        exported_symbols_for_resolved_file(project_root, path, exported_symbols_by_file)
            .is_some_and(|(_, symbols)| symbols.contains(symbol_name))
    };
    let mut file_default_export_symbol = |path: &Path| {
        default_export_symbol_for_resolved_file(project_root, path, default_export_symbols_by_file)
            .or_else(|| {
                exported_symbols_for_resolved_file(project_root, path, exported_symbols_by_file)
                    .and_then(|(_, symbols)| {
                        symbols.contains("default").then(|| "default".to_string())
                    })
            })
    };

    let (target_file, symbol) = resolve_reexported_symbol_target(
        module_entry,
        imported_symbol,
        &mut file_exports_symbol,
        &mut file_default_export_symbol,
    )?;

    let (file, symbols) =
        exported_symbols_for_resolved_file(project_root, &target_file, exported_symbols_by_file)?;
    symbols.contains(&symbol).then_some((file, symbol))
}

fn exported_symbols_for_resolved_file<'a>(
    project_root: &Path,
    file: &Path,
    exported_symbols_by_file: &'a BTreeMap<String, BTreeSet<String>>,
) -> Option<(String, &'a BTreeSet<String>)> {
    let relative = relative_path(project_root, file);
    if let Some(symbols) = exported_symbols_by_file.get(&relative) {
        return Some((relative, symbols));
    }

    // Normalized, not bare-canonical: the map keys being probed are built
    // from job-normalized (verbatim-stripped) paths.
    let canonical_root = canonicalize_normalized(project_root);
    let canonical_file = canonicalize_normalized(file);
    let relative = relative_path(&canonical_root, &canonical_file);
    exported_symbols_by_file
        .get(&relative)
        .map(|symbols| (relative, symbols))
}

fn default_export_symbol_for_resolved_file(
    project_root: &Path,
    file: &Path,
    default_export_symbols_by_file: &BTreeMap<String, String>,
) -> Option<String> {
    let relative = relative_path(project_root, file);
    if let Some(symbol) = default_export_symbols_by_file.get(&relative) {
        return Some(symbol.clone());
    }

    // Normalized, not bare-canonical: the map keys being probed are built
    // from job-normalized (verbatim-stripped) paths.
    let canonical_root = canonicalize_normalized(project_root);
    let canonical_file = canonicalize_normalized(file);
    let relative = relative_path(&canonical_root, &canonical_file);
    default_export_symbols_by_file.get(&relative).cloned()
}

fn resolve_unqualified_target(
    caller_file: &str,
    symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    if exported_symbols_by_file
        .get(caller_file)
        .is_some_and(|symbols| symbols.contains(symbol))
    {
        return Some(caller_file.to_string());
    }

    let files = files_by_exported_symbol.get(symbol)?;
    if files.len() == 1 {
        files.iter().next().cloned()
    } else {
        None
    }
}

fn dispatched_method_names_from_call(
    call: &CallgraphOutboundCall,
    caller_file: &str,
) -> Vec<String> {
    let mut names = BTreeSet::new();
    let is_go = language_for_file(caller_file) == "go";
    if is_go {
        if let Some(interface_methods) = go_well_known_interface_methods_from_call(call) {
            names.extend(interface_methods.iter().map(|name| (*name).to_string()));
            return names.into_iter().collect();
        }
    }

    if let Some(name) = dispatched_method_name_from_call(call) {
        names.insert(name);
    }
    names.into_iter().collect()
}

fn dispatched_method_name_from_call(call: &CallgraphOutboundCall) -> Option<String> {
    let (target, full_callee) = split_call_target_metadata(&call.target);
    if let Some(full_callee) = full_callee {
        return dispatched_method_name_from_callee(full_callee);
    }
    if target.contains("::") || target.contains('#') {
        return None;
    }
    dispatched_method_name_from_callee(target)
}

fn dispatched_method_name_from_callee(callee: &str) -> Option<String> {
    let callee = callee.trim();
    if !callee.contains('.') {
        return None;
    }

    clean_symbol(callee.rsplit('.').next()?.trim().trim_start_matches('?'))
}

fn go_well_known_interface_methods_from_call(
    call: &CallgraphOutboundCall,
) -> Option<&'static [&'static str]> {
    let (target, full_callee) = split_call_target_metadata(&call.target);
    let callee = full_callee.unwrap_or(target).trim();
    // Go interface methods are invoked by library code outside the project
    // graph. These entry calls add method names only; the final liveness check
    // is still gated to Go method exports, not functions.
    match callee {
        "sort.Sort" | "sort.Stable" | "sort.IsSorted" => Some(&["Len", "Less", "Swap"]),
        "list.New" => Some(&["FilterValue"]),
        _ => None,
    }
}

fn split_call_target_metadata(target: &str) -> (&str, Option<&str>) {
    target
        .split_once(DISPATCHED_CALLEE_SEPARATOR)
        .map_or((target, None), |(target, full_callee)| {
            (target, Some(full_callee))
        })
}

fn symbol_liveness_name(symbol: &str) -> &str {
    symbol
        .rsplit(['.', ':', '#'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(symbol)
}

fn is_type_like_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct" | "enum" | "trait" | "type" | "type_alias" | "interface"
    )
}

fn parse_target(project_root: &Path, target: &str) -> ParsedTarget {
    let (target, _) = split_call_target_metadata(target);
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return ParsedTarget {
            file: None,
            symbol: None,
        };
    }

    if let Some((file, symbol)) = split_file_symbol_target(project_root, trimmed, "::") {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    if let Some((file, symbol)) = trimmed.rsplit_once('#') {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    ParsedTarget {
        file: None,
        symbol: clean_symbol(trimmed),
    }
}

fn split_file_symbol_target<'a>(
    project_root: &Path,
    target: &'a str,
    separator: &str,
) -> Option<(&'a str, &'a str)> {
    let mut search_start = 0;
    while let Some(offset) = target[search_start..].find(separator) {
        let split_at = search_start + offset;
        let file = &target[..split_at];
        let symbol = &target[split_at + separator.len()..];
        if !symbol.trim().is_empty() && looks_like_source_file_target(project_root, file) {
            return Some((file, symbol));
        }
        search_start = split_at + separator.len();
    }
    None
}

fn looks_like_source_file_target(project_root: &Path, file: &str) -> bool {
    let path = Path::new(file);
    language_for_file(file) != "unknown" || path.is_file() || project_root.join(path).is_file()
}

fn clean_symbol(symbol: &str) -> Option<String> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn liveness_roots_for_file(
    file_name: &str,
    exports: &[ExportContribution],
    internal_calls: &[InternalCall],
    attribute_entry_points: &BTreeSet<String>,
    executable_root_exports: Option<&BTreeSet<String>>,
    is_liveness_root_file: bool,
    is_public_api_file: bool,
) -> Vec<String> {
    let mut roots = attribute_entry_points
        .iter()
        .filter_map(|symbol| clean_symbol(symbol))
        .collect::<BTreeSet<_>>();

    if !is_liveness_root_file && !is_public_api_file {
        return roots.into_iter().collect();
    }

    roots.insert("<top-level>".to_string());
    if is_public_api_file {
        roots.extend(exports.iter().map(|export| export.symbol.clone()));
    } else if let Some(executable_root_exports) = executable_root_exports {
        roots.extend(executable_root_exports.iter().cloned());
    } else {
        roots.extend(
            exports
                .iter()
                .filter(|export| is_explicit_liveness_symbol(file_name, &export.symbol))
                .map(|export| export.symbol.clone()),
        );
        roots.extend(
            internal_calls
                .iter()
                .map(|call| call.caller_symbol.as_str())
                .filter(|symbol| is_explicit_liveness_symbol(file_name, symbol))
                .map(str::to_string),
        );
    }

    roots.into_iter().collect()
}

fn is_explicit_liveness_symbol(file_name: &str, symbol: &str) -> bool {
    let symbol = symbol.rsplit("::").next().unwrap_or(symbol);
    if symbol == "<top-level>" {
        return true;
    }

    let lower = symbol.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "main" | "init" | "setup" | "bootstrap" | "run"
    ) {
        return true;
    }

    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == symbol)
}

pub(crate) fn collect_public_api_files(project_root: &Path) -> BTreeSet<String> {
    crate::inspect::entry_points::resolve_entry_points(project_root)
        .public_api_files_relative(project_root)
}

fn language_for_file(file: &str) -> &'static str {
    detect_language(Path::new(file))
        .map(language_name)
        .unwrap_or("unknown")
}

fn supports_type_refs(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
}

fn collect_freshness(file: &Path) -> FileFreshness {
    cache_freshness::collect(file).unwrap_or_else(|_| FileFreshness {
        mtime: UNIX_EPOCH,
        size: 0,
        content_hash: cache_freshness::zero_hash(),
    })
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    let normalized_root = canonicalize_normalized(project_root);
    let normalized = canonicalize_normalized(&absolute);
    normalized
        .strip_prefix(&normalized_root)
        .unwrap_or(normalized.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

fn canonical_or_normalized(project_root: &Path, path: &Path) -> PathBuf {
    // Delegates to the oxc engine's input normalizer so FileFacts paths built
    // here compare equal to the engine's entry-point/executable-root sets.
    // Calling fs::canonicalize directly is wrong on Windows: it returns
    // verbatim (\\?\C:\) paths while those sets are de-verbatimed, and the
    // membership miss silently drops entry-point liveness.
    crate::inspect::oxc_engine::normalize_input_path(project_root, path)
}

fn normalize_absolute(project_root: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_path(&absolute)
}

fn normalize_path(path: &Path) -> PathBuf {
    // Delegates to the subsystem-wide normalizer: a components-only local
    // version kept Windows verbatim prefixes, so map keys built here failed
    // to join lookups built from verbatim-stripped roots.
    crate::inspect::job::normalize_path(path)
}

#[derive(Debug, Clone, Deserialize)]
struct DeadCodeContribution {
    file: String,
    #[serde(default)]
    generated: Option<bool>,
    exports: Vec<ExportContribution>,
    #[serde(default)]
    facts_format_version: Option<u32>,
    #[serde(default)]
    raw_imports: Vec<RawImportContribution>,
    #[serde(default)]
    raw_reexports: Vec<RawReexportContribution>,
    #[serde(default)]
    rust_imports: Vec<RawImportContribution>,
    #[serde(default)]
    macro_token_refs: Vec<MacroTokenRefContribution>,
    #[serde(default)]
    attribute_entry_points: Vec<String>,
    #[serde(default)]
    cfg_test_ranges: Vec<RustCfgTestRange>,
    #[serde(default)]
    oxc_facts: Option<OxcFactsContribution>,
    #[serde(default)]
    internal_calls: Vec<InternalCallContribution>,
    #[serde(default)]
    liveness_roots: Vec<String>,
    #[serde(default)]
    imported_exports: Vec<ImportedExportContribution>,
    #[serde(default)]
    namespace_imported_exports: Vec<ImportedExportContribution>,
    #[serde(default)]
    dispatched_method_names: Vec<String>,
    #[serde(default)]
    type_ref_names: Vec<String>,
    #[serde(default)]
    parse_errors: Vec<Value>,
    #[serde(default)]
    skipped_files: Vec<Value>,
    #[serde(default)]
    skipped_languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawImportContribution {
    source: String,
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    default_import: Option<String>,
    #[serde(default)]
    namespace_import: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawReexportContribution {
    language: String,
    source: String,
    kind: String,
    #[serde(default)]
    imported: Option<String>,
    #[serde(default)]
    exported: Option<String>,
    line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct MacroTokenRefContribution {
    caller_symbol: String,
    line: u32,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<Vec<String>>,
    shape: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OxcFactsContribution {
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

#[derive(Debug, Clone, Deserialize)]
struct ImportedExportContribution {
    file: String,
    symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
    #[serde(default)]
    is_type_like: bool,
    #[serde(default)]
    is_entry_point: bool,
    #[serde(default)]
    has_references: bool,
    #[serde(default)]
    test_only_reference_files: Vec<String>,
    #[serde(default)]
    verdict: Option<LivenessVerdict>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
    #[serde(default)]
    also_reexported: Vec<OxcReExportContext>,
}

#[derive(Debug, Clone, Deserialize)]
struct InternalCallContribution {
    #[serde(default)]
    caller_symbol: String,
    file: String,
    symbol: String,
    #[serde(default)]
    test_origin: Option<bool>,
}

impl From<InternalCall> for InternalCallContribution {
    fn from(call: InternalCall) -> Self {
        Self {
            caller_symbol: call.caller_symbol,
            file: call.file,
            symbol: call.symbol,
            test_origin: call.test_origin,
        }
    }
}

#[derive(Debug, Clone)]
struct InternalCall {
    caller_symbol: String,
    file: String,
    symbol: String,
    line: u32,
    provenance: String,
    test_origin: Option<bool>,
}

#[derive(Debug, Clone)]
struct ParsedTarget {
    file: Option<String>,
    symbol: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn reachability_fixture(edges: &[(&str, &str)], roots: &[&str]) -> ReachabilityState {
        let mut by_source = BTreeMap::<ExportNode, BTreeSet<ExportNode>>::new();
        for (source, target) in edges {
            by_source
                .entry(("graph.ts".to_string(), (*source).to_string()))
                .or_default()
                .insert(("graph.ts".to_string(), (*target).to_string()));
        }
        let mut roots = roots
            .iter()
            .map(|root| ("graph.ts".to_string(), (*root).to_string()))
            .collect::<BTreeSet<_>>();
        roots.insert(("stable.ts".to_string(), "stable_root".to_string()));
        let mut state = ReachabilityState {
            edges: by_source,
            imported_by_file: BTreeMap::new(),
            namespace_by_file: BTreeMap::new(),
            roots,
            dispatch_roots: BTreeSet::new(),
            reachable: BTreeSet::new(),
        };
        state.reachable = traverse_reachable(&state, BTreeSet::new(), state.roots.iter().cloned());
        state
    }

    fn assert_incremental_reachability_parity(
        previous: &ReachabilityState,
        mut current: ReachabilityState,
    ) -> ReachabilityState {
        let full = current.reachable.clone();
        current.reachable = incremental_reachable(
            previous,
            &current,
            &["graph.ts".to_string()].into_iter().collect(),
        );
        assert_eq!(current.reachable, full);
        current
    }

    /// Builds a contribution the way the production path does — from the JSON
    /// facts record — so every `#[serde(default)]` field keeps its default.
    fn dispatch_contribution(file: &str, exports: &[(&str, &str)]) -> DeadCodeContribution {
        serde_json::from_value(json!({
            "file": file,
            "exports": exports
                .iter()
                .map(|(symbol, kind)| json!({"symbol": symbol, "kind": kind, "line": 1}))
                .collect::<Vec<_>>(),
        }))
        .expect("contribution facts")
    }

    /// One edge per source, all pointing at the same target: only the source
    /// side of an edge decides whether it is a dispatch root.
    fn dispatch_edges(sources: &[(&str, &str)]) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
        sources
            .iter()
            .map(|(file, symbol)| {
                (
                    ((*file).to_string(), (*symbol).to_string()),
                    BTreeSet::from([("src/target.ts".to_string(), "target".to_string())]),
                )
            })
            .collect()
    }

    #[test]
    fn dispatch_roots_require_a_contribution_and_a_dispatched_name() {
        // Two non-Go languages, Go, and a language with no dispatched names.
        let contributions = vec![
            dispatch_contribution("src/service.ts", &[("render", "method")]),
            dispatch_contribution("src/worker.py", &[("process", "function")]),
            dispatch_contribution(
                "src/server.go",
                &[
                    ("Serve", "method"),
                    ("Handle", "function"),
                    ("helper", "method"),
                ],
            ),
            dispatch_contribution("src/plain.zig", &[("render", "method")]),
        ];
        let dispatched_method_names = MethodNamesByLanguage::from([
            (
                "typescript".to_string(),
                BTreeSet::from(["render".to_string(), "handle".to_string()]),
            ),
            ("python".to_string(), BTreeSet::from(["process".to_string()])),
            (
                "go".to_string(),
                BTreeSet::from(["Serve".to_string(), "Handle".to_string()]),
            ),
        ]);
        let edges = dispatch_edges(&[
            // Non-Go: membership is the language's name set, with no
            // method-kind gate.
            ("src/service.ts", "render"),
            ("src/service.ts", "handle"),
            ("src/service.ts", "missing"),
            ("src/worker.py", "process"),
            // Go: only methods the file itself exports and that are dispatched.
            ("src/server.go", "Serve"),
            ("src/server.go", "Handle"),
            ("src/server.go", "helper"),
            // The file's language has no dispatched names at all.
            ("src/plain.zig", "render"),
            // The extension maps to a language that has the name, but the file
            // is not a contribution, so it can never be a dispatch root.
            ("src/absent.ts", "render"),
            ("src/absent.py", "process"),
            // Unknown extension.
            ("src/absent.xyz", "render"),
        ]);

        let state = reachability_inputs(&contributions, edges, &dispatched_method_names);

        assert_eq!(
            state.dispatch_roots,
            BTreeSet::from([
                ("src/service.ts".to_string(), "render".to_string()),
                ("src/service.ts".to_string(), "handle".to_string()),
                ("src/worker.py".to_string(), "process".to_string()),
                ("src/server.go".to_string(), "Serve".to_string()),
            ])
        );
    }

    #[test]
    fn dispatch_index_does_not_materialize_names_per_file() {
        // The defect this guards: every non-Go file used to receive a copy of
        // its language's whole dispatched-name set, so the index cost
        // files x names. With 400 files and 400 names that is 160,000 stored
        // names; the index must instead store none of them.
        const FILES: usize = 400;
        const NAMES: usize = 400;
        let contributions = (0..FILES)
            .map(|index| dispatch_contribution(&format!("src/file_{index}.ts"), &[]))
            .collect::<Vec<_>>();
        let dispatched_method_names = MethodNamesByLanguage::from([(
            "typescript".to_string(),
            (0..NAMES).map(|index| format!("method_{index}")).collect(),
        )]);

        let index = dispatch_live_source_names_by_file(&contributions, &dispatched_method_names);

        assert_eq!(index.len(), FILES, "one entry per contributing file");
        let materialized = index
            .values()
            .map(DispatchNamesForFile::materialized_name_count)
            .sum::<usize>();
        assert_eq!(
            materialized, 0,
            "non-Go files must borrow their language's name set, not copy it"
        );
        // Membership still answers from the language set.
        assert!(index["src/file_0.ts"].contains("method_399"));
        assert!(!index["src/file_0.ts"].contains("absent"));
    }

    #[test]
    fn vanished_contribution_drops_its_fragment_without_a_changed_file_entry() {
        // A deleted file's contribution disappears from the set the rollup is
        // given, but the host may spell the deletion differently from the
        // contribution key (Windows backslashes) or not name it at all; the
        // fragment must go regardless.
        let (_temp, root, files) = fixture_project(&[
            ("main.rs", "pub fn main() {}\n"),
            ("dead.rs", "pub fn planted_dead() {}\n"),
        ]);
        let snapshot = snapshot_with_entry_points(
            files.clone(),
            vec![
                export(&root, "main.rs", "main", "function"),
                export(&root, "dead.rs", "planted_dead", "function"),
            ],
            Vec::new(),
            [root.join("main.rs")].into_iter().collect(),
        );
        let scan_job = job(&root, files.clone(), snapshot.clone());
        let contributions = run_dead_code_scan(&scan_job)
            .outcome
            .expect("initial scan")
            .contributions;
        let public = BTreeSet::new();
        let roles = crate::inspect::entry_points::ProjectRoles::default();
        let (initial, state, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &snapshot,
            &contributions,
            &public,
            &roles,
            None,
            Some("vanish"),
            None,
            &BTreeSet::new(),
        );
        assert!(aggregate_has_item(&initial, "dead.rs", "planted_dead"));

        let remaining = contributions
            .iter()
            .filter(|contribution| contribution.contribution["file"] != "dead.rs")
            .cloned()
            .collect::<Vec<_>>();
        let (incremental, _, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &snapshot,
            &remaining,
            &public,
            &roles,
            None,
            Some("vanish"),
            Some(&state),
            &["dead\\.rs".to_string()].into_iter().collect(),
        );
        let (full, _, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &snapshot,
            &remaining,
            &public,
            &roles,
            None,
            Some("vanish"),
            None,
            &BTreeSet::new(),
        );
        assert_eq!(incremental, full);
        assert!(!aggregate_has_item(&incremental, "dead.rs", "planted_dead"));
    }

    #[test]
    fn incremental_aggregate_matches_full_for_reachability_flip_and_contribution_change() {
        let (_temp, root, files) = fixture_project(&[
            ("main.rs", "pub fn main() { target(); }\n"),
            ("target.rs", "pub fn target() {}\n"),
        ]);
        let live_snapshot = snapshot_with_entry_points(
            files.clone(),
            vec![
                export(&root, "main.rs", "main", "function"),
                export(&root, "target.rs", "target", "function"),
            ],
            vec![outbound(&root, "main.rs", "main", "target.rs::target")],
            [root.join("main.rs")].into_iter().collect(),
        );
        let scan_job = job(&root, files.clone(), live_snapshot.clone());
        let contributions = run_dead_code_scan(&scan_job)
            .outcome
            .expect("initial scan")
            .contributions;
        let public = BTreeSet::new();
        let roles = crate::inspect::entry_points::ProjectRoles::default();
        let (live, state, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &live_snapshot,
            &contributions,
            &public,
            &roles,
            None,
            Some("sequence"),
            None,
            &BTreeSet::new(),
        );

        let orphaned_snapshot = snapshot_with_entry_points(
            files,
            vec![
                export(&root, "main.rs", "main", "function"),
                export(&root, "target.rs", "target", "function"),
            ],
            Vec::new(),
            [root.join("main.rs")].into_iter().collect(),
        );
        let changed = ["main.rs".to_string()].into_iter().collect();
        let (incremental, state, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &orphaned_snapshot,
            &contributions,
            &public,
            &roles,
            None,
            Some("sequence"),
            Some(&state),
            &changed,
        );
        let (full, _, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &orphaned_snapshot,
            &contributions,
            &public,
            &roles,
            None,
            Some("sequence"),
            None,
            &BTreeSet::new(),
        );
        assert_eq!(incremental, full);
        assert_ne!(incremental, live);
        assert!(aggregate_has_item(&incremental, "target.rs", "target"));

        let mut changed_contributions = contributions.clone();
        let target = changed_contributions
            .iter_mut()
            .find(|contribution| contribution.contribution["file"] == "target.rs")
            .expect("target contribution");
        target.contribution["exports"][0]["line"] = json!(99);
        let changed = ["target.rs".to_string()].into_iter().collect();
        let (incremental, _, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &orphaned_snapshot,
            &changed_contributions,
            &public,
            &roles,
            None,
            Some("sequence"),
            Some(&state),
            &changed,
        );
        let (full, _, _) = aggregate_dead_code_contributions_incremental(
            &root,
            &orphaned_snapshot,
            &changed_contributions,
            &public,
            &roles,
            None,
            Some("sequence"),
            None,
            &BTreeSet::new(),
        );
        assert_eq!(incremental, full);
        assert_eq!(incremental["items"][0]["line"], json!(99));
    }

    #[test]
    fn incremental_reachability_matches_full_across_cycle_diamond_and_orphan_edits() {
        let initial = reachability_fixture(
            &[
                ("root", "a"),
                ("a", "b"),
                ("a", "c"),
                ("b", "d"),
                ("c", "d"),
                ("d", "e"),
                ("e", "d"),
                ("a", "orphan"),
                ("x", "y"),
            ],
            &["root"],
        );
        let removed_only_path = assert_incremental_reachability_parity(
            &initial,
            reachability_fixture(
                &[
                    ("root", "a"),
                    ("a", "b"),
                    ("a", "c"),
                    ("b", "d"),
                    ("c", "d"),
                    ("d", "e"),
                    ("e", "d"),
                    ("x", "y"),
                ],
                &["root"],
            ),
        );
        assert!(!removed_only_path
            .reachable
            .contains(&("graph.ts".to_string(), "orphan".to_string())));

        let newly_reached = assert_incremental_reachability_parity(
            &removed_only_path,
            reachability_fixture(
                &[
                    ("root", "a"),
                    ("a", "b"),
                    ("a", "c"),
                    ("b", "d"),
                    ("c", "d"),
                    ("d", "e"),
                    ("e", "d"),
                    ("c", "orphan"),
                    ("x", "z"),
                ],
                &["root"],
            ),
        );
        assert!(newly_reached
            .reachable
            .contains(&("graph.ts".to_string(), "orphan".to_string())));

        let root_removed = assert_incremental_reachability_parity(
            &newly_reached,
            reachability_fixture(
                &[
                    ("a", "b"),
                    ("a", "c"),
                    ("b", "d"),
                    ("c", "d"),
                    ("d", "e"),
                    ("e", "d"),
                    ("c", "orphan"),
                    ("x", "z"),
                ],
                &[],
            ),
        );
        assert_eq!(
            root_removed.reachable,
            [("stable.ts".to_string(), "stable_root".to_string())]
                .into_iter()
                .collect()
        );
    }
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};

    use crate::config::Config;
    use crate::inspect::job::{CALLGRAPH_PROVENANCE_TREESITTER, DISPATCHED_CALLEE_SEPARATOR};
    use crate::inspect::{CallgraphExport, JobKey};
    use crate::parser::SymbolCache;

    fn fixture_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        fs::create_dir_all(&root).expect("create project root");

        let paths = files
            .iter()
            .map(|(relative, contents)| {
                let path = root.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("create parent");
                }
                fs::write(&path, contents).expect("write fixture file");
                path
            })
            .collect::<Vec<_>>();

        (temp_dir, root, paths)
    }

    fn job(root: &Path, scope_files: Vec<PathBuf>, snapshot: CallgraphSnapshot) -> InspectJob {
        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join(".aft-cache").join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            inspect_writer: true,
            callgraph_writer: true,
            callgraph_snapshot: Some(Arc::new(snapshot)),
        }
    }

    fn snapshot(
        files: Vec<PathBuf>,
        exported_symbols: Vec<CallgraphExport>,
        outbound_calls: Vec<CallgraphOutboundCall>,
    ) -> CallgraphSnapshot {
        snapshot_with_entry_points(files, exported_symbols, outbound_calls, BTreeSet::new())
    }

    fn snapshot_with_entry_points(
        files: Vec<PathBuf>,
        exported_symbols: Vec<CallgraphExport>,
        outbound_calls: Vec<CallgraphOutboundCall>,
        entry_points: BTreeSet<PathBuf>,
    ) -> CallgraphSnapshot {
        CallgraphSnapshot {
            generated_at: None,
            files,
            exported_symbols,
            outbound_calls,
            entry_points,
            entry_point_symbols: BTreeMap::new(),
        }
    }

    fn export(root: &Path, file: &str, symbol: &str, kind: &str) -> CallgraphExport {
        CallgraphExport {
            file: root.join(file),
            symbol: symbol.to_string(),
            kind: kind.to_string(),
            line: 1,
        }
    }

    fn outbound(
        root: &Path,
        caller_file: &str,
        caller_symbol: &str,
        target: &str,
    ) -> CallgraphOutboundCall {
        CallgraphOutboundCall {
            caller_file: root.join(caller_file),
            caller_symbol: caller_symbol.to_string(),
            target: target.to_string(),
            line: 1,
            provenance: CALLGRAPH_PROVENANCE_TREESITTER.to_string(),
        }
    }

    fn dispatched_target(target: &str, full_callee: &str) -> String {
        format!("{target}{DISPATCHED_CALLEE_SEPARATOR}{full_callee}")
    }

    fn scan(job: InspectJob) -> serde_json::Value {
        run_dead_code_scan(&job)
            .outcome
            .expect("scan succeeds")
            .aggregate
    }

    #[test]
    fn cfg_test_predicate_requires_every_possible_branch_to_be_test_only() {
        assert!(cfg_predicate_requires_test("test"));
        assert!(cfg_predicate_requires_test("all(unix,test)"));
        assert!(cfg_predicate_requires_test(
            "any(all(test,unix),all(test,windows))"
        ));
        assert!(!cfg_predicate_requires_test("any(test,unix)"));
        assert!(!cfg_predicate_requires_test("not(test)"));
    }

    fn aggregate_has_item(aggregate: &serde_json::Value, file: &str, symbol: &str) -> bool {
        aggregate
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .any(|item| {
                item.get("file").and_then(serde_json::Value::as_str) == Some(file)
                    && item.get("symbol").and_then(serde_json::Value::as_str) == Some(symbol)
            })
    }

    #[test]
    fn contributions_persist_non_generated_classification() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/hand.ts", "export const hand = 1;\n"),
            ("build.gradle", "task smokeTest {}\n"),
        ]);
        let success = run_dead_code_scan(&job(
            &root,
            paths.clone(),
            snapshot(paths.clone(), Vec::new(), Vec::new()),
        ))
        .outcome
        .expect("scan succeeds");

        assert_eq!(success.contributions.len(), 2);
        assert!(success.contributions.iter().all(|contribution| {
            contribution
                .contribution
                .get("generated")
                .and_then(Value::as_bool)
                == Some(false)
        }));
    }

    #[test]
    fn groovy_dead_code_scan_reports_language_skipped_without_fabricated_counts() {
        let (_temp_dir, root, paths) = fixture_project(&[(
            "build.gradle",
            "task smokeTest {\n    doLast {\n        println 'smoke'\n    }\n}\n",
        )]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(paths.clone(), Vec::new(), Vec::new()),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["total_count"], 0);
        assert_eq!(
            aggregate["languages_skipped"],
            serde_json::json!(["groovy"])
        );
        assert_eq!(aggregate["by_language"], serde_json::json!({}));
        assert!(aggregate["items"]
            .as_array()
            .is_some_and(|items| items.is_empty()));
        assert_eq!(aggregate["complete"], true);
    }

    fn rust_entry_scan(
        files: &[(&str, &str)],
        exports: &[(&str, &str, &str)],
    ) -> serde_json::Value {
        let (_temp_dir, root, paths) = fixture_project(files);
        let entry_points = [root.join("src/main.rs")]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let exports = exports
            .iter()
            .map(|(file, symbol, kind)| export(&root, file, symbol, kind))
            .collect::<Vec<_>>();
        scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, Vec::new(), entry_points),
        ))
    }

    fn scan_success_with_oxc(job: InspectJob) -> InspectScanSuccess {
        let entry_points = crate::inspect::entry_points::resolve_entry_points(&job.project_root);
        let options = AnalyzeOptions {
            entry_points: job
                .callgraph_snapshot
                .as_ref()
                .map(|snapshot| snapshot.entry_points.iter().cloned().collect())
                .unwrap_or_default(),
            public_api_files: Vec::new(),
            executable_root_exports: entry_points.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: true,
        };
        let oxc_result =
            crate::inspect::oxc_engine::analyze_files(&job.project_root, &job.scope_files, options)
                .expect("oxc analyze succeeds");
        run_dead_code_scan_with_oxc(&job, Some(&oxc_result))
            .outcome
            .expect("scan succeeds")
    }

    fn scan_with_oxc(job: InspectJob) -> serde_json::Value {
        scan_success_with_oxc(job).aggregate
    }

    fn aggregate_item<'a>(
        aggregate: &'a serde_json::Value,
        file: &str,
        symbol: &str,
    ) -> Option<&'a serde_json::Value> {
        aggregate["items"].as_array()?.iter().find(|item| {
            item["file"].as_str() == Some(file) && item["symbol"].as_str() == Some(symbol)
        })
    }

    fn aggregate_generated_item<'a>(
        aggregate: &'a serde_json::Value,
        file: &str,
        symbol: &str,
    ) -> Option<&'a serde_json::Value> {
        aggregate["generated_items"]
            .as_array()?
            .iter()
            .find(|item| {
                item["file"].as_str() == Some(file) && item["symbol"].as_str() == Some(symbol)
            })
    }

    fn aggregate_test_only_item<'a>(
        aggregate: &'a serde_json::Value,
        file: &str,
        symbol: &str,
    ) -> Option<&'a serde_json::Value> {
        aggregate["test_only_items"]
            .as_array()?
            .iter()
            .find(|item| {
                item["file"].as_str() == Some(file) && item["symbol"].as_str() == Some(symbol)
            })
    }

    #[test]
    fn oxc_dead_code_splits_test_only_references_from_headline() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("package.json", r#"{"main":"src/main.ts"}"#),
            (
                "src/main.ts",
                "import { productUsed } from './api';
export function main() { productUsed(); }
",
            ),
            (
                "src/api.ts",
                "export function testOnly() {}
export function productUsed() {}
",
            ),
            (
                "src/dead.ts",
                "export function plantedDead() {}
",
            ),
            (
                "src/api.test.ts",
                "import { testOnly } from './api';
testOnly();
",
            ),
            (
                "src/barrel-target.ts",
                "export function throughBarrel() {}
export function barrelDead() {}
",
            ),
            (
                "src/barrel.ts",
                "export { throughBarrel } from './barrel-target';
",
            ),
            (
                "src/barrel.test.ts",
                "import { throughBarrel } from './barrel';
throughBarrel();
",
            ),
        ]);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture path"))
            .collect::<Vec<_>>();
        let entry_points = BTreeSet::from([root.join("src/main.ts")]);
        let graph = snapshot_with_entry_points(paths.clone(), Vec::new(), Vec::new(), entry_points);

        let aggregate = scan_with_oxc(job(&root, paths, graph));

        assert_eq!(aggregate["count"], 2, "{aggregate:#}");
        assert!(aggregate_item(&aggregate, "src/dead.ts", "plantedDead").is_some());
        assert!(aggregate_item(&aggregate, "src/barrel-target.ts", "barrelDead").is_some());
        assert!(aggregate_item(&aggregate, "src/api.ts", "testOnly").is_none());
        assert!(aggregate_item(&aggregate, "src/api.ts", "productUsed").is_none());
        assert!(aggregate_item(&aggregate, "src/barrel-target.ts", "throughBarrel").is_none());

        assert_eq!(aggregate["test_only_count"], 2, "{aggregate:#}");
        assert_eq!(
            aggregate_test_only_item(&aggregate, "src/api.ts", "testOnly")
                .and_then(|item| item["used_by"].as_array())
                .and_then(|items| items.first())
                .and_then(serde_json::Value::as_str),
            Some("api.test.ts")
        );
        assert_eq!(
            aggregate_test_only_item(&aggregate, "src/barrel-target.ts", "throughBarrel")
                .and_then(|item| item["used_by"].as_array())
                .and_then(|items| items.first())
                .and_then(serde_json::Value::as_str),
            Some("barrel.test.ts")
        );
    }

    #[test]
    fn oxc_dead_code_buckets_generated_exports_below_headline() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("package.json", r#"{"main":"src/main.ts"}"#),
            (
                "src/main.ts",
                "console.log('main');
",
            ),
            (
                "src/hand.ts",
                "export function handDead() {}
",
            ),
            (
                "gen/schema_pb.ts",
                "export function generatedPathDead() {}
",
            ),
            (
                "src/banner.ts",
                "// Code generated by fixture. DO NOT EDIT.
export function bannerDead() {}
",
            ),
        ]);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture path"))
            .collect::<Vec<_>>();
        let entry_points = BTreeSet::from([root.join("src/main.ts")]);
        let graph = snapshot_with_entry_points(paths.clone(), Vec::new(), Vec::new(), entry_points);

        let first = scan_success_with_oxc(job(&root, paths.clone(), graph.clone()));
        let second = scan_success_with_oxc(job(&root, paths.clone(), graph.clone()));
        assert_eq!(
            first.aggregate, second.aggregate,
            "twice-cold scan must be deterministic"
        );

        assert_eq!(first.aggregate["count"], 1, "{:#}", first.aggregate);
        assert_eq!(
            first.aggregate["generated_count"], 2,
            "{:#}",
            first.aggregate
        );
        assert_eq!(first.aggregate["total_count"], 3, "{:#}", first.aggregate);
        assert!(aggregate_item(&first.aggregate, "src/hand.ts", "handDead").is_some());
        assert!(aggregate_generated_item(
            &first.aggregate,
            "gen/schema_pb.ts",
            "generatedPathDead"
        )
        .is_some());
        assert!(
            aggregate_generated_item(&first.aggregate, "src/banner.ts", "bannerDead").is_some()
        );

        let item_files = first.aggregate["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["file"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(item_files.first(), Some(&"src/hand.ts"), "{item_files:?}");

        let roles = crate::inspect::entry_points::resolve_project_roles(&root);
        let rolled_up = aggregate_dead_code_contributions_with_snapshot(
            &root,
            &graph,
            &first.contributions,
            &collect_public_api_files(&root),
            &roles,
            Some(MAX_DRILL_DOWN_ITEMS),
        );
        assert_eq!(
            rolled_up, first.aggregate,
            "cached rollup must match cold aggregate"
        );
    }

    #[test]
    fn oxc_dead_code_test_file_edit_cached_rollup_matches_cold() {
        let (_temp_dir, root, paths) = fixture_project(&[
            (
                "src/api.ts",
                "export function testOnly() {}
export function plantedDead() {}
",
            ),
            (
                "src/api.test.ts",
                "import { testOnly } from './api';
testOnly();
",
            ),
        ]);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture path"))
            .collect::<Vec<_>>();
        let graph =
            snapshot_with_entry_points(paths.clone(), Vec::new(), Vec::new(), BTreeSet::new());
        let first = scan_success_with_oxc(job(&root, paths.clone(), graph.clone()));
        assert_eq!(first.aggregate["count"], 1, "{:#}", first.aggregate);
        assert_eq!(
            first.aggregate["test_only_count"], 1,
            "{:#}",
            first.aggregate
        );

        fs::write(
            root.join("src/api.test.ts"),
            "console.log('import removed');
",
        )
        .expect("edit test file");

        let cold = scan_success_with_oxc(job(&root, paths.clone(), graph.clone()));
        let changed_test = scan_success_with_oxc(job(
            &root,
            vec![root.join("src/api.test.ts")],
            graph.clone(),
        ));
        let mut cached_contributions = first.contributions.clone();
        for changed in changed_test.contributions {
            let slot = cached_contributions
                .iter_mut()
                .find(|contribution| contribution.file_path == changed.file_path)
                .expect("cached test contribution exists");
            *slot = changed;
        }
        let roles = crate::inspect::entry_points::resolve_project_roles(&root);
        let rolled_up = aggregate_dead_code_contributions_with_snapshot(
            &root,
            &graph,
            &cached_contributions,
            &collect_public_api_files(&root),
            &roles,
            Some(MAX_DRILL_DOWN_ITEMS),
        );

        assert_eq!(rolled_up, cold.aggregate);
        assert_eq!(rolled_up["count"], 2, "{rolled_up:#}");
        assert_eq!(rolled_up["test_only_count"], 0, "{rolled_up:#}");
    }

    #[test]
    fn rust_macro_receiver_call_in_cfg_test_module_is_test_only() {
        let (temp_dir, root, paths) = fixture_project(&[
            ("src/lib.rs", "mod index;\n"),
            (
                "src/index.rs",
                r#"pub struct Index(u32);

impl Index {
    pub fn shares_index_with(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

#[cfg(test)]
mod tests {
    use super::Index;

    #[test]
    fn compares_indexes() {
        let before = Index(1);
        let after = Index(1);
        assert!(before.shares_index_with(&after));
    }
}
"#,
            ),
        ]);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture file"))
            .collect::<Vec<_>>();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dead-code-test-module-fixture\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let analysis =
            DeadCodeFileAnalyzer::default().analyze_file(&root.join("src/index.rs"), false);
        assert!(
            analysis
                .cfg_test_ranges
                .iter()
                .any(|range| range.contains(17)),
            "cfg(test) module should classify its receiver call line as test-only: {:?}",
            analysis.cfg_test_ranges
        );
        let store = crate::callgraph_store::CallGraphStore::open(
            temp_dir.path().join("callgraph-store"),
            root.clone(),
        )
        .expect("open callgraph store");
        store.cold_build(&paths).expect("build callgraph store");
        let snapshot = crate::callgraph_store::project_dead_code_snapshot(store.sqlite_path())
            .expect("project dead-code snapshot");
        let aggregate = scan(job(&root, paths, snapshot));
        assert!(
            aggregate_test_only_item(&aggregate, "src/index.rs", "shares_index_with").is_some(),
            "receiver method should be reported only in the test-only bucket: {aggregate:#}"
        );
        assert!(
            !aggregate_has_item(&aggregate, "src/index.rs", "shares_index_with"),
            "receiver method must not remain in the dead-code headline: {aggregate:#}"
        );
    }

    #[test]
    fn method_dispatched_by_receiver_call_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/service.ts", "export class Service { render() {} }\n"),
            (
                "src/consumer.ts",
                "function run(service: Service) { service.render(); }\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/service.ts", "render", "method")],
                vec![outbound(
                    &root,
                    "src/consumer.ts",
                    "run",
                    &dispatched_target("render", "service.render"),
                )],
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn method_without_any_dispatch_is_still_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/service.ts", "export class Service { render() {} }\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/service.ts", "render", "method")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "render");
        assert_eq!(aggregate["uncertain_count"], 0);
    }

    #[test]
    fn free_function_called_from_dispatch_live_method_body_is_live() {
        // Regression for the dead_code reachability bug: a free function reached
        // only through a method whose only caller is a receiver dispatch
        // (`obj.method()`) must NOT be reported dead. The method ("render") is
        // rescued from the dead list by dispatch-name, but liveness must also
        // flow THROUGH its body to the free function it calls ("helper").
        // Mirrors the real `BgTaskRegistry::spawn` -> `task_paths` case, where
        // `task_paths` had 33 callers yet was flagged dead because the BFS never
        // entered the dispatch-only method body. Method bodies are keyed by
        // scoped identity (`Service::render`) while exports are bare (`render`),
        // so the body edge is unreachable without seeding the scoped method node.
        let (_temp_dir, root, paths) = fixture_project(&[
            (
                "src/service.ts",
                "export class Service { render() { helper(); } }\n",
            ),
            ("src/helper.ts", "export function helper() {}\n"),
            (
                "src/consumer.ts",
                "function run(service: Service) { service.render(); }\n",
            ),
        ]);
        let helper_target = format!("{}::helper", root.join("src/helper.ts").display());
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![
                    export(&root, "src/service.ts", "render", "method"),
                    export(&root, "src/helper.ts", "helper", "function"),
                ],
                vec![
                    // The method's ONLY caller is a receiver dispatch — no
                    // resolvable edge into `Service::render`.
                    outbound(
                        &root,
                        "src/consumer.ts",
                        "run",
                        &dispatched_target("render", "service.render"),
                    ),
                    // The dispatch-only method body calls a free function. The
                    // caller identity is scoped (`Service::render`), the form the
                    // edge map uses for sources.
                    outbound(&root, "src/service.ts", "Service::render", &helper_target),
                ],
            ),
        ));

        assert_eq!(
            aggregate["count"], 0,
            "free function reached via dispatch-live method body must be live: {aggregate:#}"
        );
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn rust_struct_referenced_only_in_types_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/types.rs", "pub struct Widget { id: u64 }\n"),
            (
                "src/main.rs",
                "use crate::types::Widget;\nstruct Holder { value: Widget }\npub fn main(input: Widget) -> Widget { input }\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(
                paths,
                vec![
                    export(&root, "src/types.rs", "Widget", "struct"),
                    export(&root, "src/main.rs", "main", "function"),
                ],
                Vec::new(),
                BTreeSet::from([root.join("src/main.rs")]),
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ts_interface_referenced_only_in_type_annotation_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/types.ts", "export interface Widget { id: string }\n"),
            (
                "src/main.ts",
                "import type { Widget } from './types';\nexport function run(input: Widget): void {}\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(
                paths,
                vec![
                    export(&root, "src/types.ts", "Widget", "interface"),
                    export(&root, "src/main.ts", "run", "function"),
                ],
                Vec::new(),
                BTreeSet::from([root.join("src/main.ts")]),
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn type_like_export_without_call_or_type_ref_is_precise_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/types.ts", "export interface Widget { id: string }\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/types.ts", "Widget", "interface")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "Widget");
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["uncertain_items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn rust_attribute_entry_points_seed_command_liveness() {
        let (_temp_dir, root, paths) = fixture_project(&[
            (
                "src/commands.rs",
                r#"use crate::db;

#[tauri::command]
pub fn get_primers() -> String {
    db::helper()
}

pub fn planted_dead() -> String {
    "dead".to_string()
}

#[tauri::command]
fn private_command() -> String {
    db::private_helper()
}
"#,
            ),
            (
                "src/imported.rs",
                r#"use crate::db;
use tauri::command;

#[command]
pub fn imported_command() -> String {
    db::imported_helper()
}
"#,
            ),
            (
                "src/unimported.rs",
                r#"use crate::db;

#[command]
pub fn false_command() -> String {
    db::false_helper()
}
"#,
            ),
            (
                "src/db.rs",
                r#"pub fn helper() -> String { "live".to_string() }
pub fn imported_helper() -> String { "live".to_string() }
pub fn private_helper() -> String { "live".to_string() }
pub fn false_helper() -> String { "dead".to_string() }
"#,
            ),
        ]);
        let helper_target = format!("{}::helper", root.join("src/db.rs").display());
        let imported_helper_target =
            format!("{}::imported_helper", root.join("src/db.rs").display());
        let private_helper_target = format!("{}::private_helper", root.join("src/db.rs").display());
        let false_helper_target = format!("{}::false_helper", root.join("src/db.rs").display());
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![
                    export(&root, "src/commands.rs", "get_primers", "function"),
                    export(&root, "src/commands.rs", "planted_dead", "function"),
                    export(&root, "src/imported.rs", "imported_command", "function"),
                    export(&root, "src/unimported.rs", "false_command", "function"),
                    export(&root, "src/db.rs", "helper", "function"),
                    export(&root, "src/db.rs", "imported_helper", "function"),
                    export(&root, "src/db.rs", "private_helper", "function"),
                    export(&root, "src/db.rs", "false_helper", "function"),
                ],
                vec![
                    outbound(&root, "src/commands.rs", "get_primers", &helper_target),
                    outbound(
                        &root,
                        "src/imported.rs",
                        "imported_command",
                        &imported_helper_target,
                    ),
                    outbound(
                        &root,
                        "src/commands.rs",
                        "private_command",
                        &private_helper_target,
                    ),
                    outbound(
                        &root,
                        "src/unimported.rs",
                        "false_command",
                        &false_helper_target,
                    ),
                ],
            ),
        ));

        assert!(!aggregate_has_item(
            &aggregate,
            "src/commands.rs",
            "get_primers"
        ));
        assert!(!aggregate_has_item(&aggregate, "src/db.rs", "helper"));
        assert!(!aggregate_has_item(
            &aggregate,
            "src/imported.rs",
            "imported_command"
        ));
        assert!(!aggregate_has_item(
            &aggregate,
            "src/db.rs",
            "imported_helper"
        ));
        assert!(!aggregate_has_item(
            &aggregate,
            "src/db.rs",
            "private_helper"
        ));
        assert!(aggregate_has_item(
            &aggregate,
            "src/commands.rs",
            "planted_dead"
        ));
        assert!(aggregate_has_item(
            &aggregate,
            "src/unimported.rs",
            "false_command"
        ));
        assert!(aggregate_has_item(&aggregate, "src/db.rs", "false_helper"));
    }

    #[test]
    fn rust_macro_token_liveness_rescues_bare_join_calls() {
        let aggregate = rust_entry_scan(
            &[(
                "src/main.rs",
                "fn main() { tokio::join!(fetch_a(), fetch_b()); }\nfn fetch_a() {}\nfn fetch_b() {}\nfn dead() {}\n",
            )],
            &[
                ("src/main.rs", "main", "function"),
                ("src/main.rs", "fetch_a", "function"),
                ("src/main.rs", "fetch_b", "function"),
                ("src/main.rs", "dead", "function"),
            ],
        );

        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "fetch_a"));
        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "fetch_b"));
        assert!(aggregate_has_item(&aggregate, "src/main.rs", "dead"));
    }

    #[test]
    fn rust_macro_token_liveness_rescues_upper_camel_component_and_nested_call() {
        let aggregate = rust_entry_scan(
            &[(
                "src/main.rs",
                "fn main() { element! { Header { title() } } }\nstruct Header;\nfn title() {}\nfn dead() {}\n",
            )],
            &[
                ("src/main.rs", "main", "function"),
                ("src/main.rs", "Header", "struct"),
                ("src/main.rs", "title", "function"),
                ("src/main.rs", "dead", "function"),
            ],
        );

        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "Header"));
        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "title"));
        assert!(aggregate_has_item(&aggregate, "src/main.rs", "dead"));
    }

    #[test]
    fn rust_macro_token_liveness_ignores_json_string_keys_but_keeps_values() {
        let aggregate = rust_entry_scan(
            &[(
                "src/main.rs",
                "fn main() { json!({\"dead_key\": compute_x()}); }\nfn compute_x() {}\nfn dead_key() {}\n",
            )],
            &[
                ("src/main.rs", "main", "function"),
                ("src/main.rs", "compute_x", "function"),
                ("src/main.rs", "dead_key", "function"),
            ],
        );

        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "compute_x"));
        assert!(aggregate_has_item(&aggregate, "src/main.rs", "dead_key"));
    }

    #[test]
    fn rust_macro_token_liveness_resolves_path_qualified_calls() {
        let aggregate = rust_entry_scan(
            &[
                (
                    "src/main.rs",
                    "mod m;\nfn main() { wrapper!(m::helper()); }\n",
                ),
                ("src/m.rs", "pub fn helper() {}\npub fn dead() {}\n"),
            ],
            &[
                ("src/main.rs", "main", "function"),
                ("src/m.rs", "helper", "function"),
                ("src/m.rs", "dead", "function"),
            ],
        );

        assert!(!aggregate_has_item(&aggregate, "src/m.rs", "helper"));
        assert!(aggregate_has_item(&aggregate, "src/m.rs", "dead"));
    }

    #[test]
    fn rust_macro_token_liveness_rescues_turbofish_calls() {
        let aggregate = rust_entry_scan(
            &[(
                "src/main.rs",
                "fn main() { wrapper!(parse::<T>()); }\nstruct T;\nfn parse<T>() {}\nfn dead() {}\n",
            )],
            &[
                ("src/main.rs", "main", "function"),
                ("src/main.rs", "T", "struct"),
                ("src/main.rs", "parse", "function"),
                ("src/main.rs", "dead", "function"),
            ],
        );

        assert!(!aggregate_has_item(&aggregate, "src/main.rs", "parse"));
        assert!(aggregate_has_item(&aggregate, "src/main.rs", "dead"));
    }

    #[test]
    fn rust_macro_token_liveness_does_not_rescue_receiver_methods_or_bare_idents() {
        let aggregate = rust_entry_scan(
            &[
                (
                    "src/main.rs",
                    "mod other;\nfn main() { wrapper!(socket.recv(), recv); }\n",
                ),
                ("src/other.rs", "pub fn recv() {}\n"),
            ],
            &[
                ("src/main.rs", "main", "function"),
                ("src/other.rs", "recv", "function"),
            ],
        );

        assert!(aggregate_has_item(&aggregate, "src/other.rs", "recv"));
    }

    #[test]
    fn rust_macro_token_liveness_inside_dead_caller_does_not_rescue_target() {
        let aggregate = rust_entry_scan(
            &[(
                "src/main.rs",
                "fn main() {}\nfn unreachable() { wrapper!(target()); }\nfn target() {}\n",
            )],
            &[
                ("src/main.rs", "main", "function"),
                ("src/main.rs", "unreachable", "function"),
                ("src/main.rs", "target", "function"),
            ],
        );

        assert!(aggregate_has_item(&aggregate, "src/main.rs", "unreachable"));
        assert!(aggregate_has_item(&aggregate, "src/main.rs", "target"));
    }

    #[test]
    fn genuinely_unreachable_function_is_still_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/build.ts", "export function build() {}\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/build.ts", "build", "function")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "build");
        assert_eq!(aggregate["uncertain_count"], 0);
    }
}
