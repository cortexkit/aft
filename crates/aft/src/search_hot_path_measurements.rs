//! Opt-in search work counts and per-pass timings.
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static FILE_READS: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn record_file_read() {
    FILE_READS.fetch_add(1, Ordering::Relaxed);
    record(|counts| counts.file_reads += 1);
}

pub(crate) fn record_file_read_path(path: &Path) {
    record_file_read();
    let first = READ_PATHS.with(|paths| paths.borrow_mut().insert(path.to_path_buf()));
    record(|counts| {
        if first {
            counts.unique_read_paths += 1;
        } else {
            counts.repeated_path_reads += 1;
        }
    });
}

pub(crate) fn record_memo_file_read(path: &Path) {
    record_file_read_path(path);
    let first = MEMO_PATHS.with(|paths| paths.borrow_mut().insert(path.to_path_buf()));
    record(|counts| {
        counts.memo_digest_reads += 1;
        if first {
            counts.memo_unique_paths += 1;
        } else {
            counts.memo_repeated_path_reads += 1;
        }
    });
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct Counts {
    pub file_reads: usize,
    pub regex_compilations: usize,
    pub score_evaluations: usize,
    pub candidates_sorted: usize,
    pub token_scans: usize,
    pub token_lines_scanned: usize,
    pub anchor_windows_evaluated: usize,
    pub reused_payload_clones: usize,
    pub refresh_entry_visits: usize,
    pub project_walks: usize,
    pub project_walk_entries: usize,
    pub project_walk_files: usize,
    pub exact_passes: usize,
    pub exact_preparations: usize,
    pub exact_pass_candidates: usize,
    pub exact_pass_reads: usize,
    pub exact_pass_micros: usize,
    pub unique_read_paths: usize,
    pub repeated_path_reads: usize,
    pub memo_digest_reads: usize,
    pub memo_unique_paths: usize,
    pub memo_repeated_path_reads: usize,
}

thread_local! {
    static COUNTS: Cell<Counts> = Cell::new(Counts::default());
    static READ_PATHS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
    static MEMO_PATHS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// Start a thread-local measurement window, including repeated-path counts.
pub(crate) fn reset() {
    COUNTS.with(|cell| cell.set(Counts::default()));
    READ_PATHS.with(|paths| paths.borrow_mut().clear());
    MEMO_PATHS.with(|paths| paths.borrow_mut().clear());
}

pub(crate) fn counts() -> Counts {
    COUNTS.with(Cell::get)
}

pub(crate) fn record(update: impl FnOnce(&mut Counts)) {
    COUNTS.with(|cell| {
        let mut counts = cell.get();
        update(&mut counts);
        cell.set(counts);
    });
}

pub(crate) fn measure<T>(case: &str, operation: impl FnOnce() -> T) -> T {
    FILE_READS.store(0, Ordering::SeqCst);
    reset();
    #[cfg(debug_assertions)]
    crate::search_index::reset_postings_for_trigram_count_for_debug();
    let started = std::time::Instant::now();
    let (result, allocations, allocated_bytes) = crate::test_allocations::count_process(operation);
    record(|counts| counts.file_reads = FILE_READS.load(Ordering::SeqCst));
    #[cfg(debug_assertions)]
    let posting_reads = Some(crate::search_index::postings_for_trigram_count_for_debug());
    #[cfg(not(debug_assertions))]
    let posting_reads = None::<usize>;
    println!(
        "HOT_PATH {}",
        serde_json::json!({
            "case": case,
            "profile": "dev-test",
            "latency_ms": started.elapsed().as_secs_f64() * 1000.0,
            "allocations": allocations,
            "allocated_bytes": allocated_bytes,
            "posting_reads": posting_reads,
            "work": counts(),
        })
    );
    result
}

pub(crate) fn result_digest(case: &str, value: &impl serde::Serialize) {
    let mut value = serde_json::to_value(value).unwrap();
    fn relative_paths(value: &mut serde_json::Value, root: &str) {
        match value {
            serde_json::Value::String(text) => {
                if let Some(relative) = text
                    .strip_prefix(root)
                    .filter(|relative| relative.starts_with(std::path::MAIN_SEPARATOR))
                {
                    *text = format!("<corpus>{relative}");
                }
            }
            serde_json::Value::Array(values) => values
                .iter_mut()
                .for_each(|value| relative_paths(value, root)),
            serde_json::Value::Object(values) => values
                .values_mut()
                .for_each(|value| relative_paths(value, root)),
            _ => {}
        }
    }
    if let Ok(root) = std::env::var("AFT_PERF_CORPUS") {
        relative_paths(&mut value, &root);
    }
    let bytes = serde_json::to_vec(&value).unwrap();
    println!(
        "HOT_PATH {}",
        serde_json::json!({"case": case, "ranked_blake3": blake3::hash(&bytes).to_hex().to_string()})
    );
}

/// Replay captured benchmark requests against one shared lexical-only context.
/// Corpus setup and response serialization stay outside the measured interval.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "set AFT_PERF_CORPUS and AFT_ENGINE_PROFILE_OUT; Linux parallel real-query profile"]
fn engine_only_real_query_parallel_profile() {
    use crate::commands::semantic_search::handle_semantic_search;
    use crate::config::Config;
    use crate::context::{AppContext, SemanticIndexStatus};
    use crate::parser::TreeSitterProvider;
    use crate::protocol::RawRequest;
    use crate::search_index::SearchIndex;
    use std::sync::Mutex;
    use std::time::Instant;

    fn cpu_ms() -> f64 {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // The clock writes the timespec only on success.
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) },
            0
        );
        let time = unsafe { time.assume_init() };
        time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0
    }

    let root = PathBuf::from(std::env::var_os("AFT_PERF_CORPUS").expect("AFT_PERF_CORPUS"));
    let output =
        PathBuf::from(std::env::var_os("AFT_ENGINE_PROFILE_OUT").expect("AFT_ENGINE_PROFILE_OUT"));
    let manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../benchmarks/aft-search/real-query-manifest.json"
    ))
    .unwrap();
    let rows = manifest["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row.get("excluded_reason").is_none())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 108);
    let index = SearchIndex::build(&root);
    let file_count = index.file_count();
    assert!(
        file_count > 1000,
        "use the pinned AFT corpus, not a synthetic fixture"
    );
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(root.clone()),
            ..Config::default()
        },
    );
    *ctx.semantic_index_status().write().unwrap() = SemanticIndexStatus::Disabled;
    *ctx.search_index().write().unwrap() = Some(index);
    let samples = Mutex::new(Vec::new());
    for (label, workers, repeats) in [("serial", 1, 1), ("parallel", 4, 4)] {
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let ctx = &ctx;
                let rows = &rows;
                let samples = &samples;
                let root = &root;
                scope.spawn(move || {
                    for number in (worker..rows.len() * repeats).step_by(workers) {
                        let row = rows[number % rows.len()];
                        let id = format!("engine-profile-{number}");
                        let mut request = serde_json::json!({
                            "id": id, "command": "semantic_search", "query": row["query"],
                            "include_tests": row["include_tests"],
                            "top_k": if label == "serial" { 50 } else { row["recorded_top_k"].as_u64().unwrap_or(10) },
                        });
                        if let Some(pattern) = row.get("pattern") {
                            request["pattern"] = pattern.clone();
                        }
                        let request: RawRequest = serde_json::from_value(request).unwrap();
                        reset();
                        let cpu = cpu_ms();
                        let started = Instant::now();
                        let response = handle_semantic_search(&request, ctx);
                        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                        let cpu_ms = cpu_ms() - cpu;
                        let work = counts();
                        let value = serde_json::to_value(response).unwrap();
                        assert_eq!(value["success"], true, "{value}");
                        let bytes = serde_json::to_string(&value).unwrap()
                            .replace(root.to_str().unwrap(), "<corpus>");
                        samples.lock().unwrap().push(serde_json::json!({
                            "mode": label, "number": number, "episode_id": row["episode_id"],
                            "elapsed_ms": elapsed_ms, "cpu_ms": cpu_ms, "work": work,
                            "response_blake3": blake3::hash(bytes.as_bytes()).to_hex().to_string(),
                            "response": serde_json::from_str::<serde_json::Value>(&bytes).unwrap(),
                        }));
                    }
                });
            }
        });
    }
    let mut samples = samples.into_inner().unwrap();
    samples.sort_by_key(|sample| {
        (
            sample["mode"].as_str().unwrap().to_string(),
            sample["number"].as_u64().unwrap(),
        )
    });
    std::fs::write(
        output,
        serde_json::to_vec(&serde_json::json!({"files": file_count, "samples": samples})).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "set AFT_PERF_CORPUS to a real corpus; opt-in disk and allocation benchmark"]
