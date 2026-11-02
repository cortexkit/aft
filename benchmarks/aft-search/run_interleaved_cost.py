"""Build two stage binaries once, then run baseline/candidate/candidate/baseline.

Repeat that four-run order (ABBA, where A=baseline and B=candidate) five times.
Each of the 20 ordered slots runs the same 108 requests with one caller and
eight callers in separate fresh daemons, corpus copies and stores. All builds
finish before measurement. Index setup is outside timing, but each query list
is issued only once, without preliminary query warmup or a second query pass.
"""
from __future__ import annotations

import argparse
from collections import Counter
import contextlib
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys
import tarfile
import time
from types import SimpleNamespace
import unittest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
ORDER = ("baseline", "candidate", "candidate", "baseline")
TARGET_PHASES = ("candidate_text", "matching_best_line")
TOP_LEVEL_PHASES = ("embedding", "ann", "lexical", "exact_and_path", "fusion", "rerank", "snippet")
BASELINE = "3ee12c3ab3cb9b0aa7c500b9738de67a8d2b4d21"
CANDIDATE = "26db5191b64918084a33905cd480a3ab0f06dc12"


def now():
    return datetime.now(timezone.utc).isoformat()


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def command(argv, cwd=ROOT, env=None, stdout=None):
    return subprocess.run(argv, cwd=cwd, env=env, stdout=stdout,
                          stderr=subprocess.STDOUT if stdout else None, check=True)


def git(*arguments, cwd=ROOT):
    return subprocess.check_output(["git", *arguments], cwd=cwd, text=True).strip()


def distribution(values):
    values = sorted(values)
    if not values:
        raise ValueError("empty measurement population")
    quartiles = statistics.quantiles(values, n=4, method="inclusive") if len(values) > 1 else values * 3
    return {"n": len(values), "min": values[0], "q25": quartiles[0],
            "median": statistics.median(values), "q75": quartiles[2],
            "max": values[-1], "spread": values[-1] - values[0]}


def overlaps(before, after, low="min", high="max"):
    return max(before[low], after[low]) <= min(before[high], after[high])


def cpu_verdict(before, after):
    if after["median"] < before["median"]:
        return "candidate whole-query CPU median is better"
    if overlaps(before, after):
        return "candidate median is not better, but observed CPU ranges overlap"
    return "candidate whole-query CPU is consistently worse (non-overlapping ranges)"


def sample_metrics(document):
    count = document["requests"]
    if count != 108 or len(document["records"]) != count or len(document["traces"]) != count:
        raise ValueError("every execution must contain exactly 108 measured requests and traces")
    if document.get("settled_replays", 0) or document.get("settle_replies"):
        raise ValueError("interleaved measurements must not include warm reply replay")
    phases = {name: phase["total"] / count for name, phase in document["cpu_phase_ms"].items()}
    calls = Counter(name for trace in document["traces"] for name, _ in trace.get("cpu_phases", []))
    cpu = document["cpu_seconds_per_query"] * 1000
    for name in TARGET_PHASES:
        if name not in phases:
            raise ValueError(f"missing CPU phase: {name}")
    return {"requests": count, "cpu_ms_per_query": cpu,
            "p50_ms": document["latency_ms"]["p50"], "p95_ms": document["latency_ms"]["p95"],
            "phase_cpu_ms_per_query": phases,
            "phase_calls_per_query": {name: number / count for name, number in sorted(calls.items())},
            "target_cpu_share_pct": {name: phases[name] / cpu * 100 for name in TARGET_PHASES},
            "unaccounted_cpu_ms_per_query": cpu - sum(phases.get(name, 0) for name in TOP_LEVEL_PHASES)}


def host_snapshot():
    affinity = sorted(os.sched_getaffinity(0))
    frequencies = {}
    for cpu in affinity:
        path = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq")
        if path.is_file():
            try:
                frequencies[str(cpu)] = int(path.read_text().strip())
            except OSError:
                pass
    return {"at": now(), "load_average": list(os.getloadavg()),
            "affinity": affinity, "cpu_frequency_khz": frequencies}


