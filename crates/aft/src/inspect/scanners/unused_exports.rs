use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};
use tree_sitter::{Node, Tree};

use crate::cache_freshness;
use crate::imports::{parse_file_imports, specifier_imported_name, ImportBlock, ImportStatement};
use crate::inspect::job::{is_test_file, is_test_support_file};
use crate::inspect::oxc_engine::{
    ExportFact, FileFacts, LivenessVerdict, OxcEngineError, OxcEngineResult, FACTS_FORMAT_VERSION,
    OXC_PROVENANCE,
};
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, LangId};

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
const DRILL_DOWN_LIMIT: usize = 100;

#[derive(Debug, Clone)]
struct ExportSymbol {
    symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct ImportEdge {
    from_module: String,
    resolved_file: Option<PathBuf>,
    named: Vec<String>,
}

#[derive(Debug)]
struct FileScan {
    file_path: PathBuf,
    relative_file: String,
    contribution: FileContribution,
    exports: Vec<ExportSymbol>,
    imports: Vec<ImportEdge>,
    skipped_language: Option<&'static str>,
    generated: bool,
}

#[derive(Debug, Serialize)]
struct OxcUnusedExportsFactsPayload<'a> {
    format_version: u32,
    content_hash: &'a str,
    exports: &'a [ExportFact],
    imports: &'a [crate::inspect::oxc_engine::ImportFact],
    re_exports: &'a [crate::inspect::oxc_engine::ReExportFact],
    dynamic_imports: &'a [crate::inspect::oxc_engine::DynamicImportFact],
    same_file_value_references: &'a std::collections::BTreeSet<String>,
    used_import_bindings: &'a std::collections::BTreeSet<String>,
    type_referenced_import_bindings: &'a std::collections::BTreeSet<String>,
    value_referenced_import_bindings: &'a std::collections::BTreeSet<String>,
    parse_error: &'a Option<String>,
}

pub fn run_unused_exports_scan(job: &InspectJob) -> InspectResult {
    run_unused_exports_legacy_scan(job, Instant::now())
}

pub(crate) fn run_unused_exports_scan_with_oxc(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
) -> InspectResult {
    run_unused_exports_scan_with_oxc_started(job, oxc_result, Instant::now())
}

fn run_unused_exports_scan_with_oxc_started(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    started: Instant,
) -> InspectResult {
    if let Some(oxc_result) = oxc_result {
        return run_unused_exports_oxc_scan(job, oxc_result, started);
    }
    run_unused_exports_legacy_scan(job, started)
}

