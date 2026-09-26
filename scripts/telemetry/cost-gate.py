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
import platform
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
    # The resolution stage's own duration, not its share of the build. A share
    # rises when resolution slows down and equally when the stages around it
    # speed up, so a gate on it pages on an extraction speed-up as if it were a
    # resolution regression. The floor is lower than the other timings because
    # this interval is measured inside the process and carries no startup or
    # first-poll cost; it only has to keep a stage of a few milliseconds (the
    # synthetic tree) from paging on one tick.
    "callgraph_resolution_ms": (20.0, 500.0),
    "peak_rss_mb": (20.0, 128.0),
    "cpu_seconds": (25.0, 1.0),
    "search_first_query_ms": (25.0, 3_000.0),
    "callgraph_first_query_ms": (25.0, 3_000.0),
}
WAITING_DEFAULT = (25.0, 1.0)
# Peak RSS, CPU time and wall time are all properties of the machine that runs
# the matrix, not only of the code under test.  A baseline captured on one
# platform cannot bound another: the first seventeen scheduled runs compared a
# macOS arm64 capture against a Linux x86_64 runner and reported the difference
# between the two machines as a code regression every night.
PLATFORM_KEY = "measured_on"
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
    # A tolerance regression says a number grew. A bimodal finding says the two
    # runs disagreed about which state the metric was in, which is a different
    # statement and must not be printed as if it were the first one.
    kind: str = "tolerance"


@dataclass
class BimodalFinding:
    metric: str
    band: tuple[float, float]
    values: list[float]
    verdict: str
    evidence: str


# A metric measured into two separated states, with the band between them
# recorded as empty because no observation has ever landed in it.
#
# A tolerance is the wrong instrument for this shape: it is a band around a
# centre, and a metric with two states has no centre to scatter around. The
# two-run minimum is wrong for it as well -- it exists so one slow scheduler
# interval does not page anyone, and it turns "we saw both states" into a pass
# that reports only the lower one. So the states are named here and compared
# directly, and a run that reaches the high state fails whether or not the high
# state happens to fit under the tolerance that night.
#
# An entry is a claim about measurements, so it carries the measurements. Delete
# it when the metric stops having two states; the gate reports an observation
# inside the band as a falsified claim rather than quietly re-classifying it.
BIMODAL_BANDS: dict[tuple[str, str], tuple[float, float, str]] = {}


def classify_bimodal(repo: str, runs: list[RunData]) -> list[BimodalFinding]:
    """Say which recorded state each run landed in, for every declared band."""
    findings: list[BimodalFinding] = []
    for (band_repo, metric), (low_edge, high_edge, evidence) in sorted(BIMODAL_BANDS.items()):
        if band_repo != repo:
            continue
        values = [value for value in (run.metrics.get(metric) for run in runs if run.ready) if value is not None]
        if not values:
            continue
        if any(low_edge < value < high_edge for value in values):
            verdict = "inside"
        elif all(value <= low_edge for value in values):
            verdict = "low"
        elif all(value >= high_edge for value in values):
            verdict = "high"
        else:
            verdict = "straddle"
        findings.append(BimodalFinding(metric, (low_edge, high_edge), values, verdict, evidence))
    return findings


BIMODAL_REASONS = {
    "straddle": (
        "the two runs landed in different recorded states, so the two-run minimum would "
        "report the low one and pass on a metric that was also observed high"
    ),
    "high": "every run reached the high state, which the baseline does not describe",
    "inside": (
        "an observation landed inside a band recorded as empty, so the recorded states no "
        "longer describe this metric and must be re-derived before it can gate"
    ),
}


