#!/usr/bin/env python3
"""Compare full inspect replies and elapsed request time on pinned TypeORM.

--baseline-sources supplies a JSON map of repository paths to original source;
--corpus-archive supplies a tar archive of the pinned TypeORM tree. Build that
baseline and the current source with the same phase timers. Timers write only to
stderr so they cannot alter NDJSON responses. Alternate before/after requests in
warm processes and save raw replies and timings under --output, outside target/.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import statistics
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
spec = importlib.util.spec_from_file_location("inspect_truth", HERE / "run.py")
truth = importlib.util.module_from_spec(spec)
spec.loader.exec_module(truth)


def replace_once(source: str, old: str, new: str) -> str:
    if source.count(old) != 1:
        raise ValueError(f"probe anchor occurs {source.count(old)} times: {old[:100]}")
    return source.replace(old, new)


def instrument(root: Path) -> None:
    manager = root / "crates/aft/src/inspect/manager.rs"
    source = manager.read_text()
    probe = """
pub(crate) struct InspectPerfProbe(String, std::time::Instant);
impl InspectPerfProbe {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self(label.into(), std::time::Instant::now())
    }
}
impl Drop for InspectPerfProbe {
    fn drop(&mut self) {
        eprintln!("INSPECT_PERF {} {:.9}", self.0, self.1.elapsed().as_secs_f64());
    }
}
"""
    source += probe
    anchor = "    let project_scope = JobScope::for_project(snapshot.project_root.clone());"
    # The function boundary keeps probes out of similarly named worker paths.
    start = source.index("fn scoped_tier2_payload_from_contributions(")
    end = source.index("fn scoped_tier2_rollup_job(", start)
    body = source[start:end]
    body = replace_once(body, anchor, "    let _scope_probe = InspectPerfProbe::new(format!(\"scope/{}\", category.as_str()));\n" + anchor)
    body = replace_once(body, "    let contributions = load_contributions(cache, &rollup_job)?;", "    let contributions = { let _p = InspectPerfProbe::new(format!(\"decode/{}\", category.as_str())); load_contributions(cache, &rollup_job)? };")
    source = source[:start] + body + source[end:]
    start = source.index("    fn tier2_read_cached_from_cache(")
    end = source.index("    fn tier2_cached_aggregate_is_fresh(", start)
    body = source[start:end]
    body = replace_once(body, "match cache.get_aggregated_for_config(&key, snapshot.config.as_ref()) {", "match { let _p = InspectPerfProbe::new(format!(\"fetch/{}\", category.as_str())); cache.get_aggregated_for_config(&key, snapshot.config.as_ref()) } {")
    body = replace_once(body, "match self.tier2_cached_aggregate_is_fresh(snapshot, category, cache) {", "match { let _p = InspectPerfProbe::new(format!(\"freshness/{}\", category.as_str())); self.tier2_cached_aggregate_is_fresh(snapshot, category, cache) } {")
    source = source[:start] + body + source[end:]
    manager.write_text(source)
    module = root / "crates/aft/src/inspect/mod.rs"
    module.write_text(module.read_text() + "\npub(crate) use manager::InspectPerfProbe;\n")

    command = root / "crates/aft/src/commands/inspect.rs"
    source = command.read_text()
    start = source.index("fn handle_inspect_payload(")
    end = source.index("fn start_on_demand_callgraph_for_inspect(", start)
    body = source[start:end]
    body = replace_once(body, ") -> Response {", ") -> Response {\n    let _request_probe = crate::inspect::InspectPerfProbe::new(\"request\");")
    body = replace_once(body, "    let mut payload = build_inspect_payload_with_offset(", "    let render_probe = crate::inspect::InspectPerfProbe::new(\"render\");\n    let mut payload = build_inspect_payload_with_offset(")
    body = replace_once(body, "    let runtime_notes = if scope_was_provided {", "    drop(render_probe);\n    let notes_probe = crate::inspect::InspectPerfProbe::new(\"runtime_notes\");\n    let runtime_notes = if scope_was_provided {")
    body = replace_once(body, "    append_inspect_runtime_notes(&mut payload, &runtime_notes);", "    append_inspect_runtime_notes(&mut payload, &runtime_notes);\n    drop(notes_probe);")
    source = source[:start] + body + source[end:]
    command.write_text(source)


def build_pair(baseline_sources: Path, out: Path, after_binary: Path | None = None,
               before_binary: Path | None = None) -> dict[str, Path]:
    binaries = {}
    for name, supplied in (("before", before_binary), ("after", after_binary)):
        if supplied:
            binaries[name] = out / f"aft-{name}"
            shutil.copy2(supplied, binaries[name])
    sources = json.loads(baseline_sources.read_text())
    # One target directory allows dependency reuse, but builds never overlap.
    env = {**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target/scoped-parity")}
    for name in ("before", "after"):
        if name in binaries:
            continue
        with tempfile.TemporaryDirectory(prefix="inspect-sources-") as temporary:
            tree = Path(temporary) / "aft"
            shutil.copytree(ROOT, tree, ignore=shutil.ignore_patterns(
                ".git", "node_modules", "target", "local-ignore", "dist", "*.tar.gz",
                "scoped-baseline-sources.json"))
            if name == "before":
                for path, content in sources.items():
                    (tree / path).write_text(content)
            instrument(tree)
            subprocess.run(["cargo", "build", "--release", "-p", "agent-file-tools", "--bin", "aft", "-j", "4"], cwd=tree, env=env, check=True)
            binary = out / f"aft-{name}"
            shutil.copy2(Path(env["CARGO_TARGET_DIR"]) / "release/aft", binary)
            binaries[name] = binary
    persistent = ROOT / "target/scoped-parity-pair"
    persistent.mkdir(exist_ok=True)
    for name, binary in binaries.items():
        shutil.copy2(binary, persistent / f"aft-{name}")
    return {name: binaries[name] for name in ("before", "after")}


def canonical(response: dict) -> bytes:
    response = json.loads(json.dumps(response))
    response.pop("id", None)
    response.get("scanner_state", {}).pop("tier2_last_run", None)
    return json.dumps(response, separators=(",", ":"), ensure_ascii=False).encode()


def phases(log: Path) -> list[dict[str, float]]:
    requests, current = [], {}
    for line in log.read_text().splitlines():
        if not line.startswith("INSPECT_PERF "):
            continue
        _, name, seconds = line.split()
        current[name] = current.get(name, 0.0) + float(seconds)
        if name == "request":
            requests.append(current)
            current = {}
    return requests


def producer_barrier_ready(graph: dict, response: dict, *, seed_only: bool = False) -> bool:
    # A successful navigation answer comes only from a published Ready store.
    # A selected scheduler refresh clears its demand; a subsequent blocking inspect
    # must also have produced all five Tier-2 summaries, not pending/unknown rows.
    if not (graph.get("success") is True
            and graph.get("symbol") == "DataSource::initialize"
            and graph.get("file") == "packages/typeorm/src/data-source/DataSource.ts"
            and type(graph.get("scanned_files")) is int
            and graph["scanned_files"] > 0):
        return False
    if response.get("success") is not True:
        return False
    state = response.get("scanner_state") or {}
    if not seed_only and state.get("tier2_trigger_reason") not in ("pull", "configure_warm"):
        return False
    summaries = response.get("summary") or {}
    for category in ("dead_code", "unused_exports", "duplicates", "cycles", "complexity"):
        row = summaries.get(category) or {}
        if (type(row.get("count")) is not int
                or row.get("status") in ("pending", "unavailable", "not_requested")
                or row.get("unavailable") is True or row.get("not_computed") is True
                or row.get("building") is not None):
            return False
    return True


def wait_for_producer_barrier(session, *, seed_only: bool = False, deadline_s: float = 600) -> dict:
    deadline = time.monotonic() + deadline_s
    observations = []
    while time.monotonic() < deadline:
        # These are real blocking analysis requests, not a timed sleep or a
        # guess based on a null scheduler field. They also drain producer events.
        response = session.inspect(truth.AFT_CATEGORIES)
        if response.get("success") is not True:
            raise RuntimeError(response)
        graph = session.client.request(
            "callers", timeout_s=1800,
            file=str(session.repo / "packages/typeorm/src/data-source/DataSource.ts"),
            symbol="initialize", depth=1)
        if graph.get("success") is True:
            response = session.inspect(truth.AFT_CATEGORIES)
        elif graph.get("code") != "callgraph_building":
            raise RuntimeError(f"callgraph barrier failed: {graph}")
        observations.append({"graph": {key: graph.get(key) for key in ("success", "code", "symbol", "file", "scanned_files")},
                             "scanner_state": response.get("scanner_state"),
                             "counts": {key: row.get("count") for key, row in response.get("summary", {}).items()}})
        if producer_barrier_ready(graph, response, seed_only=seed_only):
            (session.storage / "producer-barrier.json").write_text(json.dumps(observations, indent=2) + "\n")
            print(f"producer barrier: {session.storage.name}, seed={seed_only}, {len(observations)} observations, reason={response['scanner_state']['tier2_trigger_reason']}", flush=True)
            return response
    (session.storage / "producer-barrier.json").write_text(json.dumps(observations, indent=2) + "\n")
    raise RuntimeError("producer barrier did not observe a published graph and completed scheduler refresh")


def measure(binaries: dict[str, Path], corpus: Path, out: Path, runs: int) -> dict:
    sessions, records, barriers = {}, [], {}
    configs = out / "user.jsonc"
    configs.write_text("{}\n")
    disabled = out / "disabled.jsonc"
    disabled.write_text(json.dumps(truth.COLLECTION_USER_CONFIG))
    try:
        for name, binary in binaries.items():
            storage = out / name
            seed = truth.AftSession(binary, corpus, storage)
            configured = seed.configure(disabled)
            if not configured.get("success"):
                raise RuntimeError(configured)
            try:
                wait_for_producer_barrier(seed, seed_only=True)
            finally:
                seed.close()
            # Restart from the same published stores. Pending cold-producer
            # events belong to the seed process, not to the measured session.
            for path in [storage / "aft-stderr.log", storage / "producer-barrier.json", *storage.glob("inspect-*.json")]:
                if path.exists():
                    path.rename(path.with_name("seed-" + path.name))
            session = truth.AftSession(binary, corpus, storage)
            sessions[name] = session
            configured = session.configure(disabled)
            if not configured.get("success"):
                raise RuntimeError(configured)
            settled = wait_for_producer_barrier(session)
            barriers[name] = settled["scanner_state"]["tier2_trigger_reason"]
            session.configure(configs)

        if barriers["before"] != barriers["after"]:
            raise RuntimeError(f"producer barriers reached different observable states: {barriers}")

        cases = [
            {"scope": ["packages"], "sections": ["dead_code"], "topK": 100},
            {"scope": ["packages/typeorm/src/index.ts"], "sections": ["dead_code"], "topK": 100},
            {"scope": ["."], "sections": ["dead_code"], "topK": 100},
            {"scope": ["packages/typeorm/src"], "sections": ["dead_code", "unused_exports"], "topK": 7},
            {"scope": ["packages/legacy-naming-strategies/src"], "sections": ["all"], "topK": 100},
            {"scope": ["packages/typeorm/src/index.ts"], "topK": 100},
            {"scope": ["packages/typeorm/src/index.ts"], "sections": ["diagnostics"], "topK": 100},
            {"sections": ["dead_code"], "topK": 100},
        ]
        # Diagnostics parity uses disabled producers for a stable, honest named-
        # gap response. Performance calls leave servers configured normally.
        for index, params in enumerate(cases):
            replies = {}
            for name, session in sessions.items():
                session.configure(disabled)
                session.calls += 1
                response = session.client.request("inspect", timeout_s=1800, **params)
                (out / f"parity-{index}-{name}.json").write_text(json.dumps(response, indent=2))
                if not response.get("success"):
                    raise RuntimeError(response)
                replies[name] = canonical(response)
            if replies["before"] != replies["after"]:
                raise RuntimeError(f"RESPONSE PARITY FAILED case {index}; see raw replies")
            print(f"parity {index}: {hashlib.sha256(replies['before']).hexdigest()}", flush=True)

        for session in sessions.values():
            session.configure(configs)
            session.inspect(["dead_code"], ["packages"])
        for iteration in range(runs):
            # Alternate which binary is first, while retaining each warm process.
            order = ("before", "after") if iteration % 2 == 0 else ("after", "before")
            replies = {}
            for name in order:
                session = sessions[name]
                started = time.monotonic()
                response = session.inspect(["dead_code"], ["packages"])
                elapsed = time.monotonic() - started
                replies[name] = canonical(response)
                records.append({"binary": name, "iteration": iteration, "seconds": elapsed,
                                "inspect_index": session.calls - 1})
                print(f"timing {name} {iteration}: {elapsed:.6f}s", flush=True)
            if replies["before"] != replies["after"]:
                raise RuntimeError(f"RESPONSE PARITY FAILED timing {iteration}")
    finally:
        for session in sessions.values():
            session.close()

    for record in records:
        timing = phases(out / record["binary"] / "aft-stderr.log")
        record["phases"] = timing[record["inspect_index"]]
    result = {"runs": records, "parity_cases": len(cases), "medians": {}, "producer_barriers": barriers}
    for name in binaries:
        selected = [r for r in records if r["binary"] == name]
        result["medians"][name] = statistics.median(r["seconds"] for r in selected)
        result.setdefault("phase_means", {})[name] = {
            key: statistics.mean(r["phases"].get(key, 0) for r in selected)
            for key in sorted({key for r in selected for key in r["phases"]})
        }
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-sources", type=Path, required=True)
    parser.add_argument("--corpus-archive", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--before-binary", type=Path, help="reuse an already built baseline")
    parser.add_argument("--after-binary", type=Path, help="reuse an already built candidate")
    parser.add_argument("--binaries-dir", type=Path, help="reuse a previously built, probed pair")
    args = parser.parse_args()
    if args.runs < 5:
        parser.error("at least five runs per binary are required")
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    subprocess.run(["rustc", "--version"], check=True)
    binaries = ({name: (args.binaries_dir / f"aft-{name}").resolve() for name in ("before", "after")}
                if args.binaries_dir else build_pair(args.baseline_sources, out, args.after_binary, args.before_binary))
    corpus = out / "typeorm"
    corpus.mkdir()
    with tarfile.open(args.corpus_archive) as archive:
        archive.extractall(corpus, filter="data")
    (corpus / ".cortexkit").mkdir(exist_ok=True)
    (corpus / ".cortexkit/aft.jsonc").write_text(json.dumps(truth.AFT_PROJECT_CONFIG))
    exclusions = [
        "column-readonly-metadata-args-namespace-type-only.input.ts",
        "column-readonly-metadata-args-namespace-type-only.output.ts",
        "column-readonly-metadata-args-namespace.input.ts",
        "column-readonly-metadata-args-namespace.output.ts",
    ]
    ignore = corpus / ".aftignore"
    with ignore.open("a") as stream:
        stream.write("\n" + "\n".join("packages/codemod/test/transforms/v1/fixtures/column-readonly/" + p for p in exclusions) + "\n")
    result = measure(binaries, corpus, out, args.runs)
    result["binary_sha256"] = {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in binaries.items()}
    (out / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2), flush=True)
    print(f"PARITY PASSED: {result['parity_cases']} cases and {args.runs} interleaved pairs", flush=True)


if __name__ == "__main__":
    main()
