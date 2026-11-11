use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cache_freshness::{self, FileFreshness};
use crate::callgraph::{resolve_module_path, resolve_reexported_symbol_target};
use crate::imports::specifier_imported_name;
use crate::inspect::job::{
    canonicalize_normalized as canonicalize_uncached, dead_code_skipped_language,
    is_test_tree_file, language_name, ExcludedTestTally, CALLGRAPH_PROVENANCE_REEXPORT,
    CALLGRAPH_PROVENANCE_TREESITTER, DISPATCHED_CALLEE_SEPARATOR,
};
use crate::inspect::oxc_engine::{
    analyze_file_facts, AnalyzeOptions, DynamicImportFact, ExportFact, FileFacts, FileId,
    ImportFact, LivenessVerdict, OxcEngineResult, OxcFileVerdicts, OxcReExportContext,
    ReExportFact, ReExportKind, FACTS_FORMAT_VERSION, OXC_PROVENANCE,
};
use crate::inspect::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory,
    InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::dead_code_facts::*;
use crate::parser::{detect_language, LangId};

use super::source_text::{
    graph_source_permit, read_graph_source_text, SourceText, GRAPH_TOO_LARGE,
    MAX_GRAPH_SOURCE_BYTES,
};
use super::DEFAULT_EXPORT_MARKER_KIND;

pub(crate) mod engine;
pub(crate) mod index;
pub(crate) mod kept;
pub(crate) use crate::data_layer::dead_code_reads as reads;
pub(crate) mod section;

use index::Index;
use reads::{Lookup, Tracked};

pub(crate) const DEAD_CODE_FACTS_FORMAT_VERSION: u32 = 11;
#[cfg(test)]
const MAX_DRILL_DOWN_ITEMS: usize = 100;
const MACRO_TOKEN_LIVENESS_PROVENANCE: &str = "macro_token_liveness";
const RUST_PATH_LIVENESS_PROVENANCE: &str = "rust_path_liveness";

type ExportNode = (String, String);

// A scan and its rollup share physical path identities, including on Rayon
// workers. Never retain them across jobs: a symlink or missing file can change
// between requests, and the next job must resolve it again.
#[derive(Default)]
struct CanonicalPaths {
    paths: parking_lot::RwLock<HashMap<PathBuf, PathBuf>>,
    relative: parking_lot::RwLock<HashMap<PathBuf, HashMap<PathBuf, String>>>,
}

thread_local! {
    static CANONICAL_PATHS: std::cell::RefCell<Option<Arc<CanonicalPaths>>> = const { std::cell::RefCell::new(None) };
}

fn current_canonical_paths() -> Option<Arc<CanonicalPaths>> {
    CANONICAL_PATHS.with(|slot| slot.borrow().clone())
}

struct CanonicalPathsScope(Option<Arc<CanonicalPaths>>);

impl Drop for CanonicalPathsScope {
    fn drop(&mut self) {
        CANONICAL_PATHS.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

fn canonical_paths_scope(paths: Option<Arc<CanonicalPaths>>) -> CanonicalPathsScope {
    CanonicalPathsScope(CANONICAL_PATHS.with(|slot| slot.replace(paths)))
}

fn with_canonical_paths<T>(paths: Option<Arc<CanonicalPaths>>, run: impl FnOnce() -> T) -> T {
    let _restore = canonical_paths_scope(paths);
    run()
}

fn with_job_canonical_paths<T>(run: impl FnOnce() -> T) -> T {
    // Rayon can execute an independent job while a parent job is waiting.
    // Only explicitly dispatched file items inherit the canonical-path memo;
    // other Rayon jobs do not.
    with_canonical_paths(Some(Arc::default()), run)
}

fn canonicalize_normalized(path: &Path) -> PathBuf {
    match current_canonical_paths() {
        Some(paths) => {
            if let Some(canonical) = paths.paths.read().get(path) {
                return canonical.clone();
            }
            paths
                .paths
                .write()
                .entry(path.to_path_buf())
                .or_insert_with(|| canonicalize_uncached(path))
                .clone()
        }
        None => canonicalize_uncached(path),
    }
}

#[derive(Debug, Clone)]
struct GoInitializerContribution {
    callers: Vec<String>,
    targets: Arc<BTreeSet<ExportNode>>,
    test_origin: bool,
}
struct OutboundCallsByCallerFile<'a> {
    by_file: BTreeMap<String, Vec<&'a CallgraphOutboundCall>>,
    canonicalization_attempts: usize,
    lexical_fallbacks: usize,
}
type MethodNamesByLanguage = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug, Default)]
struct ImportedExportLiveness {
    root_exports: Vec<ImportedExportContribution>,
    namespace_exports: Vec<ImportedExportContribution>,
}

#[derive(Debug, Clone)]
struct RustImportedSymbolSpec {
    local_name: String,
    module_segments: Vec<String>,
    imported_name: String,
}

#[derive(Default)]
struct DeadCodeFileAnalyzer {
    parsers: FactParsers,
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
    fn analyze_file(
        &mut self,
        file: &Path,
        relative_file: &str,
        has_oxc_file: bool,
    ) -> FileAnalysis {
        let Some(lang) = detect_language(file) else {
            return FileAnalysis::default();
        };
        if !StoredFileFacts::read_for(lang) {
            return FileAnalysis::default();
        }

        let metadata = fs::metadata(file).ok();
        let _permit = graph_source_permit(metadata.as_ref().map_or(0, fs::Metadata::len));
        #[cfg(test)]
        AFTER_SOURCE_METADATA.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let source = match read_graph_source_text(file) {
            Ok(SourceText::Text(source)) => source,
            Ok(SourceText::TooLarge) => {
                return FileAnalysis {
                    oversized: true,
                    ..Default::default()
                }
            }
            _ => return FileAnalysis::default(),
        };
        let tree = if file.extension().and_then(|ext| ext.to_str()) == Some("h") {
            crate::parser::parse_source_with_cached_parser(file, &source, lang).ok()
        } else {
            self.parse_source(lang, &source)
        };
        let mut analysis = StoredFileFacts::extract(&source, lang, tree.as_ref()).into_analysis(
            lang,
            file,
            relative_file,
            has_oxc_file,
        );
        analysis.freshness = Some(crate::cache_freshness::FileFreshness {
            mtime: metadata
                .and_then(|metadata| metadata.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH),
            size: source.len() as u64,
            content_hash: cache_freshness::hash_bytes(source.as_bytes()),
        });
        #[cfg(test)]
        AFTER_ANALYSIS.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        analysis
    }

    fn parse_source(&mut self, lang: LangId, source: &str) -> Option<tree_sitter::Tree> {
        self.parsers.parse_source(lang, source)
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
    with_job_canonical_paths(|| run_dead_code_scan_body(job, oxc_result, started))
}

/// [`run_dead_code_scan_with_oxc`] for a caller that keeps only the
/// files' facts and rolls them up later with every other file's: the
/// aggregate it returns is empty rather than computed over these files
/// alone.
pub(crate) fn run_dead_code_facts_scan_with_oxc(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
) -> InspectResult {
    let started = Instant::now();
    with_job_canonical_paths(|| run_dead_code_scan_inner(job, oxc_result, started, false))
}

fn run_dead_code_scan_body(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    started: Instant,
) -> InspectResult {
    run_dead_code_scan_inner(job, oxc_result, started, true)
}

fn run_dead_code_scan_inner(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    started: Instant,
    roll_up: bool,
) -> InspectResult {
    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        let success = InspectScanSuccess {
            scanned_files: job.scope_files.clone(),
            contributions: Vec::new(),
            aggregate: callgraph_unavailable_aggregate_for_job(job),
        };
        return InspectResult::success(job, success, started.elapsed());
    };

    // A snapshot from the data layer reads each file's syntax facts from
    // its content record; any other is scanned by parsing.
    let scanned = match snapshot.data_layer.as_ref() {
        Some(view) => view
            .exported_symbols_of(&job.project_root, &job.scope_files)
            .and_then(|exported_symbols| {
                let files = view.files();
                let lookup = |relative_file: &str| engine::stored_facts(&files, relative_file);
                scan_dead_code_facts_stored(job, oxc_result, &exported_symbols, &lookup)
                    .map(|scanned| scanned.map(|(contributions, _)| contributions))
            }),
        None => Ok(scan_dead_code_facts(
            job,
            oxc_result,
            &snapshot.exported_symbols,
        )),
    };
    let contributions = match scanned {
        Ok(Some(contributions)) => contributions,
        Ok(None) => {
            return InspectResult::failed(job, "dead-code scan cancelled", started.elapsed())
        }
        Err(error) => {
            return InspectResult::failed(
                job,
                &format!("dead-code facts unreadable: {error}"),
                started.elapsed(),
            )
        }
    };

    if !roll_up {
        let success = InspectScanSuccess {
            scanned_files: job.scope_files.clone(),
            contributions,
            aggregate: json!({}),
        };
        return InspectResult::success(job, success, started.elapsed());
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
        None,
    );
    let success = InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions,
        aggregate,
    };

    InspectResult::success(job, success, started.elapsed())
}

/// The dead-code facts of `job`'s scope files: each file's own facts, with
/// `exported_symbols` (the call graph's exports) as the exports of a file
/// the Oxc engine did not analyze. `None` when the job was cancelled.
pub(crate) fn scan_dead_code_facts(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    exported_symbols: &[CallgraphExport],
) -> Option<Vec<FileContribution>> {
    with_job_canonical_paths(|| {
        scan_dead_code_facts_body(job, oxc_result, exported_symbols, FactSource::Parse)
    })
}

/// The stored syntax facts of a file, by its project-relative path: `None`
/// when the call graph has no such file, an error when its record cannot be
/// read or holds no facts.
pub(crate) type StoredFactsLookup<'a> = dyn Fn(&str) -> Option<Result<Vec<u8>, String>> + Sync + 'a;

/// [`scan_dead_code_facts`], reading each file's syntax facts from the
/// facts its content record stores (`stored`) rather than parsing it. A
/// file the call graph does not have is parsed, as the scan always did;
/// the second value counts those files.
///
/// # Errors
///
/// When a file's stored facts cannot be read.
pub(crate) fn scan_dead_code_facts_stored(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    exported_symbols: &[CallgraphExport],
    stored: &StoredFactsLookup<'_>,
) -> Result<Option<(Vec<FileContribution>, usize)>, String> {
    let failures = parking_lot::Mutex::new(Vec::new());
    let parsed = std::sync::atomic::AtomicUsize::new(0);
    let contributions = with_job_canonical_paths(|| {
        scan_dead_code_facts_body(
            job,
            oxc_result,
            exported_symbols,
            FactSource::Stored {
                lookup: stored,
                failures: &failures,
                parsed: &parsed,
            },
        )
    });
    if let Some(failure) = failures.into_inner().into_iter().next() {
        return Err(failure);
    }
    Ok(contributions.map(|contributions| (contributions, parsed.into_inner())))
}

/// Where a scan reads each file's syntax facts from.
#[derive(Clone, Copy)]
enum FactSource<'a> {
    /// Parsing the file.
    Parse,
    /// The facts its content record stores.
    Stored {
        lookup: &'a StoredFactsLookup<'a>,
        /// Files whose stored facts could not be read.
        failures: &'a parking_lot::Mutex<Vec<String>>,
        /// Files the call graph does not have, parsed instead.
        parsed: &'a std::sync::atomic::AtomicUsize,
    },
}

impl FactSource<'_> {
    fn analyze(
        self,
        analyzer: &mut DeadCodeFileAnalyzer,
        file: &Path,
        relative_file: &str,
        has_oxc_file: bool,
    ) -> FileAnalysis {
        let Self::Stored {
            lookup,
            failures,
            parsed,
        } = self
        else {
            return analyzer.analyze_file(file, relative_file, has_oxc_file);
        };
        // A header's language comes from parsing it, and no header language
        // has facts; every other file's language comes from its extension.
        if file.extension().and_then(|ext| ext.to_str()) == Some("h") {
            return FileAnalysis::default();
        }
        let Some(lang) = crate::parser::language_for_extension(file) else {
            return FileAnalysis::default();
        };
        if !StoredFileFacts::read_for(lang) {
            return FileAnalysis::default();
        }
        let bytes = match lookup(relative_file) {
            None => {
                parsed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return analyzer.analyze_file(file, relative_file, has_oxc_file);
            }
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => {
                failures.lock().push(error);
                return FileAnalysis::default();
            }
        };
        let facts = match StoredFileFacts::decode(&bytes) {
            Ok(facts) => facts,
            Err(error) => {
                failures.lock().push(format!(
                    "stored dead-code facts of {relative_file}: {error}"
                ));
                return FileAnalysis::default();
            }
        };
        let mut analysis = facts.into_analysis(lang, file, relative_file, has_oxc_file);
        analysis.freshness = if has_oxc_file {
            // The file's Oxc facts carry the hash of the content they were
            // read from, and a contribution keeps its file's metadata only
            // when its freshness has that same hash (else the file looks
            // changed to every later pass, which then scans it again). So
            // this file is hashed, as parsing it would have.
            Some(collect_freshness(file))
        } else {
            // The facts are the content's the call graph read; nothing here
            // reads the file again to hash it.
            fs::metadata(file).ok().map(|metadata| FileFreshness {
                mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
                size: metadata.len(),
                content_hash: cache_freshness::zero_hash(),
            })
        };
        analysis
    }
}

fn scan_dead_code_facts_body(
    job: &InspectJob,
    oxc_result: Option<&OxcEngineResult>,
    exported_symbols: &[CallgraphExport],
    source: FactSource<'_>,
) -> Option<Vec<FileContribution>> {
    let fallback_exports_by_file = fallback_export_contributions_by_file(job, exported_symbols);
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
    let canonical_paths = current_canonical_paths();
    let progress = crate::tool_progress::FileProgress::current();
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
            Some(with_canonical_paths(canonical_paths.clone(), || {
                gather_file_contribution(
                    job,
                    file,
                    &fallback_exports_by_file,
                    &oxc_facts_by_file,
                    &oxc_parse_errors_by_file,
                    &oxc_skipped_files,
                    &mut file_analyzer,
                    source,
                )
            }))
        })
        .inspect(|_| {
            if let Some(progress) = &progress {
                progress.finished();
            }
        })
        .collect::<Vec<_>>();
    if crate::executor::current_job_cancelled() {
        return None;
    }
    Some(contributions)
}

