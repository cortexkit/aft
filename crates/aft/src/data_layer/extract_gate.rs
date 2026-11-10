//! Checks the single-walk call-graph extractor against the extractor it
//! replaces, and incremental reparsing against a full parse, on real
//! repositories. Runs from `examples/extract_gate.rs`; it goes away with the
//! replaced extractor.
//!
//! Every file must give the same parse output from both extractors, field
//! for field and in the same order, apart from the temporary syntax-tree
//! index (`ParseBlob::ast_nodes`) that no reader uses after extraction.
//! Every edit of an edit pack must give the same content record from an
//! incremental reparse of the previous tree as from parsing the new text.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use rayon::prelude::*;

use super::content::{
    extract, extract_parsed, extract_with_tree, language_for, reparse, SyntaxTree,
};
use crate::callgraph_store::join::{CallgraphBlob, ParseBlob};

/// The extractor version both extractors are given in the gate; equality
/// does not depend on it.
const GATE_VERSION: &str = "extract-gate";

/// Source files larger than this are not read by a checkout scan, so
/// neither extractor ever sees them (the scan policy's read limit).
const MAX_FILE_LEN: u64 = 4 * 1024 * 1024;

/// One parsed file of a repository.
pub struct CorpusFile {
    pub path: String,
    pub language: String,
    pub text: String,
}

/// Reads the files of `root` named one per line in `list`, keeping those the
/// call graph parses (configuration inputs are stored raw, not extracted).
///
/// # Errors
///
/// When `list` cannot be read.
pub fn load(root: &Path, list: &Path) -> std::io::Result<Vec<CorpusFile>> {
    let mut files = Vec::new();
    for path in std::fs::read_to_string(list)?.lines() {
        let full = root.join(path);
        let Ok(metadata) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_FILE_LEN {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        if Path::new(path).extension().is_some_and(|ext| ext == "h") {
            // A header's language is chosen by parsing it, and the data
            // layer reads that choice from the parse cache this fills.
            let _ = crate::parser::detect_language_from_source(Path::new(path), &text);
        }
        let Some(language) = language_for(path, text.as_bytes()) else {
            continue;
        };
        // The gate compares call-graph extractors: configuration is read
        // raw, and Markdown files, minified bundles and headers no grammar
        // resolves get records of their TODO markers alone, never a parse.
        if !crate::data_layer::content::is_parsed_language(&language) {
            continue;
        }
        files.push(CorpusFile {
            path: path.to_string(),
            language,
            text,
        });
    }
    Ok(files)
}

/// Which extractor to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Extractor {
    /// The extractor the single walk replaces.
    Reference,
    /// The single-walk extractor.
    SingleWalk,
}

fn run_extractor(
    extractor: Extractor,
    file: &CorpusFile,
) -> Result<CallgraphBlob, crate::callgraph_store::join::ManifestJoinError> {
    match extractor {
        Extractor::Reference => {
            CallgraphBlob::extract_reference(&file.text, &file.language, GATE_VERSION)
        }
        Extractor::SingleWalk => CallgraphBlob::extract(&file.text, &file.language, GATE_VERSION),
    }
}

/// The fields of two parse outputs that differ, ignoring the temporary
/// syntax-tree index.
#[must_use]
pub fn differing_fields(left: &ParseBlob, right: &ParseBlob) -> Vec<&'static str> {
    let mut fields = Vec::new();
    let mut check = |differs: bool, name: &'static str| {
        if differs {
            fields.push(name);
        }
    };
    check(
        left.entry_point_symbols != right.entry_point_symbols,
        "entry_point_symbols",
    );
    check(
        left.dispatch.go_package != right.dispatch.go_package,
        "dispatch.go_package",
    );
    check(
        left.dispatch.types != right.dispatch.types,
        "dispatch.types",
    );
    check(
        left.dispatch.methods != right.dispatch.methods,
        "dispatch.methods",
    );
    check(
        left.dispatch.sites != right.dispatch.sites,
        "dispatch.sites",
    );
    check(
        left.dispatch.fields != right.dispatch.fields,
        "dispatch.fields",
    );
    check(
        left.dispatch.returns != right.dispatch.returns,
        "dispatch.returns",
    );
    check(
        left.extractor_version != right.extractor_version,
        "extractor_version",
    );
    check(left.language != right.language, "language");
    check(left.symbols != right.symbols, "symbols");
    check(
        left.default_export_symbol != right.default_export_symbol,
        "default_export_symbol",
    );
    check(
        left.exported_symbols != right.exported_symbols,
        "exported_symbols",
    );
    check(
        left.callable_symbols != right.callable_symbols,
        "callable_symbols",
    );
    check(left.imports != right.imports, "imports");
    check(
        left.python_bindings != right.python_bindings,
        "python_bindings",
    );
    check(left.refs != right.refs, "refs");
    fields
}

