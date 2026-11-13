//! Reading a scope file as source text for the Tier-2 tree-sitter scanners
//! (complexity, duplicates), and reporting the files that could not be read
//! that way.
//!
//! A single file whose bytes are not valid UTF-8 (a GBK- or Latin-1-encoded
//! data file, for example) used to abort the whole category build. Such a file
//! is now a per-file skip: it contributes nothing, its contribution records the
//! reason, and the category reports it as a named gap
//! (`complete: false`, `skipped_files: [{file, reason}]`). The skip is stored
//! as the file's cached contribution, so it is not re-read until the file
//! changes, and a changed file that is now valid UTF-8 is analyzed normally.

use std::io::Read;
use std::path::Path;
#[cfg(any(debug_assertions, test))]
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

use serde_json::{json, Value};

/// Reason recorded for a file whose bytes are not valid UTF-8.
pub(crate) const NOT_VALID_UTF8: &str = "not valid UTF-8";

pub(crate) const MAX_SOURCE_BYTES: u64 = crate::search_index::DEFAULT_MAX_FILE_SIZE;
pub(crate) const MAX_GRAPH_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
// Agent-visible reason, approved by Ufuk; change it only with his approval.
pub(crate) const TOO_LARGE: &str = "over the 1 MiB size limit";
pub(crate) const GRAPH_TOO_LARGE: &str = "over the 4 MiB size limit";

// Charge source bytes, not file count, so small files can use the whole Rayon
// pool. Reservations stay live through parsing and tree traversal. Source-byte
// limits bound input, not the amplified size of parser trees and scratch space.
pub(super) const SOURCE_SCAN_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
static SOURCE_SCANS: SourceScanBudget = SourceScanBudget::new(SOURCE_SCAN_BUDGET_BYTES);

struct SourceScanBudget {
    in_flight: Mutex<u64>,
    available: Condvar,
    capacity: u64,
}

impl SourceScanBudget {
    const fn new(capacity: u64) -> Self {
        Self {
            in_flight: Mutex::new(0),
            available: Condvar::new(),
            capacity,
        }
    }

    fn reserve(&self, stat_size: u64) -> SourceScanPermit<'_> {
        self.reserve_with_limit(stat_size, MAX_SOURCE_BYTES)
    }

    fn reserve_with_limit(&self, stat_size: u64, limit: u64) -> SourceScanPermit<'_> {
        let bytes = stat_size.min(limit);
        let mut active = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        // One file must make progress even if the budget is smaller than that
        // file's reservation; other nonempty reservations wait for its release.
        while *active != 0 && bytes > self.capacity.saturating_sub(*active) {
            active = self
                .available
                .wait(active)
                .unwrap_or_else(|e| e.into_inner());
        }
        *active += bytes;
        SourceScanPermit {
            budget: self,
            bytes,
        }
    }
}

pub(crate) struct SourceScanPermit<'a> {
    budget: &'a SourceScanBudget,
    bytes: u64,
}

pub(crate) fn source_scan_permit(stat_size: u64) -> SourceScanPermit<'static> {
    SOURCE_SCANS.reserve(stat_size)
}

pub(crate) fn graph_source_permit(stat_size: u64) -> SourceScanPermit<'static> {
    SOURCE_SCANS.reserve_with_limit(stat_size, MAX_GRAPH_SOURCE_BYTES)
}

/// Generated and minified JS/TS inputs are deliberate graph exclusions, not
/// missing coverage. Name-only classifications avoid even a header read.
pub(crate) fn graph_source_excluded(project_root: &Path, path: &Path) -> bool {
    use crate::file_kind::ContentKind;
    use crate::parser::LangId;
    let _permit = source_scan_permit(crate::file_kind::HEAD_BYTES as u64);
    matches!(
        crate::parser::detect_language(path),
        Some(LangId::JavaScript | LangId::TypeScript | LangId::Tsx)
    ) && matches!(
        crate::file_kind::classify(project_root, path, None).content,
        ContentKind::Generated | ContentKind::Minified
    )
}

pub(crate) fn metadata_freshness(
    path: &Path,
) -> std::io::Result<crate::cache_freshness::FileFreshness> {
    let metadata = std::fs::metadata(path)?;
    Ok(crate::cache_freshness::FileFreshness {
        mtime: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        size: metadata.len(),
        content_hash: crate::cache_freshness::zero_hash(),
    })
}

impl Drop for SourceScanPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .budget
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *active -= self.bytes;
        self.budget.available.notify_all();
    }
}