def bimodal_line(repo: str, finding: BimodalFinding) -> str:
    values = "/".join(f"{value:g}" for value in finding.values)
    low_edge, high_edge = finding.band
    if finding.verdict == "low":
        detail = (
            "every run is in the low state; this is not evidence that the high state is gone, "
            f"only that neither run reached it (recorded high state starts at {high_edge:g})"
        )
    else:
        detail = BIMODAL_REASONS[finding.verdict]
    return (
        f"BIMODAL {repo} {finding.metric}: runs {values} against recorded states "
        f"low<={low_edge:g} high>={high_edge:g} -> {finding.verdict}; {detail}"
    )


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
        "callgraph_resolution_ms": "callgraph_resolution_ms",
        "search_first_query_ms": "search_first_query_ms",
        "callgraph_first_query_ms": "callgraph_first_query_ms",
        "peak_rss_mb": "peak_rss_mb",
        "cpu_seconds": "cpu_s",
    }
    metrics: dict[str, float] = {}
    for metric, field in fields.items():
        parsed = compact_p50(row.get(field)) if field.endswith("_ms") else scalar(row.get(field))
        if parsed is not None:
            metrics[metric] = parsed
    if "callgraph_resolution_ms" not in metrics and "callgraph_resolution_ms" not in row:
        derived = resolution_ms_from_share(row)
        if derived is not None:
            metrics["callgraph_resolution_ms"] = derived
    metrics.update(waiting_metrics(row.get("waiting_on")))
    return metrics


def compact_count(value: str | None) -> int | None:
    """The observation count of an ``n/p50/max`` cell."""
    if not value:
        return None
    head = value.strip().split("/", 1)[0]
    return int(head) if head.isdigit() else None


def resolution_ms_from_share(row: dict[str, str]) -> float | None:
    """Recover the resolution duration from a CSV written before it had a column.

    Results captured before the matrix recorded ``callgraph_resolution_ms``
    still carry the build time and the resolution share, and blessing a
    baseline from an already-uploaded workflow artifact needs those results to
    be readable. The product of the two is the duration only when each cell
    describes the same single build; with several builds the two medians can
    come from different builds, so nothing is derived.
    """
    wall_cell = row.get("callgraph_wall_ms")
    share_cell = row.get("callgraph_resolution_share_pct")
    if compact_count(wall_cell) != 1 or compact_count(share_cell) != 1:
        return None
    wall = compact_p50(wall_cell)
    share = compact_p50(share_cell)
    if wall is None or share is None:
        return None
    return float(round(wall * share / 100.0))


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


def current_platform() -> str:
    """The identity a baseline must carry to be comparable with this run."""
    return f"{platform.system().lower()}-{platform.machine()}"


def metric_is_gated(value: Any) -> bool:
    """False for a metric whose value is recorded but deliberately not compared."""
    return not (isinstance(value, dict) and value.get("gated") is False)


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
    # With no recorded counts to compare against, every observed event would
    # print as a change from zero and read as a regression diagnosis.
    if not baseline:
        return []
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
        # A metric can be recorded without being compared.  The alternative --
        # dropping it from the baseline -- loses the observation as well as the
        # comparison, and hides that a deliberate decision was made.
        if not metric_is_gated(raw_baseline):
            continue
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
    for finding in classify_bimodal(repo, ready_runs):
        if finding.verdict == "low":
            continue
        regressions.append(Regression(
            finding.metric,
            None,
            max(finding.values),
            None,
            BIMODAL_REASONS[finding.verdict],
            ready_runs[0],
            kind="bimodal",
        ))
    return regressions


def validate_baseline(payload: dict[str, Any]) -> None:
    if payload.get("schema") != SCHEMA_VERSION:
        raise ValueError(f"baseline schema must be {SCHEMA_VERSION}")
    measured_on = payload.get(PLATFORM_KEY)
    if not isinstance(measured_on, dict) or not isinstance(measured_on.get("platform"), str) or not measured_on["platform"]:
        raise ValueError(f"baseline must record {PLATFORM_KEY}.platform, the platform string it was captured on")
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
        for metric, raw in (config.get("metrics") or {}).items():
            # Leaving a metric out of the gate is a decision someone has to be
            # able to read back, so the reason travels with the exclusion.
            if not metric_is_gated(raw) and not str(raw.get("ungated_reason", "")).strip():
                raise ValueError(f"baseline metric {name}.{metric} is ungated without an ungated_reason")


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
    written: dict[str, Any] = {"value": round(value, 3), "tolerance_pct": tolerance, "absolute_floor": floor}
    # A re-capture records a fresh number for an ungated metric but must not
    # quietly put it back under the gate: the reason it was excluded is about
    # the metric, not about the values of any one capture.
    if isinstance(raw, dict) and raw.get("gated") is False:
        written["gated"] = False
        written["ungated_reason"] = raw.get("ungated_reason", "")
    return written


