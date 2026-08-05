use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
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
    language_name, CALLGRAPH_PROVENANCE_REEXPORT, DISPATCHED_CALLEE_SEPARATOR,
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
pub(crate) const DEAD_CODE_FACTS_FORMAT_VERSION: u32 = 3;
const MACRO_TOKEN_LIVENESS_PROVENANCE: &str = "macro_token_liveness";
const RUST_MACRO_REF_SHAPE_CALL: &str = "call";
const RUST_MACRO_REF_SHAPE_METHOD: &str = "method";
const RUST_MACRO_REF_SHAPE_STRUCT: &str = "struct";
const TOP_LEVEL_SYMBOL: &str = "<top-level>";

type ExportNode = (String, String);
type OutboundCallsByCallerFile<'a> = BTreeMap<PathBuf, Vec<&'a CallgraphOutboundCall>>;
type MethodNamesByLanguage = BTreeMap<String, BTreeSet<String>>;

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
    type_ref_names: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct RustMacroToken<'a> {
    text: &'a str,
    kind: &'a str,
    line: u32,
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

        FileAnalysis {
            raw_imports,
            rust_imports,
            raw_reexports,
            attribute_entry_points,
            macro_token_refs,
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

    let contributions = job
        .scope_files
        .par_iter()
        .map_init(DeadCodeFileAnalyzer::default, |file_analyzer, file| {
            gather_file_contribution(
                job,
                file,
                &fallback_exports_by_file,
                &oxc_facts_by_file,
                &oxc_parse_errors_by_file,
                &oxc_skipped_files,
                file_analyzer,
            )
        })
        .collect::<Vec<_>>();

    let public_api_files = collect_public_api_files(&job.project_root);
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
    json!({
        "count": 0,
        "items": [],
        "by_language": {},
        "languages_skipped": [],
        "drill_down_capped": false,
        "uncertain_count": 0,
        "uncertain_items": [],
        "callgraph_available": false,
        "scanned_files": scanned_files,
        "notes": ["callgraph_unavailable"],
    })
}

pub(crate) fn aggregate_dead_code_contributions_with_snapshot(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let parsed = parse_dead_code_contributions(contributions);
    let materialized =
        materialize_dead_code_contributions(project_root, snapshot, parsed, public_api_files);
    aggregate_materialized_dead_code_contributions(
        project_root,
        &materialized,
        public_api_files,
        roles,
        drill_down_limit,
        contributions.len(),
    )
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
    let oxc_by_file = oxc_verdicts_by_file(project_root, snapshot, &parsed, public_api_files);

    // CI probe instrumentation (probe branch only): dump the verdict inputs
    // for the Cargo-bin liveness decision.
    eprintln!("PROBE2 verdict project_root = {}", project_root.display());
    for f in &liveness_root_files {
        eprintln!("PROBE2 liveness_root_rel = {f}");
    }
    for (f, exports) in &executable_root_exports_by_file {
        eprintln!("PROBE2 exec_root_rel = {f} exports = {exports:?}");
    }
    for (key, calls) in &outbound_calls_by_caller_file {
        for call in calls.iter().take(4) {
            eprintln!(
                "PROBE2 outbound key = {} | caller_file = {} | callee = {}",
                key.display(),
                call.caller_file.display(),
                call.callee_name
            );
        }
    }

    parsed
        .into_iter()
        .map(|mut contribution| {
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
    });
    internal_calls.dedup_by(|left, right| {
        left.caller_symbol == right.caller_symbol
            && left.file == right.file
            && left.symbol == right.symbol
            && left.line == right.line
            && left.provenance == right.provenance
    });
}

