#![cfg(feature = "semantic-chunk-census")]

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use aft::parser::{detect_language, LangId};
use aft::semantic_index::{
    collect_file_chunks_for_census, is_semantic_indexed_extension, ChunkCaps, SemanticChunk,
};
use aft::symbols::SymbolKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

const MODEL_REPO: &str = "Alibaba-NLP/gte-modernbert-base";
const PRODUCTION_MODEL: &str = "text-embedding-qwen3-embedding-0.6b";
const PRODUCTION_EMBEDDINGS_URL: &str = "http://localhost:1234/v1/embeddings";
const EXPECTED_QWEN_DIMENSION: usize = 1024;
const PROBE_TARGETS: [usize; 5] = [400, 700, 1100, 2100, 4200];
const TOKENIZER_FILE: &str = "tokenizer.json";
const SEMANTIC_FILE_BYTES: u64 = 4 * 1024 * 1024;
const THRESHOLDS: [usize; 4] = [512, 1024, 2048, 4096];
const CEILINGS: [usize; 4] = [1024, 2048, 4096, 8192];
const CORPORA: [(&str, &str); 6] = [
    ("aft", "Work/Projects/CortexKit/aft"),
    ("magic-context", "Work/Projects/CortexKit/magic-context"),
    ("opencode", "Work/OSS/opencode"),
    ("rails", "Work/OSS/rails"),
    ("kubernetes", "Work/OSS/kubernetes"),
    ("elasticsearch", "Work/OSS/elasticsearch"),
];

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Row {
    kind: RowKind,
    language: u8,
    today_tokens: u32,
    uncapped_tokens: u32,
    body_cut_today: bool,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
enum RowKind {
    Symbol,
    FileSummary,
}

impl RowKind {
    fn label(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::FileSummary => "file summary",
        }
    }
}