def repo_metrics_from_runs(name: str, config: dict[str, Any], runs: list[RunData]) -> dict[str, Any]:
    """The two-run minimum per metric, in the shape the baseline stores."""
    ready = [run for run in runs if run.ready]
    if not ready:
        raise RuntimeError(f"cannot write baseline: {name} was not ready in either run")
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
        values = [value for value in (run.metrics.get(metric) for run in ready) if value is not None]
        if not values:
            continue
        metrics[metric] = metric_object(old_metrics.get(metric), metric, min(values))
    return metrics


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
    payload[PLATFORM_KEY] = {
        "platform": current_platform(),
        "provenance": f"local --write-baseline on {current_platform()}",
    }
    for name in names:
        runs = runs_by_repo[name]
        config = payload["repos"][name]
        config["metrics"] = repo_metrics_from_runs(name, config, runs)
        config["index_events"] = best_run(runs).events
        config["sample"] = {"runs": 2, "selection": "minimum observed value per metric"}
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"wrote baseline {path}")


def write_baseline_from_results(
    path: Path,
    old_payload: dict[str, Any],
    results_dir: Path,
    measured_on: str,
    provenance: str,
) -> int:
    """Bless a matrix that already ran elsewhere, from its uploaded CSVs.

    The scheduled workflow measures on its own runner and a developer machine
    cannot reproduce those numbers, so the blessing path has to be able to take
    the runner's own results rather than re-measuring here.
    """
    csv_paths = sorted(results_dir.rglob("oss-matrix-run-*.csv"))
    if len(csv_paths) != 2:
        raise RuntimeError(f"expected exactly two oss-matrix-run-*.csv files under {results_dir}, found {len(csv_paths)}")
    runs_by_repo: dict[str, list[RunData]] = defaultdict(list)
    for csv_path in csv_paths:
        for run in read_run_csv(csv_path):
            runs_by_repo[run.repo].append(run)
    incomplete = [name for name in REPO_ORDER if len(runs_by_repo.get(name, [])) != 2]
    if incomplete:
        raise RuntimeError(f"results are not a complete two-run matrix; incomplete: {', '.join(incomplete)}")
    payload = json.loads(json.dumps(old_payload))
    payload["schema"] = SCHEMA_VERSION
    payload["generated_at"] = datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    payload[PLATFORM_KEY] = {"platform": measured_on, "provenance": provenance}
    for name in REPO_ORDER:
        config = payload["repos"][name]
        config["metrics"] = repo_metrics_from_runs(name, config, runs_by_repo[name])
        # Event counts are parsed from the per-repo AFT logs, which are only
        # present when the artifact carried them.  Keeping another capture's
        # counts here would make the regression diagnostics describe a run that
        # never happened, so an absent log set records as absent.
        events = best_run(runs_by_repo[name]).events
        config["index_events"] = events
        config["sample"] = {"runs": 2, "selection": "minimum observed value per metric"}
        if not events:
            print(f"note: {name} index_event counts are not in these results; event deltas will be omitted until the next capture")
    validate_baseline(payload)
    path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n", encoding="utf-8")
    print(f"wrote baseline {path} from {results_dir} (measured on {measured_on})")
    return 0