fn aggregate_materialized_dead_code_contributions(
    project_root: &Path,
    parsed: &[DeadCodeContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    drill_down_limit: Option<usize>,
    scanned_files: usize,
) -> serde_json::Value {
    let edges_by_source = edges_by_source(parsed);
    let dispatched_method_names = collect_dispatched_method_names_by_language(parsed);
    let reachable = reachable_exports(parsed, &edges_by_source, &dispatched_method_names);
    let referenced_type_names = collect_referenced_type_names(parsed);

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
) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
    let mut edges: BTreeMap<ExportNode, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        for call in &contribution.internal_calls {
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

fn reachable_exports(
    contributions: &[DeadCodeContribution],
    edges_by_source: &BTreeMap<ExportNode, BTreeSet<ExportNode>>,
    dispatched_method_names: &MethodNamesByLanguage,
) -> BTreeSet<ExportNode> {
    let imported_exports_by_file = imported_exports_by_file(contributions);
    let namespace_imports_by_file = namespace_imported_exports_by_file(contributions);
    let dispatch_live_source_names_by_file =
        dispatch_live_source_names_by_file(contributions, dispatched_method_names);
    let mut expanded_file_imports = BTreeSet::new();
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();

    for contribution in contributions {
        for root in &contribution.liveness_roots {
            queue.push_back((contribution.file.clone(), root.clone()));
        }
        for export in &contribution.exports {
            if export.is_entry_point {
                queue.push_back((contribution.file.clone(), export.symbol.clone()));
            }
        }
    }

    // Methods reached only via receiver or interface dispatch often have no
    // precise call edge because the concrete receiver type is unknown. They are
    // rescued from the dead list by method name, but that alone would not let
    // liveness flow through the method body. Go uses the method-only gate below;
    // other languages keep their existing name-based behavior.
    for source in edges_by_source.keys() {
        if dispatch_live_source_names_by_file
            .get(&source.0)
            .is_some_and(|method_names| method_names.contains(symbol_liveness_name(&source.1)))
        {
            queue.push_back(source.clone());
        }
    }

    while let Some(node) = queue.pop_front() {
        if !reachable.insert(node.clone()) {
            continue;
        }
        if expanded_file_imports.insert(node.0.clone()) {
            // Static imports are file-level liveness edges: an imported export
            // should keep the target live only when the importer file itself is
            // reachable. This prevents dead consumers from making their imports
            // look live while still covering references the call graph cannot
            // see (type-only imports, JSX/value usage, barrel consumers, etc.).
            if let Some(targets) = imported_exports_by_file.get(&node.0) {
                for target in targets {
                    if !reachable.contains(target) {
                        queue.push_back(target.clone());
                    }
                }
            }

            // Namespace imports remain conservative file-level edges: once the
            // importer file is reached, every export of the imported module is
            // considered live because member access is not tracked here.
            if let Some(targets) = namespace_imports_by_file.get(&node.0) {
                for target in targets {
                    if !reachable.contains(target) {
                        queue.push_back(target.clone());
                    }
                }
            }
        }
        if let Some(targets) = edges_by_source.get(&node) {
            for target in targets {
                if !reachable.contains(target) {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    reachable
}

fn dispatch_live_source_names_by_file(
    contributions: &[DeadCodeContribution],
    dispatched_method_names: &MethodNamesByLanguage,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for contribution in contributions {
        let language = language_for_file(&contribution.file);
        let Some(language_method_names) = dispatched_method_names.get(language) else {
            continue;
        };
        if language != "go" {
            by_file
                .entry(contribution.file.clone())
                .or_default()
                .extend(language_method_names.iter().cloned());
            continue;
        }

        for export in &contribution.exports {
            if export_is_method(export)
                && language_method_names.contains(symbol_liveness_name(&export.symbol))
            {
                by_file
                    .entry(contribution.file.clone())
                    .or_default()
                    .insert(symbol_liveness_name(&export.symbol).to_string());
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

    let canonical_root = fs::canonicalize(project_root).ok()?;
    let canonical_file = fs::canonicalize(file).ok()?;
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

    let canonical_root = fs::canonicalize(project_root).ok()?;
    let canonical_file = fs::canonicalize(file).ok()?;
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
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
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
}

impl From<InternalCall> for InternalCallContribution {
    fn from(call: InternalCall) -> Self {
        Self {
            caller_symbol: call.caller_symbol,
            file: call.file,
            symbol: call.symbol,
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