pub(crate) enum SourceBytes {
    Bytes(Vec<u8>),
    TooLarge,
    Vanished,
}

/// Admit by metadata before opening, then bound the read too: a file can grow
/// between stat and read. The extra byte detects overflow, never a partial parse.
pub(crate) fn read_source_bytes(path: &Path) -> std::io::Result<SourceBytes> {
    read_source_bytes_with_limit(path, MAX_SOURCE_BYTES)
}

fn read_source_bytes_with_limit(path: &Path, limit: u64) -> std::io::Result<SourceBytes> {
    if crate::project_content::is_excluded(path) {
        return Ok(SourceBytes::Vanished);
    }
    let read = || -> std::io::Result<SourceBytes> {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > limit {
            return Ok(SourceBytes::TooLarge);
        }
        let file = crate::project_content::open(path)?;
        if file.metadata()?.len() > limit {
            return Ok(SourceBytes::TooLarge);
        }
        #[cfg(any(debug_assertions, test))]
        bump_source_read_count(path);
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        crate::io_accounting::WORK
            .content_read_bytes
            .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(if bytes.len() as u64 > limit {
            SourceBytes::TooLarge
        } else {
            SourceBytes::Bytes(bytes)
        })
    };
    match read() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SourceBytes::Vanished),
        result => result,
    }
}

/// Oversized contributions need metadata for invalidation, not a content hash.
/// Keep the returned reservation through the caller's parsing and traversal;
/// it also covers any source read needed to compute freshness.
pub(crate) fn source_freshness(
    path: &Path,
) -> std::io::Result<(
    crate::cache_freshness::FileFreshness,
    SourceScanPermit<'static>,
)> {
    source_freshness_with_limit(path, MAX_SOURCE_BYTES)
}

pub(crate) fn graph_source_freshness(
    path: &Path,
) -> std::io::Result<(
    crate::cache_freshness::FileFreshness,
    SourceScanPermit<'static>,
)> {
    source_freshness_with_limit(path, MAX_GRAPH_SOURCE_BYTES)
}

fn source_freshness_with_limit(
    path: &Path,
    limit: u64,
) -> std::io::Result<(
    crate::cache_freshness::FileFreshness,
    SourceScanPermit<'static>,
)> {
    let metadata = std::fs::metadata(path)?;
    let permit = SOURCE_SCANS.reserve_with_limit(metadata.len(), limit);
    if metadata.len() > limit {
        return Ok((
            crate::cache_freshness::FileFreshness {
                mtime: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
                size: metadata.len(),
                content_hash: crate::cache_freshness::zero_hash(),
            },
            permit,
        ));
    }
    crate::cache_freshness::collect(path).map(|freshness| (freshness, permit))
}

/// Most skipped files named in a project-wide aggregate. The total is always
/// reported in `skipped_files_count`, so a capped list is never mistaken for
/// the whole set.
pub(crate) const SKIPPED_FILES_LIMIT: usize = 20;

/// Outcome of reading one scope file as source text.
#[derive(Debug)]
pub(crate) enum SourceText {
    /// The file decoded as UTF-8.
    Text(String),
    /// The file was read but is not valid UTF-8; the scanner skips it.
    NotUtf8,
    /// The file exceeds the source limit and was not parsed.
    TooLarge,
    /// The file disappeared or is excluded from project source (for example,
    /// internal storage). No contribution, exactly as the next walk will see it.
    Vanished,
}

/// Read `path` as UTF-8 source text. Errors other than "not found" (for
/// example permission denied) are returned unchanged so each scanner keeps
/// its existing contract for them.
pub(crate) fn read_source_text(path: &Path) -> std::io::Result<SourceText> {
    read_source_text_with_limit(path, MAX_SOURCE_BYTES)
}

pub(crate) fn read_graph_source_text(path: &Path) -> std::io::Result<SourceText> {
    read_source_text_with_limit(path, MAX_GRAPH_SOURCE_BYTES)
}

fn read_source_text_with_limit(path: &Path, limit: u64) -> std::io::Result<SourceText> {
    match read_source_bytes_with_limit(path, limit)? {
        SourceBytes::Bytes(bytes) => Ok(match String::from_utf8(bytes) {
            Ok(text) => SourceText::Text(text),
            Err(_) => SourceText::NotUtf8,
        }),
        SourceBytes::TooLarge => Ok(SourceText::TooLarge),
        SourceBytes::Vanished => Ok(SourceText::Vanished),
    }
}

