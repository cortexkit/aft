//! Pure oxc-backed TS/JS module graph facts and export-liveness verdicts.
//!
//! H1-1 intentionally stops at an engine/API boundary: scanners wire this into
//! inspect contributions in H1-2. The engine is pure (files in, verdicts out)
//! and keeps only an in-memory facts cache keyed by file content hash + parser
//! source type + facts format. That is enough for the required warm-cache perf gate while
//! avoiding premature persistence coupling to InspectCache/AppContext.

mod facts;
mod graph;
mod resolver;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use oxc_span::SourceType;

use facts::parse_file_facts;
use graph::compute_verdicts;
use resolver::{normalize_path, ModuleResolver};
pub use types::{
    DecoratorFact, DynamicImportFact, ExportFact, ExportName, ExportUsage, FileFacts, FileId,
    ImportFact, ImportKind, LivenessVerdict, OxcEngineError, OxcEngineResult, OxcEngineStats,
    OxcExportVerdict, OxcFileVerdicts, OxcReExportContext, OxcResolvedEdge, ReExportFact,
    ReExportKind, ResolverConfigInput, OXC_PROVENANCE,
};

pub(crate) const FACTS_FORMAT_VERSION: u32 = 5;

#[derive(Debug, Clone, Default)]
pub struct AnalyzeOptions {
    pub entry_points: Vec<PathBuf>,
    pub public_api_files: Vec<PathBuf>,
    pub executable_root_exports: BTreeMap<PathBuf, BTreeSet<String>>,
    /// Files already proven stale by the inspect freshness layer. These paths
    /// bypass the path metadata fast path so same-size/same-mtime edits are
    /// still re-read and content-hashed before facts are reused.
    pub force_reparse_files: Vec<PathBuf>,
    /// When true, imports/re-exports only make targets live after execution is
    /// reachable from entry/public files. Used by dead_code; unused_exports keeps
    /// the default import-usage semantics.
    pub entry_reachability: bool,
}

#[derive(Debug, Clone, Default)]
pub struct OxcFactsCache {
    entries_by_hash: BTreeMap<String, FileFacts>,
    entries_by_path: BTreeMap<PathBuf, OxcFactsPathEntry>,
}

#[derive(Debug, Clone)]
struct OxcFactsPathEntry {
    mtime: SystemTime,
    size: u64,
    cache_key: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OxcFactsCacheStats {
    pub hits: usize,
    pub misses: usize,
}

impl OxcFactsCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries_by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries_by_hash.is_empty()
    }

    fn facts_for_file(
        &mut self,
        file_id: FileId,
        path: &Path,
        force_reparse: bool,
        stats: &mut OxcFactsCacheStats,
    ) -> std::io::Result<FileFacts> {
        let source_type = SourceType::from_path(path).unwrap_or_default();
        let source_type_key = source_type_cache_key(source_type);
        let metadata = fs::metadata(path)?;
        let mtime = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        let size = metadata.len();
        let path_key = path.to_path_buf();

        if !force_reparse {
            if let Some(entry) = self.entries_by_path.get(&path_key) {
                if entry.mtime == mtime && entry.size == size {
                    if let Some(cached) = self.entries_by_hash.get(&entry.cache_key) {
                        stats.hits += 1;
                        return Ok(rebind_facts(cached, file_id, path, &cached.content_hash));
                    }
                }
            }
        }

        let source = fs::read_to_string(path)?;
        Ok(self.facts_for_source_with_metadata(
            file_id,
            path,
            &source,
            source_type,
            source_type_key,
            Some((mtime, size)),
            stats,
        ))
    }

    fn facts_for_source_with_metadata(
        &mut self,
        file_id: FileId,
        path: &Path,
        source: &str,
        source_type: SourceType,
        source_type_key: String,
        metadata: Option<(SystemTime, u64)>,
        stats: &mut OxcFactsCacheStats,
    ) -> FileFacts {
        let content_hash = crate::cache_freshness::hash_bytes(source.as_bytes())
            .to_hex()
            .to_string();
        let cache_key = format!("v{FACTS_FORMAT_VERSION}:{source_type_key}:{content_hash}");
        if let Some(cached) = self.entries_by_hash.get(&cache_key) {
            stats.hits += 1;
            if let Some((mtime, size)) = metadata {
                self.entries_by_path.insert(
                    path.to_path_buf(),
                    OxcFactsPathEntry {
                        mtime,
                        size,
                        cache_key,
                    },
                );
            }
            return rebind_facts(cached, file_id, path, &content_hash);
        }

        stats.misses += 1;
        let facts = parse_file_facts(file_id, path, source, content_hash, source_type);
        self.entries_by_hash
            .insert(cache_key.clone(), facts.clone());
        if let Some((mtime, size)) = metadata {
            self.entries_by_path.insert(
                path.to_path_buf(),
                OxcFactsPathEntry {
                    mtime,
                    size,
                    cache_key,
                },
            );
        }
        facts
    }
}

