#!/usr/bin/env python3
"""Run isolated standalone AFT cold builds over a diverse OSS repo matrix.

The shell entry point is ``oss-matrix.sh``. This helper owns NDJSON transport,
resource sampling, and result rendering so every repository gets the same
measurement path without relying on a plugin host.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import re
import select
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from datetime import date, datetime, timezone
from pathlib import Path
from typing import Any, Iterable

DEFAULT_REPOS = (
    "tmignore-rs", "rails", "laravel", "home-assistant", "nx", "mdn", "kotlin", "roslyn",
    "spark", "kubernetes", "elasticsearch", "llvm", "linux", "_nongit-24k", "chromium",
)
PLANES = ("search", "callgraph")
MEASUREMENT_PROFILE = "index-store-no-inspect-refresh-v4"
CSV_FIELDS: tuple[str, ...] = (
    "repo", "path", "git", "files", "top_languages", "semantic", "search_status",
    "callgraph_status", "index_store_build_ready_ms", "callgraph_store_build_ready_ms",
    "search_first_query_latency_ms", "callgraph_first_query_latency_ms", "callgraph_refresh_ms", "waiting_on",
    "peak_rss_mb", "peak_rss_sampled_mb", "peak_rss_hwm_mb", "hwm_by_phase", "cpu_s", "cpu_precision", "disk_write_bytes", "outcome", "log_path", "gaps", "measurement_profile",
)
TEXT_SUFFIXES = {
    ".c", ".cc", ".cpp", ".cs", ".go", ".h", ".hpp", ".java", ".js", ".jsx", ".kt",
    ".m", ".php", ".py", ".rb", ".rs", ".scala", ".sh", ".swift", ".ts", ".tsx", ".vue",
}
LANGUAGE_NAMES = {
    "c": "C", "cc": "C++", "cpp": "C++", "cs": "C#", "go": "Go", "h": "C/C++ header",
    "hpp": "C++ header", "java": "Java", "js": "JavaScript", "jsx": "JavaScript", "kt": "Kotlin",
    "m": "Objective-C", "php": "PHP", "py": "Python", "rb": "Ruby", "rs": "Rust",
    "scala": "Scala", "sh": "Shell", "swift": "Swift", "ts": "TypeScript", "tsx": "TSX", "vue": "Vue",
}
SYMBOL_RE = re.compile(
    r"(?:\b(?:async\s+)?(?:fn|function|def|func|class|struct|interface|enum)\s+|\b(?:public|private|protected|internal)\s+(?:static\s+)?(?:class|struct|interface|enum|(?:[A-Za-z_<>,?]+\s+)+))([A-Za-z_][A-Za-z0-9_]*)"
)
WORD_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]{2,}")


class BudgetExceeded(RuntimeError):
    pass


class NdjsonError(RuntimeError):
    pass


class NdjsonClient:
    """Minimal request/response client that ignores asynchronous standalone frames."""

    def __init__(self, binary: Path, root: Path, storage: Path, *, allow_non_git_callgraph: bool = False):
        env = os.environ.copy()
        env["AFT_STORAGE_DIR"] = str(storage)
        env.setdefault("RUST_LOG", "info")
        self.proc = subprocess.Popen(
            [str(binary)],
            cwd=str(root),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        self._next_id = 1
        self._buffer = b""

    def request(self, command: str, *, timeout_s: float = 30, **params: Any) -> dict[str, Any]:
        if self.proc.stdin is None or self.proc.stdout is None:
            raise NdjsonError("AFT stdin/stdout pipes are unavailable")
        request_id = str(self._next_id)
        self._next_id += 1
        payload = {"id": request_id, "command": command, **params}
        self.proc.stdin.write(json.dumps(payload, separators=(",", ":")).encode() + b"\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise NdjsonError(f"AFT exited before response {request_id} (exit {self.proc.returncode})")
            remaining = max(0.0, deadline - time.monotonic())
            readable, _, _ = select.select([self.proc.stdout], [], [], min(0.2, remaining))
            if not readable:
                continue
            chunk = os.read(self.proc.stdout.fileno(), 65536)
            if not chunk:
                raise NdjsonError(f"AFT closed stdout before response {request_id}")
            self._buffer += chunk
            while b"\n" in self._buffer:
                raw, self._buffer = self._buffer.split(b"\n", 1)
                try:
                    frame = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                if str(frame.get("id")) == request_id:
                    return frame
        raise NdjsonError(f"timed out waiting for {command}")

    def close(self) -> str | None:
        if self.proc.stdin is not None and not self.proc.stdin.closed:
            self.proc.stdin.close()
        try:
            self.proc.wait(timeout=10)
            return None
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
            return "AFT did not exit after stdin closed; terminated after 10s"


@dataclass
class ProcessSampler:
    proc: subprocess.Popen[bytes]
    peak_rss_kb: int = 0
    # The kernel's own high-water mark, recorded beside the sampled maximum.
    # peak_rss_kb is the largest value a two-second poll happened to see, so it
    # can only ever be at or below the true peak, and a short-lived spike
    # between two polls is invisible to it. VmHWM is exact and monotonic, which
    # makes the difference between the two a direct measure of how much the
    # poll missed. The gate uses this kernel peak, with the sampled maximum
    # retained as a diagnostic. Linux is the only platform this gate compares on.
    peak_rss_hwm_kb: int = 0
    hwm_available: bool = False
    cpu_seconds: float | None = None
    disk_write_bytes: int | None = None
    gaps: list[str] = field(default_factory=list)
    _stop: threading.Event = field(default_factory=threading.Event)
    _thread: threading.Thread | None = None

    @property
    def cpu_precision(self) -> str:
        return "0.01-second" if sys.platform.startswith("linux") else "whole-second"

    def _add_gap(self, message: str) -> None:
        if message not in self.gaps:
            self.gaps.append(message)

    def start(self) -> None:
        if sys.platform == "darwin":
            self.gaps.append("disk write bytes unavailable: macOS ps does not expose them")
        self._thread = threading.Thread(target=self._sample_loop, name="oss-matrix-resource-sampler", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=3)
        self._sample_once()

    def _sample_loop(self) -> None:
        self._sample_once()
        while not self._stop.wait(2):
            self._sample_once()

    def _sample_once(self) -> None:
        self.hwm_available = False
        if self.proc.poll() is not None:
            self._add_gap("CPU measurement gap: process exited before a sample")
            return
        try:
            completed = subprocess.run(
                ["ps", "-o", "rss=", "-o", "cputime=", "-p", str(self.proc.pid)],
                check=False,
                capture_output=True,
                text=True,
                timeout=2,
            )
            fields = completed.stdout.split()
            if len(fields) >= 2:
                self.peak_rss_kb = max(self.peak_rss_kb, int(fields[0]))
                if not sys.platform.startswith("linux"):
                    sampled_cpu = parse_cpu_time(fields[1])
                    self.cpu_seconds = max(self.cpu_seconds or 0.0, sampled_cpu)
            elif "ps resource sampling unavailable" not in self.gaps:
                self.gaps.append("ps resource sampling unavailable")
        except (OSError, ValueError, subprocess.SubprocessError):
            if "ps resource sampling unavailable" not in self.gaps:
                self.gaps.append("ps resource sampling unavailable")
        if sys.platform.startswith("linux"):
            try:
                stat = Path(f"/proc/{self.proc.pid}/stat").read_text(encoding="utf-8")
                sampled_cpu = parse_proc_stat_cpu_seconds(stat)
                self.cpu_seconds = max(self.cpu_seconds or 0.0, sampled_cpu)
            except (OSError, ValueError):
                self._add_gap("CPU measurement gap: /proc/<pid>/stat unavailable during sampling")
            try:
                for line in Path(f"/proc/{self.proc.pid}/status").read_text(encoding="utf-8").splitlines():
                    if line.startswith("VmHWM:"):
                        self.peak_rss_hwm_kb = max(self.peak_rss_hwm_kb, int(line.split()[1]))
                        self.hwm_available = True
                        break
                if not self.hwm_available:
                    self._add_gap("peak RSS high-water mark missing from /proc/<pid>/status")
            except (OSError, ValueError, IndexError):
                if "peak RSS high-water mark unavailable: /proc/<pid>/status" not in self.gaps:
                    self.gaps.append("peak RSS high-water mark unavailable: /proc/<pid>/status")
            try:
                for line in Path(f"/proc/{self.proc.pid}/io").read_text(encoding="utf-8").splitlines():
                    if line.startswith("write_bytes:"):
                        self.disk_write_bytes = max(self.disk_write_bytes or 0, int(line.split()[1]))
                        break
            except (OSError, ValueError):
                if "disk write bytes unavailable: /proc/<pid>/io" not in self.gaps:
                    self.gaps.append("disk write bytes unavailable: /proc/<pid>/io")


# Phases are sampled four times a second: fine enough to separate two builds
# that overlap for a second or two, cheap enough to be two small /proc reads.
PHASE_SAMPLE_INTERVAL_S = 0.25
PHASE_TRACE_FIELDS = ("elapsed_ms", "rss_kb", "hwm_kb", "callgraph_stage", "search_stage", "tier2_stage", "phase")
# The planes whose work can land inside a measured cold build. `search` and
# `callgraph` are what the matrix asks for; `tier2` is the inspect plane, which
# the process schedules for itself and which therefore shows up in the same
# peak without appearing anywhere in the matrix's own timings.
TRACKED_PLANES = ("callgraph", "search", "tier2")
INDEX_EVENT_MARKER = "index_event "
INDEX_EVENT_FIELD_RE = re.compile(r"([a-z_]+)=(\S+)")
LOG_TIMESTAMP_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(\.\d+)?Z")


def index_event_fields(line: str) -> dict[str, str] | None:
    """Field map of one ``index_event`` log line, or None for any other line."""
    marker = line.find(INDEX_EVENT_MARKER)
    if marker < 0:
        return None
    return dict(INDEX_EVENT_FIELD_RE.findall(line[marker + len(INDEX_EVENT_MARKER):]))


def apply_stage_event(stages: dict[str, str], fields: dict[str, str]) -> None:
    """Advance one plane's current stage from an ``index_event`` field map."""
    plane = fields.get("plane", "")
    if plane not in TRACKED_PLANES:
        return
    kind = fields.get("kind", "")
    if plane == "tier2":
        # Each Tier-2 category is its own short build inside the process, so the
        # category name is the stage: "which category was running" is the
        # question a peak taken during one of them has to answer.
        if kind == "build_started":
            stages[plane] = fields.get("category", "unknown")
        elif kind in ("build_ready", "build_failed", "build_suspended", "build_superseded"):
            stages[plane] = "idle"
        return
    if kind == "build_started":
        stages[plane] = "started"
    elif kind == "build_progress":
        stages[plane] = fields.get("stage", "unknown")
    elif kind == "build_ready":
        stages[plane] = "ready"
    elif kind in ("build_failed", "build_suspended", "build_superseded"):
        stages[plane] = kind[len("build_"):]


