#!/usr/bin/env python3
"""Shared, read-mostly helpers for the content-addressed views soak drills."""

from __future__ import annotations

import hashlib
import json
import os
import re
import select
import shutil
import sqlite3
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence


SOAK_SCOPE_BY_ROOT = {
    str((Path.home() / "Work/Projects/CortexKit/prefrontal").resolve()): "c0c39eea197fcc68",
    str((Path.home() / "Work/Projects/CortexKit/magic-context").resolve()): "8f93aad09f2535d0",
    str((Path.home() / "Work/OSS/opencode").resolve()): "0f3900af641f5248",
}
SOURCE_SUFFIXES = {
    ".c",
    ".cc",
    ".cpp",
    ".cs",
    ".go",
    ".java",
    ".js",
    ".jsx",
    ".kt",
    ".lua",
    ".m",
    ".php",
    ".py",
    ".r",
    ".rb",
    ".rs",
    ".scala",
    ".sh",
    ".sol",
    ".swift",
    ".ts",
    ".tsx",
    ".vue",
}
TEST_PATH_PARTS = {"__tests__", "fixtures", "test", "tests", "vendor", "node_modules"}
TOP_LEVEL_FUNCTION_RE = re.compile(
    r"^  (?:E\s+)?(?:(?:pub(?:\([^)]*\))?|export|async|unsafe|extern|static)\s+)*"
    r"(?:function|fn|def|func)\s+([A-Za-z_$][A-Za-z0-9_$]*)\b"
)
PUBLIC_RESPONSE_FIELDS = ("success", "status", "code", "message", "text", "results")


class SoakError(RuntimeError):
    """An actionable instrumentation failure."""


@dataclass(frozen=True)
class StableSymbol:
    path: str
    symbol: str
    changed_at: int


@dataclass(frozen=True)
class ProcessSample:
    cpu_s: float
    rss_kib: int


@dataclass(frozen=True)
class SwitchProbe:
    path: str
    symbol: str
    token: str


class NdjsonClient:
    """One placed-binary process speaking the public standalone NDJSON protocol."""

    def __init__(
        self,
        binary: Path,
        project_root: Path,
        storage_root: Path,
        stderr_path: Path,
        session_id: str,
    ) -> None:
        stderr_path.parent.mkdir(parents=True, exist_ok=True)
        self.project_root = project_root.resolve()
        self.storage_root = storage_root.resolve()
        self.session_id = session_id
        self.stderr_path = stderr_path
        self._stderr = stderr_path.open("w+", encoding="utf-8")
        env = os.environ.copy()
        env["AFT_STORAGE_DIR"] = str(self.storage_root)
        env.setdefault("RUST_LOG", "info")
        self.proc = subprocess.Popen(
            [str(binary)],
            cwd=self.project_root,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            bufsize=0,
        )
        self._buffer = b""
        self._next_id = 0

    def close(self) -> None:
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        self._stderr.close()

    def __enter__(self) -> "NdjsonClient":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def call(self, command: str, timeout_s: float = 180.0, **params: Any) -> dict[str, Any]:
        self._next_id += 1
        request_id = str(self._next_id)
        request = {"id": request_id, "command": command, **params}
        if self.proc.stdin is None or self.proc.stdout is None:
            raise SoakError("standalone AFT pipes are unavailable")
        self.proc.stdin.write(canonical_json_bytes(request) + b"\n")
        self.proc.stdin.flush()
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise SoakError(
                    f"standalone AFT exited with {self.proc.returncode} during {command}: "
                    f"{self.stderr_tail()}"
                )
            wait = max(0.0, min(0.2, deadline - time.monotonic()))
            ready, _, _ = select.select([self.proc.stdout], [], [], wait)
            if ready:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk:
                    self._buffer += chunk
            while b"\n" in self._buffer:
                line, self._buffer = self._buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if str(frame.get("id")) == request_id:
                    if not isinstance(frame, dict):
                        raise SoakError(f"non-object response to {command}: {frame!r}")
                    return frame
        raise SoakError(f"timed out waiting for {command}: {self.stderr_tail()}")

    def configure(self, user_config: Path | None) -> dict[str, Any]:
        params: dict[str, Any] = {
            "project_root": str(self.project_root),
            "harness": "opencode",
            "storage_dir": str(self.storage_root),
        }
        if user_config is not None and user_config.is_file():
            params["cortexkit_user_config_path"] = str(user_config.resolve())
        response = self.call("configure", timeout_s=240.0, **params)
        require_success(response, "configure")
        return response

    def status(self, timeout_s: float = 300.0) -> dict[str, Any]:
        response = self.call("status", timeout_s=timeout_s)
        require_success(response, "status")
        return response

    def tool(self, name: str, arguments: Mapping[str, Any], timeout_s: float = 240.0) -> dict[str, Any]:
        return self.call(
            "tool_call",
            timeout_s=timeout_s,
            session_id=self.session_id,
            name=name,
            arguments=dict(arguments),
        )

    def log_mark(self) -> int:
        self._stderr.flush()
        try:
            return self.stderr_path.stat().st_size
        except FileNotFoundError:
            return 0

    def log_since(self, mark: int) -> str:
        self._stderr.flush()
        with self.stderr_path.open(encoding="utf-8", errors="replace") as handle:
            handle.seek(mark)
            return handle.read()

    def stderr_tail(self, limit: int = 4000) -> str:
        self._stderr.flush()
        with self.stderr_path.open(encoding="utf-8", errors="replace") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            handle.seek(max(0, size - limit))
            return handle.read().strip()


