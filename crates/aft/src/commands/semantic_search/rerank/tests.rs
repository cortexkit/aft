use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::onnx::{
    attention_batches_for_lengths, joint_rerank_threads, OnnxReranker, PairScorer, ScorerLoader,
};
use super::*;
use crate::commands::semantic_search::handle_semantic_search;
use crate::config::{Config, RerankConfig};
use crate::context::{AppContext, SemanticIndexStatus};
use crate::parser::TreeSitterProvider;
use crate::protocol::RawRequest;
use crate::search_index::SearchIndex;

thread_local! {
    static TEST_BACKEND: RefCell<Option<SelectedBackend>> = const { RefCell::new(None) };
    static IO_ON_THIS_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts a file or network access made on the calling thread.
pub(super) fn count_io() {
    IO_ON_THIS_THREAD.with(|count| count.set(count.get() + 1));
}

fn io_on_this_thread() -> usize {
    IO_ON_THIS_THREAD.with(std::cell::Cell::get)
}

/// The backend a test installed for its own thread, if any.
pub(super) fn test_backend_override() -> Option<SelectedBackend> {
    TEST_BACKEND.with(|slot| slot.borrow().clone())
}

/// Run `body` with `backend` serving every rerank on this thread.
fn with_backend<R>(
    backend: Arc<dyn RerankBackend>,
    fail_closed: bool,
    body: impl FnOnce() -> R,
) -> R {
    TEST_BACKEND.with(|slot| {
        *slot.borrow_mut() = Some(SelectedBackend {
            backend,
            fail_closed,
        })
    });
    let result = body();
    TEST_BACKEND.with(|slot| *slot.borrow_mut() = None);
    result
}

/// Scores a candidate by the number in its file name (`src/file_07.rs` scores
/// 7), so a reranked head is the reverse of name order. Can be told to fail a
/// number of first calls with a given error.
struct NumberedBackend {
    calls: AtomicUsize,
    failures: Mutex<Vec<RerankError>>,
    queries: Mutex<Vec<String>>,
}

impl NumberedBackend {
    fn new(failures: Vec<RerankError>) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            failures: Mutex::new(failures),
            queries: Mutex::new(Vec::new()),
        })
    }
}

fn file_number(text: &str) -> f32 {
    // The first line is `path:line`.
    let first_line = text.lines().next().unwrap_or_default();
    let path = first_line
        .rsplit_once(':')
        .map_or(first_line, |(path, _)| path);
    path.rsplit('_')
        .next()
        .and_then(|tail| tail.trim_end_matches(".rs").parse::<f32>().ok())
        .unwrap_or(-1.0)
}

impl RerankBackend for NumberedBackend {
    fn fingerprint(&self) -> RerankFingerprint {
        RerankFingerprint {
            backend: "numbered",
            model: "by-file-number".to_string(),
            revision: "1".to_string(),
        }
    }

    fn max_batch(&self) -> usize {
        usize::MAX
    }

    fn score(
        &self,
        query: &str,
        docs: &[RerankDoc<'_>],
        _deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.queries.lock().unwrap().push(query.to_string());
        let mut failures = self.failures.lock().unwrap();
        if !failures.is_empty() {
            return Err(failures.remove(0));
        }
        Ok(docs.iter().map(|doc| file_number(doc.text)).collect())
    }
}

// ---- pure ordering rules -------------------------------------------------

#[test]
fn noop_backend_returns_one_score_per_doc() {
    let backend = NoopRerankBackend;
    let docs = [RerankDoc { text: "a" }, RerankDoc { text: "b" }];
    let scores = backend
        .score("q", &docs, Instant::now() + Duration::from_secs(1))
        .expect("noop backend never fails");
    assert_eq!(scores, vec![0.0, 0.0]);
    assert_eq!(backend.fingerprint().backend, "noop");
}

#[test]
fn permutation_orders_by_score_within_segments_and_keeps_ties_stable() {
    // Two exact entries then three non-exact ones. The best non-exact score
    // must not climb above either exact entry.
    let segments = [1, 1, 0, 0, 0];
    let scores = [0.1, 0.9, 5.0, 2.0, 5.0];
    assert_eq!(permutation(&scores, &segments), vec![1, 0, 2, 4, 3]);
}

#[test]
fn permutation_uses_total_order_for_signed_zero() {
    // total_cmp puts +0.0 above -0.0, then the prior order breaks true ties.
    let segments = [0, 0, 0];
    assert_eq!(permutation(&[-0.0, 0.0, 0.0], &segments), vec![1, 2, 0]);
}

#[test]
fn settings_default_to_off_and_clamp_depth_and_timeout() {
    let settings = RerankSettings::resolve(&SearchConfig::default());
    assert_eq!(settings.backend, RerankBackendKind::Off);
    assert_eq!(settings.top_n, DEFAULT_TOP_N);
    assert_eq!(settings.timeout, Duration::from_millis(DEFAULT_TIMEOUT_MS));

    let settings = RerankSettings::resolve(&SearchConfig {
        rerank: Some(RerankConfig {
            backend: Some(RerankBackendKind::Onnx),
            top_n: Some(10_000),
            timeout_ms: Some(1),
            ..RerankConfig::default()
        }),
    });
    assert_eq!(settings.top_n, MAX_TOP_N);
    assert_eq!(settings.timeout, Duration::from_millis(MIN_TIMEOUT_MS));
    let settings = RerankSettings::resolve(&SearchConfig {
        rerank: Some(RerankConfig {
            timeout_ms: Some(u64::MAX),
            top_n: Some(0),
            ..RerankConfig::default()
        }),
    });
    assert_eq!(settings.top_n, 1);
    assert_eq!(settings.timeout, Duration::from_millis(MAX_TIMEOUT_MS));
}

#[test]
fn joint_thread_budget_never_starves_or_oversubscribes() {
    // 16 cores: embedder 8, reranker 4.
    assert_eq!(joint_rerank_threads(8, 16, None), 4);
    // 4 cores: embedder 2, reranker 1.
    assert_eq!(joint_rerank_threads(2, 4, None), 1);
    // A 3-CPU quota on a 64-core host: embedder 2, reranker gets the 1 left.
    assert_eq!(joint_rerank_threads(2, 64, Some(3)), 1);
    // One core: the floor of one thread still applies.
    assert_eq!(joint_rerank_threads(1, 1, None), 1);
    // 32 cores: embedder capped at 8, reranker 4.
    assert_eq!(joint_rerank_threads(8, 32, None), 4);
}

#[test]
fn attention_batches_respect_the_budget_and_keep_order() {
    // 100² = 10k units per row; a 25k budget holds two rows.
    let batches = attention_batches_for_lengths(&[100, 100, 100, 100, 100], 25_000);
    assert_eq!(batches, vec![0..2, 2..4, 4..5]);
    // A single over-budget row still gets its own batch.
    let batches = attention_batches_for_lengths(&[10, 1_000, 10], 25_000);
    assert_eq!(batches, vec![0..1, 1..2, 2..3]);
}

// ---- candidate text ------------------------------------------------------

#[test]
fn candidate_text_is_bounded_relative_and_repeatable() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("src/long.rs");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let mut source = String::from("fn header() {}\r\n\r\n");
    for line in 0..400 {
        source.push_str(&format!(
            "    let value_{line} = compute_the_answer({line});\r\n"
        ));
    }
    std::fs::write(&file, &source).unwrap();

    let whole = CandidateResult::new_exact(
        file.clone(),
        None,
        super::super::evidence_descriptor::EvidenceDescriptor::for_e1(1, true, false),
    );
    let first = candidate_text(project.path(), &whole, "compute_the_answer");
    let second = candidate_text(project.path(), &whole, "compute_the_answer");
    assert_eq!(first, second);
    assert!(
        first.len() <= CANDIDATE_TEXT_BUDGET_BYTES,
        "{}",
        first.len()
    );
    assert!(
        first.starts_with("src/long.rs:3\ncompute_the_answer\n"),
        "{first}"
    );
    assert!(!first.contains('\r'));
    assert!(!first.contains("\n\n"), "blank lines are skipped");
    // Without a code identifier in the prose, the name line is the file stem.
    let prose_only = candidate_text(project.path(), &whole, "how is the value computed");
    assert_eq!(prose_only.lines().nth(1), Some("long"), "{prose_only}");

    let symbol = CandidateResult::new_exact(
        file.clone(),
        Some(super::super::comparator::SymbolOffsetRange::new(0, 14)),
        super::super::evidence_descriptor::EvidenceDescriptor::for_e1(1, true, false),
    );
    assert_eq!(
        candidate_text(project.path(), &symbol, "anything"),
        "src/long.rs:1\nheader\nfn header() {}"
    );

