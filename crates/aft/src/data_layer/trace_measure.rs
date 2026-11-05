//! Measures `trace_to` and `impact` on a real checkout: latency, reply size,
//! and how many paths each answer returns or cuts. It builds the checkout's
//! call graph the way the data layer stores it (content records and
//! fragments in a segment, read back as slices), samples function and method
//! symbols evenly in path order, and runs both commands' result builders on
//! each sample twice: once on a fresh reader (slices assembled on first
//! read) and once more on the same reader. Runs from
//! `examples/trace_measure.rs`.
//!
//! The report reads the answers as JSON fields, so it measures answers of
//! any shape and the same sample compares two builds of AFT.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::ids::ContentKey;
use aft_store::scan::{scan, ContentKind, ContentSink};
use serde_json::{json, Value};

use super::callgraph::{resolve_checkout, CheckoutInputs};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use crate::callgraph_store::join::CallgraphBlob;

#[derive(Default)]
struct Records {
    by_key: Mutex<HashMap<RecordKey, Arc<ContentRecord>>>,
    by_path: Mutex<Vec<(String, RecordKey)>>,
    failures: Mutex<Vec<String>>,
}

impl ContentSink for Records {
    fn read(&self, path: &[u8], content: ContentKey, kind: ContentKind, bytes: &[u8]) {
        if kind == ContentKind::Binary {
            return;
        }
        let Ok(path) = std::str::from_utf8(path) else {
            return;
        };
        let Some(language) = language_for(path, bytes) else {
            return;
        };
        let key = RecordKey::new(content, &language, EXTRACTOR_VERSION);
        let known = self
            .by_key
            .lock()
            .expect("no panics while holding it")
            .contains_key(&key);
        if !known {
            match extract(bytes, &language) {
                Ok(record) => {
                    self.by_key
                        .lock()
                        .expect("no panics while holding it")
                        .insert(key, Arc::new(record));
                }
                Err(error) => {
                    self.failures
                        .lock()
                        .expect("no panics while holding it")
                        .push(format!("{path}: {error}"));
                    return;
                }
            }
        }
        self.by_path
            .lock()
            .expect("no panics while holding it")
            .push((path.to_string(), key));
    }
}

/// One command's answers over the sample.
#[derive(Default)]
struct Samples {
    cold_ms: Vec<f64>,
    warm_ms: Vec<f64>,
    json_bytes: Vec<usize>,
    text_bytes: Vec<usize>,
    errors: usize,
    paths_returned: usize,
    total_paths: usize,
    truncated_paths: usize,
    depth_limited: usize,
    lower_bound: usize,
    callers_listed: usize,
    roots_labelled: usize,
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

fn sizes(values: &[usize]) -> Value {
    let floats = values.iter().map(|value| *value as f64).collect::<Vec<_>>();
    json!({
        "total": values.iter().sum::<usize>(),
        "p50": percentile(&floats, 0.5),
        "p95": percentile(&floats, 0.95),
        "max": floats.iter().copied().fold(0.0, f64::max),
    })
}

fn latencies(values: &[f64]) -> Value {
    json!({
        "p50_ms": percentile(values, 0.5),
        "p95_ms": percentile(values, 0.95),
        "max_ms": values.iter().copied().fold(0.0, f64::max),
    })
}

impl Samples {
    fn report(&self) -> Value {
        json!({
            "answers": self.json_bytes.len(),
            "errors": self.errors,
            "cold": latencies(&self.cold_ms),
            "warm": latencies(&self.warm_ms),
            "json_bytes": sizes(&self.json_bytes),
            "text_bytes": sizes(&self.text_bytes),
            "paths_returned": self.paths_returned,
            "total_paths": self.total_paths,
            "truncated_paths": self.truncated_paths,
            "depth_limited_answers": self.depth_limited,
            "lower_bound_answers": self.lower_bound,
            "callers_listed": self.callers_listed,
            "roots_labelled": self.roots_labelled,
        })
    }