/// What comparing both extractors on a repository found.
#[derive(Debug, Default, serde::Serialize)]
pub struct Comparison {
    pub files: usize,
    pub bytes: usize,
    pub mismatches: usize,
    /// Files on which exactly one extractor failed, or both failed with
    /// different errors.
    pub error_mismatches: usize,
    /// Files on which both extractors failed the same way.
    pub both_failed: usize,
    /// Up to twenty `path: fields` lines for a first look.
    pub examples: Vec<String>,
}

/// Runs both extractors on every file and compares their output.
#[must_use]
pub fn compare(files: &[CorpusFile], threads: usize) -> Comparison {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .expect("a thread pool builds");
    let examples = Mutex::new(Vec::new());
    let outcomes = pool.install(|| {
        files
            .par_iter()
            .map(|file| {
                let reference = run_extractor(Extractor::Reference, file);
                let single = run_extractor(Extractor::SingleWalk, file);
                let differs = match (&reference, &single) {
                    (Ok(CallgraphBlob::Parse(left)), Ok(CallgraphBlob::Parse(right))) => {
                        differing_fields(left, right)
                    }
                    (Err(left), Err(right)) if left == right => return (0, 0, 1),
                    (Err(_), _) | (_, Err(_)) => {
                        let mut examples = examples.lock().expect("no panics while holding it");
                        if examples.len() < 20 {
                            examples.push(format!(
                                "{}: {:?} / {:?}",
                                file.path,
                                reference.as_ref().err(),
                                single.as_ref().err()
                            ));
                        }
                        return (0, 1, 0);
                    }
                    _ => vec!["kind"],
                };
                if differs.is_empty() {
                    return (0, 0, 0);
                }
                let mut examples = examples.lock().expect("no panics while holding it");
                if examples.len() < 20 {
                    examples.push(format!("{}: {}", file.path, differs.join(", ")));
                }
                (1, 0, 0)
            })
            .collect::<Vec<_>>()
    });
    let mut comparison = Comparison {
        files: files.len(),
        bytes: files.iter().map(|file| file.text.len()).sum(),
        examples: examples.into_inner().expect("no panics while holding it"),
        ..Comparison::default()
    };
    for (mismatch, error_mismatch, both_failed) in outcomes {
        comparison.mismatches += mismatch;
        comparison.error_mismatches += error_mismatch;
        comparison.both_failed += both_failed;
    }
    comparison
}

/// CPU time of the calling thread, in nanoseconds. Falls back to elapsed
/// time where the platform has no per-thread clock.
fn thread_cpu_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `time` is a valid, writable timespec for the call.
        let status = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
        if status == 0 {
            return time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64;
        }
    }
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Extraction cost of one language.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LanguageCost {
    pub files: usize,
    pub bytes: usize,
    pub cpu_ms: f64,
    /// Microseconds of CPU per kilobyte of source.
    pub us_per_kb: f64,
}

/// Extraction cost of one repository with one extractor and thread count.
#[derive(Debug, Default, serde::Serialize)]
pub struct Timing {
    pub threads: usize,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub languages: BTreeMap<String, LanguageCost>,
}

/// Extracts every file once with `extractor` on `threads` threads and
/// reports the CPU time each extraction took, summed per language.
#[must_use]
pub fn measure(files: &[CorpusFile], extractor: Extractor, threads: usize) -> Timing {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .expect("a thread pool builds");
    let started = Instant::now();
    let costs = pool.install(|| {
        files
            .par_iter()
            .map(|file| {
                let before = thread_cpu_ns();
                let output = run_extractor(extractor, file);
                let cpu = thread_cpu_ns().saturating_sub(before);
                drop(output);
                (file.language.as_str(), file.text.len(), cpu)
            })
            .collect::<Vec<_>>()
    });
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut languages = BTreeMap::<String, LanguageCost>::new();
    for (language, bytes, cpu) in costs {
        let cost = languages.entry(language.to_string()).or_default();
        cost.files += 1;
        cost.bytes += bytes;
        cost.cpu_ms += cpu as f64 / 1e6;
    }
    for cost in languages.values_mut() {
        cost.us_per_kb = if cost.bytes == 0 {
            0.0
        } else {
            cost.cpu_ms * 1000.0 / (cost.bytes as f64 / 1024.0)
        };
    }
    Timing {
        threads,
        wall_ms,
        cpu_ms: languages.values().map(|cost| cost.cpu_ms).sum(),
        languages,
    }
}

/// Percentiles of per-edit times, in milliseconds.
#[derive(Debug, Default, serde::Serialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub total: f64,
}

impl Percentiles {
    fn of(mut values: Vec<f64>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_by(f64::total_cmp);
        let at = |fraction: f64| {
            let index = ((values.len() as f64 * fraction).ceil() as usize).max(1) - 1;
            values[index.min(values.len() - 1)]
        };
        Self {
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            max: values[values.len() - 1],
            total: values.iter().sum(),
        }
    }
}