fn real_corpus_lane_work_counts() {
    use crate::commands::semantic_search::{
        anchored_lane::AnchoredLane, lexical_lane::CanonicalLexicalLane,
    };
    use crate::search_index::{extract_trigrams, SearchIndex};
    let root =
        std::path::PathBuf::from(std::env::var_os("AFT_PERF_CORPUS").expect("AFT_PERF_CORPUS"));
    let mut index = SearchIndex::new();
    let mut files = 0;
    for entry in ignore::WalkBuilder::new(&root)
        .hidden(true)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() <= 1024 * 1024)
        {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                index.index_file(entry.path(), &bytes);
                files += 1;
            }
        }
    }
    assert!(files > 0, "corpus must contain indexed files");
    index.ready = true;
    let snapshot = index.snapshot();
    for (label, query) in [
        ("natural_language", "how does the search index discover candidate files and verify exact matches before ranking results"),
        ("code_literal", "SearchIndex::new"),
        ("common_rare", "search repeated_page_marker"),
    ] {
        let trigrams = extract_trigrams(query.as_bytes()).into_iter().map(|(trigram, _, _)| trigram).collect::<Vec<_>>();
        let lane = measure(&format!("lexical/{label}"), || CanonicalLexicalLane::from_snapshot(&snapshot, &trigrams, None, 50).unwrap());
        result_digest(&format!("lexical/{label}"), &lane.canonical_order());
        let exact = measure(&format!("exact/{label}"), || snapshot.whole_corpus_exact_pass(query, &root, None).unwrap());
        result_digest(&format!("exact/{label}"), &exact.iter().map(|hit| (&hit.path, hit.symbol_range, &hit.evidence, &hit.content_digest)).collect::<Vec<_>>());
    }
    let anchored = measure("anchored/search.*index", || {
        AnchoredLane::new()
            .execute_ready_mode(&index, &root, "search.*index", true)
            .unwrap()
    });
    result_digest("anchored/search.*index", &anchored);
}
