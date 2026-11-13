#!/usr/bin/env python3
"""Release exact-lane replay on one frozen tree, with opt-in source instrumentation.

The generated sources and targets are private to this experiment. Instrumentation
uses otherwise idle wait slots (semantic/rerank are disabled); production sources
and the measured search responses are never edited by the replay.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import math

ROOT = Path(__file__).resolve().parents[2]
DRIVER = r'''
#![allow(clippy::unwrap_used, clippy::print_stderr)]
use aft::{config::Config, context::{AppContext, default_language_provider_factory}, protocol::RawRequest};
use aft::commands::semantic_search::{self, waits::{self, SearchWait}};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf, time::{Duration, Instant}};
fn request(v: Value) -> RawRequest { serde_json::from_value(v).unwrap() }
fn cpu() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    usage.ru_utime.tv_sec as f64 + usage.ru_utime.tv_usec as f64 / 1e6
        + usage.ru_stime.tv_sec as f64 + usage.ru_stime.tv_usec as f64 / 1e6
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let root = std::fs::canonicalize(&args[1]).unwrap();
    let storage = PathBuf::from(&args[2]);
    let queries = std::fs::read_to_string(&args[3]).unwrap();
    let profile = args[5] == "profile";
    semantic_search::exact_lane::exact_bench::ENABLED.store(profile, std::sync::atomic::Ordering::Relaxed);
    let ctx = AppContext::new(default_language_provider_factory(), Config {
        storage_dir: Some(storage.clone()), ..Config::default()
    });
    let response = aft::commands::configure::handle_configure(&request(json!({
        "id":"configure", "command":"configure", "session_id":"exact-bench", "harness":"opencode",
        "project_root":root, "storage_dir":storage,
        "config":[{"tier":"user", "doc":json!({"indexes":{"trigram":true,"semantic":false,"callgraph":false},
            "inspect":{"enabled":false}, "search":{"rerank":{"backend":"off"}}}).to_string()}]
    })), &ctx);
    assert!(response.success, "configure: {:?}", response.data);
    aft::runtime_drain::drain_deferred_configure_maintenance(&ctx);
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let health = serde_json::to_value(ctx.try_health_snapshot(&root)).unwrap();
        if health["search_index"]["status"] == "ready" { break; }
        assert!(Instant::now() < deadline, "not ready: {health}");
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("text ready; semantic disabled; profile={profile}");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&args[4]).unwrap());
    let start_cpu = cpu();
    let start = Instant::now();
    for (id, line) in queries.lines().enumerate() {
        let (query, pattern) = line.split_once('\t').map_or((line, None), |(q,p)| (q,Some(p)));
        let mut body = json!({"id":format!("q-{id}"), "command":"semantic_search",
            "session_id":"exact-bench", "query":query, "top_k":10});
        if let Some(pattern) = pattern { body["pattern"] = json!(pattern); }
        let req = request(body);
        let scope = profile.then(waits::begin_with_wait_cpu);
        let before = Instant::now();
        let response = semantic_search::handle_semantic_search(&req, &ctx);
        let wall = before.elapsed().as_secs_f64()*1000.;
        assert!(response.success, "query {id}: {:?}", response.data);
        let mut row = json!({"query":id, "wall_ms":wall, "response":response});
        if let Some(w) = scope.and_then(|s|s.finish()) {
            let slots: serde_json::Map<_,_> = [SearchWait::ModelLock,SearchWait::EmbedBackend,
                SearchWait::VectorSearch,SearchWait::Embed].into_iter().map(|kind| {
                let s = w.slot(kind); (kind.as_str().to_owned(),json!({"calls":s.count,
                    "wall_ms":s.total.as_secs_f64()*1000., "cpu_ms":s.cpu.as_secs_f64()*1000.}))
            }).collect();
            row["hotspots"] = json!(slots);
            row["prepared"] = json!({"hits":w.prepared.hits,"misses":w.prepared.misses,
                "evicted_by_count":w.prepared.evicted_by_count,"evicted_by_bytes":w.prepared.evicted_by_bytes});
        }
        writeln!(out,"{}",row).unwrap();
        if id % 100 == 0 { eprintln!("query {id}; elapsed {:.1}s",start.elapsed().as_secs_f64()); }
    }
    out.flush().unwrap();
    let summary = json!({"queries":queries.lines().count(), "process_cpu_seconds":cpu()-start_cpu,
        "wall_seconds":start.elapsed().as_secs_f64(), "cache":semantic_search::exact_lane::exact_bench::snapshot()});
    std::fs::write(format!("{}.summary.json",args[4]),serde_json::to_vec_pretty(&summary).unwrap()).unwrap();
    eprintln!("{summary}");
}
'''
BENCH = r'''
/// Instrumentation compiled only into the benchmark's generated source copy.
pub mod exact_bench {
    use std::{collections::HashMap, sync::{Mutex, OnceLock, atomic::{AtomicBool, Ordering}}};
    pub static ENABLED: AtomicBool = AtomicBool::new(false);
    static SEEN: OnceLock<Mutex<HashMap<blake3::Hash, usize>>> = OnceLock::new();
    static CACHE: OnceLock<Mutex<(usize, usize)>> = OnceLock::new();
    pub fn prepared(text: &str) {
        if ENABLED.load(Ordering::Relaxed) {
            *SEEN.get_or_init(Default::default).lock().unwrap().entry(blake3::hash(text.as_bytes())).or_default() += 1;
        }
    }
    pub fn cache(entries: usize, bytes: usize) {
        if ENABLED.load(Ordering::Relaxed) { *CACHE.get_or_init(Default::default).lock().unwrap() = (entries, bytes); }
    }
    pub fn snapshot() -> serde_json::Value {
        let seen = SEEN.get_or_init(Default::default).lock().unwrap();
        let cache = CACHE.get_or_init(Default::default).lock().unwrap();
        serde_json::json!({"distinct_contents_prepared":seen.len(),"preparations":seen.values().sum::<usize>(),
            "contents_prepared_again":seen.values().filter(|n| **n > 1).count(),
            "repeat_preparations":seen.values().map(|n|n-1).sum::<usize>(),
            "entries":cache.0,"charged_bytes":cache.1})
    }
}
'''


def replace(path, old, new):
    text = path.read_text()
    if text.count(old) != 1:
        raise ValueError(f"expected unique instrumentation site: {path}: {old}")
    path.write_text(text.replace(old, new))


def instrument(source):
    exact = source / "crates/aft/src/commands/semantic_search/exact_lane.rs"
    replace(exact, "fn new(text: &str) -> Self {", "fn new(text: &str) -> Self {\n        let _bench = super::waits::time(super::waits::SearchWait::ModelLock);\n        exact_bench::prepared(text);")
    replace(exact, "pub fn scan_symbols_in_text(text: &str) -> Vec<(String, SymbolOffsetRange)> {", "pub fn scan_symbols_in_text(text: &str) -> Vec<(String, SymbolOffsetRange)> {\n    let _bench = super::waits::time(super::waits::SearchWait::EmbedBackend);")
    replace(exact, "        Arc::clone(prepared)\n    }\n}", "        exact_bench::cache(state.entries.len(), state.bytes);\n        Arc::clone(prepared)\n    }\n}")
    with exact.open("a") as out:
        out.write(BENCH)
    data = source / "crates/aft/src/data_layer/tools/search.rs"
    replace(data, "fn candidates_for(&self, query: &Query) -> io::Result<BTreeSet<Vec<u8>>> {", "fn candidates_for(&self, query: &Query) -> io::Result<BTreeSet<Vec<u8>>> {\n        let _bench = waits::time(SearchWait::VectorSearch);")
    memo = source / "crates/aft/src/commands/semantic_search/memo.rs"
    replace(memo, "        self.get_or_verify_with_miss_hook(key, offset, top_k, verifier, || {})", "        let _bench = super::waits::time(super::waits::SearchWait::Embed);\n        self.get_or_verify_with_miss_hook(key, offset, top_k, verifier, || {})")
    (source / "crates/aft/examples/exact_bench.rs").write_text(DRIVER)


def run(argv, **kwargs):
    print("RUN", " ".join(map(str, argv)), flush=True)
    subprocess.run(list(map(str, argv)), check=True, **kwargs)


def archive(ref, destination):
    destination.mkdir(parents=True)
    tar = destination.parent / (destination.name + ".tar")
    with tar.open("wb") as out:
        subprocess.run(["git", "archive", ref], cwd=ROOT, stdout=out, check=True)
    with tarfile.open(tar) as src:
        src.extractall(destination, filter="data")


def build(args, name, ref):
    source = args.out / (name + "-source")
    archive(ref, source)
    instrument(source)
    env = {**os.environ, "CARGO_TARGET_DIR": str(args.out / "build-target")}
    # This target belongs only to the generated sources, never the shared cache.
    run(["cargo", "build", "--release", "--locked", "--offline", "-p", "agent-file-tools", "--example", "exact_bench"], cwd=source, env=env)
    binary = args.out / (name + "-exact-bench")
    shutil.copy2(args.out / "build-target/release/examples/exact_bench", binary)
    return binary


def replay(args, name, label, profile=False):
    output = args.out / (label + ".jsonl")
    run(["timeout", "3600", args.out / (name + "-exact-bench"), args.out / "frozen",
         args.out / (label + "-store"), args.queries, output, "profile" if profile else "timing"],
        env={**os.environ, "AFT_TEST_DISABLE_FILE_WATCHER": "1"})


def compare(args):
    labels = ["baseline-profile", "candidate-profile", "0-baseline", "1-candidate", "2-candidate", "3-baseline"]
    reports = {}
    reference = None
    for label in labels:
        rows = [json.loads(line) for line in (args.out / (label + ".jsonl")).read_text().splitlines()]
        assert len(rows) == 6897, (label, len(rows))
        replies = [json.dumps(row["response"], sort_keys=True, separators=(",", ":")) for row in rows]
        if reference is None:
            reference = replies
        differences = [i for i, (a, b) in enumerate(zip(reference, replies)) if a != b]
        summary = json.loads((args.out / (label + ".jsonl.summary.json")).read_text())
        times = sorted(row["wall_ms"] for row in rows)
        summary.update({"p50_ms":times[math.ceil(len(times)*.50)-1],
                        "p95_ms":times[math.ceil(len(times)*.95)-1],
                        "cpu_ms_per_query":summary["process_cpu_seconds"]*1000/len(rows),
                        "full_response_sha256":hashlib.sha256("\n".join(replies).encode()).hexdigest(),
                        "different_queries":differences})
        if "profile" in label:
            names = {"model_lock":"PreparedExactText::new", "embed_backend":"scan_symbols_in_text",
                     "vector_search":"candidates_for", "embed":"get_or_verify"}
            summary["hotspots_per_query"] = {name:{key:sum(row["hotspots"][slot][key] for row in rows)/len(rows)
                                                   for key in ["calls", "wall_ms", "cpu_ms"]}
                                               for slot,name in names.items()}
            summary["prepared_totals"] = {key:sum(row["prepared"][key] for row in rows) for key in rows[0]["prepared"]}
        reports[label] = summary
    result = {"base":args.base,"candidate":args.candidate,
              "queries_sha256":hashlib.sha256(args.queries.read_bytes()).hexdigest(),
              "profile":"release; default features; temporary identical instrumentation",
              "order":["baseline","candidate","candidate","baseline"],
              "runs":reports}
    (args.out / "comparison.json").write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps(result,indent=2))
    assert not any(report["different_queries"] for report in reports.values()), "response parity failed"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=["baseline", "candidate", "compare"])
    parser.add_argument("--base", required=True)
    parser.add_argument("--candidate", default="HEAD")
    parser.add_argument("--queries", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out = args.out.resolve()
    args.queries = args.queries.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    if args.phase == "baseline":
        archive(args.base, args.out / "frozen")
        build(args, "baseline", args.base)
        replay(args, "baseline", "baseline-profile", True)
    elif args.phase == "candidate":
        build(args, "candidate", args.candidate)
        replay(args, "candidate", "candidate-profile", True)
        for index, arm in enumerate(["baseline", "candidate", "candidate", "baseline"]):
            replay(args, arm, f"{index}-{arm}")
    else:
        compare(args)


if __name__ == "__main__":
    main()