/// Record the named per-file gap on a category aggregate. Does nothing when
/// no file was skipped, so aggregates without skips are unchanged.
///
/// `skipped` holds `(file, reason)` pairs. The list is sorted by file; with a
/// `drill_down_limit` (the project-wide rollup) at most
/// [`SKIPPED_FILES_LIMIT`] entries are named, and `skipped_files_count` always
/// carries the full total. Scoped rollups pass no limit so scope filtering
/// sees every skipped file.
pub(crate) fn write_skipped_files(
    aggregate: &mut Value,
    mut skipped: Vec<(String, String)>,
    drill_down_limit: Option<usize>,
) {
    if skipped.is_empty() {
        return;
    }
    skipped.sort();
    let total = skipped.len();
    let named = if drill_down_limit.is_some() {
        SKIPPED_FILES_LIMIT
    } else {
        usize::MAX
    };
    let rows = skipped
        .into_iter()
        .take(named)
        .map(|(file, reason)| json!({ "file": file, "reason": reason }))
        .collect::<Vec<_>>();
    aggregate["complete"] = Value::Bool(false);
    aggregate["skipped_files"] = Value::Array(rows);
    aggregate["skipped_files_count"] = json!(total);
}

#[cfg(any(debug_assertions, test))]
static SOURCE_READS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<PathBuf, usize>>,
> = std::sync::OnceLock::new();

#[cfg(any(debug_assertions, test))]
fn bump_source_read_count(path: &Path) {
    let reads = SOURCE_READS.get_or_init(Default::default);
    if let Ok(mut reads) = reads.lock() {
        *reads.entry(path.to_path_buf()).or_default() += 1;
    }
}