def runner_facts() -> str:
    """Describe the machine this run measured on, for the workflow log.

    Peak RSS and CPU seconds depend on how many cores the process found: the
    callgraph cold build sizes its parse pool from the core count, so two hosts
    with different core counts produce different numbers from identical code.
    The runner image version is printed by the job, but the core count and
    memory are not, which leaves the question unanswerable after the fact.
    """
    cpus: str
    try:
        # The scheduler affinity mask, not the machine's core count: a cgroup or
        # affinity-restricted job gets fewer cores than the host advertises, and
        # the restricted number is the one the pool sizing sees.
        cpus = str(len(os.sched_getaffinity(0)))  # type: ignore[attr-defined]
    except (AttributeError, OSError):
        cpus = str(os.cpu_count() or "unknown")
    memory = "unknown"
    try:
        for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                memory = f"{int(line.split()[1]) / 1024 / 1024:.1f} GiB"
                break
    except (OSError, ValueError, IndexError):
        pass
    return f"runner: platform={current_platform()} cpus={cpus} memory={memory}"


def observation_line(repo: str, runs: list[RunData]) -> str:
    """Render what both runs measured, whether or not the gate is about to fail.

    Printing values only beside a regression censors the record exactly where a
    noise question needs it: the nights that passed are the nights whose numbers
    are missing, so no run-to-run spread can be reconstructed from the logs, and
    answering "is this step larger than the usual scatter?" means downloading
    half a gigabyte of artifacts per night. Both runs are shown rather than the
    compared minimum, because the gap between them is the within-night spread.
    """
    metrics = sorted({metric for run in runs if run.ready for metric in run.metrics})
    if not metrics:
        return f"OBSERVED {repo}: no ready run"
    parts: list[str] = []
    for metric in metrics:
        values = []
        for run in runs:
            value = run.metrics.get(metric) if run.ready else None
            values.append("n/a" if value is None else f"{value:g}")
        parts.append(f"{metric}={'/'.join(values)}")
    # Read straight from the CSV row rather than through row_metrics, so this
    # stays a recorded observation: anything row_metrics returns is eligible to
    # be written into a future baseline by repo_metrics_from_runs, and would
    # then silently acquire a tolerance and start gating.
    hwm = [run.row.get("peak_rss_hwm_mb", "n/a") or "n/a" for run in runs if run.ready]
    if any(value != "n/a" for value in hwm):
        parts.append(f"peak_rss_hwm_mb(ungated)={'/'.join(hwm)}")
    # The resolution share stays visible as a description of where build time
    # went, but it is not a budget: it cannot tell a slower resolution stage
    # from faster stages around it.
    shares = [compact_p50(run.row.get("callgraph_resolution_share_pct")) for run in runs if run.ready]
    if any(value is not None for value in shares):
        rendered = "/".join("n/a" if value is None else f"{value:g}" for value in shares)
        parts.append(f"callgraph_resolution_share_pct(ungated)={rendered}")
    return f"OBSERVED {repo}: " + " ".join(parts)


def phase_line(repo: str, runs: list[RunData]) -> str | None:
    """Where each run's high-water mark was taken, phase by phase.

    A peak RSS number says how much a run used; it cannot say which part of the
    build used it, and the difference between two runs of one binary is exactly
    that second question. The harness charges each step of the kernel
    high-water mark to the phase that took it, so printing the breakdown beside
    the values turns "these two runs disagree" into "they disagree here"
    without downloading half a gigabyte of artifacts.

    Read straight from the CSV row, like the high-water mark itself: anything
    row_metrics returns can be written into a future baseline and would then
    acquire a tolerance, and this is a diagnostic, not a budget.
    """
    values = [run.row.get("hwm_by_phase", "") or "n/a" for run in runs if run.ready]
    if not values or all(value == "n/a" for value in values):
        return None
    return f"PHASES {repo}: " + " | ".join(values)