def build_binaries(out, baseline_ref, candidate_ref):
    """Reuse one generated source path and Cargo target for both builds."""
    source = out / "build-source"
    objects = git("rev-parse", "--path-format=absolute", "--git-path", "objects")
    builds = {}
    for name, reference in (("baseline", baseline_ref), ("candidate", candidate_ref)):
        revision = git("rev-parse", f"{reference}^{{commit}}")
        archive_path = out / f"{name}-source.tar"
        with archive_path.open("wb") as archive:
            command(["git", "archive", "--format=tar", revision], stdout=archive)
        if source.exists():
            shutil.rmtree(source)
        source.mkdir()
        with tarfile.open(archive_path) as archive:
            for member in archive.getmembers():
                if Path(member.name).is_absolute() or ".." in Path(member.name).parts:
                    raise ValueError(f"unsafe archive path: {member.name}")
                if (member.issym() or member.islnk()) and not hasattr(tarfile, "data_filter"):
                    raise ValueError("archive links require Python's safe data extraction filter")
            options = {"filter": "data"} if hasattr(tarfile, "data_filter") else {}
            archive.extractall(source, **options)
        command(["git", "init", "--quiet"], cwd=source)
        alternates = source / ".git/objects/info/alternates"
        alternates.parent.mkdir(parents=True, exist_ok=True)
        alternates.write_text(objects + "\n")
        command(["git", "update-ref", "--no-deref", "HEAD", revision], cwd=source)
        command(["git", "read-tree", "HEAD"], cwd=source)
        env = {**os.environ, "CARGO_TARGET_DIR": str(ROOT / "target")}
        argv = ["cargo", "build", "--offline", "--locked", "-p", "agent-file-tools",
                "--bin", "aft", "--example", "views_perf_daemon", "--profile", "stage",
                "--features", "test-timing-hooks"]
        started = now()
        print(f"BUILD {name} {revision}", flush=True)
        with (out / f"build-{name}.log").open("w") as log:
            command(argv, cwd=source, env=env, stdout=log)
        binaries = out / "binaries" / name
        binaries.mkdir(parents=True)
        for origin, target in ((ROOT / "target/stage/aft", binaries / "aft"),
                               (ROOT / "target/stage/examples/views_perf_daemon", binaries / "broker")):
            shutil.copy2(origin, target)
        builds[name] = {"revision": revision, "started_at": started, "finished_at": now(),
                        "command": argv, "binary_sha256": sha256(binaries / "aft"),
                        "broker_sha256": sha256(binaries / "broker"),
                        "lock_sha256": sha256(source / "Cargo.lock"),
                        "profile_sha256": sha256(source / "Cargo.toml"),
                        "package_sha256": sha256(source / "crates/aft/Cargo.toml"),
                        "scorer_sha256": sha256(source / "crates/aft/src/commands/semantic_search/mod.rs"),
                        "broker_source_sha256": sha256(source / "crates/aft/examples/views_perf_daemon.rs")}
    for key in ("lock_sha256", "profile_sha256", "package_sha256", "broker_source_sha256"):
        if builds["baseline"][key] != builds["candidate"][key]:
            raise ValueError(f"build inputs differ beyond scorer/version: {key}")
    if builds["baseline"]["scorer_sha256"] == builds["candidate"]["scorer_sha256"]:
        raise ValueError("baseline and candidate must contain different scorer implementations")
    if builds["baseline"]["binary_sha256"] == builds["candidate"]["binary_sha256"]:
        raise ValueError("the two committed source versions produced the same measured binary")
    return builds


def summarize(runs):
    result = {}
    for concurrency in (1, 8):
        arms = {}
        for arm in ("baseline", "candidate"):
            samples = [run["executions"][str(concurrency)]["metrics"]
                       for run in runs if run["binary"] == arm]
            metrics = {key: distribution([sample[key] for sample in samples])
                       for key in ("cpu_ms_per_query", "p50_ms", "p95_ms")}
            phases = sorted({name for sample in samples for name in sample["phase_cpu_ms_per_query"]})
            metrics["phase_cpu_ms_per_query"] = {
                name: distribution([sample["phase_cpu_ms_per_query"].get(name, 0) for sample in samples])
                for name in phases}
            metrics["phase_calls_per_query"] = {
                name: distribution([sample.get("phase_calls_per_query", {}).get(name, 0) for sample in samples])
                for name in phases}
            metrics["target_cpu_share_pct"] = {
                name: {"aggregate": sum(sample["phase_cpu_ms_per_query"][name] for sample in samples)
                                    / sum(sample["cpu_ms_per_query"] for sample in samples) * 100,
                       "per_run": distribution([sample["target_cpu_share_pct"][name] for sample in samples])}
                for name in TARGET_PHASES}
            arms[arm] = metrics
        before = arms["baseline"]["cpu_ms_per_query"]
        after = arms["candidate"]["cpu_ms_per_query"]
        blocks = []
        for offset in range(0, len(runs), 4):
            block = runs[offset:offset + 4]
            if [run["binary"] for run in block] != list(ORDER):
                raise ValueError("measurement order is not ABBA")
            values = [run["executions"][str(concurrency)]["metrics"]["cpu_ms_per_query"] for run in block]
            baseline = (values[0] + values[3]) / 2
            candidate = (values[1] + values[2]) / 2
            blocks.append({"block": offset // 4 + 1, "baseline_cpu_ms": baseline,
                           "candidate_cpu_ms": candidate, "candidate_over_baseline": candidate / baseline})
        phase_growth = []
        for name in TOP_LEVEL_PHASES:
            old = arms["baseline"]["phase_cpu_ms_per_query"].get(name, {}).get("median", 0)
            new = arms["candidate"]["phase_cpu_ms_per_query"].get(name, {}).get("median", 0)
            phase_growth.append({"phase": name, "baseline_median_ms": old,
                                 "candidate_median_ms": new, "delta_ms": new - old})
        result[str(concurrency)] = {
            "arms": arms, "cpu_verdict": cpu_verdict(before, after),
            "candidate_over_baseline_cpu_median": after["median"] / before["median"],
            "cpu_range_overlap": overlaps(before, after),
            "cpu_iqr_overlap": overlaps(before, after, "q25", "q75"),
            "paired_blocks": blocks,
            "paired_cpu_ratio": distribution([block["candidate_over_baseline"] for block in blocks]),
            "all_paired_blocks_worse": all(block["candidate_over_baseline"] > 1 for block in blocks),
            "top_level_phase_growth": sorted(phase_growth, key=lambda row: row["delta_ms"], reverse=True)}
    return result