    // A symbol deep in the file names its own line and declared name, and the
    // first two lines survive a body that fills the budget.
    let decorated = project.path().join("src/decorated.rs");
    let mut source =
        String::from("// leading comment\n\n#[inline]\npub(crate) fn decorated_entry_point() {\n");
    for line in 0..200 {
        source.push_str(&format!(
            "    let value_{line} = compute_the_answer({line});\n"
        ));
    }
    source.push_str("}\n");
    std::fs::write(&decorated, &source).unwrap();
    let start = source.find("#[inline]").unwrap();
    let symbol = CandidateResult::new_exact(
        decorated,
        Some(super::super::comparator::SymbolOffsetRange::new(
            start,
            source.len(),
        )),
        super::super::evidence_descriptor::EvidenceDescriptor::for_e1(1, true, false),
    );
    let text = candidate_text(project.path(), &symbol, "anything");
    assert!(
        text.starts_with("src/decorated.rs:3\ndecorated_entry_point\n#[inline]\n"),
        "{text}"
    );
    assert!(text.len() <= CANDIDATE_TEXT_BUDGET_BYTES, "{}", text.len());
}

// ---- served path ---------------------------------------------------------

#[test]
fn audit01_raw_range_survives_crlf_and_invalid_prefix() {
    let project = tempfile::tempdir().unwrap();
    let file = project.path().join("raw.rs");
    let mut source = b"// bad: \xff\r\n// second\r\n// third\r\n".to_vec();
    let start = source.len();
    source.extend_from_slice(b"pub fn selected() {\r\n    target();\r\n}\r\n");
    std::fs::write(&file, &source).unwrap();
    let symbol = CandidateResult::new_exact(
        file,
        Some(super::super::comparator::SymbolOffsetRange::new(
            start,
            source.len(),
        )),
        super::super::evidence_descriptor::EvidenceDescriptor::for_e1(1, true, false),
    );
    assert_eq!(
        candidate_text(project.path(), &symbol, "anything"),
        "raw.rs:4\nselected\npub fn selected() {\n    target();\n}"
    );
}

fn page_request(query: &str, top_k: usize, offset: usize) -> RawRequest {
    serde_json::from_value(serde_json::json!({
        "id": "rerank-page-test",
        "command": "semantic_search",
        "query": query,
        "top_k": top_k,
        "offset": offset,
        "hint": "literal",
    }))
    .unwrap()
}

const PROBE: &str = "rerank_probe_token";
/// A prose question about the probe files. The reranker only reorders queries
/// the router classifies as prose, so the served-path tests ask this.
const PROBE_QUESTION: &str = "where is the rerank probe token handled";