def phase_label(stages: dict[str, str]) -> str:
    """Name what every plane was doing, because they overlap.

    The search (trigram) build, the callgraph build and the Tier-2 inspect
    categories all run in one process, so a per-plane breakdown would hide a
    peak that only exists while two of them coincide. One composite label per
    sample keeps that case visible, and an overlap is what a peak with two
    states looks like from the inside.
    """
    label = f"{stages.get('callgraph', 'idle')}/{stages.get('search', 'idle')}"
    tier2 = stages.get("tier2", "idle")
    if tier2 != "idle":
        label += f"+tier2:{tier2}"
    return label


@dataclass
class PhaseSample:
    elapsed_ms: int
    rss_kb: int
    hwm_kb: int
    wall_s: float = 0.0
    callgraph_stage: str = "idle"
    search_stage: str = "idle"
    tier2_stage: str = "idle"

    @property
    def phase(self) -> str:
        return phase_label({
            "callgraph": self.callgraph_stage,
            "search": self.search_stage,
            "tier2": self.tier2_stage,
        })


def log_seconds(line: str) -> float | None:
    """Wall clock of a log line, retaining fractional seconds when available.

    Older logs stamp whole seconds. Taking their midpoint keeps the rounding
    error symmetric instead of charging every boundary to the earlier phase.
    """
    stamp = LOG_TIMESTAMP_RE.match(line)
    if not stamp:
        return None
    parsed = datetime.strptime(stamp.group(1), "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return parsed.timestamp() + (float(stamp.group(2)) if stamp.group(2) else 0.5)


@dataclass
class LogTimeline:
    """What each plane was doing over time, rebuilt from a finished log."""

    stages: list[tuple[float, dict[str, str]]] = field(default_factory=list)
    tier2_windows: list[tuple[float, float, str]] = field(default_factory=list)

    def is_empty(self) -> bool:
        return not self.stages and not self.tier2_windows


def build_timeline(lines: Iterable[str]) -> LogTimeline:
    """Rebuild plane activity from the timestamps the process wrote.

    Attribution cannot use the moment the harness read a line. The log is
    written through a buffer, so a stretch that emits few lines holds them
    until something else forces a flush, and everything that happened during
    the silence then arrives at once.

    The two planes the matrix builds announce each stage as they enter it, so
    their boundaries are the line timestamps. The Tier-2 plane does not: it
    logs a category's `build_started` and `build_ready` together once the
    category is finished, both stamped with the same second. Reading those in
    order would say the category was never running. Its window is taken from
    the `elapsed_ms` the completion carries instead, which is the only record
    of when the work actually happened.
    """
    stages = {plane: "idle" for plane in TRACKED_PLANES if plane != "tier2"}
    timeline = LogTimeline()
    for line in lines:
        seconds = log_seconds(line)
        if seconds is None:
            continue
        fields = index_event_fields(line)
        if not fields:
            continue
        if fields.get("plane") == "tier2":
            if fields.get("kind") == "build_ready":
                try:
                    elapsed_s = int(fields.get("elapsed_ms", "0")) / 1000
                except ValueError:
                    continue
                timeline.tier2_windows.append(
                    (seconds - elapsed_s, seconds, fields.get("category", "unknown"))
                )
            continue
        apply_stage_event(stages, fields)
        if timeline.stages and timeline.stages[-1][0] == seconds:
            timeline.stages[-1] = (seconds, dict(stages))
        else:
            timeline.stages.append((seconds, dict(stages)))
    timeline.tier2_windows.sort()
    return timeline


def label_samples(samples: list[PhaseSample], timeline: LogTimeline) -> None:
    """Give every sample the stages that were current when it was taken."""
    index = 0
    stages = {plane: "idle" for plane in TRACKED_PLANES}
    for sample in samples:
        while index < len(timeline.stages) and timeline.stages[index][0] <= sample.wall_s:
            stages = timeline.stages[index][1]
            index += 1
        sample.callgraph_stage = stages.get("callgraph", "idle")
        sample.search_stage = stages.get("search", "idle")
        sample.tier2_stage = "idle"
        for start, end, category in timeline.tier2_windows:
            if start > sample.wall_s:
                break
            if sample.wall_s <= end:
                sample.tier2_stage = category
                break


@dataclass
class PhaseRollup:
    phase: str
    samples: int = 0
    elapsed_ms: int = 0
    rss_max_kb: int = 0
    hwm_growth_kb: int = 0
    hwm_end_kb: int = 0


def attribute_phases(samples: list[PhaseSample]) -> list[PhaseRollup]:
    """Charge each step of the kernel high-water mark to the phase that took it.

    ``VmHWM`` only ever rises, so the growth between two samples was caused by
    whatever ran between them. An increment is charged to the later sample's
    phase: a boundary increment then lands on the phase that had just begun,
    which is the phase that allocated it.
    """
    rollups: dict[str, PhaseRollup] = {}
    previous: PhaseSample | None = None
    for sample in samples:
        rollup = rollups.setdefault(sample.phase, PhaseRollup(sample.phase))
        rollup.samples += 1
        rollup.rss_max_kb = max(rollup.rss_max_kb, sample.rss_kb)
        rollup.hwm_end_kb = max(rollup.hwm_end_kb, sample.hwm_kb)
        if previous is not None:
            rollup.elapsed_ms += max(0, sample.elapsed_ms - previous.elapsed_ms)
            rollup.hwm_growth_kb += max(0, sample.hwm_kb - previous.hwm_kb)
        else:
            # Nothing observed the process before its first sample, so its
            # starting high-water mark is the cost of getting to that sample.
            rollup.hwm_growth_kb += sample.hwm_kb
        previous = sample
    return sorted(rollups.values(), key=lambda item: (-item.hwm_growth_kb, item.phase))


def format_phase_summary(rollups: list[PhaseRollup], limit: int = 6) -> str:
    """Compact ``phase=+MB`` record of where the high-water mark was taken."""
    if not rollups:
        return "n/a"
    parts = [f"{rollup.phase}=+{rollup.hwm_growth_kb / 1024:.1f}" for rollup in rollups[:limit] if rollup.hwm_growth_kb]
    return ";".join(parts) or "none"


def write_phase_trace(path: Path, samples: list[PhaseSample]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=PHASE_TRACE_FIELDS, lineterminator="\n")
        writer.writeheader()
        for sample in samples:
            writer.writerow({
                "elapsed_ms": sample.elapsed_ms,
                "rss_kb": sample.rss_kb,
                "hwm_kb": sample.hwm_kb,
                "callgraph_stage": sample.callgraph_stage,
                "search_stage": sample.search_stage,
                "tier2_stage": sample.tier2_stage,
                "phase": sample.phase,
            })


@dataclass
class PhaseSampler:
    """Resolve peak memory by build phase instead of by process.

    ``ProcessSampler`` answers how much memory a run used. It cannot answer
    which part of the build used it, and a peak with two reproducible states
    needs that second answer: the phase whose high-water growth differs between
    a low run and a high run is the phase that owns the difference.

    This reads ``VmHWM`` rather than ``VmRSS`` for attribution because the
    high-water mark is exact and monotonic, so no spike can open and close
    between two samples without being counted.
    """

    proc: subprocess.Popen[bytes]
    storage: Path
    interval_s: float = PHASE_SAMPLE_INTERVAL_S
    samples: list[PhaseSample] = field(default_factory=list)
    gaps: list[str] = field(default_factory=list)
    observed_stages: list[tuple[float, dict[str, str]]] = field(default_factory=list)
    _started: float = 0.0
    _stop: threading.Event = field(default_factory=threading.Event)
    _thread: threading.Thread | None = None

    def start(self) -> None:
        if not sys.platform.startswith("linux"):
            self.gaps.append("phase memory trace unavailable: /proc/<pid>/status is Linux only")
            return
        self._started = time.monotonic()
        self._thread = threading.Thread(target=self._loop, name="oss-matrix-phase-sampler", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop sampling, then label the samples from the finished log."""
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=3)
        if not self.samples:
            return
        timeline = (LogTimeline(stages=self.observed_stages) if self.observed_stages
                    else build_timeline(self._log_lines()))
        if timeline.is_empty():
            self.gaps.append("phase memory trace unlabelled: no index_event records in the AFT log")
            return
        label_samples(self.samples, timeline)

    def mark(self, search: str, callgraph: str) -> None:
        """Label externally observed phases, not retired store internals."""
        self.observed_stages.append((time.time(), {"search": search, "callgraph": callgraph}))

    def _log_lines(self) -> list[str]:
        lines: list[str] = []
        for path in sorted((self.storage / "logs").glob("aft-*.log")):
            try:
                lines.extend(path.read_text(encoding="utf-8", errors="replace").splitlines())
            except OSError as error:
                self.gaps.append(f"phase memory trace could not read {path.name}: {error}")
        return lines

    def _loop(self) -> None:
        self._sample_once()
        while not self._stop.wait(self.interval_s):
            self._sample_once()

    def _sample_once(self) -> None:
        if self.proc.poll() is not None:
            return
        rss_kb, hwm_kb = self._read_memory()
        if rss_kb is None or hwm_kb is None:
            return
        self.samples.append(PhaseSample(
            elapsed_ms=int((time.monotonic() - self._started) * 1000),
            rss_kb=rss_kb,
            hwm_kb=hwm_kb,
            wall_s=time.time(),
        ))

    def _read_memory(self) -> tuple[int | None, int | None]:
        rss_kb: int | None = None
        hwm_kb: int | None = None
        try:
            text = Path(f"/proc/{self.proc.pid}/status").read_text(encoding="utf-8")
        except (OSError, ValueError):
            if "phase memory trace incomplete: /proc/<pid>/status" not in self.gaps:
                self.gaps.append("phase memory trace incomplete: /proc/<pid>/status")
            return None, None
        for line in text.splitlines():
            try:
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                elif line.startswith("VmHWM:"):
                    hwm_kb = int(line.split()[1])
            except (IndexError, ValueError):
                return None, None
            if rss_kb is not None and hwm_kb is not None:
                break
        return rss_kb, hwm_kb


def parse_cpu_time(value: str) -> float:
    pieces = [float(piece) for piece in value.split(":")]
    if len(pieces) == 2:
        return pieces[0] * 60 + pieces[1]
    if len(pieces) == 3:
        return pieces[0] * 3600 + pieces[1] * 60 + pieces[2]
    return 0.0


def parse_proc_stat_cpu_seconds(stat: str, clock_ticks: int | None = None) -> float:
    """Return cumulative user+system CPU time from one Linux proc stat row."""
    close = stat.rfind(")")
    if close < 0:
        raise ValueError("/proc stat row has no closing comm delimiter")
    fields = stat[close + 1 :].split()
    # The remainder starts at field 3 (state), so fields 14 and 15 are indexes
    # 11 and 12. The comm field may itself contain spaces and parentheses.
    if len(fields) <= 12:
        raise ValueError("/proc stat row does not contain CPU tick fields")
    ticks = os.sysconf("SC_CLK_TCK") if clock_ticks is None else clock_ticks
    if ticks <= 0:
        raise ValueError("SC_CLK_TCK must be positive")
    try:
        user_ticks, system_ticks = int(fields[11]), int(fields[12])
    except ValueError as error:
        raise ValueError("/proc stat CPU tick fields are not integers") from error
    return (user_ticks + system_ticks) / ticks


def run_command(argv: list[str], *, cwd: Path | None = None, timeout: int = 60) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, cwd=str(cwd) if cwd else None, text=True, capture_output=True, check=False, timeout=timeout)


def repo_is_git(root: Path) -> bool:
    try:
        return run_command(["git", "rev-parse", "--is-inside-work-tree"], cwd=root).returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def tracked_paths(root: Path) -> list[Path] | None:
    try:
        completed = run_command(["git", "ls-files", "-z"], cwd=root)
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode != 0:
        return None
    return [root / item for item in completed.stdout.split("\0") if item]


def walked_paths(root: Path) -> Iterable[Path]:
    for directory, _dirs, files in os.walk(root):
        for filename in files:
            yield Path(directory) / filename


def repo_shape(root: Path, git: bool) -> tuple[int, str, list[Path]]:
    paths = tracked_paths(root) if git else None
    if paths is None:
        paths = list(walked_paths(root))
    extensions: dict[str, int] = {}
    for path in paths:
        suffix = path.suffix.lower().lstrip(".")
        if suffix:
            extensions[suffix] = extensions.get(suffix, 0) + 1
    languages = ", ".join(
        f"{LANGUAGE_NAMES.get(extension, extension)} ({count})"
        for extension, count in sorted(extensions.items(), key=lambda item: (-item[1], item[0]))[:3]
    ) or "n/a"
    return len(paths), languages, paths


def select_probe_files(root: Path, paths: Iterable[Path]) -> list[Path]:
    selected = [path for path in paths if path.suffix.lower() in TEXT_SUFFIXES and path.is_file()]
    return selected[:80]


def existing_literal(paths: Iterable[Path]) -> str | None:
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue
        match = WORD_RE.search(text)
        if match:
            return re.escape(match.group(0))
    return None


def outlined_symbol(client: NdjsonClient, root: Path, paths: Iterable[Path], timeout_s: float) -> tuple[str, str] | None:
    for path in paths:
        try:
            relative = str(path.relative_to(root))
            response = client.request(
                "tool_call", timeout_s=timeout_s, name="outline", arguments={"target": relative}
            )
        except NdjsonError:
            continue
        text = str(response.get("text") or "")
        match = SYMBOL_RE.search(text)
        if response.get("success") and match and match.group(1) not in {"init"}:
            # Go permits package init functions that are not addressable by the
            # callgraph query resolver. Prefer a named callable so the probe
            # measures the query path rather than a resolver false negative.
            return relative, match.group(1)
    return None


def status_value(response: dict[str, Any], plane: str) -> str:
    keys = (
        ("search_index", "search", "search_status")
        if plane == "search"
        else ("callgraph", "callgraph_store", "callgraph_index", "callgraph_status")
    )
    for key in keys:
        value = response.get(key)
        if isinstance(value, str):
            return value.lower()
        if isinstance(value, dict):
            state = value.get("status") or value.get("state")
            if isinstance(state, str):
                return state.lower()
    return "unknown"


def logged_plane_state(storage: Path, plane: str) -> str | None:
    """Read only the tail because an isolated run has one root and small live logs."""
    for path in sorted((storage / "logs").glob("aft-*.log")):
        try:
            with path.open("rb") as handle:
                handle.seek(max(0, path.stat().st_size - 262_144))
                text = handle.read().decode("utf-8", errors="replace")
        except OSError:
            continue
        for kind, state in (("build_suspended", "suspended"), ("build_failed", "failed"), ("build_ready", "ready")):
            if f"index_event kind={kind} plane={plane}" in text:
                return state
    return None


def terminal_outcome(states: dict[str, str]) -> str | None:
    values = set(states.values())
    if any("suspend" in value for value in values):
        return "suspended"
    if any(value == "disabled" for value in values):
        return "failed"
    if any("fail" in value or "error" in value for value in values):
        return "failed"
    return None


def census_metrics(script_dir: Path, root: Path, storage: Path, git: bool, gaps: list[str]) -> dict[str, str]:
    logs = sorted((storage / "logs").glob("aft-*.log"))
    if not logs:
        gaps.append("no AFT log found under storage/logs")
        return {}
    cache_keys = storage / "oss-matrix-cache-keys.json"
    cache_keys.write_text(
        json.dumps({str(root.resolve()): {"key": "oss-matrix", "git_root_commit": "matrix" if git else None}}),
        encoding="utf-8",
    )
    output = storage / "census"
    env = os.environ.copy()
    env["CENSUS_OUTPUT_DIR"] = str(output)
    try:
        completed = subprocess.run(
            [sys.executable, str(script_dir / "index-census.py"), str(storage / "logs"), str(cache_keys), str(root.resolve())],
            text=True,
            capture_output=True,
            env=env,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as error:
        gaps.append(f"index-census could not run: {error}")
        return {}
    if completed.returncode != 0:
        gaps.append(f"index-census failed: {completed.stderr.strip()[:200] or completed.stdout.strip()[:200]}")
        return {}
    csv_path = output / "census-roots.csv"
    try:
        with csv_path.open(encoding="utf-8", newline="") as handle:
            rows = list(csv.DictReader(handle))
    except OSError as error:
        gaps.append(f"index-census CSV missing: {error}")
        return {}
    return rows[0] if rows else {}


def log_paths(storage: Path) -> str:
    logs = sorted((storage / "logs").glob("aft-*.log"))
    return ";".join(str(path) for path in logs) if logs else "n/a"


def empty_row(repo: str, root: Path) -> dict[str, str]:
    return {field: "n/a" for field in CSV_FIELDS} | {
        "repo": repo,
        "path": str(root),
        "semantic": "disabled (matrix policy)",
    }


def close_after_final_sample(sampler: ProcessSampler | None, client: NdjsonClient | None) -> str | None:
    if sampler is not None:
        sampler.stop()
    return client.close() if client is not None else None


def peak_memory_metrics(sampler: ProcessSampler) -> dict[str, str]:
    # Only the kernel peak captures a spike that falls between resource polls.
    # Require a successful final read while AFT is alive, before closing stdin;
    # a missing HWM must not become a zero or fall back to the sampled maximum.
    hwm = f"{sampler.peak_rss_hwm_kb / 1024:.1f}" if sampler.hwm_available and sampler.peak_rss_hwm_kb > 0 else "n/a"
    return {"peak_rss_mb": hwm, "peak_rss_hwm_mb": hwm,
            "peak_rss_sampled_mb": f"{sampler.peak_rss_kb / 1024:.1f}"}


PROBE_NAME = "aft_cost_gate_probe.rs"
PROBE_CALLER = "aft_cost_gate_caller"


def probe_source(target: str) -> str:
    return ("pub fn aft_cost_gate_before() {}\npub fn aft_cost_gate_after() {}\n"
            f"pub fn {PROBE_CALLER}() {{ {target}(); }}\n")


def observe_store_ready(status: dict[str, Any], ready_ms: dict[str, float], elapsed_ms: float) -> dict[str, str]:
    if not status.get("success") or not isinstance(status.get("data_store"), dict):
        raise NdjsonError("status did not expose the current index store")
    states = {plane: status_value(status, plane) for plane in PLANES}
    if any(state not in ("building", "ready") for state in states.values()):
        raise NdjsonError(f"index store unavailable: {states}")
    for plane, state in states.items():
        if state == "ready" and plane not in ready_ms:
            ready_ms[plane] = elapsed_ms
    return states


def checked_tool(client: NdjsonClient, deadline: float, name: str, **arguments: Any) -> dict[str, Any]:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise BudgetExceeded
    response = client.request("tool_call", timeout_s=min(60.0, remaining), name=name, arguments=arguments)
    if not response.get("success"):
        raise NdjsonError(f"{name} probe failed: {response.get('message', response.get('code', 'unknown error'))}")
    return response


def probe_callers(client: NdjsonClient, deadline: float, target: str) -> bool:
    response = checked_tool(client, deadline, "callgraph", op="callers", path=PROBE_NAME, symbol=target)
    return PROBE_CALLER in json.dumps(response)


def measure_first_queries(client: NdjsonClient, deadline: float) -> dict[str, float]:
    """Measure request latency, not the old idle gap from ready to first query."""
    started = time.monotonic()
    response = checked_tool(client, deadline, "grep", pattern="aft_cost_gate_before", path=PROBE_NAME)
    if "aft_cost_gate_before" not in json.dumps(response):
        raise NdjsonError("search did not contain the first-query probe's literal")
    search_ms = (time.monotonic() - started) * 1000.0
    started = time.monotonic()
    if not probe_callers(client, deadline, "aft_cost_gate_before"):
        raise NdjsonError("cold call graph did not contain the first-query probe's original edge")
    callgraph_ms = (time.monotonic() - started) * 1000.0
    return {"search_first_query_latency_ms": search_ms, "callgraph_first_query_latency_ms": callgraph_ms}


def measure_refresh(client: NdjsonClient, deadline: float) -> float:
    """Time a real edit through publication and an exact call-graph read.

    A successful tool response alone can still describe the old graph. Require
    the edge to move between two existing targets before recording a duration.
    Editing through AFT also exercises its production path for processing
    the changed file before the next reader answers.
    """
    if not probe_callers(client, deadline, "aft_cost_gate_before"):
        raise NdjsonError("cold call graph did not contain the refresh probe's original edge")
    started = time.monotonic()
    checked_tool(client, deadline, "edit", path=PROBE_NAME, edits=[{
        "oldString": "{ aft_cost_gate_before(); }", "newString": "{ aft_cost_gate_after(); }",
    }])
    if not probe_callers(client, deadline, "aft_cost_gate_after"):
        raise NdjsonError("refresh did not publish the new call edge")
    if probe_callers(client, deadline, "aft_cost_gate_before"):
        raise NdjsonError("refresh retained the removed call edge")
    return (time.monotonic() - started) * 1000.0


def run_repo(binary: Path, script_dir: Path, root: Path, storage: Path, budget_seconds: float) -> dict[str, str]:
    row = empty_row(root.name, root)
    row["measurement_profile"] = MEASUREMENT_PROFILE
    row["semantic"] = "disabled (matrix policy; no embedding backend)"
    if not root.is_dir():
        row.update({"git": "n/a", "files": "n/a", "top_languages": "n/a", "outcome": "missing", "gaps": "repository path absent"})
        return row
    git = repo_is_git(root)
    file_count, languages, _all_paths = repo_shape(root, git)
    row.update({"git": "yes" if git else "no", "files": str(file_count), "top_languages": languages})
    gaps: list[str] = []
    if storage.exists():
        raise RuntimeError(f"refusing to reuse storage (cold-start fence): {storage}")
    storage.mkdir(parents=True)
    started = time.monotonic()
    deadline = started + budget_seconds
    states = {plane: "loading" for plane in PLANES}
    ready_ms: dict[str, float] = {}
    probe_path = root / PROBE_NAME
    if probe_path.exists():
        raise RuntimeError(f"refusing to overwrite refresh probe: {probe_path}")
    probe_created = False
    client: NdjsonClient | None = None
    sampler: ProcessSampler | None = None
    phases: PhaseSampler | None = None
    outcome = "failed"
    try:
        with probe_path.open("x", encoding="utf-8") as handle:
            probe_created = True
            handle.write(probe_source("aft_cost_gate_before"))
        client = NdjsonClient(binary, root, storage, allow_non_git_callgraph=not git)
        sampler = ProcessSampler(client.proc)
        sampler.start()
        phases = PhaseSampler(client.proc, storage)
        phases.start()
        # Inspect warms its own scanners in the same process. Their scheduling
        # and allocator retention must not become part of a cold-index budget.
        config_doc = json.dumps({
            "indexes": {"trigram": True, "callgraph": True, "semantic": False},
            "inspect": {"enabled": False},
        })
        phases.mark("building", "building")
        build_started = time.monotonic()
        configured = client.request(
            "configure",
            timeout_s=max(0.1, min(30.0, deadline - time.monotonic())),
            project_root=str(root.resolve()),
            harness="runner",
            storage_dir=str(storage.resolve()),
            config=[{"tier": "user", "source": "<oss-matrix>", "doc": config_doc}],
            _bypass_size_limits=True,
        )
        if not configured.get("success"):
            raise NdjsonError(f"configure failed: {configured.get('message', configured.get('code', 'unknown error'))}")
        while len(ready_ms) < len(PLANES):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise BudgetExceeded
            # Status is the new store's public readiness surface. A short sleep
            # bounds observation error without turning readiness into a busy loop.
            time.sleep(min(0.05, remaining))
            status = client.request("status", timeout_s=max(0.1, min(10.0, deadline - time.monotonic())))
            states = observe_store_ready(status, ready_ms, (time.monotonic() - build_started) * 1000.0)
            phases.mark(states["search"], states["callgraph"])
        if len(ready_ms) == len(PLANES):
            row["index_store_build_ready_ms"] = f"{ready_ms['search']:.3f}"
            row["callgraph_store_build_ready_ms"] = f"{ready_ms['callgraph']:.3f}"
            row.update({metric: f"{value:.3f}" for metric, value in measure_first_queries(client, deadline).items()})
            phases.mark("ready", "refresh")
            row["callgraph_refresh_ms"] = f"{measure_refresh(client, deadline):.3f}"
            phases.mark("ready", "ready")
            outcome = "ready"
    except BudgetExceeded:
        outcome = "budget_exceeded"
    except (NdjsonError, OSError, ValueError) as error:
        gaps.append(str(error))
        outcome = "failed"
    finally:
        close_gap = close_after_final_sample(sampler, client)
        if close_gap:
            gaps.append(close_gap)
        if probe_created:
            probe_path.unlink(missing_ok=True)
        if phases is not None:
            phases.stop()
            gaps.extend(phases.gaps)
            rollups = attribute_phases(phases.samples)
            row["hwm_by_phase"] = format_phase_summary(rollups)
            if phases.samples:
                write_phase_trace(storage / "phase-memory.csv", phases.samples)
        if sampler is not None:
            gaps.extend(sampler.gaps)
            row.update(peak_memory_metrics(sampler))
            row["cpu_s"] = f"{sampler.cpu_seconds:.2f}" if sampler.cpu_seconds is not None else "n/a"
            row["cpu_precision"] = sampler.cpu_precision
            row["disk_write_bytes"] = str(sampler.disk_write_bytes) if sampler.disk_write_bytes is not None else "n/a"
    metrics = census_metrics(script_dir, root, storage, git, gaps)
    row.update({
        "search_status": states["search"],
        "callgraph_status": states["callgraph"],
        "waiting_on": metrics.get("index_waiting_on", "n/a"),
        "outcome": outcome,
        "log_path": log_paths(storage),
        "gaps": "; ".join(dict.fromkeys(gaps)) or "none",
    })
    return row


def load_roots(list_file: Path | None) -> list[Path]:
    if list_file is None:
        base = Path.home() / "Work" / "OSS"
        return [base / slug for slug in DEFAULT_REPOS]
    return [Path(line).expanduser() for line in list_file.read_text(encoding="utf-8").splitlines() if line.strip() and not line.lstrip().startswith("#")]


def append_row(path: Path, row: dict[str, str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    exists = path.exists() and path.stat().st_size > 0
    if exists:
        with path.open(encoding="utf-8", newline="") as handle:
            headers = next(csv.reader(handle), [])
        if tuple(headers) != CSV_FIELDS:
            raise RuntimeError(f"existing CSV schema differs: {path}")
    with path.open("a", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=CSV_FIELDS, lineterminator="\n")
        if not exists:
            writer.writeheader()
        writer.writerow(row)


def read_rows(path: Path) -> list[dict[str, str]]:
    if not path.exists():
        return []
    with path.open(encoding="utf-8", newline="") as handle:
        return list(csv.DictReader(handle))


def write_markdown(path: Path, rows: list[dict[str, str]], scratch: Path) -> None:
    with path.open("w", encoding="utf-8") as handle:
        handle.write("# OSS repository-shape cold-build matrix\n\n")
        handle.write("Every row uses a fresh `AFT_STORAGE_DIR`; semantic and inspect are disabled. Timings are monotonic milliseconds observed through production status and edit/callgraph responses.\n\n")
        handle.write("| Repo | Git? | Files | Top languages | Semantic | Index-store ready ms | Callgraph ready / refresh ms | Peak RSS MB | CPU s | Outcome | Log |\n")
        handle.write("| --- | --- | ---: | --- | --- | --- | --- | ---: | ---: | --- | --- |\n")
        for row in rows:
            handle.write(
                f"| {row['repo']} | {row['git']} | {row['files']} | {row['top_languages']} | {row['semantic']} | {row['index_store_build_ready_ms']} | {row['callgraph_store_build_ready_ms']} / {row['callgraph_refresh_ms']} | {row['peak_rss_mb']} | {row['cpu_s']} | {row['outcome']} | `{row['log_path']}` |\n"
            )
        gaps = [(row["repo"], row["gaps"]) for row in rows if row["gaps"] != "none"]
        handle.write("\n## Peak memory by build phase\n\n")
        handle.write("High-water growth charged to the phase that took it, largest first. `callgraph_stage/search_stage`, with `+tier2:<category>` where an inspect category overlapped, so a phase that only exists while two builds run at once is named as one.\n\n")
        handle.write("| Repo | Gated kernel peak RSS MB | Diagnostic sampled peak MB | High-water growth by phase |\n| --- | ---: | ---: | --- |\n")
        for row in rows:
            handle.write(
                f"| {row['repo']} | {row['peak_rss_mb']} | {row.get('peak_rss_sampled_mb', 'n/a')} | {row.get('hwm_by_phase', 'n/a')} |\n"
            )
        handle.write("\n## Gaps\n\n")
        handle.write(f"- Scratch root: `{scratch}`.\n")
        handle.write("- Semantic indexing is intentionally disabled: no embedding backend is measured.\n")
        if gaps:
            for repo, gap in gaps:
                handle.write(f"- `{repo}`: {gap}.\n")
        else:
            handle.write("- No per-repository measurement gaps were recorded.\n")


def default_binary(repo_root: Path) -> Path:
    return Path(os.environ.get("AFT_BINARY", repo_root / "target" / "release" / "aft")).expanduser()


def run_matrix(args: argparse.Namespace) -> tuple[Path, Path, list[dict[str, str]]]:
    script_dir = Path(__file__).resolve().parent
    repo_root = script_dir.parents[1]
    binary = Path(args.aft_binary).expanduser() if args.aft_binary else default_binary(repo_root)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"release AFT binary is not executable: {binary} (set AFT_BINARY to override)")
    result_dir = Path(args.results_dir).expanduser().resolve()
    result_dir.mkdir(parents=True, exist_ok=True)
    stamp = args.date or date.today().isoformat()
    csv_path = result_dir / f"oss-matrix-{stamp}.csv"
    md_path = result_dir / f"oss-matrix-{stamp}.md"
    scratch = Path(args.scratch).expanduser().resolve() if args.scratch else result_dir / f"oss-matrix-{stamp}-storage-{datetime.now().strftime('%H%M%S')}"
    scratch.mkdir(parents=True, exist_ok=False)
    rows: list[dict[str, str]] = []
    for index, root in enumerate(load_roots(Path(args.list_file).expanduser() if args.list_file else None), start=1):
        slug = re.sub(r"[^A-Za-z0-9._-]+", "-", root.name) or f"repo-{index}"
        row = run_repo(binary, script_dir, root.resolve(), scratch / slug, args.budget_min * 60)
        append_row(csv_path, row)
        rows.append(row)
        print(f"{row['repo']}: {row['outcome']}", flush=True)
    all_rows = read_rows(csv_path)
    write_markdown(md_path, all_rows, scratch)
    print(f"wrote {csv_path}")
    print(f"wrote {md_path}")
    return csv_path, md_path, rows


def compact_p50(value: str) -> int:
    """Return the p50 from index-census's n/p50/max compact timing field."""
    parts = value.split("/")
    if len(parts) != 3:
        return 0
    try:
        return int(parts[1])
    except ValueError:
        return 0


def phase_self_test() -> int:
    """Check phase attribution without a corpus, a binary, or a Linux kernel.

    The sampler itself can only run where /proc exists, but every decision it
    makes -- which line is a stage boundary, which phase a high-water step
    belongs to -- is pure and testable anywhere, which is where the mistakes
    would be.
    """
    line = (
        "2026-09-21T16:58:32Z [aft] index_event kind=build_progress plane=callgraph "
        "build_id=b1 root=/tmp/jupyterlab key=k1 stage=resolution completed=10 total=99 elapsed_ms=4200"
    )
    fields = index_event_fields(line)
    assert fields is not None and fields["stage"] == "resolution", fields
    assert fields["kind"] == "build_progress" and fields["plane"] == "callgraph", fields
    assert index_event_fields("2026-09-21T16:58:32Z [aft] search index cold streaming build: 1 files") is None

    stages = {plane: "idle" for plane in TRACKED_PLANES}
    apply_stage_event(stages, {"kind": "build_started", "plane": "search"})
    apply_stage_event(stages, {"kind": "build_progress", "plane": "search", "stage": "streaming"})
    apply_stage_event(stages, {"kind": "build_progress", "plane": "callgraph", "stage": "extraction"})
    assert phase_label(stages) == "extraction/streaming", stages
    apply_stage_event(stages, {"kind": "build_ready", "plane": "search"})
    assert phase_label(stages) == "extraction/ready", stages
    # The inspect plane runs inside the same process and its memory lands in
    # the same peak, so an overlap has to be named rather than attributed to
    # whatever the callgraph happened to be doing at the time.
    apply_stage_event(stages, {"kind": "build_started", "plane": "tier2", "category": "duplicates"})
    assert phase_label(stages) == "extraction/ready+tier2:duplicates", stages
    apply_stage_event(stages, {"kind": "build_ready", "plane": "tier2", "category": "duplicates"})
    assert phase_label(stages) == "extraction/ready", stages
    # A plane that is not building must not keep claiming a stage.
    apply_stage_event(stages, {"kind": "build_superseded", "plane": "callgraph"})
    assert phase_label(stages) == "superseded/ready", stages
    # Only the planes whose work can land in a measured cold build are tracked;
    # anything else the process logs must not create a stage of its own.
    apply_stage_event(stages, {"kind": "build_progress", "plane": "views", "stage": "assembly"})
    assert "views" not in stages, stages

    samples = [
        PhaseSample(0, 40_000, 40_000, 1000.0),
        PhaseSample(250, 60_000, 60_000, 1000.25),
        PhaseSample(500, 50_000, 60_000, 1000.5),
        PhaseSample(750, 140_000, 140_000, 1000.75),
        PhaseSample(1000, 90_000, 140_000, 1001.0),
        PhaseSample(1250, 190_000, 190_000, 1001.25),
    ]
    for sample, stage in zip(samples, (
        ("idle", "streaming", "idle"),
        ("idle", "streaming", "idle"),
        ("extraction", "ready", "idle"),
        ("extraction", "ready", "idle"),
        ("resolution", "ready", "idle"),
        ("resolution", "ready", "duplicates"),
    )):
        sample.callgraph_stage, sample.search_stage, sample.tier2_stage = stage
    rollups = {rollup.phase: rollup for rollup in attribute_phases(samples)}
    # The first sample carries the cost of reaching it, and each later step is
    # charged to the phase that was running when the mark moved.
    assert rollups["idle/streaming"].hwm_growth_kb == 60_000, rollups["idle/streaming"]
    assert rollups["extraction/ready"].hwm_growth_kb == 80_000, rollups["extraction/ready"]
    # A phase that only holds memory someone else allocated is charged nothing,
    # which is what makes the largest entry an answer rather than a ranking.
    assert rollups["resolution/ready"].hwm_growth_kb == 0, rollups["resolution/ready"]
    # Memory taken while an inspect category overlaps the callgraph build is
    # charged to the overlap, not to the callgraph stage underneath it.
    assert rollups["resolution/ready+tier2:duplicates"].hwm_growth_kb == 50_000, rollups
    assert rollups["extraction/ready"].rss_max_kb == 140_000, rollups["extraction/ready"]
    assert rollups["idle/streaming"].elapsed_ms == 250, rollups["idle/streaming"]
    summary = format_phase_summary(attribute_phases(samples))
    assert summary == (
        "extraction/ready=+78.1;idle/streaming=+58.6;resolution/ready+tier2:duplicates=+48.8"
    ), summary
    assert format_phase_summary([]) == "n/a"

    # The log arrives in bursts and the Tier-2 plane logs a category only once
    # it has finished, with both of its records stamped the same second. Read
    # in order those records say the category was never running, so its window
    # comes from the elapsed_ms its completion carries.
    log_lines = [
        "2026-09-21T16:58:30Z [aft] index_event kind=build_started plane=callgraph build_id=b1 root=/tmp/x key=k",
        "2026-09-21T16:58:31Z [aft] index_event kind=build_progress plane=callgraph build_id=b1 root=/tmp/x key=k stage=extraction completed=1 total=9 elapsed_ms=1000",
        "2026-09-21T16:58:33Z [aft] index_event kind=build_progress plane=callgraph build_id=b1 root=/tmp/x key=k stage=resolution completed=0 total=99 elapsed_ms=3000",
        "2026-09-21T16:58:36Z [aft] index_event kind=build_started plane=tier2 build_id=b2 root=/tmp/x key=k category=duplicates files=9",
        "2026-09-21T16:58:36Z [aft] index_event kind=build_ready plane=tier2 build_id=b2 root=/tmp/x key=k category=duplicates elapsed_ms=2000 files=9",
        "2026-09-21T16:58:38Z [aft] index_event kind=build_ready plane=callgraph build_id=b1 root=/tmp/x key=k elapsed_ms=8000",
        "not a log line at all",
    ]
    timeline = build_timeline(log_lines)
    assert timeline.tier2_windows and timeline.tier2_windows[0][2] == "duplicates", timeline
    base = datetime(2026, 9, 21, 16, 58, 30, tzinfo=timezone.utc).timestamp()
    offsets = [-0.5 + index * 0.5 for index in range(19)]
    traced = [PhaseSample(index * 500, 1, 1, base + offset) for index, offset in enumerate(offsets)]
    label_samples(traced, timeline)
    by_offset = {round(sample.wall_s - base, 2): sample.phase for sample in traced}
    # A sample taken before anything was logged belongs to no build.
    assert by_offset[-0.5] == "idle/idle", by_offset
    assert by_offset[1.0] == "started/idle", by_offset
    assert by_offset[2.0] == "extraction/idle", by_offset
    assert by_offset[4.0] == "resolution/idle", by_offset
    # The category finished during second 36 after two seconds of work, so the
    # samples it was running through are the ones before its own records.
    assert by_offset[5.0] == "resolution/idle+tier2:duplicates", by_offset
    assert by_offset[6.0] == "resolution/idle+tier2:duplicates", by_offset
    assert by_offset[7.0] == "resolution/idle", by_offset
    assert by_offset[8.5] == "ready/idle", by_offset

    temporary = Path(tempfile.mkdtemp(prefix="aft-oss-matrix-phase-self-test-"))
    try:
        (temporary / "logs").mkdir()
        (temporary / "logs" / "aft-42.log").write_text("\n".join(log_lines) + "\n", encoding="utf-8")
        sampler = PhaseSampler(proc=None, storage=temporary)  # type: ignore[arg-type]
        sampler.samples = [PhaseSample(500, 1, 1, base + 5.5)]
        sampler.stop()
        assert sampler.samples[0].phase == "resolution/idle+tier2:duplicates", sampler.samples
        assert sampler.gaps == [], sampler.gaps
        # A run whose log carries no index_event records must say so rather
        # than reporting every sample as idle.
        (temporary / "logs" / "aft-42.log").write_text("2026-09-21T16:58:30Z [aft] started\n", encoding="utf-8")
        unlabelled = PhaseSampler(proc=None, storage=temporary)  # type: ignore[arg-type]
        unlabelled.samples = [PhaseSample(500, 1, 1, base + 4.5)]
        unlabelled.stop()
        assert unlabelled.gaps and "no index_event records" in unlabelled.gaps[0], unlabelled.gaps
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    print("oss-matrix phase self-test passed")
    return 0


def store_self_test() -> int:
    from types import SimpleNamespace
    from unittest.mock import patch

    ready: dict[str, float] = {}
    status = {"success": True, "data_store": {}, "search_index": {"status": "ready"},
              "callgraph_store": {"status": "building"}}
    observe_store_ready(status, ready, 120)
    assert ready == {"search": 120}, ready
    status["callgraph_store"]["status"] = "ready"
    observe_store_ready(status, ready, 240)
    assert ready == {"search": 120, "callgraph": 240}, ready
    for invalid in ({"success": True}, {**status, "callgraph_store": {"status": "unavailable"}}):
        try:
            observe_store_ready(invalid, {}, 999)
        except NdjsonError:
            pass
        else:
            raise AssertionError("missing or unavailable store was accepted")

    class FakeClient:
        def __init__(self, stale: bool = False):
            self.edited = False
            self.stale = stale

        def request(self, command: str, **params: Any) -> dict[str, Any]:
            assert command == "tool_call", command
            if params["name"] == "edit":
                self.edited = not self.stale
                return {"success": True}
            if params["name"] == "grep":
                return {"success": True, "matches": "aft_cost_gate_before"}
            symbol = params["arguments"]["symbol"]
            present = symbol == ("aft_cost_gate_after" if self.edited else "aft_cost_gate_before")
            return {"success": True, "callers": [PROBE_CALLER] if present else []}

    with patch("time.monotonic", side_effect=[10, 10, 10.012, 20, 20, 20.034]):
        latencies = measure_first_queries(FakeClient(), 1000)  # type: ignore[arg-type]
    assert abs(latencies["search_first_query_latency_ms"] - 12) < 0.000001, latencies
    assert abs(latencies["callgraph_first_query_latency_ms"] - 34) < 0.000001, latencies
    failed_client = SimpleNamespace(request=lambda *_a, **_k: {"success": False, "message": "query failed"})
    try:
        measure_first_queries(failed_client, time.monotonic() + 30)  # type: ignore[arg-type]
    except NdjsonError as error:
        assert "query failed" in str(error), error
    else:
        raise AssertionError("a failed first query was accepted as a latency")
    empty_client = SimpleNamespace(request=lambda *_a, **_k: {"success": True})
    try:
        measure_first_queries(empty_client, time.monotonic() + 30)  # type: ignore[arg-type]
    except NdjsonError as error:
        assert "probe's literal" in str(error), error
    else:
        raise AssertionError("an empty first-query result was accepted as a latency")
    assert measure_refresh(FakeClient(), time.monotonic() + 30) >= 0  # type: ignore[arg-type]
    try:
        measure_refresh(FakeClient(stale=True), time.monotonic() + 30)  # type: ignore[arg-type]
    except NdjsonError as error:
        assert "new call edge" in str(error), error
    else:
        raise AssertionError("a successful edit serving a stale graph was accepted")
    proc = SimpleNamespace(pid=4321, poll=lambda: None)
    sampler = ProcessSampler(proc=proc)  # type: ignore[arg-type]
    paths: list[str] = []

    def proc_text(path: Path, **_kwargs: Any) -> str:
        paths.append(str(path))
        return "VmHWM: 307200 kB\n" if path.name == "status" else "write_bytes: 10\n"

    with patch.object(sys, "platform", "linux"), patch.object(Path, "read_text", proc_text), \
            patch("subprocess.run", return_value=SimpleNamespace(stdout="90000 00:00:01")), \
            patch.dict(globals(), {"parse_proc_stat_cpu_seconds": lambda _stat: 1.0}):
        sampler._sample_once()
    assert "/proc/4321/status" in paths and all(path.startswith("/proc/4321/") for path in paths), paths
    assert peak_memory_metrics(sampler) == {
        "peak_rss_mb": "300.0", "peak_rss_hwm_mb": "300.0", "peak_rss_sampled_mb": "87.9",
    }, peak_memory_metrics(sampler)
    with patch.object(sys, "platform", "linux"), patch.object(Path, "read_text", lambda *_a, **_k: ""), \
            patch("subprocess.run", return_value=SimpleNamespace(stdout="90000 00:00:01")), \
            patch.dict(globals(), {"parse_proc_stat_cpu_seconds": lambda _stat: 1.0}):
        sampler._sample_once()
    assert peak_memory_metrics(sampler)["peak_rss_mb"] == "n/a", peak_memory_metrics(sampler)
    print("oss-matrix store self-test passed: 13 checks (readiness, first-query timers and failures, refresh, stale graph, same-process kernel peak, missing HWM)")
    return 0


def self_test(args: argparse.Namespace) -> int:
    root = Path.home() / "Work" / "OSS" / "tmignore-rs"
    if not root.is_dir():
        raise RuntimeError(f"self-test requires {root}")
    temporary = Path(tempfile.mkdtemp(prefix="aft-oss-matrix-self-test-"))
    try:
        list_file = temporary / "repos.txt"
        list_file.write_text(f"{root}\n", encoding="utf-8")
        positive = argparse.Namespace(**vars(args))
        positive.list_file = str(list_file)
        positive.results_dir = str(temporary / "positive-results")
        positive.scratch = str(temporary / "positive-scratch")
        positive.budget_min = 5.0
        positive.self_test = False
        csv_path, _md_path, rows = run_matrix(positive)
        row = rows[0]
        assert row["outcome"] == "ready", row
        assert row["search_status"] == "ready" and row["callgraph_status"] == "ready", row
        for metric in ("index_store_build_ready_ms", "callgraph_store_build_ready_ms", "search_first_query_latency_ms", "callgraph_first_query_latency_ms", "callgraph_refresh_ms"):
            assert float(row[metric]) > 0, row
        assert not (root / PROBE_NAME).exists(), "refresh probe was not cleaned up"
        print(f"positive row: {json.dumps(row, sort_keys=True)}")
        negative = argparse.Namespace(**vars(args))
        negative.list_file = str(list_file)
        negative.results_dir = str(temporary / "negative-results")
        negative.scratch = str(temporary / "negative-scratch")
        negative.budget_min = 0.01
        negative.self_test = False
        _csv, _md, negative_rows = run_matrix(negative)
        assert negative_rows[0]["outcome"] == "budget_exceeded", negative_rows[0]
        print(f"self-test CSV: {csv_path}")
        print("oss-matrix self-test passed")
        return 0
    finally:
        shutil.rmtree(temporary, ignore_errors=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("list_file", nargs="?", help="One repository path per line (default: ~/Work/OSS matrix)")
    parser.add_argument("--budget-min", type=float, default=45.0, help="Per-repository ready budget in minutes (default: 45)")
    parser.add_argument("--results-dir", default="results", help="CSV/Markdown output directory (default: results)")
    parser.add_argument("--scratch", help="Fresh storage parent; must not already exist")
    parser.add_argument("--aft-binary", help="Release binary (default: $AFT_BINARY or target/release/aft)")
    parser.add_argument("--date", help="Output date suffix, YYYY-MM-DD (default: today)")
    parser.add_argument("--self-test", action="store_true", help="Run tmignore-rs positive and short-budget negative controls")
    parser.add_argument("--self-test-phases", action="store_true", help="Check the phase-attribution helpers; needs no corpus, binary, or /proc")
    parser.add_argument("--self-test-store", action="store_true", help="Check current-store readiness and refresh probes without a binary")
    args = parser.parse_args()
    if args.budget_min <= 0:
        parser.error("--budget-min must be greater than zero")
    return args


if __name__ == "__main__":
    try:
        arguments = parse_args()
        if arguments.self_test_phases:
            raise SystemExit(phase_self_test())
        if arguments.self_test_store:
            raise SystemExit(store_self_test())
        raise SystemExit(self_test(arguments) if arguments.self_test else (run_matrix(arguments) and 0))
    except (AssertionError, RuntimeError, OSError, ValueError) as error:
        print(f"oss-matrix: {error}", file=sys.stderr)
        raise SystemExit(1)