/// What replaying an edit pack found.
#[derive(Debug, Default, serde::Serialize)]
pub struct Replay {
    pub base_files: usize,
    /// Edits that changed a file the call graph parses.
    pub edits: usize,
    /// Edits skipped because they delete a file or change one the call
    /// graph does not parse.
    pub skipped: usize,
    /// Edits whose incremental record differs from the full extraction's.
    pub mismatches: usize,
    /// Edits whose incremental reparse reused the previous tree (the rest
    /// had no previous tree in the same language and were parsed whole).
    pub reused_tree: usize,
    /// Full parse and extraction of the new text by the replaced extractor,
    /// per edit: the cost of an edit before this extractor.
    pub reference_ms: Percentiles,
    /// Full parse and extraction of the new text, per edit.
    pub full_ms: Percentiles,
    /// Incremental reparse and extraction of the new text, per edit.
    pub incremental_ms: Percentiles,
    /// The reparse alone, per edit that reused a previous tree.
    pub incremental_parse_ms: Percentiles,
    pub examples: Vec<String>,
}

/// Replays the edit pack at `pack`: a first line with every file's text,
/// then one edit line per diff hunk carrying the edited path's new text.
/// After each edit the record of an incremental reparse must equal a full
/// extraction of the new text.
///
/// # Errors
///
/// When the pack cannot be read or an extraction fails.
pub fn replay(pack: &Path) -> Result<Replay, String> {
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(pack).map_err(|error| error.to_string())?,
    ));
    let mut lines = reader.lines();
    let first: serde_json::Value = serde_json::from_str(
        &lines
            .next()
            .ok_or("empty pack")?
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut replay = Replay::default();
    // Each parsed file's text, language and the tree it parsed to.
    let mut files = BTreeMap::<String, (String, String, Option<SyntaxTree>)>::new();
    for (path, text) in first["files"].as_object().ok_or("no base files")? {
        let text = text.as_str().unwrap_or_default().to_string();
        let Some(language) = gate_language(path, &text) else {
            continue;
        };
        let (_, tree) =
            extract_with_tree(text.as_bytes(), &language).map_err(|e| format!("{path}: {e}"))?;
        files.insert(path.clone(), (text, language, tree));
    }
    replay.base_files = files.len();
    let mut reference_ms = Vec::new();
    let mut full_ms = Vec::new();
    let mut incremental_ms = Vec::new();
    let mut parse_ms = Vec::new();
    for line in lines {
        let line = line.map_err(|error| error.to_string())?;
        let edit: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| error.to_string())?;
        if edit["type"] != "edit" {
            continue;
        }
        let path = edit["path"]
            .as_str()
            .ok_or("edit without path")?
            .to_string();
        if edit["kind"] == "delete" {
            files.remove(&path);
            replay.skipped += 1;
            continue;
        }
        let text = edit["text"].as_str().unwrap_or_default().to_string();
        let Some(language) = gate_language(&path, &text) else {
            replay.skipped += 1;
            continue;
        };
        replay.edits += 1;
        let started = Instant::now();
        let reference = CallgraphBlob::extract_reference(&text, &language, GATE_VERSION)
            .map_err(|e| format!("{path}: {e}"))?;
        reference_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        drop(reference);
        let started = Instant::now();
        let full = extract(text.as_bytes(), &language).map_err(|e| format!("{path}: {e}"))?;
        full_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        let previous = files.remove(&path);
        let started = Instant::now();
        let (incremental, tree, parse) = match &previous {
            Some((old_text, old_language, Some(old_tree))) if *old_language == language => {
                replay.reused_tree += 1;
                let tree = reparse(old_text.as_bytes(), old_tree, text.as_bytes(), &language)
                    .map_err(|e| format!("{path}: {e}"))?;
                let parse = started.elapsed();
                let record = match &tree {
                    Some(tree) => extract_parsed(text.as_bytes(), &language, tree),
                    None => extract(text.as_bytes(), &language),
                }
                .map_err(|e| format!("{path}: {e}"))?;
                (record, tree, Some(parse))
            }
            _ => {
                let (record, tree) = extract_with_tree(text.as_bytes(), &language)
                    .map_err(|e| format!("{path}: {e}"))?;
                (record, tree, None)
            }
        };
        incremental_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        if let Some(parse) = parse {
            parse_ms.push(parse.as_secs_f64() * 1000.0);
        }
        if incremental != full {
            replay.mismatches += 1;
            if replay.examples.len() < 20 {
                replay.examples.push(format!(
                    "commit {} seq {} {path}",
                    edit["commit"], edit["seq"]
                ));
            }
        }
        files.insert(path, (text, language, tree));
    }
    replay.reference_ms = Percentiles::of(reference_ms);
    replay.full_ms = Percentiles::of(full_ms);
    replay.incremental_ms = Percentiles::of(incremental_ms);
    replay.incremental_parse_ms = Percentiles::of(parse_ms);
    Ok(replay)
}

/// The language a parsed file of the pack is extracted as, or `None` for a
/// file the call graph does not parse.
fn gate_language(path: &str, text: &str) -> Option<String> {
    if Path::new(path).extension().is_some_and(|ext| ext == "h") {
        let _ = crate::parser::detect_language_from_source(Path::new(path), text);
    }
    language_for(path, text.as_bytes())
        .filter(|language| crate::data_layer::content::is_parsed_language(language))
}