/// A lexical-only project with `count` files that all contain the probe.
fn probe_project(count: usize) -> (tempfile::TempDir, AppContext) {
    let project = tempfile::tempdir().unwrap();
    let mut index = SearchIndex::new();
    for number in 0..count {
        let file = project.path().join(format!("src/file_{number:02}.rs"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let source = format!("pub fn {PROBE}_{number:02}() {{ /* rerank probe */ }}\n");
        std::fs::write(&file, &source).unwrap();
        index.index_file(&file, source.as_bytes());
    }
    index.ready = true;
    let ctx = AppContext::new(
        Box::new(TreeSitterProvider::new()),
        Config {
            project_root: Some(project.path().to_path_buf()),
            ..Config::default()
        },
    );
    *ctx.search_index()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
    *ctx.semantic_index_status()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = SemanticIndexStatus::Disabled;
    (project, ctx)
}

/// (file names of one page, the serialized response).
fn page(ctx: &AppContext, top_k: usize, offset: usize) -> (Vec<String>, String) {
    page_for(ctx, PROBE_QUESTION, top_k, offset)
}

fn page_for(ctx: &AppContext, query: &str, top_k: usize, offset: usize) -> (Vec<String>, String) {
    let response = handle_semantic_search(&page_request(query, top_k, offset), ctx);
    assert!(response.success, "search failed: {response:?}");
    let value = serde_json::to_value(&response).unwrap();
    let files = value["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|result| {
                    Path::new(result["file"].as_str().unwrap())
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    (files, value.to_string())
}

fn stream(ctx: &AppContext, top_k: usize, total: usize) -> Vec<String> {
    let mut all = Vec::new();
    let mut offset = 0;
    while offset < total {
        let (files, _) = page(ctx, top_k, offset);
        if files.is_empty() {
            break;
        }
        all.extend(files);
        offset += top_k;
    }
    all
}

#[test]
fn served_stream_is_identical_across_page_sizes_and_offsets_around_the_head() {
    let (_project, ctx) = probe_project(40);
    let fused = stream(&ctx, 100, 40);
    assert_eq!(fused.len(), 40, "{fused:?}");

    let backend = NumberedBackend::new(Vec::new());
    with_backend(backend.clone(), false, || {
        let reference = stream(&ctx, 100, 40);
        assert_eq!(reference.len(), 40);

        // The head is the fused head, reordered by descending file number;
        // everything below the head keeps its fused position.
        let mut expected_head = fused[..DEFAULT_TOP_N].to_vec();
        expected_head.sort_by(|left, right| {
            file_number(&format!("src/{right}")).total_cmp(&file_number(&format!("src/{left}")))
        });
        assert_eq!(reference[..DEFAULT_TOP_N], expected_head[..]);
        assert_ne!(reference[..DEFAULT_TOP_N], fused[..DEFAULT_TOP_N]);
        assert_eq!(reference[DEFAULT_TOP_N..], fused[DEFAULT_TOP_N..]);

        for top_k in [10, 25, 50] {
            assert_eq!(stream(&ctx, top_k, 40), reference, "topK={top_k}");
        }
        for (top_k, offset) in [
            (5, 15),
            (5, 18),
            (3, 19),
            (2, 20),
            (5, 20),
            (5, 21),
            (1, 19),
        ] {
            let (files, text) = page(&ctx, top_k, offset);
            assert_eq!(
                files,
                reference[offset..offset + top_k],
                "topK={top_k} offset={offset}"
            );
            assert!(!text.contains("rerank skipped"));
        }
    });
    // Every page reused the order computed by the first request.
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.queries.lock().unwrap()[0], PROBE_QUESTION);
}

#[test]
fn a_committed_skip_is_replayed_even_after_the_backend_recovers() {
    let (_project, ctx) = probe_project(30);
    let fused = stream(&ctx, 100, 30);

    let backend = NumberedBackend::new(vec![RerankError::Unavailable("model loading".into())]);
    with_backend(backend.clone(), false, || {
        let (first, text) = page(&ctx, 10, 0);
        assert!(text.contains("rerank skipped: model loading"), "{text}");
        assert_eq!(first, fused[..10]);
        // The backend would now succeed; the page must still continue the
        // fused stream, with the same note.
        let (second, text) = page(&ctx, 10, 10);
        assert!(text.contains("rerank skipped: model loading"), "{text}");
        assert_eq!(second, fused[10..20]);
        assert_eq!(stream(&ctx, 25, 30), fused);
    });
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_committed_order_survives_a_later_backend_failure() {
    let (_project, ctx) = probe_project(30);
    // The first call succeeds and commits an order; later calls would time
    // out (the failures are queued below), but none should be made.
    let backend = NumberedBackend::new(Vec::new());
    let reference = with_backend(backend.clone(), false, || stream(&ctx, 100, 30));
    *backend.failures.lock().unwrap() = vec![RerankError::Timeout; 8];
    with_backend(backend.clone(), false, || {
        for top_k in [7, 10, 30] {
            assert_eq!(stream(&ctx, top_k, 30), reference);
        }
    });
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn default_config_never_reranks_or_notes() {
    let (_project, ctx) = probe_project(25);
    let (_, text) = page(&ctx, 25, 0);
    assert!(!text.contains("rerank skipped"), "{text}");
}

// ---- the head of one canonical list --------------------------------------

fn tiny_list(project: &Path, count: usize) -> CanonicalList {
    use super::super::blocks::{BlockEntry, CanonicalListKey, FrozenBlock};
    use super::super::evidence_descriptor::EvidenceDescriptor;
    let entries = (0..count)
        .map(|index| {
            let path = project.join(format!("src/file_{index:02}.rs"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("fn item_{index}() {{}}\n")).unwrap();
            BlockEntry {
                result: CandidateResult::new_non_exact(
                    path,
                    None,
                    EvidenceDescriptor::for_non_exact(false, false),
                    1.0,
                    1.0,
                    super::super::plan_table::SearchLaneKind::Lexical,
                ),
                tier_index: 0,
                admitted_contributions: Vec::new(),
                lane_attribution: Vec::new(),
                r3_order_index: index,
            }
        })
        .collect();
    CanonicalList {
        key: CanonicalListKey {
            project_root: project.to_path_buf(),
            snapshot_generation: "g1".to_string(),
            normalized_query: "q".to_string(),
            include_tests: false,
            split: None,
        },
        blocks: vec![FrozenBlock {
            tier_index: 0,
            entries,
        }],
    }
}

fn head_request<'a>(
    search: &'a SearchConfig,
    project: &'a Path,
    prose: Option<&'a str>,
    scope: Option<&'a HashSet<PathBuf>>,
) -> RerankRequest<'a> {
    RerankRequest {
        search,
        backend: slot::Installed::Off,
        project_root: project,
        prose,
        public_query: prose.unwrap_or_default(),
        path_scope: scope,
    }
}

fn names(list: &CanonicalList) -> Vec<String> {
    list.entries()
        .map(|entry| {
            entry
                .result
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn pattern_only_requests_are_never_reranked() {
    let project = tempfile::tempdir().unwrap();
    let mut list = tiny_list(project.path(), 5);
    let before = names(&list);
    let backend = NumberedBackend::new(Vec::new());
    let selected = Ok(SelectedBackend {
        backend: backend.clone(),
        fail_closed: false,
    });
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);
    for prose in [None, Some(""), Some("   ")] {
        let outcome = rerank_block_zero(
            &mut list,
            &selected,
            &settings,
            &head_request(&search, project.path(), prose, None),
        )
        .unwrap();
        assert_eq!(outcome, HeadOutcome::NotApplicable);
    }
    assert_eq!(names(&list), before);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

/// Scores benchmark pools with the production local ONNX scorer, outside any
/// search deadline. `AFT_RERANK_SCORE_POOLS` names a JSON list of
/// `{"query", "texts"}` objects and `AFT_RERANK_SCORE_OUTPUT` the file the
/// results are written to: the scores (one list per pool, in input order),
/// the session build time, wall and CPU time per pool, and the process's peak
/// resident memory. `AFT_RERANK_ONNX_TEST_CACHE_DIR` must hold
/// gte-reranker-modernbert-base at the commit pinned in `onnx::ALLOWED_MODELS`
/// (it is downloaded there when missing). Runs only when all three are set.
///
/// For the hardware experiment, `AFT_RERANK_SCORE_COREML` (`ALL`,
/// `CPU_AND_GPU`, `CPU_AND_NE` or `CPU_ONLY`) runs the graph through Core ML,
/// `AFT_RERANK_SCORE_BUCKETS=1` pads every inference to a bucketed length
/// and batch,
/// `AFT_RERANK_SCORE_FIXED_SHAPE=BATCHxLENGTH` declares one static input
/// shape and pads every inference to it,
/// `AFT_RERANK_SCORE_PROFILE` names an ONNX Runtime profile file prefix (it
/// slows every call, so time runs without it), and `AFT_RERANK_SCORE_LIMIT`
/// scores only the first N pools.
#[cfg(unix)]
#[test]
fn score_pools_with_local_onnx_when_asked() {
    use ort::ep::coreml::ComputeUnits;
    let (Some(input), Some(output), Some(cache_dir)) = (
        std::env::var_os("AFT_RERANK_SCORE_POOLS"),
        std::env::var_os("AFT_RERANK_SCORE_OUTPUT"),
        std::env::var_os("AFT_RERANK_ONNX_TEST_CACHE_DIR").map(PathBuf::from),
    ) else {
        return;
    };
    #[derive(serde::Deserialize)]
    struct Pool {
        query: String,
        texts: Vec<String>,
    }
    let coreml = std::env::var("AFT_RERANK_SCORE_COREML")
        .ok()
        .map(|units| match units.as_str() {
            "ALL" => ComputeUnits::All,
            "CPU_AND_GPU" => ComputeUnits::CPUAndGPU,
            "CPU_AND_NE" => ComputeUnits::CPUAndNeuralEngine,
            "CPU_ONLY" => ComputeUnits::CPUOnly,
            other => panic!("unknown Core ML compute units {other}"),
        });
    let profile_prefix = std::env::var("AFT_RERANK_SCORE_PROFILE")
        .ok()
        .map(|prefix| &*Box::leak(prefix.into_boxed_str()));
    let tuning = onnx::SessionTuning {
        coreml,
        length_buckets: std::env::var("AFT_RERANK_SCORE_BUCKETS").is_ok_and(|value| value == "1"),
        profile_prefix,
        fixed_shape: std::env::var("AFT_RERANK_SCORE_FIXED_SHAPE")
            .ok()
            .map(|value| {
                let (batch, length) = value.split_once('x').expect("BATCHxLENGTH");
                (batch.parse().unwrap(), length.parse().unwrap())
            }),
        ..onnx::SessionTuning::default()
    };
    let pools: Vec<Pool> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let limit = std::env::var("AFT_RERANK_SCORE_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(pools.len());
    let pools = &pools[..limit.min(pools.len())];
    let model = onnx::find_allowed_model("gte-reranker-modernbert-base").unwrap();
    let files = onnx::provision_model_files_in(model, &cache_dir).expect("provision pinned model");
    let started = Instant::now();
    let mut scorer =
        onnx::OnnxPairScorer::load_with(model.name, &files, onnx::rerank_intra_threads(), tuning)
            .expect("load model");
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut scores = Vec::new();
    let mut timings = Vec::new();
    for pool in pools {
        let cpu = process_cpu_seconds();
        let started = Instant::now();
        scores.push(
            scorer
                .score_pairs(
                    &pool.query,
                    &pool.texts,
                    Instant::now() + Duration::from_secs(600),
                )
                .unwrap(),
        );
        timings.push(serde_json::json!({
            "wall_ms": started.elapsed().as_secs_f64() * 1000.0,
            "cpu_s": process_cpu_seconds() - cpu,
        }));
    }
    let profile = profile_prefix.map(|_| scorer.end_profiling().unwrap());
    // SAFETY: getrusage only writes the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    std::fs::write(
        output,
        serde_json::to_vec(&serde_json::json!({
            "tuning": format!("{tuning:?}"),
            "build_ms": build_ms,
            "timings": timings,
            // ru_maxrss is in bytes on macOS and kilobytes on Linux.
            "max_rss_raw": usage.ru_maxrss,
            "profile": profile,
            "scores": scores,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Prints the execution providers the loaded ONNX Runtime library reports.
/// Runs only when `AFT_RERANK_PRINT_PROVIDERS` is set (it loads the runtime
/// named by `ORT_DYLIB_PATH`).
#[test]
fn print_onnx_runtime_providers_when_asked() {
    if std::env::var_os("AFT_RERANK_PRINT_PROVIDERS").is_none() {
        return;
    }
    crate::semantic_index::pre_validate_onnx_runtime().expect("onnx runtime");
    let api = ort::api();
    let mut providers: *mut *mut std::ffi::c_char = std::ptr::null_mut();
    let mut count: std::ffi::c_int = 0;
    // SAFETY: the runtime fills a list it owns and frees it in the release
    // call below; each entry is a NUL-terminated name.
    unsafe {
        let status = (api.GetAvailableProviders)(&mut providers, &mut count);
        assert!(status.0.is_null(), "GetAvailableProviders failed");
        for index in 0..count as isize {
            let name = std::ffi::CStr::from_ptr(*providers.offset(index));
            println!("provider\t{}", name.to_string_lossy());
        }
        let _ = (api.ReleaseAvailableProviders)(providers, count);
    }
}

/// Prints the reranker gate's decision for each query in the JSON list named
/// by `AFT_RERANK_GATE_QUERIES`, one line per query: `prose` or `skip`, the
/// query, the router's shape and the query-shape kind, so a benchmark can
/// label its rows with the live classification. Runs only when the variable
/// is set.
#[test]
fn print_gate_decisions_when_asked() {
    let Some(path) = std::env::var_os("AFT_RERANK_GATE_QUERIES") else {
        return;
    };
    let queries: Vec<String> =
        serde_json::from_slice(&std::fs::read(path).unwrap()).expect("a JSON list of strings");
    for query in queries {
        let decision = if is_prose_query(&query) {
            "prose"
        } else {
            "skip"
        };
        let (shape, _) = crate::search_b2::install_defaults()
            .classify(&super::super::extensions::RawQuery::new(&query));
        let kind = crate::query_shape::classify(&query).kind;
        println!(
            "gate\t{decision}\t{}\t{shape:?}\t{kind:?}",
            serde_json::to_string(&query).unwrap()
        );
    }
}

#[test]
fn only_queries_the_router_reads_as_prose_are_reranked() {
    for prose in [
        // natural language
        "where is the session cache invalidated",
        // mixed: prose words around a code identifier
        "how does LSPManager handle a timeout",
        // short mixed and short natural language
        "LSPManager initialization timeout",
        "permanent key dict",
        "process group already terminated",
    ] {
        assert!(is_prose_query(prose), "{prose} should be reranked");
    }
    for not_prose in [
        "useState",
        "handle_grep",
        "LSPManager",
        "crates/aft/src/commands/grep.rs",
        "ERR_PNPM_",
        r"fn\s+parse_\w+",
        "remove_file|Error",
        "\"exact phrase here\"",
    ] {
        assert!(
            !is_prose_query(not_prose),
            "{not_prose} should not be reranked"
        );
    }
    // Prose the router reads as a code literal or a log excerpt still has prose.
    for prose in [
        "subc module drain on shutdown: what does it wait for (lsp shutdown, watcher stop)",
        "profile sample file removed on symbolization failure download error decoding",
    ] {
        assert!(is_prose_query(prose), "{prose} should be reranked");
    }
}

#[test]
fn a_query_that_is_not_prose_never_reaches_the_backend() {
    let project = tempfile::tempdir().unwrap();
    let mut list = tiny_list(project.path(), 5);
    let before = names(&list);
    let backend = NumberedBackend::new(Vec::new());
    let selected = Ok(SelectedBackend {
        backend: backend.clone(),
        fail_closed: false,
    });
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);
    for query in ["item_3", "src/file_03.rs", r"item_\d+"] {
        let outcome = rerank_block_zero(
            &mut list,
            &selected,
            &settings,
            &head_request(&search, project.path(), Some(query), None),
        )
        .unwrap();
        assert_eq!(outcome, HeadOutcome::NotApplicable, "{query}");
    }
    assert_eq!(names(&list), before);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn an_identifier_search_keeps_the_fused_order_and_carries_no_note() {
    let (_project, ctx) = probe_project(30);
    let fused = (0..3)
        .flat_map(|page_index| page_for(&ctx, PROBE, 10, page_index * 10).0)
        .collect::<Vec<_>>();
    assert!(!fused.is_empty());
    // A backend that would fail every call: if the search reached it, the
    // response would carry a skip note.
    let backend = NumberedBackend::new(vec![RerankError::Timeout; 8]);
    with_backend(backend.clone(), false, || {
        let mut served = Vec::new();
        for page_index in 0..3 {
            let (files, text) = page_for(&ctx, PROBE, 10, page_index * 10);
            assert!(!text.contains("rerank skipped"), "{text}");
            served.extend(files);
        }
        assert_eq!(served, fused);
    });
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_quoted_literal_is_not_reranked_after_its_route_strips_the_quotes() {
    // The code-literal route removes the quotes and ranks the phrase inside,
    // which reads as prose; the gate must still see the quoted query.
    let (_project, ctx) = probe_project(30);
    let quoted = "\"rerank probe token handled\"";
    let (fused, _) = page_for(&ctx, quoted, 30, 0);
    assert!(!fused.is_empty());
    let backend = NumberedBackend::new(vec![RerankError::Timeout; 8]);
    with_backend(backend.clone(), false, || {
        let (served, text) = page_for(&ctx, quoted, 30, 0);
        assert!(!text.contains("rerank skipped"), "{text}");
        assert_eq!(served, fused);
    });
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn exact_entries_keep_their_order_and_positions_and_only_non_exact_entries_move() {
    use super::super::evidence_descriptor::EvidenceDescriptor;
    let project = tempfile::tempdir().unwrap();
    let mut list = tiny_list(project.path(), 6);
    for index in [0, 2] {
        let entry = &mut list.blocks[0].entries[index];
        entry.result = CandidateResult::new_exact(
            entry.result.path.clone(),
            None,
            EvidenceDescriptor::for_e1(1, true, false),
        );
    }
    let backend = NumberedBackend::new(Vec::new());
    let selected = Ok(SelectedBackend {
        backend: backend.clone(),
        fail_closed: false,
    });
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);
    let outcome = rerank_block_zero(
        &mut list,
        &selected,
        &settings,
        &head_request(
            &search,
            project.path(),
            Some("where are the items made"),
            None,
        ),
    )
    .unwrap();
    assert_eq!(outcome, HeadOutcome::Reordered);
    // The backend prefers high file numbers: the four non-exact entries are
    // reversed among the positions they held; the exact ones never move.
    assert_eq!(
        names(&list),
        [
            "file_00.rs",
            "file_05.rs",
            "file_02.rs",
            "file_04.rs",
            "file_03.rs",
            "file_01.rs"
        ]
    );
    let indices = list
        .entries()
        .map(|entry| entry.r3_order_index)
        .collect::<Vec<_>>();
    assert_eq!(indices, (0..6).collect::<Vec<_>>());
    // One backend call scored the four non-exact entries.
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn path_scope_preference_is_kept_by_the_reorder() {
    let project = tempfile::tempdir().unwrap();
    let mut list = tiny_list(project.path(), 6);
    // Files 00..02 are in scope and lead; the backend prefers high numbers,
    // which are out of scope, so they may only reorder among themselves.
    let scope = (0..3)
        .map(|index| project.path().join(format!("src/file_{index:02}.rs")))
        .collect::<HashSet<_>>();
    let selected = Ok(SelectedBackend {
        backend: NumberedBackend::new(Vec::new()),
        fail_closed: false,
    });
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);
    rerank_block_zero(
        &mut list,
        &selected,
        &settings,
        &head_request(
            &search,
            project.path(),
            Some("which files are in scope here"),
            Some(&scope),
        ),
    )
    .unwrap();
    assert_eq!(
        names(&list),
        [
            "file_02.rs",
            "file_01.rs",
            "file_00.rs",
            "file_05.rs",
            "file_04.rs",
            "file_03.rs"
        ]
    );
    let indices = list
        .entries()
        .map(|entry| entry.r3_order_index)
        .collect::<Vec<_>>();
    assert_eq!(indices, (0..6).collect::<Vec<_>>());
}

#[test]
fn a_fail_closed_backend_turns_a_miss_into_a_search_error() {
    let project = tempfile::tempdir().unwrap();
    let mut list = tiny_list(project.path(), 4);
    let pack = fixture::FixturePack::empty(&RerankFingerprint {
        backend: "onnx",
        model: "bge-reranker-base".to_string(),
        revision: "r".to_string(),
    });
    let selected = Ok(SelectedBackend {
        backend: Arc::new(fixture::FixtureBackend::from_pack(&pack)),
        fail_closed: true,
    });
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);
    let error = rerank_block_zero(
        &mut list,
        &selected,
        &settings,
        &head_request(
            &search,
            project.path(),
            Some("where is the fixture miss reported"),
            None,
        ),
    )
    .expect_err("a fixture miss must fail the search");
    assert!(error.contains("no recorded score"), "{error}");
}

#[test]
fn recorder_captures_scores_that_the_fixture_backend_then_replays() {
    let project = tempfile::tempdir().unwrap();
    let pack_path = project.path().join("pack.json");
    let live = NumberedBackend::new(Vec::new());
    let recorder = fixture::recording(live.clone(), pack_path.clone());
    let search = SearchConfig::default();
    let settings = RerankSettings::resolve(&search);

    let mut recorded_list = tiny_list(project.path(), 5);
    rerank_block_zero(
        &mut recorded_list,
        &Ok(SelectedBackend {
            backend: recorder,
            fail_closed: false,
        }),
        &settings,
        &head_request(
            &search,
            project.path(),
            Some("where are the recorded scores kept"),
            None,
        ),
    )
    .unwrap();

    let pack = fixture::FixturePack::load(&pack_path).unwrap();
    assert_eq!(pack.scores.len(), 5);
    assert_eq!(pack.fingerprint.backend, "numbered");
    assert_eq!(pack.text_policy, TEXT_POLICY_REVISION);

    // Replaying the recorded scores from the pack gives the same order without
    // calling the live backend.
    let mut replayed_list = tiny_list(project.path(), 5);
    replayed_list.key.snapshot_generation = "g2".to_string();
    rerank_block_zero(
        &mut replayed_list,
        &Ok(SelectedBackend {
            backend: Arc::new(fixture::FixtureBackend::from_pack(&pack)),
            fail_closed: true,
        }),
        &settings,
        &head_request(
            &search,
            project.path(),
            Some("where are the recorded scores kept"),
            None,
        ),
    )
    .unwrap();
    assert_eq!(names(&replayed_list), names(&recorded_list));
    assert_eq!(live.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn invalid_backend_configuration_fails_build_with_a_reason() {
    let inputs = |rerank: RerankConfig| slot::BuildInputs {
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
        search: SearchConfig {
            rerank: Some(rerank),
        },
        semantic: Default::default(),
        under_subc: false,
    };
    for (backend, reason) in [
        (
            RerankBackendKind::Remote,
            "remote rerank endpoint is required",
        ),
        (
            RerankBackendKind::Synapse,
            "synapse rerank model is required",
        ),
    ] {
        let built = slot::build_backend(&inputs(RerankConfig {
            backend: Some(backend),
            ..RerankConfig::default()
        }));
        assert_eq!(built.err().as_deref(), Some(reason));
    }
    let built = slot::build_backend(&inputs(RerankConfig {
        backend: Some(RerankBackendKind::Onnx),
        model: Some("some-other-model".to_string()),
        ..RerankConfig::default()
    }));
    assert!(built.err().unwrap().contains("unsupported rerank model"));
    assert!(slot::build_backend(&inputs(RerankConfig::default()))
        .unwrap()
        .is_none());
}

// ---- the backend slot ----------------------------------------------------

fn onnx_inputs(model: Option<&str>) -> slot::BuildInputs {
    slot::BuildInputs {
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
        search: SearchConfig {
            rerank: Some(RerankConfig {
                backend: Some(RerankBackendKind::Onnx),
                model: model.map(str::to_string),
                ..RerankConfig::default()
            }),
        },
        semantic: Default::default(),
        under_subc: false,
    }
}

fn numbered_selected() -> SelectedBackend {
    SelectedBackend {
        backend: NumberedBackend::new(Vec::new()),
        fail_closed: false,
    }
}

fn wait_for(mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "condition never held"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn is_ready(installed: &slot::Installed) -> bool {
    matches!(installed, slot::Installed::Ready(_))
}

#[test]
fn the_search_path_reads_the_slot_without_io_and_never_builds_on_its_thread() {
    let search_thread = std::thread::current().id();
    let built_on_search_thread = Arc::new(AtomicUsize::new(0));
    let builds = Arc::new(AtomicUsize::new(0));
    let constructor: slot::Constructor = {
        let built_on_search_thread = built_on_search_thread.clone();
        let builds = builds.clone();
        Arc::new(move |inputs: &slot::BuildInputs| {
            builds.fetch_add(1, Ordering::SeqCst);
            if std::thread::current().id() == search_thread {
                built_on_search_thread.fetch_add(1, Ordering::SeqCst);
            }
            // Stands in for real construction, which does I/O.
            note_io();
            match RerankSettings::resolve(&inputs.search).backend {
                RerankBackendKind::Off => Ok(None),
                _ => Ok(Some(numbered_selected())),
            }
        })
    };
    let mut all_inputs = vec![slot::BuildInputs {
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
        search: SearchConfig::default(),
        semantic: Default::default(),
        under_subc: false,
    }];
    for backend in [
        RerankBackendKind::Off,
        RerankBackendKind::Onnx,
        RerankBackendKind::Remote,
        RerankBackendKind::Synapse,
    ] {
        for under_subc in [false, true] {
            all_inputs.push(slot::BuildInputs {
                cache_unavailable: false,
                cache_lease: None,
                cache_epoch: 0,
                search: SearchConfig {
                    rerank: Some(RerankConfig {
                        backend: Some(backend),
                        ..RerankConfig::default()
                    }),
                },
                semantic: Default::default(),
                under_subc,
            });
        }
    }
    for model in onnx::ALLOWED_MODELS.iter().map(|model| model.name) {
        all_inputs.push(onnx_inputs(Some(model)));
    }

    let before = io_on_this_thread();
    for inputs in all_inputs {
        let slot = slot::BackendSlot::with_constructor(constructor.clone());
        let off = RerankSettings::resolve(&inputs.search).backend == RerankBackendKind::Off;
        // First read: an unseen config starts a build and reads as not ready
        // (or off). The build runs on another thread and can finish before
        // `read` re-reads the slot, so `Ready` is also correct here, but only
        // once that build has run.
        let builds_before = builds.load(Ordering::SeqCst);
        match slot.read(inputs.clone()) {
            slot::Installed::Off => assert!(off),
            slot::Installed::NotReady(_) => assert!(!off),
            slot::Installed::Ready(_) => assert!(
                !off && builds.load(Ordering::SeqCst) > builds_before,
                "installed before any build ran"
            ),
        }
        if !off {
            wait_for(|| is_ready(&slot.read(inputs.clone())));
            // Everything the search path asks of an installed backend.
            let slot::Installed::Ready(selected) = slot.read(inputs.clone()) else {
                unreachable!()
            };
            let first = selected.backend.fingerprint();
            let _ = selected.backend.max_batch();
            assert_eq!(selected.backend.fingerprint(), first);
        }
    }
    assert_eq!(io_on_this_thread(), before, "the search path reached I/O");
    assert_eq!(built_on_search_thread.load(Ordering::SeqCst), 0);
    assert!(builds.load(Ordering::SeqCst) > 0);
}

#[test]
fn a_failed_build_is_retried_with_backoff_and_a_success_installs_a_new_instance() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let constructor: slot::Constructor = {
        let attempts = attempts.clone();
        Arc::new(move |_: &slot::BuildInputs| {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Err("server not reachable".to_string())
            } else {
                Ok(Some(numbered_selected()))
            }
        })
    };
    let slot = slot::BackendSlot::default();
    slot.set_constructor_for_test(constructor, Duration::from_millis(5));
    let inputs = onnx_inputs(None);
    slot.reconcile(inputs.clone());
    wait_for(|| is_ready(&slot.read(inputs.clone())));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);

    // A constructor that always fails is tried a bounded number of times and
    // leaves the slot not ready, with the last reason.
    let attempts = Arc::new(AtomicUsize::new(0));
    let constructor: slot::Constructor = {
        let attempts = attempts.clone();
        Arc::new(move |_: &slot::BuildInputs| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err("still down".to_string())
        })
    };
    let slot = slot::BackendSlot::default();
    slot.set_constructor_for_test(constructor, Duration::from_millis(1));
    slot.reconcile(inputs.clone());
    wait_for(|| attempts.load(Ordering::SeqCst) == slot::MAX_BUILD_ATTEMPTS as usize);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        slot::MAX_BUILD_ATTEMPTS as usize
    );
    match slot.read(inputs) {
        slot::Installed::NotReady(reason) => assert_eq!(reason, "still down"),
        _ => panic!("a failed build must leave the slot not ready"),
    }
}

#[test]
fn a_build_for_superseded_inputs_is_discarded() {
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let constructor: slot::Constructor = {
        let gate = gate.clone();
        Arc::new(move |inputs: &slot::BuildInputs| {
            if RerankSettings::resolve(&inputs.search).model.as_deref() == Some("slow") {
                let (open, changed) = &*gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = changed.wait(open).unwrap();
                }
            }
            Ok(Some(numbered_selected()))
        })
    };
    let slot = slot::BackendSlot::with_constructor(constructor);
    slot.reconcile(onnx_inputs(Some("slow")));
    let current = onnx_inputs(Some("fast"));
    slot.reconcile(current.clone());
    wait_for(|| is_ready(&slot.read(current.clone())));
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    std::thread::sleep(Duration::from_millis(50));
    // The slow build finished last but belonged to superseded inputs.
    assert!(is_ready(&slot.read(current)));
}

#[test]
fn a_slow_build_never_delays_a_search_and_searches_skip_until_it_is_installed() {
    let (_project, ctx) = probe_project(30);
    let fused = stream(&ctx, 100, 30);

    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let backend = NumberedBackend::new(Vec::new());
    let constructor: slot::Constructor = {
        let gate = gate.clone();
        let backend = backend.clone();
        Arc::new(move |_: &slot::BuildInputs| {
            let (open, changed) = &*gate;
            let mut open = open.lock().unwrap();
            while !*open {
                // Bounded so a broken test cannot hang the suite.
                let (next, timeout) = changed.wait_timeout(open, Duration::from_secs(30)).unwrap();
                open = next;
                if timeout.timed_out() {
                    break;
                }
            }
            Ok(Some(SelectedBackend {
                backend: backend.clone(),
                fail_closed: false,
            }))
        })
    };
    ctx.rerank_slot()
        .set_constructor_for_test(constructor, Duration::from_millis(5));
    ctx.update_config(|config| {
        config.search.rerank = Some(RerankConfig {
            backend: Some(RerankBackendKind::Onnx),
            ..RerankConfig::default()
        });
    });

    let started = Instant::now();
    let (first, text) = page(&ctx, 10, 0);
    let (second, _) = page(&ctx, 10, 10);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a search waited for the backend build"
    );
    assert!(text.contains("rerank skipped: backend not ready"), "{text}");
    assert_eq!(first, fused[..10]);
    assert_eq!(second, fused[10..20]);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);

    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    wait_for(|| is_ready(&installed_backend(&ctx)));
    let reranked = stream(&ctx, 100, 30);
    let mut expected_head = fused[..DEFAULT_TOP_N].to_vec();
    expected_head.sort_by(|left, right| {
        file_number(&format!("src/{right}")).total_cmp(&file_number(&format!("src/{left}")))
    });
    assert_eq!(reranked[..DEFAULT_TOP_N], expected_head[..]);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn onnx_fingerprints_carry_the_pinned_commit() {
    let mut names = HashSet::new();
    let is_hex = |value: &str, len: usize| {
        value.len() == len
            && value
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    };
    for model in onnx::ALLOWED_MODELS {
        assert!(names.insert(model.name), "duplicate model {}", model.name);
        assert!(is_hex(model.commit, 40), "{} commit", model.name);
        for file in std::iter::once(&model.model_file)
            .chain(model.extra_files)
            .chain([&model.tokenizer_file])
        {
            assert!(is_hex(file.sha256, 64), "{} {}", model.name, file.path);
        }
        let fingerprint = onnx::shared_onnx_reranker(Some(model.name))
            .unwrap()
            .fingerprint();
        assert_eq!(fingerprint.backend, "onnx");
        assert_eq!(fingerprint.model, model.name);
        assert_eq!(fingerprint.revision, model.commit);
    }
    assert!(names.contains(onnx::DEFAULT_MODEL));
}

#[test]
fn a_cached_file_with_the_wrong_hash_is_refused_and_deleted() {
    let cache = tempfile::tempdir().unwrap();
    let model = onnx::find_allowed_model("gte-reranker-modernbert-base").unwrap();
    let snapshot = onnx::pinned_snapshot_dir(cache.path(), model);
    std::fs::create_dir_all(snapshot.join("onnx")).unwrap();
    // hf-hub stores content in a blob store and links the snapshot to it; the
    // blob behind a bad file must go too.
    let blobs = cache.path().join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let blob = blobs.join("tampered");
    std::fs::write(&blob, b"not the pinned model").unwrap();
    let model_path = snapshot.join("onnx/model.onnx");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&blob, &model_path).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&blob, &model_path).unwrap();
    std::fs::write(snapshot.join("tokenizer.json"), b"{}").unwrap();

    let error = onnx::verify_pinned_files(&snapshot, model).expect_err("a wrong hash is refused");
    assert!(
        error.contains("onnx/model.onnx") && error.contains("deleted"),
        "{error}"
    );
    assert!(!model_path.exists() && std::fs::symlink_metadata(&model_path).is_err());
    #[cfg(unix)]
    assert!(!blob.exists(), "the blob behind the bad file is deleted");
}

// ---- ONNX worker lifecycle (fake scorer) -------------------------------

struct CountingScorer {
    computed: Arc<AtomicUsize>,
    delay: Duration,
}

impl PairScorer for CountingScorer {
    fn score_pairs(
        &mut self,
        _query: &str,
        docs: &[String],
        _deadline: Instant,
    ) -> Result<Vec<f32>, RerankError> {
        // Counted when inference starts, so a test can tell a dropped request
        // from one that was computed after its caller gave up.
        self.computed.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        Ok(docs.iter().map(|doc| doc.len() as f32).collect())
    }
}

fn fake_onnx(delay: Duration, loads: Arc<AtomicUsize>, computed: Arc<AtomicUsize>) -> OnnxReranker {
    let loader: ScorerLoader = Arc::new(move || {
        loads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingScorer {
            computed: computed.clone(),
            delay,
        }) as Box<dyn PairScorer>)
    });
    OnnxReranker::with_loader(
        RerankFingerprint {
            backend: "onnx",
            model: "fake".to_string(),
            revision: "0".to_string(),
        },
        loader,
    )
}

fn wait_until_ready(backend: &OnnxReranker) {
    let docs = [RerankDoc { text: "x" }];
    for _ in 0..200 {
        match backend.score("q", &docs, Instant::now() + Duration::from_secs(2)) {
            Err(RerankError::Unavailable(reason)) if reason == "model loading" => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => return,
        }
    }
    panic!("the fake model never became ready");
}

#[test]
fn onnx_first_use_starts_loading_off_the_request_and_skips() {
    let loads = Arc::new(AtomicUsize::new(0));
    let computed = Arc::new(AtomicUsize::new(0));
    let backend = fake_onnx(Duration::ZERO, loads.clone(), computed.clone());
    let docs = [RerankDoc { text: "ab" }];
    let started = Instant::now();
    let first = backend.score("q", &docs, Instant::now() + Duration::from_secs(5));
    assert_eq!(
        first,
        Err(RerankError::Unavailable("model loading".to_string()))
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    wait_until_ready(&backend);
    assert_eq!(
        backend.score("q", &docs, Instant::now() + Duration::from_secs(5)),
        Ok(vec![2.0])
    );
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[test]
fn onnx_worker_drops_a_request_whose_deadline_passed_and_refuses_while_busy() {
    let computed = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    struct GatedScorer {
        computed: Arc<AtomicUsize>,
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    }
    impl PairScorer for GatedScorer {
        fn score_pairs(
            &mut self,
            query: &str,
            docs: &[String],
            _deadline: Instant,
        ) -> Result<Vec<f32>, RerankError> {
            self.computed.fetch_add(1, Ordering::SeqCst);
            if query == "held" {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            Ok(docs.iter().map(|doc| doc.len() as f32).collect())
        }
    }
    let scorer_count = computed.clone();
    let loader: ScorerLoader = Arc::new(move || {
        Ok(Box::new(GatedScorer {
            computed: scorer_count.clone(),
            entered: entered_tx.clone(),
            release: release_rx.clone(),
        }) as Box<dyn PairScorer>)
    });
    let backend = OnnxReranker::with_loader(
        RerankFingerprint {
            backend: "onnx",
            model: "gated".into(),
            revision: "0".into(),
        },
        loader,
    );
    wait_until_ready(&backend);
    let baseline = computed.load(Ordering::SeqCst);
    let docs = [RerankDoc { text: "abc" }];

    // A slow job occupies the worker; a second request is refused as busy
    // instead of queuing behind it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let slow = backend.enqueue("held", &docs, deadline).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(
        backend.score("q", &docs, Instant::now() + Duration::from_secs(5)),
        Err(RerankError::Unavailable("busy".to_string()))
    );
    // The caller of the slow job gave up at its deadline.
    assert_eq!(
        OnnxReranker::wait_for_reply(&slow, deadline, deadline),
        Err(RerankError::Timeout)
    );
    release_tx.send(()).unwrap();
    // The worker clears busy before sending the reply, even after the caller
    // has timed out. Receiving it is an idle handshake, not a timing guess.
    assert_eq!(
        slow.recv_timeout(Duration::from_secs(30)).unwrap(),
        Ok(vec![3.0])
    );

    // A request whose deadline has already passed when the worker receives it
    // is dropped without inference.
    let computed_before = computed.load(Ordering::SeqCst);
    let expired = backend.enqueue("q", &docs, Instant::now()).unwrap();
    assert_eq!(
        expired.recv_timeout(Duration::from_secs(30)).unwrap(),
        Err(RerankError::Timeout)
    );
    assert_eq!(computed.load(Ordering::SeqCst), computed_before);
    assert!(computed_before > baseline);
}

/// Provisions the pinned gte-reranker-modernbert-base into the model cache
/// named by `AFT_RERANK_ONNX_TEST_CACHE_DIR` (downloading the pinned commit and
/// verifying every file hash when it is not already there), loads it, scores
/// 20 query/document pairs and prints the latency. Runs only when the variable
/// is set (it needs the network or a populated cache); otherwise it returns
/// without loading anything.
#[test]
fn real_onnx_model_scores_twenty_pairs_when_available() {
    let Some(cache_dir) = std::env::var_os("AFT_RERANK_ONNX_TEST_CACHE_DIR").map(PathBuf::from)
    else {
        eprintln!("skipped: AFT_RERANK_ONNX_TEST_CACHE_DIR is not set");
        return;
    };
    let model = onnx::find_allowed_model("gte-reranker-modernbert-base").unwrap();
    let started = Instant::now();
    let files = onnx::provision_model_files_in(model, &cache_dir).expect("provision pinned model");
    eprintln!(
        "provisioned {}@{} in {} ms",
        model.repo,
        model.commit,
        started.elapsed().as_millis()
    );
    assert!(files
        .model
        .starts_with(onnx::pinned_snapshot_dir(&cache_dir, model)));
    let threads = onnx::rerank_intra_threads();
    let mut scorer =
        onnx::OnnxPairScorer::load(model.name, &files, threads).expect("load test model");
    let query = "where is the session cache invalidated";
    let mut docs = (0..19)
        .map(|index| {
            format!(
                "src/render/widget_{index}.rs\npub fn draw_widget_{index}(frame: &mut Frame) {{\n    frame.fill(Color::rgb({index}, 0, 0));\n    frame.stroke_border();\n}}"
            )
        })
        .collect::<Vec<_>>();
    docs.push(
        "src/session/cache.rs\npub fn invalidate(&mut self, id: SessionId) {\n    // Drop the cached session so the next lookup reloads it.\n    self.entries.remove(&id);\n}"
            .to_string(),
    );
    // Run once untimed (the first run allocates buffers), then time a second.
    scorer
        .score_pairs(query, &docs, Instant::now() + Duration::from_secs(60))
        .unwrap();
    let started = Instant::now();
    let scores = scorer
        .score_pairs(query, &docs, Instant::now() + Duration::from_secs(60))
        .unwrap();
    let elapsed = started.elapsed();
    eprintln!(
        "real rerank model: 20 pairs in {} ms with {threads} intra-op threads",
        elapsed.as_millis()
    );
    assert_eq!(scores.len(), 20);
    let best = scores
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .unwrap()
        .0;
    assert_eq!(
        best, 19,
        "the session-cache document should score highest: {scores:?}"
    );

    // Twenty candidates filled to the candidate-text byte budget, the longest
    // text the engine builds.
    let long_docs = (0..20)
        .map(|index| {
            let mut doc = format!("src/module_{index}.rs\n");
            while doc.len() < CANDIDATE_TEXT_BUDGET_BYTES - 40 {
                doc.push_str(&format!(
                    "    let value_{index} = compute(state, {index});\n"
                ));
            }
            doc
        })
        .collect::<Vec<_>>();
    let started = Instant::now();
    let scores = scorer
        .score_pairs(query, &long_docs, Instant::now() + Duration::from_secs(60))
        .unwrap();
    eprintln!(
        "real rerank model: 20 budget-sized pairs in {} ms",
        started.elapsed().as_millis()
    );
    assert_eq!(scores.len(), 20);
}

/// CPU time this process has used, user plus system, in seconds.
#[cfg(unix)]
fn process_cpu_seconds() -> f64 {
    // SAFETY: getrusage only writes the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let seconds = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

/// Resident memory of this process in MB, as `ps` reports it.
#[cfg(unix)]
fn process_rss_mb() -> f64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1024.0
}

/// Cost profile of the local ONNX reranker on realistic candidates: the
/// engine's own candidate texts for source files of this crate, scored for a
/// series of distinct queries the way consecutive searches would be. Prints
/// token lengths, attention batches, and per-call wall time, CPU time and
/// resident memory. Runs only when `AFT_RERANK_ONNX_TEST_CACHE_DIR` names a
/// cache holding the pinned gte-reranker-modernbert-base. The session options
/// can be varied with `AFT_RERANK_PROFILE_SPINNING`,
/// `AFT_RERANK_PROFILE_MEMORY_PATTERN`, `AFT_RERANK_PROFILE_CPU_ARENA` and
/// `AFT_RERANK_PROFILE_ARENA_SHRINKAGE` (`0` or `1`) and
/// `AFT_RERANK_PROFILE_ATTENTION_UNITS`, the thread count with
/// `AFT_RERANK_PROFILE_THREADS`, and the candidate count and text budget with
/// `AFT_RERANK_PROFILE_DOCS` and `AFT_RERANK_PROFILE_BYTES`.
#[cfg(unix)]
#[test]
fn onnx_cost_profile_when_available() {
    use super::super::evidence_descriptor::EvidenceDescriptor;

    let Some(cache_dir) = std::env::var_os("AFT_RERANK_ONNX_TEST_CACHE_DIR").map(PathBuf::from)
    else {
        eprintln!("skipped: AFT_RERANK_ONNX_TEST_CACHE_DIR is not set");
        return;
    };
    let flag =
        |name: &str, default: bool| std::env::var(name).map_or(default, |value| value != "0");
    let number = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let defaults = onnx::SessionTuning::default();
    let tuning = onnx::SessionTuning {
        intra_op_spinning: flag("AFT_RERANK_PROFILE_SPINNING", defaults.intra_op_spinning),
        memory_pattern: flag("AFT_RERANK_PROFILE_MEMORY_PATTERN", defaults.memory_pattern),
        cpu_arena: flag("AFT_RERANK_PROFILE_CPU_ARENA", defaults.cpu_arena),
        arena_shrinkage: flag(
            "AFT_RERANK_PROFILE_ARENA_SHRINKAGE",
            defaults.arena_shrinkage,
        ),
        attention_units: number(
            "AFT_RERANK_PROFILE_ATTENTION_UNITS",
            defaults.attention_units,
        ),
        ..defaults
    };
    let doc_count = number("AFT_RERANK_PROFILE_DOCS", DEFAULT_TOP_N);
    let byte_budget = number("AFT_RERANK_PROFILE_BYTES", CANDIDATE_TEXT_BUDGET_BYTES);
    let threads = number("AFT_RERANK_PROFILE_THREADS", onnx::rerank_intra_threads());

    let model = onnx::find_allowed_model("gte-reranker-modernbert-base").unwrap();
    let files = onnx::provision_model_files_in(model, &cache_dir).expect("provision pinned model");
    let rss_before_load = process_rss_mb();
    let mut scorer = onnx::OnnxPairScorer::load_with(model.name, &files, threads, tuning)
        .expect("load test model");
    let rss_loaded = process_rss_mb();

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = std::fs::read_dir(root.join("src"))
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    sources.sort();
    let queries = [
        "where is the session cache invalidated after a config change",
        "how are background bash tasks restored after restart",
        "how does the semantic index persist across configure runs",
        "where are lsp diagnostics polled for server responses",
        "how are imports organized after edits",
        "which code writes the backup before an edit is applied",
        "how does the watcher invalidate stale files",
        "where is the search result page cut and paged",
        "how are tool call timeouts reported to the agent",
        "where is the project root resolved from the request path",
        "how is the callgraph store refreshed after a file change",
        "where are outline symbols extracted from tree-sitter",
    ];
    eprintln!(
        "profile: threads={threads} {tuning:?} docs={doc_count} bytes={byte_budget} rss_before_load_mb={rss_before_load:.0} rss_loaded_mb={rss_loaded:.0}"
    );
    for (index, query) in queries.iter().enumerate() {
        // A sliding window over the files, so consecutive calls see
        // different texts and lengths, as consecutive searches do.
        let docs = (0..doc_count)
            .map(|offset| {
                let path = sources[(index * 7 + offset) % sources.len()].clone();
                let result = CandidateResult::new_exact(
                    path,
                    None,
                    EvidenceDescriptor::for_e1(1, true, false),
                );
                let mut text = candidate_text(root, &result, query);
                let mut cut = byte_budget.min(text.len());
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text.truncate(cut);
                text
            })
            .collect::<Vec<_>>();
        let lengths = scorer.pair_token_lengths(query, &docs).unwrap();
        let batches = onnx::attention_batches_for_lengths(&lengths, tuning.attention_units);
        let cpu = process_cpu_seconds();
        let started = Instant::now();
        scorer
            .score_pairs(query, &docs, Instant::now() + Duration::from_secs(120))
            .unwrap();
        let wall = started.elapsed().as_secs_f64();
        let cpu = process_cpu_seconds() - cpu;
        eprintln!(
            "call={index:02} tokens_total={} tokens_max={} batches={:?} wall_s={wall:.3} cpu_s={cpu:.3} rss_mb={:.0}",
            lengths.iter().sum::<usize>(),
            lengths.iter().max().unwrap(),
            batches.iter().map(|range| range.len()).collect::<Vec<_>>(),
            process_rss_mb()
        );
    }
}
#[test]
fn remote_slot_factory_installs_ready_and_reports_bad_configuration() {
    let (endpoint, server) =
        remote::tests::serve(r#"[{"index":0,"score":0.7}]"#, 200, Duration::ZERO);
    let inputs = slot::BuildInputs {
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
        search: SearchConfig {
            rerank: Some(RerankConfig {
                backend: Some(RerankBackendKind::Remote),
                model: Some("served-model".into()),
                endpoint: Some(format!("tei+{endpoint}")),
                ..Default::default()
            }),
        },
        semantic: Default::default(),
        under_subc: false,
    };
    let slot = slot::BackendSlot::default();
    slot.reconcile(inputs.clone());
    wait_for(|| is_ready(&slot.read(inputs.clone())));
    let slot::Installed::Ready(selected) = slot.read(inputs.clone()) else {
        panic!("remote build was not installed")
    };
    assert!(!selected.fail_closed);
    assert_eq!(selected.backend.max_batch(), 20);
    assert_eq!(selected.backend.fingerprint().model, "served-model");
    assert_eq!(
        selected
            .backend
            .score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap(),
        vec![0.7]
    );
    assert_eq!(
        server.join().unwrap(),
        serde_json::json!({"query":"q","texts":["a"]})
    );
    let mut invalid = inputs;
    invalid.search.rerank.as_mut().unwrap().endpoint = Some("file:///tmp/model".into());
    slot.reconcile(invalid.clone());
    wait_for(
        || matches!(slot.read(invalid.clone()), slot::Installed::NotReady(reason) if reason.contains("HTTP(S) base URL")),
    );
    // Turn reranking off after checking the failure reason, so retries for this
    // invalid endpoint cannot install a backend in the test's slot.
    let mut off = invalid;
    off.search.rerank = None;
    slot.reconcile(off);
}

#[test]
fn synapse_slot_factory_installs_pinned_ready_and_reports_failed_discovery() {
    let (_directory, path, server) = synapse::tests::fake_daemon(
        vec![
            Some(
                serde_json::json!({"result":{"models":[{"model_id":"reranker","fingerprints":["fp-slot"]}]}}),
            ),
            Some(serde_json::json!({"result":{"fingerprint":"fp-slot","scores":[3.0]}})),
        ],
        true,
    );
    let inputs = slot::BuildInputs {
        cache_unavailable: false,
        cache_lease: None,
        cache_epoch: 0,
        search: SearchConfig {
            rerank: Some(RerankConfig {
                backend: Some(RerankBackendKind::Synapse),
                model: Some("reranker".into()),
                ..Default::default()
            }),
        },
        semantic: synapse::tests::config(path),
        under_subc: true,
    };
    let slot = slot::BackendSlot::default();
    slot.reconcile(inputs.clone());
    wait_for(|| match slot.read(inputs.clone()) {
        slot::Installed::Ready(_) => true,
        slot::Installed::NotReady(reason) => reason != "backend not ready",
        slot::Installed::Off => false,
    });
    let slot::Installed::Ready(selected) = slot.read(inputs.clone()) else {
        panic!("Synapse build was not installed")
    };
    assert!(!selected.fail_closed);
    assert_eq!(selected.backend.fingerprint().revision, "fp-slot");
    assert_eq!(
        selected
            .backend
            .score(
                "q",
                &[RerankDoc { text: "a" }],
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap(),
        vec![3.0]
    );
    let requests = server.join().unwrap();
    assert_eq!(requests[0]["method"], "models.list");
    assert_eq!(requests[1]["params"]["required_fingerprint"], "fp-slot");
    let (_directory, path, server) = synapse::tests::fake_daemon(
        vec![Some(serde_json::json!({"result":{"models":[]}}))],
        true,
    );
    let mut invalid = inputs;
    invalid.semantic = synapse::tests::config(path);
    slot.reconcile(invalid.clone());
    wait_for(
        || matches!(slot.read(invalid.clone()), slot::Installed::NotReady(reason) if reason.contains("does not serve reranker")),
    );
    assert_eq!(server.join().unwrap().len(), 1);
    let mut off = invalid;
    off.search.rerank = None;
    slot.reconcile(off);
}

#[test]
fn blocked_synapse_connect_never_delays_searches_reading_not_ready() {
    let (_project, ctx) = probe_project(30);
    let fused = stream(&ctx, 100, 30);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (_directory, path, server) = synapse::tests::fake_daemon_gated(
        vec![Some(
            serde_json::json!({"result":{"models":[{"model_id":"reranker","fingerprints":["fp-slot"]}]}}),
        )],
        true,
        Some((entered_tx, release_rx)),
    );
    ctx.update_config(|config| {
        config.semantic = synapse::tests::config(path);
        config.search.rerank = Some(RerankConfig {
            backend: Some(RerankBackendKind::Synapse),
            model: Some("reranker".into()),
            timeout_ms: Some(5000),
            ..Default::default()
        });
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = Instant::now();
    let (first, text) = page(&ctx, 10, 0);
    let (second, _) = page(&ctx, 10, 10);
    let elapsed = started.elapsed();
    let pending = matches!(installed_backend(&ctx), slot::Installed::NotReady(_));
    release_tx.send(()).unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "search waited for Synapse connect: {elapsed:?}"
    );
    assert!(
        pending,
        "search should read NotReady while models.list is blocked"
    );
    assert!(text.contains("rerank skipped: backend not ready"), "{text}");
    assert_eq!(first, fused[..10]);
    assert_eq!(second, fused[10..20]);
    wait_for(|| is_ready(&installed_backend(&ctx)));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "models.list");
}

#[test]
fn cache_root_unavailable_reranker_never_activates() {
    let mut inputs = onnx_inputs(None);
    inputs.cache_unavailable = true;
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&builds);
    let slot = slot::BackendSlot::with_constructor(Arc::new(move |_| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(None)
    }));
    assert!(matches!(slot.read(inputs.clone()), slot::Installed::Off));
    assert_eq!(builds.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(slot::build_backend(&inputs)
        .err()
        .unwrap()
        .contains("cache_dir_unavailable"));
}

#[test]
fn cache_root_onnx_factory_passes_configured_model_cache_to_provisioner() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("volume");
    std::fs::create_dir(&parent).unwrap();
    // Resolve the folder the way prepare does (on Windows, the plain spelling
    // rather than a verbatim `\\?\` path), so the simulated volume and the
    // expected model path name the folder prepare will use.
    let parent = crate::bash_background::persistence::canonical_root_path(&parent).unwrap();
    let model = onnx::ALLOWED_MODELS.last().unwrap().name;
    let mut config = crate::config::Config {
        storage_dir: Some(temp.path().join("storage")),
        cache_dir: Some(parent.clone()),
        ..Default::default()
    };
    config.search.rerank = Some(crate::config::RerankConfig {
        backend: Some(crate::config::RerankBackendKind::Onnx),
        model: Some(model.to_string()),
        ..Default::default()
    });
    assert!(crate::root_cache::prepare_cache_root_for_test(&mut config).is_none());
    let inputs = slot::BuildInputs::from_config(&config, false);
    let (result, observed) = onnx::with_provision_probe(model, || slot::build_backend(&inputs));
    assert!(result.err().unwrap().contains("provision path observed"));
    assert_eq!(observed, vec![parent.join("aft/semantic/models")]);
    assert!(!temp.path().join("storage/semantic").exists());
}

#[test]
fn cache_root_onnx_backends_do_not_share_across_cache_paths() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first-model-cache");
    let second = temp.path().join("second-model-cache");
    let one = onnx::shared_onnx_reranker_in(None, Some(first.clone()), None).unwrap();
    let same = onnx::shared_onnx_reranker_in(None, Some(first.clone()), None).unwrap();
    let other = onnx::shared_onnx_reranker_in(None, Some(second.clone()), None).unwrap();
    assert!(Arc::ptr_eq(&one, &same));
    assert!(!Arc::ptr_eq(&one, &other));
    assert!(!first.exists());
    assert!(!second.exists());
}

/// The reranker's cache lease only proves the cache context is alive. When
/// the cache volume disappears and leaves a writable empty folder at its path,
/// the model download must still refuse rather than let hf-hub recreate the
/// folders and put the model on the main disk.
#[test]
fn cache_root_rerank_model_download_refuses_a_replaced_cache_root() {
    let temp = tempfile::tempdir().unwrap();
    let storage = temp.path().join("storage");
    let volume = temp.path().join("cache-volume");
    std::fs::create_dir(&volume).unwrap();
    let mut config = Config {
        storage_dir: Some(storage.clone()),
        cache_dir: Some(volume.clone()),
        ..Default::default()
    };
    let warning = crate::root_cache::prepare_cache_root_for_test(&mut config);
    assert!(warning.is_none(), "cache root not enrolled: {warning:?}");
    let models = config.semantic.local_cache_dir.clone().unwrap();
    // On Unix the lease stays alive, as it does for a running reranker whose
    // context never retired. Windows cannot remove a folder the lease holds.
    #[cfg(windows)]
    drop(config);
    #[cfg(not(windows))]
    let _live_lease = config;
    std::fs::remove_dir_all(&volume).unwrap();
    std::fs::create_dir(&volume).unwrap();

    let model = onnx::find_allowed_model(onnx::DEFAULT_MODEL).unwrap();
    let error = onnx::provision_model_files_in(model, &models).unwrap_err();

    assert!(
        error.starts_with(crate::root_cache::CACHE_DIR_UNAVAILABLE),
        "{error}"
    );
    assert_eq!(
        std::fs::read_dir(&volume).unwrap().count(),
        0,
        "the download recreated folders on the replacement volume"
    );
    assert!(!storage.join("semantic").exists());
}