fn rebind_facts(cached: &FileFacts, file_id: FileId, path: &Path, content_hash: &str) -> FileFacts {
    let mut facts = cached.clone();
    facts.file_id = file_id;
    facts.path = path.to_path_buf();
    facts.content_hash = content_hash.to_string();
    facts
}

fn source_type_cache_key(source_type: SourceType) -> String {
    let language = if source_type.is_typescript_definition() {
        "dts"
    } else if source_type.is_typescript() {
        "ts"
    } else {
        "js"
    };
    let module_kind = if source_type.is_commonjs() {
        "commonjs"
    } else if source_type.is_module() {
        "module"
    } else if source_type.is_script() {
        "script"
    } else {
        "unambiguous"
    };
    let variant = if source_type.is_jsx() {
        "jsx"
    } else {
        "standard"
    };

    format!("{language}:{module_kind}:{variant}")
}

pub fn analyze_files(
    project_root: &Path,
    files: &[PathBuf],
    options: AnalyzeOptions,
) -> Result<OxcEngineResult, String> {
    let mut cache = OxcFactsCache::new();
    analyze_files_with_cache(project_root, files, options, &mut cache)
}

pub fn analyze_files_with_cache(
    project_root: &Path,
    files: &[PathBuf],
    options: AnalyzeOptions,
    cache: &mut OxcFactsCache,
) -> Result<OxcEngineResult, String> {
    // De-verbatim the canonical root (Windows \\?\ form) so strip_prefix
    // against normalize_path-built module paths keeps working.
    let project_root = fs::canonicalize(project_root)
        .map(|canonical| normalize_path(&canonical))
        .unwrap_or_else(|_| normalize_path(project_root));
    let force_reparse_files = normalize_option_paths(&options.force_reparse_files);
    let normalized_files = normalize_file_set(&project_root, files);
    let files = normalized_files.files;
    let skipped_outside_root = normalized_files.skipped_outside_root;
    let mut cache_stats = OxcFactsCacheStats::default();
    let mut errors = Vec::new();
    let mut facts = Vec::with_capacity(files.len());

    for (idx, path) in files.iter().enumerate() {
        match cache.facts_for_file(
            FileId(idx),
            path,
            force_reparse_files.contains(path),
            &mut cache_stats,
        ) {
            Ok(file_facts) => facts.push(file_facts),
            Err(error) => errors.push(OxcEngineError {
                file: path.clone(),
                message: format!("read: {error}"),
            }),
        }
    }

    Ok(analyze_preparsed_facts(
        project_root,
        facts,
        options,
        cache_stats,
        errors,
        skipped_outside_root,
    ))
}

pub(crate) fn analyze_file_facts(
    project_root: &Path,
    facts: Vec<FileFacts>,
    options: AnalyzeOptions,
    skipped_outside_root: Vec<PathBuf>,
) -> OxcEngineResult {
    // Same de-verbatim rule as analyze_files_with_cache: FileFacts paths are
    // normalize_path-built, so the root they are relativized against must be too.
    let project_root = fs::canonicalize(project_root)
        .map(|canonical| normalize_path(&canonical))
        .unwrap_or_else(|_| normalize_path(project_root));
    analyze_preparsed_facts(
        project_root,
        facts,
        options,
        OxcFactsCacheStats::default(),
        Vec::new(),
        skipped_outside_root,
    )
}