fn fallback_export_contributions_by_file(
    job: &InspectJob,
    exported_symbols: &[CallgraphExport],
) -> BTreeMap<String, Vec<ExportContribution>> {
    let mut by_file: BTreeMap<String, Vec<ExportContribution>> = BTreeMap::new();
    for export in exported_symbols {
        if export.kind == DEFAULT_EXPORT_MARKER_KIND {
            continue;
        }
        by_file
            .entry(relative_path(&job.project_root, &export.file))
            .or_default()
            .push(ExportContribution {
                public_api: false,
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
    let lexical_root = normalize_path(project_root);
    let canonical_root = canonicalize_normalized(project_root);
    let mut by_file: BTreeMap<String, Vec<&'a CallgraphOutboundCall>> = BTreeMap::new();
    let mut comparison_keys = BTreeMap::new();
    let mut attempts = 0;
    let mut fallbacks = 0;
    for call in outbound_calls {
        let comparison_path = caller_comparison_path(
            project_root,
            &lexical_root,
            &canonical_root,
            &call.caller_file,
        );
        let key = comparison_keys
            .entry(comparison_path.clone())
            .or_insert_with(|| {
                attempts += 1;
                let (key, fallback) =
                    caller_comparison_key(&lexical_root, &canonical_root, comparison_path);
                fallbacks += usize::from(fallback);
                key
            });
        by_file.entry(key.clone()).or_default().push(call);
    }
    let grouped = OutboundCallsByCallerFile {
        by_file,
        canonicalization_attempts: attempts,
        lexical_fallbacks: fallbacks,
    };
    crate::slog_debug!(
        "dead-code caller comparison keys: {} distinct file probes, {} lexical fallbacks",
        grouped.canonicalization_attempts,
        grouped.lexical_fallbacks
    );
    if grouped.lexical_fallbacks > 0 {
        crate::slog_warn!("dead-code caller comparison: retained lexical keys for {} deleted or unreadable caller files",
            grouped.lexical_fallbacks);
    }
    grouped
}

/// `caller_file` made absolute, with a root alias rebased onto the
/// canonical root, so its physical and alias spellings share one file probe.
fn caller_comparison_path(
    project_root: &Path,
    lexical_root: &Path,
    canonical_root: &Path,
    caller_file: &Path,
) -> PathBuf {
    let absolute = normalize_absolute(project_root, caller_file);
    absolute
        .strip_prefix(lexical_root)
        .map(|relative| canonical_root.join(relative))
        .unwrap_or(absolute)
}

/// The project-relative file a caller file's calls are filed under: its
/// physical path when it still exists, else its spelling (the bool says the
/// spelling was kept). Keys never become reported or persisted paths.
fn caller_comparison_key(
    lexical_root: &Path,
    canonical_root: &Path,
    comparison_path: PathBuf,
) -> (String, bool) {
    let (path, fallback) = match fs::canonicalize(&comparison_path) {
        Ok(canonical) => (normalize_path(&canonical), false),
        Err(_) => (comparison_path, true),
    };
    (
        caller_key_of_physical(lexical_root, canonical_root, &path),
        fallback,
    )
}

/// The key [`caller_comparison_key`] files a caller's calls under, given
/// its physical path (canonical and normalized).
fn caller_key_of_physical(lexical_root: &Path, canonical_root: &Path, path: &Path) -> String {
    path.strip_prefix(canonical_root)
        .or_else(|_| path.strip_prefix(lexical_root))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// [`relative_path`] of a path already canonical and normalized, which
/// canonicalizing again would give back unchanged.
fn relative_path_of_canonical(canonical_root: &Path, path: &Path) -> String {
    path.strip_prefix(canonical_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn gather_file_contribution(
    job: &InspectJob,
    file: &Path,
    fallback_exports_by_file: &BTreeMap<String, Vec<ExportContribution>>,
    oxc_facts_by_file: &BTreeMap<String, FileFacts>,
    oxc_parse_errors_by_file: &BTreeMap<String, Vec<String>>,
    oxc_skipped_files: &[Value],
    file_analyzer: &mut DeadCodeFileAnalyzer,
    source: FactSource<'_>,
) -> FileContribution {
    let file_name = relative_path(&job.project_root, file);
    if super::source_text::graph_source_excluded(&job.project_root, file) {
        return FileContribution::new(
            InspectCategory::DeadCode,
            file.to_path_buf(),
            super::source_text::metadata_freshness(file).unwrap_or(FileFreshness {
                mtime: UNIX_EPOCH,
                size: 0,
                content_hash: cache_freshness::zero_hash(),
            }),
            json!({"file": file_name, "facts_format_version": DEAD_CODE_FACTS_FORMAT_VERSION,
                "exports": [], "generated": false, "excluded_from_graph": true}),
        );
    }
    if fs::metadata(file).is_ok_and(|metadata| metadata.len() > MAX_GRAPH_SOURCE_BYTES)
        || oxc_parse_errors_by_file
            .get(&file_name)
            .is_some_and(|errors| errors.iter().any(|error| error == GRAPH_TOO_LARGE))
    {
        return oversized_file_contribution(file, &file_name);
    }
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
        oversized,
        freshness,
        rust_api,
        go_dispatch,
        raw_imports,
        rust_imports,
        raw_reexports,
        attribute_entry_points,
        macro_token_refs,
        cfg_test_ranges,
        cfg_test_module_files,
        globs_parent_module,
        trait_impl_methods,
        type_ref_names,
    } = source.analyze(file_analyzer, file, &file_name, oxc_facts.is_some());
    if oversized {
        return oversized_file_contribution(file, &file_name);
    }

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

    if let Some(facts) = rust_api {
        payload["rust_api"] = json!(facts);
    }
    if let Some(facts) = go_dispatch {
        payload["go_dispatch"] = json!(facts);
    }

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
    if !cfg_test_module_files.is_empty() {
        payload["cfg_test_module_files"] = json!(cfg_test_module_files);
    }
    if globs_parent_module {
        payload["globs_parent_module"] = json!(true);
    }
    if !trait_impl_methods.is_empty() {
        payload["trait_impl_methods"] = json!(trait_impl_methods);
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

    // OXC facts may have been parsed before this scan. Without metadata tied
    // to that buffer, force content verification rather than blessing the
    // facts with metadata from a later save.
    let freshness = oxc_facts
        .and_then(|facts| blake3::Hash::from_hex(&facts.content_hash).ok())
        .map(|hash| match freshness.as_ref() {
            Some(freshness) if freshness.content_hash == hash => *freshness,
            _ => crate::cache_freshness::FileFreshness {
                mtime: std::time::UNIX_EPOCH,
                size: 0,
                content_hash: hash,
            },
        })
        .or(freshness);

    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        freshness.unwrap_or_else(|| collect_freshness(file)),
        payload,
    )
    .with_type_ref_names(type_ref_names)
}

fn oversized_file_contribution(file: &Path, file_name: &str) -> FileContribution {
    let freshness = fs::metadata(file)
        .ok()
        .map(|metadata| FileFreshness {
            mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
            size: metadata.len(),
            content_hash: cache_freshness::zero_hash(),
        })
        .unwrap_or(FileFreshness {
            mtime: UNIX_EPOCH,
            size: 0,
            content_hash: cache_freshness::zero_hash(),
        });
    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        freshness,
        json!({
            "file": file_name, "facts_format_version": DEAD_CODE_FACTS_FORMAT_VERSION,
            "exports": [], "skipped_files": [{"file": file_name, "reason": GRAPH_TOO_LARGE}]
        }),
    )
}

fn oxc_fact_export_contributions(facts: &FileFacts) -> Vec<ExportContribution> {
    facts
        .exports
        .iter()
        .map(|export| ExportContribution {
            public_api: false,
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
            public_api: false,
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

/// The unavailable aggregate for `job`, naming the cause when the call graph
/// was never built: the index is off, or (with views on) the current view
/// generation was published while it was off. Either way dead code is not
/// determined, so no count is reported.
pub(crate) fn callgraph_unavailable_aggregate_for_job(job: &InspectJob) -> serde_json::Value {
    let mut aggregate = callgraph_unavailable_aggregate(job.scope_files.len());
    let disabled = if !job.config.indexes.callgraph {
        Some("call graph is disabled (indexes.callgraph=false)")
    } else {
        None
    };
    if let Some(reason) = disabled {
        aggregate["notes"] = json!(["callgraph_unavailable", "callgraph_disabled"]);
        aggregate["callgraph_unavailable_reason"] = json!(reason);
    }
    aggregate
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

/// Gap reason for dead code in a borrow-only root.
pub(crate) const BORROW_ONLY_DEAD_CODE_REASON: &str =
    "dead code: unavailable in this worktree (call graph is borrow-only); run aft_inspect in the main checkout";

/// Dead-code aggregate for a borrow-only root with no readable call graph. It
/// carries no `count`: the analysis did not run, so there is nothing to count,
/// and a zero would read as "no dead code". It is marked unavailable with a
/// named gap so every surface reports it as incomplete.
pub(crate) fn borrow_only_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    let mut aggregate = callgraph_unavailable_aggregate_with_reason(scanned_files, None);
    aggregate["notes"] = json!(["callgraph_unavailable", "callgraph_borrow_only"]);
    aggregate["callgraph_unavailable_reason"] = json!(BORROW_ONLY_DEAD_CODE_REASON);
    aggregate["unavailable"] = json!(true);
    aggregate["complete"] = json!(false);
    aggregate["gaps"] = json!([{
        "kind": "tier2_unavailable",
        "reason": BORROW_ONLY_DEAD_CODE_REASON,
    }]);
    aggregate
}

/// The dead-code aggregate of `contributions` (every scanned file's facts)
/// over `snapshot`'s call graph. The drill-down limit is accepted for the
/// callers' sake: the aggregate always lists every finding.
pub(crate) fn aggregate_dead_code_contributions_with_snapshot(
    project_root: &Path,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    _drill_down_limit: Option<usize>,
) -> serde_json::Value {
    match engine::aggregate_with_snapshot(
        project_root,
        snapshot,
        contributions,
        public_api_files,
        roles,
        &mut Vec::new(),
    ) {
        Ok(aggregate) => aggregate,
        Err(reason) => {
            crate::slog_warn!(
                "tier2 dead_code: reading the call graph failed for {}: {reason}",
                project_root.display()
            );
            // Dead code is not determined without the call graph, so no
            // count is reported.
            let mut aggregate = callgraph_unavailable_aggregate(contributions.len());
            aggregate["notes"] = json!(["callgraph_unavailable", "callgraph_read_failed"]);
            aggregate["callgraph_unavailable_reason"] = json!(reason);
            aggregate
        }
    }
}

// Aggregates retain all findings; presentation applies its page limit after
// offset. Ties must not depend on contribution or callgraph iteration order.
fn rank_dead_code_items(
    mut items: Vec<Value>,
    roles: &crate::inspect::entry_points::ProjectRoles,
) -> Vec<Value> {
    items.sort_by_cached_key(|item| {
        #[cfg(test)]
        SORT_KEY_EVALUATIONS.with(|count| count.set(count.get() + 1));
        (
            roles.role_for(item["file"].as_str().unwrap_or("")),
            item["file"].as_str().unwrap_or("").to_string(),
            item["line"].as_u64().unwrap_or(0),
            item["symbol"].as_str().unwrap_or("").to_string(),
        )
    });
    items
}

fn parse_dead_code_contributions(
    project_root: &Path,
    contributions: &[FileContribution],
) -> Vec<DeadCodeContribution> {
    contributions
        .iter()
        .map(|contribution| {
            serde_json::from_value::<DeadCodeContribution>(contribution.contribution.clone())
                .unwrap_or_else(|_| {
                    // An undecodable record is a coverage gap, not a file with
                    // no findings. Its JSON file field may also be corrupt, so
                    // retain the authoritative cache-row path instead.
                    let file = relative_path(project_root, &contribution.file_path);
                    DeadCodeContribution {
                        file: file.clone(),
                        generated: Some(false),
                        decode_failed: true,
                        parse_errors: vec![json!({
                            "file": file,
                            "message": "cached dead-code contribution could not be decoded",
                        })],
                        ..Default::default()
                    }
                })
        })
        .collect::<Vec<_>>()
}

/// A Go initializer group's callers, and its targets as project-relative
/// (file, symbol) nodes.
type RelativeInitializerGroup = (Vec<String>, BTreeSet<ExportNode>);

fn relative_initializer_group(
    project_root: &Path,
    group: &crate::inspect::job::GoInitializerGroup,
) -> RelativeInitializerGroup {
    (
        group.callers.clone(),
        group
            .targets
            .iter()
            .map(|(file, symbol)| (relative_path(project_root, file), symbol.to_string()))
            .collect(),
    )
}

/// The call graph's facts about whole files that materializing reads.
struct GraphFileFacts {
    /// Project-relative files the project's entry points name.
    liveness_root_files: BTreeSet<String>,
    /// The same files, as the snapshot names them (for the Oxc engine).
    entry_points: BTreeSet<PathBuf>,
    /// The symbol each file exports as its default export.
    default_export_symbols_by_file: BTreeMap<String, String>,
}

impl GraphFileFacts {
    fn from_snapshot(project_root: &Path, snapshot: &CallgraphSnapshot) -> Self {
        let mut default_export_symbols_by_file = BTreeMap::new();
        for export in &snapshot.exported_symbols {
            if export.kind == DEFAULT_EXPORT_MARKER_KIND {
                default_export_symbols_by_file.insert(
                    relative_path(project_root, &export.file),
                    export.symbol.clone(),
                );
            }
        }
        Self::new(
            project_root,
            snapshot.entry_points.clone(),
            default_export_symbols_by_file,
        )
    }

    fn new(
        project_root: &Path,
        entry_points: BTreeSet<PathBuf>,
        default_export_symbols_by_file: BTreeMap<String, String>,
    ) -> Self {
        Self {
            liveness_root_files: entry_points
                .iter()
                .map(|file| relative_path(project_root, file))
                .collect(),
            entry_points,
            default_export_symbols_by_file,
        }
    }
}

/// What materializing one file reads about every other file: indexes over
/// all files' facts, and the call graph's facts about whole files. Built
/// once per rollup, then each file is materialized on its own.
struct MaterializeContext<'p> {
    project_root: &'p Path,
    public_api_files: Tracked<&'p BTreeSet<String>>,
    liveness_root_files: Tracked<BTreeSet<String>>,
    executable_root_exports_by_file: Tracked<Index<BTreeSet<String>>>,
    exported_symbols_by_file: Tracked<Index<BTreeSet<String>>>,
    files_by_exported_symbol: Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: Tracked<BTreeMap<String, String>>,
    oxc_by_file: Tracked<Index<OxcFileVerdicts>>,
    test_module_files: Tracked<BTreeSet<String>>,
    rust_public_items: Tracked<Index<BTreeSet<(String, u32)>>>,
    rust_used_receivers: Tracked<BTreeSet<String>>,
    go_live_methods: Tracked<Index<BTreeSet<String>>>,
    rust_items_by_file: Tracked<Index<Vec<RustApiItem>>>,
    rust_modules: RustModuleIndex,
    /// Rust type declarations, keyed by (file, symbol) like the edge targets:
    /// the module-qualified name of each type export's own declaration, plus
    /// the short export symbol when every item of the file with that name is
    /// such a declaration. A same-named function in another module
    /// (`callbacks::Probe` beside `types::Probe`) is not the type. Held as
    /// each file's symbols.
    rust_type_declarations: Tracked<Index<BTreeSet<String>>>,
    rust_imports_by_file: Tracked<Index<Vec<RawImportContribution>>>,
}

/// The whole-project indexes a [`MaterializeContext`] reads, before their
/// reads are tracked: built from every file's facts, or patched from a kept
/// dead-code graph.
struct ContextIndexes {
    liveness_root_files: BTreeSet<String>,
    executable_root_exports_by_file: Index<BTreeSet<String>>,
    exported_symbols_by_file: Index<BTreeSet<String>>,
    files_by_exported_symbol: Index<BTreeSet<String>>,
    default_export_symbols_by_file: BTreeMap<String, String>,
    oxc_by_file: Index<OxcFileVerdicts>,
    test_module_files: BTreeSet<String>,
    rust_public_items: Index<BTreeSet<(String, u32)>>,
    rust_used_receivers: BTreeSet<String>,
    go_live_methods: Index<BTreeSet<String>>,
    rust_items_by_file: Index<Vec<RustApiItem>>,
    rust_modules: RustModuleIndex,
    rust_type_declarations: Index<BTreeSet<String>>,
    rust_imports_by_file: Index<Vec<RawImportContribution>>,
    /// The resolver configuration files the Oxc analysis read (path and
    /// hash of bytes); empty when it was not run.
    oxc_config: Vec<(PathBuf, String)>,
}

/// The symbols `contribution`'s file exports, as the exported-symbols index
/// holds them: its exports, and for a Rust file every local declaration
/// (macro calls can pass through private helpers before reaching a
/// reportable export) and initializer. `None` when it has neither exports
/// nor Rust facts.
fn file_exported_symbols(contribution: &DeadCodeContribution) -> Option<BTreeSet<String>> {
    let mut symbols = None::<BTreeSet<String>>;
    for export in &contribution.exports {
        symbols
            .get_or_insert_with(BTreeSet::new)
            .insert(export.symbol.clone());
    }
    if let Some(facts) = &contribution.rust_api {
        let symbols = symbols.get_or_insert_with(BTreeSet::new);
        for item in &facts.items {
            symbols.insert(rust_qualified_item_name(item));
        }
        symbols.extend(facts.initializers.iter().map(|init| init.name.clone()));
    }
    symbols
}

/// The Rust items of `contribution`'s file, `None` without Rust facts.
fn file_rust_items(contribution: &DeadCodeContribution) -> Option<Vec<RustApiItem>> {
    contribution
        .rust_api
        .as_ref()
        .map(|facts| facts.items.clone())
}

impl Tracked<Index<Vec<RustApiItem>>> {
    /// The items of the Rust file `file` named `name`, recorded as a read of
    /// that name only, so an edit to the file's other items leaves the
    /// answer, and every file that asked for it, alone.
    fn items_named(&self, file: &str, name: &str) -> Vec<RustApiItem> {
        self.record_as(Lookup::RustItemsNamed, &reads::pair_key(file, name));
        self.all()
            .peek(file)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item.name == name)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The names whose Rust items differ between a file's `old` and `new` items.
fn rust_item_names_changed<'a>(
    old: &'a [RustApiItem],
    new: &'a [RustApiItem],
) -> BTreeSet<&'a str> {
    let by_name = |items: &'a [RustApiItem]| {
        let mut groups = BTreeMap::<&'a str, Vec<&'a RustApiItem>>::new();
        for item in items {
            groups.entry(item.name.as_str()).or_default().push(item);
        }
        groups
    };
    let (old, new) = (by_name(old), by_name(new));
    old.keys()
        .chain(new.keys())
        .filter(|name| reads::digest(&old.get(*name)) != reads::digest(&new.get(*name)))
        .copied()
        .collect()
}

/// The Rust imports of `contribution`'s file, `None` when it has none.
fn file_rust_imports(contribution: &DeadCodeContribution) -> Option<Vec<RawImportContribution>> {
    (!contribution.rust_imports.is_empty()).then(|| contribution.rust_imports.clone())
}

/// The symbols of `contribution`'s file that name Rust type declarations
/// (see [`MaterializeContext::rust_type_declarations`]), `None` when there
/// are none.
fn file_rust_type_declarations(contribution: &DeadCodeContribution) -> Option<BTreeSet<String>> {
    if language_for_file(&contribution.file) != "rust" {
        return None;
    }
    let items = contribution
        .rust_api
        .as_ref()
        .map(|facts| facts.items.as_slice())
        .unwrap_or_default();
    let type_exports = contribution
        .exports
        .iter()
        .filter(|export| export.is_type_like || is_type_like_kind(&export.kind))
        .collect::<Vec<_>>();
    // An export's line can sit on attributes or doc comments before the
    // item, so its declaration is the first same-named item outside any impl
    // (`owner` is `None`) starting at or after that line. An export with no
    // such item keeps every same-named declaration, as it cannot be told
    // apart.
    let declared = type_exports
        .iter()
        .flat_map(|export| {
            let same_named = items
                .iter()
                .filter(|item| item.owner.is_none() && item.name == export.symbol);
            match same_named
                .clone()
                .filter(|item| item.line >= export.line)
                .min_by_key(|item| item.line)
            {
                Some(item) => vec![rust_qualified_item_name(item)],
                None => same_named.map(rust_qualified_item_name).collect(),
            }
        })
        .collect::<BTreeSet<_>>();
    let short = type_exports
        .iter()
        .filter(|export| {
            items
                .iter()
                .filter(|item| item.owner.is_none() && item.name == export.symbol)
                .all(|item| declared.contains(&rust_qualified_item_name(item)))
        })
        .map(|export| export.symbol.clone())
        .collect::<Vec<_>>();
    let symbols = declared.into_iter().chain(short).collect::<BTreeSet<_>>();
    (!symbols.is_empty()).then_some(symbols)
}

/// A per-file index of `parsed`: `entry` of each file that has one.
fn per_file_index<V>(
    parsed: &[DeadCodeContribution],
    entry: impl Fn(&DeadCodeContribution) -> Option<V>,
) -> Index<V> {
    Index::memory(
        parsed
            .iter()
            .filter_map(|contribution| Some((contribution.file.clone(), entry(contribution)?)))
            .collect(),
    )
}

/// The executable-root exports of the project's entry points, by
/// project-relative file.
fn executable_root_exports_by_file(
    project_root: &Path,
    entries: &crate::inspect::entry_points::EntryPointSet,
) -> BTreeMap<String, BTreeSet<String>> {
    entries
        .executable_root_exports()
        .into_iter()
        .map(|(file, exports)| (relative_path(project_root, &file), exports))
        .collect()
}

impl ContextIndexes {
    /// Every index, built from every file's facts (`parsed`), with the
    /// project's entry points `entries`. The Oxc verdicts are computed only
    /// `with_oxc`.
    fn build(
        project_root: &Path,
        graph: GraphFileFacts,
        parsed: &[DeadCodeContribution],
        public_api_files: &BTreeSet<String>,
        entries: &crate::inspect::entry_points::EntryPointSet,
        with_oxc: bool,
    ) -> Self {
        let mut files_by_exported_symbol = BTreeMap::<String, BTreeSet<String>>::new();
        for contribution in parsed {
            for export in &contribution.exports {
                files_by_exported_symbol
                    .entry(export.symbol.clone())
                    .or_default()
                    .insert(contribution.file.clone());
            }
        }
        let (oxc_by_file, oxc_config) = if with_oxc {
            oxc_verdicts_by_file(
                project_root,
                &graph.entry_points,
                parsed,
                public_api_files,
                entries,
            )
        } else {
            (BTreeMap::new(), Vec::new())
        };
        Self {
            liveness_root_files: graph.liveness_root_files,
            executable_root_exports_by_file: Index::memory(executable_root_exports_by_file(
                project_root,
                entries,
            )),
            exported_symbols_by_file: per_file_index(parsed, file_exported_symbols),
            files_by_exported_symbol: Index::memory(files_by_exported_symbol),
            default_export_symbols_by_file: graph.default_export_symbols_by_file,
            oxc_by_file: Index::memory(oxc_by_file),
            test_module_files: cfg_test_module_files(parsed),
            rust_public_items: Index::memory(rust_public_api_items(parsed, public_api_files)),
            rust_used_receivers: parsed
                .iter()
                .filter_map(|c| c.rust_api.as_ref())
                .flat_map(|facts| facts.receiver_references.iter().cloned())
                .collect(),
            go_live_methods: Index::memory(go_interface_live_methods(parsed)),
            rust_items_by_file: per_file_index(parsed, file_rust_items),
            rust_modules: RustModuleIndex::new(parsed),
            rust_type_declarations: per_file_index(parsed, file_rust_type_declarations),
            rust_imports_by_file: per_file_index(parsed, file_rust_imports),
            oxc_config,
        }
    }
}

impl<'p> MaterializeContext<'p> {
    /// Indexes `parsed`. The Oxc verdicts are computed only `with_oxc`: a
    /// rollup patched from a kept graph reads the verdicts it needs from
    /// that graph instead.
    fn new(
        project_root: &'p Path,
        graph: GraphFileFacts,
        parsed: &[DeadCodeContribution],
        public_api_files: &'p BTreeSet<String>,
        with_oxc: bool,
    ) -> Self {
        let entries = crate::inspect::entry_points::resolve_entry_points(project_root);
        let indexes = ContextIndexes::build(
            project_root,
            graph,
            parsed,
            public_api_files,
            &entries,
            with_oxc,
        );
        Self::with_indexes(project_root, public_api_files, indexes)
    }

    /// A context reading `indexes`.
    fn with_indexes(
        project_root: &'p Path,
        public_api_files: &'p BTreeSet<String>,
        indexes: ContextIndexes,
    ) -> Self {
        let ContextIndexes {
            liveness_root_files,
            executable_root_exports_by_file,
            exported_symbols_by_file,
            files_by_exported_symbol,
            default_export_symbols_by_file,
            oxc_by_file,
            test_module_files,
            rust_public_items,
            rust_used_receivers,
            go_live_methods,
            rust_items_by_file,
            rust_modules,
            rust_type_declarations,
            rust_imports_by_file,
            oxc_config: _,
        } = indexes;
        Self {
            project_root,
            public_api_files: Tracked::new(Lookup::PublicApiFiles, public_api_files),
            liveness_root_files: Tracked::new(Lookup::LivenessRootFiles, liveness_root_files),
            executable_root_exports_by_file: Tracked::new(
                Lookup::ExecutableRootExports,
                executable_root_exports_by_file,
            ),
            exported_symbols_by_file: Tracked::new(
                Lookup::ExportedSymbolsByFile,
                exported_symbols_by_file,
            ),
            files_by_exported_symbol: Tracked::new(
                Lookup::FilesByExportedSymbol,
                files_by_exported_symbol,
            ),
            default_export_symbols_by_file: Tracked::new(
                Lookup::DefaultExportByFile,
                default_export_symbols_by_file,
            ),
            oxc_by_file: Tracked::new(Lookup::OxcByFile, oxc_by_file),
            test_module_files: Tracked::new(Lookup::TestModuleFiles, test_module_files),
            rust_public_items: Tracked::new(Lookup::RustPublicItems, rust_public_items),
            rust_used_receivers: Tracked::new(Lookup::RustUsedReceivers, rust_used_receivers),
            go_live_methods: Tracked::new(Lookup::GoLiveMethods, go_live_methods),
            rust_items_by_file: Tracked::new(Lookup::RustItemsByFile, rust_items_by_file),
            rust_modules,
            rust_type_declarations: Tracked::new(
                Lookup::RustTypeDeclarations,
                rust_type_declarations,
            ),
            rust_imports_by_file: Tracked::new(Lookup::RustImportsByFile, rust_imports_by_file),
        }
    }

    /// The first kept index entry a patched context could not read: its
    /// answers are then incomplete.
    fn failure(&self) -> Option<String> {
        [
            self.executable_root_exports_by_file.all().failure(),
            self.exported_symbols_by_file.all().failure(),
            self.files_by_exported_symbol.all().failure(),
            self.oxc_by_file.all().failure(),
            self.rust_public_items.all().failure(),
            self.go_live_methods.all().failure(),
            self.rust_items_by_file.all().failure(),
            self.rust_type_declarations.all().failure(),
            self.rust_imports_by_file.all().failure(),
        ]
        .into_iter()
        .flatten()
        .next()
    }

    /// A digest of each whole-project index, to check that indexes built
    /// from stored inputs equal indexes built from every file's facts. A
    /// patched index reads every kept entry for this.
    fn index_digests(&self) -> Vec<(&'static str, u64)> {
        use reads::digest;
        vec![
            ("public_api_files", digest(self.public_api_files.all())),
            (
                "liveness_root_files",
                digest(self.liveness_root_files.all()),
            ),
            (
                "executable_root_exports_by_file",
                digest(&self.executable_root_exports_by_file.all().entries()),
            ),
            (
                "exported_symbols_by_file",
                digest(&self.exported_symbols_by_file.all().entries()),
            ),
            (
                "files_by_exported_symbol",
                digest(&self.files_by_exported_symbol.all().entries()),
            ),
            (
                "default_export_symbols_by_file",
                digest(self.default_export_symbols_by_file.all()),
            ),
            ("oxc_by_file", digest(&self.oxc_by_file.all().entries())),
            ("test_module_files", digest(self.test_module_files.all())),
            (
                "rust_public_items",
                digest(&self.rust_public_items.all().entries()),
            ),
            (
                "rust_used_receivers",
                digest(self.rust_used_receivers.all()),
            ),
            (
                "go_live_methods",
                digest(&self.go_live_methods.all().entries()),
            ),
            (
                "rust_items_by_file",
                digest(&self.rust_items_by_file.all().entries()),
            ),
            ("rust_modules", digest(&self.rust_modules)),
            (
                "rust_type_declarations",
                digest(&self.rust_type_declarations.all().entries()),
            ),
            (
                "rust_imports_by_file",
                digest(&self.rust_imports_by_file.all().entries()),
            ),
        ]
    }

    /// The project-relative files compiled only for tests by a
    /// `#[cfg(test)]` module declaration.
    fn test_module_files(&self) -> &BTreeSet<String> {
        self.test_module_files.all()
    }

    /// The node a liveness root of `file` names: a method root is named
    /// with its impl, as the edges name it.
    fn root_node(&self, file: &str, root: String) -> String {
        // The answer looks only at the file's items named like the root's
        // last path segment.
        let name = root.rsplit("::").next().unwrap_or_default();
        let items = self.rust_items_by_file.items_named(file, name);
        rust_method_node(&items, &root, None).unwrap_or(root)
    }

    /// Materializes `contribution`: its edges from `outbound_calls` (the
    /// call graph's calls made in the file) and from its own facts, its
    /// liveness roots, imported exports and dispatched method names.
    /// `go_initializers` is the file's Go package initialization group and
    /// `snapshot_roots` the symbols the call graph marks as keeping
    /// themselves alive.
    fn materialize_file(
        &self,
        mut contribution: DeadCodeContribution,
        outbound_calls_for_file: &[&CallgraphOutboundCall],
        go_initializers: Option<RelativeInitializerGroup>,
        snapshot_roots: Option<&BTreeSet<String>>,
    ) -> DeadCodeContribution {
        let project_root = self.project_root;
        let public_api_files = &self.public_api_files;
        let test_module_files = &self.test_module_files;
        let exported_symbols_by_file = &self.exported_symbols_by_file;
        let files_by_exported_symbol = &self.files_by_exported_symbol;
        let default_export_symbols_by_file = &self.default_export_symbols_by_file;
        let rust_items_by_file = &self.rust_items_by_file;
        let rust_modules = &self.rust_modules;
        let rust_public_items = &self.rust_public_items;
        let rust_used_receivers = &self.rust_used_receivers;
        let rust_type_declarations = &self.rust_type_declarations;
        let _facts_format_version = contribution.facts_format_version;
        if let Some(facts) = &mut contribution.go_dispatch {
            facts.live_methods = self
                .go_live_methods
                .get(&contribution.file)
                .cloned()
                .unwrap_or_default();
        }
        let mut exports = self
            .oxc_by_file
            .get(&contribution.file)
            .map(oxc_export_contributions)
            .unwrap_or_else(|| contribution.exports.clone());

        let imports_in_scope = rust_imports_in_scope(
            &contribution.file,
            &contribution.rust_imports,
            contribution.globs_parent_module,
            &self.rust_imports_by_file,
        );
        let mut internal_calls = outbound_calls_for_file
            .iter()
            .copied()
            .flat_map(|call| {
                let test_origin = contribution.location_is_test_code(test_module_files, call.line);
                let projected = match project_internal_call(
                    project_root,
                    call,
                    &contribution.file,
                    test_origin,
                    exported_symbols_by_file,
                    files_by_exported_symbol,
                ) {
                    Some(internal_call) => vec![internal_call],
                    None => rust_unresolved_path_call_edges(
                        call,
                        &contribution.file,
                        &imports_in_scope,
                        test_origin,
                        exported_symbols_by_file,
                        files_by_exported_symbol,
                        rust_modules,
                        rust_items_by_file
                            .get(&contribution.file)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                    ),
                };
                projected
                    .into_iter()
                    .filter(|call| {
                        // The stored callgraph can collapse same-named module
                        // functions to a short callee. Source edges below retain
                        // the path and are authoritative for ambiguous names.
                        call.symbol.contains("::")
                            || rust_items_by_file
                                .items_named(&call.file, &call.symbol)
                                .iter()
                                .filter(|item| item.owner.is_none())
                                .map(rust_qualified_item_name)
                                .collect::<BTreeSet<_>>()
                                .len()
                                <= 1
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        internal_calls.extend(resolve_raw_reexport_liveness_edges(
            project_root,
            &contribution.file,
            &contribution.raw_reexports,
            exported_symbols_by_file,
            default_export_symbols_by_file,
        ));
        if let Some(oxc_facts) = &contribution.oxc_facts {
            internal_calls.extend(resolve_oxc_reexport_liveness_edges(
                project_root,
                &contribution.file,
                oxc_facts,
                exported_symbols_by_file,
                default_export_symbols_by_file,
            ));
        }
        // References found in source (inside macro tokens, function
        // bodies and function values) take the test status of their own
        // line, exactly like the stored callgraph edges above, so a call
        // made only by test code never counts as production usage. Edges
        // to type declarations drop that status again below.
        let source_test_origin =
            |line: u32| contribution.location_is_test_code(test_module_files, line);
        let caller_items = rust_items_by_file
            .get(&contribution.file)
            .map(Vec::as_slice)
            .unwrap_or_default();
        internal_calls.extend(resolve_macro_token_liveness_edges(
            project_root,
            &contribution.file,
            &contribution.macro_token_refs,
            &contribution.rust_imports,
            exported_symbols_by_file,
            rust_modules,
            caller_items,
            &source_test_origin,
        ));
        if let Some(facts) = &contribution.rust_api {
            internal_calls.extend(resolve_macro_token_liveness_edges(
                project_root,
                &contribution.file,
                &facts.initializer_references,
                &contribution.rust_imports,
                exported_symbols_by_file,
                rust_modules,
                caller_items,
                &source_test_origin,
            ));
            // The parser does not expose private statics as symbols, so
            // their calls arrive with a top-level owner. Give each static
            // its own node; only reading that static activates its body.
            for call in &mut internal_calls {
                if call.caller_symbol == "<top-level>" {
                    if let Some(init) = facts
                        .initializers
                        .iter()
                        .find(|init| init.range.contains(call.line))
                    {
                        call.caller_symbol = init.name.clone();
                    }
                }
            }
        }
        for call in &mut internal_calls {
            if let Some(items) = rust_items_by_file.get(&contribution.file) {
                call.caller_symbol = rust_method_node(items, &call.caller_symbol, Some(call.line))
                    .unwrap_or_else(|| {
                        rust_function_identity(items, &call.caller_symbol, Some(call.line), None)
                    });
            }
            // Both lookups below look only at the callee file's items named
            // like the callee's last path segment.
            let callee_name = call.symbol.rsplit("::").next().unwrap_or_default();
            let items = rust_items_by_file.items_named(&call.file, callee_name);
            call.symbol = rust_method_node(&items, &call.symbol, None).unwrap_or_else(|| {
                rust_function_identity(
                    &items,
                    &call.symbol,
                    None,
                    (call.file == contribution.file).then_some(call.caller_symbol.as_str()),
                )
            });
            // An edge to a struct, enum or other type declaration carries no
            // test status: production code uses types through type
            // positions and plain values, which are not edges, so a test
            // label on an edge alone would mark production-used types
            // test-only. Edges to functions and methods keep their status.
            if rust_type_declarations.contains_pair(&(call.file.clone(), call.symbol.clone())) {
                call.test_origin = None;
            }
        }
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
            exported_symbols_by_file,
            default_export_symbols_by_file,
        );
        let mut attribute_entry_points = contribution
            .attribute_entry_points
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if let Some(snapshot_roots) = snapshot_roots {
            attribute_entry_points.extend(snapshot_roots.iter().cloned());
        }
        // Trait methods in a library need an exported type and trait path;
        // a private impl is not a blanket public-API root.
        if !rust_file_in_library(&contribution.file, rust_public_items) {
            attribute_entry_points.extend(contribution.trait_impl_methods.iter().cloned());
        }
        for export in &mut exports {
            export.public_api = rust_item_is_public_api(&contribution, export, rust_public_items);
        }
        if let Some(facts) = &contribution.rust_api {
            for item in &facts.items {
                // A used private receiver can be invoked through a trait
                // (including a derived Default on a containing type). Its
                // impl body is live even without a public module path.
                if item.trait_name.is_some()
                    && item.owner.as_ref().is_some_and(|owner| {
                        rust_used_receivers.contains(owner.rsplit("::").next().unwrap_or(owner))
                    })
                {
                    if let (Some(owner), Some(trait_name)) = (&item.owner, &item.trait_name) {
                        attribute_entry_points
                            .insert(format!("{trait_name} for {owner}::{}", item.name));
                        attribute_entry_points.insert(item.name.clone());
                    }
                }
                if rust_public_items
                    .get(&contribution.file)
                    .is_some_and(|items| items.contains(&(item.name.clone(), item.line)))
                {
                    if facts
                        .items
                        .iter()
                        .filter(|candidate| candidate.name == item.name)
                        .count()
                        == 1
                    {
                        attribute_entry_points.insert(item.name.clone());
                    }
                    if item.owner.is_none() {
                        attribute_entry_points.insert(macro_scoped_symbol(&item.scope, &item.name));
                    }
                    if let Some(owner) = &item.owner {
                        attribute_entry_points.insert(format!("{owner}::{}", item.name));
                        if let Some(trait_name) = &item.trait_name {
                            attribute_entry_points
                                .insert(format!("{trait_name} for {owner}::{}", item.name));
                        }
                    }
                }
            }
        }
        let mut root_exports = exports.clone();
        for export in &mut root_exports {
            export.symbol = rust_export_identity(export, contribution.rust_api.as_ref());
        }
        let mut liveness_roots = liveness_roots_for_file(
            &contribution.file,
            &root_exports,
            &internal_calls,
            &attribute_entry_points,
            self.executable_root_exports_by_file.get(&contribution.file),
            self.liveness_root_files.contains(&contribution.file),
            language_for_file(&contribution.file) != "rust"
                && public_api_files.contains(&contribution.file),
        );
        // Roots name methods with their impl (`Owner::name`) as well; use
        // the same method node as the edges above.
        if let Some(items) = rust_items_by_file.get(&contribution.file) {
            for root in &mut liveness_roots {
                if let Some(node) = rust_method_node(items, root, None) {
                    *root = node;
                }
            }
            liveness_roots.sort();
            liveness_roots.dedup();
        }
        contribution.go_initializers =
            go_initializers.map(|(callers, targets)| GoInitializerContribution {
                callers,
                targets: Arc::new(targets),
                test_origin: is_test_tree_file(&contribution.file)
                    || test_module_files.contains(&contribution.file),
            });
        if self.liveness_root_files.contains(&contribution.file)
            && !self
                .executable_root_exports_by_file
                .contains_key(&contribution.file)
        {
            if let Some(group) = &contribution.go_initializers {
                liveness_roots.extend(
                    group
                        .callers
                        .iter()
                        .filter(|caller| {
                            is_explicit_liveness_symbol(&contribution.file, caller)
                                && group.targets.iter().any(|target| {
                                    target != &(contribution.file.clone(), (*caller).clone())
                                })
                        })
                        .cloned(),
                );
                liveness_roots.sort();
                liveness_roots.dedup();
            }
        }
        for export in &mut exports {
            export.is_entry_point = liveness_roots.contains(&rust_export_identity(
                export,
                contribution.rust_api.as_ref(),
            ));
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
    }
}

/// The Oxc engine's verdicts for the files of `contributions` with current
/// Oxc facts, by project-relative file, and the resolver configuration files
/// the analysis read (path and hash of bytes).
fn oxc_verdicts_by_file<'c>(
    project_root: &Path,
    entry_points: &BTreeSet<PathBuf>,
    contributions: impl IntoIterator<Item = &'c DeadCodeContribution>,
    public_api_files: &BTreeSet<String>,
    entries: &crate::inspect::entry_points::EntryPointSet,
) -> (BTreeMap<String, OxcFileVerdicts>, Vec<(PathBuf, String)>) {
    let facts = contributions
        .into_iter()
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
        return (BTreeMap::new(), Vec::new());
    }

    let result = analyze_file_facts(
        project_root,
        facts,
        AnalyzeOptions {
            entry_points: entry_points.iter().cloned().collect(),
            public_api_files: public_api_files
                .iter()
                .map(|file| project_root.join(file))
                .collect(),
            executable_root_exports: entries.executable_root_exports(),
            force_reparse_files: Vec::new(),
            entry_reachability: true,
        },
        Vec::new(),
    );
    let config = result
        .resolver_config_inputs
        .iter()
        .map(|input| (input.path.clone(), input.content_hash.clone()))
        .collect();
    let verdicts = result
        .files
        .into_iter()
        .map(|file| (file.relative_file.clone(), file))
        .collect();
    (verdicts, config)
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

/// What rendering the findings asks about the liveness of one node, a
/// (file, identity) pair as the graph names it.
trait LivenessAnswers {
    /// Whether any root reaches the node.
    fn reachable(&self, file: &str, identity: &str) -> bool;
    /// Whether a root reaches the node without passing a test-only edge.
    fn production_reachable(&self, file: &str, identity: &str) -> bool;
    /// The tests that alone use the node, when only tests use it: no
    /// production caller reaches it, directly or through other code.
    fn test_only_callers(&self, file: &str, identity: &str) -> Option<Vec<String>>;
}

/// What rendering reads about every file at once.
struct RenderInputs<'a> {
    /// A source too large to read is a graph-wide gap: it can be the only
    /// caller or importer of a symbol elsewhere.
    has_source_gaps: bool,
    /// Type names referenced in type position anywhere in the project.
    referenced_type_names: BTreeSet<String>,
    /// Files compiled only for tests by a `#[cfg(test)]` declaration.
    test_module_files: BTreeSet<String>,
    /// Method names called on receivers of unknown type, by language.
    dispatched_method_names: &'a MethodNamesByLanguage,
}

/// The dead-code findings of the files `parsed`, from their materialized
/// facts and the liveness `answers` give.
fn render_dead_code_findings(
    project_root: &Path,
    parsed: &[DeadCodeContribution],
    public_api_files: &BTreeSet<String>,
    roles: &crate::inspect::entry_points::ProjectRoles,
    scanned_files: usize,
    answers: &dyn LivenessAnswers,
    inputs: &RenderInputs<'_>,
) -> serde_json::Value {
    let referenced_type_names = &inputs.referenced_type_names;
    let test_module_files = &inputs.test_module_files;
    let dispatched_method_names = inputs.dispatched_method_names;

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut count = 0usize;
    let mut headline_items = Vec::new();
    let mut generated_count = 0usize;
    let mut generated_items = Vec::new();
    let mut test_only_count = 0usize;
    let mut test_only_items = Vec::new();
    let mut uncertain_count = 0usize;
    let mut uncertain_items: Vec<serde_json::Value> = Vec::new();
    let mut excluded_by_file: BTreeMap<String, usize> = BTreeMap::new();
    // A skipped source can be the only caller/importer of a symbol elsewhere.
    // With no edges from that file, deadness cannot be established graph-wide.
    let has_source_gaps = inputs.has_source_gaps;
    for contribution in parsed {
        if has_source_gaps {
            continue;
        }
        let generated_file = crate::inspect::generated::is_generated_file_with_cached_hint(
            project_root,
            &contribution.file,
            contribution.generated,
        );
        // Test code (test files, fixtures, mock data, `#[cfg(test)]` module
        // files) is analyzed so its calls keep product code live and mark
        // test-only usage, but its own dead symbols are not product findings:
        // they are tallied as excluded instead of counted.
        let test_code = is_test_code_file(&contribution.file, test_module_files);
        let is_public_api_file = language_for_file(&contribution.file) != "rust"
            && public_api_files.contains(&contribution.file);
        let candidates = contribution
            .exports
            .iter()
            .filter(|export| !export.public_api)
            .cloned()
            .collect::<Vec<_>>();
        for (export, other_lines) in
            exports_deduplicated_by_node(&candidates, contribution.rust_api.as_ref())
        {
            if !test_code && contribution.line_is_cfg_test(export.line) {
                *excluded_by_file
                    .entry(contribution.file.clone())
                    .or_default() += 1;
                continue;
            }
            if export_uses_oxc(export) {
                match export.verdict.unwrap_or(LivenessVerdict::Unused) {
                    LivenessVerdict::Used => {
                        if !test_code && !export.test_only_reference_files.is_empty() {
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
                    LivenessVerdict::Uncertain if test_code => continue,
                    LivenessVerdict::Uncertain => {
                        uncertain_count += 1;
                        {
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
                        if !test_code && !export.test_only_reference_files.is_empty() {
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
                let identity = rust_export_identity(export, contribution.rust_api.as_ref());
                let file = contribution.file.as_str();
                let test_only_callers = (!test_code
                    && !is_public_api_file
                    && !export.public_api
                    && !export.is_entry_point
                    && !answers.production_reachable(file, &identity))
                .then(|| answers.test_only_callers(file, &identity))
                .flatten();
                if let Some(used_by) = test_only_callers {
                    // Transitive test usage names the original tests, not the
                    // production helper in the middle of the chain.
                    let mut item = json!({
                        "file": contribution.file,
                        "symbol": export.symbol,
                        "kind": export.kind,
                        "line": export.line,
                        "provenance": CALLGRAPH_PROVENANCE_TREESITTER,
                        "used_by": used_by,
                    });
                    add_other_definition_lines(&mut item, &other_lines);
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
                if answers.reachable(file, &identity)
                    || is_public_api_file
                    || export.public_api
                    || dispatch_liveness_keeps_export_live(
                        contribution,
                        export,
                        dispatched_method_names,
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

            if test_code {
                *excluded_by_file
                    .entry(contribution.file.clone())
                    .or_default() += 1;
                continue;
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
            add_other_definition_lines(&mut item, &other_lines);
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

    let headline_items = rank_dead_code_items(headline_items, roles);
    let generated_items = rank_dead_code_items(generated_items, roles);
    let uncertain_items = rank_dead_code_items(uncertain_items, roles);
    let top = crate::inspect::entry_points::top_preview_symbols(&headline_items);
    let mut dead_items = headline_items;
    dead_items.extend(generated_items.iter().cloned());
    let generated_top = generated_items
        .iter()
        .take(crate::inspect::entry_points::TOP_PREVIEW_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let test_only_items = rank_dead_code_items(test_only_items, roles);
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
        "drill_down_capped": false,
        "generated_drill_down_capped": false,
        "test_only_drill_down_capped": false,
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
        aggregate["skipped_files_count"] = json!(skipped_files.len());
        aggregate["skipped_files"] = Value::Array(skipped_files);
    }
    let mut excluded = ExcludedTestTally::default();
    for (file, findings) in &excluded_by_file {
        excluded.record_many(file, *findings);
    }
    excluded.write_into(&mut aggregate);
    aggregate
}

/// Keep separate declarations in separate modules distinct while coalescing
/// cfg alternatives of the same declaration. Report rows retain their original
/// short symbol and source line; the qualified identity is used internally for
/// deduplication and graph lookup.
fn exports_deduplicated_by_node<'a>(
    exports: &'a [ExportContribution],
    rust_api: Option<&RustApiFacts>,
) -> Vec<(&'a ExportContribution, Vec<u32>)> {
    let mut deduplicated: Vec<(&ExportContribution, Vec<u32>)> = Vec::new();
    let mut index_by_node: HashMap<(String, &str), usize> = HashMap::new();
    for export in exports {
        match index_by_node.entry((rust_export_identity(export, rust_api), export.kind.as_str())) {
            Entry::Occupied(entry) => {
                let (first, other_lines) = &mut deduplicated[*entry.get()];
                if first.line != export.line && !other_lines.contains(&export.line) {
                    other_lines.push(export.line);
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(deduplicated.len());
                deduplicated.push((export, Vec::new()));
            }
        }
    }
    deduplicated
}

fn add_other_definition_lines(item: &mut Value, other_lines: &[u32]) {
    if !other_lines.is_empty() {
        item["other_lines"] = json!(other_lines);
    }
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

fn has_oversized_sources(contributions: &[DeadCodeContribution]) -> bool {
    contributions.iter().any(|contribution| {
        contribution
            .skipped_files
            .iter()
            .any(|skip| skip["reason"] == GRAPH_TOO_LARGE)
    })
}

// Both inputs are sorted and unique. Bulk union avoids a B-tree insertion for
// every label on every graph edge, including edges that add no new origins.
fn merge_test_origin_ids(ids: &mut Vec<usize>, labels: &[usize]) -> bool {
    if labels.is_empty() || ids.as_slice() == labels {
        return false;
    }
    if ids
        .last()
        .zip(labels.first())
        .is_none_or(|(last, first)| last < first)
    {
        ids.extend_from_slice(labels);
        return true;
    }
    let mut merged = Vec::with_capacity(ids.len() + labels.len());
    let (mut left, mut right) = (0, 0);
    while left < ids.len() && right < labels.len() {
        match ids[left].cmp(&labels[right]) {
            std::cmp::Ordering::Less => {
                merged.push(ids[left]);
                left += 1;
            }
            std::cmp::Ordering::Greater => {
                merged.push(labels[right]);
                right += 1;
            }
            std::cmp::Ordering::Equal => {
                merged.push(ids[left]);
                left += 1;
                right += 1;
            }
        }
    }
    merged.extend_from_slice(&ids[left..]);
    merged.extend_from_slice(&labels[right..]);
    if merged.len() == ids.len() {
        return false;
    }
    *ids = merged;
    true
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
        if language == "go" {
            let mut methods = contribution
                .go_dispatch
                .as_ref()
                .map(|facts| facts.live_methods.clone())
                .unwrap_or_default();
            // An explicit receiver call is independent evidence of method
            // use. Interface satisfaction adds implicit uses; it must not
            // remove the existing callgraph's name-based dispatch evidence.
            if let Some(names) = dispatched_method_names.get(language) {
                methods.extend(
                    contribution
                        .exports
                        .iter()
                        .filter(|export| {
                            export_is_method(export)
                                && names.contains(symbol_liveness_name(&export.symbol))
                        })
                        .map(|export| symbol_liveness_name(&export.symbol).to_string()),
                );
            }
            by_file.insert(
                contribution.file.as_str(),
                DispatchNamesForFile::GoMethods(methods),
            );
            continue;
        }
        let Some(language_method_names) = dispatched_method_names.get(language) else {
            continue;
        };
        by_file.insert(
            contribution.file.as_str(),
            DispatchNamesForFile::Language(language_method_names),
        );
    }
    by_file
}

fn dispatch_liveness_keeps_export_live(
    contribution: &DeadCodeContribution,
    export: &ExportContribution,
    dispatched_method_names: &MethodNamesByLanguage,
) -> bool {
    let language = language_for_file(&contribution.file);
    if language == "go" {
        return export_is_method(export)
            && (dispatched_method_names
                .get(language)
                .is_some_and(|names| names.contains(symbol_liveness_name(&export.symbol)))
                || contribution.go_dispatch.as_ref().is_some_and(|facts| {
                    facts
                        .live_methods
                        .contains(symbol_liveness_name(&export.symbol))
                }));
    }
    let Some(method_names) = dispatched_method_names.get(language) else {
        return false;
    };
    method_names.contains(symbol_liveness_name(&export.symbol))
}

fn export_is_method(export: &ExportContribution) -> bool {
    export.kind == "method"
}

fn project_internal_call(
    project_root: &Path,
    call: &CallgraphOutboundCall,
    caller_file: &str,
    test_origin: bool,
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    files_by_exported_symbol: &Tracked<Index<BTreeSet<String>>>,
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

/// Where each Rust module lives, read from the module declarations of the
/// scanned files (honouring `#[path]`) instead of from file names alone. It
/// resolves module paths the file-name rules miss: `super::` written inside an
/// inline module, modules declared with `#[path]`, and module names a file
/// sees only through a chain of `use super::*` globs.
#[derive(Debug, Default)]
struct RustModuleIndex {
    /// (declaring file, module path inside that file) -> file of an
    /// out-of-line module.
    files: BTreeMap<(String, Vec<String>), String>,
    /// (file, module path inside that file) of each inline module.
    inline: BTreeSet<(String, Vec<String>)>,
    /// Out-of-line module file -> the file and inline module path declaring it.
    parents: BTreeMap<String, (String, Vec<String>)>,
    /// Files with a file-level `use super::*;`.
    globs_parent: BTreeSet<String>,
}

impl RustModuleIndex {
    fn new(contributions: &[DeadCodeContribution]) -> Self {
        let known = contributions
            .iter()
            .map(|contribution| contribution.file.as_str())
            .collect::<BTreeSet<_>>();
        Self::from_inputs(
            &known,
            contributions.iter().map(|contribution| {
                (
                    contribution.file.as_str(),
                    contribution.globs_parent_module,
                    contribution
                        .rust_api
                        .as_ref()
                        .map(|facts| facts.modules.as_slice())
                        .unwrap_or_default(),
                )
            }),
        )
    }

    /// The index of the module rules `inputs` give (each file, whether it
    /// globs its parent module, and its module declarations), resolving
    /// out-of-line modules to the `known` files only.
    fn from_inputs<'a>(
        known: &BTreeSet<&str>,
        inputs: impl Iterator<Item = (&'a str, bool, &'a [RustApiModule])>,
    ) -> Self {
        let mut index = Self::default();
        for (file, globs_parent_module, modules) in inputs {
            if globs_parent_module {
                index.globs_parent.insert(file.to_string());
            }
            for module in modules {
                let mut path = module.scope.clone();
                path.push(module.name.clone());
                let key = (file.to_string(), path.clone());
                let Some(declared) = &module.file else {
                    index.inline.insert(key);
                    continue;
                };
                // The declared location, its `mod.rs` form, and (for a crate
                // root with a manifest-chosen name) the file beside the root.
                let beside_root = Path::new(file)
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(path.join("/"))
                    .with_extension("rs")
                    .to_string_lossy()
                    .replace('\\', "/");
                let Some(found) = [
                    declared.clone(),
                    format!("{}/mod.rs", declared.trim_end_matches(".rs")),
                    beside_root.clone(),
                    format!("{}/mod.rs", beside_root.trim_end_matches(".rs")),
                ]
                .into_iter()
                .find(|candidate| known.contains(candidate.as_str())) else {
                    continue;
                };
                index
                    .parents
                    .insert(found.clone(), (file.to_string(), module.scope.clone()));
                index.files.insert(key, found);
            }
        }
        index
    }

    /// The module (file and inline module path inside it) that `path` names
    /// when written in module `scope` of `file`.
    fn resolve_module(
        &self,
        file: &str,
        scope: &[String],
        path: &[String],
    ) -> Option<(String, Vec<String>)> {
        let mut position = (file.to_string(), scope.to_vec());
        let mut rest = path;
        match rest.first().map(String::as_str) {
            Some("crate") => {
                // Bounded so a cyclic `#[path]` declaration cannot loop.
                reads::record(Lookup::RustModuleParentCount, "");
                for _ in 0..=self.parents.len() {
                    reads::record(Lookup::RustModuleParents, &position.0);
                    let Some((parent, _)) = self.parents.get(&position.0) else {
                        break;
                    };
                    position = (parent.clone(), Vec::new());
                }
                position.1.clear();
                rest = &rest[1..];
            }
            Some("self") => rest = &rest[1..],
            _ => {
                while rest.first().is_some_and(|segment| segment == "super") {
                    if position.1.pop().is_none() {
                        reads::record(Lookup::RustModuleParents, &position.0);
                        position = self.parents.get(&position.0)?.clone();
                    }
                    rest = &rest[1..];
                }
            }
        }
        for segment in rest {
            let mut key = (position.0.clone(), position.1.clone());
            key.1.push(segment.clone());
            let read_key = reads::path_key(&key.0, &key.1);
            reads::record(Lookup::RustModuleFiles, &read_key);
            if let Some(child) = self.files.get(&key) {
                position = (child.clone(), Vec::new());
            } else if {
                reads::record(Lookup::RustModuleInline, &read_key);
                self.inline.contains(&key)
            } {
                position = key;
            } else {
                return None;
            }
        }
        Some(position)
    }

    /// The declaration `path::name` names from module `scope` of `file`. The
    /// module path is resolved relative to that module; when `scope` is the
    /// file's own module, it is also tried from each parent module reached
    /// through a chain of file-level `use super::*` globs. A path ending in a
    /// type (`module::Type::method`) names that type's method.
    fn resolve_item(
        &self,
        file: &str,
        scope: &[String],
        path: &[String],
        name: &str,
        exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    ) -> Option<ExportNode> {
        if path.is_empty() {
            return None;
        }
        let lookup = |module_path: &[String], symbol_prefix: &[String]| {
            let target = |(module_file, module_scope): (String, Vec<String>)| {
                let symbol = module_scope
                    .iter()
                    .chain(symbol_prefix)
                    .map(String::as_str)
                    .chain(std::iter::once(name))
                    .collect::<Vec<_>>()
                    .join("::");
                exported_symbol_target(&module_file, &symbol, exported_symbols_by_file)
            };
            if let Some(found) = self
                .resolve_module(file, scope, module_path)
                .and_then(target)
            {
                return Some(found);
            }
            if !scope.is_empty() || matches!(module_path[0].as_str(), "crate" | "self" | "super") {
                return None;
            }
            let mut current = file.to_string();
            let mut visited = BTreeSet::new();
            while {
                reads::record(Lookup::RustModuleGlobsParent, &current);
                self.globs_parent.contains(&current)
            } && visited.insert(current.clone())
            {
                reads::record(Lookup::RustModuleParents, &current);
                let (parent, parent_scope) = self.parents.get(&current)?.clone();
                if let Some(found) = self
                    .resolve_module(&parent, &parent_scope, module_path)
                    .and_then(target)
                {
                    return Some(found);
                }
                current = parent;
            }
            None
        };
        lookup(path, &[]).or_else(|| {
            let (type_name, module_path) = path.split_last()?;
            if module_path.is_empty() {
                return None;
            }
            lookup(module_path, std::slice::from_ref(type_name))
        })
    }
}

/// The inline module path a caller is declared in, from its declaration in
/// the file's items; without one, the leading module segments of its name.
fn rust_caller_module_scope(items: &[RustApiItem], caller_symbol: &str, line: u32) -> Vec<String> {
    let name = caller_symbol.rsplit("::").next().unwrap_or(caller_symbol);
    let declared_before = items
        .iter()
        .filter(|item| item.name == name && item.line <= line)
        .collect::<Vec<_>>();
    // Prefer the declaration whose qualified name the caller's name matches.
    if let Some(item) = declared_before
        .iter()
        .filter(|item| rust_qualified_item_name(item).ends_with(caller_symbol))
        .max_by_key(|item| item.line)
        .or_else(|| declared_before.iter().max_by_key(|item| item.line))
    {
        return item.scope.clone();
    }
    let mut scope = rust_path_segments(caller_symbol);
    scope.pop();
    if scope.iter().any(|segment| {
        segment.contains(' ') || segment.chars().next().is_some_and(char::is_uppercase)
    }) {
        // An impl name (`Type::method`, `Trait for Type::method`) is not a
        // module path.
        return Vec::new();
    }
    scope
}

/// Imports in scope for resolving paths written in `file`: its own, plus its
/// parent module's when it glob-imports the parent (`use super::*;`, the usual
/// first line of an out-of-line `tests.rs`).
fn rust_imports_in_scope(
    file: &str,
    own_imports: &[RawImportContribution],
    globs_parent_module: bool,
    rust_imports_by_file: &Tracked<Index<Vec<RawImportContribution>>>,
) -> Vec<RawImportContribution> {
    let mut imports = own_imports.to_vec();
    if globs_parent_module {
        if let Some(parent_imports) = rust_parent_module_files(file)
            .iter()
            .find_map(|parent| rust_imports_by_file.get(parent))
        {
            imports.extend(parent_imports.iter().cloned());
        }
    }
    imports
}

/// Files that can declare the module `file` belongs to. `x/y/z.rs` is module
/// `z` of `x/y` (declared in `x/y.rs` or `x/y/mod.rs`, or a crate root there);
/// `x/y/mod.rs` is module `y` of `x`.
fn rust_parent_module_files(file: &str) -> Vec<String> {
    let normalized = file.replace('\\', "/");
    let Some((dir, name)) = normalized.rsplit_once('/') else {
        return Vec::new();
    };
    let module_dir = if name == "mod.rs" {
        match dir.rsplit_once('/') {
            Some((parent, _)) => parent.to_string(),
            None => return Vec::new(),
        }
    } else {
        dir.to_string()
    };
    vec![
        format!("{module_dir}.rs"),
        format!("{module_dir}/mod.rs"),
        format!("{module_dir}/lib.rs"),
        format!("{module_dir}/main.rs"),
    ]
}

/// Liveness edges for a Rust path call (`module::f()`, `Self::f()`,
/// `Type::f()`) that the callgraph left unresolved.
///
/// The callgraph keeps such a call as its bare path text, which never names a
/// definition, so the callee looked dead even though it is plainly called.
/// This resolves the path the way rustc would for the common shapes, from the
/// same per-file facts the macro-token resolver uses:
/// - `Self::f` names an associated function of a type in the caller's file;
/// - `Type::f` names `f` in a file that exports `Type` (unknown types such as
///   `String` match nothing, so std calls add no edges);
/// - `a::b::f` is resolved as a module path: child modules of the caller first
///   (Rust 2018 paths are relative to the current module), then `use` aliases
///   including `use crate::a::{self, ..}`, then crate-relative.
///
/// Every resolved target gets an edge, so an ambiguous `Type::f` keeps all
/// matching definitions live rather than guessing one.
fn rust_unresolved_path_call_edges(
    call: &CallgraphOutboundCall,
    caller_file: &str,
    rust_imports: &[RawImportContribution],
    test_origin: bool,
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    files_by_exported_symbol: &Tracked<Index<BTreeSet<String>>>,
    modules: &RustModuleIndex,
    caller_items: &[RustApiItem],
) -> Vec<InternalCall> {
    if language_for_file(caller_file) != "rust" {
        return Vec::new();
    }
    let (target, _) = split_call_target_metadata(&call.target);
    // Generic arguments (`Vec::<u8>::new`, `Foo::<T>::bar`) are not part of
    // the item path.
    let segments = rust_path_segments(target)
        .into_iter()
        .filter(|segment| !segment.starts_with('<'))
        .collect::<Vec<_>>();
    let Some((name, path)) = segments.split_last() else {
        return Vec::new();
    };
    let Some(qualifier) = path.last() else {
        return Vec::new();
    };
    if !rust_macro_text_is_identifier(name) {
        return Vec::new();
    }

    let exports_symbol = |file: &str, symbol: &str| {
        exported_symbols_by_file
            .get(file)
            .is_some_and(|symbols| symbols.contains(symbol))
    };
    let mut targets = BTreeSet::new();
    let mut scope = call
        .caller_symbol
        .split("::")
        .map(str::to_string)
        .collect::<Vec<_>>();
    scope.pop();
    loop {
        let mut candidate = scope.clone();
        candidate.extend(segments.iter().cloned());
        let qualified = candidate.join("::");
        if exports_symbol(caller_file, &qualified) {
            targets.insert((caller_file.to_string(), qualified));
            break;
        }
        if scope.pop().is_none() {
            break;
        }
    }
    if qualifier == "Self" {
        if exports_symbol(caller_file, name) {
            targets.insert((caller_file.to_string(), name.clone()));
        }
    } else if rust_macro_name_is_upper_camel(qualifier) {
        for file in files_by_exported_symbol
            .get(qualifier.as_str())
            .into_iter()
            .flatten()
        {
            if exports_symbol(file, name) {
                targets.insert((file.clone(), name.clone()));
            }
        }
    } else {
        targets.extend(resolve_macro_module_targets(
            caller_file,
            path,
            name,
            rust_imports,
            exported_symbols_by_file,
        ));
    }
    if targets.is_empty() && qualifier != "Self" {
        targets.extend(modules.resolve_item(
            caller_file,
            &rust_caller_module_scope(caller_items, &call.caller_symbol, call.line),
            path,
            name,
            exported_symbols_by_file,
        ));
    }

    targets
        .into_iter()
        .map(|(file, symbol)| InternalCall {
            caller_symbol: call.caller_symbol.clone(),
            file,
            symbol,
            line: call.line,
            provenance: RUST_PATH_LIVENESS_PROVENANCE.to_string(),
            test_origin: Some(test_origin),
        })
        .collect()
}

/// Liveness edges for source references found outside the stored callgraph,
/// each labelled with the test status `test_origin` gives its line.
fn resolve_macro_token_liveness_edges(
    _project_root: &Path,
    caller_file: &str,
    refs: &[MacroTokenRefContribution],
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    modules: &RustModuleIndex,
    caller_items: &[RustApiItem],
    test_origin: &dyn Fn(u32) -> bool,
) -> Vec<InternalCall> {
    let mut calls = Vec::new();
    for reference in refs {
        let Some((file, symbol)) = resolve_macro_token_ref_target(
            caller_file,
            reference,
            rust_imports,
            exported_symbols_by_file,
        )
        .or_else(|| {
            modules.resolve_item(
                caller_file,
                &rust_caller_module_scope(caller_items, &reference.caller_symbol, reference.line),
                reference.path.as_deref().unwrap_or(&[]),
                &reference.name,
                exported_symbols_by_file,
            )
        }) else {
            continue;
        };
        calls.push(InternalCall {
            caller_symbol: reference.caller_symbol.clone(),
            file,
            symbol,
            line: reference.line,
            provenance: MACRO_TOKEN_LIVENESS_PROVENANCE.to_string(),
            test_origin: Some(test_origin(reference.line)),
        });
    }
    sort_dedup_internal_calls(&mut calls);
    calls
}

fn resolve_macro_token_ref_target(
    caller_file: &str,
    reference: &MacroTokenRefContribution,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
) -> Option<ExportNode> {
    let path = reference.path.as_deref().unwrap_or(&[]);
    if reference.shape == RUST_MACRO_REF_SHAPE_CALL
        && path.first().is_none_or(|part| part != "crate")
    {
        let mut scope = reference
            .caller_symbol
            .split("::")
            .map(str::to_string)
            .collect::<Vec<_>>();
        scope.pop();
        let mut path = path;
        while let Some(first) = path.first() {
            match first.as_str() {
                "self" => {
                    path = &path[1..];
                    break;
                }
                "super" => {
                    scope.pop();
                    path = &path[1..];
                }
                _ => break,
            }
        }
        scope.extend(path.iter().cloned());
        scope.push(reference.name.clone());
        if let Some(target) =
            exported_symbol_target(caller_file, &scope.join("::"), exported_symbols_by_file)
        {
            return Some(target);
        }
    }
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
) -> BTreeSet<ExportNode> {
    let mut targets = BTreeSet::new();
    for candidate in rust_macro_module_path_candidates(module_path, rust_imports) {
        let segment_refs = candidate.iter().map(String::as_str).collect::<Vec<_>>();
        for resolved_segments in rust_resolve_segment_candidates(caller_file, &segment_refs) {
            let Some(file) = rust_file_for_segments_from_contributions(
                caller_file,
                &resolved_segments,
                exported_symbols_by_file,
            ) else {
                continue;
            };
            if let Some(target) =
                exported_symbol_target(&file, target_symbol, exported_symbols_by_file)
            {
                targets.insert(target);
                break;
            }
        }
    }
    targets
}

fn imported_macro_targets_for_local(
    caller_file: &str,
    local_name: &str,
    rust_imports: &[RawImportContribution],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
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
            for resolved_segments in rust_resolve_segment_candidates(caller_file, &segment_refs) {
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
                    break;
                }
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
            for (local_name, mut import_segments) in rust_import_module_alias_segments(import) {
                if &local_name == first {
                    import_segments.extend(path[1..].iter().cloned());
                    push_unique_macro_path_candidate(&mut candidates, import_segments);
                }
            }
        }
    }
    push_unique_macro_path_candidate(&mut candidates, path.to_vec());
    candidates
}

/// Module aliases a `use` declaration brings into scope: `use a::b;` and
/// `use a::b as c;` bind one module name, and a use list binds `self` (the
/// prefix module itself) plus each lowercase entry, so
/// `use crate::pins::{self, PinError};` makes `pins::f()` resolvable.
/// Upper-camel names are types, not modules, and are skipped.
fn rust_import_module_alias_segments(import: &RawImportContribution) -> Vec<(String, Vec<String>)> {
    let path = import.source.trim().trim_end_matches(';').trim();
    if path.contains('*') {
        return Vec::new();
    }
    if let Some((prefix, rest)) = path.split_once("::{") {
        if rest.contains('{') {
            return Vec::new();
        }
        let prefix_segments = rust_path_segments(prefix);
        return rest
            .trim_end_matches('}')
            .split(',')
            .filter_map(|specifier| {
                let specifier = specifier.trim();
                let (path_without_alias, alias) = specifier
                    .split_once(" as ")
                    .map(|(left, right)| (left.trim(), Some(right.trim())))
                    .unwrap_or((specifier, None));
                if path_without_alias == "self" {
                    let local_name =
                        alias.or_else(|| prefix_segments.last().map(String::as_str))?;
                    return Some((local_name.to_string(), prefix_segments.clone()));
                }
                let mut segments = prefix_segments.clone();
                segments.extend(rust_path_segments(path_without_alias));
                let local_name = alias.or_else(|| segments.last().map(String::as_str))?;
                (!local_name.is_empty()
                    && local_name != "_"
                    && !rust_macro_name_is_upper_camel(local_name))
                .then(|| (local_name.to_string(), segments.clone()))
            })
            .collect();
    }
    if path.contains('{') {
        return Vec::new();
    }
    let (path_without_alias, alias) = path
        .split_once(" as ")
        .map(|(left, right)| (left.trim(), Some(right.trim())))
        .unwrap_or((path, None));
    let segments = rust_path_segments(path_without_alias);
    let Some(local_name) = alias.or_else(|| segments.last().map(String::as_str)) else {
        return Vec::new();
    };
    if rust_macro_name_is_upper_camel(local_name) {
        return Vec::new();
    }
    vec![(local_name.to_string(), segments)]
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

fn push_unique_macro_path_candidate(candidates: &mut Vec<Vec<String>>, candidate: Vec<String>) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

/// Module paths a Rust path's leading segments can denote, most likely first.
/// `crate`, `self` and `super` are unambiguous. A bare first segment is, in
/// Rust 2018, resolved relative to the current module (a child module such as
/// `mod registry;` used as `registry::f()`); the older crate-relative reading
/// (a sibling of a top-level module) is kept as a second candidate.
fn rust_resolve_segment_candidates(caller_file: &str, segments: &[&str]) -> Vec<Vec<String>> {
    if segments.is_empty() {
        return vec![Vec::new()];
    }
    let caller_segments = rust_module_segments_for_rel(caller_file);
    let rest = |from: usize| segments[from..].iter().map(|item| (*item).to_string());
    match segments[0] {
        "crate" => vec![rest(1).collect()],
        "self" => {
            let mut resolved = caller_segments;
            resolved.extend(rest(1));
            vec![resolved]
        }
        "super" => {
            let mut resolved = caller_segments;
            resolved.pop();
            resolved.extend(rest(1));
            vec![resolved]
        }
        _ => {
            let mut child = caller_segments.clone();
            child.extend(rest(0));
            let mut sibling = caller_segments;
            sibling.pop();
            sibling.extend(rest(0));
            if child == sibling {
                vec![child]
            } else {
                vec![child, sibling]
            }
        }
    }
}

fn rust_file_for_segments_from_contributions(
    caller_file: &str,
    segments: &[String],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
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

fn exported_symbol_target(
    file: &str,
    symbol: &str,
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
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

/// Every file named by some contribution's `#[cfg(test)] mod` declarations.
fn cfg_test_module_files(contributions: &[DeadCodeContribution]) -> BTreeSet<String> {
    contributions
        .iter()
        .flat_map(|contribution| contribution.cfg_test_module_files.iter().cloned())
        .collect()
}

/// Whether `file` is test code: a test-tree path (tests, fixtures, mocks) or
/// a module file that is only compiled under `#[cfg(test)]`.
pub(crate) fn is_test_code_file(file: &str, cfg_test_module_files: &BTreeSet<String>) -> bool {
    is_test_tree_file(file) || cfg_test_module_files.contains(file)
}

fn resolve_raw_reexport_liveness_edges(
    project_root: &Path,
    file_name: &str,
    raw_reexports: &[RawReexportContribution],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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
                    true,
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
                    false,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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
            true,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
    require_declared_export: bool,
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
            // A Rust `pub use` / `pub(crate) use` is itself the export: the
            // callgraph records no definition for the re-exported name in the
            // re-exporting file, so requiring one would drop every such edge
            // and leave the item dead even when callers import it through
            // the re-export.
            if imported.is_empty()
                || exported.is_empty()
                || (require_declared_export
                    && !file_exports_symbol(file_name, exported, exported_symbols_by_file))
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

fn rust_export_identity(export: &ExportContribution, facts: Option<&RustApiFacts>) -> String {
    if export.kind == "function" {
        if let Some(item) = facts
            .into_iter()
            .flat_map(|facts| &facts.items)
            .filter(|item| {
                item.owner.is_none() && item.name == export.symbol && item.line >= export.line
            })
            .min_by_key(|item| item.line)
        {
            return rust_qualified_item_name(item);
        }
    }
    export.symbol.clone()
}

/// The graph node of a method named with its impl, such as `Config::production`
/// or `Display for Config::fmt` (how source scans name a method, optionally
/// behind its module path). The stored callgraph and the export rows name a
/// method by its own name only. When exactly one impl in this file (one type
/// and trait) defines a method with that name, both spellings denote the same
/// declaration, so it maps to the short name and the method has one node.
/// Same-named methods of several types keep their separate impl-qualified
/// nodes.
///
/// The name must denote that method and nothing else. A module path written
/// before the type must be the impl's own module. A free function whose
/// module-qualified name ends with the same path (`mod Config { fn
/// production() }` beside `impl Config { fn production() }` in another
/// module) may be what the name means, so the name keeps its own node. For a
/// caller, `caller_line` (a line inside the caller's body) decides instead:
/// the caller is the nearest same-named declaration starting at or before
/// it, and only that impl's method maps. Anything else returns `None`.
fn rust_method_node(
    items: &[RustApiItem],
    symbol: &str,
    caller_line: Option<u32>,
) -> Option<String> {
    let (prefix, name) = symbol.rsplit_once("::")?;
    let type_name = |path: &str| {
        let path = path.split('<').next().unwrap_or(path).trim();
        path.rsplit("::").next().unwrap_or(path).to_string()
    };
    // The module path written before the type; trait-form names carry none
    // that can be told apart from the trait's own path.
    let (trait_name, owner, module_path) = match prefix.rsplit_once(" for ") {
        Some((trait_path, owner)) => (Some(type_name(trait_path)), type_name(owner), None),
        None => {
            let path = prefix.split('<').next().unwrap_or(prefix).trim();
            let mut segments = path.split("::").collect::<Vec<_>>();
            segments.pop();
            (None, type_name(prefix), Some(segments))
        }
    };
    let impls = items
        .iter()
        .filter(|item| item.name == name && item.owner.is_some())
        .map(|item| (&item.owner, &item.trait_name))
        .collect::<BTreeSet<_>>();
    // A free function with the method's name already owns the short node.
    if impls.len() != 1
        || items
            .iter()
            .any(|item| item.owner.is_none() && item.scope.is_empty() && item.name == name)
    {
        return None;
    }
    let is_the_method = |item: &RustApiItem| {
        item.name == name
            && item
                .owner
                .as_deref()
                .is_some_and(|item_owner| type_name(item_owner) == owner)
            && trait_name.as_ref().is_none_or(|trait_name| {
                item.trait_name
                    .as_deref()
                    .is_some_and(|item_trait| &type_name(item_trait) == trait_name)
            })
            && module_path.as_ref().is_none_or(|path| {
                path.is_empty()
                    || path
                        .iter()
                        .copied()
                        .eq(item.scope.iter().map(String::as_str))
            })
    };
    if let Some(line) = caller_line {
        if let Some(declaration) = items
            .iter()
            .filter(|item| item.name == name && item.line <= line)
            .max_by_key(|item| item.line)
        {
            return is_the_method(declaration).then(|| name.to_string());
        }
    }
    let symbol_path = symbol.split("::").collect::<Vec<_>>();
    if items.iter().any(|item| {
        item.owner.is_none()
            && item
                .scope
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(item.name.as_str()))
                .collect::<Vec<_>>()
                .ends_with(&symbol_path)
    }) {
        return None;
    }
    items.iter().any(is_the_method).then(|| name.to_string())
}

fn rust_function_identity(
    items: &[RustApiItem],
    symbol: &str,
    caller_line: Option<u32>,
    caller: Option<&str>,
) -> String {
    if symbol.contains("::") {
        return symbol.to_string();
    }
    let candidates = items
        .iter()
        .filter(|item| item.owner.is_none() && item.name == symbol)
        .collect::<Vec<_>>();
    if let Some(line) = caller_line {
        if let Some(item) = candidates
            .iter()
            .filter(|item| item.line <= line)
            .max_by_key(|item| item.line)
        {
            return rust_qualified_item_name(item);
        }
    }
    if let Some(caller) = caller {
        let mut scope = caller.split("::").collect::<Vec<_>>();
        scope.pop();
        loop {
            let qualified = scope
                .iter()
                .copied()
                .chain(std::iter::once(symbol))
                .collect::<Vec<_>>()
                .join("::");
            if candidates
                .iter()
                .any(|item| rust_qualified_item_name(item) == qualified)
            {
                return qualified;
            }
            if scope.pop().is_none() {
                break;
            }
        }
    }
    let identities = candidates
        .into_iter()
        .map(rust_qualified_item_name)
        .collect::<BTreeSet<_>>();
    if identities.len() == 1 {
        identities.into_iter().next().unwrap()
    } else {
        symbol.to_string()
    }
}

// A module's private name table is used to resolve a path, but only its public
// table can escape through a public module or a glob. Items, not files, become
// liveness roots, so re-exporting one function never rescues its siblings.
type RustApiModuleId = (String, Vec<String>);
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RustApiTarget {
    Module(RustApiModuleId),
    Item(String, usize),
}
type RustApiTable = BTreeMap<RustApiModuleId, BTreeMap<String, BTreeSet<RustApiTarget>>>;

fn rust_public_api_items(
    contributions: &[DeadCodeContribution],
    library_roots: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<(String, u32)>> {
    let facts = contributions
        .iter()
        .filter_map(|c| c.rust_api.as_ref().map(|facts| (c.file.clone(), facts)))
        .collect::<BTreeMap<_, _>>();
    let files = facts.keys().cloned().collect::<BTreeSet<_>>();
    let mut names = RustApiTable::new();
    let mut public = RustApiTable::new();
    let mut parents = BTreeMap::<RustApiModuleId, RustApiModuleId>::new();
    for (file, facts) in &facts {
        names.entry((file.clone(), vec![])).or_default();
        for module in &facts.modules {
            let parent = (file.clone(), module.scope.clone());
            let child = if let Some(path) = &module.file {
                // Manifest-named crate roots have children beside the root,
                // not under a directory named after its arbitrary file stem.
                // Library root identity comes from the entry-point resolver.
                let path = if library_roots.contains(file) && module.path_override.is_none() {
                    let mut scope = module.scope.clone();
                    scope.push(module.name.clone());
                    Path::new(file)
                        .parent()
                        .unwrap_or(Path::new(""))
                        .join(scope.join("/"))
                        .with_extension("rs")
                        .to_string_lossy()
                        .replace('\\', "/")
                } else {
                    path.clone()
                };
                let nested = format!("{}/mod.rs", path.trim_end_matches(".rs"));
                if files.contains(&path) {
                    (path.clone(), vec![])
                } else {
                    (nested, vec![])
                }
            } else {
                let mut scope = module.scope.clone();
                scope.push(module.name.clone());
                (file.clone(), scope)
            };
            parents.insert(child.clone(), parent.clone());
            names.entry(child.clone()).or_default();
            let target = RustApiTarget::Module(child);
            names
                .entry(parent.clone())
                .or_default()
                .entry(module.name.clone())
                .or_default()
                .insert(target.clone());
            if module.public {
                public
                    .entry(parent)
                    .or_default()
                    .entry(module.name.clone())
                    .or_default()
                    .insert(target);
            }
        }
        for (index, item) in facts
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.owner.is_none())
        {
            let module = (file.clone(), item.scope.clone());
            let target = RustApiTarget::Item(file.clone(), index);
            names
                .entry(module.clone())
                .or_default()
                .entry(item.name.clone())
                .or_default()
                .insert(target.clone());
            if item.public {
                public
                    .entry(module)
                    .or_default()
                    .entry(item.name.clone())
                    .or_default()
                    .insert(target);
            }
        }
    }
    loop {
        let mut additions = Vec::new();
        for (file, facts) in &facts {
            for use_item in &facts.uses {
                let module = (file.clone(), use_item.scope.clone());
                if use_item.alias == "*" {
                    let path = &use_item.path[..use_item.path.len().saturating_sub(1)];
                    for target in rust_api_resolve(&module, path, &names, &parents) {
                        if let RustApiTarget::Module(target) = target {
                            for (name, targets) in public.get(&target).into_iter().flatten() {
                                additions.push((
                                    module.clone(),
                                    name.clone(),
                                    targets.clone(),
                                    use_item.public,
                                ));
                            }
                        }
                    }
                } else {
                    let targets = rust_api_resolve(&module, &use_item.path, &names, &parents);
                    additions.push((module, use_item.alias.clone(), targets, use_item.public));
                }
            }
        }
        let mut changed = false;
        for (module, name, targets, is_public) in additions {
            for target in targets {
                changed |= names
                    .entry(module.clone())
                    .or_default()
                    .entry(name.clone())
                    .or_default()
                    .insert(target.clone());
                if is_public {
                    changed |= public
                        .entry(module.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default()
                        .insert(target);
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut frontier = library_roots
        .iter()
        .filter(|file| facts.contains_key(*file))
        .map(|file| RustApiTarget::Module((file.clone(), vec![])))
        .collect::<Vec<_>>();
    let mut live = BTreeSet::new();
    while let Some(target) = frontier.pop() {
        if !live.insert(target.clone()) {
            continue;
        }
        if let RustApiTarget::Module(module) = target {
            frontier.extend(
                public
                    .get(&module)
                    .into_iter()
                    .flat_map(|names| names.values())
                    .flatten()
                    .cloned(),
            );
        }
    }
    let mut result = BTreeMap::<String, BTreeSet<(String, u32)>>::new();
    // Include even private modules belonging to a library, so their trait
    // impls do not fall back to the executable's conservative dispatch roots.
    let mut modules = library_roots
        .iter()
        .filter(|file| facts.contains_key(*file))
        .map(|file| (file.clone(), vec![]))
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(module) = modules.pop() {
        if !visited.insert(module.clone()) {
            continue;
        }
        result.entry(module.0.clone()).or_default();
        for target in names
            .get(&module)
            .into_iter()
            .flat_map(|names| names.values())
            .flatten()
        {
            if let RustApiTarget::Module(target) = target {
                modules.push(target.clone());
            }
        }
    }
    // Some projections carry a synthetic export for the `pub use` binding
    // itself. Its visibility belongs to the re-exporting module, even though
    // the definition (and its executable body) lives in the source module.
    for (file, facts) in &facts {
        for use_item in &facts.uses {
            let module = (file.clone(), use_item.scope.clone());
            if use_item.public
                && use_item.alias != "*"
                && live.contains(&RustApiTarget::Module(module.clone()))
                && rust_api_resolve(&module, &use_item.path, &names, &parents)
                    .iter()
                    .any(|target| live.contains(target))
            {
                result
                    .entry(file.clone())
                    .or_default()
                    .insert((use_item.alias.clone(), use_item.line));
            }
        }
    }
    for (file, facts) in &facts {
        for (index, item) in facts.items.iter().enumerate() {
            let module = (file.clone(), item.scope.clone());
            let reachable = if let Some(owner) = &item.owner {
                item.public
                    && rust_api_resolve(&module, &rust_path_segments(owner), &names, &parents)
                        .iter()
                        .any(|target| live.contains(target))
                    && item.trait_name.as_ref().is_none_or(|trait_name| {
                        rust_api_resolve(&module, &rust_path_segments(trait_name), &names, &parents)
                            .iter()
                            .any(|target| live.contains(target))
                    })
            } else {
                item.public && live.contains(&RustApiTarget::Item(file.clone(), index))
            };
            if reachable {
                result
                    .entry(file.clone())
                    .or_default()
                    .insert((item.name.clone(), item.line));
            }
        }
    }
    result
}

fn rust_file_in_library(file: &str, public: &Tracked<Index<BTreeSet<(String, u32)>>>) -> bool {
    public.contains_key(file)
}

fn rust_item_is_public_api(
    contribution: &DeadCodeContribution,
    export: &ExportContribution,
    public: &Tracked<Index<BTreeSet<(String, u32)>>>,
) -> bool {
    let Some(facts) = &contribution.rust_api else {
        return false;
    };
    let name = symbol_liveness_name(&export.symbol);
    // Parser ranges can start at doc comments or attributes before the item.
    let item = facts
        .items
        .iter()
        .filter(|item| item.name == name && item.line >= export.line)
        .min_by_key(|item| item.line)
        .or_else(|| facts.items.iter().find(|item| item.name == name));
    if item.is_none() {
        return public.get(&contribution.file).is_some_and(|items| {
            items
                .iter()
                .any(|(name, line)| name == &export.symbol && *line >= export.line)
        });
    }
    item.is_some_and(|item| {
        public
            .get(&contribution.file)
            .is_some_and(|items| items.contains(&(item.name.clone(), item.line)))
    })
}

fn rust_api_resolve(
    module: &RustApiModuleId,
    path: &[String],
    names: &RustApiTable,
    parents: &BTreeMap<RustApiModuleId, RustApiModuleId>,
) -> BTreeSet<RustApiTarget> {
    let mut module = module.clone();
    let mut path = path;
    if path.first().is_some_and(|s| s == "crate") {
        let mut visited = BTreeSet::new();
        while let Some(parent) = parents.get(&module) {
            if !visited.insert(module.clone()) {
                return BTreeSet::new();
            }
            module = parent.clone();
        }
        path = &path[1..];
    } else {
        while path.first().is_some_and(|s| s == "super" || s == "self") {
            if path[0] == "super" {
                if let Some(parent) = parents.get(&module) {
                    module = parent.clone();
                }
            }
            path = &path[1..];
        }
    }
    let mut targets = BTreeSet::from([RustApiTarget::Module(module)]);
    for name in path {
        targets = targets
            .into_iter()
            .filter_map(|target| match target {
                RustApiTarget::Module(module) => Some(module),
                _ => None,
            })
            .flat_map(|module| {
                names
                    .get(&module)
                    .and_then(|names| names.get(name))
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
    }
    targets
}

fn go_interface_live_methods(
    contributions: &[DeadCodeContribution],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut used = BTreeMap::<String, BTreeSet<String>>::new();
    let mut methods = BTreeMap::<(String, String), BTreeSet<String>>::new();
    let mut interfaces = BTreeMap::<String, Vec<Vec<String>>>::new();
    for contribution in contributions {
        let Some(facts) = &contribution.go_dispatch else {
            continue;
        };
        let package = Path::new(&contribution.file)
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .to_string();
        used.entry(package.clone())
            .or_default()
            .extend(facts.used_types.iter().cloned());
        interfaces
            .entry(facts.module.clone())
            .or_default()
            .extend(facts.interfaces.iter().cloned());
        for method in &facts.methods {
            methods
                .entry((package.clone(), method.receiver.clone()))
                .or_default()
                .insert(method.signature.clone());
        }
    }
    // Well-known implicit contracts, including the existing sort and list
    // dispatch set. Name-only arbitrary external interfaces are not guessed.
    let well_known = [
        vec!["Len()(int)", "Less(int,int)(bool)", "Swap(int,int)()"],
        vec!["FilterValue()(string)"],
        vec!["Read([]byte)(int,error)"],
        vec!["Write([]byte)(int,error)"],
        vec!["Close()(error)"],
        vec!["String()(string)"],
        vec!["Error()(string)"],
    ]
    .into_iter()
    .map(|methods| methods.into_iter().map(str::to_string).collect::<Vec<_>>())
    .collect::<Vec<_>>();
    let mut result = BTreeMap::<String, BTreeSet<String>>::new();
    for contribution in contributions {
        let Some(facts) = &contribution.go_dispatch else {
            continue;
        };
        let package = Path::new(&contribution.file)
            .parent()
            .unwrap_or(Path::new(""))
            .to_string_lossy()
            .to_string();
        for method in &facts.methods {
            if !used
                .get(&package)
                .is_some_and(|types| types.contains(&method.receiver))
            {
                continue;
            }
            let implemented = &methods[&(package.clone(), method.receiver.clone())];
            if interfaces
                .get(&facts.module)
                .into_iter()
                .flatten()
                .chain(well_known.iter())
                .any(|interface| {
                    interface
                        .iter()
                        .all(|signature| implemented.contains(signature))
                        && interface.contains(&method.signature)
                })
            {
                result
                    .entry(contribution.file.clone())
                    .or_default()
                    .insert(method.name.clone());
            }
        }
    }
    result
}

fn resolve_raw_imported_export_liveness_roots(
    project_root: &Path,
    file_name: &str,
    raw_imports: &[RawImportContribution],
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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

fn reexport_edges_for_all_target_symbols(
    project_root: &Path,
    file_name: &str,
    namespace_export: &str,
    module_entry: &Path,
    line: u32,
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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

fn file_exports_symbol(
    file_name: &str,
    symbol: &str,
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
) -> bool {
    exported_symbols_by_file
        .get(file_name)
        .is_some_and(|symbols| symbols.contains(symbol))
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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
    exported_symbols_by_file: &'a Tracked<Index<BTreeSet<String>>>,
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
    default_export_symbols_by_file: &Tracked<BTreeMap<String, String>>,
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
    exported_symbols_by_file: &Tracked<Index<BTreeSet<String>>>,
    files_by_exported_symbol: &Tracked<Index<BTreeSet<String>>>,
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
        if go_well_known_interface_methods_from_call(call).is_some() {
            // These are implicit interface uses, not explicit receiver calls.
            // The interface predicate checks the receiver and full method set.
            return Vec::new();
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
    // These library entry calls dispatch implicitly. Keep them out of the
    // explicit-name rescue; receiver-aware interface satisfaction handles them.
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
        // Names a public API file re-exports (`pub use inner::f;`) are part of
        // its API even though the file defines no item with that name.
        roots.extend(
            internal_calls
                .iter()
                .filter(|call| call.provenance == CALLGRAPH_PROVENANCE_REEXPORT)
                .map(|call| call.caller_symbol.clone()),
        );
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
    if symbol.contains("::") {
        return false;
    }
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

fn collect_freshness(file: &Path) -> FileFreshness {
    super::source_text::graph_source_freshness(file)
        .map(|(freshness, _permit)| freshness)
        .unwrap_or_else(|_| FileFreshness {
            mtime: UNIX_EPOCH,
            size: 0,
            content_hash: cache_freshness::zero_hash(),
        })
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        std::borrow::Cow::Borrowed(path)
    } else {
        std::borrow::Cow::Owned(project_root.join(path))
    };
    let paths = current_canonical_paths();
    if let Some(paths) = &paths {
        if let Some(relative) = paths
            .relative
            .read()
            .get(project_root)
            .and_then(|files| files.get(absolute.as_ref()))
        {
            return relative.clone();
        }
    }
    let normalized_root = canonicalize_normalized(project_root);
    let normalized = canonicalize_normalized(&absolute);
    let relative = normalized
        .strip_prefix(&normalized_root)
        .unwrap_or(normalized.as_path())
        .to_string_lossy()
        .replace('\\', "/");
    if let Some(paths) = paths {
        paths
            .relative
            .write()
            .entry(project_root.to_path_buf())
            .or_default()
            .insert(absolute.into_owned(), relative.clone());
    }
    relative
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
    crate::path_identity::normalize_path(path)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeadCodeContribution {
    #[serde(default)]
    excluded_from_graph: bool,
    // This marker belongs only to the in-memory rollup, never cached JSON.
    #[serde(skip)]
    decode_failed: bool,
    #[serde(skip)]
    go_initializers: Option<GoInitializerContribution>,
    file: String,
    #[serde(default)]
    rust_api: Option<RustApiFacts>,
    #[serde(default)]
    go_dispatch: Option<GoDispatchFacts>,
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
    /// Files of out-of-line modules this file declares under `#[cfg(test)]`
    /// (`#[cfg(test)] mod tests;`). Each declaration lists both candidate
    /// paths (`name.rs` and `name/mod.rs`); only existing files ever match.
    #[serde(default)]
    cfg_test_module_files: Vec<String>,
    /// The file starts with a file-level `use super::*;`, so its parent
    /// module's imports are in scope for path resolution.
    #[serde(default)]
    globs_parent_module: bool,
    /// Names of methods defined in `impl Trait for Type` blocks. Rust code
    /// calls them through the trait (`Default::default()`, `.parse()`,
    /// formatting, drop glue), which the callgraph rarely sees, so they are
    /// liveness roots: what they call is used whenever the trait is.
    #[serde(default)]
    trait_impl_methods: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImportedExportContribution {
    file: String,
    symbol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportContribution {
    #[serde(default)]
    public_api: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InternalCallContribution {
    #[serde(default)]
    line: u32,
    #[serde(default)]
    caller_symbol: String,
    file: String,
    symbol: String,
    #[serde(default)]
    test_origin: Option<bool>,
}

impl DeadCodeContribution {
    /// Whether `line` of this file sits inside a `#[cfg(test)]` item, statement
    /// or block (see `rust_cfg_test_ranges`).
    /// The facts of this file the whole-project indexes are built from
    /// ([`MaterializeContext::new`] reads nothing else), kept so a later
    /// rollup can rebuild the indexes without reading every file's facts.
    fn index_skeleton(&self) -> DeadCodeContribution {
        DeadCodeContribution {
            file: self.file.clone(),
            exports: self.exports.clone(),
            rust_api: self.rust_api.clone(),
            go_dispatch: self.go_dispatch.clone(),
            oxc_facts: self.oxc_facts.clone(),
            cfg_test_module_files: self.cfg_test_module_files.clone(),
            globs_parent_module: self.globs_parent_module,
            rust_imports: self.rust_imports.clone(),
            type_ref_names: self.type_ref_names.clone(),
            ..Default::default()
        }
    }

    fn line_is_cfg_test(&self, line: u32) -> bool {
        self.cfg_test_ranges
            .iter()
            .any(|range| range.contains(line))
    }

    /// Whether code at `line` of this file is compiled only for tests: the
    /// whole file is test code, or the line is under `#[cfg(test)]`. Every
    /// liveness edge built from a source location takes its test origin from
    /// this one rule, so a test-only call never counts as production usage.
    fn location_is_test_code(
        &self,
        test_module_files: &Tracked<BTreeSet<String>>,
        line: u32,
    ) -> bool {
        is_test_tree_file(&self.file)
            || test_module_files.contains(&self.file)
            || self.line_is_cfg_test(line)
    }
}

impl From<InternalCall> for InternalCallContribution {
    fn from(call: InternalCall) -> Self {
        Self {
            line: call.line,
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
thread_local! {
    static AFTER_SOURCE_METADATA: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_ANALYSIS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static SORT_KEY_EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_origin_bulk_union_matches_set_union() {
        // Exhaustive small subsets cover equality, subset, overlap, empty and
        // append-only unions without computing expectations through the merger.
        for left in 0..64 {
            for right in 0..64 {
                let mut ids = (0..6)
                    .filter(|bit| left & (1 << bit) != 0)
                    .collect::<Vec<_>>();
                let labels = (0..6)
                    .filter(|bit| right & (1 << bit) != 0)
                    .collect::<Vec<_>>();
                let expected = (0..6)
                    .filter(|bit| (left | right) & (1 << bit) != 0)
                    .collect::<Vec<_>>();
                let changed = merge_test_origin_ids(&mut ids, &labels);
                assert_eq!(ids, expected);
                assert_eq!(changed, (right & !left) != 0);
            }
        }
    }

    fn uncached_relative_path(root: &Path, path: &Path) -> String {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let root = canonicalize_uncached(root);
        let path = canonicalize_uncached(&absolute);
        path.strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn relative_paths_resolve_once_per_job_across_parallel_items() {
        let (_temp, root, _) = canonical_fixture(&[("src/a.rs", "fn a() {}\n")]);
        let files = [
            root.join("src/a.rs"),
            PathBuf::from("src/a.rs"),
            root.join("src/missing.rs"),
        ];
        let requests = (0..64)
            .flat_map(|_| files.iter().cloned())
            .collect::<Vec<_>>();
        let expected = requests
            .iter()
            .map(|path| uncached_relative_path(&root, path))
            .collect::<Vec<_>>();
        let paths = Arc::new(CanonicalPaths::default());
        let actual = with_canonical_paths(Some(Arc::clone(&paths)), || {
            requests
                .par_iter()
                .map(|path| {
                    with_canonical_paths(Some(Arc::clone(&paths)), || relative_path(&root, path))
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        let resolved = paths.paths.read();
        assert_eq!(
            resolved.len(),
            3,
            "one root and two distinct absolute paths"
        );
        assert!(resolved.contains_key(&root));
        assert!(resolved.contains_key(&root.join("src/a.rs")));
        assert!(resolved.contains_key(&root.join("src/missing.rs")));
        assert!(current_canonical_paths().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_job_cache_preserves_aliases_and_rechecks_next_job() {
        let (temp, root, _) =
            canonical_fixture(&[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")]);
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let link = root.join("src/link.rs");
        std::os::unix::fs::symlink("a.rs", &link).unwrap();
        with_job_canonical_paths(|| {
            for path in [
                &link,
                &alias.join("src/link.rs"),
                &root.join("src/missing/../a.rs"),
            ] {
                assert_eq!(
                    relative_path(&alias, path),
                    uncached_relative_path(&alias, path)
                );
            }
            assert_eq!(relative_path(&alias, &link), "src/a.rs");
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink("b.rs", &link).unwrap();
            assert_eq!(
                with_job_canonical_paths(|| relative_path(&alias, &link)),
                "src/b.rs"
            );
            assert_eq!(
                relative_path(&alias, &link),
                "src/a.rs",
                "nested job restores its parent's memo"
            );
        });
        assert_eq!(
            with_job_canonical_paths(|| relative_path(&alias, &link)),
            "src/b.rs"
        );
    }

    #[test]
    fn cached_scan_keeps_contribution_and_aggregate_bytes() {
        let (_temp, root, files) = canonical_fixture(&[(
            "main.rs",
            "fn main() { helper(); }\nfn helper() {}\nfn unused() {}\n",
        )]);
        let graph = snapshot(
            files.clone(),
            vec![
                export(&root, "main.rs", "helper", "function"),
                export(&root, "main.rs", "unused", "function"),
            ],
            vec![outbound(&root, "main.rs", "main", "helper")],
        );
        let job = job(&root, files, graph);
        let before = run_dead_code_scan_body(&job, None, Instant::now())
            .outcome
            .unwrap();
        let after = run_dead_code_scan(&job).outcome.unwrap();
        assert_eq!(
            serde_json::to_vec(&before.aggregate).unwrap(),
            serde_json::to_vec(&after.aggregate).unwrap()
        );
        let payloads = |success: &InspectScanSuccess| {
            serde_json::to_vec(
                &success
                    .contributions
                    .iter()
                    .map(|row| &row.contribution)
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        assert_eq!(payloads(&before), payloads(&after));
    }

    #[test]
    #[ignore = "opt-in CPU measurement on the checkout containing the crate"]
    fn live_cpu_relative_paths_checkout_cost() {
        let root =
            fs::canonicalize(Path::new(crate::test_support::manifest_dir()).join("../..")).unwrap();
        let mut parser = crate::parser::FileParser::new();
        let mut requests: Vec<PathBuf> = Vec::new();
        let mut files = 0;
        for path in crate::callgraph::walk_project_files(&root) {
            if let Ok(symbols) = parser.extract_symbols(&path) {
                files += 1;
                requests.extend(symbols.iter().map(|_| path.clone()));
            }
        }
        let before = Instant::now();
        let expected = requests
            .par_iter()
            .map(|path| uncached_relative_path(&root, path))
            .collect::<Vec<_>>();
        let before_time = before.elapsed();
        let paths = Arc::new(CanonicalPaths::default());
        let after = Instant::now();
        let actual = requests
            .par_iter()
            .map(|path| {
                with_canonical_paths(Some(Arc::clone(&paths)), || relative_path(&root, path))
            })
            .collect::<Vec<_>>();
        let after_time = after.elapsed();
        assert_eq!(
            serde_json::to_vec(&expected).unwrap(),
            serde_json::to_vec(&actual).unwrap()
        );
        println!("relative paths: files={files} items={} before={before_time:?} realpath_calls={} after={after_time:?} realpath_calls={}", requests.len(), requests.len() * 2, paths.paths.read().len());
    }

    use std::fs;
    #[test]
    fn cached_dead_code_decode_gap_is_named_incomplete_and_repairable() {
        let (_temp, root, files) = fixture_project(&[
            ("src/good.rs", "pub fn good_unused() {}\n"),
            ("src/bad.rs", "pub fn bad_unused() {}\n"),
        ]);
        let snapshot = snapshot(
            files.clone(),
            vec![
                export(&root, "src/good.rs", "good_unused", "function"),
                export(&root, "src/bad.rs", "bad_unused", "function"),
            ],
            vec![],
        );
        let contributions = run_dead_code_scan(&job(&root, files.clone(), snapshot.clone()))
            .outcome
            .expect("valid scan")
            .contributions;
        let public = BTreeSet::new();
        let roles = crate::inspect::entry_points::ProjectRoles::default();
        let rollup = |facts: &[FileContribution]| {
            aggregate_dead_code_contributions_with_snapshot(
                &root, &snapshot, facts, &public, &roles, None,
            )
        };
        let valid = rollup(&contributions);
        assert_eq!(valid["complete"], true);
        assert_eq!(valid["count"], 2);
        let inspect_dir = root.join(".aft-cache/inspect");
        let writer =
            crate::inspect::cache::InspectCache::open(inspect_dir.clone(), root.clone()).unwrap();
        let key = JobKey::for_project_category(InspectCategory::DeadCode);

        for case in [
            "missing exports",
            "wrong exports and forged file",
            "wrong file",
        ] {
            let mut malformed = contributions.clone();
            let bad = malformed
                .iter_mut()
                .find(|item| item.contribution["file"] == "src/bad.rs")
                .unwrap();
            match case {
                "missing exports" => {
                    bad.contribution.as_object_mut().unwrap().remove("exports");
                }
                "wrong exports and forged file" => {
                    bad.contribution["exports"] = json!("invalid cached value");
                    bad.contribution["file"] = json!("forged.rs");
                }
                _ => {
                    bad.contribution["file"] = json!(42);
                }
            }
            assert_eq!(
                bad.contribution["facts_format_version"],
                DEAD_CODE_FACTS_FORMAT_VERSION
            );
            writer
                .store_tier2_result(key.clone(), &files, &malformed, valid.clone())
                .unwrap();
            let borrower = crate::inspect::cache::InspectCache::open_readonly(
                inspect_dir.clone(),
                root.clone(),
            )
            .unwrap()
            .unwrap();
            let rows = borrower
                .load_tier2_contributions(InspectCategory::DeadCode)
                .unwrap();
            let raw = rows
                .iter()
                .map(|row| row.contribution.clone())
                .collect::<Vec<_>>();
            let loaded = rows
                .into_iter()
                .map(|row| {
                    let path = if row.file_path.is_absolute() {
                        row.file_path
                    } else {
                        root.join(row.file_path)
                    };
                    FileContribution::new(row.category, path, row.freshness, row.contribution)
                        .with_type_ref_names(row.type_ref_names)
                })
                .collect::<Vec<_>>();
            let cold = rollup(&loaded);
            assert_eq!(cold["complete"], false, "{case}");
            assert_eq!(cold["scanned_files"], 2);
            assert_eq!(cold["count"], 1);
            assert!(aggregate_has_item(&cold, "src/good.rs", "good_unused"));
            assert!(!aggregate_has_item(&cold, "src/bad.rs", "bad_unused"));
            assert_eq!(
                cold["parse_errors"],
                json!([{
                    "file": "src/bad.rs",
                    "message": "cached dead-code contribution could not be decoded",
                }])
            );
            let repaired = rollup(&contributions);
            assert_eq!(repaired, valid, "repaired contribution: {case}");
            assert_eq!(
                borrower
                    .load_tier2_contributions(InspectCategory::DeadCode)
                    .unwrap()
                    .iter()
                    .map(|row| row.contribution.clone())
                    .collect::<Vec<_>>(),
                raw,
                "read-only rollup must not rewrite cached rows"
            );
        }
    }

    #[test]
    fn audit05_freshness_survives_missing_pre_read_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let source = "pub fn old() {}\n";
        let create_path = path.clone();
        AFTER_SOURCE_METADATA.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(create_path, source).unwrap();
            }))
        });
        let save_path = path.clone();
        AFTER_ANALYSIS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(save_path, "pub fn new() {}\n").unwrap();
            }))
        });
        let job = job(
            dir.path(),
            vec![path.clone()],
            snapshot(vec![], vec![], vec![]),
        );
        let contribution = gather_file_contribution(
            &job,
            &path,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            &mut DeadCodeFileAnalyzer::default(),
            FactSource::Parse,
        );
        assert_eq!(
            contribution.freshness.content_hash,
            cache_freshness::hash_bytes(source.as_bytes())
        );
        assert_eq!(contribution.freshness.mtime, std::time::UNIX_EPOCH);
        assert_eq!(
            cache_freshness::verify_file(&path, &contribution.freshness),
            crate::cache_freshness::FreshnessVerdict::Stale
        );
    }

    #[test]
    fn audit05_ranking_computes_one_key_per_item() {
        let items = (0..100)
            .rev()
            .map(|line| json!({"file": "src/file.rs", "line": line, "symbol": "function"}))
            .collect();
        SORT_KEY_EVALUATIONS.with(|count| count.set(0));
        let ranked = rank_dead_code_items(
            items,
            &crate::inspect::entry_points::ProjectRoles::default(),
        );
        assert_eq!(ranked[0]["line"], 0);
        assert_eq!(ranked[99]["line"], 99);
        assert_eq!(SORT_KEY_EVALUATIONS.with(|count| count.get()), 100);
    }

    #[test]
    fn audit05_freshness_hashes_the_analyzed_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let source = "pub fn old() {}\n";
        fs::write(&path, source).unwrap();
        let save_path = path.clone();
        AFTER_ANALYSIS.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(save_path, "pub fn new() {}\n").unwrap();
            }))
        });
        let job = job(
            dir.path(),
            vec![path.clone()],
            snapshot(vec![], vec![], vec![]),
        );
        let contribution = gather_file_contribution(
            &job,
            &path,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            &mut DeadCodeFileAnalyzer::default(),
            FactSource::Parse,
        );
        assert_eq!(
            contribution.freshness.content_hash,
            cache_freshness::hash_bytes(source.as_bytes())
        );
        assert_ne!(
            contribution.freshness.content_hash,
            cache_freshness::collect(&path).unwrap().content_hash
        );
    }

    #[test]
    fn audit05_oxc_contribution_freshness_uses_facts_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.ts");
        let source = "export const old = 1;\n";
        fs::write(&path, source).unwrap();
        let result = crate::inspect::oxc_engine::analyze_files(
            dir.path(),
            std::slice::from_ref(&path),
            Default::default(),
        )
        .unwrap();
        fs::write(&path, "export const new = 1;\n").unwrap();
        let job = job(
            dir.path(),
            vec![path.clone()],
            snapshot(vec![], vec![], vec![]),
        );
        let facts = result
            .facts
            .into_iter()
            .map(|facts| ("file.ts".to_string(), facts))
            .collect();
        let contribution = gather_file_contribution(
            &job,
            &path,
            &BTreeMap::new(),
            &facts,
            &BTreeMap::new(),
            &[],
            &mut DeadCodeFileAnalyzer::default(),
            FactSource::Parse,
        );
        assert_eq!(
            contribution.freshness.content_hash,
            cache_freshness::hash_bytes(source.as_bytes())
        );
        assert_eq!(
            cache_freshness::verify_file(&path, &contribution.freshness),
            crate::cache_freshness::FreshnessVerdict::Stale
        );
    }

    #[test]
    fn audit05_rust_impl_scope_includes_scoped_and_reference_types() {
        let source = "impl crate::Trait for &crate::Widget { fn run(&self) {} }";
        let tree = DeadCodeFileAnalyzer::default()
            .parse_source(LangId::Rust, source)
            .unwrap();
        let node = tree.root_node().named_child(0).unwrap();
        assert_eq!(
            rust_impl_scope_name(&node, source),
            "crate::Trait for &crate::Widget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn caller_comparison_keys_memoize_root_aliases_without_rewriting_calls() {
        // Canonicalize the fixture root so the explicit symlink is the only alias under test;
        // on macOS the temporary directory itself can also be aliased through /var.
        let (temp, root, _) = canonical_fixture(&[("src/a.rs", "pub fn run() {}\n")]);
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut calls = vec![
            outbound(&alias, "src/a.rs", "run", "helper"),
            outbound(&root, "src/a.rs", "run", "other"),
            outbound(&root, "src/a.rs", "run", "last"),
        ];
        calls[2].caller_file = PathBuf::from("src/a.rs");
        let original = calls
            .iter()
            .map(|call| call.caller_file.clone())
            .collect::<Vec<_>>();
        let grouped = group_outbound_calls_by_caller_file(&alias, &calls);
        assert_eq!(grouped.canonicalization_attempts, 1);
        assert_eq!(grouped.lexical_fallbacks, 0);
        assert_eq!(grouped.by_file["src/a.rs"].len(), 3);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.caller_file.clone())
                .collect::<Vec<_>>(),
            original
        );
    }

    #[test]
    fn caller_comparison_keys_keep_and_count_missing_file_edges() {
        let (_temp, root, _) = fixture_project(&[("src/present.rs", "pub fn run() {}\n")]);
        let calls = vec![
            outbound(&root, "src/missing.rs", "run", "helper"),
            outbound(&root, "src/missing.rs", "run", "other"),
        ];
        let grouped = group_outbound_calls_by_caller_file(&root, &calls);
        assert_eq!(grouped.canonicalization_attempts, 1);
        assert_eq!(grouped.lexical_fallbacks, 1);
        assert_eq!(grouped.by_file["src/missing.rs"].len(), 2);
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

    #[test]
    fn dispatch_index_does_not_materialize_names_per_file() {
        // The defect this guards: every non-Go file used to receive a copy of
        // its language's whole dispatched-name set, so the index cost
        // files x names. With 400 files and 400 names that is 160,000 copied
        // strings. The index must instead hold one entry per file that points
        // at the language's own set.
        const FILES: usize = 400;
        const NAMES: usize = 400;
        let contributions = (0..FILES)
            .map(|index| {
                let extension = if index % 2 == 0 { "ts" } else { "py" };
                dispatch_contribution(&format!("src/file_{index}.{extension}"), &[])
            })
            .collect::<Vec<_>>();
        let names = (0..NAMES)
            .map(|index| format!("method_{index}"))
            .collect::<BTreeSet<_>>();
        let dispatched_method_names = MethodNamesByLanguage::from([
            ("typescript".to_string(), names.clone()),
            ("python".to_string(), names),
        ]);

        let (index, allocations) = crate::test_allocations::count(|| {
            dispatch_live_source_names_by_file(&contributions, &dispatched_method_names)
        });

        assert_eq!(index.len(), FILES, "one entry per contributing file");
        // Allocation count, not stored-name count: this catches any copy of the
        // names, whatever type ends up holding it. Map nodes alone are a small
        // fraction of the file count; one copied set is already NAMES strings.
        assert!(
            allocations < FILES,
            "building the index allocated {allocations} times for {FILES} files; \
             it must stay O(files), not O(files x names)"
        );
        // Every non-Go entry must be the language's own set, not an equal copy.
        for (file, entry) in &index {
            let language = language_for_file(file);
            let DispatchNamesForFile::Language(borrowed) = entry else {
                panic!("{file} holds its own name set instead of borrowing {language}'s");
            };
            assert!(
                std::ptr::eq(*borrowed, &dispatched_method_names[language]),
                "{file} does not borrow {language}'s dispatched-name set"
            );
        }
        // Membership still answers from the language set.
        assert!(index["src/file_0.ts"].contains("method_399"));
        assert!(index["src/file_1.py"].contains("method_0"));
        assert!(!index["src/file_0.ts"].contains("absent"));
    }

    #[test]
    fn compact_initializers_match_expanded_calls() {
        let (_temp, root, files) = fixture_project(&[
            ("main.go", "package main\nfunc main() {}\n"),
            (
                "lib/lib.go",
                "package lib\nfunc Alive() {}\nfunc Dead() {}\n",
            ),
        ]);
        let mut snapshot = snapshot_with_entry_points(
            files.clone(),
            vec![
                export(&root, "lib/lib.go", "Alive", "function"),
                export(&root, "lib/lib.go", "Dead", "function"),
            ],
            Vec::new(),
            [root.join("main.go")].into_iter().collect(),
        );
        snapshot
            .go_initializers
            .push(crate::inspect::job::GoInitializerGroup {
                caller_file: root.join("main.go"),
                callers: vec!["main".into()],
                targets: vec![(Arc::new(root.join("lib/lib.go")), Arc::from("Alive"))],
            });
        let contributions = run_dead_code_scan(&job(&root, files, snapshot.clone()))
            .outcome
            .unwrap()
            .contributions;
        let roles = crate::inspect::entry_points::ProjectRoles::default();
        let public = BTreeSet::new();
        let rollup = |snapshot: &CallgraphSnapshot| {
            aggregate_dead_code_contributions_with_snapshot(
                &root,
                snapshot,
                &contributions,
                &public,
                &roles,
                None,
            )
        };
        let initial = rollup(&snapshot);
        assert!(!aggregate_has_item(&initial, "lib/lib.go", "Alive"));
        assert!(aggregate_has_item(&initial, "lib/lib.go", "Dead"));

        // The group's target moves: the one group's calls answer as the
        // same calls written out one by one.
        snapshot.go_initializers[0].targets[0].1 = Arc::from("Dead");
        let compact = rollup(&snapshot);
        let mut expanded = snapshot.clone();
        expanded.outbound_calls = snapshot.expanded_outbound_calls();
        expanded.go_initializers.clear();
        let written_out = rollup(&expanded);
        assert!(aggregate_has_item(&compact, "lib/lib.go", "Alive"));
        assert!(!aggregate_has_item(&compact, "lib/lib.go", "Dead"));
        assert_eq!(
            serde_json::to_vec(&compact).unwrap(),
            serde_json::to_vec(&written_out).unwrap()
        );
        snapshot.go_initializers.clear();
        let removed = rollup(&snapshot);
        assert!(aggregate_has_item(&removed, "lib/lib.go", "Alive"));
        assert!(aggregate_has_item(&removed, "lib/lib.go", "Dead"));
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

    #[test]
    fn oversized_js_ts_sources_are_unopened_graph_wide_gaps_including_cached_rollups() {
        for extension in ["js", "ts"] {
            let importer = format!("src/importer.{extension}");
            let target = format!("src/target.{extension}");
            let handwritten = super::super::source_text::handwritten_graph_source_for_test(
                1_572_864,
                "export function inLarge() {}\n",
            );
            let (_temp, root, files) = fixture_project(&[
                (&importer, &handwritten),
                (&target, "export function used() {}\n"),
            ]);
            let snapshot = snapshot(
                files.clone(),
                vec![
                    export(&root, &importer, "inLarge", "function"),
                    export(&root, &target, "used", "function"),
                ],
                vec![],
            );
            let scan_job = job(&root, files.clone(), snapshot.clone());
            let public = BTreeSet::new();
            let roles = crate::inspect::entry_points::ProjectRoles::default();
            for oxc in [false, true] {
                let scan = || {
                    if oxc {
                        scan_success_with_oxc(scan_job.clone())
                    } else {
                        run_dead_code_scan(&scan_job).outcome.unwrap()
                    }
                };
                let rollup = |facts: &[FileContribution]| {
                    aggregate_dead_code_contributions_with_snapshot(
                        &root, &snapshot, facts, &public, &roles, None,
                    )
                };
                let initial = scan();
                assert_eq!(initial.aggregate["count"], 2);
                let source_reads =
                    super::super::source_text::source_read_count_for_debug(&files[0]);
                let head_bytes = crate::file_kind::read_probe::bytes_read(&files[0]);
                fs::write(&files[0], super::super::source_text::handwritten_graph_source_for_test(5_242_880, &format!("import {{ used }} from './target.{extension}'; used();\nexport function inLarge() {{}}\n"))).unwrap();
                let probe = crate::project_content::Probe::new(&root);
                let skipped = scan();
                let expected = json!([{"file": importer, "reason": "over the 4 MiB size limit"}]);
                assert_eq!(
                    skipped.aggregate["count"], 0,
                    "{extension}, oxc={oxc}: {}",
                    skipped.aggregate
                );
                assert_eq!(skipped.aggregate["complete"], false);
                assert_eq!(skipped.aggregate["skipped_files"], expected);
                let cached = rollup(&skipped.contributions);
                assert_eq!(cached, skipped.aggregate);
                assert_eq!(
                    super::super::source_text::source_read_count_for_debug(&files[0]),
                    source_reads
                );
                assert!(
                    crate::file_kind::read_probe::bytes_read(&files[0]) - head_bytes
                        <= crate::file_kind::HEAD_BYTES * 2
                );
                drop(probe);
                fs::write(&files[0], &handwritten).unwrap();
                let repaired = scan();
                let cached_repair = rollup(&repaired.contributions);
                assert_eq!(cached_repair, initial.aggregate);
            }
        }
    }

    #[test]
    fn generated_and_minified_js_ts_are_unopened_deliberate_graph_exclusions() {
        let (_temp, root, files) = fixture_project(&[
            (
                "src/api.generated.ts",
                "import { used } from './target'; used(); export function generatedUnused() {}\n",
            ),
            ("src/bundle.min.js", "export function minifiedUnused() {}\n"),
            ("src/target.ts", "export function used() {}\n"),
        ]);
        let snapshot = snapshot_with_entry_points(
            files.clone(),
            vec![
                export(&root, "src/api.generated.ts", "generatedUnused", "function"),
                export(&root, "src/bundle.min.js", "minifiedUnused", "function"),
                export(&root, "src/target.ts", "used", "function"),
            ],
            vec![outbound(
                &root,
                "src/api.generated.ts",
                "<top-level>",
                "used",
            )],
            BTreeSet::from([files[0].clone()]),
        );
        let scan_job = job(&root, files.clone(), snapshot);
        let probe = crate::project_content::Probe::new(&root);
        for oxc in [false, true] {
            let success = if oxc {
                scan_success_with_oxc(scan_job.clone())
            } else {
                run_dead_code_scan(&scan_job).outcome.unwrap()
            };
            assert_eq!(success.aggregate["count"], 1, "{}", success.aggregate);
            assert_eq!(success.aggregate["generated_count"], 0);
            assert_eq!(success.aggregate["complete"], true);
            assert!(success.aggregate.get("skipped_files").is_none());
            assert!(aggregate_has_item(
                &success.aggregate,
                "src/target.ts",
                "used"
            ));
        }
        assert!(!probe.paths().contains(&files[0]));
        assert!(!probe.paths().contains(&files[1]));
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
            go_initializers: Vec::new(),
            entry_points,
            entry_point_symbols: BTreeMap::new(),
            data_layer: None,
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

    fn library_fixture_scan(lib: &str, hidden: &str) -> Value {
        let (_temp, root, paths) = canonical_fixture(&[
            (
                "Cargo.toml",
                "[package]\nname = 'demo'\nversion = '0.1.0'\n",
            ),
            ("src/lib.rs", lib),
            ("src/hidden.rs", hidden),
        ]);
        let mut analyzer = DeadCodeFileAnalyzer::default();
        let exports = paths
            .iter()
            .filter_map(|file| {
                let lang = detect_language(file)?;
                let source = fs::read_to_string(file).unwrap();
                let tree = analyzer.parse_source(lang, &source).unwrap();
                Some(
                    crate::parser::extract_symbols_from_tree(&source, &tree, lang)
                        .unwrap()
                        .into_iter()
                        .filter(|symbol| symbol.exported)
                        .map(|symbol| CallgraphExport {
                            file: file.clone(),
                            symbol: symbol.name,
                            kind: serde_json::to_value(symbol.kind)
                                .unwrap()
                                .as_str()
                                .unwrap()
                                .to_string(),
                            line: symbol.range.start_line + 1,
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .flatten()
            .collect();
        scan(job(
            &root,
            paths.clone(),
            snapshot(paths, exports, Vec::new()),
        ))
    }

    fn stored_rust_fixture_scan(files: &[(&str, &str)]) -> Value {
        let (temp, root, paths) = canonical_fixture(files);

        let snapshot = crate::callgraph_store::dead_code_projection::snapshot_for_test(&root);
        let aggregate = scan(job(&root, paths, snapshot));
        drop(temp);
        aggregate
    }

    #[test]
    fn rust_module_paths_and_function_values_in_impl_bodies_are_live() {
        let (temp, root, paths) = canonical_fixture(&[
            (
                "src/main.rs",
                r#"
mod checks;
struct Handler { draw: fn() }
impl Handler {
    fn new() -> Self { Self::with_draw() }
    fn with_draw() -> Self { Self { draw: checks::draw } }
    fn register(&self) { Self::register_in(); }
    fn register_in() {
        checks::check();
        let callback = checks::assigned;
        let callbacks = [checks::arrayed];
        consume(checks::argument);
    }
}
fn consume(_: fn()) {}
fn main() { let handler = Handler::new(); handler.register(); }
"#,
            ),
            (
                "src/checks.rs",
                r#"
pub(crate) fn check() {}
pub(crate) fn draw() {}
pub(crate) fn assigned() {}
pub(crate) fn arrayed() {}
pub(crate) fn argument() {}
pub(crate) fn planted_dead() {}
#[cfg(test)]
mod tests {
    #[test]
    fn checks() { super::check(); }
}
"#,
            ),
        ]);

        let snapshot = crate::callgraph_store::dead_code_projection::snapshot_for_test(&root);
        let aggregate = scan(job(&root, paths, snapshot));
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/checks.rs::planted_dead"],
            "{aggregate:#}"
        );
        assert_eq!(aggregate["test_only_count"], 0, "{aggregate:#}");

        let view = crate::callgraph_store::dead_code_projection::view_for_test(&root);
        let tree = crate::commands::callgraph_store_adapter::call_tree_result(
            &view.callgraph(),
            Path::new("src/main.rs"),
            "Handler::with_draw",
            1,
            true,
        )
        .unwrap();
        assert!(
            tree.children.is_empty(),
            "function values are not executed: {tree:#?}"
        );

        drop(temp);
    }

    #[test]
    fn rust_qualified_paths_resolve_from_the_enclosing_module() {
        let (temp, root, paths) = canonical_fixture(&[
            (
                "src/main.rs",
                "mod driver; mod targets; fn main() { driver::run(); }",
            ),
            (
                "src/driver.rs",
                r#"
use crate::targets::drop;
pub fn run() { std::mem::drop(0); outer::run(); }
pub(crate) fn via_super() {}
mod outer {
    use crate::targets as alias;
    pub fn run() {
        child::plain();
        child::typed::<()>();
        self::child::via_self();
        super::via_super();
        crate::targets::via_crate();
        alias::via_alias();
        child::deep::run();
    }
    pub(crate) mod child {
        pub(crate) fn plain() {}
        pub(crate) fn typed<T>() {}
        pub(crate) fn via_self() {}
        pub(crate) mod deep {
            pub fn run() { super::super::super::via_super(); }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::outer::child::typed;
    #[test] fn checks() { super::outer::child::plain(); typed::<()>(); }
}
"#,
            ),
            (
                "src/targets.rs",
                r#"
pub(crate) fn via_crate() {}
pub(crate) fn via_alias() {}
pub(crate) fn drop(_value: i32) {}
"#,
            ),
        ]);

        let snapshot = crate::callgraph_store::dead_code_projection::snapshot_for_test(&root);
        let aggregate = scan(job(&root, paths, snapshot));
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/targets.rs::drop"],
            "{aggregate:#}"
        );
        assert_eq!(aggregate["test_only_count"], 0, "{aggregate:#}");

        drop(temp);
    }

    #[test]
    fn own_repo_cfg_test_declarations_are_not_product_dead_code() {
        let aggregate = stored_rust_fixture_scan(&[
            ("Cargo.toml", "[package]\nname = 'demo'\nversion = '0.1.0'\n"),
            ("src/lib.rs", "mod hidden;\n"),
            ("src/hidden.rs", "#[cfg(test)]\npub(crate) fn test_gate() {}\n#[cfg(all(test, feature = \"hooks\"))]\npub(crate) fn test_probe() {}\npub(crate) fn planted_dead() {}\n"),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/hidden.rs::planted_dead"],
            "{aggregate:#}"
        );
        assert_eq!(aggregate["excluded_test_count"], 2, "{aggregate:#}");
    }

    #[test]
    fn own_accuracy_inline_platform_helpers_keep_their_call_chains_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { platform::copy(); }
mod platform {
    #[cfg(unix)]
    pub fn copy() {}
    #[cfg(windows)]
    pub fn copy() { verify(); }
    pub fn verify() { leaf(); }
    pub fn leaf() {}
    pub fn unused() {}
}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_private_generic_read_path_keeps_handle_guard_live() {
        let aggregate = stored_rust_fixture_scan(&[
            (
                "src/main.rs",
                "mod read; mod guard; fn main() { read::read_exact(); }",
            ),
            (
                "src/read.rs",
                r#"
pub fn read_exact() { read_with(|_| ()); }
fn read_with<T>(consume: impl FnOnce(bool) -> T) -> T { consume(crate::guard::is_live()) }
"#,
            ),
            (
                "src/guard.rs",
                "pub(crate) fn is_live() -> bool { false } pub(crate) fn unused() {}",
            ),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/guard.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_exact_read_handle_guard_has_a_live_source_path() {
        let aggregate = stored_rust_fixture_scan(&[
            (
                "src/main.rs",
                "mod read; mod db { pub mod file_identity; } fn main() { read::read_exact(); }",
            ),
            (
                "src/read.rs",
                include_str!("../../commands/file_read_exact.rs"),
            ),
            (
                "src/db/file_identity.rs",
                "pub(crate) fn is_live_sqlite_handle() {}",
            ),
        ]);
        assert!(
            !aggregate_has_item(
                &aggregate,
                "src/db/file_identity.rs",
                "is_live_sqlite_handle"
            ),
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_module_declarations_with_same_short_name_stay_separate() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { a::run(); }
mod a { pub(crate) fn run() {} }
mod b {
    pub(crate) fn run() { private_leaf(); }
    pub(crate) fn private_leaf() {}
}
"#,
        )]);
        let items = aggregate["items"].as_array().unwrap();
        assert_eq!(items.len(), 2, "{aggregate:#}");
        assert!(
            items
                .iter()
                .any(|item| item["symbol"] == "run" && item["line"] == 5),
            "{aggregate:#}"
        );
        assert!(
            items.iter().any(|item| item["symbol"] == "private_leaf"),
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_bare_function_branch_and_block_values_are_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    let c = true;
    let render = if c { normal } else { confined }; render();
    let nested = { { block_tail } }; nested();
}
pub(crate) fn normal() {}
pub(crate) fn confined() {}
pub(crate) fn block_tail() {}
pub(crate) fn unused() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_bare_branch_local_binding_shadows_function() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    let normal = || {};
    let render = if true { normal } else { confined }; render();
}
pub(crate) fn normal() {}
pub(crate) fn confined() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::normal"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_macro_callback_later_binding_does_not_shadow() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    tokio::select! { reply = query() => { reply.is_ok_and(ends_scope); } }
    let ends_scope = false;
}
pub(crate) fn ends_scope() {}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
    }

    #[test]
    fn review_macro_callback_unrelated_nested_binding_does_not_shadow() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    { let ends_scope = false; }
    tokio::select! { reply = query() => { reply.is_ok_and(ends_scope); } }
}
pub(crate) fn ends_scope() {}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
    }

    fn test_only_symbols(aggregate: &Value) -> Vec<String> {
        let mut symbols = aggregate["test_only_items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{}::{}",
                    item["file"].as_str().unwrap_or_default(),
                    item["symbol"].as_str().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>();
        symbols.sort();
        symbols
    }

    /// A `#[cfg(test)]` statement or block inside a production function only
    /// exists in test builds. Its calls are test usage, while the production
    /// call beside it stays production usage.
    #[test]
    fn review_cfg_test_statement_and_block_in_production_function_are_test_only() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    #[cfg(test)]
    statement_helper();
    #[cfg(test)]
    {
        block_helper();
    }
    production_helper();
}
pub(crate) fn statement_helper() {}
pub(crate) fn block_helper() {}
pub(crate) fn production_helper() {}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert_eq!(
            test_only_symbols(&aggregate),
            vec!["src/main.rs::block_helper", "src/main.rs::statement_helper"],
            "{aggregate:#}"
        );
    }

    /// A `#[cfg(test)]` module nested in a production function body: the
    /// enclosing function's body scan also sees the module's calls, and they
    /// must keep the module's test status.
    #[test]
    fn review_cfg_test_module_in_production_function_is_test_only() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    #[cfg(test)]
    mod checks {
        pub(crate) fn probe() { crate::module_helper(); }
    }
    production_helper();
}
pub(crate) fn module_helper() {}
pub(crate) fn production_helper() {}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert_eq!(
            test_only_symbols(&aggregate),
            vec!["src/main.rs::module_helper"],
            "{aggregate:#}"
        );
    }

    /// serde callbacks named inside an inline module root that module's
    /// declaration, for a bare name and for a relative path, and do not root a
    /// same-named function at the file root.
    #[test]
    fn review_serde_callbacks_in_inline_module_root_their_declarations() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { let _ = config::Git { author: None, email: None }; }
mod config {
    #[derive(serde::Deserialize)]
    pub(crate) struct Git {
        #[serde(deserialize_with = "deserialize_author")]
        pub author: Option<String>,
        #[serde(deserialize_with = "helpers::parse")]
        pub email: Option<String>,
    }
    fn deserialize_author() { normalize_author(); }
    pub(crate) fn normalize_author() {}
    mod helpers {
        pub(crate) fn parse() { leaf(); }
        pub(crate) fn leaf() {}
    }
    pub(crate) fn planted_dead() {}
}
pub(crate) fn deserialize_author() {}
"#,
        )]);
        let mut headline = aggregate["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{}:{}",
                    item["symbol"].as_str().unwrap_or_default(),
                    item["line"]
                )
            })
            .collect::<Vec<_>>();
        headline.sort();
        assert_eq!(
            headline,
            vec!["deserialize_author:19", "planted_dead:17"],
            "{aggregate:#}"
        );
    }

    /// Test status of a source reference follows the declaration it reaches,
    /// never the spelling of its name: a function stays test usage even with
    /// an upper-case name, and a tuple struct built in a test assertion stays
    /// production-used even with a lower-case name.
    #[test]
    fn review_source_reference_test_status_follows_declaration_kind() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { let _keep: Option<counter> = None; }
#[allow(non_snake_case)]
pub(crate) fn Probe() -> bool { true }
#[allow(non_camel_case_types)]
pub(crate) struct counter(pub u32);
#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert!(super::Probe()); assert_eq!(super::counter(1).0, 1); }
}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert_eq!(
            test_only_symbols(&aggregate),
            vec!["src/main.rs::Probe"],
            "{aggregate:#}"
        );
    }

    /// A type declared under `#[cfg(test)]` is test code, like a
    /// `#[cfg(test)]` function: it is neither product dead code nor a
    /// test-only product row.
    #[test]
    fn review_cfg_test_type_declaration_is_test_code() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {}
#[cfg(test)]
pub(crate) struct Gate(u32);
#[cfg(test)]
pub(crate) fn gate_for_test() -> Gate { Gate(1) }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = super::gate_for_test(); }
}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert!(test_only_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_count"], 2, "{aggregate:#}");
    }

    /// A method called from a production closure and from a test is one
    /// declaration: the test call must not make what the method's body uses
    /// look test-only.
    #[test]
    fn review_method_called_from_production_closure_and_test_keeps_body_production() {
        let watchdog = r#"
pub(super) struct PlatformCapture;
pub(crate) struct Config { capture: std::sync::Arc<PlatformCapture> }
impl Config {
    pub(super) fn production() -> Self {
        Self { capture: std::sync::Arc::new(PlatformCapture) }
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn production_config() { let _ = super::Config::production(); }
}
"#;
        // The module-qualified call is also resolved by the source scan; the
        // imported-type call is resolved only by the stored callgraph, which
        // names the method `production`.
        for start in [
            "mod stall_watchdog;\npub(crate) fn start(o: Option<stall_watchdog::Config>) {\n    let _ = o.unwrap_or_else(|| stall_watchdog::Config::production());\n}\n",
            "mod stall_watchdog;\nuse stall_watchdog::Config;\npub(crate) fn start(o: Option<Config>) {\n    let _ = o.unwrap_or_else(|| Config::production());\n}\n",
        ] {
            let aggregate = stored_rust_fixture_scan(&[
                ("src/main.rs", "mod subc;\nfn main() { subc::start(None); }\n"),
                ("src/subc/mod.rs", start),
                ("src/subc/stall_watchdog.rs", watchdog),
            ]);
            assert!(headline_symbols(&aggregate).is_empty(), "{start}\n{aggregate:#}");
            assert!(
                test_only_symbols(&aggregate).is_empty(),
                "{start}\n{aggregate:#}"
            );
        }
    }

    fn headline_symbol_lines(aggregate: &Value) -> Vec<String> {
        let mut headline = aggregate["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{}:{}",
                    item["symbol"].as_str().unwrap_or_default(),
                    item["line"]
                )
            })
            .collect::<Vec<_>>();
        headline.sort();
        headline
    }

    /// A free function in a module named like a type (`Config::production` in
    /// `mod Config`) is not that type's method, even when one impl in the file
    /// defines a method with the same name: a call to the free function keeps
    /// the free function live, and the planted dead `other::production` stays
    /// dead. That short name is ambiguous, so only the source scan, which
    /// keeps the module path, links the call.
    ///
    /// For now the index store roots every exported (pub or pub(crate))
    /// method and type, so the uncalled method stays live as well. AFT
    /// 0.63's SQLite call-graph store, which the index store replaced,
    /// reported it dead, and the index store can return to that verdict once
    /// member calls it cannot resolve keep their method name live (a
    /// name-based dispatch rescue); without that rescue, dropping the
    /// exported roots reports called methods dead.
    /// The helper only the method's body calls stays live with it.
    #[test]
    fn review_module_qualified_free_function_is_not_the_same_named_method() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { Config::production(); }
#[allow(non_snake_case)]
mod Config {
    pub(crate) fn production() {}
}
mod other {
    pub(crate) fn production() {}
}
mod types {
    pub(crate) struct Config;
    impl Config {
        pub(crate) fn production() -> Self { helper(); Config }
    }
    pub(crate) fn helper() {}
}
"#,
        )]);
        assert_eq!(
            headline_symbol_lines(&aggregate),
            vec!["production:8"],
            "{aggregate:#}"
        );
    }

    /// The same in a nested module (`settings::Config::production`): the call
    /// from `main` keeps the free function live, and the planted dead
    /// `other::production` stays dead.
    ///
    /// For now the index store roots every exported (pub or pub(crate))
    /// method and type, so the uncalled method stays live as well. AFT
    /// 0.63's SQLite call-graph store, which the index store replaced,
    /// reported it dead, and the index store can return to that verdict once
    /// member calls it cannot resolve keep their method name live (a
    /// name-based dispatch rescue); without that rescue, dropping the
    /// exported roots reports called methods dead.
    #[test]
    fn review_nested_module_free_function_called_from_main_stays_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { settings::Config::production(); }
mod settings {
    #[allow(non_snake_case)]
    pub(crate) mod Config {
        pub(crate) fn production() {}
    }
    pub(crate) mod other {
        pub(crate) fn production() {}
    }
}
mod types {
    pub(crate) struct Config;
    impl Config {
        pub(crate) fn production() -> Self { Config }
    }
}
"#,
        )]);
        assert_eq!(
            headline_symbol_lines(&aggregate),
            vec!["production:9"],
            "{aggregate:#}"
        );
    }

    /// The impl-named spelling maps to the short method node only when it can
    /// denote nothing but that impl's method.
    #[test]
    fn rust_method_node_maps_only_the_impl_method_identity() {
        let item = |scope: &[&str], name: &str, line: u32, owner: Option<&str>| RustApiItem {
            name: name.to_string(),
            scope: scope.iter().map(|segment| segment.to_string()).collect(),
            public: false,
            line,
            owner: owner.map(str::to_string),
            trait_name: None,
        };
        // `mod Config { fn production() }` at line 2 and
        // `mod types { impl Config { fn production() } }` at line 10.
        let items = vec![
            item(&["Config"], "production", 2, None),
            item(&["types"], "production", 10, Some("Config")),
        ];
        let node =
            |symbol: &str, caller_line: Option<u32>| rust_method_node(&items, symbol, caller_line);
        assert_eq!(
            node("types::Config::production", None).as_deref(),
            Some("production")
        );
        // The free function's module-qualified name.
        assert_eq!(node("Config::production", None), None);
        // A module path that is not the impl's own module.
        assert_eq!(node("other::Config::production", None), None);
        // A caller is the declaration enclosing its call line.
        assert_eq!(
            node("Config::production", Some(11)).as_deref(),
            Some("production")
        );
        assert_eq!(node("Config::production", Some(3)), None);
    }

    /// A function sharing a type's name in another module is not that type:
    /// a test call to `callbacks::Probe()` keeps its test status, so the
    /// function is test-only, while the type used by production is not.
    #[test]
    fn review_same_named_function_in_other_module_is_not_the_type_declaration() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { let _keep: Option<types::Probe> = None; }
mod types {
    pub(crate) struct Probe;
}
mod callbacks {
    #[allow(non_snake_case)]
    pub(crate) fn Probe() -> bool { true }
}
#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert!(super::callbacks::Probe()); }
}
"#,
        )]);
        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert_eq!(
            test_only_symbols(&aggregate),
            vec!["src/main.rs::Probe"],
            "{aggregate:#}"
        );
    }

    /// `super::` inside an inline module names the enclosing module first:
    /// from `mod source_replacement` in `edit.rs` it reaches `edit`'s
    /// out-of-line child `edit/windows_replace.rs`, also from a cfg branch.
    #[test]
    fn review_super_path_in_inline_module_reaches_out_of_line_sibling() {
        let aggregate = stored_rust_fixture_scan(&[
            ("src/main.rs", "mod edit;\nfn main() { edit::write(); }\n"),
            (
                "src/edit.rs",
                r#"
#[cfg(windows)]
mod windows_replace;
pub(crate) fn write() { let _ = source_replacement::may_keep_access(); }
mod source_replacement {
    #[cfg(unix)]
    pub(super) fn may_keep_access() -> bool { true }
    #[cfg(windows)]
    pub(super) fn may_keep_access() -> bool { super::windows_replace::link_count() == Some(1) }
}
"#,
            ),
            (
                "src/edit/windows_replace.rs",
                "pub(super) fn link_count() -> Option<u32> { None }\npub(super) fn unused() {}\n",
            ),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/edit/windows_replace.rs::unused"],
            "{aggregate:#}"
        );
    }

    /// A module name can reach a file through chained `use super::*` globs and
    /// `#[path]` declarations: `read_executor.rs` (the `#[path]` module
    /// `executor` of `worker_reads`) sees `relay_client`, which `gh_shim.rs`
    /// declares with `#[path = "gh_shim_relay_client.rs"]`.
    #[test]
    fn review_module_through_chained_globs_and_path_attributes_resolves() {
        let aggregate = stored_rust_fixture_scan(&[
            ("src/main.rs", "mod gh_shim;\nfn main() { gh_shim::run(); }\n"),
            (
                "src/gh_shim.rs",
                "mod worker_reads;\n#[path = \"gh_shim_relay_client.rs\"]\nmod relay_client;\npub(crate) fn run() { worker_reads::serve(); }\n",
            ),
            (
                "src/gh_shim_relay_client.rs",
                "pub(super) fn read_request() -> u32 { 1 }\npub(super) fn unused() {}\n",
            ),
            (
                "src/gh_shim/worker_reads.rs",
                "use super::*;\n#[path = \"read_executor.rs\"]\nmod executor;\nuse executor::execute_read;\npub(super) fn serve() { let mut send = |r: u32| execute_read(r); send(1); }\n",
            ),
            (
                "src/gh_shim/read_executor.rs",
                "use super::*;\npub(super) fn execute_read(r: u32) -> u32 {\n    execute_read_with(r, &mut |_| relay_client::read_request())\n}\npub(super) fn execute_read_with(r: u32, send: &mut impl FnMut(u32) -> u32) -> u32 { send(r) }\n",
            ),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/gh_shim_relay_client.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_selected_function_item_keeps_catalog_renderers_live() {
        let aggregate = stored_rust_fixture_scan(&[
            (
                "src/main.rs",
                r#"
mod catalog;
fn main() { let render = if true { catalog::normal } else { catalog::confined }; render(); }
"#,
            ),
            (
                "src/catalog.rs",
                r#"
pub(crate) fn normal() { text(); }
pub(crate) fn confined() { text(); }
fn text() {}
pub(crate) fn unused() {}
"#,
            ),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/catalog.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_macro_callback_is_live_but_unused_sibling_is_dead() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { tokio::select! { reply = query() => { if reply.is_ok_and(ends_scope) {} } } }
pub(crate) fn ends_scope() {}
pub(crate) fn unused() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_cfg_variants_preserve_nested_and_parent_module_calls() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { replacement::copy(); }
mod replacement {
    #[cfg(unix)] pub(super) fn copy() { attrs::verify(); }
    #[cfg(windows)] pub(super) fn copy() { windows::links(); super::recovery::protect(); }
    #[cfg(not(any(unix, windows)))] pub(super) fn copy() {}
    mod attrs {
        #[cfg(target_os = "linux")] pub(crate) fn verify() {}
        #[cfg(target_os = "macos")] pub(crate) fn verify() { leaf(); }
        pub(crate) fn leaf() {}
    }
    mod windows { pub(super) fn links() {} }
}
mod recovery { pub(super) fn protect() {} }
pub(crate) fn unused() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_macro_bound_callback_is_not_a_function_reference() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { let callback = false; tokio::select! { reply = query() => { reply.is_ok_and(callback); } } }
pub(crate) fn callback() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::callback"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_accuracy_private_macro_hop_keeps_handle_guard_live() {
        // Exercise the source fallback without callgraph edges for the opaque
        // macros. The private intermediate is not a reportable export.
        let aggregate = rust_entry_scan(
            &[
                (
                    "src/main.rs",
                    r#"
mod guard;
fn main() { tokio::select! { reply = work() => { read_with(); } } }
fn read_with() { wrapper!(guard::is_live()); }
"#,
                ),
                (
                    "src/guard.rs",
                    "pub(crate) fn is_live() {} pub(crate) fn unused() {}",
                ),
            ],
            &[
                ("src/main.rs", "main", "function"),
                ("src/guard.rs", "is_live", "function"),
                ("src/guard.rs", "unused", "function"),
            ],
        );
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/guard.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_local_macro_never_invoked_does_not_keep_template_callee_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            "fn main() { macro_rules! unused { () => { helper() }; } }\npub(crate) fn helper() {}\n",
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::helper"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_local_macro_invoked_in_body_keeps_template_callee_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            "fn main() { macro_rules! used { () => { helper() }; } used!(); }\npub(crate) fn helper() {}\npub(crate) fn unused() {}\n",
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_callback_passed_through_invoked_local_macro_stays_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    macro_rules! with_callback { ($f:expr) => { run($f) }; }
    with_callback!(callback);
}
pub(crate) fn run(f: fn()) { f() }
pub(crate) fn callback() {}
pub(crate) fn unused() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_local_macro_invoked_only_from_dead_function_leaves_callee_dead() {
        // `main` defines a same-named macro but never invokes it; only the
        // unreachable `dead` expands `m!`, so `helper` and its callee stay dead.
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() { macro_rules! m { () => { helper() }; } }
pub(crate) fn dead() { macro_rules! m { () => { helper() }; } m!(); }
pub(crate) fn helper() { leaf(); }
pub(crate) fn leaf() {}
"#,
        )]);
        let mut headline = headline_symbols(&aggregate);
        headline.sort();
        assert_eq!(
            headline,
            vec![
                "src/main.rs::dead",
                "src/main.rs::helper",
                "src/main.rs::leaf"
            ],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_local_macro_expanded_through_another_template_keeps_callee_live() {
        let aggregate = stored_rust_fixture_scan(&[(
            "src/main.rs",
            r#"
fn main() {
    macro_rules! inner { () => { chained() }; }
    macro_rules! outer { () => { inner!() }; }
    macro_rules! target { () => { by_metavariable() }; }
    macro_rules! apply { ($m:ident) => { $m!() }; }
    outer!();
    apply!(target);
}
pub(crate) fn chained() {}
pub(crate) fn by_metavariable() {}
pub(crate) fn unused() {}
"#,
        )]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    fn metavariable_macro_fixture_scan(apply: &str) -> Value {
        stored_rust_fixture_scan(&[(
            "src/main.rs",
            &format!(
                r#"
fn main() {{
    macro_rules! target {{ () => {{ helper() }}; }}
    {apply}
    apply!(target);
}}
pub(crate) fn helper() {{}}
pub(crate) fn unused() {{}}
"#
            ),
        )])
    }

    #[test]
    fn review_metavariable_macro_invocation_after_block_comment_keeps_callee_live() {
        let aggregate = metavariable_macro_fixture_scan(
            "macro_rules! apply { ($m:ident) => { $m /* expand */ !() }; }",
        );
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_metavariable_macro_invocation_after_line_comment_keeps_callee_live() {
        let aggregate = metavariable_macro_fixture_scan(
            "macro_rules! apply { ($m:ident) => { $m // x\n !() }; }",
        );
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn review_unicode_metavariable_macro_invocation_keeps_callee_live() {
        let aggregate =
            metavariable_macro_fixture_scan("macro_rules! apply { ($mé:ident) => { $mé!() }; }");
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/main.rs::unused"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn rust_template_metavariable_macro_invocation_is_detected() {
        let mut analyzer = DeadCodeFileAnalyzer::default();
        // Returns (parse error in the template, metavariable invocation found).
        let mut check = |template: &str| {
            let source = format!("macro_rules! t {{ () => {{ {template} }}; }}");
            let tree = analyzer.parse_source(LangId::Rust, &source).unwrap();
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if node.kind() == "macro_rule" {
                    let right = node.child_by_field_name("right").unwrap();
                    return (
                        right.has_error(),
                        rust_token_tree_invokes_metavariable_macro(right),
                    );
                }
                let mut cursor = node.walk();
                stack.extend(node.children(&mut cursor));
            }
            panic!("no macro rule in {source}");
        };
        assert_eq!(check("$m!()"), (false, true));
        assert_eq!(check("$cb ! { x }"), (false, true));
        assert_eq!(check("{ inner($f); $m!() }"), (false, true));
        assert_eq!(check("$a != $b"), (false, false));
        assert_eq!(check("run($f)"), (false, false));
        assert_eq!(check("$m; done!()"), (false, false));
        // A bare `$` does not parse; the caller keeps such a template's calls
        // live instead of trusting the token scan.
        assert!(check("$ !").0);
    }

    #[test]
    fn used_test_file_exports_are_not_tallied_as_excluded_dead_findings() {
        let (_temp, root, _) = canonical_fixture(&[("tests/helper.rs", "pub fn called() {}\n")]);
        let contribution = dispatch_contribution("tests/helper.rs", &[("called", "function")]);
        /// Every node is reached by production code.
        struct AllReached;
        impl LivenessAnswers for AllReached {
            fn reachable(&self, _: &str, _: &str) -> bool {
                true
            }
            fn production_reachable(&self, _: &str, _: &str) -> bool {
                true
            }
            fn test_only_callers(&self, _: &str, _: &str) -> Option<Vec<String>> {
                None
            }
        }
        let aggregate = render_dead_code_findings(
            &root,
            std::slice::from_ref(&contribution),
            &BTreeSet::new(),
            &crate::inspect::entry_points::resolve_project_roles(&root),
            1,
            &AllReached,
            &RenderInputs {
                has_source_gaps: false,
                referenced_type_names: BTreeSet::new(),
                test_module_files: BTreeSet::new(),
                dispatched_method_names: &BTreeMap::new(),
            },
        );
        assert_eq!(
            aggregate["excluded_test_count"], 0,
            "a used test export is not a withheld dead finding: {aggregate:#}"
        );
    }

    #[test]
    fn own_repo_private_default_impl_keeps_constructor_callback_chain_live() {
        let aggregate = stored_rust_fixture_scan(&[
            ("Cargo.toml", "[package]\nname = 'demo'\nversion = '0.1.0'\n"),
            ("src/lib.rs", "mod hidden;\n#[derive(Default)]\npub struct Context { slot: hidden::Slot }\n"),
            ("src/hidden.rs", "pub(crate) struct Slot;\nimpl Default for Slot { fn default() -> Self { Self::with_constructor(build_backend) } }\nimpl Slot { pub(crate) fn with_constructor(f: fn()) -> Self { f(); Slot } }\npub(crate) fn build_backend() { downstream(); }\npub(crate) fn downstream() {}\npub(crate) fn planted_dead() {}\n"),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/hidden.rs::planted_dead"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn own_repo_static_receiver_and_index_uses_keep_initializers_live() {
        let aggregate = stored_rust_fixture_scan(&[
            ("Cargo.toml", "[package]\nname = 'demo'\nversion = '0.1.0'\n"),
            ("src/lib.rs", "mod hidden;\npub fn run() { hidden::run(); }\n"),
            ("src/hidden.rs", "use std::sync::LazyLock;\nstruct Env;\nimpl Env { pub(crate) const fn new() -> Self { Env } fn get(&self) {} }\nstatic ENV: Env = Env::new();\nstatic TOOLS: LazyLock<Vec<()>> = LazyLock::new(|| { build_tools(); vec![] });\nstatic DEAD: LazyLock<()> = LazyLock::new(planted_dead);\nfn build_tools() { remote_tool(); }\npub(crate) fn remote_tool() {}\npub(crate) fn run() { ENV.get(); let _ = &TOOLS[0]; }\npub(crate) fn planted_dead() {}\n"),
        ]);
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/hidden.rs::planted_dead"],
            "{aggregate:#}"
        );
    }

    #[test]
    fn rust_public_api_requires_public_module_path() {
        let hidden = "pub fn exposed() {}\npub(crate) fn spare() {}\n";
        let public = library_fixture_scan("pub mod hidden;\n", hidden);
        assert!(
            !aggregate_has_item(&public, "src/hidden.rs", "exposed"),
            "{public:#}"
        );
        assert!(
            aggregate_has_item(&public, "src/hidden.rs", "spare"),
            "{public:#}"
        );
        let private = library_fixture_scan("mod hidden;\n", hidden);
        assert!(
            aggregate_has_item(&private, "src/hidden.rs", "exposed"),
            "{private:#}"
        );
    }

    #[test]
    fn rust_public_api_path_override_normalizes_native_separators() {
        assert_eq!(
            normalize_relative_segments(r"custom\..\custom\redirected.rs"),
            "custom/redirected.rs"
        );
    }

    #[test]
    fn rust_public_api_manifest_named_lib_root_has_root_relative_modules() {
        let (_temp, root, paths) = canonical_fixture(&[
            (
                "Cargo.toml",
                "[package]\nname = 'demo'\nversion = '0.1.0'\n[lib]\npath = 'custom/entry.rs'\n",
            ),
            (
                "custom/entry.rs",
                "pub mod hidden;\n#[path = \"redirected.rs\"]\npub mod renamed;\n",
            ),
            ("custom/hidden.rs", "pub fn exposed() {}\n"),
            ("custom/redirected.rs", "pub fn via_attribute() {}\n"),
        ]);
        let graph = snapshot(
            paths.clone(),
            vec![
                export(&root, "custom/hidden.rs", "exposed", "function"),
                export(&root, "custom/redirected.rs", "via_attribute", "function"),
            ],
            vec![],
        );
        let aggregate = scan(job(&root, paths, graph));
        assert!(
            !aggregate_has_item(&aggregate, "custom/hidden.rs", "exposed"),
            "{aggregate:#}"
        );
        assert!(
            !aggregate_has_item(&aggregate, "custom/redirected.rs", "via_attribute"),
            "{aggregate:#}"
        );
    }

    #[test]
    fn rust_public_api_named_and_aliased_reexports_are_per_item() {
        for use_statement in [
            "pub use hidden::exposed;",
            "pub use hidden::exposed as shown;",
        ] {
            let aggregate = library_fixture_scan(
                &format!("mod hidden;\n{use_statement}\n"),
                "pub fn exposed() {}\npub fn spare() {}\n",
            );
            assert!(
                !aggregate_has_item(&aggregate, "src/hidden.rs", "exposed"),
                "{aggregate:#}"
            );
            assert!(
                aggregate_has_item(&aggregate, "src/hidden.rs", "spare"),
                "{aggregate:#}"
            );
        }
    }

    #[test]
    fn rust_public_api_reexported_type_exposes_only_public_methods() {
        let source =
            "pub struct S;\nimpl S {\n pub fn exposed() {}\n pub(crate) fn spare() {}\n}\n";
        let aggregate = library_fixture_scan("mod hidden;\npub use hidden::S;\n", source);
        assert!(
            !aggregate_has_item(&aggregate, "src/hidden.rs", "exposed"),
            "{aggregate:#}"
        );
        assert!(
            aggregate_has_item(&aggregate, "src/hidden.rs", "spare"),
            "{aggregate:#}"
        );
        let private = library_fixture_scan("mod hidden;\n", source);
        assert!(
            aggregate_has_item(&private, "src/hidden.rs", "exposed"),
            "{private:#}"
        );
    }

    #[test]
    fn rust_public_api_same_named_method_on_private_type_remains_candidate() {
        let aggregate = library_fixture_scan("mod hidden;\npub use hidden::S;\n",
            "pub struct S;\npub struct Private;\nimpl S { pub fn exposed() {} }\nimpl Private { pub fn exposed() {} }\n");
        let item = aggregate_item(&aggregate, "src/hidden.rs", "exposed").unwrap();
        assert_eq!(
            item["line"], 4,
            "only Private::exposed is dead: {aggregate:#}"
        );
    }

    #[test]
    fn rust_public_api_inline_glob_alias_chain_and_macro_reexports() {
        for statement in [
            "pub use hidden::*;",
            "pub use facade::shown;",
            "macro_rules! expose { () => { pub use hidden::exposed; }; }\nexpose!();",
            "macro_rules! expose { ($module:ident, $item:ident) => { pub use $module::$item; }; }\nexpose!(hidden, exposed);",
        ] {
            let aggregate = library_fixture_scan(&format!(
                "mod hidden {{ pub fn exposed() {{}} pub(crate) fn spare() {{}} }}\nmod facade {{ pub use crate::hidden::exposed as shown; }}\n{statement}\n"), "");
            assert!(
                !aggregate_has_item(&aggregate, "src/lib.rs", "exposed"),
                "{aggregate:#}"
            );
            assert!(
                aggregate_has_item(&aggregate, "src/lib.rs", "spare"),
                "{aggregate:#}"
            );
        }
    }

    #[test]
    fn rust_public_api_trait_method_needs_public_type_and_trait() {
        let hidden = "pub struct S;\npub trait T { fn invoke(); }\nimpl T for S { fn invoke() { helper(); } }\npub(crate) fn helper() {}\n";
        for (public_trait, statement) in [
            (true, "pub use hidden::{S, T};"),
            (false, "pub use hidden::S;"),
        ] {
            let (_temp, root, paths) = canonical_fixture(&[
                (
                    "Cargo.toml",
                    "[package]\nname = 'demo'\nversion = '0.1.0'\n",
                ),
                ("src/lib.rs", &format!("mod hidden;\n{statement}\n")),
                ("src/hidden.rs", hidden),
            ]);
            let mut call = outbound(
                &root,
                "src/hidden.rs",
                "T for S::invoke",
                &resolved_target(&root, "src/hidden.rs", "helper"),
            );
            call.line = 3;
            let aggregate = scan(job(
                &root,
                paths.clone(),
                snapshot(
                    paths,
                    vec![export(&root, "src/hidden.rs", "helper", "function")],
                    vec![call],
                ),
            ));
            assert_eq!(
                aggregate_has_item(&aggregate, "src/hidden.rs", "helper"),
                !public_trait,
                "{aggregate:#}"
            );
        }
    }

    #[test]
    fn rust_macro_body_edges_require_reachable_invocation() {
        let (_temp, root, paths) = canonical_fixture(&[(
            "src/main.rs",
            "macro_rules! m { () => { helper() }; }\npub(crate) fn helper() {}\nfn main() {}\n",
        )]);
        let exports = vec![export(&root, "src/main.rs", "helper", "function")];
        let macro_body = outbound(
            &root,
            "src/main.rs",
            "m!",
            &resolved_target(&root, "src/main.rs", "helper"),
        );
        let invocation = outbound(
            &root,
            "src/main.rs",
            "main",
            &resolved_target(&root, "src/main.rs", "m!"),
        );
        for (invoked, calls) in [
            (true, vec![macro_body.clone(), invocation]),
            (false, vec![macro_body.clone()]),
        ] {
            let aggregate = scan(job(
                &root,
                paths.clone(),
                snapshot_with_entry_points(
                    paths.clone(),
                    exports.clone(),
                    calls,
                    BTreeSet::from([root.join("src/main.rs")]),
                ),
            ));
            assert_eq!(
                aggregate_has_item(&aggregate, "src/main.rs", "helper"),
                !invoked,
                "{aggregate:#}"
            );
        }
    }

    #[test]
    fn rust_library_private_pub_function_used_by_cfg_test_names_call_line() {
        let (_temp, root, paths) = canonical_fixture(&[
            ("Cargo.toml", "[package]\nname = 'demo'\nversion = '0.1.0'\n"),
            ("src/lib.rs", "mod hidden;\n#[cfg(test)]\nmod tests {\n fn check() { crate::hidden::exposed(); }\n}\n"),
            ("src/hidden.rs", "pub fn exposed() {}\n"),
        ]);
        let exports = vec![export(&root, "src/hidden.rs", "exposed", "function")];
        let mut call = outbound(
            &root,
            "src/lib.rs",
            "tests::check",
            &resolved_target(&root, "src/hidden.rs", "exposed"),
        );
        call.line = 4;
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(paths, exports, vec![call]),
        ));
        let item = aggregate_test_only_item(&aggregate, "src/hidden.rs", "exposed").unwrap();
        assert_eq!(item["used_by"], json!(["src/lib.rs:4"]));
        let public = library_fixture_scan("pub mod hidden;\n", "pub fn exposed() {}\n");
        assert!(!aggregate_has_item(&public, "src/hidden.rs", "exposed"));
        assert!(aggregate_test_only_item(&public, "src/hidden.rs", "exposed").is_none());
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
            Some("src/api.test.ts")
        );
        assert_eq!(
            aggregate_test_only_item(&aggregate, "src/barrel-target.ts", "throughBarrel")
                .and_then(|item| item["used_by"].as_array())
                .and_then(|items| items.first())
                .and_then(serde_json::Value::as_str),
            Some("src/barrel.test.ts")
        );
    }

    #[test]
    fn oxc_dead_code_excludes_generated_exports_from_the_graph() {
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
            first.aggregate["generated_count"], 0,
            "{:#}",
            first.aggregate
        );
        assert_eq!(first.aggregate["total_count"], 1, "{:#}", first.aggregate);
        assert!(aggregate_item(&first.aggregate, "src/hand.ts", "handDead").is_some());
        assert!(aggregate_generated_item(
            &first.aggregate,
            "gen/schema_pb.ts",
            "generatedPathDead"
        )
        .is_none());
        assert!(
            aggregate_generated_item(&first.aggregate, "src/banner.ts", "bannerDead").is_none()
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

    /// A receiver call inside a macro in a `#[cfg(test)]` module, whose
    /// receiver (`Index(1)`) the index store cannot type, never makes the
    /// method production code: it is not in the dead-code headline. AFT
    /// 0.63's SQLite call-graph store, which the index store replaced,
    /// reported the method in the test-only bucket. For now the index store
    /// roots every exported (pub or pub(crate)) method, so it is in neither
    /// bucket; the test-only verdict returns once member calls the index
    /// store cannot resolve keep their method name live (a name-based
    /// dispatch rescue), which dropping the exported roots needs.
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
        let analysis = DeadCodeFileAnalyzer::default().analyze_file(
            &root.join("src/index.rs"),
            "src/index.rs",
            false,
        );
        assert!(
            analysis
                .cfg_test_ranges
                .iter()
                .any(|range| range.contains(17)),
            "cfg(test) module should classify its receiver call line as test-only: {:?}",
            analysis.cfg_test_ranges
        );

        let snapshot = crate::callgraph_store::dead_code_projection::snapshot_for_test(&root);
        let aggregate = scan(job(&root, paths, snapshot));
        assert!(
            aggregate_test_only_item(&aggregate, "src/index.rs", "shares_index_with").is_none(),
            "the exported receiver method is rooted until the name rescue lands: {aggregate:#}"
        );
        assert!(
            !aggregate_has_item(&aggregate, "src/index.rs", "shares_index_with"),
            "receiver method must not remain in the dead-code headline: {aggregate:#}"
        );
        drop(temp_dir);
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
    fn go_interface_method_liveness_requires_used_receiver() {
        let (_temp, root, paths) = canonical_fixture(&[
            ("go.mod", "module example.com/demo\n"),
            ("main.go", "package main\ntype Webhooks interface { DeleteWebhookByID(id int) error }\ntype Store struct {}\ntype Unused struct {}\nfunc (s *Store) DeleteWebhookByID(id int) error { return nil }\nfunc (s *Unused) DeleteWebhookByID(id int) error { return nil }\nfunc (s *Store) Unrelated() {}\nfunc main() { _ = Store{} }\n"),
        ]);
        let mut exports = vec![
            export(&root, "main.go", "DeleteWebhookByID", "method"),
            export(&root, "main.go", "Unrelated", "method"),
        ];
        exports[0].line = 5;
        let mut unused = exports[0].clone();
        unused.file = root.join("unused.go");
        unused.line = 3;
        fs::write(root.join("unused.go"), "package main\ntype Never struct {}\nfunc (s *Never) DeleteWebhookByID(id int) error { return nil }\n").unwrap();
        let mut paths = paths;
        paths.push(root.join("unused.go"));
        exports.push(unused);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(
                paths,
                exports,
                vec![],
                BTreeSet::from([root.join("main.go")]),
            ),
        ));
        assert!(
            !aggregate_has_item(&aggregate, "main.go", "DeleteWebhookByID"),
            "{aggregate:#}"
        );
        assert!(
            aggregate_has_item(&aggregate, "unused.go", "DeleteWebhookByID"),
            "{aggregate:#}"
        );
        assert!(
            aggregate_has_item(&aggregate, "main.go", "Unrelated"),
            "{aggregate:#}"
        );
    }

    #[test]
    fn go_interface_dispatch_requires_known_matching_contract() {
        for contract in [
            "",
            "type API interface { DeleteWebhookByID(id string) error }",
        ] {
            let source = format!("package main\n{contract}\ntype Store struct {{}}\nfunc (s *Store) DeleteWebhookByID(id int) error {{ return nil }}\nfunc main() {{ _ = Store{{}} }}\n");
            let (_temp, root, paths) = canonical_fixture(&[
                ("go.mod", "module example.com/demo\n"),
                ("main.go", &source),
            ]);
            let graph = snapshot_with_entry_points(
                paths.clone(),
                vec![export(&root, "main.go", "DeleteWebhookByID", "method")],
                vec![],
                BTreeSet::from([root.join("main.go")]),
            );
            let aggregate = scan(job(&root, paths, graph));
            assert!(aggregate_has_item(&aggregate, "main.go", "DeleteWebhookByID"), "neither arbitrary names nor incompatible signatures imply a contract: {aggregate:#}");
        }
    }

    #[test]
    fn dead_code_returns_full_stably_ranked_list_even_with_limit() {
        let (_temp, root, paths) = fixture_project(&[("src/unused.ts", "")]);
        let exports = (0..150)
            .rev()
            .map(|index| {
                let mut item = export(
                    &root,
                    "src/unused.ts",
                    &format!("dead{index:03}"),
                    "function",
                );
                item.line = index + 1;
                item
            })
            .collect();
        let aggregate = scan(job(&root, paths.clone(), snapshot(paths, exports, vec![])));
        let items = aggregate["items"].as_array().unwrap();
        assert_eq!(items.len(), 150, "{aggregate:#}");
        assert_eq!(items[0]["symbol"], "dead000");
        assert_eq!(items[149]["symbol"], "dead149");
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
    fn rust_macro_token_liveness_resolves_use_list_calls() {
        let aggregate = rust_entry_scan(
            &[
                (
                    "src/main.rs",
                    "mod m;\nuse crate::m::{helper, other};\nfn main() { wrapper!(helper()); }\n",
                ),
                ("src/m.rs", "pub fn helper() {}\npub fn other() {}\n"),
            ],
            &[
                ("src/main.rs", "main", "function"),
                ("src/m.rs", "helper", "function"),
                ("src/m.rs", "other", "function"),
            ],
        );
        assert!(!aggregate_has_item(&aggregate, "src/m.rs", "helper"));
        assert!(aggregate_has_item(&aggregate, "src/m.rs", "other"));
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

    fn canonical_fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let (temp_dir, root, paths) = fixture_project(files);
        let root = fs::canonicalize(root).expect("canonical project root");
        let paths = paths
            .into_iter()
            .map(|path| fs::canonicalize(path).expect("canonical fixture file"))
            .collect::<Vec<_>>();
        (temp_dir, root, paths)
    }

    fn resolved_target(root: &Path, file: &str, symbol: &str) -> String {
        format!("{}::{symbol}", root.join(file).display())
    }

    fn headline_symbols(aggregate: &serde_json::Value) -> Vec<String> {
        aggregate["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                format!(
                    "{}::{}",
                    item["file"].as_str().unwrap_or_default(),
                    item["symbol"].as_str().unwrap_or_default()
                )
            })
            .collect()
    }

    /// The callgraph keeps a Rust path call it could not resolve as its bare
    /// path text. The scanner must still link `module::f`, a module bound by
    /// `use crate::m::{self, ..}`, `Type::f` and `Self::f`.
    #[test]
    fn unresolved_rust_path_calls_keep_their_targets_live() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod cache;\nmod net;\nfn main() { net::start(); }\n"),
            (
                "src/net/mod.rs",
                "mod registry;\nuse crate::cache::{self, Cache};\n\npub fn start() {\n    registry::resolve_shell();\n    cache::warm();\n    let _ = Cache::open();\n}\n",
            ),
            ("src/net/registry.rs", "pub fn resolve_shell() {}\n"),
            (
                "src/cache.rs",
                "pub struct Cache;\nimpl Cache {\n    pub fn open() -> Self { Self::with_loader() }\n    pub fn with_loader() -> Self { Cache }\n}\npub fn warm() {}\npub fn planted_dead() {}\n",
            ),
        ]);
        let exports = [
            ("src/main.rs", "main", "function"),
            ("src/net/mod.rs", "start", "function"),
            ("src/net/registry.rs", "resolve_shell", "function"),
            ("src/cache.rs", "Cache", "struct"),
            ("src/cache.rs", "open", "method"),
            ("src/cache.rs", "with_loader", "method"),
            ("src/cache.rs", "warm", "function"),
            ("src/cache.rs", "planted_dead", "function"),
        ]
        .iter()
        .map(|(file, symbol, kind)| export(&root, file, symbol, kind))
        .collect::<Vec<_>>();
        let calls = vec![
            outbound(
                &root,
                "src/main.rs",
                "main",
                &resolved_target(&root, "src/net/mod.rs", "start"),
            ),
            outbound(&root, "src/net/mod.rs", "start", "registry::resolve_shell"),
            outbound(&root, "src/net/mod.rs", "start", "cache::warm"),
            outbound(&root, "src/net/mod.rs", "start", "Cache::open"),
            outbound(&root, "src/cache.rs", "open", "Self::with_loader"),
            // A std path names no project definition and must add nothing.
            outbound(&root, "src/net/mod.rs", "start", "String::planted_dead"),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/cache.rs::planted_dead".to_string()],
            "{aggregate:#}"
        );
    }

    /// End to end through a real callgraph store: a binary calling its library
    /// through the crate name, `crate::` and child-module paths, a use-list
    /// `self` alias, a struct-literal field call next to a nested
    /// `use super::{..}`, a `FromStr` impl used through `.parse()`, and a
    /// library item used only by an example.
    #[test]
    fn store_backed_rust_path_forms_examples_and_trait_impls_are_live() {
        let (temp_dir, root, paths) = canonical_fixture(&[
            (
                "src/lib.rs",
                "pub mod alerts;\npub mod cache;\npub mod lang;\npub mod net;\npub mod util;\n",
            ),
            (
                "src/main.rs",
                "fn main() {\n    path_forms::net::start();\n    let _ = path_forms::lang::parse_lang(\"rust\");\n}\n",
            ),
            (
                "src/net/mod.rs",
                "mod registry;\nuse crate::cache::{self, Cache};\n\npub fn start() {\n    registry::resolve_shell();\n    cache::warm();\n    let _ = Cache::open();\n}\n",
            ),
            ("src/net/registry.rs", "pub fn resolve_shell() {}\n"),
            (
                "src/cache.rs",
                "pub struct Cache;\n\nimpl Cache {\n    pub fn open() -> Self {\n        Self::with_loader()\n    }\n\n    pub fn with_loader() -> Self {\n        Cache\n    }\n}\n\npub fn warm() {\n    crate::alerts::build(\"x\");\n}\n",
            ),
            (
                "src/alerts.rs",
                "pub struct Alert {\n    pub message: String,\n}\n\npub fn build(message: &str) -> Alert {\n    Alert {\n        message: normalize(message),\n    }\n}\n\npub fn normalize(message: &str) -> String {\n    message.trim().to_string()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::{normalize, Alert};\n\n    #[test]\n    fn normalizes() {\n        let _ = Alert { message: normalize(\" x \") };\n    }\n}\n",
            ),
            (
                "src/lang.rs",
                "use std::str::FromStr;\n\npub struct Lang;\n\nimpl FromStr for Lang {\n    type Err = ();\n\n    fn from_str(_text: &str) -> Result<Self, Self::Err> {\n        Ok(Lang)\n    }\n}\n\npub fn parse_lang(text: &str) -> Option<Lang> {\n    text.parse().ok()\n}\n",
            ),
            (
                "src/util.rs",
                "pub fn only_from_example() {}\n\npub fn planted_dead() {}\n",
            ),
            (
                "examples/demo.rs",
                "fn main() {\n    path_forms::util::only_from_example();\n}\n",
            ),
        ]);
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"path-forms\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write manifest");

        let snapshot = crate::callgraph_store::dead_code_projection::snapshot_for_test(&root);
        let aggregate = scan(job(&root, paths, snapshot));

        assert_eq!(
            headline_symbols(&aggregate),
            Vec::<String>::new(),
            "{aggregate:#}"
        );
        assert_eq!(aggregate["test_only_count"], 0, "{aggregate:#}");
        drop(temp_dir);
    }

    #[test]
    fn dead_exports_in_test_trees_are_excluded_from_the_product_count() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/product.ts", "export function productDead() {}\n"),
            (
                "tests/docker/scenarios/edit/fixture/bulk/bulk.ts",
                "export const bulkSymbol0 = 0;\nexport const bulkSymbol1 = 1;\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/product.ts", "productDead", "function"),
            export(
                &root,
                "tests/docker/scenarios/edit/fixture/bulk/bulk.ts",
                "bulkSymbol0",
                "variable",
            ),
            export(
                &root,
                "tests/docker/scenarios/edit/fixture/bulk/bulk.ts",
                "bulkSymbol1",
                "variable",
            ),
        ];
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(paths, exports, Vec::new()),
        ));

        assert_eq!(aggregate["count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["total_count"], 1, "{aggregate:#}");
        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/product.ts::productDead".to_string()]
        );
        assert_eq!(aggregate["excluded_test_count"], 2, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_files"], 1, "{aggregate:#}");
    }

    /// `#[cfg(test)] mod tests;` pulls in a file whose name does not look like a
    /// test. Its calls are test usage and its own items are test code.
    #[test]
    fn cfg_test_module_file_is_test_code() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod index;\nfn main() { index::run(); }\n"),
            (
                "src/index.rs",
                "pub fn run() {}\npub fn only_tests() {}\n\n#[cfg(test)]\nmod tests;\n",
            ),
            (
                "src/index/tests.rs",
                "use super::only_tests;\npub fn helper() {\n    only_tests();\n}\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/index.rs", "run", "function"),
            export(&root, "src/index.rs", "only_tests", "function"),
            export(&root, "src/index/tests.rs", "helper", "function"),
        ];
        let calls = vec![
            outbound(
                &root,
                "src/main.rs",
                "main",
                &resolved_target(&root, "src/index.rs", "run"),
            ),
            outbound(
                &root,
                "src/index/tests.rs",
                "helper",
                &resolved_target(&root, "src/index.rs", "only_tests"),
            ),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert!(
            aggregate_test_only_item(&aggregate, "src/index.rs", "only_tests").is_some(),
            "{aggregate:#}"
        );
        assert_eq!(aggregate["excluded_test_count"], 1, "{aggregate:#}");
        assert_eq!(aggregate["excluded_test_files"], 1, "{aggregate:#}");
    }

    /// A call the callgraph resolved only as far as a re-exporting module
    /// (`pub(crate) use projection::{project_snapshot};`) must still reach the
    /// definition behind the re-export.
    #[test]
    fn rust_crate_visible_reexport_carries_liveness_to_the_definition() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            (
                "src/main.rs",
                "mod inspect;\nmod store;\nfn main() { inspect::run(); }\n",
            ),
            (
                "src/store/mod.rs",
                "mod projection;\npub(crate) use projection::{project_snapshot};\n",
            ),
            (
                "src/store/projection.rs",
                "pub fn project_snapshot() {}\npub fn planted_dead() {}\n",
            ),
            (
                "src/inspect.rs",
                "use crate::store::project_snapshot;\npub fn run() { project_snapshot(); }\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/inspect.rs", "run", "function"),
            export(
                &root,
                "src/store/projection.rs",
                "project_snapshot",
                "function",
            ),
            export(&root, "src/store/projection.rs", "planted_dead", "function"),
        ];
        let calls = vec![
            outbound(
                &root,
                "src/main.rs",
                "main",
                &resolved_target(&root, "src/inspect.rs", "run"),
            ),
            outbound(
                &root,
                "src/inspect.rs",
                "run",
                &resolved_target(&root, "src/store/mod.rs", "project_snapshot"),
            ),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/store/projection.rs::planted_dead".to_string()],
            "{aggregate:#}"
        );
    }

    /// An out-of-line test module that starts with `use super::*;` sees its
    /// parent's imports, so `store::f()` there resolves through the parent's
    /// `use crate::store;` and counts as test-only usage.
    #[test]
    fn glob_super_import_in_test_module_file_uses_parent_imports() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            (
                "src/main.rs",
                "mod index;\nmod store;\nfn main() { index::run(); }\n",
            ),
            (
                "src/index.rs",
                "use crate::store;\npub fn run() {}\n\n#[cfg(test)]\nmod tests;\n",
            ),
            (
                "src/index/tests.rs",
                "use super::*;\npub fn helper() {\n    store::only_tests();\n}\n",
            ),
            ("src/store.rs", "pub fn only_tests() {}\n"),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/index.rs", "run", "function"),
            export(&root, "src/index/tests.rs", "helper", "function"),
            export(&root, "src/store.rs", "only_tests", "function"),
        ];
        let calls = vec![
            outbound(
                &root,
                "src/main.rs",
                "main",
                &resolved_target(&root, "src/index.rs", "run"),
            ),
            outbound(&root, "src/index/tests.rs", "helper", "store::only_tests"),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert!(headline_symbols(&aggregate).is_empty(), "{aggregate:#}");
        assert!(
            aggregate_test_only_item(&aggregate, "src/store.rs", "only_tests").is_some(),
            "{aggregate:#}"
        );
    }

    /// Trait-impl methods run through the trait (`Default::default()`,
    /// `.parse()`, formatting), which the callgraph does not see, so what
    /// they call must stay live.
    #[test]
    fn calls_inside_trait_impl_methods_keep_targets_live() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod slot;\nfn main() {}\n"),
            (
                "src/slot.rs",
                "pub struct Slot;\nimpl Slot {\n    pub fn with_constructor() -> Self { Slot }\n    pub fn planted_dead() {}\n}\nimpl Default for Slot {\n    fn default() -> Self {\n        Self::with_constructor()\n    }\n}\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/slot.rs", "with_constructor", "method"),
            export(&root, "src/slot.rs", "planted_dead", "method"),
        ];
        let calls = vec![outbound(
            &root,
            "src/slot.rs",
            "default",
            &resolved_target(&root, "src/slot.rs", "with_constructor"),
        )];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/slot.rs::planted_dead".to_string()],
            "{aggregate:#}"
        );
    }

    /// A production-file helper that only tests call is test-only usage, and so
    /// is everything only that helper calls.
    #[test]
    fn symbols_reached_only_through_test_only_code_are_test_only() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod calls;\nfn main() {}\n"),
            (
                "src/calls.rs",
                "pub fn extract_in_range() { walk(); }\npub fn walk() {}\npub fn planted_dead() {}\n",
            ),
            (
                "tests/calls_test.rs",
                "#[test]\nfn extracts() { demo::calls::extract_in_range(); }\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/calls.rs", "extract_in_range", "function"),
            export(&root, "src/calls.rs", "walk", "function"),
            export(&root, "src/calls.rs", "planted_dead", "function"),
        ];
        let calls = vec![
            outbound(
                &root,
                "tests/calls_test.rs",
                "extracts",
                &resolved_target(&root, "src/calls.rs", "extract_in_range"),
            ),
            outbound(
                &root,
                "src/calls.rs",
                "extract_in_range",
                &resolved_target(&root, "src/calls.rs", "walk"),
            ),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/calls.rs::planted_dead".to_string()],
            "{aggregate:#}"
        );
        let walk = aggregate_test_only_item(&aggregate, "src/calls.rs", "walk")
            .unwrap_or_else(|| panic!("walk is reached only from tests: {aggregate:#}"));
        assert_eq!(walk["used_by"], json!(["tests/calls_test.rs"]), "{walk:#}");
        assert!(
            aggregate_test_only_item(&aggregate, "src/calls.rs", "extract_in_range").is_some(),
            "{aggregate:#}"
        );
    }

    /// serde calls `deserialize_with` / `default` functions from derived code
    /// the callgraph never sees; what they call must stay live.
    #[test]
    fn serde_attribute_functions_are_liveness_roots() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod config;\nfn main() {}\n"),
            (
                "src/config.rs",
                "#[derive(serde::Deserialize)]\npub struct Git {\n    #[serde(deserialize_with = \"deserialize_author\")]\n    pub author: Option<String>,\n}\n\nfn deserialize_author<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {\n    let _ = d;\n    Ok(normalize_author(\"x\"))\n}\n\npub fn normalize_author(value: &str) -> Option<String> {\n    Some(value.to_string())\n}\n\npub fn planted_dead() {}\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/config.rs", "normalize_author", "function"),
            export(&root, "src/config.rs", "planted_dead", "function"),
        ];
        let calls = vec![outbound(
            &root,
            "src/config.rs",
            "deserialize_author",
            &resolved_target(&root, "src/config.rs", "normalize_author"),
        )];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, calls, entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/config.rs::planted_dead".to_string()],
            "{aggregate:#}"
        );
    }

    /// A struct built only with a struct literal is used even though no type
    /// annotation names it.
    #[test]
    fn struct_built_by_a_struct_literal_is_live() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod zoom;\nfn main() {}\n"),
            (
                "src/zoom.rs",
                "pub struct ZoomResponse {\n    pub lines: usize,\n}\npub struct PlantedDead;\npub fn respond() -> usize {\n    let response = ZoomResponse { lines: 1 };\n    response.lines\n}\n",
            ),
        ]);
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            export(&root, "src/zoom.rs", "ZoomResponse", "struct"),
            export(&root, "src/zoom.rs", "PlantedDead", "struct"),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, Vec::new(), entry_points),
        ));

        assert_eq!(
            headline_symbols(&aggregate),
            vec!["src/zoom.rs::PlantedDead".to_string()],
            "{aggregate:#}"
        );
    }

    /// Two `new` methods on different types in one file share one liveness
    /// node; report it once with both definition lines instead of twice.
    #[test]
    fn same_named_exports_in_one_file_are_reported_once() {
        let (_temp_dir, root, paths) = canonical_fixture(&[
            ("src/main.rs", "mod guards;\nfn main() {}\n"),
            (
                "src/guards.rs",
                "pub struct A;\nimpl A {\n    pub fn new() -> Self { A }\n}\npub struct B;\nimpl B {\n    pub fn new() -> Self { B }\n}\n",
            ),
        ]);
        let new_at = |line| CallgraphExport {
            file: root.join("src/guards.rs"),
            symbol: "new".to_string(),
            kind: "method".to_string(),
            line,
        };
        let exports = vec![
            export(&root, "src/main.rs", "main", "function"),
            new_at(3),
            new_at(7),
        ];
        let entry_points = [root.join("src/main.rs")].into_iter().collect();
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(paths, exports, Vec::new(), entry_points),
        ));

        assert_eq!(aggregate["count"], 1, "{aggregate:#}");
        let item = aggregate_item(&aggregate, "src/guards.rs", "new").expect("new item");
        assert_eq!(item["line"], 3, "{item:#}");
        assert_eq!(item["other_lines"], json!([7]), "{item:#}");
    }
}

#[cfg(test)]
#[path = "dead_code/projected_tests.rs"]
mod projected_tests;
