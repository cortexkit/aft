#!/usr/bin/env python3
"""Acquire checkout-state baselines; do not treat partial coverage as a pass.

Build: cargo build -p agent-file-tools --release --bin aft --example views_perf_daemon
Test:  python3 scripts/test_views_performance_gate.py
Run:   python3 scripts/views-performance-gate.py --help
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import select
import shutil
import signal
import subprocess
import tempfile
import sys
import time

COUNTERS = (
    "source_hashes", "source_hash_bytes", "content_reads", "content_read_bytes",
    "full_walks", "applicability_walks", "logical_aggregates", "own_generation_folds",
)
PROBE = "views_perf_probe.rs"
SEMANTIC_QUERY = "function probe_entry in views_perf_probe.rs"
EDIT_STATES = ("atomic_edit", "revert", "tool_write", "tool_revert", "branch_switch", "branch_return")


def round_states(roots, round_number):
    return [f"round{round_number}:root{root}:{state}" for root in range(roots) for state in EDIT_STATES]

BASE = "pub fn probe_alpha() -> usize { 11 }\npub fn probe_beta() -> usize { 29 }\npub fn probe_entry() -> usize { probe_alpha() } // TODO VIEWS_PERF_STATE_ALPHA\n"
CHANGED = BASE.replace("{ probe_alpha() } // TODO VIEWS_PERF_STATE_ALPHA", "{ probe_beta() } // TODO VIEWS_PERF_STATE_BETA")


class MeasurementError(RuntimeError):
    pass


def validate_subject(expected_pid, reported_pid, expected_registry, reported_registry):
    if expected_pid <= 1 or expected_pid != reported_pid:
        raise MeasurementError(f"wrong subject PID: expected {expected_pid}, got {reported_pid}")
    if not expected_registry or expected_registry != reported_registry:
        raise MeasurementError("clients reached different family registry/storage roots")


def require_counters(counters):
    if not isinstance(counters, dict):
        raise MeasurementError("missing production counters")
    for name in COUNTERS:
        if type(counters.get(name)) is not int or counters[name] < 0:
            raise MeasurementError(f"missing/invalid production counter {name}")
    return counters


def require_base_ref_prototype(counters, *, semantic=True):
    # Each arm starts a fresh daemon; the opt-in selects the base-ref prototype
    # only when semantic search is enabled.
    expected = (os.environ.get("AFT_VIEWS_BASE_REF_PROTOTYPE") == "1"
                and semantic)
    active = counters.get("base_ref_prototype") is True
    if expected and not active:
        raise MeasurementError("prototype-on acquisition did not report metrics.views_work.base_ref_prototype: true")
    if not expected and active:
        raise MeasurementError("inactive arm unexpectedly reported metrics.views_work.base_ref_prototype: true")


def counter_delta(before, after):
    require_counters(before)
    require_counters(after)
    delta = {name: after[name] - before[name] for name in COUNTERS}
    if any(value < 0 for value in delta.values()):
        raise MeasurementError("production counters regressed (daemon restart or wrong subject)")
    return delta


def validate_witnesses(expected, observed):
    if not expected or not observed:
        raise MeasurementError("zero workload cases/witnesses")
    ids = [row["id"] for row in observed]
    if len(set(ids)) != len(ids):
        raise MeasurementError("duplicate state witness")
    for state in expected:
        if state not in ids:
            raise MeasurementError(f"skipped intermediate state {state}")
    if ids != [state for state in expected if state in ids]:
        raise MeasurementError("state witnesses out of order or unexpected")
    for row in observed:
        if not row.get("correct"):
            raise MeasurementError(f"incorrect witness {row['id']}")


def optional_delta(before, after):
    if before is None or after is None:
        return None
    if after < before:
        raise MeasurementError("kernel counter regressed")
    return after - before


def percentile(values, percent):
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)] if values else None


def graph_witness(answer, callee):
    if answer.get("complete") is False or answer.get("status") in ("building", "unavailable"):
        return False
    # Search only answer rows, never the echoed query or explanatory error text.
    children = answer.get("children", [])
    return (answer.get("name") == "probe_entry" and answer.get("truncated", 0) == 0
            and [child.get("name") for child in children] == [callee]
            and all(child.get("resolved") is True for child in children))


def lexical_witness(grep, glob, search, path, token):
    """Require current content from the intended checkout, not a sibling's row.

    Search may be partial because its semantic lane is disabled; this checks only
    lexical usefulness, never full search coverage.
    """
    matches = grep.get("matches", [])
    return (grep.get("success") is True and grep.get("complete") is True
            and len(matches) == 1 and Path(matches[0].get("file", "")) == path
            and token in matches[0].get("line_text", "")
            and glob.get("success") is True and glob.get("complete") is True and glob.get("total") == 1
            and [Path(file) for file in glob.get("files", [])] == [path]
            and search.get("success") is True
            and any(Path(row.get("file", "")) == path and token in row.get("snippet", "")
                    for row in search.get("results", [])))


def semantic_witness(answer, path, token):
    return (answer.get("success") is True and answer.get("semantic_status") == "ready"
            and answer.get("lanes", {}).get("semantic", {}).get("status") == "ready"
            and answer.get("complete") is True
            and any(Path(row.get("file", "")) == path and token in row.get("snippet", "")
                    for row in answer.get("results", [])))


def checked(args, cwd=None, env=None):
    return subprocess.check_output([str(arg) for arg in args], cwd=cwd, env=env, stderr=subprocess.PIPE).decode().strip()


def git(root, *args):
    return checked(["git", "-c", "core.hooksPath=/dev/null", "-C", root, *args])


def dump(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def sha(path):
    with Path(path).open("rb") as source:
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def proc_stat(pid):
    raw = Path(f"/proc/{pid}/stat").read_text()
    fields = raw[raw.rindex(")") + 2:].split()
    return {"pid": pid, "ppid": int(fields[1]), "start_ticks": int(fields[19]),
            "ticks": sum(int(fields[index]) for index in (11, 12, 13, 14)),
            "rss_bytes": int(fields[21]) * os.sysconf("SC_PAGE_SIZE")}


def process_sample(pid, start_ticks=None):
    """Live descendant totals plus waited-child accounting; never sample a name match.

    Linux propagates waited-child CPU and I/O to the waiting parent. Summing the
    live tree includes LSP/model children but not the broker or Git workload.
    Short-lived unreaped children can be missed: this is not cgroup accounting.
    """
    root = proc_stat(pid)
    if start_ticks is not None and root["start_ticks"] != start_ticks:
        raise MeasurementError("subject PID was reused")
    all_processes = {}
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            try:
                row = proc_stat(int(entry.name))
                all_processes[row["pid"]] = row
            except (OSError, ValueError):
                pass
    owned = {pid: root}
    while True:
        children = {key: row for key, row in all_processes.items() if row["ppid"] in owned}
        new = children.keys() - owned.keys()
        if not new:
            break
        owned.update({key: children[key] for key in new})
    physical = 0
    for key in owned:
        try:
            io = dict(line.split(": ", 1) for line in Path(f"/proc/{key}/io").read_text().splitlines())
            physical += int(io["write_bytes"])
        except (OSError, KeyError, ValueError):
            physical = None
            break
    return {"monotonic_s": time.monotonic(), "pid": pid, "start_ticks": root["start_ticks"],
            "cpu_seconds": sum(row["ticks"] for row in owned.values()) / os.sysconf("SC_CLK_TCK"),
            "physical_bytes_written": physical,
            "rss_bytes": sum(row["rss_bytes"] for row in owned.values()),
            "processes": list(owned.values())}


def storage_sample(root):
    apparent = allocated = 0
    files = 0
    for path in root.rglob("*"):
        try:
            if path.is_file():
                stat = path.stat()
                apparent += stat.st_size
                allocated += stat.st_blocks * 512
                files += 1
        except FileNotFoundError:
            pass
    return {"apparent_bytes": apparent, "allocated_bytes": allocated, "files": files,
            "unique_shared_extent_bytes": None,
            "family_registries": sorted(str(path.relative_to(root)) for path in root.rglob("members.sqlite"))}


class Broker:
    """Keep checkout sessions bound to one Agent File Tools daemon over its
    production authenticated subprocess-controller transport, rather than
    launching a separate indexer for each session.
    """
    def __init__(self, binary, broker, out, root, request_timeout=120):
        self.request_timeout = request_timeout
        self.failed = False
        self.out = out
        self.buffer = b""
        self.stderr = (out / "broker.log").open("w")
        env = {key: os.environ[key] for key in ("PATH", "TMPDIR", "LD_LIBRARY_PATH", "SSL_CERT_FILE", "SSL_CERT_DIR", "AFT_VIEWS_BASE_REF_PROTOTYPE") if key in os.environ}
        env.update({"RUST_LOG": "info", "RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": ""})
        self.proc = subprocess.Popen([str(broker), str(binary), str(out), str(root)],
                                     env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, start_new_session=True)
        try:
            self.identity = self.read(90)
            self.pid = self.identity["pid"]
            self.registry = str(out / "storage")
            self.initial_sample = process_sample(self.pid)
            validate_subject(self.pid, self.identity["pid"], self.registry, self.identity["registry"])
        except BaseException:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
            self.stderr.close()
            raise

    def read(self, timeout):
        stdout = self.proc.stdout
        if stdout is None:
            raise MeasurementError("public transport stdout pipe is unavailable")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                return json.loads(line)
            ready, _, _ = select.select([stdout], [], [], max(0, deadline - time.monotonic()))
            if ready:
                chunk = os.read(stdout.fileno(), 65536)
                if not chunk:
                    raise MeasurementError(f"broker exited: {self.out / 'broker.log'}")
                self.buffer += chunk
        raise MeasurementError(f"public transport timeout; see {self.out / 'daemon.log'}")

    def call(self, op, timeout=None, **args):
        if self.failed:
            raise MeasurementError("transport already failed; a late response cannot serve another request")
        try:
            stdin = self.proc.stdin
            if stdin is None:
                raise MeasurementError("public transport stdin pipe is unavailable")
            stdin.write((json.dumps({"op": op, **args}) + "\n").encode())
            stdin.flush()
            return self.read(min(self.request_timeout, timeout if timeout is not None else self.request_timeout))
        except (MeasurementError, OSError) as error:
            self.failed = True
            raise MeasurementError(str(error)) from error

    def health(self):
        response = self.call("health")
        report = response.get("report", response)
        metrics = report.get("metrics", {})
        counters = require_counters(metrics.get("views_work"))
        validate_subject(self.pid, counters.get("pid"), self.registry, self.identity["registry"])
        process_sample(self.pid, self.initial_sample["start_ticks"])
        return report

    def sample(self):
        return {"process": process_sample(self.pid, self.initial_sample["start_ticks"]), "health": self.health()}

    def close(self):
        if self.proc.poll() is None and self.failed:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=5)
        if self.proc.poll() is None:
            try:
                stdin = self.proc.stdin
                if stdin is not None:
                    stdin.write(b'{"op":"quit"}\n')
                    stdin.flush()
                self.proc.wait(timeout=20)
            except (BrokenPipeError, subprocess.TimeoutExpired):
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait()
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        self.stderr.close()


def configuration(semantic, model):
    config = {
        "indexes": {"trigram": True, "semantic": semantic, "callgraph": True},
        "disabled_tools": [],
        "inspect": {"enabled": True},
        "lsp": {"auto_install": False, "disabled": ["rust", "typescript", "python", "go", "bash", "yaml", "ty"]},
        "semantic": {"backend": "fastembed", "max_files": 20000},
    }
    if model:
        config["semantic"]["model"] = model
    return config


def provision(source, pin, out, count, config):
    """Mutate only fresh clones below an exclusive, newly created output directory."""
    primary = out / "repo"
    checked(["git", "clone", "--shared", "--no-checkout", source, primary])
    git(primary, "checkout", "--detach", pin)
    original_files = git(primary, "ls-files", "-z").split("\0")
    original_files = [path for path in original_files if path]
    inventory = {"commit": git(primary, "rev-parse", "HEAD"), "tracked_files": len(original_files),
                 "tracked_bytes": sum((primary / path).lstat().st_size for path in original_files if (primary / path).is_file()),
                 "policy_sha256": {path: sha(primary / path) for path in original_files
                                   if Path(path).name in (".gitignore", ".gitattributes", "aft.jsonc") and (primary / path).is_file()}}
    if inventory["commit"] != pin:
        raise MeasurementError("corpus checkout is not the immutable manifest commit")
    git(primary, "config", "user.name", "Views measurement harness")
    git(primary, "config", "user.email", "views-harness@example.invalid")
    git(primary, "config", "commit.gpgsign", "false")
    # Project tiers and both global config locations are identical in both arms.
    config_paths = {".cortexkit/aft.jsonc", ".cortexkit/aft.opencode.jsonc"}
    config_paths.update(path for path in original_files if Path(path).name.startswith("aft") and path.endswith(".jsonc") and ".cortexkit/" in path)
    for path in config_paths:
        destination = primary / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        dump(destination, config)
    (primary / PROBE).write_text(BASE)
    git(primary, "add", PROBE)
    for path in config_paths:
        git(primary, "add", "--force", path)
    git(primary, "-c", "core.quotePath=false", "commit", "-m", "measurement base and frozen config")
    base = git(primary, "rev-parse", "HEAD")
    (primary / PROBE).write_text(CHANGED)
    git(primary, "add", PROBE)
    git(primary, "commit", "-m", "measurement branch variant")
    changed = git(primary, "rev-parse", "HEAD")
    git(primary, "checkout", "--detach", base)
    roots = [primary]
    for index in range(1, count):
        root = out / f"root-{index}"
        git(primary, "worktree", "add", "--detach", root, base)
        roots.append(root)
    for directory in ("home/.config/cortexkit", "config/cortexkit", "home", "data", "cache", "storage"):
        (out / directory).mkdir(parents=True, exist_ok=True)
    (out / "storage/views-harness.marker").write_text("isolated views performance subject\n")
    dump(out / "config/cortexkit/aft.jsonc", config)
    dump(out / "home/.config/cortexkit/aft.jsonc", config)
    inventory.update({"base_workload_commit": base, "changed_workload_commit": changed,
                      "base_workload_tree": git(primary, "rev-parse", f"{base}^{{tree}}"),
                      "changed_workload_tree": git(primary, "rev-parse", f"{changed}^{{tree}}"),
                      "common_git_dir": git(primary, "rev-parse", "--git-common-dir"),
                      "config_override_paths": sorted(config_paths), "config_sha256": sha(out / "config/cortexkit/aft.jsonc")})
    return roots, inventory


class Acquisition:
    def __init__(self, broker, out, timeout, semantic, ready_timeout=600):
        self.broker = broker
        self.ready_timeout = ready_timeout
        self.active_deadline = None
        self.active_state = None
        self.roots = {}
        self.out = out
        self.timeout = timeout
        self.semantic = semantic
        self.queries = []
        self.samples = []
        self.expected = []
        self.witnesses = []
        self.binds = []

    def append(self, filename, row):
        with (self.out / filename).open("a") as stream:
            stream.write(json.dumps(row, sort_keys=True) + "\n")

    def sample(self, label):
        row = {"label": label, **self.broker.sample()}
        self.samples.append(row)
        self.append("samples.jsonl", row)
        return row

    def tool(self, channel, name, arguments, state):
        start = time.monotonic()
        budget = max(0.001, self.active_deadline - start) if self.active_deadline is not None else None
        try:
            response = self.broker.call("tool", timeout=budget, channel=channel, name=name, arguments=arguments)
        except MeasurementError as error:
            row = {"state": state, "channel": channel, "tool": "aft_search" if name == "search" else name,
                   "start_s": start, "wall_s": time.monotonic() - start, "error": True,
                   "request_budget_s": min(self.broker.request_timeout, budget) if budget is not None else self.broker.request_timeout,
                   "failure": str(error)}
            self.queries.append(row)
            self.append("queries.jsonl", row)
            raise
        answer = response.get("structuredContent", response)
        row = {"state": state, "channel": channel, "tool": "aft_search" if name == "search" else name,
               "start_s": start, "wall_s": time.monotonic() - start,
               "error": response.get("isError", False), "response": response}
        self.queries.append({key: value for key, value in row.items() if key != "response"})
        self.append("queries.jsonl", row)
        return answer

    def response_completed_s(self):
        row = self.queries[-1]
        return row["start_s"] + row["wall_s"]

    def record_failed_state(self, reason):
        if self.active_state is None:
            return
        row = {key: value for key, value in self.active_state.items() if key != "since"}
        row.update({"correct": False, "time_to_correct_s": None,
                    "elapsed_s": time.monotonic() - self.active_state["since"], "failure": reason})
        self.witnesses.append(row)
        self.append("witnesses.jsonl", row)
        self.active_state = None
        self.active_deadline = None

    def witness(self, channel, state, changed, since, *, require_indexed=False):
        self.active_state = {"id": state, "channel": channel, "since": since, "state_started_s": since, "planes_s": {}}
        deadline = since + (self.ready_timeout if require_indexed else self.timeout)
        self.active_deadline = deadline
        expected_source_sha = hashlib.sha256((CHANGED if changed else BASE).encode()).hexdigest()
        observed_source_sha = sha(self.roots[channel] / PROBE)
        if observed_source_sha != expected_source_sha:
            raise MeasurementError(f"workload source does not match intended state: {state}")
        callee = "probe_beta" if changed else "probe_alpha"
        token = "VIEWS_PERF_STATE_BETA" if changed else "VIEWS_PERF_STATE_ALPHA"
        planes = self.active_state["planes_s"]
        attempts = 0
        latest = {}
        graph = {}
        while time.monotonic() < deadline:
            attempts += 1
            checks = {}

            def observe(plane, correct):
                checks[plane] = correct
                if correct and plane not in planes:
                    planes[plane] = self.response_completed_s() - since

            grep = self.tool(channel, "grep", {"pattern": token, "path": PROBE}, state)
            glob = self.tool(channel, "glob", {"pattern": f"**/{PROBE}"}, state)
            search = self.tool(channel, "search", {"query": "probe_entry", "pattern": "probe_entry", "includeTests": True}, state)
            lexical_correct = lexical_witness(grep, glob, search, self.roots[channel] / PROBE, token)
            lexical_indexed = (lexical_correct and grep.get("index", {}).get("trigram", {}).get("status") == "ready"
                               and glob.get("index", {}).get("trigram", {}).get("status") == "ready"
                               and search.get("lanes", {}).get("trigram", {}).get("status") == "ready"
                               and not grep.get("fallback") and not glob.get("fallback"))
            observe("lexical_answer", lexical_correct)
            observe("lexical_indexed", lexical_indexed)
            graph = self.tool(channel, "callgraph", {"path": PROBE, "symbol": "probe_entry", "op": "call_tree", "depth": 2, "includeTests": True}, state)
            observe("callgraph", graph.get("success") is True and graph_witness(graph, callee))
            zoom = self.tool(channel, "zoom", {"path": PROBE, "symbols": "probe_entry", "callgraph": True}, state)
            observe("zoom", zoom.get("success") is True and token in zoom.get("content", "")
                    and [row.get("name") for row in zoom.get("annotations", {}).get("calls_out", [])] == [callee])
            inspect = self.tool(channel, "inspect", {"scope": PROBE, "sections": ["todos"]}, state)
            observe("inspect_todos", inspect.get("success") is True and inspect.get("complete") is True
                    and token in json.dumps(inspect.get("details", {})))
            latest = checks
            if self.semantic:
                semantic_answer = self.tool(channel, "search", {"query": SEMANTIC_QUERY, "includeTests": True, "topK": 20}, state)
                semantic_status = semantic_answer.get("semantic_status")
                observe("semantic", semantic_witness(semantic_answer, self.roots[channel] / PROBE, token))
            else:
                semantic_status = search.get("semantic_status")
            required = ["lexical_answer", "callgraph", "zoom", "inspect_todos"]
            if self.semantic:
                required.append("semantic")
            if require_indexed:
                required.append("lexical_indexed")
            if all(checks[key] for key in required):
                if sha(self.roots[channel] / PROBE) != expected_source_sha:
                    raise MeasurementError(f"workload source changed during witness: {state}")
                row = {"id": state, "channel": channel, "state_started_s": since,
                       "correct": True, "attempts": attempts,
                       "time_to_correct_s": time.monotonic() - since, "planes_s": planes,
                       "semantic": {"status": semantic_status, "bind_to_ready_s": planes.get("semantic")},
                       "source_sha256": observed_source_sha, "expected_source_sha256": expected_source_sha}
                self.witnesses.append(row)
                self.append("witnesses.jsonl", row)
                self.active_deadline = None
                self.active_state = None
                self.sample(state)
                return row
            self.sample(state + ":pending")
            time.sleep(0.25)
        row = {"id": state, "channel": channel, "state_started_s": since, "correct": False, "attempts": attempts,
               "time_to_correct_s": None, "elapsed_s": time.monotonic() - since, "planes_s": planes, "last_checks": latest,
               "last_callgraph": graph}
        self.witnesses.append(row)
        self.append("witnesses.jsonl", row)
        self.active_state = None
        self.active_deadline = None
        raise MeasurementError(f"intermediate state never became correct: {state}: {latest}; callgraph={graph}")

    def bind(self, channel, root):
        self.roots[channel] = root
        before = self.sample(f"bind:{channel}:before")
        start = time.monotonic()
        response = self.broker.call("bind", channel=channel, root=str(root))
        ack_s = time.monotonic() - start
        health = self.broker.health()
        validate_subject(self.broker.pid, health["metrics"]["views_work"]["pid"], self.broker.registry, self.broker.identity["registry"])
        row = {"channel": channel, "root": str(root), "pid": self.broker.pid, "registry": self.broker.registry,
               "started_s": start, "ack_s": ack_s, "response": response, "before": before}
        self.binds.append(row)
        return start

    def idle(self, seconds, label):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.sample(label)
            time.sleep(min(5, max(0, deadline - time.monotonic())))

    def workload(self, roots, inventory, activity_seconds, cycle_seconds):
        deadline = time.monotonic() + activity_seconds
        round_number = 0
        self.expected.extend(["worktree_create", "worktree_remove"])
        while True:
            self.expected[-2:-2] = round_states(len(roots), round_number)
            for index, root in enumerate(roots):
                channel = index + 1
                round_start = time.monotonic()
                for name, contents, writer in [("atomic_edit", CHANGED, "atomic"), ("revert", BASE, "atomic"),
                                                ("tool_write", CHANGED, "tool"), ("tool_revert", BASE, "tool")]:
                    state = f"round{round_number}:root{index}:{name}"
                    start = time.monotonic()
                    if writer == "atomic":
                        temporary = root / (PROBE + ".save")
                        temporary.write_text(contents)
                        temporary.replace(root / PROBE)
                    else:
                        answer = self.tool(channel, "write", {"path": PROBE, "content": contents}, state)
                        if answer.get("success") is not True:
                            raise MeasurementError(f"public write failed: {state}")
                    self.witness(channel, state, contents == CHANGED, start)
                for name, commit, changed in [("branch_switch", inventory["changed_workload_commit"], True),
                                               ("branch_return", inventory["base_workload_commit"], False)]:
                    start = time.monotonic()
                    git(root, "checkout", "--detach", commit)
                    self.witness(channel, f"round{round_number}:root{index}:{name}", changed, start)
                if time.monotonic() < deadline:
                    self.idle(max(0, cycle_seconds - (time.monotonic() - round_start)), "stagger")
            round_number += 1
            if time.monotonic() >= deadline:
                break
        transient = self.out / "transient-root"
        start = time.monotonic()
        git(roots[0], "worktree", "add", "--detach", transient, inventory["base_workload_commit"])
        channel = len(roots) + 1
        self.bind(channel, transient)
        self.witness(channel, "worktree_create", False, start, require_indexed=True)
        self.broker.call("unbind", channel=channel)
        git(roots[0], "worktree", "remove", transient)
        # Removing a root must not contaminate the surviving checkout's answers.
        self.witness(1, "worktree_remove", False, time.monotonic())
        validate_witnesses(self.expected, self.witnesses)


def distributions(rows):
    tools = sorted({row["tool"] for row in rows})
    return {tool: {"queries": len(samples := [row for row in rows if row["tool"] == tool]),
                   "errors": sum(row["error"] for row in samples),
                   "p50_s": percentile([row["wall_s"] for row in samples], 50),
                   "p95_s": percentile([row["wall_s"] for row in samples], 95),
                   "max_s": max(row["wall_s"] for row in samples)} for tool in tools}


def run_arm(args, corpus, count, out):
    out.mkdir(parents=True, exist_ok=False)
    config = configuration(args.semantic, args.model)
    subject = args.subjects / out.name
    subject.mkdir(parents=True, exist_ok=False)
    broker = None
    acquisition = None
    # Built before provisioning so a failed clone or checkout still leaves a
    # result.json, and the caller's matrix keeps the arms that did finish.
    result = {"schema": 1, "corpus": args.corpus, "roots": count,
              "profile": "witness-gated-structural-acquisition-v2", "config": config,
              "plane_clock": "public_response_completion",
              "inventory": None, "status": "failed", "gates_certified": [],
              "semantic": {"enabled": args.semantic, "backend": "fastembed", "model": args.model,
                           "status": "unavailable" if not args.semantic else "not_yet_observed"},
              "deadlines_seconds": {"state": args.state_timeout, "ready": args.ready_timeout, "request": args.request_timeout},
              "graph_inspect_primary_sample": args.graph_inspect,
              "durations_minutes": {"warmup": args.warmup_minutes, "activity_minimum": args.activity_minutes, "quiet": args.quiet_minutes},
              "limitations": [
                  "One paired acquisition, not 30 bind roots, 3 cold/import builds, 5 steady repetitions or 200 warm queries.",
                  "Unattested linked worktrees; no trusted-provisioning fast path, old-cache upgrade cohort or sealed donor arm.",
                  "Bind readiness is an indexed witness answer, not proof of complete repository-wide coverage.",
                  "State transitions wait for witnesses; cadence overruns are included, never skipped. This is not fixed-rate concurrent G2.",
                  "Process-tree /proc accounting includes live and waited children; short-lived unreaped children can be missed and subprocess roles are not classified.",
                  "Counter scope is instrumented sites (see scripts/views-performance-contract.md); no whole-program zero-work claim.",
                  "Kernel write_bytes is writeback accounting, not APFS unique/shared extent accounting; G3/G6 platform verdict unknown.",
                  "No independent full cold graph oracle or ranked semantic quality replay in this acquisition.",
              ]}
    try:
        roots, inventory = provision(args.source, corpus["commit"], subject, count, config)
        dump(out / "inventory.json", inventory)
        result["inventory"] = inventory
        broker = Broker(args.binary, args.broker, subject, roots[0], args.request_timeout)
        acquisition = Acquisition(broker, out, args.state_timeout, args.semantic, args.ready_timeout)
        result["identity"] = broker.identity
        initial = acquisition.sample("startup")
        result["initial"] = initial
        # Charge base construction once and retain its route while workers bind.
        acquisition.expected.extend(f"bind:root{index}" for index in range(count))
        start = acquisition.bind(1, roots[0])
        ready = acquisition.witness(1, "bind:root0", False, start, require_indexed=True)
        acquisition.binds[-1]["ready"] = ready
        acquisition.binds[-1]["after"] = acquisition.samples[-1]
        result["base_ready"] = acquisition.sample("base_ready")
        pending = []
        for index, root in enumerate(roots[1:], 2):
            pending.append((index, acquisition.bind(index, root)))
        for channel, start in pending:
            ready = acquisition.witness(channel, f"bind:root{channel - 1}", False, start, require_indexed=True)
            acquisition.binds[channel - 1]["ready"] = ready
            acquisition.binds[channel - 1]["after"] = acquisition.samples[-1]
        if args.graph_inspect:
            acquisition.tool(1, "inspect", {"sections": ["dead_code", "cycles"]}, "graph_inspect_primary")
        acquisition.idle(args.warmup_minutes * 60, "warmup")
        result["activity_start"] = acquisition.sample("activity_start")
        acquisition.workload(roots, inventory, args.activity_minutes * 60, args.cycle_seconds)
        result["activity_end"] = acquisition.sample("activity_end")
        # Include delayed checkpoint/writeback work in a separate, explicit tail.
        acquisition.idle(max(2, args.quiet_minutes * 60), "quiet")
        final = acquisition.sample("final")
        require_base_ref_prototype(final["health"]["metrics"]["views_work"],
                                   semantic=args.semantic)
        result["final"] = final
        result["storage"] = storage_sample(subject / "storage")
        result["counter_delta"] = counter_delta(initial["health"]["metrics"]["views_work"], final["health"]["metrics"]["views_work"])
        for label, before in [("total_since_startup", initial), ("activity", result["activity_start"]), ("quiet", result["activity_end"])]:
            after = result["activity_end"] if label == "activity" else final
            wall = after["process"]["monotonic_s"] - before["process"]["monotonic_s"]
            cpu = optional_delta(before["process"]["cpu_seconds"], after["process"]["cpu_seconds"])
            result[label] = {"wall_s": wall, "cpu_seconds": cpu, "mean_cores": cpu / wall if cpu is not None else None,
                             "physical_bytes_written": optional_delta(before["process"]["physical_bytes_written"], after["process"]["physical_bytes_written"]),
                             "counter_delta": counter_delta(before["health"]["metrics"]["views_work"], after["health"]["metrics"]["views_work"])}
        result["process_lifetime_totals_including_startup"] = final["process"]
        for root in roots:
            if git(root, "diff", "--name-only") or (root / PROBE).read_text() != BASE:
                raise MeasurementError(f"workload failed restoration: {root}")
        result["status"] = "measured"
        result["semantic"]["status"] = "ready" if args.semantic and all("semantic" in row["planes_s"] for row in acquisition.witnesses) else "unavailable"
    except (MeasurementError, subprocess.SubprocessError, OSError, KeyError) as error:
        result["failure"] = str(error)
        if acquisition:
            acquisition.record_failed_state(str(error))
        if broker:
            try:
                result["failure_process"] = process_sample(broker.pid, broker.initial_sample["start_ticks"])
                result["storage"] = storage_sample(subject / "storage")
                result["failure_snapshot"] = broker.sample()
            except (MeasurementError, OSError, KeyError):
                pass
    finally:
        if acquisition:
            result["binds"] = acquisition.binds
            result["expected_states"] = acquisition.expected
            result["witnesses"] = acquisition.witnesses
            result["queries"] = distributions(acquisition.queries)
            result["samples"] = len(acquisition.samples)
            correct_times = [row["time_to_correct_s"] for row in acquisition.witnesses if row.get("correct")]
            result["state_time_to_correct_s"] = {"correct_states": len(correct_times),
                                                  "failed_states": sum(not row.get("correct") for row in acquisition.witnesses),
                                                  "p50": percentile(correct_times, 50),
                                                  "p95": percentile(correct_times, 95),
                                                  "max": max(correct_times, default=None)}
        if broker:
            broker.close()
        for filename in ("daemon.log", "broker.log"):
            if (subject / filename).exists():
                shutil.copyfile(subject / filename, out / filename)
        dump(out / "result.json", result)
    print(json.dumps({"run": out.name, "status": result["status"], "failure": result.get("failure"), "states": len(result.get("witnesses", []))}), flush=True)
    return result


def validate_layout_receipt(result, sqlite_version):
    for name, tag in (("old", 1), ("rowid", 2)):
        arm = result[name]
        if arm["sqlite_version"] != sqlite_version or arm["layout_tag"] != tag:
            raise MeasurementError(f"{name}: SQLite version or layout does not match the pin")
        if len(arm["batches"]) != 11 or arm["discarded_warmup_batches"] != 1:
            raise MeasurementError(f"{name}: expected one warmup and eleven measured batches")
        for batch in arm["batches"]:
            if not all(isinstance(batch[key], int) and batch[key] >= 0
                       for key in ("wall_ns", "cpu_ns", "vm_steps", "full_scan_steps")) or batch["vm_steps"] == 0:
                raise MeasurementError(f"{name}: missing per-statement counters or clocks")
        if arm["probes_per_batch"] != 256 or arm["present_probes"] != 128 or arm["absent_probes"] != 128:
            raise MeasurementError(f"{name}: incorrect probe cohort")
    if result["byte_identical_keys"] != 32 or result["speedup"] < 10:
        raise MeasurementError("rowid parity or 10x lookup gate failed")


def rowid_layout_gate(args):
    """Acquire the storage-only gate without starting a daemon or changing real storage."""
    if args.corpus != "aft":
        raise MeasurementError("rowid layout currently pins the AFT Cargo.lock corpus")
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = json.loads(args.manifest.read_text())
    corpus = manifest["corpora"][args.corpus]
    # These are Git object reads, not reads of a possibly modified checkout.
    payload = subprocess.check_output(["git", "-C", str(args.source), "show", f"{corpus['commit']}:Cargo.lock"])
    Path("target").mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="v3-layout-", dir="target") as scratch:
        payload_path = Path(scratch).resolve() / "corpus-payload"
        payload_path.write_bytes(payload)
        receipt = args.output.resolve() / "lookup.json"
        env = dict(os.environ, AFT_V3_GATE_PAYLOAD=str(payload_path), AFT_V3_GATE_RECEIPT=str(receipt))
        command = ["cargo", "test", "-p", "agent-file-tools", "--lib", "--", "--ignored", "--exact",
                   "blob_store::v3::layout_gate::rowid_layout_receipt", "--nocapture"]
        subprocess.run(command, env=env, check=True, timeout=600)
        result = json.loads(receipt.read_text())
        baseline = json.loads(Path(__file__).with_name("views-performance-baselines").joinpath("slice-3b-linux-x86_64.json").read_text())
        validate_layout_receipt(result, baseline["sqlite_version"])
        result["corpus"] = dict(corpus, path="Cargo.lock", payload_sha256=hashlib.sha256(payload).hexdigest(),
                                payload_bytes=len(payload), row_bytes=1024 * 1024, rows=32)
        result["composition"] = ("ffbbea3e0 digest SELECT and 256 probes, composed with its shared-31-byte-prefix "
                                 "key fixture: final byte 0..63 repeated four times, 128 hits and 128 misses; "
                                 "each payload repeats the pinned slice-1 Cargo.lock to 1 MiB. Identical rows and insert order in both arms.")
        dump(receipt, result)
        print(json.dumps(result, sort_keys=True), flush=True)
        subprocess.run(["cargo", "test", "-p", "agent-file-tools", "--lib", "--", "--ignored", "--exact",
                        "blob_store::v3::open_tests::old_store_dispatch_and_copy_readers_never_migrate_on_open", "--nocapture"],
                       env=env, check=True, timeout=600)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rowid-layout", action="store_true", help="run the ignored v3 storage-layout gate, not daemon arms")
    parser.add_argument("--source", type=Path, default=Path.cwd(), help="local Git repository containing the pinned corpus commit; cloned, never mutated")
    parser.add_argument("--corpus", choices=["aft", "opencode", "kubernetes"], default="aft")
    parser.add_argument("--manifest", type=Path, default=Path(__file__).with_name("views-performance-corpus.json"))
    parser.add_argument("--binary", type=Path, default=Path("target/release/aft"))
    parser.add_argument("--broker", type=Path, default=Path("target/release/examples/views_perf_daemon"))
    parser.add_argument("--output", required=True, type=Path, help="new evidence directory, outside target/dist/cache")
    parser.add_argument("--subjects", type=Path, help="optional NEW scratch directory retaining clones/stores; default uses job temp space")
    parser.add_argument("--roots", nargs="+", type=int, choices=[1, 5, 15], default=[1, 5, 15])
    parser.add_argument("--warmup-minutes", type=float, default=5)
    parser.add_argument("--activity-minutes", type=float, default=30)
    parser.add_argument("--quiet-minutes", type=float, default=20)
    parser.add_argument("--cycle-seconds", type=float, default=20)
    parser.add_argument("--state-timeout", type=float, default=120, help="edit-to-correct deadline, including query wait time")
    parser.add_argument("--ready-timeout", type=float, default=600, help="initial indexed-witness deadline from bind or worktree creation")
    parser.add_argument("--request-timeout", type=float, default=120, help="hard bound on each public response; timeout poisons the connection")
    parser.add_argument("--graph-inspect", action="store_true", help="also sample one whole-root graph-backed inspect on the primary root")
    parser.add_argument("--semantic", action="store_true", help="enable real local fastembed, never a fixture/fake backend")
    parser.add_argument("--model", default="all-MiniLM-L6-v2", help="fixed AFT fastembed model name")
    args = parser.parse_args()
    if args.rowid_layout:
        return rowid_layout_gate(args)

    def interrupted(signum, _frame):
        raise MeasurementError(f"acquisition interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    if not sys.platform.startswith("linux") or not Path("/proc/self/stat").exists():
        parser.error("Linux /proc accounting is required; other platform metrics are unknown")
    if any(getattr(args, name) < 0 for name in ("warmup_minutes", "activity_minutes", "quiet_minutes", "cycle_seconds")) or min(args.state_timeout, args.ready_timeout, args.request_timeout) <= 0:
        parser.error("durations must be nonnegative; state timeout must be positive")
    args.source = args.source.resolve()
    args.binary = args.binary.resolve(strict=True)
    args.broker = args.broker.resolve(strict=True)
    args.output = args.output.resolve()
    if any(part in ("target", "dist", "node_modules", ".cache") for part in args.output.parts):
        parser.error("evidence directory must not be regenerable build output")
    args.output.mkdir(parents=True, exist_ok=False)
    if args.subjects:
        args.subjects = args.subjects.resolve()
        args.subjects.mkdir(parents=True, exist_ok=False)
    else:
        args.subjects = Path(tempfile.mkdtemp(prefix="views-performance-subjects-"))
    manifest = json.loads(args.manifest.read_text())
    shutil.copyfile(Path(__file__), args.output / "acquisition-harness.py")
    shutil.copyfile(args.manifest, args.output / "corpus-manifest.json")
    corpus = manifest["corpora"][args.corpus]
    environment = {"schema": 1, "manifest": manifest, "manifest_sha256": sha(args.manifest),
                   "python": sys.version, "git": checked(["git", "--version"]),
                   "binary_version": checked([args.binary, "--version"]),
                   "binary_sha256": sha(args.binary), "broker_sha256": sha(args.broker),
                   "source_revision": git(Path(__file__).resolve().parent.parent, "rev-parse", "HEAD"),
                   "source_diff": git(Path(__file__).resolve().parent.parent, "diff", "HEAD", "--stat"),
                   "implementation_sha256": {str(path): sha(Path(__file__).resolve().parent.parent / path)
                                             for path in git(Path(__file__).resolve().parent.parent, "diff", "HEAD", "--name-only").splitlines()
                                             if (Path(__file__).resolve().parent.parent / path).is_file()},
                   "platform": platform.platform(), "host": platform.node(), "cpu_count": os.cpu_count(),
                   "cpu_affinity": sorted(getattr(os, "sched_getaffinity")(0)),
                   "filesystem": checked(["stat", "-f", "-c", "%T", args.output]),
                   "cache_warmness": "fresh isolated stores; OS page cache uncontrolled; alternating sequential arms",
                   "clock": "time.monotonic; nearest-rank percentiles; failed states retained, never dropped",
                   "argv": sys.argv}
    dump(args.output / "environment.json", environment)
    results = []
    for count in args.roots:
        results.append(run_arm(args, corpus, count, args.output / f"n{count}"))
    dump(args.output / "matrix.json", [{"roots": row["roots"], "status": row["status"], "failure": row.get("failure")} for row in results])
    return 0 if results and all(row["status"] == "measured" for row in results) else 1


if __name__ == "__main__":
    sys.exit(main())