    fn record(&mut self, op: &str, value: &Value) {
        let text = crate::subc_format::format_callgraph(op, value, false);
        self.json_bytes
            .push(serde_json::to_vec(value).map_or(0, |bytes| bytes.len()));
        self.text_bytes.push(text.len());
        let number = |field: &str| value.get(field).and_then(Value::as_u64).unwrap_or(0) as usize;
        let flag = |field: &str| value.get(field).and_then(Value::as_bool) == Some(true);
        let paths = value
            .get("paths")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        self.paths_returned += paths.len();
        self.total_paths += number("total_paths");
        self.truncated_paths += number("truncated_paths");
        self.depth_limited += usize::from(flag("max_depth_reached") || flag("depth_limited"));
        self.lower_bound += usize::from(flag("total_paths_is_lower_bound"));
        let callers = value
            .get("callers")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        self.callers_listed += callers.len();
        // A labelled root: a path's or caller's `root`, or, in answers from
        // before roots had kinds, the entry-point flag on a path's first hop
        // or on a caller.
        self.roots_labelled += paths
            .iter()
            .filter(|path| {
                path.get("root").is_some()
                    || path
                        .get("hops")
                        .and_then(Value::as_array)
                        .and_then(|hops| hops.first())
                        .and_then(|hop| hop.get("is_entry_point"))
                        .and_then(Value::as_bool)
                        == Some(true)
            })
            .count();
        self.roots_labelled += callers
            .iter()
            .filter(|caller| {
                caller.get("root").is_some()
                    || caller.get("is_entry_point").and_then(Value::as_bool) == Some(true)
            })
            .count();
    }
}

/// A checkout's call graph as the data layer builds it: every file scanned
/// and extracted to a content record, and every parsed file resolved.
struct Built {
    inputs: CheckoutInputs,
    resolved: BTreeMap<String, super::callgraph::FileResolution>,
    records: HashMap<RecordKey, Arc<ContentRecord>>,
}

fn build(root: &Path, threads: usize) -> Result<Built, String> {
    let records = Records::default();
    let scanned = scan(root, &super::content::scan_policy(), None, &records)
        .map_err(|error| error.to_string())?;
    let failures = records.failures.into_inner().expect("no panics");
    if !failures.is_empty() {
        return Err(format!("extraction failed: {failures:?}"));
    }
    let by_key = records.by_key.into_inner().expect("no panics");
    let by_path = records.by_path.into_inner().expect("no panics");
    let mut inputs = CheckoutInputs {
        state: scanned.state,
        parses: super::callgraph::Parses::default(),
        configs: HashMap::new(),
    };
    for (path, key) in by_path {
        let full_key = key.digest().to_string();
        let record = &by_key[&key];
        match &record.data {
            RecordBody::Parsed { .. } => {
                let CallgraphBlob::Parse(parse) =
                    blob_from_record(record).map_err(|error| error.to_string())?
                else {
                    unreachable!("a parsed record converts to a parse blob");
                };
                inputs.parses.insert(path, (full_key, Arc::new(parse)));
            }
            RecordBody::Config { source } => {
                inputs
                    .configs
                    .insert(path.into_bytes(), Arc::from(source.as_slice()));
            }
        }
    }
    let resolved = resolve_checkout(&inputs, threads).map_err(|error| error.to_string())?;
    Ok(Built {
        inputs,
        resolved,
        records: by_key,
    })
}

/// The data layer's call graph of the checkout at `root`, held in memory,
/// for tests of the commands built on it.
#[cfg(test)]
pub(crate) fn memory_reader(root: &Path) -> super::query::SliceReader<super::query::MemorySlices> {
    let built = build(root, 2).unwrap_or_else(|error| panic!("build {}: {error}", root.display()));
    let mut slices = super::query::MemorySlices::default();
    for (path, file) in &built.resolved {
        let (key, parse) = &built.inputs.parses[path];
        slices.insert(super::slice::FileSlice::build(path, key, parse, file));
    }
    super::query::SliceReader::new(slices, root.to_path_buf(), "test".to_string())
}

/// Builds the call graph of the checkout at `root` and runs `trace_to`
/// (depth 10, tests left out) and `impact` (depth 5, tests left out) on
/// `sample` function and method symbols spread evenly over it. Returns the
/// measurements as JSON.
///
/// # Errors
///
/// When the checkout cannot be scanned, extracted or resolved, or the
/// segment cannot be written or read.
pub fn run(root: &Path, threads: usize, sample: usize) -> Result<Value, String> {
    use aft_store::cache::BlockCache;
    use aft_store::ids::{FamilyId, SegmentId, WriterIncarnation};
    use aft_store::segment::{Segment, SegmentMeta};

    let started = Instant::now();
    let Built {
        inputs,
        resolved,
        records: by_key,
    } = build(root, threads)?;
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;

    let records = by_key
        .iter()
        .map(|(key, record)| (key.digest().to_string(), (*key, Arc::clone(record))))
        .collect::<HashMap<_, _>>();
    let directory = std::env::temp_dir().join(format!("aft-trace-measure-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let segment_path = directory.join("1.seg");
    let family = FamilyId::from_digest(aft_store::ids::Digest128::of(b"trace measure"));
    let fragments = super::graph_store::write_segment(
        &segment_path,
        SegmentMeta {
            family,
            segment: SegmentId::new(1),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 0,
        },
        &inputs,
        &resolved,
        &records,
    )?;
    let segment = Arc::new(
        Segment::open(
            &segment_path,
            family,
            &super::graph_store::known_sections(),
            Arc::new(BlockCache::new(256 << 20)),
        )
        .map_err(|error| error.to_string())?,
    );
    let open = || -> Result<_, String> {
        let stored = super::graph_store::StoredSlices::open(
            vec![Arc::clone(&segment)],
            &inputs,
            &fragments,
        )?;
        Ok(super::query::SliceReader::new(
            stored,
            root.to_path_buf(),
            "trace-measure".to_string(),
        ))
    };

    // Every function and method, once per file and scoped name, in path
    // order; then an even spread of `sample` of them.
    let mut symbols = Vec::new();
    for (path, (_, parse)) in inputs.parses.iter() {
        let mut names = parse
            .symbols
            .iter()
            .filter(|symbol| matches!(symbol.kind.as_str(), "function" | "method"))
            .map(|symbol| symbol.scoped_name.clone())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        symbols.extend(names.into_iter().map(|name| (path.clone(), name)));
    }
    let step = (symbols.len() / sample.max(1)).max(1);
    let chosen = symbols
        .iter()
        .step_by(step)
        .take(sample)
        .cloned()
        .collect::<Vec<_>>();

    use crate::commands::callgraph_store_adapter as adapter;
    let mut by_op = BTreeMap::<&str, Samples>::new();
    for pass in ["cold", "warm"] {
        let reader = open()?;
        for (file, symbol) in &chosen {
            let path = Path::new(file);
            for op in ["trace_to", "impact"] {
                let started = Instant::now();
                let value = if op == "trace_to" {
                    adapter::trace_to_result(&reader, path, symbol, 10, false).map(|result| {
                        let envelope = crate::commands::trace_to::trace::build_trace_to_envelope(
                            result.paths.len(),
                            result.total_paths,
                            result.max_depth_reached,
                            result.total_paths_is_lower_bound,
                        );
                        let mut value = serde_json::to_value(&result).unwrap_or(Value::Null);
                        crate::commands::trace_to::trace::attach_trace_to_envelope(
                            &mut value,
                            envelope.as_ref(),
                        );
                        value
                    })
                } else {
                    adapter::impact_result(&reader, path, symbol, 5, false)
                        .map(|result| serde_json::to_value(&result).unwrap_or(Value::Null))
                };
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let samples = by_op.entry(op).or_default();
                match value {
                    Ok(value) => {
                        if pass == "cold" {
                            samples.cold_ms.push(elapsed);
                            samples.record(op, &value);
                        } else {
                            samples.warm_ms.push(elapsed);
                        }
                    }
                    Err(_) if pass == "cold" => samples.errors += 1,
                    Err(_) => {}
                }
            }
        }
    }
    drop(segment);
    let _ = std::fs::remove_dir_all(&directory);
    Ok(json!({
        "parsed_files": inputs.parses.len(),
        "functions_and_methods": symbols.len(),
        "sampled": chosen.len(),
        "build_ms": build_ms,
        "ops": by_op
            .iter()
            .map(|(op, samples)| ((*op).to_string(), samples.report()))
            .collect::<serde_json::Map<_, _>>(),
    }))
}