#[derive(Deserialize, Serialize)]
struct CorpusResult {
    name: String,
    root: PathBuf,
    revision: String,
    dirty: bool,
    files: usize,
    oversized_files: usize,
    errors: Vec<(PathBuf, String)>,
    rows: Vec<Row>,
    probe_rows: Vec<ProbeCandidate>,
    probe_batch: Vec<ProbeCandidate>,
    tokenizer_sha256: String,
    file_start: usize,
    file_end: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct ProbeCandidate {
    text: String,
    source: String,
    chars: usize,
    estimated_tokens: usize,
}

struct ProbeResult {
    label: String,
    sources: String,
    chars: String,
    estimated_tokens: String,
    status: String,
    latency_ms: String,
    vectors: String,
    full_dimension: bool,
    error_body: String,
}

#[derive(Default)]
struct GroupStats {
    rows: usize,
    today_tokens: u64,
    uncapped_tokens: u64,
    percentiles: [usize; 4],
    over: [usize; 4],
    ceiling_tokens: [u64; 4],
}

struct TokenizerInfo {
    path: PathBuf,
    revision: String,
    sha256: String,
    downloaded: bool,
}

#[test]
#[ignore = "full-corpus investigation; writes a report when AFT_SEMANTIC_CENSUS_OUT is set"]
fn semantic_chunk_census() {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is required to locate corpora and the Hugging Face cache");
    let (tokenizer, tokenizer_info) = load_tokenizer(&home);
    let cache_dir = env::var_os("AFT_SEMANTIC_CENSUS_CACHE_DIR").map(PathBuf::from);

    if let Some(only) = env::var_os("AFT_SEMANTIC_CENSUS_ONLY") {
        let only = only.to_string_lossy();
        let (name, relative) = CORPORA
            .iter()
            .find(|(name, _)| *name == only)
            .copied()
            .expect("AFT_SEMANTIC_CENSUS_ONLY must name a configured corpus");
        let root = home.join(relative);
        assert!(
            root.is_dir(),
            "selected corpus is missing: {}",
            root.display()
        );
        let result = measure_corpus(name, root, &tokenizer, tokenizer_info.sha256.clone());
        let cache_dir = cache_dir.expect("sharded census requires AFT_SEMANTIC_CENSUS_CACHE_DIR");
        save_corpus_cache(&cache_dir, &result);
        return;
    }

    let mut results = Vec::new();
    let mut missing = Vec::new();

    for (name, relative) in CORPORA {
        let root = home.join(relative);
        if !root.is_dir() {
            eprintln!(
                "census: skipping missing corpus {name} ({})",
                root.display()
            );
            missing.push((name, root));
            continue;
        }
        if let Some(cache_dir) = cache_dir.as_deref() {
            results.push(load_corpus_cache(
                cache_dir,
                name,
                &root,
                &tokenizer_info.sha256,
            ));
        } else {
            results.push(measure_corpus(
                name,
                root,
                &tokenizer,
                tokenizer_info.sha256.clone(),
            ));
        }
    }

    assert!(!results.is_empty(), "none of the requested corpora exist");
    let aft_result = results
        .iter()
        .find(|result| result.name == "aft")
        .expect("the aft corpus is required for realistic production-lane probes");
    let probe_results = run_production_probes(&aft_result.probe_rows, &aft_result.probe_batch);
    let report = render_report(&home, &tokenizer_info, &results, &missing, &probe_results);
    let output = env::var_os("AFT_SEMANTIC_CENSUS_OUT")
        .map(PathBuf::from)
        .expect("set AFT_SEMANTIC_CENSUS_OUT to the report destination");
    fs::write(&output, report).expect("write semantic chunk census report");
    eprintln!("census: wrote {}", output.display());
}

fn measure_corpus(
    name: &str,
    root: PathBuf,
    tokenizer: &Tokenizer,
    tokenizer_sha256: String,
) -> CorpusResult {
    let mut files = aft::callgraph::walk_project_files(&root)
        .filter(|path| is_semantic_indexed_extension(path))
        .collect::<Vec<_>>();
    files.sort();

    let total_files = files.len();
    let (file_start, file_end) = env::var("AFT_SEMANTIC_CENSUS_FILE_RANGE")
        .ok()
        .map(|range| parse_file_range(&range, total_files))
        .unwrap_or((0, total_files));

    let revision = git_stdout(&root, &["rev-parse", "HEAD"])
        .unwrap_or_else(|| "not-a-git-worktree".to_string());
    let dirty =
        git_stdout(&root, &["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut oversized_files = 0;
    let mut probe_rows = vec![None; PROBE_TARGETS.len()];
    let mut probe_batch = Vec::new();
    let uncapped = ChunkCaps {
        signature_chars: usize::MAX,
        body_lines: usize::MAX,
        body_chars: usize::MAX,
        total_chars: usize::MAX,
    };

    eprintln!("census: {name}: files {file_start}..{file_end} of {total_files} semantic files");
    for (relative_index, file) in files[file_start..file_end].iter().enumerate() {
        let index = file_start + relative_index;
        if fs::metadata(file).is_ok_and(|metadata| metadata.len() > SEMANTIC_FILE_BYTES) {
            oversized_files += 1;
        }

        match collect_file_chunks_for_census(&root, file, uncapped) {
            Ok((today, full)) => {
                assert_eq!(
                    today.len(),
                    full.len(),
                    "caps changed row count for {}",
                    file.display()
                );
                let language = detect_language(file)
                    .expect("the production walker returned an unsupported language");
                for (today_chunk, full_chunk) in today.iter().zip(&full) {
                    assert_eq!(today_chunk.kind, full_chunk.kind);
                    assert_eq!(today_chunk.name, full_chunk.name);
                    assert_eq!(today_chunk.start_line, full_chunk.start_line);
                    assert_eq!(today_chunk.end_line, full_chunk.end_line);

                    let kind = if matches!(full_chunk.kind, SymbolKind::FileSummary) {
                        RowKind::FileSummary
                    } else {
                        RowKind::Symbol
                    };
                    rows.push(Row {
                        kind,
                        language: language_code(language),
                        today_tokens: token_count(tokenizer, &today_chunk.embed_text),
                        uncapped_tokens: token_count(tokenizer, &full_chunk.embed_text),
                        body_cut_today: kind == RowKind::Symbol
                            && full_chunk.end_line.saturating_sub(full_chunk.start_line) + 1 > 15,
                    });
                    if name == "aft"
                        && matches!(
                            full_chunk.kind,
                            SymbolKind::Function | SymbolKind::Method | SymbolKind::Kernel
                        )
                    {
                        consider_probe_candidate(
                            &root,
                            full_chunk,
                            &mut probe_rows,
                            &mut probe_batch,
                        );
                    }
                }
            }
            Err(error) => errors.push((file.clone(), error)),
        }

        if (index + 1) % 1_000 == 0 {
            eprintln!(
                "census: {name}: processed {}/{} files, {} rows",
                index + 1,
                total_files,
                rows.len()
            );
        }
    }
    eprintln!(
        "census: {name}: complete: {} rows, {} errors, {} oversized files",
        rows.len(),
        errors.len(),
        oversized_files
    );

    CorpusResult {
        name: name.to_string(),
        root,
        revision,
        dirty,
        files: total_files,
        oversized_files,
        errors,
        rows,
        probe_rows: if name == "aft" {
            probe_rows
                .into_iter()
                .map(|candidate| {
                    candidate.expect("aft corpus has a probe candidate for every target")
                })
                .collect()
        } else {
            Vec::new()
        },
        probe_batch,
        tokenizer_sha256,
        file_start,
        file_end,
    }
}

fn parse_file_range(range: &str, total_files: usize) -> (usize, usize) {
    let (start, end) = range
        .split_once(':')
        .expect("AFT_SEMANTIC_CENSUS_FILE_RANGE must be START:END");
    let start = start
        .parse::<usize>()
        .expect("file-range start is a number");
    let end = end.parse::<usize>().expect("file-range end is a number");
    assert!(start < end && end <= total_files, "invalid file range");
    (start, end)
}

fn save_corpus_cache(cache_dir: &Path, result: &CorpusResult) {
    fs::create_dir_all(cache_dir).expect("create semantic census cache directory");
    let filename = if result.file_start == 0 && result.file_end == result.files {
        format!("{}.json", result.name)
    } else {
        format!(
            "{}-{}-{}.json",
            result.name, result.file_start, result.file_end
        )
    };
    let path = cache_dir.join(filename);
    let file = fs::File::create(&path).expect("create semantic census corpus cache");
    serde_json::to_writer(BufWriter::new(file), result)
        .expect("write semantic census corpus cache");
    eprintln!("census: cached {} at {}", result.name, path.display());
}

fn load_corpus_cache(
    cache_dir: &Path,
    name: &str,
    root: &Path,
    tokenizer_sha256: &str,
) -> CorpusResult {
    let full_path = cache_dir.join(format!("{name}.json"));
    let mut shards = if full_path.is_file() {
        vec![read_corpus_cache(&full_path)]
    } else {
        let prefix = format!("{name}-");
        let mut paths = fs::read_dir(cache_dir)
            .expect("read semantic census cache directory")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|filename| filename.to_str())
                    .is_some_and(|filename| {
                        filename.starts_with(&prefix) && filename.ends_with(".json")
                    })
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .iter()
            .map(|path| read_corpus_cache(path))
            .collect::<Vec<_>>()
    };
    assert!(
        !shards.is_empty(),
        "no census cache shards found for {name}"
    );
    shards.sort_by_key(|shard| shard.file_start);
    let mut result = shards.remove(0);
    for mut shard in shards {
        assert_eq!(result.name, shard.name, "cache corpus name mismatch");
        assert_eq!(result.root, shard.root, "cache corpus root mismatch");
        assert_eq!(result.revision, shard.revision, "cache revision mismatch");
        assert_eq!(result.files, shard.files, "cache file-count mismatch");
        assert_eq!(
            result.tokenizer_sha256, shard.tokenizer_sha256,
            "cache tokenizer mismatch"
        );
        assert_eq!(
            result.file_end, shard.file_start,
            "cache shards have a gap or overlap"
        );
        result.file_end = shard.file_end;
        result.oversized_files += shard.oversized_files;
        result.errors.append(&mut shard.errors);
        result.rows.append(&mut shard.rows);
        result.probe_rows.append(&mut shard.probe_rows);
        result.probe_batch.append(&mut shard.probe_batch);
    }
    assert_eq!(
        result.file_start, 0,
        "first cache shard does not start at zero"
    );
    assert_eq!(
        result.file_end, result.files,
        "cache shards do not cover the corpus"
    );
    assert_eq!(result.name, name, "cache corpus name mismatch");
    assert_eq!(result.root, root, "cache corpus root mismatch");
    assert_eq!(
        result.tokenizer_sha256, tokenizer_sha256,
        "cache tokenizer mismatch"
    );
    eprintln!(
        "census: loaded {name}: {} rows from {}",
        result.rows.len(),
        cache_dir.display()
    );
    result
}

fn read_corpus_cache(path: &Path) -> CorpusResult {
    let file = fs::File::open(path)
        .unwrap_or_else(|error| panic!("open census cache {}: {error}", path.display()));
    serde_json::from_reader(BufReader::new(file))
        .unwrap_or_else(|error| panic!("read census cache {}: {error}", path.display()))
}

fn consider_probe_candidate(
    root: &Path,
    chunk: &SemanticChunk,
    probe_rows: &mut [Option<ProbeCandidate>],
    probe_batch: &mut Vec<ProbeCandidate>,
) {
    let chars = chunk.embed_text.chars().count();
    let estimated_tokens = chars.saturating_mul(2).saturating_add(3) / 7;
    let source = format!(
        "{}::{}",
        chunk
            .file
            .strip_prefix(root)
            .unwrap_or(&chunk.file)
            .display(),
        chunk.name
    );
    let candidate = || ProbeCandidate {
        text: chunk.embed_text.clone(),
        source: source.clone(),
        chars,
        estimated_tokens,
    };

    for (index, target) in PROBE_TARGETS.iter().enumerate() {
        let distance = estimated_tokens.abs_diff(*target);
        let should_replace = probe_rows[index]
            .as_ref()
            .is_none_or(|current| distance < current.estimated_tokens.abs_diff(*target));
        if should_replace {
            probe_rows[index] = Some(candidate());
        }
    }

    let batch_target = 700;
    let distance = estimated_tokens.abs_diff(batch_target);
    let should_add = probe_batch.len() < 8
        || probe_batch
            .iter()
            .map(|current| current.estimated_tokens.abs_diff(batch_target))
            .max()
            .is_some_and(|worst| distance < worst);
    if should_add {
        probe_batch.push(candidate());
        probe_batch.sort_by_key(|current| current.estimated_tokens.abs_diff(batch_target));
        probe_batch.truncate(8);
    }
}

fn run_production_probes(
    probe_rows: &[ProbeCandidate],
    probe_batch: &[ProbeCandidate],
) -> Vec<ProbeResult> {
    assert_eq!(probe_rows.len(), PROBE_TARGETS.len());
    assert_eq!(probe_batch.len(), 8);
    let mut results = probe_rows
        .iter()
        .zip(PROBE_TARGETS)
        .map(|(candidate, target)| {
            curl_probe(
                format!("~{target}-token row"),
                std::slice::from_ref(candidate),
            )
        })
        .collect::<Vec<_>>();
    results.push(curl_probe("8 × ~700-token rows".to_string(), probe_batch));
    results
}

fn curl_probe(label: String, candidates: &[ProbeCandidate]) -> ProbeResult {
    let payload = serde_json::json!({
        "model": PRODUCTION_MODEL,
        "input": candidates.iter().map(|candidate| &candidate.text).collect::<Vec<_>>(),
    });
    let mut child = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--connect-timeout",
            "5",
            "--max-time",
            "180",
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            "--write-out",
            "\n__AFT_CURL_METRICS__%{http_code}\t%{time_total}",
            PRODUCTION_EMBEDDINGS_URL,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch curl for production embedding probe");
    child
        .stdin
        .take()
        .expect("curl stdin")
        .write_all(payload.to_string().as_bytes())
        .expect("write curl embedding payload");
    let output = child.wait_with_output().expect("wait for curl probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, metrics) = stdout
        .rsplit_once("\n__AFT_CURL_METRICS__")
        .unwrap_or((stdout.as_ref(), "000\t0"));
    let mut metrics = metrics.trim().split('\t');
    let status = metrics.next().unwrap_or("000").to_string();
    let latency_ms = metrics
        .next()
        .and_then(|seconds| seconds.parse::<f64>().ok())
        .map(|seconds| format!("{:.1}", seconds * 1000.0))
        .unwrap_or_else(|| "unknown".to_string());
    let response: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let dimensions = response
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|data| {
            data.iter()
                .map(|row| {
                    row.get("embedding")
                        .and_then(serde_json::Value::as_array)
                        .map_or(0, Vec::len)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let full_dimension = dimensions.len() == candidates.len()
        && dimensions
            .iter()
            .all(|dimension| *dimension == EXPECTED_QWEN_DIMENSION);
    let vectors = if dimensions.is_empty() {
        "none".to_string()
    } else if dimensions
        .iter()
        .all(|dimension| *dimension == dimensions[0])
    {
        format!("{} × {}", dimensions.len(), dimensions[0])
    } else {
        format!("{dimensions:?}")
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error_body = if status.starts_with('2') && full_dimension && output.status.success() {
        String::new()
    } else {
        truncate_report_cell(&format!("{body} {stderr}"), 500)
    };
    let chars = range_label(candidates.iter().map(|candidate| candidate.chars));
    let estimated_tokens = range_label(
        candidates
            .iter()
            .map(|candidate| candidate.estimated_tokens),
    );

    eprintln!(
        "census: production probe {label}: HTTP {status}, {latency_ms} ms, vectors {vectors}"
    );
    ProbeResult {
        label,
        sources: candidates
            .iter()
            .map(|candidate| candidate.source.as_str())
            .collect::<Vec<_>>()
            .join("<br>"),
        chars,
        estimated_tokens,
        status,
        latency_ms,
        vectors,
        full_dimension,
        error_body,
    }
}

fn range_label(values: impl IntoIterator<Item = usize>) -> String {
    let values = values.into_iter().collect::<Vec<_>>();
    let min = values.iter().min().copied().unwrap_or(0);
    let max = values.iter().max().copied().unwrap_or(0);
    if min == max {
        min.to_string()
    } else {
        format!("{min}–{max}")
    }
}

fn truncate_report_cell(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .take(max_chars)
        .collect::<String>()
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

fn token_count(tokenizer: &Tokenizer, text: &str) -> u32 {
    tokenizer
        .encode(text, true)
        .expect("ModernBERT tokenizer rejected chunk text")
        .len()
        .try_into()
        .expect("one semantic chunk exceeded u32::MAX tokens")
}

fn group_stats<'a>(rows: impl IntoIterator<Item = &'a Row>) -> GroupStats {
    let mut stats = GroupStats::default();
    let mut uncapped = Vec::new();

    for row in rows {
        stats.rows += 1;
        stats.today_tokens += row.today_tokens as u64;
        stats.uncapped_tokens += row.uncapped_tokens as u64;
        uncapped.push(row.uncapped_tokens as usize);
        for (index, threshold) in THRESHOLDS.iter().enumerate() {
            stats.over[index] += usize::from(row.uncapped_tokens > *threshold as u32);
        }
        for (index, ceiling) in CEILINGS.iter().enumerate() {
            stats.ceiling_tokens[index] += row.uncapped_tokens.min(*ceiling as u32) as u64;
        }
    }

    uncapped.sort_unstable();
    stats.percentiles = [
        percentile(&uncapped, 50),
        percentile(&uncapped, 90),
        percentile(&uncapped, 99),
        uncapped.last().copied().unwrap_or(0),
    ];
    stats
}

fn percentile(sorted: &[usize], percentile: usize) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn share(count: usize, total: usize) -> String {
    if total == 0 {
        return "n/a".to_string();
    }
    format!("{:.3}%", count as f64 * 100.0 / total as f64)
}

fn first_99_ceiling(rows: &[Row]) -> Option<usize> {
    CEILINGS.iter().copied().find(|ceiling| {
        let covered = rows
            .iter()
            .filter(|row| row.uncapped_tokens <= *ceiling as u32)
            .count();
        covered.saturating_mul(100) >= rows.len().saturating_mul(99)
    })
}

fn cost_at(rows: &[Row], ceiling: usize) -> u64 {
    rows.iter()
        .map(|row| row.uncapped_tokens.min(ceiling as u32) as u64)
        .sum()
}

fn push_production_probe_section(report: &mut String, probes: &[ProbeResult]) {
    assert_eq!(probes.len(), PROBE_TARGETS.len() + 1);
    writeln!(report, "## Production lane's real ceiling").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "The production lane is the OpenAI-compatible Bionic server at `{PRODUCTION_EMBEDDINGS_URL}` (formerly LM Studio), model `{PRODUCTION_MODEL}`. AFT's 400-signature-char / 15-body-line / 300-body-byte / 1,600-total-char guards were written for that server's former llama.cpp 512-token physical-batch limit; the Qwen3-Embedding-0.6B model itself advertises a 32k context. These read-only probes called `curl POST /v1/embeddings` directly, without AFT. Inputs are uncapped embed texts from real functions in the AFT corpus. The exact Qwen3-Embedding-0.6B `tokenizer.json` was absent from the local Hugging Face and fastembed caches, so the token column is explicitly estimated as `characters / 3.5`."
    )
    .unwrap();
    writeln!(report).unwrap();
    writeln!(report, "| Probe | Real function source(s) | Characters | Qwen tokens (estimated) | HTTP | Error body | Latency ms | Returned vectors | Full 1024 dimensions? |").unwrap();
    writeln!(report, "|---|---|---:|---:|---:|---|---:|---:|---|").unwrap();
    for probe in probes {
        writeln!(
            report,
            "| {} | `{}` | {} | {} | {} | {} | {} | {} | {} |",
            probe.label,
            probe.sources,
            probe.chars,
            probe.estimated_tokens,
            probe.status,
            if probe.error_body.is_empty() {
                "—"
            } else {
                probe.error_body.as_str()
            },
            probe.latency_ms,
            probe.vectors,
            if probe.full_dimension { "yes" } else { "no" },
        )
        .unwrap();
    }
    writeln!(report).unwrap();

    let singles = &probes[..PROBE_TARGETS.len()];
    let largest_success = singles
        .iter()
        .zip(PROBE_TARGETS)
        .filter(|(probe, _)| probe.status.starts_with('2') && probe.full_dimension)
        .map(|(_, target)| target)
        .max();
    let first_failure = singles
        .iter()
        .zip(PROBE_TARGETS)
        .find(|(probe, _)| !probe.status.starts_with('2') || !probe.full_dimension)
        .map(|(_, target)| target);
    let ceiling = match (largest_success, first_failure) {
        (Some(success), None) => format!(
            "at least ~{success} estimated Qwen tokens per row (the largest probe succeeded)"
        ),
        (Some(success), Some(failure)) => {
            format!("between ~{success} and ~{failure} estimated Qwen tokens per row")
        }
        (None, Some(failure)) => format!("below ~{failure} estimated Qwen tokens per row"),
        (None, None) => "undetermined".to_string(),
    };
    let largest_passed = singles
        .last()
        .is_some_and(|probe| probe.status.starts_with('2') && probe.full_dimension);
    let batch = probes.last().expect("batch probe");
    let batch_passed = batch.status.starts_with('2') && batch.full_dimension;
    writeln!(
        report,
        "Conclusion: the server's effective per-row ceiling today is {ceiling}; lifting AFT's caps would {} on the CURRENT lane without any backend change{}.",
        if largest_passed { "work through the measured ~4,200-token row" } else { "not cover every measured probe" },
        if batch_passed {
            ", and the successful 8 × ~700-token batch shows there is no legacy 512-token aggregate batch limit"
        } else {
            ", while the 8 × ~700-token batch did not return eight full-dimension vectors"
        }
    )
    .unwrap();
    writeln!(report).unwrap();
}

fn render_report(
    home: &Path,
    tokenizer: &TokenizerInfo,
    results: &[CorpusResult],
    missing: &[(&'static str, PathBuf)],
    probe_results: &[ProbeResult],
) -> String {
    let pooled = results
        .iter()
        .flat_map(|result| result.rows.iter().copied())
        .collect::<Vec<_>>();
    let mut report = String::new();
    writeln!(report, "# Semantic chunk token census (2026-09)").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "Generated 2026-09-12 by the ignored `semantic_chunk_census` integration test. Token counts include the tokenizer's special tokens."
    )
    .unwrap();
    writeln!(report).unwrap();
    push_production_probe_section(&mut report, probe_results);

    writeln!(report, "## Method").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "The census used `callgraph::walk_project_files` followed by `is_semantic_indexed_extension`, matching the semantic snapshot's gitignore/global-ignore/`.aftignore` filtering and supported semantic extensions. Files over `MAX_SEMANTIC_FILE_BYTES` (4 MiB) were left at zero chunks, as in production. Each eligible file was parsed once by the production tree-sitter parser and converted twice by the production semantic chunker: today's default caps (signature 400 chars, body 15 lines / 300 bytes, total 1,600 chars) and `ChunkCaps` with all four values set to `usize::MAX`. Large corpora were processed as non-overlapping file-range shards and the cached row records were checked for contiguous, gap-free coverage before aggregation; this bounds parser/tokenizer memory without changing the sorted file set."
    )
    .unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "Tokenizer: `{MODEL_REPO}` revision `{}`; `tokenizer.json` SHA-256 `{}`; source `{}` ({}). Truncation was disabled before encoding.",
        tokenizer.revision,
        tokenizer.sha256,
        display_home_relative(home, &tokenizer.path),
        if tokenizer.downloaded {
            "downloaded because no local copy was present"
        } else {
            "loaded from the local cache"
        }
    )
    .unwrap();
    writeln!(report).unwrap();

    writeln!(report, "## Input snapshots").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Corpus | Root | Git HEAD | State | Semantic files | >4 MiB | Read/parse errors |"
    )
    .unwrap();
    writeln!(report, "|---|---|---:|---|---:|---:|---:|").unwrap();
    for result in results {
        writeln!(
            report,
            "| {} | `{}` | `{}` | {} | {} | {} | {} |",
            result.name,
            display_home_relative(home, &result.root),
            result.revision,
            if result.dirty { "dirty" } else { "clean" },
            result.files,
            result.oversized_files,
            result.errors.len()
        )
        .unwrap();
    }
    if missing.is_empty() {
        writeln!(report, "\nMissing corpora: none.").unwrap();
    } else {
        writeln!(report, "\nMissing corpora:").unwrap();
        for (name, path) in missing {
            writeln!(
                report,
                "- `{name}`: `{}`",
                display_home_relative(home, path)
            )
            .unwrap();
        }
    }
    writeln!(report).unwrap();

    writeln!(report, "## Uncapped token distribution").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Corpus | Rows | Total tokens | p50 | p90 | p99 | Max | >512 | >1024 | >2048 | >4096 |"
    )
    .unwrap();
    writeln!(
        report,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    )
    .unwrap();
    for result in results {
        push_distribution_row(&mut report, &result.name, &result.rows);
    }
    push_distribution_row(&mut report, "**pooled**", &pooled);
    writeln!(report).unwrap();

    writeln!(report, "## Rotation and candidate-ceiling cost").unwrap();
    writeln!(report).unwrap();
    writeln!(report, "Tokens at a candidate ceiling are `sum(min(uncapped row tokens, ceiling))`; row count is unchanged.").unwrap();
    writeln!(report).unwrap();
    writeln!(report, "| Corpus | Today's rows | Today's tokens | Uncapped tokens | At 1024 | At 2048 | At 4096 | At 8192 |").unwrap();
    writeln!(report, "|---|---:|---:|---:|---:|---:|---:|---:|").unwrap();
    for result in results {
        push_cost_row(&mut report, &result.name, &result.rows);
    }
    push_cost_row(&mut report, "**pooled**", &pooled);
    writeln!(report).unwrap();

    writeln!(report, "## Threshold shares by chunk kind").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Corpus | Kind | Rows | >512 | >1024 | >2048 | >4096 |"
    )
    .unwrap();
    writeln!(report, "|---|---|---:|---:|---:|---:|---:|").unwrap();
    for result in results {
        push_kind_rows(&mut report, &result.name, &result.rows);
    }
    push_kind_rows(&mut report, "**pooled**", &pooled);
    writeln!(report).unwrap();

    writeln!(report, "## Threshold shares by language").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Corpus | Language | Rows | >512 | >1024 | >2048 | >4096 |"
    )
    .unwrap();
    writeln!(report, "|---|---|---:|---:|---:|---:|---:|").unwrap();
    for result in results {
        push_language_rows(&mut report, &result.name, &result.rows);
    }
    push_language_rows(&mut report, "**pooled**", &pooled);
    writeln!(report).unwrap();

    writeln!(report, "## Today's body-line truncation").unwrap();
    writeln!(report).unwrap();
    writeln!(
        report,
        "| Corpus | Symbol rows | Body exceeded 15 lines | Share cut |"
    )
    .unwrap();
    writeln!(report, "|---|---:|---:|---:|").unwrap();
    for result in results {
        push_body_cut_row(&mut report, &result.name, &result.rows);
    }
    push_body_cut_row(&mut report, "**pooled**", &pooled);
    writeln!(report).unwrap();

    writeln!(report, "## Decision input").unwrap();
    writeln!(report).unwrap();
    let coverage = results
        .iter()
        .map(|result| match first_99_ceiling(&result.rows) {
            Some(ceiling) => format!("{} needs {}", result.name, ceiling),
            None => format!("{} needs >8192", result.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let fleet_cost = results
        .iter()
        .filter(|result| matches!(result.name.as_str(), "aft" | "magic-context" | "opencode"))
        .map(|result| match first_99_ceiling(&result.rows) {
            Some(ceiling) => format!(
                "{}: {} rows / {} tokens at {}",
                result.name,
                result.rows.len(),
                cost_at(&result.rows, ceiling),
                ceiling
            ),
            None => format!(
                "{}: {} rows / {} uncapped tokens (>8192 needed)",
                result.name,
                result.rows.len(),
                group_stats(result.rows.iter()).uncapped_tokens
            ),
        })
        .collect::<Vec<_>>()
        .join("; ");
    writeln!(
        report,
        "The smallest rotated ModernBERT ceiling covering at least 99% of rows in each corpus is: {coverage}. For the three fleet corpora, a full re-embed at each corpus's selected ceiling costs {fleet_cost}."
    )
    .unwrap();

    let errors = results
        .iter()
        .flat_map(|result| {
            result
                .errors
                .iter()
                .map(move |(path, error)| (result.name.as_str(), path, error))
        })
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        writeln!(report).unwrap();
        writeln!(report, "## Read/parse errors").unwrap();
        writeln!(report).unwrap();
        writeln!(
            report,
            "The production collector skips these files; the census did the same."
        )
        .unwrap();
        for (name, path, error) in errors {
            writeln!(report, "- `{name}` `{}`: {error}", path.display()).unwrap();
        }
    }

    report
}

fn push_distribution_row(report: &mut String, name: &str, rows: &[Row]) {
    let stats = group_stats(rows.iter());
    writeln!(
        report,
        "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
        stats.rows,
        stats.uncapped_tokens,
        stats.percentiles[0],
        stats.percentiles[1],
        stats.percentiles[2],
        stats.percentiles[3],
        share(stats.over[0], stats.rows),
        share(stats.over[1], stats.rows),
        share(stats.over[2], stats.rows),
        share(stats.over[3], stats.rows),
    )
    .unwrap();
}

fn push_cost_row(report: &mut String, name: &str, rows: &[Row]) {
    let stats = group_stats(rows.iter());
    writeln!(
        report,
        "| {name} | {} | {} | {} | {} | {} | {} | {} |",
        stats.rows,
        stats.today_tokens,
        stats.uncapped_tokens,
        stats.ceiling_tokens[0],
        stats.ceiling_tokens[1],
        stats.ceiling_tokens[2],
        stats.ceiling_tokens[3],
    )
    .unwrap();
}

fn push_kind_rows(report: &mut String, name: &str, rows: &[Row]) {
    for kind in [RowKind::Symbol, RowKind::FileSummary] {
        let stats = group_stats(rows.iter().filter(|row| row.kind == kind));
        writeln!(
            report,
            "| {name} | {} | {} | {} | {} | {} | {} |",
            kind.label(),
            stats.rows,
            share(stats.over[0], stats.rows),
            share(stats.over[1], stats.rows),
            share(stats.over[2], stats.rows),
            share(stats.over[3], stats.rows),
        )
        .unwrap();
    }
}

fn push_language_rows(report: &mut String, name: &str, rows: &[Row]) {
    let mut by_language: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in rows {
        by_language
            .entry(language_label(row.language))
            .or_default()
            .push(row);
    }
    for (language, language_rows) in by_language {
        let stats = group_stats(language_rows);
        writeln!(
            report,
            "| {name} | {language} | {} | {} | {} | {} | {} |",
            stats.rows,
            share(stats.over[0], stats.rows),
            share(stats.over[1], stats.rows),
            share(stats.over[2], stats.rows),
            share(stats.over[3], stats.rows),
        )
        .unwrap();
    }
}

fn push_body_cut_row(report: &mut String, name: &str, rows: &[Row]) {
    let symbols = rows
        .iter()
        .filter(|row| row.kind == RowKind::Symbol)
        .collect::<Vec<_>>();
    let cut = symbols.iter().filter(|row| row.body_cut_today).count();
    writeln!(
        report,
        "| {name} | {} | {cut} | {} |",
        symbols.len(),
        share(cut, symbols.len())
    )
    .unwrap();
}

fn load_tokenizer(home: &Path) -> (Tokenizer, TokenizerInfo) {
    let repo_dir_name = "models--Alibaba-NLP--gte-modernbert-base";
    let candidates = [
        env::var_os("HUGGINGFACE_HUB_CACHE").map(PathBuf::from),
        env::var_os("HF_HOME").map(|path| PathBuf::from(path).join("hub")),
        Some(home.join(".cache/huggingface/hub")),
        Some(home.join(".cache/fastembed")),
    ];
    let mut tokenizer_path = candidates
        .into_iter()
        .flatten()
        .flat_map(|base| tokenizer_candidates(&base.join(repo_dir_name)))
        .next();
    let downloaded = tokenizer_path.is_none()
        || env::var("AFT_SEMANTIC_CENSUS_TOKENIZER_WAS_DOWNLOADED").as_deref() == Ok("1");

    if tokenizer_path.is_none() {
        use hf_hub::api::sync::ApiBuilder;

        let api = ApiBuilder::new()
            .with_progress(false)
            .build()
            .expect("initialize Hugging Face API");
        tokenizer_path = Some(
            api.model(MODEL_REPO.to_string())
                .get(TOKENIZER_FILE)
                .expect("download ModernBERT tokenizer.json"),
        );
    }

    let path = tokenizer_path.expect("ModernBERT tokenizer path");
    let bytes = fs::read(&path).expect("read ModernBERT tokenizer.json");
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let revision = snapshot_revision(&path).unwrap_or_else(|| "unknown".to_string());
    let mut tokenizer = Tokenizer::from_file(&path).expect("load ModernBERT tokenizer.json");
    tokenizer
        .with_truncation(None)
        .expect("disable tokenizer truncation");
    tokenizer.with_padding(None);

    (
        tokenizer,
        TokenizerInfo {
            path,
            revision,
            sha256,
            downloaded,
        },
    )
}

fn tokenizer_candidates(repo_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![repo_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(TOKENIZER_FILE) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn snapshot_revision(path: &Path) -> Option<String> {
    let components = path.components().collect::<Vec<_>>();
    components.windows(2).find_map(|pair| {
        (pair[0].as_os_str() == "snapshots")
            .then(|| pair[1].as_os_str().to_string_lossy().to_string())
    })
}

fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn display_home_relative(home: &Path, path: &Path) -> String {
    path.strip_prefix(home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn language_code(language: LangId) -> u8 {
    language as u8
}

fn language_label(language: u8) -> &'static str {
    match language {
        0 => "TypeScript",
        1 => "TSX",
        2 => "JavaScript",
        3 => "Python",
        4 => "Rust",
        5 => "Go",
        6 => "C",
        7 => "C++",
        8 => "CUDA",
        9 => "Metal",
        10 => "Zig",
        11 => "C#",
        12 => "Bash",
        13 => "HTML",
        14 => "Markdown",
        15 => "Solidity",
        16 => "SCSS",
        17 => "Vue",
        18 => "JSON",
        19 => "Scala",
        20 => "Java",
        21 => "Ruby",
        22 => "Kotlin",
        23 => "Swift",
        24 => "PHP",
        25 => "Lua",
        26 => "Perl",
        27 => "YAML",
        28 => "Pascal",
        29 => "R",
        30 => "Groovy",
        31 => "Objective-C",
        32 => "TOML",
        other => panic!("unknown cached language code {other}"),
    }
}
