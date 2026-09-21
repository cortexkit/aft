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
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable

DEFAULT_REPOS = (
    "tmignore-rs", "rails", "laravel", "home-assistant", "nx", "mdn", "kotlin", "roslyn",
    "spark", "kubernetes", "elasticsearch", "llvm", "linux", "_nongit-24k", "chromium",
)
PLANES = ("search", "callgraph")
CSV_FIELDS = (
    "repo", "path", "git", "files", "top_languages", "semantic", "search_status",
    "callgraph_status", "search_wall_ms", "search_first_query_ms", "callgraph_wall_ms",
    "callgraph_first_query_ms", "callgraph_resolution_share_pct", "search_superseded", "search_failed", "search_suspended",
    "callgraph_superseded", "callgraph_failed", "callgraph_suspended", "waiting_on",
    "peak_rss_mb", "peak_rss_hwm_mb", "hwm_by_phase", "cpu_s", "disk_write_bytes", "outcome", "log_path", "gaps",
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
        if allow_non_git_callgraph:
            # Disable file watching for the synthetic non-git corpus so both
            # planes use a cold path instead of reusing a warm callgraph.
            env["AFT_TEST_DISABLE_FILE_WATCHER"] = "1"
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
    # poll missed. Linux only, which is the only platform this gate compares on.
    peak_rss_hwm_kb: int = 0
    cpu_seconds: float = 0.0
    disk_write_bytes: int | None = None
    gaps: list[str] = field(default_factory=list)
    _stop: threading.Event = field(default_factory=threading.Event)
    _thread: threading.Thread | None = None

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
        if self.proc.poll() is not None:
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
                self.cpu_seconds = max(self.cpu_seconds, parse_cpu_time(fields[1]))
            elif "ps resource sampling unavailable" not in self.gaps:
                self.gaps.append("ps resource sampling unavailable")
        except (OSError, ValueError, subprocess.SubprocessError):
            if "ps resource sampling unavailable" not in self.gaps:
                self.gaps.append("ps resource sampling unavailable")
        if sys.platform.startswith("linux"):
            try:
                # Read while the process is alive: /proc entries vanish on exit,
                # and the caller closes the client before stopping the sampler.
                for line in Path(f"/proc/{self.proc.pid}/status").read_text(encoding="utf-8").splitlines():
                    if line.startswith("VmHWM:"):
                        self.peak_rss_hwm_kb = max(self.peak_rss_hwm_kb, int(line.split()[1]))
                        break
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


# Phases are sampled four times a second. The callgraph cold build runs for
# 70-110 seconds on the runner, so this resolves a stage boundary to well under
# a percent of the build while costing two small /proc reads per tick.
PHASE_SAMPLE_INTERVAL_S = 0.25
PHASE_TRACE_FIELDS = ("elapsed_ms", "rss_kb", "hwm_kb", "callgraph_stage", "search_stage", "phase")
# Terminal or not-yet-started plane states. A plane in one of these is not
# building, so it cannot be contributing build memory.
IDLE_STAGES = ("idle", "ready", "failed", "suspended", "superseded")
INDEX_EVENT_MARKER = "index_event "
INDEX_EVENT_FIELD_RE = re.compile(r"([a-z_]+)=(\S+)")


def index_event_fields(line: str) -> dict[str, str] | None:
    """Field map of one ``index_event`` log line, or None for any other line."""
    marker = line.find(INDEX_EVENT_MARKER)
    if marker < 0:
        return None
    return dict(INDEX_EVENT_FIELD_RE.findall(line[marker + len(INDEX_EVENT_MARKER):]))


def apply_stage_event(stages: dict[str, str], fields: dict[str, str]) -> None:
    """Advance one plane's current stage from an ``index_event`` field map."""
    plane = fields.get("plane", "")
    if plane not in PLANES:
        return
    kind = fields.get("kind", "")
    if kind == "build_started":
        stages[plane] = "started"
    elif kind == "build_progress":
        stages[plane] = fields.get("stage", "unknown")
    elif kind == "build_ready":
        stages[plane] = "ready"
    elif kind in ("build_failed", "build_suspended", "build_superseded"):
        stages[plane] = kind[len("build_"):]


def phase_label(stages: dict[str, str]) -> str:
    """Name what both planes were doing, because they overlap.

    The search (trigram) build and the callgraph build run at the same time, so
    a per-plane breakdown would hide a peak that only exists while the two
    coincide. One composite label per sample keeps that case visible.
    """
    return f"{stages.get('callgraph', 'idle')}/{stages.get('search', 'idle')}"


@dataclass
class PhaseSample:
    elapsed_ms: int
    rss_kb: int
    hwm_kb: int
    callgraph_stage: str
    search_stage: str

    @property
    def phase(self) -> str:
        return phase_label({"callgraph": self.callgraph_stage, "search": self.search_stage})


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
    _stages: dict[str, str] = field(default_factory=lambda: {plane: "idle" for plane in PLANES})
    _offsets: dict[Path, int] = field(default_factory=dict)
    _pending: dict[Path, str] = field(default_factory=dict)
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
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=3)

    def _loop(self) -> None:
        self._sample_once()
        while not self._stop.wait(self.interval_s):
            self._sample_once()

    def _sample_once(self) -> None:
        if self.proc.poll() is not None:
            return
        self._drain_logs()
        rss_kb, hwm_kb = self._read_memory()
        if rss_kb is None or hwm_kb is None:
            return
        self.samples.append(PhaseSample(
            elapsed_ms=int((time.monotonic() - self._started) * 1000),
            rss_kb=rss_kb,
            hwm_kb=hwm_kb,
            callgraph_stage=self._stages["callgraph"],
            search_stage=self._stages["search"],
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

    def _drain_logs(self) -> None:
        """Read whatever the build has logged since the last tick.

        Stage boundaries are taken from the live log rather than reconciled
        afterwards from its timestamps: the log stamps whole seconds, and a
        stage of a 70-second build deserves better than a one-second alignment
        error against the memory samples.
        """
        for path in sorted((self.storage / "logs").glob("aft-*.log")):
            try:
                with path.open("rb") as handle:
                    handle.seek(self._offsets.get(path, 0))
                    chunk = handle.read()
                    self._offsets[path] = handle.tell()
            except OSError:
                continue
            if not chunk:
                continue
            # A tick usually lands mid-line. Hold the unterminated tail back so
            # a stage boundary is never lost to the sampling cadence.
            text = self._pending.pop(path, "") + chunk.decode("utf-8", errors="replace")
            lines = text.split("\n")
            self._pending[path] = lines.pop()
            for line in lines:
                fields = index_event_fields(line)
                if fields:
                    apply_stage_event(self._stages, fields)


def parse_cpu_time(value: str) -> float:
    pieces = [float(piece) for piece in value.split(":")]
    if len(pieces) == 2:
        return pieces[0] * 60 + pieces[1]
    if len(pieces) == 3:
        return pieces[0] * 3600 + pieces[1] * 60 + pieces[2]
    return 0.0


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


def run_repo(binary: Path, script_dir: Path, root: Path, storage: Path, budget_seconds: float) -> dict[str, str]:
    row = empty_row(root.name, root)
    row["semantic"] = "disabled (matrix policy; no embedding backend)"
    if not root.is_dir():
        row.update({"git": "n/a", "files": "n/a", "top_languages": "n/a", "outcome": "missing", "gaps": "repository path absent"})
        return row
    git = repo_is_git(root)
    file_count, languages, all_paths = repo_shape(root, git)
    probe_files = select_probe_files(root, all_paths)
    row.update({"git": "yes" if git else "no", "files": str(file_count), "top_languages": languages})
    gaps: list[str] = []
    if storage.exists():
        raise RuntimeError(f"refusing to reuse storage (cold-start fence): {storage}")
    storage.mkdir(parents=True)
    started = time.monotonic()
    deadline = started + budget_seconds
    states = {plane: "loading" for plane in PLANES}
    queried = {plane: False for plane in PLANES}
    client: NdjsonClient | None = None
    sampler: ProcessSampler | None = None
    phases: PhaseSampler | None = None
    outcome = "failed"
    try:
        client = NdjsonClient(binary, root, storage, allow_non_git_callgraph=not git)
        sampler = ProcessSampler(client.proc)
        sampler.start()
        phases = PhaseSampler(client.proc, storage)
        phases.start()
        config_doc = json.dumps({"search_index": True, "callgraph_store": True, "semantic_search": False})
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
        literal = existing_literal(probe_files)
        if literal is None:
            gaps.append("no source literal found for the search first-query probe")
        while not all(queried.values()):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise BudgetExceeded
            # Polling on the same two-second cadence as resource sampling keeps the
            # small-budget control observable and avoids a status busy-loop.
            time.sleep(min(2.0, remaining))
            if time.monotonic() >= deadline:
                raise BudgetExceeded
            status = client.request("status", timeout_s=min(10.0, deadline - time.monotonic()))
            if not status.get("success"):
                raise NdjsonError(f"status failed: {status.get('message', status.get('code', 'unknown error'))}")
            states = {plane: status_value(status, plane) for plane in PLANES}
            # Older standalone status payloads omit callgraph state even though
            # they emit the authoritative index_event grammar. Keep polling
            # status, but use that event as a compatibility fallback rather than
            # turning a completed cold build into a decorative budget timeout.
            if states["callgraph"] == "unknown":
                logged_state = logged_plane_state(storage, "callgraph")
                if logged_state:
                    states["callgraph"] = logged_state
                    gaps.append("status omitted callgraph state; used index_event compatibility fallback")
            terminal = terminal_outcome(states)
            if terminal:
                outcome = terminal
                break
            if states["search"] == "ready" and not queried["search"]:
                if literal is None:
                    outcome = "failed"
                    break
                time.sleep(0.05)
                search = client.request(
                    "tool_call", timeout_s=min(30.0, deadline - time.monotonic()),
                    name="grep", arguments={"pattern": literal},
                )
                if not search.get("success"):
                    raise NdjsonError(f"grep first query failed: {search.get('message', search.get('code', 'unknown error'))}")
                queried["search"] = True
            if states["callgraph"] == "ready" and not queried["callgraph"]:
                probe = outlined_symbol(client, root, probe_files, min(15.0, deadline - time.monotonic()))
                if probe is None:
                    gaps.append("outline returned no callable symbol for the callgraph first-query probe")
                    outcome = "failed"
                    break
                path, symbol = probe
                time.sleep(0.05)
                callgraph = client.request(
                    "tool_call", timeout_s=min(30.0, deadline - time.monotonic()), name="callgraph",
                    arguments={"op": "callers", "path": path, "symbol": symbol},
                )
                if not callgraph.get("success"):
                    raise NdjsonError(f"callgraph first query failed: {callgraph.get('message', callgraph.get('code', 'unknown error'))}")
                queried["callgraph"] = True
        if all(queried.values()):
            outcome = "ready"
        elif outcome == "failed" and "unknown" in states.values():
            gaps.append("status did not expose a ready state for every requested plane")
    except BudgetExceeded:
        outcome = "budget_exceeded"
    except (NdjsonError, OSError, ValueError) as error:
        gaps.append(str(error))
        outcome = "failed"
    finally:
        if client is not None:
            close_gap = client.close()
            if close_gap:
                gaps.append(close_gap)
        if phases is not None:
            phases.stop()
            gaps.extend(phases.gaps)
            rollups = attribute_phases(phases.samples)
            row["hwm_by_phase"] = format_phase_summary(rollups)
            if phases.samples:
                write_phase_trace(storage / "phase-memory.csv", phases.samples)
        if sampler is not None:
            sampler.stop()
            gaps.extend(sampler.gaps)
            row["peak_rss_mb"] = f"{sampler.peak_rss_kb / 1024:.1f}"
            row["peak_rss_hwm_mb"] = f"{sampler.peak_rss_hwm_kb / 1024:.1f}" if sampler.peak_rss_hwm_kb else "n/a"
            row["cpu_s"] = f"{sampler.cpu_seconds:.2f}"
            row["disk_write_bytes"] = str(sampler.disk_write_bytes) if sampler.disk_write_bytes is not None else "n/a"
    metrics = census_metrics(script_dir, root, storage, git, gaps)
    row.update({
        "search_status": states["search"],
        "callgraph_status": states["callgraph"],
        "search_wall_ms": metrics.get("index_search_start_to_ready_ms_n_p50_max", "n/a"),
        "search_first_query_ms": metrics.get("index_search_ready_to_first_query_ms_n_p50_max", "n/a"),
        "callgraph_wall_ms": metrics.get("index_callgraph_start_to_ready_ms_n_p50_max", "n/a"),
        "callgraph_first_query_ms": metrics.get("index_callgraph_ready_to_first_query_ms_n_p50_max", "n/a"),
        "callgraph_resolution_share_pct": metrics.get("index_callgraph_resolution_share_pct_n_p50_max", "n/a"),
        "search_superseded": metrics.get("index_search_superseded", "n/a"),
        "search_failed": metrics.get("index_search_failed", "n/a"),
        "search_suspended": metrics.get("index_search_suspended", "n/a"),
        "callgraph_superseded": metrics.get("index_callgraph_superseded", "n/a"),
        "callgraph_failed": metrics.get("index_callgraph_failed", "n/a"),
        "callgraph_suspended": metrics.get("index_callgraph_suspended", "n/a"),
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
        handle.write("Every row uses a fresh `AFT_STORAGE_DIR`; semantic indexing is disabled for the entire matrix. Timing values are `n/p50/max` milliseconds as emitted by `index-census.py`.\n\n")
        handle.write("| Repo | Git? | Files | Top languages | Semantic | Search wall / first query | Callgraph wall / first query / resolution share | Peak RSS MB | CPU s | Outcome | Log |\n")
        handle.write("| --- | --- | ---: | --- | --- | --- | --- | ---: | ---: | --- | --- |\n")
        for row in rows:
            handle.write(
                f"| {row['repo']} | {row['git']} | {row['files']} | {row['top_languages']} | {row['semantic']} | {row['search_wall_ms']} / {row['search_first_query_ms']} | {row['callgraph_wall_ms']} / {row['callgraph_first_query_ms']} / {row['callgraph_resolution_share_pct']} | {row['peak_rss_mb']} | {row['cpu_s']} | {row['outcome']} | `{row['log_path']}` |\n"
            )
        handle.write("\n## Plane terminal events\n\n")
        handle.write("| Repo | Search superseded / failed / suspended | Callgraph superseded / failed / suspended | Waiting on |\n| --- | --- | --- | --- |\n")
        for row in rows:
            handle.write(
                f"| {row['repo']} | {row['search_superseded']} / {row['search_failed']} / {row['search_suspended']} | {row['callgraph_superseded']} / {row['callgraph_failed']} / {row['callgraph_suspended']} | {row['waiting_on']} |\n"
            )
        gaps = [(row["repo"], row["gaps"]) for row in rows if row["gaps"] != "none"]
        handle.write("\n## Peak memory by build phase\n\n")
        handle.write("High-water growth charged to the phase that took it, largest first. `callgraph_stage/search_stage`, so a phase that only exists while the two builds overlap is named as one.\n\n")
        handle.write("| Repo | Polled peak RSS MB | Kernel high-water MB | High-water growth by phase |\n| --- | ---: | ---: | --- |\n")
        for row in rows:
            handle.write(
                f"| {row['repo']} | {row['peak_rss_mb']} | {row.get('peak_rss_hwm_mb', 'n/a')} | {row.get('hwm_by_phase', 'n/a')} |\n"
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

    stages = {plane: "idle" for plane in PLANES}
    apply_stage_event(stages, {"kind": "build_started", "plane": "search"})
    apply_stage_event(stages, {"kind": "build_progress", "plane": "search", "stage": "streaming"})
    apply_stage_event(stages, {"kind": "build_progress", "plane": "callgraph", "stage": "extraction"})
    assert phase_label(stages) == "extraction/streaming", stages
    apply_stage_event(stages, {"kind": "build_ready", "plane": "search"})
    assert phase_label(stages) == "extraction/ready", stages
    # A plane that is not building must not keep claiming a stage.
    apply_stage_event(stages, {"kind": "build_superseded", "plane": "callgraph"})
    assert phase_label(stages) == "superseded/ready", stages
    # Only the two planes this matrix builds are tracked; anything else the
    # process logs must not create a stage of its own.
    apply_stage_event(stages, {"kind": "build_progress", "plane": "tier2", "stage": "dead_code"})
    assert "tier2" not in stages, stages

    samples = [
        PhaseSample(0, 40_000, 40_000, "idle", "streaming"),
        PhaseSample(250, 60_000, 60_000, "idle", "streaming"),
        PhaseSample(500, 50_000, 60_000, "extraction", "ready"),
        PhaseSample(750, 140_000, 140_000, "extraction", "ready"),
        PhaseSample(1000, 90_000, 140_000, "resolution", "ready"),
    ]
    rollups = {rollup.phase: rollup for rollup in attribute_phases(samples)}
    # The first sample carries the cost of reaching it, and each later step is
    # charged to the phase that was running when the mark moved.
    assert rollups["idle/streaming"].hwm_growth_kb == 60_000, rollups["idle/streaming"]
    assert rollups["extraction/ready"].hwm_growth_kb == 80_000, rollups["extraction/ready"]
    # A phase that only holds memory someone else allocated is charged nothing,
    # which is what makes the largest entry an answer rather than a ranking.
    assert rollups["resolution/ready"].hwm_growth_kb == 0, rollups["resolution/ready"]
    assert rollups["extraction/ready"].rss_max_kb == 140_000, rollups["extraction/ready"]
    assert rollups["idle/streaming"].elapsed_ms == 250, rollups["idle/streaming"]
    summary = format_phase_summary(attribute_phases(samples))
    assert summary == "extraction/ready=+78.1;idle/streaming=+58.6", summary
    assert format_phase_summary([]) == "n/a"

    # The log is tailed live, so a tick that lands mid-line must not lose the
    # stage boundary that line carries.
    temporary = Path(tempfile.mkdtemp(prefix="aft-oss-matrix-phase-self-test-"))
    try:
        (temporary / "logs").mkdir()
        log = temporary / "logs" / "aft-42.log"
        prefix = "2026-09-21T16:58:32Z [aft] index_event kind=build_progress plane=callgraph build_id=b1 root=/tmp/x key=k stage="
        log.write_text(f"{prefix}extraction completed=1 total=9 elapsed_ms=1\n{prefix}resol", encoding="utf-8")
        sampler = PhaseSampler(proc=None, storage=temporary)  # type: ignore[arg-type]
        sampler._drain_logs()
        assert sampler._stages["callgraph"] == "extraction", sampler._stages
        with log.open("a", encoding="utf-8") as handle:
            handle.write("ution completed=5 total=9 elapsed_ms=2\n")
        sampler._drain_logs()
        assert sampler._stages["callgraph"] == "resolution", sampler._stages
        sampler._drain_logs()
        assert sampler._stages["callgraph"] == "resolution", sampler._stages
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    print("oss-matrix phase self-test passed")
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
        assert compact_p50(row["search_first_query_ms"]) > 0, row
        assert compact_p50(row["callgraph_first_query_ms"]) > 0, row
        logs = [Path(path) for path in row["log_path"].split(";") if path]
        matching_logs = [
            path for path in logs
            if "index_event kind=build_ready plane=callgraph" in path.read_text(encoding="utf-8", errors="replace")
        ]
        assert matching_logs, row
        grep = subprocess.run(
            ["grep", "-H", "index_event kind=build_ready plane=callgraph", *map(str, matching_logs)],
            text=True,
            capture_output=True,
            check=False,
        )
        assert grep.returncode == 0, grep.stderr
        positive_fields = {
            key: row[key]
            for key in (
                "outcome", "search_status", "callgraph_status", "search_first_query_ms",
                "callgraph_first_query_ms", "log_path",
            )
        }
        print(f"positive row: {json.dumps(positive_fields, sort_keys=True)}")
        print(grep.stdout, end="")
        events = []
        for path in logs:
            events.extend(
                line.rstrip()
                for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
                if "index_event" in line
            )
        print("first 5 index_event lines:")
        for line in events[:5]:
            print(line)
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
    args = parser.parse_args()
    if args.budget_min <= 0:
        parser.error("--budget-min must be greater than zero")
    return args


if __name__ == "__main__":
    try:
        arguments = parse_args()
        if arguments.self_test_phases:
            raise SystemExit(phase_self_test())
        raise SystemExit(self_test(arguments) if arguments.self_test else (run_matrix(arguments) and 0))
    except (AssertionError, RuntimeError, OSError, ValueError) as error:
        print(f"oss-matrix: {error}", file=sys.stderr)
        raise SystemExit(1)