def print_regressions(repo: str, regressions: list[Regression], baseline_repo: dict[str, Any]) -> None:
    for regression in regressions:
        observed_events = regression.run.events if regression.run else {}
        if regression.kind == "bimodal":
            observed = "n/a" if regression.observed is None else f"{regression.observed:g}"
            print(
                f"REGRESSION {repo} {regression.metric}: highest observation={observed} "
                f"({regression.reason}); see the BIMODAL line for both runs and the recorded states",
                file=sys.stderr,
            )
        elif regression.baseline is None or regression.observed is None:
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


def self_test_baseline() -> dict[str, Any]:
    """The smallest payload validate_baseline accepts, for the rejection cases."""
    repos: dict[str, Any] = {}
    for name in REPO_ORDER:
        expected = FIXED_REPOSITORIES[name]
        config: dict[str, Any] = {"kind": expected["kind"], "metrics": {}}
        if expected["kind"] == "git":
            config.update({"url": expected["url"], "sha": "0" * 40})
        else:
            config["version"] = SYNTHETIC_VERSION
        repos[name] = config
    return {"schema": SCHEMA_VERSION, PLATFORM_KEY: {"platform": "linux-x86_64"}, "repos": repos}


def assert_rejected(payload: dict[str, Any], fragment: str) -> None:
    """Fail unless validate_baseline refuses the payload and says why."""
    try:
        validate_baseline(payload)
    except ValueError as error:
        assert fragment in str(error), (fragment, str(error))
        return
    raise AssertionError(f"validate_baseline accepted a payload it must reject ({fragment})")


