use std::collections::{BTreeMap, BinaryHeap};
use std::fs;
use std::path::{Path, PathBuf};
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
    skipped_reason: Option<String>,
    /// The file disappeared between the walk and this read, or is AFT's own
    /// storage that `project_content` excludes from project source: it
    /// contributes nothing, as the next walk will see it.
    vanished: bool,
}

#[derive(Debug, Clone)]
struct FileMetric {
    path: PathBuf,
    language: &'static str,
    symbol_count: usize,
    /// False when the parse cache had no current entry for this file.
    symbols_counted: bool,
    loc: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TopFileMetric {
    file: String,
    loc: usize,
    symbol_count: usize,
}

static METRICS_MEMO: OnceLock<Tier1FileMemo<CachedFileMetric>> = OnceLock::new();

/// Rows in `top_files_by_loc`.
const TOP_FILES_LIMIT: usize = 20;

#[cfg(debug_assertions)]
static FILE_READS: super::read_counts::FileReadCounts = super::read_counts::FileReadCounts::new();

pub fn run_metrics_scan(job: &crate::inspect::InspectJob) -> crate::inspect::InspectResult {
    #[cfg(debug_assertions)]
    wait_at_metrics_gate_for_debug(&job.project_root);
    run_metrics_scan_with_memo(job, metrics_memo())
}

#[cfg(debug_assertions)]
fn wait_at_metrics_gate_for_debug(project_root: &Path) {
    let Some(root) = std::env::var_os("AFT_TEST_METRICS_GATE_ROOT") else {
        return;
    };
    if crate::path_identity::canonicalize_normalized(Path::new(&root))
        != crate::path_identity::canonicalize_normalized(project_root)
    {
        return;
    }
    let ready = std::env::var_os("AFT_TEST_METRICS_GATE_READY").map(PathBuf::from);
    let release = std::env::var_os("AFT_TEST_METRICS_GATE_RELEASE").map(PathBuf::from);
    if let (Some(ready), Some(release)) = (ready, release) {
        fs::write(ready, b"ready").expect("signal metrics gate");
        // The release file controls ordering; the deadline only catches a
        // broken fixture rather than allowing a scanner to hang the suite.
        let hang_deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !release.exists() {
            assert!(Instant::now() < hang_deadline, "metrics gate not released");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn run_metrics_scan_with_memo(
    job: &crate::inspect::InspectJob,
    memo: &Tier1FileMemo<CachedFileMetric>,
) -> crate::inspect::InspectResult {
    let started = Instant::now();
    memo.reserve_for_scan(job.scope_files.len());
    let progress = crate::tool_progress::FileProgress::current();
    let cached_files = job
        .scope_files
        .par_iter()
        .map(|path| memo.get_or_insert_with(path, scan_file))
        .inspect(|_| {
            if let Some(progress) = &progress {
                progress.finished();
            }
        })
        .collect::<Vec<_>>();
    if job.is_full_project_scope() {
        memo.prune_to_scope(&job.project_root, &job.scope_files);
    }
    let mut skipped_files = Vec::new();
    let per_file = cached_files
        .into_iter()
        .filter_map(|cached| {
            if cached.vanished {
                return None;
            }
            if let Some(reason) = cached.skipped_reason {
                skipped_files.push((display_path(&job.project_root, &cached.path), reason));
                return None;
            }
            let symbol_count = cached_symbol_count(&cached.path, job);
            Some(FileMetric {
                symbol_count: symbol_count.unwrap_or(0),
                symbols_counted: symbol_count.is_some(),
                path: cached.path,
                language: cached.language,
                loc: cached.loc,
            })
        })
        .collect::<Vec<_>>();
    let mut aggregate = aggregate_metrics(&job.project_root, &per_file);
    super::source_text::write_skipped_files(&mut aggregate, skipped_files, None);
    let success = crate::inspect::InspectScanSuccess {
        scanned_files: per_file.iter().map(|file| file.path.clone()).collect(),
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
    let _permit =
        super::source_text::source_scan_permit(metadata.as_ref().map_or(0, fs::Metadata::len));
    let mut metric = CachedFileMetric {
        path: path.to_path_buf(),
        language: language_key(path),
        loc: 0,
        skipped_reason: None,
        vanished: false,
    };
    // Only a file whose bytes were read is memoised. A skip or a vanished file
    // is rechecked on the next scan: a permission change alters neither mtime
    // nor size, so a memoised failure would outlive the fix.
    let freshness = match read_file_bytes(path) {
        MetricRead::Bytes(bytes) => {
            metric.loc = line_count_bytes(&bytes);
            metadata
                .as_ref()
                .map(|metadata| freshness_from_metadata(metadata, Some(&bytes)))
        }
        MetricRead::Skipped(reason) => {
            metric.skipped_reason = Some(reason);
            None
        }
        MetricRead::Vanished => {
            metric.vanished = true;
            None
        }
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

enum MetricRead {
    Bytes(Vec<u8>),
    Skipped(String),
    Vanished,
}

fn read_file_bytes(path: &Path) -> MetricRead {
    use super::source_text::{read_source_bytes, unreadable_reason, SourceBytes, TOO_LARGE};
    match read_source_bytes(path) {
        Ok(SourceBytes::Bytes(bytes)) => {
            #[cfg(debug_assertions)]
            bump_file_read_count(path);
            MetricRead::Bytes(bytes)
        }
        Ok(SourceBytes::TooLarge) => MetricRead::Skipped(TOO_LARGE.to_string()),
        Ok(SourceBytes::Vanished) => MetricRead::Vanished,
        Err(error) => MetricRead::Skipped(unreadable_reason(&error)),
    }
}

/// Symbols come from the session's parse cache, so a file nobody has parsed
/// yet has no count. `None` keeps that apart from a file with zero symbols.
fn cached_symbol_count(path: &Path, job: &crate::inspect::InspectJob) -> Option<usize> {
    let metadata = fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let cache = job.symbol_cache.read().ok()?;
    cache.symbol_count_if_metadata_matches(path, mtime, metadata.len())
}

/// Same rule as `source_text::count_lines_bounded`: a trailing newline ends
/// the last line rather than starting another, and an empty file has no lines.
fn line_count_bytes(content: &[u8]) -> usize {
    let newlines = content.iter().filter(|byte| **byte == b'\n').count();
    newlines + usize::from(content.last().is_some_and(|byte| *byte != b'\n'))
}

fn aggregate_metrics(project_root: &Path, per_file: &[FileMetric]) -> serde_json::Value {
    let mut totals = MetricCounts::default();
    let mut by_language = BTreeMap::<&'static str, MetricCounts>::new();
    let symbols_counted_files = per_file.iter().filter(|file| file.symbols_counted).count();
    // Only the top rows are reported, so keep a bounded heap during the same
    // pass instead of building, then sorting, a row for every file.
    let mut top_files = BinaryHeap::<RankedTopFile>::with_capacity(TOP_FILES_LIMIT + 1);

    for file in per_file {
        totals.add_file(file);
        by_language.entry(file.language).or_default().add_file(file);
        if top_files.len() == TOP_FILES_LIMIT
            && top_files.peek().is_some_and(|worst| file.loc < worst.0.loc)
        {
            continue;
        }
        top_files.push(RankedTopFile(top_file_row(project_root, file)));
        if top_files.len() > TOP_FILES_LIMIT {
            top_files.pop();
        }
    }
    let top_files = top_files
        .into_sorted_vec()
        .into_iter()
        .map(|ranked| ranked.0)
        .collect::<Vec<_>>();

    serde_json::json!({
        "files": totals.file_count,
        "symbols": totals.symbol_count,
        // `symbols` is a lower bound unless every counted file had a current
        // parse-cache entry.
        "symbols_complete": symbols_counted_files == per_file.len(),
        "symbols_counted_files": symbols_counted_files,
        "loc": totals.loc,
        "totals": totals,
        "by_language": by_language,
        "top_files_by_loc": top_files,
    })
}

fn top_file_row(project_root: &Path, file: &FileMetric) -> TopFileMetric {
    #[cfg(test)]
    TOP_FILE_ROWS_BUILT.with(|built| built.set(built.get() + 1));
    TopFileMetric {
        file: display_path(project_root, &file.path),
        loc: file.loc,
        symbol_count: file.symbol_count,
    }
}

#[cfg(test)]
thread_local! {
    static TOP_FILE_ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Report order: more lines first, then file name. A row that ranks earlier
/// compares as smaller, so the max-heap's top is the row to evict.
struct RankedTopFile(TopFileMetric);

impl Ord for RankedTopFile {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .0
            .loc
            .cmp(&self.0.loc)
            .then_with(|| self.0.file.cmp(&other.0.file))
    }
}

impl PartialOrd for RankedTopFile {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankedTopFile {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedTopFile {}

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
        Some(LangId::Cuda) => "cuda",
        Some(LangId::Metal) => "metal",
        Some(LangId::Zig) => "zig",
        Some(LangId::CSharp) => "csharp",
        Some(LangId::Bash) => "bash",
        Some(LangId::Html) => "html",
        Some(LangId::Markdown) => "markdown",
        Some(LangId::Yaml) => "yaml",
        Some(LangId::Solidity) => "solidity",
        Some(LangId::Scss) => "scss",
        Some(LangId::Vue) => "vue",
        Some(LangId::Json) => "json",
        Some(LangId::Scala) => "scala",
        Some(LangId::Java) => "java",
        Some(LangId::Ruby) => "ruby",
        Some(LangId::Kotlin) => "kotlin",
        Some(LangId::Swift) => "swift",
        Some(LangId::Php) => "php",
        Some(LangId::Lua) => "lua",
        Some(LangId::Luau) => "luau",
        Some(LangId::Perl) => "perl",
        Some(LangId::Pascal) => "pascal",
        Some(LangId::R) => "r",
        Some(LangId::Groovy) => "groovy",
        Some(LangId::ObjC) => "objc",
        Some(LangId::Toml) => "toml",
        None => "unknown",
    }
}

#[cfg(debug_assertions)]
fn bump_file_read_count(path: &Path) {
    FILE_READS.record(path);
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn reset_file_read_count_for_debug(project_root: &Path) {
    // Normalized like the scanned paths themselves: bare canonicalize is
    // verbatim on Windows and the starts_with filter would miss every entry.
    let project_root = crate::path_identity::canonicalize_normalized(project_root);
    FILE_READS.reset(project_root);
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn file_read_count_for_debug(project_root: &Path) -> usize {
    // Normalized like the scanned paths themselves: bare canonicalize is
    // verbatim on Windows and the starts_with filter would miss every entry.
    let project_root = crate::path_identity::canonicalize_normalized(project_root);
    FILE_READS.count(&project_root)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use std::fs;
    use std::sync::{Arc, RwLock};

    use super::*;

    #[test]
    fn metrics_line_count_matches_the_bounded_line_counter() {
        for (content, expected) in [
            (b"".as_slice(), 0),
            (b"\n".as_slice(), 1),
            (b"a".as_slice(), 1),
            (b"a\nb\n".as_slice(), 2),
            (b"a\nb".as_slice(), 2),
        ] {
            assert_eq!(line_count_bytes(content), expected, "{content:?}");
        }
    }

    #[test]
    fn audit05_metrics_read_tracking_is_root_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.rs");
        fs::write(&path, "fn f() {}\n").unwrap();
        let _ = read_file_bytes(&path);
        assert_eq!(file_read_count_for_debug(dir.path()), 0);
        reset_file_read_count_for_debug(dir.path());
        let _ = read_file_bytes(&path);
        assert_eq!(file_read_count_for_debug(dir.path()), 1);
    }
    use crate::config::Config;
    use crate::inspect::{InspectCategory, InspectJob, JobKey, JobScope};
    use crate::parser::SymbolCache;

    const OVER_DEFAULT_MEMO_CAPACITY: usize = 4_097;

    fn metrics_job(root: &Path, scope_files: Vec<PathBuf>) -> InspectJob {
        let scope = JobScope::for_project(root.to_path_buf());
        InspectJob {
            job_id: 1,
            key: JobKey::for_category_scope(InspectCategory::Metrics, &scope),
            category: InspectCategory::Metrics,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join("inspect"),
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

    #[test]
    fn call_keepalive_inspect_counts_parallel_file_completions() {
        let project = tempfile::tempdir().unwrap();
        let files = (0..3)
            .map(|index| {
                let path = project.path().join(format!("source-{index}.rs"));
                fs::write(&path, "fn work() { if true { return; } }\n").unwrap();
                path
            })
            .collect();
        let mut job = metrics_job(project.path(), files);
        let progress = crate::tool_progress::FileProgress::default();
        let guard = progress.enter();
        assert_eq!(progress.completed(), 0);
        assert!(run_metrics_scan(&job).outcome.is_ok());
        assert_eq!(progress.completed(), 3);
        job.category = InspectCategory::Duplicates;
        assert!(super::super::duplicates::run_duplicates_scan(&job)
            .outcome
            .is_ok());
        assert_eq!(progress.completed(), 6);
        job.category = InspectCategory::Complexity;
        assert!(super::super::complexity::run_complexity_scan(&job)
            .outcome
            .is_ok());
        assert_eq!(progress.completed(), 9);
        drop(guard);
        assert!(crate::tool_progress::FileProgress::current().is_none());
        assert_eq!(
            progress.completed(),
            9,
            "waiting does not finish more files"
        );
    }

    fn file_metric(root: &Path, name: &str, loc: usize) -> FileMetric {
        FileMetric {
            path: root.join(name),
            language: "rust",
            symbol_count: 0,
            symbols_counted: true,
            loc,
        }
    }

    #[test]
    fn metrics_top_files_match_a_full_sort_with_ties_at_the_cutoff() {
        let root = Path::new("/project");
        // 30 files over three LOC values, so the 20-row cutoff falls inside a
        // tie and the file name decides which tied rows stay.
        let per_file = (0..30)
            .rev()
            .map(|index| file_metric(root, &format!("f{index:02}.rs"), 10 * (index % 3 + 1)))
            .collect::<Vec<_>>();
        let mut oracle = per_file
            .iter()
            .map(|file| (file.loc, display_path(root, &file.path)))
            .collect::<Vec<_>>();
        oracle.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        oracle.truncate(TOP_FILES_LIMIT);

        let aggregate = aggregate_metrics(root, &per_file);
        let rows = aggregate["top_files_by_loc"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["loc"].as_u64().unwrap() as usize,
                    row["file"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(rows, oracle);
        assert_eq!(aggregate["files"], 30);
        assert_eq!(aggregate["loc"], 10 * (10 + 20 + 30));
    }

    #[test]
    fn metrics_top_files_build_rows_only_for_candidates() {
        let root = Path::new("/project");
        // Descending LOC: once the first 20 rows are kept, no later file can
        // enter the preview, so none of them needs a row.
        let per_file = (0..1_000)
            .map(|index| file_metric(root, &format!("f{index:04}.rs"), 1_000 - index))
            .collect::<Vec<_>>();
        TOP_FILE_ROWS_BUILT.with(|built| built.set(0));
        let aggregate = aggregate_metrics(root, &per_file);
        assert_eq!(
            TOP_FILE_ROWS_BUILT.with(std::cell::Cell::get),
            TOP_FILES_LIMIT,
            "a preview row was built for a file that could not enter it"
        );
        assert_eq!(aggregate["top_files_by_loc"][0]["loc"], 1_000);
        assert_eq!(aggregate["files"], 1_000);
    }

    #[test]
    fn metrics_vanished_files_are_not_counted() {
        let project = tempfile::tempdir().unwrap();
        let present = project.path().join("present.rs");
        fs::write(&present, "fn present() {}\n").unwrap();
        let gone = project.path().join("gone.rs");
        let job = metrics_job(project.path(), vec![present.clone(), gone]);
        let success = run_metrics_scan_with_memo(&job, &Tier1FileMemo::default())
            .outcome
            .unwrap();
        assert_eq!(success.scanned_files, vec![present]);
        assert_eq!(success.aggregate["files"], 1);
        assert!(success.aggregate.get("skipped_files").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn metrics_unreadable_file_is_a_named_gap_and_is_not_memoised() {
        let project = tempfile::tempdir().unwrap();
        let readable = project.path().join("readable.rs");
        fs::write(&readable, "fn readable() {}\n").unwrap();
        // A FIFO fails the source read with an error other than NotFound,
        // even for a privileged user, so it stands in for an unreadable file.
        let unreadable = project.path().join("unreadable.rs");
        crate::bounded_io::tests::fifo(&unreadable);
        let job = metrics_job(project.path(), vec![readable.clone(), unreadable.clone()]);
        let memo = Tier1FileMemo::default();

        let success = run_metrics_scan_with_memo(&job, &memo).outcome.unwrap();
        assert_eq!(success.scanned_files, vec![readable.clone()]);
        assert_eq!(success.aggregate["files"], 1);
        assert_eq!(success.aggregate["loc"], 1);
        assert_eq!(success.aggregate["complete"], false);
        let rows = success.aggregate["skipped_files"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["file"], "unreadable.rs");
        assert!(
            rows[0]["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("could not be read: ")),
            "{rows:?}"
        );

        // The failure was not memoised: once the file reads, the same memo
        // counts its lines.
        fs::remove_file(&unreadable).unwrap();
        fs::write(&unreadable, "a\nb\nc\n").unwrap();
        let success = run_metrics_scan_with_memo(&job, &memo).outcome.unwrap();
        assert_eq!(success.aggregate["files"], 2);
        assert_eq!(success.aggregate["loc"], 4);
        assert!(success.aggregate.get("skipped_files").is_none());
    }

    #[test]
    fn full_scope_larger_than_default_capacity_stays_warm() {
        let project = tempfile::tempdir().expect("project");
        let mut files = Vec::with_capacity(OVER_DEFAULT_MEMO_CAPACITY);
        for index in 0..OVER_DEFAULT_MEMO_CAPACITY {
            let path = project.path().join(format!("file-{index:04}.rs"));
            fs::write(
                &path,
                format!("pub fn value_{index}() -> usize {{ {index} }}\n"),
            )
            .expect("write fixture");
            files.push(path);
        }
        let job = metrics_job(project.path(), files);
        let memo = Tier1FileMemo::default();

        assert!(run_metrics_scan_with_memo(&job, &memo).outcome.is_ok());
        reset_file_read_count_for_debug(project.path());
        assert!(run_metrics_scan_with_memo(&job, &memo).outcome.is_ok());

        assert_eq!(
            file_read_count_for_debug(project.path()),
            0,
            "an unchanged full scan must retain every live file above the default memo capacity"
        );
    }
}
