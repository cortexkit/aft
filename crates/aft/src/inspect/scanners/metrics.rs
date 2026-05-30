use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(debug_assertions)]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Serialize;

use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::cache::Tier1FileMemo;
use crate::parser::{detect_language, LangId};

#[derive(Debug, Clone, Default, Serialize)]
struct MetricCounts {
    file_count: usize,
    symbol_count: usize,
    loc: usize,
}

impl MetricCounts {
    fn add_file(&mut self, file: &FileMetric) {
        self.file_count += 1;
        self.symbol_count += file.symbol_count;
        self.loc += file.loc;
    }
}

#[derive(Debug, Clone)]
struct CachedFileMetric {
    path: PathBuf,
    language: &'static str,
    loc: usize,
}

#[derive(Debug, Clone)]
struct FileMetric {
    path: PathBuf,
    language: &'static str,
    symbol_count: usize,
    loc: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TopFileMetric {
    file: String,
    loc: usize,
    symbol_count: usize,
}

static METRICS_MEMO: OnceLock<Tier1FileMemo<CachedFileMetric>> = OnceLock::new();

#[cfg(debug_assertions)]
static FILE_READS: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();

pub fn run_metrics_scan(job: &crate::inspect::InspectJob) -> crate::inspect::InspectResult {
    let started = Instant::now();
    let per_file = job
        .scope_files
        .par_iter()
        .map(|path| {
            let cached = metrics_memo().get_or_insert_with(path, scan_file);
            FileMetric {
                path: cached.path,
                language: cached.language,
                symbol_count: cached_symbol_count(path, job),
                loc: cached.loc,
            }
        })
        .collect::<Vec<_>>();
    let aggregate = aggregate_metrics(&job.project_root, &per_file);
    let success = crate::inspect::InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions: Vec::new(),
        aggregate,
    };

    crate::inspect::InspectResult::success(job, success, started.elapsed())
}

fn metrics_memo() -> &'static Tier1FileMemo<CachedFileMetric> {
    METRICS_MEMO.get_or_init(Tier1FileMemo::default)
}

fn scan_file(path: &Path) -> (Option<FileFreshness>, CachedFileMetric) {
    let metadata = fs::metadata(path).ok();
    let bytes = read_file_bytes(path);
    let freshness = metadata
        .as_ref()
        .map(|metadata| freshness_from_metadata(metadata, bytes.as_deref()));
    let loc = bytes.as_deref().map(line_count_bytes).unwrap_or_default();

    let metric = CachedFileMetric {
        path: path.to_path_buf(),
        language: language_key(path),
        loc,
    };
    (freshness, metric)
}

fn freshness_from_metadata(metadata: &fs::Metadata, bytes: Option<&[u8]>) -> FileFreshness {
    let size = metadata.len();
    let content_hash = if size <= cache_freshness::CONTENT_HASH_SIZE_CAP {
        bytes
            .map(cache_freshness::hash_bytes)
            .unwrap_or_else(cache_freshness::zero_hash)
    } else {
        cache_freshness::zero_hash()
    };

    FileFreshness {
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        size,
        content_hash,
    }
}

fn read_file_bytes(path: &Path) -> Option<Vec<u8>> {
    #[cfg(debug_assertions)]
    bump_file_read_count(path);
    fs::read(path).ok()
}

fn cached_symbol_count(path: &Path, job: &crate::inspect::InspectJob) -> usize {
    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };
    let Ok(mtime) = metadata.modified() else {
        return 0;
    };
    let Ok(cache) = job.symbol_cache.read() else {
        return 0;
    };

    cache
        .symbol_count_if_metadata_matches(path, mtime, metadata.len())
        .unwrap_or(0)
}

fn line_count_bytes(content: &[u8]) -> usize {
    content.iter().filter(|byte| **byte == b'\n').count() + 1
}

fn aggregate_metrics(project_root: &Path, per_file: &[FileMetric]) -> serde_json::Value {
    let mut totals = MetricCounts::default();
    let mut by_language = BTreeMap::<&'static str, MetricCounts>::new();
    let mut top_files = Vec::with_capacity(per_file.len());

    for file in per_file {
        totals.add_file(file);
        by_language.entry(file.language).or_default().add_file(file);
        top_files.push(TopFileMetric {
            file: display_path(project_root, &file.path),
            loc: file.loc,
            symbol_count: file.symbol_count,
        });
    }

    top_files.sort_by(|left, right| {
        right
            .loc
            .cmp(&left.loc)
            .then_with(|| left.file.cmp(&right.file))
    });
    top_files.truncate(20);

    serde_json::json!({
        "files": totals.file_count,
        "symbols": totals.symbol_count,
        "loc": totals.loc,
        "totals": totals,
        "by_language": by_language,
        "top_files_by_loc": top_files,
    })
}

fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn language_key(path: &Path) -> &'static str {
    match detect_language(path) {
        Some(LangId::TypeScript) => "typescript",
        Some(LangId::Tsx) => "tsx",
        Some(LangId::JavaScript) => "javascript",
        Some(LangId::Python) => "python",
        Some(LangId::Rust) => "rust",
        Some(LangId::Go) => "go",
        Some(LangId::C) => "c",
        Some(LangId::Cpp) => "cpp",
        Some(LangId::Zig) => "zig",
        Some(LangId::CSharp) => "csharp",
        Some(LangId::Bash) => "bash",
        Some(LangId::Html) => "html",
        Some(LangId::Markdown) => "markdown",
        Some(LangId::Yaml) => "yaml",
        Some(LangId::Solidity) => "solidity",
        Some(LangId::Vue) => "vue",
        Some(LangId::Json) => "json",
        Some(LangId::Scala) => "scala",
        Some(LangId::Java) => "java",
        Some(LangId::Ruby) => "ruby",
        Some(LangId::Kotlin) => "kotlin",
        Some(LangId::Swift) => "swift",
        Some(LangId::Php) => "php",
        Some(LangId::Lua) => "lua",
        Some(LangId::Perl) => "perl",
        None => "unknown",
    }
}

#[cfg(debug_assertions)]
fn debug_file_reads() -> &'static Mutex<BTreeMap<PathBuf, usize>> {
    FILE_READS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(debug_assertions)]
fn bump_file_read_count(path: &Path) {
    if let Ok(mut reads) = debug_file_reads().lock() {
        *reads.entry(path.to_path_buf()).or_default() += 1;
    }
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_file_read_count_for_debug(project_root: &Path) {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    if let Ok(mut reads) = debug_file_reads().lock() {
        reads.retain(|path, _| !path.starts_with(&project_root));
    }
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn file_read_count_for_debug(project_root: &Path) -> usize {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    debug_file_reads()
        .lock()
        .map(|reads| {
            reads
                .iter()
                .filter(|(path, _)| path.starts_with(&project_root))
                .map(|(_, count)| *count)
                .sum()
        })
        .unwrap_or_default()
}
