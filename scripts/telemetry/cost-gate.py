#!/usr/bin/env python3
"""Run and compare the fixed nightly OSS index-cost matrix.

The shell wrapper is the public entry point.  This module deliberately reads
only the matrix CSV and ``index_event`` records: prose log messages are useful
for diagnosis, but never become a gate metric.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from dataclasses import dataclass
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
SYNTHETIC_FILE_COUNT = 24_000
SYNTHETIC_VERSION = "synthetic-v2"
REPO_ORDER = ("redox", "typescript-eslint", "jupyterlab", "hugo", "synthetic-24k")
FIXED_REPOSITORIES = {
    "redox": {
        "kind": "git",
        "url": "https://github.com/redox-os/redox.git",
    },
    "typescript-eslint": {
        "kind": "git",
        "url": "https://github.com/typescript-eslint/typescript-eslint.git",
    },
    "jupyterlab": {
        "kind": "git",
        "url": "https://github.com/jupyterlab/jupyterlab.git",
    },
    "hugo": {
        "kind": "git",
        "url": "https://github.com/gohugoio/hugo.git",
    },
    "synthetic-24k": {
        "kind": "synthetic",
        "url": None,
    },
}

# Every gate metric is a lower-is-better value.  The absolute floor is the
# minimum allowed budget; using max(relative_limit, floor) prevents tiny
# measurements from paging on one scheduler tick of noise.
DEFAULT_METRICS: dict[str, tuple[float, float]] = {
    "search_build_ready_ms": (25.0, 3_000.0),
    "callgraph_build_ready_ms": (25.0, 3_000.0),
    "callgraph_resolution_share_pct": (20.0, 5.0),
    "peak_rss_mb": (20.0, 128.0),
    "cpu_seconds": (25.0, 1.0),
    "search_first_query_ms": (25.0, 3_000.0),
    "callgraph_first_query_ms": (25.0, 3_000.0),
}
WAITING_DEFAULT = (25.0, 1.0)
WAITING_CAUSES = ("build", "limiter", "artifact_load", "resolver")
COMPACT_RE = re.compile(r"^\d+/([^/]+)/[^/]+$")
INDEX_EVENT_RE = re.compile(r"\bindex_event(?P<body>(?: [a-z_]+=[^ =]+)+)")
INDEX_NUMERIC_FIELDS = ("elapsed_ms", "ready_to_first_query_ms", "completed", "total")


@dataclass
class RunData:
    path: Path
    repo: str
    metrics: dict[str, float]
    events: dict[str, float]
    ready: bool
    row: dict[str, str]


@dataclass
class Regression:
    metric: str
    baseline: float | None
    observed: float | None
    limit: float | None
    reason: str
    run: RunData | None = None


def run_command(argv: list[str], *, cwd: Path | None = None, timeout: int = 300) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=str(cwd) if cwd else None, text=True, capture_output=True, check=False, timeout=timeout)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def compact_p50(value: str | None) -> float | None:
    if not value:
        return None
    match = COMPACT_RE.match(value.strip())
    if not match or match.group(1) in {"", "n/a"}:
        return None
    try:
        parsed = float(match.group(1))
    except ValueError:
        return None
    return parsed if math.isfinite(parsed) else None


def scalar(value: str | None) -> float | None:
    if not value or value.strip().lower() in {"", "n/a", "none"}:
        return None
    try:
        parsed = float(value)
    except ValueError:
        return None
    return parsed if math.isfinite(parsed) else None


def waiting_metrics(value: str | None) -> dict[str, float]:
    metrics: dict[str, float] = {f"waiting_on.{cause}": 0.0 for cause in WAITING_CAUSES}
    if not value or value.strip() in {"", "none", "n/a"}:
        metrics["waiting_on.total"] = 0.0
        return metrics
    total = 0
    for item in value.split(";"):
        item = item.strip()
        if not item or "=" not in item:
            continue
        name, raw_count = item.split("=", 1)
        try:
            count = int(raw_count)
        except ValueError:
            continue
        if count < 0:
            continue
        metrics[f"waiting_on.{name.strip()}"] = float(count)
        total += count
    metrics["waiting_on.total"] = float(total)
    return metrics


def parse_index_events(log_paths: str) -> dict[str, float]:
    counts: dict[str, int] = defaultdict(int)
    numeric: dict[str, list[float]] = defaultdict(list)
    if not log_paths or log_paths == "n/a":
        return {}
    for raw_path in log_paths.split(";"):
        if not raw_path:
            continue
        path = Path(raw_path)
        if not path.is_file():
            continue
        with path.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                match = INDEX_EVENT_RE.search(line)
                if not match:
                    continue
                fields: dict[str, str] = {}
                for token in match.group("body").split():
                    key, _, value = token.partition("=")
                    if key and value:
                        fields[key] = value
                kind = fields.get("kind")
                plane = fields.get("plane")
                if not kind or not plane:
                    continue
                prefix = f"{plane}.{kind}"
                counts[f"{prefix}.count"] += 1
                stage = fields.get("stage")
                if stage:
                    counts[f"{prefix}.stage.{stage}.count"] += 1
                for field in INDEX_NUMERIC_FIELDS:
                    raw_value = fields.get(field)
                    if raw_value is None:
                        continue
                    try:
                        numeric[f"{prefix}.{field}"].append(float(raw_value))
                    except ValueError:
                        continue
    summary: dict[str, float] = {key: float(value) for key, value in counts.items()}
    for key, values in numeric.items():
        ordered = sorted(values)
        middle = ordered[max(0, (len(ordered) + 1) // 2 - 1)]
        summary[f"{key}.p50"] = middle
        summary[f"{key}.max"] = max(values)
    return summary


def row_metrics(row: dict[str, str]) -> dict[str, float]:
    fields = {
        "search_build_ready_ms": "search_wall_ms",
        "callgraph_build_ready_ms": "callgraph_wall_ms",
        "callgraph_resolution_share_pct": "callgraph_resolution_share_pct",
        "search_first_query_ms": "search_first_query_ms",
        "callgraph_first_query_ms": "callgraph_first_query_ms",
        "peak_rss_mb": "peak_rss_mb",
        "cpu_seconds": "cpu_s",
    }
    metrics: dict[str, float] = {}
    for metric, field in fields.items():
        parsed = compact_p50(row.get(field)) if field.endswith("_ms") or field == "callgraph_resolution_share_pct" else scalar(row.get(field))
        if parsed is not None:
            metrics[metric] = parsed
    metrics.update(waiting_metrics(row.get("waiting_on")))
    return metrics


def read_run_csv(path: Path) -> list[RunData]:
    with path.open(encoding="utf-8", newline="") as handle:
        rows = list(csv.DictReader(handle))
    result: list[RunData] = []
    for row in rows:
        repo = row.get("repo", "")
        result.append(
            RunData(
                path=path,
                repo=repo,
                metrics=row_metrics(row),
                events=parse_index_events(row.get("log_path", "")),
                ready=row.get("outcome") == "ready",
                row=row,
            )
        )
    return result


def baseline_metric(value: Any, metric: str) -> tuple[float, float, float]:
    if isinstance(value, dict):
        raw_value = value.get("value")
        tolerance = value.get("tolerance_pct")
        floor = value.get("absolute_floor")
    else:
        raw_value = value
        tolerance, floor = DEFAULT_METRICS.get(metric, WAITING_DEFAULT)
    try:
        parsed_value = float(raw_value)
        parsed_tolerance = float(tolerance)
        parsed_floor = float(floor)
    except (TypeError, ValueError) as error:
        raise ValueError(f"invalid baseline metric {metric}: {value!r}") from error
    if not all(math.isfinite(item) for item in (parsed_value, parsed_tolerance, parsed_floor)):
        raise ValueError(f"baseline metric {metric} must be finite")
    if parsed_value < 0 or parsed_tolerance < 0 or parsed_floor < 0:
        raise ValueError(f"baseline metric {metric} cannot be negative")
    return parsed_value, parsed_tolerance, parsed_floor


def event_deltas(baseline: dict[str, Any], observed: dict[str, float]) -> list[tuple[str, float, float, float]]:
    deltas: list[tuple[str, float, float, float]] = []
    for key in set(baseline) | set(observed):
        try:
            before = float(baseline.get(key, 0.0))
            after = float(observed.get(key, 0.0))
        except (TypeError, ValueError):
            continue
        delta = after - before
        if delta:
            deltas.append((key, before, after, delta))
    return sorted(deltas, key=lambda item: (-abs(item[3]), item[0]))[:3]


def compare_repo(repo: str, baseline_repo: dict[str, Any], runs: list[RunData]) -> list[Regression]:
    ready_runs = [run for run in runs if run.ready]
    regressions: list[Regression] = []
    if not ready_runs:
        regressions.append(Regression("matrix.ready", None, None, None, "both runs were not ready", runs[0] if runs else None))
        return regressions
    raw_metrics = baseline_repo.get("metrics")
    if not isinstance(raw_metrics, dict) or not raw_metrics:
        raise ValueError(f"baseline for {repo} has no metrics; run --write-baseline first")
    for metric, raw_baseline in sorted(raw_metrics.items()):
        before, tolerance, floor = baseline_metric(raw_baseline, metric)
        candidates = [(run.metrics.get(metric, 0.0 if metric.startswith("waiting_on.") else None), run) for run in ready_runs]
        available = [(value, run) for value, run in candidates if value is not None]
        if not available:
            regressions.append(Regression(metric, before, None, None, "metric missing from both ready CSV rows", ready_runs[0]))
            continue
        observed, selected_run = min(available, key=lambda item: item[0])
        limit = max(before * (1.0 + tolerance / 100.0), floor)
        if observed > limit:
            regressions.append(Regression(metric, before, observed, limit, "observed value exceeded tolerance/floor", selected_run))
    return regressions


def validate_baseline(payload: dict[str, Any]) -> None:
    if payload.get("schema") != SCHEMA_VERSION:
        raise ValueError(f"baseline schema must be {SCHEMA_VERSION}")
    repos = payload.get("repos")
    if not isinstance(repos, dict) or tuple(repos) != REPO_ORDER:
        # JSON object order is intentional here: it makes the fixed matrix
        # visible in review and avoids silently adding an unmeasured project.
        if not isinstance(repos, dict) or set(repos) != set(REPO_ORDER):
            raise ValueError(f"baseline repos must be exactly: {', '.join(REPO_ORDER)}")
    for name in REPO_ORDER:
        config = repos[name]
        expected = FIXED_REPOSITORIES[name]
        if not isinstance(config, dict) or config.get("kind") != expected["kind"]:
            raise ValueError(f"baseline repository {name} has the wrong kind")
        if expected["kind"] == "git":
            if config.get("url") != expected["url"]:
                raise ValueError(f"baseline repository {name} URL is not the fixed public URL")
            sha = config.get("sha")
            if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-f]{40}", sha):
                raise ValueError(f"baseline repository {name} must carry a 40-character commit SHA")
        elif config.get("version") != SYNTHETIC_VERSION:
            raise ValueError(f"baseline synthetic repository must be {SYNTHETIC_VERSION}")


def load_baseline(path: Path) -> dict[str, Any]:
    with path.open(encoding="utf-8") as handle:
        payload = json.load(handle)
    if not isinstance(payload, dict):
        raise ValueError("baseline JSON must be an object")
    validate_baseline(payload)
    return payload


def git_clone_pinned(config: dict[str, Any], name: str, cache_dir: Path) -> Path:
    sha = str(config["sha"])
    # Keep the checkout basename equal to the baseline repo name because the
    # matrix CSV uses the root basename as its repository identity.
    destination = cache_dir / "repos" / sha / name
    if destination.is_dir():
        checked = run_command(["git", "-C", str(destination), "rev-parse", "HEAD"], timeout=30)
        shallow = run_command(["git", "-C", str(destination), "rev-parse", "--is-shallow-repository"], timeout=30)
        if checked.returncode == 0 and checked.stdout.strip() == sha and shallow.stdout.strip() == "true":
            return destination
        raise RuntimeError(f"cached repository is not the pinned shallow checkout: {destination}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    initialized = run_command(["git", "init", "--quiet", str(destination)], timeout=30)
    if initialized.returncode != 0:
        raise RuntimeError(f"git init failed for {name}: {initialized.stderr.strip()[:300]}")
    commands = [
        ["git", "-C", str(destination), "remote", "add", "origin", str(config["url"])],
        ["git", "-C", str(destination), "fetch", "--depth=1", "origin", sha],
        ["git", "-C", str(destination), "checkout", "--detach", "FETCH_HEAD"],
    ]
    for command in commands:
        completed = run_command(command, timeout=900)
        if completed.returncode != 0:
            raise RuntimeError(f"pinned clone failed for {name}: {completed.stderr.strip()[:500]}")
    checked = run_command(["git", "-C", str(destination), "rev-parse", "HEAD"], timeout=30)
    if checked.returncode != 0 or checked.stdout.strip() != sha:
        raise RuntimeError(f"pinned clone did not resolve {name} to {sha}")
    return destination


def ensure_synthetic_tree(cache_dir: Path) -> Path:
    root = cache_dir / "synthetic" / SYNTHETIC_VERSION / "synthetic-24k"
    marker = root.with_name(root.name + ".complete")
    root.parent.mkdir(parents=True, exist_ok=True)
    if marker.is_file():
        count = sum(1 for path in root.rglob("*") if path.is_file()) if root.is_dir() else 0
        if count == SYNTHETIC_FILE_COUNT and not (root / ".git").exists():
            return root
        raise RuntimeError(f"synthetic cache marker is stale: {root}")
    if root.exists() and not root.is_dir():
        raise RuntimeError(f"synthetic cache path is not a directory: {root}")
    root.mkdir(parents=True, exist_ok=True)
    existing = sum(1 for path in root.rglob("*") if path.is_file())
    if existing > SYNTHETIC_FILE_COUNT:
        raise RuntimeError(f"synthetic tree contains too many files: {root}")
    for index in range(existing, SYNTHETIC_FILE_COUNT):
        if index == 0:
            destination = root / "src" / "main.rs"
            destination.parent.mkdir(exist_ok=True)
            content = "fn main() { println!(\"synthetic\"); }\n"
        elif index == 1:
            destination = root / "Cargo.toml"
            content = "[package]\nname = \"synthetic-24k\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"
        else:
            directory = root / f"shard-{index // 256:03d}"
            directory.mkdir(exist_ok=True)
            destination = directory / f"file-{index:05d}.txt"
            content = f"synthetic file {index:05d}\n"
        destination.write_text(content, encoding="utf-8")
    marker.write_text("complete\n", encoding="utf-8")
    return root


def prepare_repositories(payload: dict[str, Any], names: list[str], cache_dir: Path) -> dict[str, Path]:
    paths: dict[str, Path] = {}
    for name in names:
        config = payload["repos"][name]
        if config["kind"] == "git":
            paths[name] = git_clone_pinned(config, name, cache_dir)
        else:
            paths[name] = ensure_synthetic_tree(cache_dir)
    return paths


def ensure_release_binary(repo_root: Path, requested: str | None) -> Path:
    binary = Path(requested).expanduser() if requested else Path(os.environ.get("AFT_BINARY", repo_root / "target" / "release" / "aft")).expanduser()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        completed = subprocess.run(
            ["cargo", "build", "--release", "-p", "agent-file-tools", "--quiet"],
            cwd=str(repo_root),
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            raise RuntimeError("release build failed")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"release AFT binary is not executable: {binary}")
    return binary.resolve()


def run_matrix_once(
    repo_root: Path,
    binary: Path,
    repo_paths: dict[str, Path],
    names: list[str],
    output_root: Path,
    run_number: int,
    budget_min: float,
) -> tuple[list[RunData], Path]:
    run_dir = output_root / f"run-{run_number}"
    run_dir.mkdir(parents=True, exist_ok=False)
    list_file = run_dir / "repos.txt"
    list_file.write_text("".join(f"{repo_paths[name]}\n" for name in names), encoding="utf-8")
    command = [
        "bash",
        str(repo_root / "scripts/telemetry/oss-matrix.sh"),
        str(list_file),
        "--aft-binary",
        str(binary),
        "--budget-min",
        str(budget_min),
        "--results-dir",
        str(run_dir),
        "--scratch",
        str(run_dir / "scratch"),
        "--date",
        f"run-{run_number}",
    ]
    log_file = run_dir / "runner.log"
    started = time.monotonic()
    with log_file.open("w", encoding="utf-8") as output:
        completed = subprocess.run(command, cwd=str(repo_root), stdout=output, stderr=subprocess.STDOUT, check=False)
    elapsed = time.monotonic() - started
    if completed.returncode != 0:
        raise RuntimeError(f"oss-matrix run {run_number} failed; see {log_file} ({elapsed:.1f}s)")
    csv_path = run_dir / f"oss-matrix-run-{run_number}.csv"
    if not csv_path.is_file():
        raise RuntimeError(f"oss-matrix run {run_number} did not produce {csv_path}")
    rows = read_run_csv(csv_path)
    by_repo = {row.repo: row for row in rows}
    missing = [name for name in names if name not in by_repo]
    if missing:
        raise RuntimeError(f"oss-matrix omitted repositories in run {run_number}: {', '.join(missing)}")
    print(f"run-{run_number}: {elapsed:.1f}s; CSV={csv_path}")
    return [by_repo[name] for name in names], csv_path


def best_run(runs: list[RunData]) -> RunData:
    ready = [run for run in runs if run.ready]
    if not ready:
        return runs[0]
    return min(ready, key=lambda run: sum(run.metrics.get(metric, 0.0) for metric in ("search_build_ready_ms", "callgraph_build_ready_ms")))


def metric_object(raw: Any, metric: str, value: float) -> dict[str, float]:
    if isinstance(raw, dict):
        _old, tolerance, floor = baseline_metric(raw, metric)
    else:
        tolerance, floor = DEFAULT_METRICS.get(metric, WAITING_DEFAULT)
    return {"value": round(value, 3), "tolerance_pct": tolerance, "absolute_floor": floor}


def write_baseline(
    path: Path,
    old_payload: dict[str, Any],
    names: list[str],
    runs_by_repo: dict[str, list[RunData]],
    binary: Path,
    repo_root: Path,
) -> None:
    payload = json.loads(json.dumps(old_payload))
    payload["schema"] = SCHEMA_VERSION
    payload["generated_at"] = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    payload["binary"] = {
        "sha256": sha256(binary),
        "source_commit": run_command(["git", "rev-parse", "HEAD"], cwd=repo_root, timeout=30).stdout.strip(),
        "date": date.today().isoformat(),
    }
    for name in names:
        runs = runs_by_repo[name]
        ready = [run for run in runs if run.ready]
        if not ready:
            raise RuntimeError(f"cannot write baseline: {name} was not ready in either run")
        config = payload["repos"][name]
        old_metrics = config.get("metrics", {}) if isinstance(config.get("metrics", {}), dict) else {}
        all_metrics = set(old_metrics)
        for run in ready:
            all_metrics.update(run.metrics)
        # Every fixed repo must carry every core metric, even if a future runner
        # emitted a gap.  Baseline generation fails rather than blessing n/a.
        for metric in DEFAULT_METRICS:
            if not all(metric in run.metrics for run in ready):
                raise RuntimeError(f"cannot write baseline: {name} missing {metric}")
            all_metrics.add(metric)
        metrics: dict[str, Any] = {}
        for metric in sorted(all_metrics):
            values = [run.metrics.get(metric) for run in ready]
            values = [value for value in values if value is not None]
            if not values:
                continue
            metrics[metric] = metric_object(old_metrics.get(metric), metric, min(values))
        config["metrics"] = metrics
        config["index_events"] = best_run(runs).events
        config["sample"] = {"runs": 2, "selection": "minimum observed value per metric"}
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"wrote baseline {path}")


def print_regressions(repo: str, regressions: list[Regression], baseline_repo: dict[str, Any]) -> None:
    for regression in regressions:
        observed_events = regression.run.events if regression.run else {}
        if regression.baseline is None or regression.observed is None:
            baseline = "n/a" if regression.baseline is None else f"{regression.baseline:g}"
            print(
                f"REGRESSION {repo} {regression.metric}: "
                f"baseline={baseline} observed=n/a ({regression.reason})",
                file=sys.stderr,
            )
        else:
            print(
                f"REGRESSION {repo} {regression.metric}: "
                f"baseline={regression.baseline:g} observed={regression.observed:g} limit={regression.limit:g}",
                file=sys.stderr,
            )
        for key, before, after, delta in event_deltas(baseline_repo.get("index_events", {}), observed_events):
            print(f"  index_event delta {key}: baseline={before:g} observed={after:g} delta={delta:+g}", file=sys.stderr)


def self_test() -> int:
    """Test CSV extraction, two-run minimum selection, tolerances, and floors."""
    temporary = Path(tempfile.mkdtemp(prefix="aft-cost-gate-self-test-"))
    try:
        csv_path = temporary / "synthetic.csv"
        fields = [
            "repo", "outcome", "search_wall_ms", "callgraph_wall_ms",
            "callgraph_resolution_share_pct", "peak_rss_mb", "cpu_s",
            "search_first_query_ms", "callgraph_first_query_ms", "waiting_on", "log_path",
        ]
        rows = [
            {
                "repo": "fixture", "outcome": "ready", "search_wall_ms": "1/140/140",
                "callgraph_wall_ms": "1/126/126", "callgraph_resolution_share_pct": "1/11/11",
                "peak_rss_mb": "119", "cpu_s": "13", "search_first_query_ms": "1/126/126",
                "callgraph_first_query_ms": "1/90/90", "waiting_on": "build=1", "log_path": "",
            },
            {
                "repo": "fixture", "outcome": "ready", "search_wall_ms": "1/130/130",
                "callgraph_wall_ms": "1/126/126", "callgraph_resolution_share_pct": "1/11/11",
                "peak_rss_mb": "118", "cpu_s": "12", "search_first_query_ms": "1/127/127",
                "callgraph_first_query_ms": "1/91/91", "waiting_on": "build=1", "log_path": "",
            },
        ]
        with csv_path.open("w", encoding="utf-8", newline="") as handle:
            writer = csv.DictWriter(handle, fieldnames=fields)
            writer.writeheader()
            writer.writerows(rows)
        baseline = {
            "metrics": {
                "search_build_ready_ms": {"value": 100, "tolerance_pct": 25, "absolute_floor": 130},
                "callgraph_build_ready_ms": {"value": 100, "tolerance_pct": 25, "absolute_floor": 1},
                "callgraph_resolution_share_pct": {"value": 10, "tolerance_pct": 20, "absolute_floor": 5},
                "peak_rss_mb": {"value": 100, "tolerance_pct": 20, "absolute_floor": 1},
                "cpu_seconds": {"value": 10, "tolerance_pct": 25, "absolute_floor": 1},
                "search_first_query_ms": {"value": 100, "tolerance_pct": 25, "absolute_floor": 1},
                "callgraph_first_query_ms": {"value": 100, "tolerance_pct": 25, "absolute_floor": 1},
                "waiting_on.build": {"value": 0, "tolerance_pct": 25, "absolute_floor": 1},
                "waiting_on.total": {"value": 0, "tolerance_pct": 25, "absolute_floor": 0},
            },
            "index_events": {},
        }
        baseline_path = temporary / "baseline.json"
        baseline_path.write_text(json.dumps(baseline), encoding="utf-8")
        baseline_from_file = json.loads(baseline_path.read_text(encoding="utf-8"))
        runs = read_run_csv(csv_path)
        failures = compare_repo("fixture", baseline_from_file, runs)
        failed_names = {failure.metric for failure in failures}
        expected = {"callgraph_build_ready_ms", "search_first_query_ms", "waiting_on.total"}
        assert failed_names == expected, (failed_names, expected)
        # The minimum of 140 and 130 is exactly the relative limit, so this
        # proves that a slower first run does not page the gate.
        assert all(failure.metric != "search_build_ready_ms" for failure in failures)
        print("cost-gate self-test metrics: pass=6 fail=3")
        print("cost-gate self-test passed")
        return 0
    finally:
        shutil.rmtree(temporary, ignore_errors=True)


def parse_args() -> argparse.Namespace:
    script_dir = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, default=script_dir / "cost-baselines.json")
    parser.add_argument("--cache-dir", type=Path, default=Path(os.environ.get("AFT_COST_GATE_CACHE_DIR", Path.home() / ".cache" / "aft-cost-gate")))
    parser.add_argument("--results-dir", type=Path, default=Path(os.environ.get("AFT_COST_GATE_RESULTS_DIR", tempfile.gettempdir())) / "aft-cost-gate-results")
    parser.add_argument("--aft-binary", help="Release binary (default: $AFT_BINARY or target/release/aft)")
    parser.add_argument("--budget-min", type=float, default=float(os.environ.get("AFT_COST_GATE_BUDGET_MIN", "45")))
    parser.add_argument("--repo", choices=REPO_ORDER, help="Run and compare one fixed repository")
    parser.add_argument("--write-baseline", action="store_true", help="Bless the two-run minimum as the committed baseline")
    parser.add_argument("--self-test", action="store_true", help="Run the synthetic comparator self-test")
    args = parser.parse_args()
    if args.budget_min <= 0:
        parser.error("--budget-min must be greater than zero")
    return args


def main() -> int:
    args = parse_args()
    if args.self_test:
        return self_test()
    repo_root = Path(__file__).resolve().parents[2]
    baseline = load_baseline(args.baseline.resolve())
    names = [args.repo] if args.repo else list(REPO_ORDER)
    cache_dir = args.cache_dir.expanduser().resolve()
    results_dir = args.results_dir.expanduser().resolve()
    cache_dir.mkdir(parents=True, exist_ok=True)
    results_dir.mkdir(parents=True, exist_ok=True)
    binary = ensure_release_binary(repo_root, args.aft_binary)
    print(f"release binary: {binary} sha256={sha256(binary)}")
    repo_paths = prepare_repositories(baseline, names, cache_dir)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_root = results_dir / f"cost-gate-{stamp}"
    output_root.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    first, _ = run_matrix_once(repo_root, binary, repo_paths, names, output_root, 1, args.budget_min)
    second, _ = run_matrix_once(repo_root, binary, repo_paths, names, output_root, 2, args.budget_min)
    all_elapsed = time.monotonic() - started
    runs_by_repo = {name: [first[index], second[index]] for index, name in enumerate(names)}
    print(f"cost-gate wall time: {all_elapsed:.1f}s")
    print(f"artifacts: {output_root}")
    if args.write_baseline:
        write_baseline(args.baseline.resolve(), baseline, names, runs_by_repo, binary, repo_root)
        return 0
    failed = False
    for name in names:
        regressions = compare_repo(name, baseline["repos"][name], runs_by_repo[name])
        if regressions:
            failed = True
            print_regressions(name, regressions, baseline["repos"][name])
        else:
            print(f"PASS {name}: two-run minimum is within baseline")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"cost-gate: {error}", file=sys.stderr)
        raise SystemExit(1)