def write_result(path, document):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def run_experiment(args):
    if sys.platform != "linux":
        raise ValueError("run the measurement with runon: linux,8c")
    import provision_evidence
    import run_sidekick_cost as probe

    out = Path(args.out).resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=False)
    results = Path(args.results).resolve()
    provision_evidence.provision()
    builds = build_binaries(out, args.baseline_ref, args.candidate_ref)
    manifest = json.loads((HERE / "real-query-manifest.json").read_text())
    document = {
        "schema": "aft-search-interleaved-cost-v1", "complete": False,
        "protocol": {"cycles": 5, "order_per_cycle": list(ORDER), "slots": 20,
                     "concurrencies_per_slot": [1, 8], "requests_per_execution": 108,
                     "fresh_daemon_corpus_and_stores_per_execution": True,
                     "inspect_enabled": False, "watcher_enabled": False,
                     "warmup_or_settlement_replay": False, "profile": "stage",
                     "features": ["test-timing-hooks"],
                     "spread_definition": "min/max range; inclusive quartiles also reported",
                     "cpu_scope": "all AFT daemon threads during the 108-request traffic interval",
                     "phase_shares": "inclusive thread CPU / whole-daemon CPU; nested phases must not be added"},
        "host": {"platform": platform.platform(), "node": platform.node(),
                 "python": platform.python_version(), "logical_cpus": os.cpu_count(),
                 "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip()},
        "builds": builds, "measurement_started_at": now(),
        "manifest_sha256": sha256(HERE / "real-query-manifest.json"),
        "evidence_sha": manifest["evidence_sha"],
        "evidence_tree_sha256": sorted({row["evidence_tree_sha256"] for row in manifest["rows"]
                                       if not row.get("excluded_reason")}),
        "runs": [],
    }
    broker = out / "binaries/candidate/broker"
    expected_queries = None
    os.environ["AFT_TEST_DISABLE_FILE_WATCHER"] = "1"
    write_result(results, document)
    for slot in range(20):
        arm = ORDER[slot % 4]
        run = {"slot": slot + 1, "block": slot // 4 + 1, "position_in_block": slot % 4 + 1,
               "binary": arm, "executions": {}}
        for concurrency in (1, 8):
            label = f"{slot + 1:02d}-{arm}-{concurrency}"
            directory = out / label
            before = host_snapshot()
            started = time.monotonic()
            with (out / f"{label}.log").open("w") as log, contextlib.redirect_stdout(log):
                probe.run(SimpleNamespace(binary=str(out / "binaries" / arm / "aft"),
                                          broker=str(broker), out=str(directory), concurrency=concurrency,
                                          queries=108, all_queries=True, repeats=1, rerank=True,
                                          ready_timeout=180, settle_replies=False))
            after = host_snapshot()
            raw = json.loads((directory / "result.json").read_text())
            if expected_queries is None:
                expected_queries = raw["queries"]
                document["queries"] = expected_queries
            if raw["queries"] != expected_queries or len(expected_queries) != 108:
                raise ValueError("request population changed between executions")
            metrics = sample_metrics(raw)
            pending = sum(probe.watcher_pending(row["full_response"]) for row in raw["records"])
            run["executions"][str(concurrency)] = {
                "metrics": metrics, "host_before_setup": before, "host_after_teardown": after,
                "ready_index": raw["ready_index"],
                "elapsed_with_setup_seconds": time.monotonic() - started,
                "raw_result_sha256": sha256(directory / "result.json"),
                "watcher_pending_replies": pending}
            print(f"RUN {slot + 1:02d}/20 {arm} callers={concurrency} "
                  f"CPU={metrics['cpu_ms_per_query']:.3f}ms "
                  f"p50={metrics['p50_ms']:.3f}ms p95={metrics['p95_ms']:.3f}ms", flush=True)
        document["runs"].append(run)
        write_result(results, document)
    for name in ("baseline", "candidate"):
        if sha256(out / "binaries" / name / "aft") != builds[name]["binary_sha256"]:
            raise ValueError("a measured binary changed after the builds finished")
    document["summary"] = summarize(document["runs"])
    document["measurement_finished_at"] = now()
    document["complete"] = True
    write_result(results, document)
    print(json.dumps(document["summary"], indent=2))


class ProtocolTests(unittest.TestCase):
    def test_five_abba_blocks_are_balanced(self):
        order = [ORDER[number % 4] for number in range(20)]
        self.assertEqual(order, list(ORDER) * 5)
        self.assertEqual(order.count("baseline"), 10)
        self.assertEqual(order.count("candidate"), 10)

    def test_spread_and_inclusive_quartiles(self):
        self.assertEqual(distribution([5, 1, 4, 2, 3]),
                         {"n": 5, "min": 1, "q25": 2, "median": 3, "q75": 4, "max": 5, "spread": 4})

    def test_empty_population_is_not_a_pass(self):
        with self.assertRaises(ValueError):
            distribution([])

    def test_verdict_distinguishes_overlap_and_consistent_regression(self):
        before = distribution([10, 11, 12])
        self.assertIn("better", cpu_verdict(before, distribution([8, 9, 10])))
        self.assertIn("overlap", cpu_verdict(before, distribution([11, 12, 13])))
        self.assertIn("consistently worse", cpu_verdict(before, distribution([13, 14, 15])))

    def test_phase_cpu_share_uses_whole_daemon_cpu(self):
        document = {"requests": 108, "records": [{}] * 108, "traces": [{}] * 108,
                    "cpu_seconds_per_query": .1, "latency_ms": {"p50": 5, "p95": 9},
                    "cpu_phase_ms": {"candidate_text": {"total": 2160},
                                     "matching_best_line": {"total": 1080}}}
        self.assertEqual(sample_metrics(document)["target_cpu_share_pct"],
                         {"candidate_text": 20, "matching_best_line": 10})

    def test_replayed_workload_is_rejected(self):
        with self.assertRaises(ValueError):
            sample_metrics({"requests": 108, "records": [{}] * 108, "traces": [{}] * 108,
                            "settled_replays": 1})

    def test_summary_preserves_balanced_arms_and_paired_ratios(self):
        runs = []
        for number in range(20):
            arm = ORDER[number % 4]
            executions = {}
            for concurrency in (1, 8):
                cpu = (100 if arm == "baseline" else 80) * concurrency
                executions[str(concurrency)] = {"metrics": {
                    "cpu_ms_per_query": cpu, "p50_ms": cpu / 2, "p95_ms": cpu * 2,
                    "phase_cpu_ms_per_query": {"candidate_text": cpu * .2, "matching_best_line": cpu * .1},
                    "target_cpu_share_pct": {"candidate_text": 20, "matching_best_line": 10}}}
            runs.append({"binary": arm, "executions": executions})
        result = summarize(runs)
        self.assertEqual(result["1"]["arms"]["baseline"]["cpu_ms_per_query"]["n"], 10)
        self.assertEqual(result["8"]["paired_cpu_ratio"]["n"], 5)
        self.assertAlmostEqual(result["8"]["paired_cpu_ratio"]["median"], .8)
        self.assertAlmostEqual(result["1"]["arms"]["candidate"]["target_cpu_share_pct"]["candidate_text"]["aggregate"], 20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--baseline-ref", default=BASELINE)
    parser.add_argument("--candidate-ref", default=CANDIDATE)
    parser.add_argument("--out", default=".interleaved-search-private")
    parser.add_argument("--results", default=str(HERE / "results/rerank-best-line-interleaved.json"))
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ProtocolTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1
    try:
        run_experiment(args)
    except Exception:
        logs = sorted(Path(args.out).glob("*.log"), key=lambda path: path.stat().st_mtime)
        for log in logs[-2:]:
            print(f"FAILED RUN LOG {log}\n{log.read_text()[-8000:]}", file=sys.stderr)
        raise
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