def self_test() -> int:
    """Test CSV extraction, two-run minimum selection, tolerances, and floors."""
    temporary = Path(tempfile.mkdtemp(prefix="aft-cost-gate-self-test-"))
    declared_bands = dict(BIMODAL_BANDS)
    try:
        csv_path = temporary / "synthetic.csv"
        fields = [
            "repo", "outcome", "search_wall_ms", "callgraph_wall_ms",
            "callgraph_resolution_share_pct", "callgraph_resolution_ms", "peak_rss_mb", "peak_rss_hwm_mb", "hwm_by_phase", "cpu_s",
            "search_first_query_ms", "callgraph_first_query_ms", "waiting_on", "log_path",
        ]
        rows = [
            {
                "repo": "fixture", "outcome": "ready", "search_wall_ms": "1/140/140",
                "callgraph_wall_ms": "1/126/126", "callgraph_resolution_share_pct": "1/11/11",
                "callgraph_resolution_ms": "1/110/110", "peak_rss_mb": "119", "peak_rss_hwm_mb": "171", "hwm_by_phase": "extraction/ready=+80.0",
                "cpu_s": "13", "search_first_query_ms": "1/126/126",
                "callgraph_first_query_ms": "1/90/90", "waiting_on": "build=1", "log_path": "",
            },
            {
                "repo": "fixture", "outcome": "ready", "search_wall_ms": "1/130/130",
                "callgraph_wall_ms": "1/126/126", "callgraph_resolution_share_pct": "1/11/11",
                "callgraph_resolution_ms": "1/110/110", "peak_rss_mb": "118", "peak_rss_hwm_mb": "170", "hwm_by_phase": "extraction/ready=+60.0",
                "cpu_s": "12", "search_first_query_ms": "1/127/127",
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
                "callgraph_resolution_ms": {"value": 100, "tolerance_pct": 20, "absolute_floor": 1},
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

        # A metric marked ungated keeps its recorded value but is not compared.
        # callgraph_build_ready_ms is deliberately one of the three that failed
        # above, so an ignored exclusion would show up as a failure again.
        ungated = json.loads(json.dumps(baseline_from_file))
        ungated["metrics"]["callgraph_build_ready_ms"]["gated"] = False
        ungated["metrics"]["callgraph_build_ready_ms"]["ungated_reason"] = "self-test fixture"
        still_failing = {failure.metric for failure in compare_repo("fixture", ungated, runs)}
        assert still_failing == expected - {"callgraph_build_ready_ms"}, still_failing

        # Whole-file rules: a baseline must say which platform measured it, and
        # must not drop a metric from the gate without writing down why.
        validate_baseline(self_test_baseline())
        no_platform = self_test_baseline()
        del no_platform[PLATFORM_KEY]
        assert_rejected(no_platform, "platform")
        silent_exclusion = self_test_baseline()
        silent_exclusion["repos"]["redox"]["metrics"]["peak_rss_mb"] = {
            "value": 1, "tolerance_pct": 20, "absolute_floor": 1, "gated": False,
        }
        assert_rejected(silent_exclusion, "ungated_reason")

        # Event deltas are diagnostics printed beside a regression.  With no
        # recorded counts to compare against, every observed event would print
        # as a change from zero and read as part of the diagnosis.
        assert event_deltas({}, {"callgraph.build_ready.count": 1.0}) == []
        assert event_deltas({"callgraph.build_ready.count": 0.0}, {"callgraph.build_ready.count": 1.0})

        # A night that passes must still record what it measured.  Both runs
        # appear, because the distance between them is the within-night spread
        # that any "is this noise?" question is asked against.
        line = observation_line("fixture", runs)
        assert line.startswith("OBSERVED fixture: "), line
        assert "peak_rss_mb=119/118" in line, line
        assert "cpu_seconds=13/12" in line, line
        assert observation_line("fixture", []) == "OBSERVED fixture: no ready run"
        # The high-water mark is recorded beside the sampled peak, and must stay
        # out of the gate: it has no baseline and must not acquire one.
        assert "peak_rss_hwm_mb(ungated)=171/170" in line, line
        blessed = repo_metrics_from_runs("fixture", baseline_from_file, runs)
        assert "peak_rss_hwm_mb" not in blessed, sorted(blessed)
        # The per-phase breakdown is a diagnostic beside the values, and like
        # the high-water mark it must not become a budget.
        assert phase_line("fixture", runs) == "PHASES fixture: extraction/ready=+80.0 | extraction/ready=+60.0"
        assert "hwm_by_phase" not in blessed, sorted(blessed)
        assert phase_line("fixture", []) is None

        # Resolution is gated on its own duration. The numbers are hugo's cold
        # callgraph build on the nightly runner before and after cold-build
        # extraction became cheaper (runs 35833321498 and 36227460581):
        # extraction fell from 22.2 s to 8.9 s while resolution stayed at
        # about 15.3 s, which lifted resolution's share of the build from 39.5%
        # to 60.2% with no change in resolution itself.
        hugo_baseline = {
            "metrics": {
                "callgraph_build_ready_ms": {"value": 38704, "tolerance_pct": 25, "absolute_floor": 3000},
                "callgraph_resolution_ms": {"value": 15306, "tolerance_pct": 20, "absolute_floor": 500},
            },
            "index_events": {},
        }
        resolution_fields = ["repo", "outcome", "callgraph_wall_ms", "callgraph_resolution_share_pct", "callgraph_resolution_ms"]

        def hugo_runs(name: str, rows: list[tuple[int, int]], *, with_column: bool = True) -> list[RunData]:
            path = temporary / f"{name}.csv"
            fields = resolution_fields if with_column else resolution_fields[:-1]
            with path.open("w", encoding="utf-8", newline="") as handle:
                writer = csv.DictWriter(handle, fieldnames=fields)
                writer.writeheader()
                for wall, resolution in rows:
                    share = 100.0 * resolution / wall
                    row = {
                        "repo": "hugo", "outcome": "ready",
                        "callgraph_wall_ms": f"1/{wall}/{wall}",
                        "callgraph_resolution_share_pct": f"1/{share:.3f}/{share:.3f}",
                        "callgraph_resolution_ms": f"1/{resolution}/{resolution}",
                    }
                    writer.writerow({key: row[key] for key in fields})
            return read_run_csv(path)

        # Faster extraction, unchanged resolution: nothing to report, although
        # the share went past the 47.43% limit the share gate used to page at.
        faster_extraction = hugo_runs("faster-extraction", [(25667, 15394), (25485, 15354)])
        assert compare_repo("hugo", hugo_baseline, faster_extraction) == []
        assert min(compact_p50(run.row["callgraph_resolution_share_pct"]) for run in faster_extraction) > 39.523 * 1.2
        # A resolution stage 27% slower with extraction untouched. The whole
        # build grows by only 11%, inside its own 25% band, so without a
        # resolution budget of its own this slowdown would pass.
        slower_resolution = hugo_runs("slower-resolution", [(42898, 19500), (42900, 19480)])
        assert [failure.metric for failure in compare_repo("hugo", hugo_baseline, slower_resolution)] == [
            "callgraph_resolution_ms"
        ]
        # Results uploaded before the matrix wrote the duration column still
        # yield it, from one build's time and share, so they can be blessed.
        legacy = hugo_runs("legacy", [(25485, 15354)], with_column=False)
        assert legacy[0].metrics["callgraph_resolution_ms"] == 15354, legacy[0].metrics
        # With several builds the two medians need not describe the same build.
        assert resolution_ms_from_share({"callgraph_wall_ms": "2/25485/25667", "callgraph_resolution_share_pct": "2/60.0/60.2"}) is None
        # A present but empty column is a measurement gap, not a legacy file.
        assert "callgraph_resolution_ms" not in row_metrics({
            "callgraph_wall_ms": "1/25485/25485", "callgraph_resolution_share_pct": "1/60.247/60.247",
            "callgraph_resolution_ms": "0/n/a/n/a",
        })
        # The share is still printed, marked as outside the gate.
        assert "callgraph_resolution_share_pct(ungated)=59.976/60.247" in observation_line("hugo", faster_extraction)

        # A metric with two recorded states cannot be read off a two-run
        # minimum. The straddling pair here is a real night -- 597.9 and 678.1,
        # dispatch 35629011191 -- whose minimum sits inside every tolerance and
        # whose maximum is 82 MB above the baseline. That band was later
        # deleted from BIMODAL_BANDS because the metric lost its second state,
        # so it is installed here as a fixture: the rules have to keep working
        # while no band is declared.
        BIMODAL_BANDS[("jupyterlab", "peak_rss_mb")] = (606.1, 668.0, "self-test fixture")
        jupyterlab_baseline = {
            "metrics": {"peak_rss_mb": {"value": 596.3, "tolerance_pct": 20, "absolute_floor": 128.0}},
            "index_events": {},
        }

        def jupyterlab_runs(values: list[float]) -> list[RunData]:
            return [
                RunData(csv_path, "jupyterlab", {"peak_rss_mb": value}, {}, True, {"peak_rss_mb": f"{value}"})
                for value in values
            ]

        straddle = compare_repo("jupyterlab", jupyterlab_baseline, jupyterlab_runs([597.9, 678.1]))
        assert [failure.kind for failure in straddle] == ["bimodal"], straddle
        assert classify_bimodal("jupyterlab", jupyterlab_runs([597.9, 678.1]))[0].verdict == "straddle"
        # No tolerance was exceeded on that night: passing it is exactly the
        # flap this check exists to remove.
        assert all(failure.kind != "tolerance" for failure in straddle), straddle
        # Both runs high is not a pass either, even while the high state still
        # fits under the tolerance, because the baseline describes the low one.
        both_high = compare_repo("jupyterlab", jupyterlab_baseline, jupyterlab_runs([669.8, 701.2]))
        assert [failure.kind for failure in both_high] == ["bimodal"], both_high
        # Two low runs are the only pass, and the line says what it is a pass of.
        both_low = jupyterlab_runs([597.9, 598.0])
        assert compare_repo("jupyterlab", jupyterlab_baseline, both_low) == []
        low_line = bimodal_line("jupyterlab", classify_bimodal("jupyterlab", both_low)[0])
        assert "-> low" in low_line and "not evidence that the high state is gone" in low_line, low_line
        # An observation between the states falsifies the record rather than
        # picking a side, so it asks for the record to be re-derived.
        inside = compare_repo("jupyterlab", jupyterlab_baseline, jupyterlab_runs([598.0, 640.0]))
        assert [failure.reason for failure in inside] == [BIMODAL_REASONS["inside"]], inside
        # Only the declared repository and metric are classified this way.
        assert classify_bimodal("hugo", jupyterlab_runs([597.9, 678.1])) == []

        print("cost-gate self-test metrics: pass=6 fail=3")
        print("cost-gate self-test passed")
        return 0
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
        BIMODAL_BANDS.clear()
        BIMODAL_BANDS.update(declared_bands)


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
    parser.add_argument("--write-baseline-from", type=Path, help="Bless an already-measured results directory (a downloaded workflow artifact) instead of measuring here")
    parser.add_argument("--measured-on", help="Platform string those results were captured on, for example linux-x86_64 (required with --write-baseline-from)")
    parser.add_argument("--provenance", help="Where those results came from, recorded in the baseline (required with --write-baseline-from)")
    parser.add_argument("--ignore-platform", action="store_true", help="Compare against a baseline captured on another platform; the numbers are not comparable, so this never gates")
    parser.add_argument("--self-test", action="store_true", help="Run the synthetic comparator self-test")
    args = parser.parse_args()
    if args.budget_min <= 0:
        parser.error("--budget-min must be greater than zero")
    if args.write_baseline_from and not (args.measured_on and args.provenance):
        parser.error("--write-baseline-from requires --measured-on and --provenance")
    return args


def main() -> int:
    args = parse_args()
    if args.self_test:
        return self_test()
    repo_root = Path(__file__).resolve().parents[2]
    baseline = load_baseline(args.baseline.resolve())
    if args.write_baseline_from:
        return write_baseline_from_results(
            args.baseline.resolve(), baseline, args.write_baseline_from.expanduser().resolve(),
            args.measured_on, args.provenance,
        )
    # Peak RSS, CPU seconds and wall times are properties of the machine as much
    # as of the code.  Comparing across platforms does not produce a weaker
    # signal, it produces a wrong one, so say which two platforms disagree
    # instead of reporting the gap between them as a regression.
    recorded_platform = baseline[PLATFORM_KEY]["platform"]
    if recorded_platform != current_platform() and not args.ignore_platform:
        print(
            f"cost-gate: baseline was captured on {recorded_platform}; this run measures on {current_platform()}. "
            f"Re-capture on this platform, or pass --ignore-platform to measure without gating.",
            file=sys.stderr,
        )
        return 2
    names = [args.repo] if args.repo else list(REPO_ORDER)
    cache_dir = args.cache_dir.expanduser().resolve()
    results_dir = args.results_dir.expanduser().resolve()
    cache_dir.mkdir(parents=True, exist_ok=True)
    results_dir.mkdir(parents=True, exist_ok=True)
    binary = ensure_release_binary(repo_root, args.aft_binary)
    print(runner_facts())
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
    # Print before any comparison, so a run that regenerates the baseline and a
    # run whose numbers are within limits both record what they measured,
    # exactly as a run that exceeds them does.
    for name in names:
        print(observation_line(name, runs_by_repo[name]))
        phases = phase_line(name, runs_by_repo[name])
        if phases:
            print(phases)
        # Printed on every run, pass or fail: a row whose metric has two
        # recorded states cannot be read from a single verdict, and a green
        # night on it means only that neither run reached the high state.
        for finding in classify_bimodal(name, runs_by_repo[name]):
            print(bimodal_line(name, finding))
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
    if args.ignore_platform and recorded_platform != current_platform():
        print(f"cost-gate: measured on {current_platform()} against a {recorded_platform} baseline; not gating", file=sys.stderr)
        return 0
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"cost-gate: {error}", file=sys.stderr)
        raise SystemExit(1)