fn run_unused_exports_legacy_scan(job: &InspectJob, started: Instant) -> InspectResult {
    let ctx = job.worker_ctx();
    let project_root = normalize_path(&ctx.project_root);
    let public_api_entries = crate::inspect::entry_points::resolve_entry_points(&project_root);
    let package_warnings = public_api_entries.warnings().to_vec();

    let per_file = job
        .scope_files
        .par_iter()
        .filter_map(|path| scan_file(path, &project_root))
        .map(|scan| suppress_public_api_exports(scan, &project_root, &public_api_entries))
        .collect::<Vec<_>>();

    let mut imported_by: BTreeMap<(PathBuf, String), BTreeSet<String>> = BTreeMap::new();
    let mut uncertain_by: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for scan in &per_file {
        for import in &scan.imports {
            let Some(resolved_file) = &import.resolved_file else {
                continue;
            };
            for name in &import.named {
                if name == "*" {
                    uncertain_by
                        .entry(resolved_file.clone())
                        .or_default()
                        .insert(scan.relative_file.clone());
                } else {
                    imported_by
                        .entry((resolved_file.clone(), name.clone()))
                        .or_default()
                        .insert(scan.relative_file.clone());
                }
            }
        }
    }

    let mut count = 0usize;
    let mut headline_items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let test_only_count = 0usize;
    let test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();
    for scan in &per_file {
        if public_api_entries.is_public_api_file(&scan.file_path) {
            continue;
        }
        // Fixtures/corpora/mock data are loaded by path, not imported, so their
        // exports always look unused. Skip reporting (their import edges above
        // still mark the product code they consume as used).
        if is_test_support_file(&scan.relative_file) {
            continue;
        }

        for export in &scan.exports {
            let imported = imported_by
                .get(&(scan.file_path.clone(), export.symbol.clone()))
                .map(|files| !files.is_empty())
                .unwrap_or(false);
            let uncertain = uncertain_by
                .get(&scan.file_path)
                .map(|files| !files.is_empty())
                .unwrap_or(false);

            if imported {
                continue;
            }
            if uncertain {
                uncertain_count += 1;
                if uncertain_items.len() < DRILL_DOWN_LIMIT {
                    uncertain_items.push(json!({
                        "file": scan.relative_file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "reason": "wildcard_import",
                    }));
                }
                continue;
            }

            let mut item = json!({
                "file": scan.relative_file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            });
            if scan.generated {
                item["generated"] = json!(true);
                generated_count += 1;
                generated_items.push(item);
            } else {
                count += 1;
                headline_items.push(item);
            }
        }
    }

    let roles = crate::inspect::entry_points::resolve_project_roles(&project_root);
    let headline_items = crate::inspect::entry_points::rank_and_truncate_items(
        headline_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let generated_items = crate::inspect::entry_points::rank_and_truncate_items(
        generated_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let top = crate::inspect::entry_points::top_preview_symbols(&headline_items);
    let mut items = headline_items;
    items.extend(generated_items.iter().cloned());
    items.truncate(DRILL_DOWN_LIMIT);
    let generated_top = generated_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let test_only_items = crate::inspect::entry_points::rank_and_truncate_items(
        test_only_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let test_only_top = test_only_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();

    let languages_skipped = per_file
        .iter()
        .filter_map(|scan| scan.skipped_language)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "drill_down_capped": count + generated_count > DRILL_DOWN_LIMIT,
        "generated_drill_down_capped": generated_count > DRILL_DOWN_LIMIT,
        "test_only_drill_down_capped": test_only_count > DRILL_DOWN_LIMIT,
        "scanned_files": per_file.len(),
        "languages_skipped": languages_skipped,
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
    });
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }

    let success = InspectScanSuccess {
        scanned_files: per_file.iter().map(|scan| scan.file_path.clone()).collect(),
        contributions: per_file.into_iter().map(|scan| scan.contribution).collect(),
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn run_unused_exports_oxc_scan(
    job: &InspectJob,
    oxc_result: &OxcEngineResult,
    started: Instant,
) -> InspectResult {
    let project_root = normalize_path(&job.project_root);
    let public_api_entries = crate::inspect::entry_points::resolve_entry_points(&project_root);
    let package_warnings = public_api_entries.warnings().to_vec();
    let roles = crate::inspect::entry_points::resolve_project_roles(&project_root);

    let oxc_paths = oxc_result
        .files
        .iter()
        .map(|file| normalize_path(&file.file))
        .collect::<BTreeSet<_>>();
    let parse_errors_by_file = oxc_result.errors.iter().fold(
        BTreeMap::<PathBuf, Vec<String>>::new(),
        |mut errors, error| {
            errors
                .entry(normalize_path(&error.file))
                .or_default()
                .push(error.message.clone());
            errors
        },
    );
    let skipped_files_payload = oxc_skipped_files_payload(&project_root, oxc_result);
    let facts_by_file = oxc_result
        .facts
        .iter()
        .map(|facts| (normalize_path(&facts.path), facts))
        .collect::<BTreeMap<_, _>>();
    let mut contributions = Vec::new();
    let mut count = 0usize;
    let mut headline_items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let mut test_only_count = 0usize;
    let mut test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items = Vec::new();

    for file in &oxc_result.files {
        if let Some(facts) = facts_by_file.get(&normalize_path(&file.file)) {
            if let Some(contribution) = oxc_unused_exports_contribution(
                &project_root,
                facts,
                parse_errors_by_file.get(&normalize_path(&file.file)),
                &skipped_files_payload,
            ) {
                contributions.push(contribution);
            }
        }

        if public_api_entries.is_public_api_file(&file.file)
            || is_test_support_file(&file.relative_file)
        {
            continue;
        }
        let generated_file =
            crate::inspect::generated::is_generated_file(&project_root, &file.file);

        for export in &file.exports {
            match export.verdict {
                LivenessVerdict::Used => {
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
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
                }
                LivenessVerdict::Uncertain => {
                    uncertain_count += 1;
                    if uncertain_items.len() < DRILL_DOWN_LIMIT {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "reason": export.reason,
                            "provenance": export.provenance,
                        });
                        add_reexport_contexts(&mut item, &export.also_reexported);
                        uncertain_items.push(item);
                    }
                }
                LivenessVerdict::Unused => {
                    if !is_test_file(&file.relative_file)
                        && !export.test_only_reference_files.is_empty()
                    {
                        let mut item = json!({
                            "file": file.relative_file,
                            "symbol": export.symbol,
                            "kind": export.kind,
                            "line": export.line,
                            "provenance": export.provenance,
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
                    let mut item = json!({
                        "file": file.relative_file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "provenance": export.provenance,
                    });
                    add_reexport_contexts(&mut item, &export.also_reexported);
                    if generated_file {
                        item["generated"] = json!(true);
                        generated_count += 1;
                        generated_items.push(item);
                    } else {
                        count += 1;
                        headline_items.push(item);
                    }
                }
            }
        }
    }

    for error in &oxc_result.errors {
        if facts_by_file.contains_key(&normalize_path(&error.file)) {
            continue;
        }
        if let Some(contribution) = oxc_read_error_contribution(&project_root, error) {
            contributions.push(contribution);
        }
    }

    let non_js_scans = job
        .scope_files
        .iter()
        .filter(|path| !oxc_paths.contains(&normalize_path(path)))
        .filter_map(|path| scan_non_js_empty_file(path, &project_root))
        .collect::<Vec<_>>();
    let languages_skipped = non_js_scans
        .iter()
        .filter_map(|scan| scan.skipped_language)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    contributions.extend(non_js_scans.into_iter().map(|scan| scan.contribution));

    let headline_items = crate::inspect::entry_points::rank_and_truncate_items(
        headline_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let generated_items = crate::inspect::entry_points::rank_and_truncate_items(
        generated_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let top = crate::inspect::entry_points::top_preview_symbols(&headline_items);
    let mut items = headline_items;
    items.extend(generated_items.iter().cloned());
    items.truncate(DRILL_DOWN_LIMIT);
    let generated_top = generated_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let test_only_items = crate::inspect::entry_points::rank_and_truncate_items(
        test_only_items,
        &roles,
        Some(DRILL_DOWN_LIMIT),
    );
    let test_only_top = test_only_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let mut aggregate = json!({
        "count": count,
        "generated_count": generated_count,
        "total_count": count + test_only_count + generated_count,
        "items": items,
        "top": top,
        "generated_items": generated_items,
        "generated_top": generated_top,
        "test_only_count": test_only_count,
        "test_only_items": test_only_items,
        "test_only_top": test_only_top,
        "drill_down_capped": count + generated_count > DRILL_DOWN_LIMIT,
        "generated_drill_down_capped": generated_count > DRILL_DOWN_LIMIT,
        "test_only_drill_down_capped": test_only_count > DRILL_DOWN_LIMIT,
        "scanned_files": contributions.len(),
        "languages_skipped": languages_skipped,
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "complete": oxc_result.errors.is_empty() && oxc_result.skipped_outside_root.is_empty(),
    });
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    add_oxc_honesty_fields(&mut aggregate, &project_root, oxc_result);

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

fn add_reexport_contexts(
    item: &mut Value,
    contexts: &[crate::inspect::oxc_engine::OxcReExportContext],
) {
    if !contexts.is_empty() {
        item["also_reexported"] = json!(contexts);
    }
}

fn oxc_unused_exports_contribution(
    project_root: &Path,
    facts: &FileFacts,
    parse_errors: Option<&Vec<String>>,
    skipped_files: &[Value],
) -> Option<FileContribution> {
    let freshness = cache_freshness::collect(&facts.path).ok()?;
    let exports = facts
        .exports
        .iter()
        .map(|export| {
            json!({
                "symbol": export.name.as_symbol(),
                "kind": export.kind,
                "line": export.line,
            })
        })
        .collect::<Vec<_>>();
    let facts_payload = OxcUnusedExportsFactsPayload {
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
    let generated = crate::inspect::generated::is_generated_file(project_root, &facts.path);
    let mut contribution = json!({
        "file": relative_string(project_root, &facts.path),
        "generated": generated,
        "exports": exports,
        "imports": [],
        "provenance": OXC_PROVENANCE,
        "oxc_facts": facts_payload,
    });
    if let Value::Object(object) = &mut contribution {
        if let Some(parse_errors) = parse_errors {
            object.insert(
                "parse_errors".to_string(),
                Value::Array(
                    parse_errors
                        .iter()
                        .map(|message| {
                            json!({
                                "file": relative_string(project_root, &facts.path),
                                "message": message,
                            })
                        })
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
        InspectCategory::UnusedExports,
        facts.path.clone(),
        freshness,
        contribution_with_relative_file(project_root, contribution, &facts.path),
    ))
}

fn oxc_read_error_contribution(
    project_root: &Path,
    error: &OxcEngineError,
) -> Option<FileContribution> {
    let error_file_path = absolute_path(project_root, &error.file);
    let relative_file = relative_string(project_root, &error_file_path);
    let file_path = if Path::new(&relative_file).is_absolute() {
        error_file_path
    } else {
        normalize_path(&project_root.join(&relative_file))
    };
    let freshness = freshness_for_oxc_error_file(&file_path)?;
    let generated = crate::inspect::generated::is_generated_file(project_root, &file_path);
    let contribution = json!({
        "file": relative_file,
        "generated": generated,
        "exports": [],
        "imports": [],
        "parse_errors": [{
            "file": relative_file,
            "message": error.message,
        }],
    });
    Some(FileContribution::new(
        InspectCategory::UnusedExports,
        file_path,
        freshness,
        contribution,
    ))
}

fn freshness_for_oxc_error_file(path: &Path) -> Option<cache_freshness::FileFreshness> {
    let metadata = fs::metadata(path).ok()?;
    Some(cache_freshness::FileFreshness {
        mtime: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        size: metadata.len(),
        content_hash: cache_freshness::zero_hash(),
    })
}

fn contribution_with_relative_file(
    project_root: &Path,
    mut contribution: Value,
    file_path: &Path,
) -> Value {
    if let Value::Object(object) = &mut contribution {
        object.insert(
            "file".to_string(),
            Value::String(relative_string(project_root, file_path)),
        );
    }
    contribution
}

fn scan_non_js_empty_file(path: &Path, project_root: &Path) -> Option<FileScan> {
    let file_path = absolute_path(project_root, path);
    if !file_path.is_file() {
        return None;
    }
    if is_js_ts_path(&file_path) {
        return None;
    }
    let relative_file = relative_string(project_root, &file_path);
    let freshness = cache_freshness::collect(&file_path).ok()?;
    let skipped_language = detect_language(&file_path).map(language_name);
    Some(empty_file_scan(
        file_path,
        relative_file,
        freshness,
        skipped_language,
    ))
}

fn oxc_skipped_files_payload(project_root: &Path, oxc_result: &OxcEngineResult) -> Vec<Value> {
    oxc_result
        .skipped_outside_root
        .iter()
        .map(|path| {
            json!({
                "file": relative_string(project_root, path),
                "reason": "outside_project_root",
            })
        })
        .collect::<Vec<_>>()
}

fn add_oxc_honesty_fields(
    aggregate: &mut Value,
    project_root: &Path,
    oxc_result: &OxcEngineResult,
) {
    let skipped_files = oxc_skipped_files_payload(project_root, oxc_result);
    let parse_errors = oxc_result
        .errors
        .iter()
        .map(|error| {
            json!({
                "file": relative_string(project_root, &error.file),
                "message": error.message,
            })
        })
        .collect::<Vec<_>>();
    if !skipped_files.is_empty() {
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    if !parse_errors.is_empty() {
        aggregate["parse_errors"] = Value::Array(parse_errors);
    }
}

fn suppress_public_api_exports(
    mut scan: FileScan,
    project_root: &Path,
    public_api_entries: &crate::inspect::entry_points::EntryPointSet,
) -> FileScan {
    if public_api_entries.is_public_api_file(&scan.file_path) && !scan.exports.is_empty() {
        scan.exports.clear();
        scan.contribution.contribution = contribution_value(
            project_root,
            &scan.relative_file,
            scan.generated,
            &scan.exports,
            &scan.imports,
        );
    }
    scan
}

fn scan_file(path: &Path, project_root: &Path) -> Option<FileScan> {
    let file_path = absolute_path(project_root, path);
    if !file_path.is_file() {
        return None;
    }

    let relative_file = relative_string(project_root, &file_path);
    let freshness = cache_freshness::collect(&file_path).ok()?;
    let Some(lang) = detect_language(&file_path) else {
        return Some(empty_file_scan(file_path, relative_file, freshness, None));
    };

    if !is_js_ts(lang) {
        return Some(empty_file_scan(
            file_path,
            relative_file,
            freshness,
            Some(language_name(lang)),
        ));
    }

    let Ok((source, tree, import_block)) = parse_file_imports(&file_path, lang) else {
        return Some(empty_file_scan(file_path, relative_file, freshness, None));
    };

    let generated = crate::inspect::generated::is_generated_file_from_source(&file_path, &source);
    let exports = extract_exports(&source, &tree);
    let namespace_members = namespace_member_accesses(&source, &tree, &import_block);
    let mut imports =
        import_edges_from_block(&import_block, &file_path, project_root, &namespace_members);
    imports.extend(reexport_edges(&source, &tree, &file_path, project_root));

    let contribution =
        contribution_value(project_root, &relative_file, generated, &exports, &imports);
    Some(FileScan {
        contribution: FileContribution::new(
            InspectCategory::UnusedExports,
            file_path.clone(),
            freshness,
            contribution,
        ),
        file_path,
        relative_file,
        exports,
        imports,
        skipped_language: None,
        generated,
    })
}

fn empty_file_scan(
    file_path: PathBuf,
    relative_file: String,
    freshness: cache_freshness::FileFreshness,
    skipped_language: Option<&'static str>,
) -> FileScan {
    let generated = crate::inspect::generated::is_generated_file(Path::new(""), &file_path);
    let contribution = json!({
        "file": relative_file,
        "generated": generated,
        "exports": [],
        "imports": [],
    });
    FileScan {
        contribution: FileContribution::new(
            InspectCategory::UnusedExports,
            file_path.clone(),
            freshness,
            contribution,
        ),
        file_path,
        relative_file,
        exports: Vec::new(),
        imports: Vec::new(),
        skipped_language,
        generated,
    }
}

fn contribution_value(
    project_root: &Path,
    relative_file: &str,
    generated: bool,
    exports: &[ExportSymbol],
    imports: &[ImportEdge],
) -> Value {
    let exports_json = exports
        .iter()
        .map(|export| {
            json!({
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            })
        })
        .collect::<Vec<_>>();
    let imports_json = imports
        .iter()
        .map(|import| {
            json!({
                "from_module": import.from_module,
                "resolved_file": import
                    .resolved_file
                    .as_ref()
                    .map(|path| relative_string(project_root, path)),
                "named": import.named,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "file": relative_file,
        "generated": generated,
        "exports": exports_json,
        "imports": imports_json,
    })
}

fn import_edges_from_block(
    import_block: &ImportBlock,
    importer_file: &Path,
    project_root: &Path,
    namespace_members: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<ImportEdge> {
    import_block
        .imports
        .iter()
        .map(|import| {
            import_edge_from_statement(import, importer_file, project_root, namespace_members)
        })
        .collect()
}

fn import_edge_from_statement(
    import: &ImportStatement,
    importer_file: &Path,
    project_root: &Path,
    namespace_members: &BTreeMap<String, BTreeSet<String>>,
) -> ImportEdge {
    let mut named = Vec::new();
    if import.default_import.is_some() {
        named.push("default".to_string());
    }
    if let Some(alias) = import.namespace_import.as_deref() {
        if let Some(members) = namespace_members.get(alias) {
            named.extend(members.iter().cloned());
        }
    }
    named.extend(
        import
            .names
            .iter()
            .map(|name| specifier_imported_name(name).to_string()),
    );
    named.sort();
    named.dedup();

    ImportEdge {
        from_module: import.module_path.clone(),
        resolved_file: resolve_module_path(&import.module_path, importer_file, project_root),
        named,
    }
}

fn namespace_member_accesses(
    source: &str,
    tree: &Tree,
    import_block: &ImportBlock,
) -> BTreeMap<String, BTreeSet<String>> {
    let aliases = import_block
        .imports
        .iter()
        .filter_map(|import| import.namespace_import.clone())
        .collect::<BTreeSet<_>>();
    if aliases.is_empty() {
        return BTreeMap::new();
    }

    let mut accesses = BTreeMap::new();
    collect_namespace_member_accesses(source, tree.root_node(), &aliases, &mut accesses);
    accesses
}

fn collect_namespace_member_accesses(
    source: &str,
    node: Node,
    aliases: &BTreeSet<String>,
    accesses: &mut BTreeMap<String, BTreeSet<String>>,
) {
    match node.kind() {
        "member_expression" => {
            if let Some(object) = node.child_by_field_name("object") {
                let alias = node_text(source, &object).trim();
                if aliases.contains(alias) {
                    let member = node
                        .child_by_field_name("property")
                        .and_then(|property| static_member_name(source, &property))
                        .unwrap_or_else(|| "*".to_string());
                    accesses
                        .entry(alias.to_string())
                        .or_default()
                        .insert(member);
                }
            }
        }
        "subscript_expression" => {
            if let Some(object) = node.child_by_field_name("object") {
                let alias = node_text(source, &object).trim();
                if aliases.contains(alias) {
                    let member = node
                        .child_by_field_name("index")
                        .and_then(|index| static_member_name(source, &index))
                        .unwrap_or_else(|| "*".to_string());
                    accesses
                        .entry(alias.to_string())
                        .or_default()
                        .insert(member);
                }
            }
        }
        "identifier" | "shorthand_property_identifier" => {
            let alias = node_text(source, &node).trim();
            if aliases.contains(alias) && namespace_alias_used_as_value(&node) {
                accesses
                    .entry(alias.to_string())
                    .or_default()
                    .insert("*".to_string());
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_namespace_member_accesses(source, cursor.node(), aliases, accesses);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn namespace_alias_used_as_value(node: &Node) -> bool {
    if is_inside_import_statement(*node) {
        return false;
    }

    if let Some(parent) = node.parent() {
        if matches!(parent.kind(), "member_expression" | "subscript_expression") {
            if parent
                .child_by_field_name("object")
                .is_some_and(|object| same_node(&object, node))
            {
                return false;
            }
        }
    }

    true
}

fn is_inside_import_statement(mut node: Node) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "import_statement" {
            return true;
        }
        node = parent;
    }
    false
}

fn same_node(left: &Node, right: &Node) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn static_member_name(source: &str, node: &Node) -> Option<String> {
    let text = node_text(source, node).trim();
    if text.is_empty() {
        return None;
    }

    match node.kind() {
        "identifier" | "property_identifier" | "shorthand_property_identifier" => {
            Some(text.to_string())
        }
        "string" | "string_fragment" => {
            let unquoted = strip_quotes(text);
            (!unquoted.is_empty()).then(|| unquoted.to_string())
        }
        _ => None,
    }
}

fn extract_exports(source: &str, tree: &Tree) -> Vec<ExportSymbol> {
    let mut exports = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return exports;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            extract_export_statement(source, &node, &mut exports);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    dedupe_exports(exports)
}

fn extract_export_statement(source: &str, node: &Node, exports: &mut Vec<ExportSymbol>) {
    let text = node_text(source, node).trim_start();
    if is_default_export(text) {
        exports.push(ExportSymbol {
            symbol: "default".to_string(),
            kind: default_export_kind(node).to_string(),
            line: line_number(node),
        });
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                push_named_declaration(source, &child, "function", exports);
            }
            "class_declaration" | "abstract_class_declaration" => {
                push_named_declaration(source, &child, "class", exports);
            }
            "interface_declaration" => {
                push_named_declaration(source, &child, "interface", exports);
            }
            "type_alias_declaration" => {
                push_named_declaration(source, &child, "type", exports);
            }
            "enum_declaration" => {
                push_named_declaration(source, &child, "enum", exports);
            }
            "internal_module" => {
                push_named_declaration(source, &child, "namespace", exports);
            }
            "lexical_declaration" | "variable_declaration" => {
                collect_variable_exports(source, &child, exports);
            }
            "export_clause" => {
                let has_source = export_source_module(source, node).is_some();
                collect_export_clause_symbols(source, &child, has_source, exports);
            }
            _ => {}
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn push_named_declaration(
    source: &str,
    node: &Node,
    kind: &'static str,
    exports: &mut Vec<ExportSymbol>,
) {
    if let Some(name) = declaration_name(source, node) {
        exports.push(ExportSymbol {
            symbol: name,
            kind: kind.to_string(),
            line: line_number(node),
        });
    }
}

fn declaration_name(source: &str, node: &Node) -> Option<String> {
    node.child_by_field_name("name")
        .map(|name| node_text(source, &name).to_string())
}

fn collect_variable_exports(source: &str, node: &Node, exports: &mut Vec<ExportSymbol>) {
    if node.kind() == "variable_declarator" {
        if let Some(name) = node.child_by_field_name("name") {
            if matches!(
                name.kind(),
                "identifier" | "shorthand_property_identifier_pattern"
            ) {
                exports.push(ExportSymbol {
                    symbol: node_text(source, &name).to_string(),
                    kind: "variable".to_string(),
                    line: line_number(node),
                });
            }
        }
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_variable_exports(source, &cursor.node(), exports);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn collect_export_clause_symbols(
    source: &str,
    node: &Node,
    has_source: bool,
    exports: &mut Vec<ExportSymbol>,
) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "export_specifier" {
            if let Some(symbol) = exported_specifier_name(source, &child) {
                exports.push(ExportSymbol {
                    symbol,
                    kind: if has_source { "re_export" } else { "export" }.to_string(),
                    line: line_number(&child),
                });
            }
        } else {
            collect_export_clause_symbols(source, &child, has_source, exports);
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn reexport_edges(
    source: &str,
    tree: &Tree,
    importer_file: &Path,
    project_root: &Path,
) -> Vec<ImportEdge> {
    let mut edges = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    if !cursor.goto_first_child() {
        return edges;
    }

    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            if let Some(from_module) = export_source_module(source, &node) {
                let mut named = reexport_imported_names(source, &node);
                if named.is_empty() && node_text(source, &node).contains('*') {
                    named.push("*".to_string());
                }
                named.sort();
                named.dedup();
                edges.push(ImportEdge {
                    resolved_file: resolve_module_path(&from_module, importer_file, project_root),
                    from_module,
                    named,
                });
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    edges
}

fn reexport_imported_names(source: &str, node: &Node) -> Vec<String> {
    let mut names = Vec::new();
    collect_reexport_imported_names(source, node, &mut names);
    names
}

fn collect_reexport_imported_names(source: &str, node: &Node, names: &mut Vec<String>) {
    if node.kind() == "export_specifier" {
        if let Some(name) = imported_specifier_name(source, node) {
            names.push(name);
        }
        return;
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        collect_reexport_imported_names(source, &cursor.node(), names);
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn export_source_module(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        let child = cursor.node();
        if child.kind() == "string" {
            return Some(strip_quotes(node_text(source, &child)).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    None
}

fn exported_specifier_name(source: &str, node: &Node) -> Option<String> {
    specifier_name_after_as(node_text(source, node)).or_else(|| first_specifier_name(source, node))
}

fn imported_specifier_name(source: &str, node: &Node) -> Option<String> {
    first_specifier_name(source, node)
}

fn specifier_name_after_as(text: &str) -> Option<String> {
    let cleaned = clean_specifier_text(text);
    cleaned
        .split_once(" as ")
        .map(|(_, alias)| alias.trim().to_string())
        .filter(|alias| !alias.is_empty())
}

fn first_specifier_name(source: &str, node: &Node) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(clean_specifier_text(node_text(source, &name)));
    }

    clean_specifier_text(node_text(source, node))
        .split_whitespace()
        .next()
        .map(str::to_string)
}

fn clean_specifier_text(text: &str) -> String {
    text.trim()
        .trim_start_matches("type ")
        .trim()
        .trim_matches('{')
        .trim_matches('}')
        .trim()
        .to_string()
}

fn is_default_export(text: &str) -> bool {
    text.strip_prefix("export")
        .map(str::trim_start)
        .map(|after_export| after_export.starts_with("default"))
        .unwrap_or(false)
}

fn default_export_kind(node: &Node) -> &'static str {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return "default";
    }
    loop {
        let child = cursor.node();
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => return "function",
            "class_declaration" | "abstract_class_declaration" => return "class",
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    "default"
}

fn dedupe_exports(exports: Vec<ExportSymbol>) -> Vec<ExportSymbol> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for export in exports {
        if seen.insert((export.symbol.clone(), export.line)) {
            deduped.push(export);
        }
    }
    deduped
}

fn resolve_module_path(
    module_path: &str,
    importer_file: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    if module_path.starts_with("node:") {
        return None;
    }

    if is_relative_module(module_path) {
        let base = importer_file.parent()?.join(module_path);
        return resolve_local_module(&base, project_root);
    }

    resolve_node_package(module_path, project_root)
}

fn resolve_local_module(base: &Path, project_root: &Path) -> Option<PathBuf> {
    candidate_paths(base)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.starts_with(project_root) && candidate.is_file())
}

fn resolve_node_package(module_path: &str, project_root: &Path) -> Option<PathBuf> {
    let package_name = package_name(module_path)?;
    let package_dir = project_root.join("node_modules").join(package_name);
    let package_json = package_dir.join("package.json");
    let value = fs::read_to_string(&package_json)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())?;

    let mut entries = Vec::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        entries.push(main.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_package_export_strings(exports, &mut entries);
    }

    entries
        .iter()
        .filter_map(|entry| resolve_package_entry(&package_dir, entry))
        .next()
}

fn package_name(module_path: &str) -> Option<String> {
    let mut parts = module_path.split('/');
    let first = parts.next()?.to_string();
    if first.starts_with('@') {
        let second = parts.next()?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first)
    }
}

fn collect_package_export_strings(value: &Value, entries: &mut Vec<String>) {
    match value {
        Value::String(entry) => entries.push(entry.clone()),
        Value::Array(values) => {
            for value in values {
                collect_package_export_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_package_export_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn resolve_package_entry(package_dir: &Path, entry: &str) -> Option<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return None;
    }

    let entry_path = if is_relative_module(entry) {
        package_dir.join(entry)
    } else {
        package_dir.join(entry.trim_start_matches('/'))
    };
    candidate_paths(&entry_path)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.is_file())
}

fn candidate_paths(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    // TypeScript/NodeNext resolution rewrites a `./x.js` import specifier to its
    // `./x.ts` source. So when the specifier carries a JS-family extension we
    // must also probe the source equivalents (.ts/.tsx/...), not just the
    // literal `.js` path (which does not exist in a `src/` tree). Without this,
    // every `from "./mod.js"` re-export/import fails to resolve and the imported
    // symbols are falsely reported unused.
    let has_remappable_ext = base
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| JS_MODULE_EXTENSIONS.contains(&ext))
        .unwrap_or(false);

    if base.extension().is_none() || has_remappable_ext {
        for extension in JS_MODULE_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }

    for extension in JS_MODULE_EXTENSIONS {
        candidates.push(base.join(format!("index.{extension}")));
    }

    candidates
}

fn is_relative_module(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

fn is_js_ts(lang: LangId) -> bool {
    matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript)
}

fn is_js_ts_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| JS_MODULE_EXTENSIONS.contains(&ext))
}

fn language_name(lang: LangId) -> &'static str {
    match lang {
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
        LangId::Yaml => "yaml",
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
        LangId::Pascal => "pascal",
        LangId::R => "r",
        LangId::Groovy => "groovy",
        LangId::ObjC => "objc",
        LangId::Toml => "toml",
    }
}

fn absolute_path(project_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalize_path(path)
    } else {
        normalize_path(&project_root.join(path))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    result.push(component.as_os_str());
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn relative_string(project_root: &Path, path: &Path) -> String {
    let normalized_root = normalize_path(project_root);
    let normalized_path = normalize_path(path);
    if let Ok(relative) = normalized_path.strip_prefix(&normalized_root) {
        return slash_path(relative);
    }

    if let (Ok(canonical_root), Ok(canonical_path)) =
        (fs::canonicalize(project_root), fs::canonicalize(path))
    {
        let canonical_root = normalize_path(&canonical_root);
        let canonical_path = normalize_path(&canonical_path);
        if let Ok(relative) = canonical_path.strip_prefix(&canonical_root) {
            return slash_path(relative);
        }
    }

    slash_path(&normalized_path)
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn strip_quotes(text: &str) -> &str {
    text.trim()
        .trim_start_matches(['\'', '"'])
        .trim_end_matches(['\'', '"'])
}

fn node_text<'a>(source: &'a str, node: &Node) -> &'a str {
    &source[node.byte_range()]
}

fn line_number(node: &Node) -> u32 {
    (node.start_position().row + 1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, RwLock};

    use crate::config::Config;
    use crate::inspect::JobKey;
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

    fn job(root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::UnusedExports),
            category: InspectCategory::UnusedExports,
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
            callgraph_snapshot: None,
        }
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

    #[test]
    fn unused_exports_buckets_generated_exports_below_headline() {
        let (_temp_dir, root, paths) = fixture_project(&[
            (
                "src/hand.ts",
                "export function handUnused() {}
",
            ),
            (
                "gen/schema_pb.ts",
                "export function generatedPathUnused() {}
",
            ),
            (
                "src/banner.ts",
                "// Code generated by fixture. DO NOT EDIT.
export function bannerUnused() {}
",
            ),
        ]);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture path"))
            .collect::<Vec<_>>();

        let first = run_unused_exports_scan(&job(&root, paths.clone()))
            .outcome
            .expect("scan succeeds")
            .aggregate;
        let second = run_unused_exports_scan(&job(&root, paths))
            .outcome
            .expect("scan succeeds")
            .aggregate;
        assert_eq!(first, second, "twice-cold scan must be deterministic");

        assert_eq!(first["count"], 1, "{first:#}");
        assert_eq!(first["generated_count"], 2, "{first:#}");
        assert_eq!(first["total_count"], 3, "{first:#}");
        assert!(aggregate_item(&first, "src/hand.ts", "handUnused").is_some());
        assert!(
            aggregate_generated_item(&first, "gen/schema_pb.ts", "generatedPathUnused").is_some()
        );
        assert!(aggregate_generated_item(&first, "src/banner.ts", "bannerUnused").is_some());

        let item_files = first["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|item| item["file"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(item_files.first(), Some(&"src/hand.ts"), "{item_files:?}");
    }

    #[test]
    fn contribution_producers_persist_non_generated_classification() {
        let (_dir, root, paths) = fixture_project(&[
            ("src/hand.ts", "export const hand = 1;\n"),
            ("src/other.rs", "pub fn other() {}\n"),
        ]);
        let scan_job = job(&root, paths.clone());

        let legacy = run_unused_exports_scan(&scan_job)
            .outcome
            .expect("legacy scan succeeds");
        assert!(legacy.contributions.iter().all(|contribution| {
            contribution
                .contribution
                .get("generated")
                .and_then(Value::as_bool)
                == Some(false)
        }));

        let oxc_result = crate::inspect::oxc_engine::analyze_files(
            &root,
            &paths,
            crate::inspect::oxc_engine::AnalyzeOptions::default(),
        )
        .expect("oxc analysis succeeds");
        let oxc = run_unused_exports_scan_with_oxc(&scan_job, Some(&oxc_result))
            .outcome
            .expect("oxc scan succeeds");
        assert!(oxc.contributions.iter().all(|contribution| {
            contribution
                .contribution
                .get("generated")
                .and_then(Value::as_bool)
                == Some(false)
        }));

        let error = OxcEngineError {
            file: paths[0].clone(),
            message: "fixture parse error".to_string(),
        };
        let error_contribution = oxc_read_error_contribution(&root, &error)
            .expect("readable error file produces a contribution");
        assert_eq!(
            error_contribution
                .contribution
                .get("generated")
                .and_then(Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn candidate_paths_remaps_js_specifier_to_ts_source() {
        // NodeNext: `from "./active-logger.js"` must resolve to `active-logger.ts`.
        let base = Path::new("/proj/src/active-logger.js");
        let candidates = candidate_paths(base);
        assert!(
            candidates.contains(&PathBuf::from("/proj/src/active-logger.ts")),
            "expected .ts source candidate, got: {candidates:?}"
        );
        // The literal specifier is still probed first (real .js trees still work).
        assert_eq!(candidates[0], PathBuf::from("/proj/src/active-logger.js"));
    }

    #[test]
    fn candidate_paths_remaps_mjs_and_jsx_specifiers() {
        for (specifier, expected_ts) in [
            ("/proj/src/x.mjs", "/proj/src/x.ts"),
            ("/proj/src/x.jsx", "/proj/src/x.tsx"),
        ] {
            let candidates = candidate_paths(Path::new(specifier));
            assert!(
                candidates.contains(&PathBuf::from(expected_ts)),
                "{specifier}: expected {expected_ts} candidate, got: {candidates:?}"
            );
        }
    }

    #[test]
    fn candidate_paths_extensionless_still_probes_all_extensions() {
        let candidates = candidate_paths(Path::new("/proj/src/mod"));
        assert!(candidates.contains(&PathBuf::from("/proj/src/mod.ts")));
        assert!(candidates.contains(&PathBuf::from("/proj/src/mod/index.ts")));
    }
}