/// How many times the Tier-2 scanners read `path` as source text in this
/// process. Debug and test builds only; proves a file was or was not opened.
#[cfg(any(debug_assertions, test))]
#[doc(hidden)]
pub fn source_read_count_for_debug(path: &Path) -> usize {
    SOURCE_READS
        .get()
        .and_then(|reads| reads.lock().ok().map(|reads| reads.get(path).copied()))
        .flatten()
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn handwritten_graph_source_for_test(bytes: usize, header: &str) -> String {
    assert!(header.ends_with('\n') && header.len() <= bytes);
    let line = "// handwritten source\n";
    let mut source = format!(
        "{header}{}",
        line.repeat((bytes - header.len()) / line.len())
    );
    source.push_str(&" ".repeat(bytes - source.len()));
    source
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_budget_charges_bytes_and_small_files_can_pass_a_waiting_large_file() {
        let budget = std::sync::Arc::new(SourceScanBudget::new(MAX_SOURCE_BYTES));
        let large = budget.reserve(3 * MAX_SOURCE_BYTES / 4);
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let worker_budget = budget.clone();
        let worker = std::thread::spawn(move || {
            let _permit = worker_budget.reserve(MAX_SOURCE_BYTES / 2);
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let small = budget.reserve(MAX_SOURCE_BYTES / 4);
        assert_eq!(*budget.in_flight.lock().unwrap(), MAX_SOURCE_BYTES);
        let blocked = entered_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err();
        drop(small);
        drop(large);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(*budget.in_flight.lock().unwrap(), MAX_SOURCE_BYTES / 2);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(
            blocked,
            "a reservation exceeded the shared source-byte budget"
        );
        assert_eq!(*budget.in_flight.lock().unwrap(), 0);
    }

    #[test]
    fn a_single_file_is_admitted_even_when_its_clamped_reservation_exceeds_the_budget() {
        let budget = std::sync::Arc::new(SourceScanBudget::new(MAX_SOURCE_BYTES / 2));
        let worker_budget = budget.clone();
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let worker = std::thread::spawn(move || {
            let permit = worker_budget.reserve(4 * MAX_SOURCE_BYTES);
            entered_tx.send(permit.bytes).unwrap();
            release_rx.recv().unwrap();
        });
        assert_eq!(
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("single file must not deadlock"),
            MAX_SOURCE_BYTES
        );
        assert_eq!(*budget.in_flight.lock().unwrap(), MAX_SOURCE_BYTES);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(*budget.in_flight.lock().unwrap(), 0);
    }

    /// Measure native parser allocations in an isolated Linux test process;
    /// Rust-only allocation counters do not include tree-sitter's C allocations.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "native parse-memory probe; requires AFT_TREE_MEMORY_LANGUAGE on Linux"]
    fn source_tree_memory_probe() {
        let language = std::env::var("AFT_TREE_MEMORY_LANGUAGE").unwrap();
        let (language_id, source) = match language.as_str() {
            "json" => (
                crate::parser::LangId::Json,
                format!(
                    "[{}0]",
                    "{\"k\":1},".repeat(MAX_SOURCE_BYTES as usize / 8 - 1)
                ),
            ),
            "rust" => {
                let mut source = String::new();
                let mut index = 0;
                while source.len() < MAX_SOURCE_BYTES as usize - 100 {
                    source.push_str(&format!(
                        "fn f{index}(x:i32)->i32{{if x>0{{x+1}}else{{x-1}}}}\n"
                    ));
                    index += 1;
                }
                (crate::parser::LangId::Rust, source)
            }
            _ => panic!("expected json or rust"),
        };
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::parser::grammar_for(language_id))
            .unwrap();
        let in_use = || {
            crate::memory::AllocatorObservation::measure()
                .snapshot
                .bytes_in_use
                .expect("native allocator census")
        };
        let before = in_use();
        let tree = parser.parse(&source, None).unwrap();
        assert!(!tree.root_node().has_error());
        let live = in_use();
        std::hint::black_box(&tree);
        drop(tree);
        let after_tree_drop = in_use();
        eprintln!("TREE_MEMORY language={language} source_bytes={} parse_live_bytes={} tree_owned_bytes={} parser_retained_bytes={} live_multiple={:.2}", source.len(), live.saturating_sub(before), live.saturating_sub(after_tree_drop), after_tree_drop.saturating_sub(before), live.saturating_sub(before) as f64 / source.len() as f64);
    }

    #[test]
    fn source_size_limit_is_pinned_and_enforced_before_open() {
        assert_eq!(MAX_SOURCE_BYTES, 1_048_576);
        assert_eq!(TOO_LARGE, "over the 1 MiB size limit");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        let probe = crate::project_content::Probe::new(dir.path());
        assert!(matches!(
            read_source_text(&path).unwrap(),
            SourceText::TooLarge
        ));
        assert!(probe.paths().is_empty());
        file.set_len(MAX_SOURCE_BYTES).unwrap();
        assert!(
            matches!(read_source_bytes(&path).unwrap(), SourceBytes::Bytes(bytes) if bytes.len() as u64 == MAX_SOURCE_BYTES)
        );
        assert_eq!(probe.paths(), vec![path]);
    }

    #[test]
    fn graph_sources_have_a_separate_four_mib_limit_and_byte_reservations() {
        assert_eq!(MAX_SOURCE_BYTES, 1_048_576);
        assert_eq!(MAX_GRAPH_SOURCE_BYTES, 4_194_304);
        assert_eq!(GRAPH_TOO_LARGE, "over the 4 MiB size limit");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handwritten.ts");
        std::fs::write(
            &path,
            handwritten_graph_source_for_test(1_572_864, "// handwritten source\n"),
        )
        .unwrap();
        assert!(matches!(
            read_source_text(&path).unwrap(),
            SourceText::TooLarge
        ));
        assert!(
            matches!(read_graph_source_text(&path).unwrap(), SourceText::Text(source) if source.len() == 1_572_864)
        );
        let budget = SourceScanBudget::new(SOURCE_SCAN_BUDGET_BYTES);
        let permit = budget.reserve_with_limit(3 * 1024 * 1024, MAX_GRAPH_SOURCE_BYTES);
        assert_eq!(permit.bytes, 3 * 1024 * 1024);
    }

    #[test]
    fn graph_classifier_reads_only_a_header_for_content_exclusions() {
        use crate::inspect::{InspectCategory, InspectJob, JobKey};
        use std::sync::{Arc, RwLock};
        let dir = tempfile::tempdir().unwrap();
        for (name, source) in [
            (
                "header.ts",
                format!("// @generated\n{}", "// generated source\n".repeat(10_000)),
            ),
            ("dense.js", "export const minified = 1;".repeat(10_000)),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, source).unwrap();
            let job = InspectJob {
                job_id: 1,
                key: JobKey::for_project_category(InspectCategory::UnusedExports),
                category: InspectCategory::UnusedExports,
                scope_files: vec![path.clone()],
                project_root: dir.path().to_path_buf(),
                inspect_dir: dir.path().join("inspect"),
                config: Arc::new(crate::config::Config::default()),
                symbol_cache: Arc::new(RwLock::new(crate::parser::SymbolCache::new())),
                inspect_writer: true,
                callgraph_writer: true,
                callgraph_snapshot: None,
            };
            let success = super::super::unused_exports::run_unused_exports_scan(&job)
                .outcome
                .unwrap();
            assert_eq!(success.aggregate["count"], 0);
            assert!(success.aggregate.get("skipped_files").is_none());
            assert_eq!(source_read_count_for_debug(&path), 0);
            assert!(
                crate::file_kind::read_probe::bytes_read(&path) <= crate::file_kind::HEAD_BYTES
            );
        }
    }

    #[test]
    fn oversized_sources_are_unopened_named_gaps_in_other_scanners() {
        use crate::inspect::{InspectCategory, InspectJob, JobKey, JobScope};
        use std::sync::{Arc, RwLock};
        let dir = tempfile::tempdir().unwrap();
        let large = dir.path().join("large.rs");
        std::fs::File::create(&large)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        let small = dir.path().join("small.rs");
        std::fs::write(&small, "fn small() {}\n").unwrap();
        let probe = crate::project_content::Probe::new(dir.path());
        for category in [
            InspectCategory::Metrics,
            InspectCategory::Complexity,
            InspectCategory::Duplicates,
        ] {
            let scope = JobScope::for_project(dir.path());
            let job = InspectJob {
                job_id: 1,
                key: JobKey::for_category_scope(category, &scope),
                category,
                scope_files: vec![large.clone(), small.clone()],
                project_root: dir.path().to_path_buf(),
                inspect_dir: dir.path().join("inspect"),
                config: Arc::new(crate::config::Config::default()),
                symbol_cache: Arc::new(RwLock::new(crate::parser::SymbolCache::new())),
                inspect_writer: true,
                callgraph_writer: true,
                callgraph_snapshot: None,
            };
            let result = match category {
                InspectCategory::Metrics => super::super::metrics::run_metrics_scan(&job),
                InspectCategory::Complexity => super::super::complexity::run_complexity_scan(&job),
                InspectCategory::Duplicates => super::super::duplicates::run_duplicates_scan(&job),
                _ => unreachable!(),
            }
            .outcome
            .unwrap();
            assert_eq!(result.scanned_files, vec![small.clone()], "{category:?}");
            assert_eq!(result.aggregate["complete"], false, "{category:?}");
            assert_eq!(
                result.aggregate["skipped_files"],
                json!([
                    {"file": "large.rs", "reason": "over the 1 MiB size limit"}
                ]),
                "{category:?}"
            );
            if category == InspectCategory::Metrics {
                assert_eq!(result.aggregate["files"], 1);
            } else {
                assert_eq!(result.aggregate["scanned_files"], 1);
            }
        }
        assert!(!probe.paths().contains(&large));
    }

    #[test]
    fn invalid_utf8_is_a_skip_and_a_missing_file_is_vanished() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gbk = dir.path().join("gbk.json");
        std::fs::write(&gbk, b"{\"k\":\"\xb9\xe6\xce\"}").expect("write gbk");
        let utf8 = dir.path().join("ok.ts");
        std::fs::write(&utf8, "export const k = 1;\n").expect("write utf8");

        assert!(matches!(
            read_source_text(&gbk).expect("read gbk"),
            SourceText::NotUtf8
        ));
        assert!(matches!(
            read_source_text(&utf8).expect("read utf8"),
            SourceText::Text(text) if text == "export const k = 1;\n"
        ));
        assert!(matches!(
            read_source_text(&dir.path().join("gone.ts")).expect("read missing"),
            SourceText::Vanished
        ));
    }

    #[test]
    fn skipped_files_are_sorted_capped_and_counted() {
        let skipped = (0..SKIPPED_FILES_LIMIT + 5)
            .rev()
            .map(|index| (format!("f{index:03}.ts"), NOT_VALID_UTF8.to_string()))
            .collect::<Vec<_>>();
        let mut capped = json!({ "count": 0 });
        write_skipped_files(&mut capped, skipped.clone(), Some(100));
        assert_eq!(capped["complete"], json!(false));
        assert_eq!(
            capped["skipped_files_count"],
            json!(SKIPPED_FILES_LIMIT + 5)
        );
        let rows = capped["skipped_files"].as_array().expect("rows");
        assert_eq!(rows.len(), SKIPPED_FILES_LIMIT);
        assert_eq!(
            rows[0],
            json!({ "file": "f000.ts", "reason": NOT_VALID_UTF8 })
        );

        let mut uncapped = json!({ "count": 0 });
        write_skipped_files(&mut uncapped, skipped, None);
        assert_eq!(
            uncapped["skipped_files"].as_array().map(Vec::len),
            Some(SKIPPED_FILES_LIMIT + 5)
        );

        let mut untouched = json!({ "count": 0 });
        write_skipped_files(&mut untouched, Vec::new(), Some(100));
        assert_eq!(untouched, json!({ "count": 0 }));
    }
}