fn analyze_preparsed_facts(
    project_root: PathBuf,
    mut facts: Vec<FileFacts>,
    options: AnalyzeOptions,
    cache_stats: OxcFactsCacheStats,
    mut errors: Vec<OxcEngineError>,
    skipped_outside_root: Vec<PathBuf>,
) -> OxcEngineResult {
    // Preserve dense FileId indexing when unreadable files were skipped or facts
    // were reconstructed from contribution records.
    for (idx, fact) in facts.iter_mut().enumerate() {
        fact.file_id = FileId(idx);
        if let Some(parse_error) = &fact.parse_error {
            errors.push(OxcEngineError {
                file: fact.path.clone(),
                message: format!("parse: {parse_error}"),
            });
        }
    }
    let resolved_files = facts
        .iter()
        .map(|fact| fact.path.clone())
        .collect::<Vec<_>>();
    let resolver = ModuleResolver::new(&project_root, &resolved_files);
    let (resolved_modules, tracker, edges) = resolver.resolve_modules(&facts);
    let entry_points = normalize_option_paths(&options.entry_points);
    let public_api_files = normalize_option_paths(&options.public_api_files);
    let executable_root_exports =
        normalize_executable_root_exports(&options.executable_root_exports);
    let file_verdicts = compute_verdicts(
        &project_root,
        &resolved_modules,
        &entry_points,
        &public_api_files,
        &executable_root_exports,
        options.entry_reachability,
    );
    let resolved_edges = edges
        .iter()
        .filter(|edge| edge.resolved_file.is_some())
        .count();
    let unresolved_edges = edges.len().saturating_sub(resolved_edges);
    let resolver_config_inputs = tracker.inputs();
    let resolver_config_fingerprint = tracker.fingerprint();

    OxcEngineResult {
        files: file_verdicts,
        facts,
        resolver_config_inputs,
        resolver_config_fingerprint,
        edges,
        stats: OxcEngineStats {
            files: resolved_files.len(),
            cache_hits: cache_stats.hits,
            cache_misses: cache_stats.misses,
            resolved_edges,
            unresolved_edges,
        },
        errors,
        skipped_outside_root,
    }
}

#[derive(Debug, Default)]
struct NormalizedFileSet {
    files: Vec<PathBuf>,
    skipped_outside_root: Vec<PathBuf>,
}

fn normalize_file_set(project_root: &Path, files: &[PathBuf]) -> NormalizedFileSet {
    let mut normalized = NormalizedFileSet::default();
    for path in files.iter().filter(|path| is_ts_js_file(path)) {
        let path = normalize_input_path(project_root, path);
        if path.strip_prefix(project_root).is_ok() {
            normalized.files.push(path);
        } else {
            normalized.skipped_outside_root.push(path);
        }
    }

    normalized.files.sort();
    normalized.files.dedup();
    normalized.skipped_outside_root.sort();
    normalized.skipped_outside_root.dedup();
    normalized
}

pub(crate) fn normalize_input_path(project_root: &Path, path: &Path) -> PathBuf {
    // Route the canonicalized form through normalize_path too: on Windows,
    // fs::canonicalize returns verbatim (\\?\C:\) paths, while every set we
    // compare module paths against (entry_points, public_api_files,
    // executable_root_exports) is built via normalize_path, which strips the
    // verbatim prefix. Returning the raw canonical form makes those membership
    // checks silently miss on Windows only.
    fs::canonicalize(path)
        .map(|canonical| normalize_path(&canonical))
        .unwrap_or_else(|_| {
            if path.is_absolute() {
                normalize_path(path)
            } else {
                normalize_path(&project_root.join(path))
            }
        })
}

fn normalize_executable_root_exports(
    roots: &BTreeMap<PathBuf, BTreeSet<String>>,
) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let mut normalized = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for (path, exports) in roots {
        normalized
            .entry(normalize_path(path))
            .or_default()
            .extend(exports.iter().cloned());
        if let Ok(canonical) = fs::canonicalize(path) {
            normalized
                .entry(normalize_path(&canonical))
                .or_default()
                .extend(exports.iter().cloned());
        }
    }
    normalized
}

fn normalize_option_paths(paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    // Both insertions go through normalize_path (which de-verbatims Windows
    // \\?\ canonical forms) so membership checks against module paths built by
    // normalize_input_path always compare like with like.
    let mut normalized = BTreeSet::new();
    for path in paths {
        normalized.insert(normalize_path(path));
        if let Ok(canonical) = fs::canonicalize(path) {
            normalized.insert(normalize_path(&canonical));
        }
    }
    normalized
}

fn is_ts_js_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext,
                "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs"
            )
        })
}