def canonical_json_bytes(value: Any) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def run_checked(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: Path | None = None,
    input_bytes: bytes | None = None,
    allowed: Iterable[int] = (0,),
    timeout_s: float = 300.0,
) -> subprocess.CompletedProcess[bytes]:
    rendered = [str(arg) for arg in args]
    result = subprocess.run(
        rendered,
        cwd=cwd,
        input=input_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout_s,
        check=False,
    )
    allowed_codes = set(allowed)
    if result.returncode not in allowed_codes:
        stdout = result.stdout.decode("utf-8", errors="replace")[-2000:]
        stderr = result.stderr.decode("utf-8", errors="replace")[-4000:]
        raise SoakError(
            f"command failed ({result.returncode}): {' '.join(rendered)}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
    return result


def git_bytes(root: Path, *args: str, allowed: Iterable[int] = (0,)) -> bytes:
    return run_checked(["git", "-C", root, *args], allowed=allowed).stdout


def git_text(root: Path, *args: str, allowed: Iterable[int] = (0,)) -> str:
    return git_bytes(root, *args, allowed=allowed).decode("utf-8", errors="replace").strip()


def require_success(response: Mapping[str, Any], operation: str) -> None:
    if response.get("success") is not True:
        raise SoakError(f"{operation} failed: {json.dumps(response, sort_keys=True)}")


def resolve_root(raw: str | os.PathLike[str], *, require_known: bool = True) -> tuple[Path, str]:
    root = Path(raw).expanduser().resolve()
    if not root.is_dir():
        raise SoakError(f"project root does not exist: {root}")
    if git_text(root, "rev-parse", "--is-inside-work-tree") != "true":
        raise SoakError(f"project root is not a Git worktree: {root}")
    computed = hashlib.sha256(str(root).encode("utf-8")).hexdigest()[:16]
    expected = SOAK_SCOPE_BY_ROOT.get(str(root))
    if require_known and expected is None:
        known = ", ".join(sorted(SOAK_SCOPE_BY_ROOT))
        raise SoakError(f"root is not a configured views-soak seat: {root}; expected one of {known}")
    if expected is not None and computed != expected:
        raise SoakError(f"scope derivation drift for {root}: expected {expected}, computed {computed}")
    return root, expected or computed


def assert_clean_worktree(root: Path, label: str) -> None:
    status = git_text(root, "status", "--porcelain=v1", "--untracked-files=all")
    if status:
        raise SoakError(f"{label} worktree is dirty; refusing checkout:\n{status}")


def strip_jsonc(text: str) -> str:
    output: list[str] = []
    index = 0
    in_string = False
    quote = ""
    escaped = False
    while index < len(text):
        char = text[index]
        following = text[index + 1] if index + 1 < len(text) else ""
        if in_string:
            output.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                in_string = False
            index += 1
            continue
        if char in {'"', "'"}:
            in_string = True
            quote = char
            output.append(char)
            index += 1
            continue
        if char == "/" and following == "/":
            index += 2
            while index < len(text) and text[index] not in "\r\n":
                index += 1
            continue
        if char == "/" and following == "*":
            index += 2
            while index + 1 < len(text) and text[index : index + 2] != "*/":
                index += 1
            if index + 1 >= len(text):
                raise SoakError("unterminated block comment in JSONC")
            index += 2
            continue
        output.append(char)
        index += 1

    without_comments = "".join(output)
    output = []
    index = 0
    in_string = False
    escaped = False
    while index < len(without_comments):
        char = without_comments[index]
        if in_string:
            output.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            output.append(char)
            index += 1
            continue
        if char == ",":
            lookahead = index + 1
            while lookahead < len(without_comments) and without_comments[lookahead].isspace():
                lookahead += 1
            if lookahead < len(without_comments) and without_comments[lookahead] in "}]":
                index += 1
                continue
        output.append(char)
        index += 1
    return "".join(output)


def read_jsonc(path: Path) -> dict[str, Any]:
    if not path.is_file():
        return {}
    try:
        value = json.loads(strip_jsonc(path.read_text(encoding="utf-8")))
    except (OSError, json.JSONDecodeError) as error:
        raise SoakError(f"could not parse JSONC config {path}: {error}") from error
    if not isinstance(value, dict):
        raise SoakError(f"config must contain a JSON object: {path}")
    return value


def write_views_off_config(source_root: Path, baseline: Path) -> Path:
    config = read_jsonc(source_root / ".cortexkit" / "aft.jsonc")
    config.pop("views", None)
    target = baseline / ".cortexkit" / "aft.jsonc"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(config, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return target


def _absolute_git_dir(root: Path, flag: str) -> Path:
    raw = git_text(root, "rev-parse", "--path-format=absolute", flag)
    return Path(raw).resolve()


def ensure_baseline(root: Path, scope: str, head: str) -> Path:
    baseline = Path.home() / ".cache" / "aft-views-soak" / scope / "baseline"
    baseline.parent.mkdir(parents=True, exist_ok=True)
    if baseline.exists():
        if not (baseline / ".git").exists():
            raise SoakError(f"baseline path exists but is not a Git worktree: {baseline}")
        if _absolute_git_dir(root, "--git-common-dir") != _absolute_git_dir(
            baseline, "--git-common-dir"
        ):
            raise SoakError(f"baseline belongs to a different repository: {baseline}")
        git_text(baseline, "checkout", "--quiet", "--detach", "--force", head)
    else:
        git_text(root, "worktree", "add", "--quiet", "--detach", str(baseline), head)
    write_views_off_config(root, baseline)
    return baseline


def wait_search_ready(client: NdjsonClient, timeout_s: float = 300.0) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_s
    last_status: dict[str, Any] = {}
    last_probe: dict[str, Any] = {}
    while time.monotonic() < deadline:
        last_status = client.status()
        search = last_status.get("search_index")
        search_state = search.get("status") if isinstance(search, Mapping) else None
        if search_state == "failed":
            raise SoakError(
                "search index failed while waiting for readiness: "
                + json.dumps(last_status, sort_keys=True)
            )
        if search_state == "ready":
            return last_status
        # A read-only borrower resumes a budgeted artifact load only on demand.
        # Polling status alone can therefore leave an otherwise healthy borrowed
        # search index in `loading` forever.
        last_probe = client.tool(
            "search", {"query": "viewsSoakReadinessSentinel", "topK": 1}, timeout_s=60.0
        )
        if last_probe.get("success") is True and last_probe.get("status") == "ready":
            return client.status()
        time.sleep(0.2)
    raise SoakError(
        "search readiness timed out: "
        + json.dumps({"status": last_status, "probe": last_probe}, sort_keys=True)
    )


def wait_callgraph_ready(
    client: NdjsonClient, symbol: StableSymbol | SwitchProbe, timeout_s: float = 300.0
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_s
    last: dict[str, Any] = {}
    arguments = {"op": "callers", "filePath": symbol.path, "symbol": symbol.symbol}
    while time.monotonic() < deadline:
        last = client.tool("callgraph", arguments)
        code = str(last.get("code", ""))
        text = str(last.get("text", ""))
        if last.get("success") is True or code == "symbol_not_found":
            return last
        if code not in {"callgraph_building", "callgraph_unavailable"} and "callgraph_building" not in text:
            raise SoakError(f"callgraph readiness probe failed: {json.dumps(last, sort_keys=True)}")
        time.sleep(0.2)
    raise SoakError(f"callgraph readiness timed out: {json.dumps(last, sort_keys=True)}")


def select_stable_symbols(
    client: NdjsonClient,
    root: Path,
    *,
    count: int = 5,
    older_than_days: int = 30,
) -> list[StableSymbol]:
    cutoff = int(time.time()) - older_than_days * 24 * 60 * 60
    tracked = git_bytes(root, "ls-files", "-z").split(b"\0")
    candidates = []
    for raw in tracked:
        if not raw:
            continue
        path = raw.decode("utf-8", errors="surrogateescape")
        candidate = Path(path)
        lowered_parts = {part.lower() for part in candidate.parts}
        lowered_name = candidate.name.lower()
        if candidate.suffix.lower() not in SOURCE_SUFFIXES:
            continue
        if lowered_parts & TEST_PATH_PARTS:
            continue
        if any(marker in lowered_name for marker in (".test.", ".spec.", "generated", "snapshot")):
            continue
        candidates.append(path)

    selected: list[StableSymbol] = []
    seen_symbols: set[str] = set()
    for path in sorted(candidates)[:500]:
        timestamp_text = git_text(root, "log", "-1", "--format=%ct", "--", path)
        if not timestamp_text:
            continue
        try:
            changed_at = int(timestamp_text)
        except ValueError:
            continue
        if changed_at >= cutoff:
            continue
        response = client.tool("outline", {"target": path})
        if response.get("success") is not True:
            continue
        for line in str(response.get("text", "")).splitlines():
            match = TOP_LEVEL_FUNCTION_RE.match(line)
            if not match:
                continue
            symbol = match.group(1)
            if symbol in seen_symbols or symbol in {"main", "new", "default"}:
                continue
            selected.append(StableSymbol(path=path, symbol=symbol, changed_at=changed_at))
            seen_symbols.add(symbol)
            if len(selected) == count:
                return selected
    raise SoakError(
        f"found only {len(selected)} top-level functions last changed more than "
        f"{older_than_days} days ago; need {count}"
    )


def public_response(response: Mapping[str, Any]) -> dict[str, Any]:
    return {field: response[field] for field in PUBLIC_RESPONSE_FIELDS if field in response}


def normalize_paths(value: Any, prefixes: Sequence[Path]) -> Any:
    normalized_prefixes = sorted(
        {candidate for prefix in prefixes for candidate in (str(prefix), str(prefix.resolve()))},
        key=len,
        reverse=True,
    )

    def normalize(item: Any) -> Any:
        if isinstance(item, str):
            result = item
            for prefix in normalized_prefixes:
                result = result.replace(prefix, "<root>")
                result = result.replace(prefix.replace("/", "\\"), "<root>")
            return result.replace("\\", "/")
        if isinstance(item, list):
            return [normalize(child) for child in item]
        if isinstance(item, dict):
            return {key: normalize(item[key]) for key in sorted(item)}
        return item

    return normalize(value)


def canonical_output(response: Mapping[str, Any], prefixes: Sequence[Path]) -> str:
    value = normalize_paths(public_response(response), prefixes)
    return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False)


def first_differing_line(left: str, right: str) -> tuple[int, str, str] | None:
    left_lines = left.splitlines()
    right_lines = right.splitlines()
    for index in range(max(len(left_lines), len(right_lines))):
        left_line = left_lines[index] if index < len(left_lines) else "<EOF>"
        right_line = right_lines[index] if index < len(right_lines) else "<EOF>"
        if left_line != right_line:
            return index + 1, left_line, right_line
    return None


def extract_embedding_calls(response: Mapping[str, Any]) -> int:
    structured = response.get("structuredContent")
    if not isinstance(structured, Mapping):
        return 0
    search = structured.get("search")
    if not isinstance(search, Mapping):
        return 0
    value = search.get("embedding_calls", 0)
    return int(value) if isinstance(value, (int, float)) else 0


def current_generation(view_dir: Path) -> str | None:
    pointer = view_dir / "pointer.sqlite"
    if not pointer.is_file():
        return None
    uri = pointer.resolve().as_uri() + "?mode=ro&immutable=1"
    try:
        with sqlite3.connect(uri, uri=True, timeout=5.0) as connection:
            row = connection.execute(
                "SELECT generation FROM pointer WHERE singleton = 1"
            ).fetchone()
    except sqlite3.Error as error:
        raise SoakError(f"could not read view pointer {pointer}: {error}") from error
    if not row or not row[0]:
        return None
    return str(row[0])


def manifest_paths(view_dir: Path) -> list[Path]:
    return sorted(
        path
        for path in view_dir.glob("manifest-*.json")
        if path.is_file() and re.fullmatch(r"manifest-\d+-[0-9a-f]+\.json", path.name)
    )


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SoakError(f"could not read manifest {path}: {error}") from error
    if not isinstance(value, dict) or not isinstance(value.get("entries"), list):
        raise SoakError(f"manifest has no entries array: {path}")
    return value


def manifest_entry_count(view_dir: Path, generation: str | None = None) -> int:
    generation = generation if generation is not None else current_generation(view_dir)
    if generation is None:
        return 0
    path = view_dir / f"manifest-{generation}.json"
    return len(load_manifest(path)["entries"])


def view_accounting(storage_root: Path, root: Path, scope: str) -> dict[str, Any]:
    view_dir = storage_root / "views" / scope
    if not view_dir.is_dir():
        raise SoakError(f"view store does not exist: {view_dir}")
    generation = current_generation(view_dir)
    manifests = manifest_paths(view_dir)
    fingerprints: dict[str, list[tuple[Path, bytes]]] = {}
    current_entries = 0
    for path in manifests:
        match = re.fullmatch(r"manifest-(\d+)-([0-9a-f]+)\.json", path.name)
        assert match is not None
        manifest = load_manifest(path)
        entries_bytes = canonical_json_bytes(manifest["entries"])
        fingerprints.setdefault(match.group(2), []).append((path, entries_bytes))
        if generation is not None and path.name == f"manifest-{generation}.json":
            current_entries = len(manifest["entries"])
    duplicate_groups = []
    for fingerprint, records in sorted(fingerprints.items()):
        by_entries: dict[bytes, list[str]] = {}
        for path, entries in records:
            by_entries.setdefault(entries, []).append(path.name)
        for names in by_entries.values():
            if len(names) > 1:
                duplicate_groups.append(
                    {"head_tree": fingerprint, "manifests": sorted(names)}
                )

    cache_keys_path = storage_root / "cache-keys.json"
    try:
        cache_keys = json.loads(cache_keys_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise SoakError(f"could not read {cache_keys_path}: {error}") from error
    family_record = cache_keys.get(str(root.resolve())) if isinstance(cache_keys, dict) else None
    family = family_record.get("key") if isinstance(family_record, dict) else None
    if not isinstance(family, str) or not family:
        raise SoakError(f"no artifact family recorded for {root} in {cache_keys_path}")
    blob_dir = storage_root / "blobs" / family
    blob_sizes = {
        str(path.relative_to(blob_dir)): path.stat().st_size
        for path in sorted(blob_dir.rglob("*"))
        if path.is_file()
    }
    derived = view_dir / "derived.sqlite"
    return {
        "scope": scope,
        "family": family,
        "generation": generation,
        "manifest_count": len(manifests),
        "head_tree_count": len(fingerprints),
        "duplicate_generation": bool(duplicate_groups),
        "duplicate_generations": duplicate_groups,
        "entries": current_entries,
        "derived_sqlite_bytes": derived.stat().st_size if derived.is_file() else 0,
        "blob_store_bytes": blob_sizes,
        "blob_store_total_bytes": sum(blob_sizes.values()),
    }


def parse_cpu_time(value: str) -> float:
    text = value.strip()
    days = 0
    if "-" in text:
        day_text, text = text.split("-", 1)
        days = int(day_text)
    parts = text.split(":")
    if len(parts) == 3:
        hours, minutes, seconds = parts
    elif len(parts) == 2:
        hours = "0"
        minutes, seconds = parts
    else:
        hours = minutes = "0"
        seconds = parts[0]
    return days * 86400 + int(hours) * 3600 + int(minutes) * 60 + float(seconds)


def sample_process(pid: int) -> ProcessSample:
    result = run_checked(["ps", "-p", str(pid), "-o", "time=", "-o", "rss="])
    fields = result.stdout.decode("utf-8", errors="replace").split()
    if len(fields) != 2:
        raise SoakError(f"unexpected ps output for pid {pid}: {result.stdout!r}")
    return ProcessSample(cpu_s=parse_cpu_time(fields[0]), rss_kib=int(fields[1]))


def find_subc_daemon_pid() -> int:
    output = run_checked(["ps", "-axo", "pid=,command="]).stdout.decode(
        "utf-8", errors="replace"
    )
    matches = []
    for line in output.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) != 2:
            continue
        command = fields[1]
        tokens = command.split()
        if not tokens:
            continue
        if Path(tokens[0]).name not in {"aft", "ck-aft"}:
            continue
        if any(token == "--subc" or token.startswith("--subc=") for token in tokens[1:]):
            matches.append(int(fields[0]))
    if len(matches) != 1:
        raise SoakError(f"expected exactly one running AFT subc daemon, found {matches}")
    return matches[0]


def health_snapshot(binary: Path) -> dict[str, Any]:
    ck = shutil.which("ck")
    if ck is None:
        raise SoakError("ck is not on PATH; cannot capture daemon health")
    health_result = run_checked([ck, "health", "aft", "--json"], timeout_s=60.0)
    memory_result = run_checked([binary, "profile", "--memory", "--json"], timeout_s=60.0)
    try:
        health = json.loads(health_result.stdout)
        memory = json.loads(memory_result.stdout)
    except json.JSONDecodeError as error:
        raise SoakError(f"health command returned invalid JSON: {error}") from error
    metrics = health.get("metrics", {}) if isinstance(health, dict) else {}
    return {
        "dispatch_liveness": metrics.get("dispatch_liveness"),
        "memory_process": memory.get("process") if isinstance(memory, dict) else None,
    }


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    temporary.replace(path)


def append_json_line(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False))
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())


def latest_json_line(path: Path) -> dict[str, Any] | None:
    latest: dict[str, Any] | None = None
    with path.open(encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            if not line.strip():
                continue
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                raise SoakError(f"invalid JSONL in {path}:{number}: {error}") from error
            if not isinstance(value, dict):
                raise SoakError(f"non-object JSONL row in {path}:{number}")
            latest = value
    return latest


def markdown_cell(value: Any) -> str:
    return str(value).replace("|", "\\|").replace("\n", "<br>")
