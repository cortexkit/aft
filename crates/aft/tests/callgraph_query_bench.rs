use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aft::callgraph::walk_project_files;
use aft::callgraph_store::{CallGraphStore, StoredEdge};
use aft::commands::callgraph_store_adapter;
use aft::parser::SymbolCache;
use serde::Serialize;
use serde_json::{json, Value};

const RUNS: usize = 7;
const IMPACT_FILE: &str = "packages/aft-bridge/src/subc-transport.ts";
const IMPACT_SYMBOL: &str = "SubcTransportPool::lifecycleEnabled";
const DIFFERENTIAL_FILE: &str = "packages/opencode-plugin/src/config.ts";
const DIFFERENTIAL_SYMBOL: &str = "ensureRecordAtPath";
const REVERSE_BATCH_SIZE: usize = 499;

#[derive(Debug, Serialize)]
struct Timing {
    query: String,
    runs_ms: Vec<f64>,
    median_ms: f64,
    callers_rendered: usize,
    reverse_selects_before: usize,
    reverse_selects_after: usize,
}

#[test]
#[ignore]
fn callgraph_query_plane_benchmark() {
    let root = crate::test_helpers::manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let files = walk_project_files(&root).collect::<Vec<_>>();
    let store = CallGraphStore::open(
        root.join("target/callgraph-query-bench-store"),
        root.clone(),
    )
    .expect("open benchmark store");
    let stats = store.cold_build(&files).expect("build benchmark store");
    let edges = store.edge_snapshot().expect("read benchmark edges");

    eprintln!(
        "callgraph_query_corpus files={} nodes={} refs={} edges={} build_ms={}",
        stats.files, stats.nodes, stats.refs, stats.edges, stats.elapsed_ms
    );
    let timing = benchmark_impact(&store, &root, &edges);
    eprintln!(
        "callgraph_query_timing {}",
        serde_json::to_string(&timing).unwrap()
    );

    if let Ok(output) = std::env::var("AFT_CALLGRAPH_BENCH_OUTPUT") {
        let snapshot = differential_snapshot(&store, &root, &edges);
        let output = PathBuf::from(output);
        let output = if output.is_absolute() {
            output
        } else {
            root.join(output)
        };
        fs::write(output, serde_json::to_vec(&snapshot).unwrap()).expect("write snapshot");
    }
}

fn benchmark_impact(store: &CallGraphStore, root: &Path, edges: &BTreeSet<StoredEdge>) -> Timing {
    let run = || {
        callgraph_store_adapter::impact_result(
            store,
            &root.join(IMPACT_FILE),
            IMPACT_SYMBOL,
            5,
            true,
        )
        .expect("impact query")
    };
    let warm = run();
    let mut durations = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        std::hint::black_box(run());
        durations.push(started.elapsed());
    }
    durations.sort();
    let (reverse_selects_before, reverse_selects_after) =
        reverse_query_counts(edges, IMPACT_FILE, IMPACT_SYMBOL, 5);
    Timing {
        query: format!("{IMPACT_FILE}::{IMPACT_SYMBOL}"),
        runs_ms: durations.iter().map(duration_ms).collect(),
        median_ms: duration_ms(&durations[RUNS / 2]),
        callers_rendered: warm.callers.len(),
        reverse_selects_before,
        reverse_selects_after,
    }
}

fn reverse_query_counts(
    edges: &BTreeSet<StoredEdge>,
    file: &str,
    symbol: &str,
    max_depth: usize,
) -> (usize, usize) {
    let mut incoming: HashMap<(&str, &str), Vec<(&str, &str)>> = HashMap::new();
    for edge in edges {
        incoming
            .entry((&edge.target_file, &edge.target_symbol))
            .or_default()
            .push((&edge.source_file, &edge.source_symbol));
    }
    let mut fetched = HashSet::new();
    let mut frontier = BTreeSet::from([(file, symbol)]);
    let mut serial = 0usize;
    let mut batched = 0usize;
    for depth in 0..max_depth {
        let targets = frontier
            .into_iter()
            .filter(|target| fetched.insert(*target))
            .collect::<Vec<_>>();
        if targets.is_empty() {
            break;
        }
        serial += targets.len();
        batched += targets.len().div_ceil(REVERSE_BATCH_SIZE);
        let mut next = BTreeSet::new();
        if depth + 1 < max_depth {
            for target in targets {
                next.extend(incoming.get(&target).into_iter().flatten().copied());
            }
        }
        frontier = next;
    }
    (serial, batched)
}

fn differential_snapshot(
    store: &CallGraphStore,
    root: &Path,
    edges: &BTreeSet<StoredEdge>,
) -> Value {
    let source = edges
        .iter()
        .find(|edge| {
            edge.target_file == DIFFERENTIAL_FILE
                && edge.target_symbol == DIFFERENTIAL_SYMBOL
                && edge.source_symbol != "<top-level>"
                && !is_test_path(&edge.source_file)
        })
        .expect("non-test function edge for differential query");
    let cache = Arc::new(RwLock::new(SymbolCache::new()));
    json!({
        "callers": callgraph_store_adapter::callers_result(store, &root.join(DIFFERENTIAL_FILE), DIFFERENTIAL_SYMBOL, 1, false).unwrap(),
        "call_tree": callgraph_store_adapter::call_tree_result(store, &root.join(&source.source_file), &source.source_symbol, 5, false).unwrap(),
        "impact": callgraph_store_adapter::impact_result(store, &root.join(DIFFERENTIAL_FILE), DIFFERENTIAL_SYMBOL, 5, false).unwrap(),
        "trace_to": callgraph_store_adapter::trace_to_result(store, &root.join(DIFFERENTIAL_FILE), DIFFERENTIAL_SYMBOL, 10, false).unwrap(),
        "trace_to_symbol": callgraph_store_adapter::trace_to_symbol_result(store, &root.join(&source.source_file), &source.source_symbol, DIFFERENTIAL_SYMBOL, Some(&root.join(DIFFERENTIAL_FILE)), 10, false).unwrap(),
        "trace_data": callgraph_store_adapter::trace_data_result(store, &root.join(&source.source_file), &source.source_symbol, DIFFERENTIAL_SYMBOL, 5, cache).unwrap(),
    })
}

fn is_test_path(file: &str) -> bool {
    file.contains("/__tests__/")
        || file.contains("/tests/")
        || file.contains("/test/")
        || file.contains(".test.")
        || file.contains(".spec.")
}

fn duration_ms(duration: &Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
