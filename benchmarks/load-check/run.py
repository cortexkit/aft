#!/usr/bin/env python3
"""Real-process, open-loop read-only load check. Python standard library only."""

import argparse
import asyncio
import gzip
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time
import unittest


TOOLS = ("grep", "glob", "search", "read", "outline", "zoom", "callgraph")


def percentile(values, percent):
    """Nearest-rank percentiles keep a single long stall visible in small groups."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * percent / 100) - 1)] if ordered else None


def distribution(values):
    return {key: percentile(values, p) for key, p in
            (("p50_ms", 50), ("p95_ms", 95), ("p99_ms", 99), ("max_ms", 100))}


def summarize(calls, samples, duration, args):
    steady = [call for call in calls if call["phase"] == "steady"]
    tools = {}
    reasons = []
    for tool in TOOLS:
        rows = [call for call in steady if call["tool"] == tool]
        tools[tool] = {"count": len(rows), **distribution([c["latency_ms"] for c in rows]),
                       "errors": sum(bool(c.get("error")) for c in rows),
                       "refusals": sum(c.get("refusal", False) for c in rows),
                       "client_queue": distribution([c.get("client_queue_ms", c["latency_ms"])
                                                     for c in rows])}
        if len(rows) < 20:
            reasons.append(f"{tool}: fewer than 20 steady calls")
        elif tools[tool]["p95_ms"] >= args.p95_ms or tools[tool]["max_ms"] >= args.max_ms:
            reasons.append(f"{tool}: latency budget exceeded")
    errors = [c for c in steady if c.get("error")]
    if errors:
        reasons.append(f"{len(errors)} steady errors/refusals/timeouts")
    cold = [call for call in calls if call["phase"] == "cold"]
    cpu = [s for s in samples if s["phase"] == "steady"]
    seconds = sum(s["interval_s"] for s in cpu)
    cpu_seconds = sum(s["cpu_s"] for s in cpu)
    busy = sum(s["interval_s"] for s in cpu if s["cores"] >= 0.9)
    mean_cores = cpu_seconds / seconds if seconds else None
    busy_fraction = busy / seconds if seconds else None
    if not cpu or mean_cores >= args.cpu_cores or busy_fraction >= args.busy_fraction:
        reasons.append("daemon CPU budget exceeded or telemetry missing")
    achieved = sum(1 for c in steady if c.get("dispatched_s", float("inf")) < duration) / duration * 60
    if achieved < args.rate * 0.98:
        reasons.append("arrival generator did not maintain requested rate")
    return {
        "passed": not reasons, "failures": reasons, "tools": tools,
        "cold": {"count": len(cold), **distribution([c["latency_ms"] for c in cold])},
        "errors": errors, "steady_calls_per_minute": achieved,
        "steady_cpu_seconds": cpu_seconds, "steady_mean_cpu_cores": mean_cores,
        "steady_busy_fraction": busy_fraction,
        "peak_rss_bytes": max((s["peak_rss_bytes"] for s in samples), default=0),
    }


def process_sample(pid):
    # Linux's process clock is independent of the load generator and includes all
    # daemon threads. One core is 100%, not 1/16 of the allocated build machine.
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    ticks = int(stat[11]) + int(stat[12])
    status = Path(f"/proc/{pid}/status").read_text()
    peak = re.search(r"^VmHWM:\s+(\d+) kB", status, re.M)
    rss = re.search(r"^VmRSS:\s+(\d+) kB", status, re.M)
    return (ticks / os.sysconf("SC_CLK_TCK"), int(peak[1]) * 1024 if peak else 0,
            int(rss[1]) * 1024 if rss else 0)


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def disk_bytes(path):
    result = subprocess.check_output(["du", "-sk", str(path)], text=True)
    return int(result.split()[0]) * 1024


def repository_info(root):
    return {"path": str(root), "head": git(root, "rev-parse", "HEAD"),
            "disk_bytes": disk_bytes(root),
            "ignored_trees": {str(p.relative_to(root)): disk_bytes(p) for p in
                              (root / "target", root / "node_modules",
                               root / "benchmarks/aft-search/.bench") if p.exists()}}


def parse_timing(line):
    """Keep original log lines as well as extracted fields for future formats."""
    fields = {}
    for name, value, unit in re.findall(
            r"\b(exec|service|queue|queue_wait|queue-wait|queued|wait)(?:_ms)?=([\d.]+)(µs|us|ms|s)?", line):
        fields[name + "_ms"] = float(value) * {"µs": .001, "us": .001,
                                                        "ms": 1, "s": 1000, "": 1}[unit]
    return fields


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--transport", choices=("standalone", "subc"), default="standalone")
    parser.add_argument("--broker", type=Path, help="views_perf_daemon example binary for subc")
    parser.add_argument("--baseline", type=Path, help="Run paired lifecycle matrix against this binary")
    parser.add_argument("--views-startup-memory-mib", type=float, default=1024,
                        help="Reported startup-memory reference only; never gates")
    parser.add_argument("--repo", type=Path, help="Real checkout, not a small fixture")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--views", choices=("on", "off"), default="on")
    parser.add_argument("--label", default="current")
    parser.add_argument("--phase", choices=("cold-alone", "cold-traffic", "in-process-warm", "warm-restart"), default="in-process-warm")
    parser.add_argument("--state", type=Path, help="Reuse warmed state for steady runs only")
    parser.add_argument("--ready-timeout", type=float, default=1200)
    parser.add_argument("--rate", type=float, default=300, help="Total arrivals/minute")
    parser.add_argument("--sessions", type=int, default=8)
    parser.add_argument("--inflight", type=int, default=32)
    parser.add_argument("--duration", type=float, default=240)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--p95-ms", type=float, default=1000)
    parser.add_argument("--max-ms", type=float, default=5000)
    parser.add_argument("--cpu-cores", type=float, default=.8)
    parser.add_argument("--busy-fraction", type=float, default=.5)
    parser.add_argument("--profile-on-failure", action="store_true")
    args = parser.parse_args()
    if not args.self_test:
        if not args.binary or not args.repo or not args.out:
            parser.error("--binary, --repo and --out are required")
        if platform.system() != "Linux":
            parser.error("Linux /proc process accounting is required")
        if (not 150 <= args.rate <= 600 or args.sessions < 2 or args.inflight < args.sessions
                or args.duration <= 0 or args.timeout <= 0 or args.ready_timeout <= 0):
            parser.error("use 150..600 calls/minute, >=2 sessions, inflight >= sessions, "
                         "and positive duration/timeouts")
        args.binary = args.binary.resolve(strict=True)
        if args.baseline:
            args.baseline = args.baseline.resolve(strict=True)
        if args.broker:
            args.broker = args.broker.resolve(strict=True)
        if (args.transport == "subc" or args.baseline) and not args.broker:
            parser.error("--broker is required for subc or the paired matrix")
        args.repo = args.repo.resolve(strict=True)
        args.out = args.out.resolve()
    return args


class NDJSONClient:
    def __init__(self, process, output):
        self.process = process
        self.subject_pid = getattr(process, "pid", None)
        self.output = output
        self.pending = {}
        self.sequence = 0
        self.timings = []
        self.protocol_errors = []
        self.started = time.monotonic()
        self.events = []

    def encode(self, payload):
        return payload

    def decode(self, message):
        return message

    async def stdout(self):
        while line := await self.process.stdout.readline():
            try:
                message = self.decode(json.loads(line))
                key = str(message.get("id"))
                if key in self.pending:
                    future = self.pending.pop(key)
                    if not future.done():
                        future.set_result(message)
            except (ValueError, AttributeError) as error:
                self.protocol_errors.append(f"{error}: {line[:300]!r}")
        for future in self.pending.values():
            if not future.done():
                future.set_exception(RuntimeError("daemon stdout closed"))
        self.pending.clear()

    async def stderr(self):
        with (self.output / ("broker.log" if isinstance(self, SubcClient) else "daemon.log")).open("w") as log:
            while line := await self.process.stderr.readline():
                text = line.decode(errors="replace")
                log.write(text)
                fields = parse_timing(text)
                if fields:
                    self.timings.append({"line": text.rstrip(), **fields})
                if "index_event " in text:
                    self.events.append({"received_s": time.monotonic() - self.started,
                                        "fields": dict(re.findall(r"(\w+)=([^\s]+)", text)),
                                        "line": text.rstrip()})

    async def request(self, payload, timeout):
        self.sequence += 1
        key = str(self.sequence)
        future = asyncio.get_running_loop().create_future()
        self.pending[key] = future
        payload = {"id": key, **payload}
        async def exchange():
            self.process.stdin.write(json.dumps(self.encode(payload)).encode() + b"\n")
            await self.process.stdin.drain()
            return await future
        try:
            response = await asyncio.wait_for(exchange(), timeout)
            return key, response
        finally:
            self.pending.pop(key, None)

    async def close(self):
        if self.process.returncode is None:
            self.process.stdin.close()
            try:
                await asyncio.wait_for(self.process.wait(), 10)
            except asyncio.TimeoutError:
                os.killpg(self.process.pid, signal.SIGKILL)
                await self.process.wait()


class SubcClient(NDJSONClient):
    def encode(self, payload):
        session = payload.get("session_id", "load-0")
        channel = int(session.split("-")[-1]) + 1
        if payload["command"] == "configure":
            return {"id": payload["id"], "op": "bind", "channel": channel,
                    "root": payload["project_root"]}
        return {"id": payload["id"], "op": "tool", "channel": channel,
                "name": "aft_search" if payload["name"] == "search" else payload["name"],
                "arguments": payload["arguments"]}

    def decode(self, message):
        body = message["body"]
        if message.get("transport_error"):
            return {"id": message["id"], "success": False, "error": body}
        if body.get("type") == "route_bind_ack":
            return {"id": message["id"], "success": True, "bind": body}
        text = "\n".join(c.get("text", "") for c in body.get("content", []))
        return {"id": message["id"], "success": not body.get("isError", False),
                "text": text, "body": body}


def source_function(file, text):
    if (Path(file).suffix not in (".rs", ".ts", ".go") or
            re.search(r"(^|/)(tests?|__tests__|fixtures?|vendor|testdata)(/|$)", file) or
            re.search(r"(^test_|\.(test|spec)\.|_test\.go$)", Path(file).name)):
        return None
    patterns = {
        ".rs": r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)\s*\(",
        ".ts": r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_]\w*)\s*\(",
        ".go": r"^\s*func\s+(?:\([^\n)]*\)\s*)?([A-Za-z_]\w*)\s*\(",
    }
    match = re.search(patterns[Path(file).suffix], text, re.M)
    return match[1] if match else None


def workload(root):
    candidates = []
    for file in git(root, "ls-files").splitlines():
        path = root / file
        if path.suffix not in (".rs", ".ts", ".go") or not path.is_file():
            continue
        size = path.stat().st_size
        if 3000 <= size <= 100000:
            text = path.read_text(errors="replace")
            symbol = source_function(file, text)
            if symbol:
                candidates.append((file, symbol))
    if len(candidates) < 3:
        raise ValueError("Need at least three real source files with named functions")
    # Spread requests over source files rather than hammering a cached README.
    targets = [candidates[i * (len(candidates) - 1) // 2] for i in range(3)]
    variants = []
    for file, symbol in targets:
        scope = str(Path(file).parent)
        extension = Path(file).suffix
        variants.append({
            "grep": {"pattern": symbol, "path": scope, "include": f"*{extension}"},
            "glob": {"pattern": f"**/*{extension}", "path": scope},
            "search": {"query": symbol, "topK": 10},
            "read": {"path": file, "startLine": 1, "limit": 80},
            "outline": {"target": file},
            "zoom": {"batch": [{"target": file, "symbols": symbol}]},
            "callgraph": {"op": "callers", "path": file, "symbol": symbol},
        })
    return variants


def response_error(response):
    if response.get("success") is not True or response.get("isError"):
        return json.dumps(response, ensure_ascii=False)[:2000]
    return None


def indexed_search_ready(response):
    """The query's own lane disclosure is authoritative, not legacy status."""
    if response_error(response):
        return False
    text = response.get("text", "")
    if response.get("fallback") or re.search(r"filesystem fallback|\[index: fallback\]", text, re.I):
        return False
    lane = (response.get("lanes") or {}).get("trigram", {})
    return lane.get("status") == "ready" or bool(re.search(r"^trigram: ready\s*$", text, re.M))


async def run_traffic(client, args, variants, start, calls, duration=None, phase="steady", stop=None):
    semaphore = asyncio.Semaphore(args.inflight)

    async def call(number, due):
        tool = TOOLS[number % len(TOOLS)]
        session = f"load-{number % args.sessions}"
        row = {"tool": tool, "session": session, "scheduled_s": due - start,
               "phase": phase, "id": None}
        row["arguments"] = variants[(number // len(TOOLS)) % len(variants)][tool]

        async def dispatch():
            async with semaphore:
                row["client_queue_ms"] = (time.monotonic() - due) * 1000
                remaining = args.timeout - (time.monotonic() - due)
                if remaining <= 0:
                    raise TimeoutError("expired in client backlog before dispatch")
                row["dispatched_s"] = time.monotonic() - start
                row["id"], response = await client.request({
                    "command": "tool_call", "session_id": session,
                    "name": tool, "arguments": row["arguments"]}, remaining)
                row["response_bytes"] = len(json.dumps(response))
                row["response_excerpt"] = str(response.get("text", ""))[:300]
                error = response_error(response)
                if error:
                    row["error"] = error
                    row["refusal"] = bool(re.search(r"refus|denied|permission|not.allowed|scope", error, re.I))
        try:
            await asyncio.wait_for(dispatch(), max(.001, args.timeout - (time.monotonic() - due)))
        except (TimeoutError, asyncio.TimeoutError, RuntimeError, BrokenPipeError, ConnectionError) as error:
            row["error"] = f"{type(error).__name__}: {error}"
        row["latency_ms"] = (time.monotonic() - due) * 1000
        calls.append(row)

    tasks = []
    span = duration if duration is not None else args.duration
    count = math.ceil(span * args.rate / 60)
    for number in range(count):
        due = start + number * 60 / args.rate
        await asyncio.sleep(max(0, due - time.monotonic()))
        if stop is not None and stop.is_set():
            break
        tasks.append(asyncio.create_task(call(number, due)))
    await asyncio.gather(*tasks)


async def telemetry(process, stage, start, samples):
    before = process_sample(stage["pid"])
    previous = time.monotonic()
    previous_stage = stage["name"]
    while process.returncode is None:
        await asyncio.sleep(1)
        try:
            cpu, peak, rss = process_sample(stage["pid"])
        except FileNotFoundError:
            break
        now = time.monotonic()
        interval = now - previous
        sample_stage = previous_stage
        if previous_stage != stage["name"] or (sample_stage == "steady" and now > stage.get("steady_until", float("inf"))):
            sample_stage = "drain_or_profile"
        samples.append({"elapsed_s": now - start, "phase": sample_stage,
                        "interval_s": interval, "cpu_s": cpu - before[0],
                        "cores": (cpu - before[0]) / interval, "peak_rss_bytes": peak,
                        "rss_bytes": rss})
        before, previous = (cpu, peak, rss), now
        previous_stage = stage["name"]


async def profile(client, args, variants):
    # The product profiler tries perf and falls back to /proc when perf is denied.
    # Keep the daemon under the same arrival load while collecting the profile.
    output = args.out / "profile.json"
    with output.open("wb") as stream, (args.out / "profile.stderr").open("wb") as stderr:
        scratch = args.out / "profile-tmp"
        scratch.mkdir()
        sampler = await asyncio.create_subprocess_exec(
             str(args.binary), "profile", "--pid", str(client.subject_pid),
            "--seconds", "30", "--json", stdout=stream, stderr=stderr, cwd=args.out,
            env={**os.environ, "TMPDIR": str(scratch)})
        await run_traffic(client, args, variants, time.monotonic(), [], duration=30)
        try:
            code = await asyncio.wait_for(sampler.wait(), 60)
        except asyncio.TimeoutError:
            sampler.kill()
            await sampler.wait()
            code = -1
    return {"exit_code": code, "path": str(output), "bytes": output.stat().st_size}


async def wait_ready(client, args, variants, result):
    """Readiness probes use the same query routes as traffic, in every lifecycle."""
    started = client.started
    planes = {"search": None, "callgraph": None, "semantic": "disabled"}
    evidence = {}
    snapshots, errors = [], []
    callers_probes = 0
    deadline = time.monotonic() + args.ready_timeout

    async def monitored(payload):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise asyncio.TimeoutError("readiness deadline reached")
        return await client.request(payload, min(120, remaining))

    while time.monotonic() < deadline:
        try:
            _, search = await monitored({"command": "tool_call", "session_id": "load-0",
                "name": "search", "arguments": variants[1]["search"]})
            elapsed = time.monotonic() - started
            snapshots.append({"elapsed_s": elapsed, "search": search})
            if indexed_search_ready(search) and planes["search"] is None:
                planes["search"], evidence["search"] = elapsed, "query trigram: ready (indexed route)"
            if planes["callgraph"] is None:
                callers_probes += 1
                _, probe = await monitored({"command": "tool_call", "session_id": "load-0",
                    "name": "callgraph", "arguments": variants[1]["callgraph"]})
                snapshots[-1]["callgraph_probe"] = probe
                if response_error(probe) is None:
                    planes["callgraph"] = time.monotonic() - started
                    evidence["callgraph"] = "callers readiness probe"
        except (TimeoutError, asyncio.TimeoutError, RuntimeError, BrokenPipeError, ConnectionError) as error:
            errors.append({"elapsed_s": time.monotonic() - started, "error": str(error)})
        if planes["search"] is not None and planes["callgraph"] is not None:
            break
        await asyncio.sleep(min(5, max(0, deadline - time.monotonic())))
    ready = planes["search"] is not None and planes["callgraph"] is not None
    result["readiness"] = {"all_ready": ready, "ready_s": planes, "evidence": evidence,
                           "waited_s": time.monotonic() - started,
                           "status_polls": len(snapshots), "callers_probes": callers_probes,
                           "samples": snapshots, "errors": errors}
    return ready


def markdown(result):
    gate = result["summary"]
    verdict = "MEASUREMENT ONLY" if gate["passed"] is None else "PASS" if gate["passed"] else "FAIL"
    lines = [f"# {result['label']} — {result['phase']} — views {result['views']}: {verdict}", "",
             f"Repo: `{result['repository']['head']}` at `{result['repository']['path']}`",
             f"Binary SHA256: `{result['binary_sha256']}`", "",
              "| Tool | Calls | p50 ms | p95 ms | p99 ms | Max ms | Errors | Refusals | Backlog p95 ms |",
              "|---|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for tool, stats in result["measurement_tools"].items():
        values = [str(stats["count"])] + [f"{stats[k]:.2f}" if stats[k] is not None else "n/a"
                    for k in ("p50_ms", "p95_ms", "p99_ms", "max_ms")]
        values += [str(stats["errors"]), str(stats["refusals"]),
                   str(stats["client_queue"]["p95_ms"])]
        lines.append(f"| {tool} | " + " | ".join(values) + " |")
    readiness = result.get("readiness", {})
    lines += ["", f"Index ready seconds since configure queued: {json.dumps(readiness.get('ready_s', {}))}",
              f"All indexes/views ready: {readiness.get('all_ready', False)}",
              f"Steady mean CPU cores: {gate.get('steady_mean_cpu_cores', 'n/a')}",
              f"Steady busy fraction: {gate.get('steady_busy_fraction', 'n/a')}",
               f"Peak RSS: {result['peak_rss_bytes'] / 2**20:.1f} MiB",
               f"Phase RSS (sampled current RSS, not cumulative HWM): {json.dumps(result['phase_peak_rss_bytes'])}",
               f"Views startup-memory budget (report only): {json.dumps(result['views_startup_memory_budget'])}",
               f"Achieved arrivals/minute: {gate.get('steady_calls_per_minute', 'n/a')}",
              f"Errors/refusals: {len(gate['errors'])}",
              f"Allocated repo disk: {result['repository']['disk_bytes'] / 2**30:.2f} GiB",
              f"Daemon timing records: {len(result['daemon_timings'])}",
              "Daemon queue/exec fields absent in standalone mode are unavailable.", ""]
    lines += [f"- {reason}" for reason in gate["failures"]]
    if result.get("profile"):
        lines += ["", "Failure profile: " + json.dumps(result["profile"])]
    return "\n".join(lines) + "\n"


async def main(args):
    args.out.mkdir(parents=True, exist_ok=False)
    variants = workload(args.repo)
    config = {"indexes": {"trigram": True, "semantic": False, "callgraph": True},
              "views": {"enabled": args.views == "on"}, "inspect": {"enabled": False},
              "lsp": {"disabled": ["rust"]}}
    state = args.state.resolve() if args.state else args.out / "state"
    if args.phase != "warm-restart" and state.exists():
        raise ValueError("Cold measurements require a new state directory")
    if args.phase == "warm-restart" and not state.exists():
        raise ValueError("Warm restart requires a previously warmed --state")
    state.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "RUST_LOG": "info", "AFT_PERF_TICK_INTERVAL_MS": "1000",
           "HOME": str(state / "home"), "AFT_STORAGE_DIR": str(state / "storage"),
           "XDG_DATA_HOME": str(state / "data"), "XDG_CACHE_HOME": str(state / "cache"),
           "XDG_CONFIG_HOME": str(state / "config")}
    with args.binary.open("rb") as binary:
        digest = hashlib.sha256()
        for block in iter(lambda: binary.read(1024 * 1024), b""):
            digest.update(block)
    for directory in ("home/.config/cortexkit", "config/cortexkit", "data", "cache", "storage"):
        (state / directory).mkdir(parents=True, exist_ok=True)
    for path in (state / "config/cortexkit/aft.jsonc", state / "home/.config/cortexkit/aft.jsonc"):
        path.write_text(json.dumps(config))
    result = {"schema_version": 3, "label": args.label, "transport": args.transport,
              "views": args.views, "phase": args.phase,
              "binary_sha256": digest.hexdigest(), "state": str(state),
              "repository": repository_info(args.repo), "config": config,
              "parameters": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              "host": {"platform": platform.platform(), "cpus": os.cpu_count(),
                        "affinity": sorted(getattr(os, "sched_getaffinity")(0)),
                        "python": sys.version, "loadavg_start": os.getloadavg()}, "workload": variants}
    start = time.monotonic()
    command = ([str(args.broker), str(args.binary), str(state), str(args.repo), "--pipelined"]
               if args.transport == "subc" else [str(args.binary)])
    process = await asyncio.create_subprocess_exec(
        *command, cwd=args.repo, env=env, stdin=asyncio.subprocess.PIPE, start_new_session=True,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, limit=16 * 1024 * 1024)
    result["pid"] = process.pid
    client = (SubcClient if args.transport == "subc" else NDJSONClient)(process, args.out)
    client.subject_pid = process.pid
    readers = [asyncio.create_task(client.stderr())]
    calls, samples = [], []
    stage = {"name": "startup", "pid": process.pid}
    monitor = None
    stop = asyncio.Event()
    traffic = None
    configure = None
    try:
        if args.transport == "subc":
            assert process.stdout is not None
            identity = await asyncio.wait_for(process.stdout.readline(), 75)
            if not identity:
                raise RuntimeError("broker exited before identity; see broker.log and daemon.log")
            result["identity"] = json.loads(identity)
            result["pid"] = stage["pid"] = result["identity"]["pid"]
            client.subject_pid = result["pid"]
        readers.append(asyncio.create_task(client.stdout()))
        monitor = asyncio.create_task(telemetry(process, stage, start, samples))
        client.started = time.monotonic()
        result["configure_queued_s"] = client.started - start
        configure = asyncio.create_task(client.request({
            "command": "configure", "session_id": "load-0", "project_root": str(args.repo),
            "harness": "opencode", "storage_dir": str(state),
            "config": [{"tier": "user", "source": "<load-check>", "doc": json.dumps(config)}]},
            args.timeout))
        await asyncio.sleep(0)
        if args.transport == "subc":
            _, result["configure_response"] = await configure
            result["binds"] = [result["configure_response"]]
            if response_error(result["configure_response"]):
                raise RuntimeError("initial route bind failed")
            for session in range(1, args.sessions):
                _, bound = await client.request({"command": "configure", "session_id": f"load-{session}",
                                                "project_root": str(args.repo)}, 120)
                result["binds"].append(bound)
                if response_error(bound):
                    raise RuntimeError(f"route bind {session + 1} failed: {bound}")
        if args.phase == "cold-traffic":
            traffic = asyncio.create_task(run_traffic(client, args, variants, time.monotonic(), calls,
                duration=args.ready_timeout, phase="cold", stop=stop))
        ready = await wait_ready(client, args, variants, result)
        stop.set()
        if traffic:
            await traffic
        _, result["configure_response"] = await configure
        if response_error(result["configure_response"]):
            raise RuntimeError("configure failed: " + str(result["configure_response"]))
        if args.phase in ("in-process-warm", "warm-restart") and ready:
            stage["name"] = "steady"
            result["steady_started_s"] = time.monotonic() - start
            stage["steady_until"] = time.monotonic() + args.duration
            await run_traffic(client, args, variants, time.monotonic(), calls)
            stage["name"] = "drain_or_profile"
            result["summary"] = summarize(calls, samples, args.duration, args)
            if client.protocol_errors:
                result["summary"]["failures"].extend(client.protocol_errors)
            result["summary"]["passed"] = not result["summary"]["failures"]
        elif args.phase in ("in-process-warm", "warm-restart"):
            result["summary"] = summarize(calls, samples, args.duration, args)
            result["summary"]["failures"].append("indexes/views did not become ready; steady gate not run")
            result["summary"]["passed"] = False
        else:
            result["summary"] = {"passed": None, "failures": [], "tools": {},
                                 "errors": [c for c in calls if c.get("error")]}
        if args.profile_on_failure and (result["summary"]["passed"] is False or not ready):
            stage["name"] = "profile"
            result["profile"] = await profile(client, args, variants)
    except Exception as error:
        result["summary"] = summarize(calls, samples, args.duration, args)
        result["summary"]["passed"] = False
        result["summary"]["failures"].append(f"harness: {type(error).__name__}: {error}")
        if args.profile_on_failure and process.returncode is None:
            try:
                stage["name"] = "profile"
                result["profile"] = await profile(client, args, variants)
            except Exception as profile_error:
                result["profile"] = {"error": str(profile_error)}
    finally:
        stop.set()
        if traffic and not traffic.done():
            traffic.cancel()
            await asyncio.gather(traffic, return_exceptions=True)
        if configure and not configure.done():
            configure.cancel()
            await asyncio.gather(configure, return_exceptions=True)
        await client.close()
        if monitor:
            monitor.cancel()
            await asyncio.gather(monitor, return_exceptions=True)
        await asyncio.gather(*readers)
        if args.transport == "subc" and (state / "daemon.log").exists():
            shutil.copyfile(state / "daemon.log", args.out / "daemon.log")
    result["wall_seconds"] = time.monotonic() - start
    result["calls"] = sorted(calls, key=lambda c: c["scheduled_s"])
    result["samples"] = samples
    result["measurement_cpu"] = {}
    for name in sorted({s["phase"] for s in samples}):
        rows = [s for s in samples if s["phase"] == name]
        seconds = sum(s["interval_s"] for s in rows)
        cpu = sum(s["cpu_s"] for s in rows)
        result["measurement_cpu"][name] = {"wall_s": seconds, "cpu_s": cpu,
                                           "mean_cores": cpu / seconds if seconds else None}
    result["measurement_tools"] = {
        tool: {"count": len(rows), "errors": sum(bool(c.get("error")) for c in rows),
               "refusals": sum(c.get("refusal", False) for c in rows),
               "client_queue": distribution([c.get("client_queue_ms", c["latency_ms"]) for c in rows]),
               **distribution([c["latency_ms"] for c in rows])}
        for tool in TOOLS if (rows := [c for c in calls if c["tool"] == tool])}
    result["peak_rss_bytes"] = max((s["peak_rss_bytes"] for s in samples), default=0)
    result["phase_peak_rss_bytes"] = {name: max(s["rss_bytes"] for s in samples if s["phase"] == name)
                                      for name in sorted({s["phase"] for s in samples})}
    result["views_startup_memory_budget"] = {
        "name": "views-startup-memory", "gating": False,
        "reference_bytes": int(args.views_startup_memory_mib * 2**20),
        "observed_bytes": max((s["peak_rss_bytes"] for s in samples if s["phase"] == "startup"), default=0)
                          if args.views == "on" else None}
    result["daemon_timings"] = client.timings
    result["index_events"] = client.events
    result["protocol_errors"] = client.protocol_errors
    result["host"]["loadavg_end"] = os.getloadavg()
    for log in state.glob("**/aft-*.log"):
        for line in log.read_text(errors="replace").splitlines():
            fields = parse_timing(line)
            if fields:
                result["daemon_timings"].append({"line": line, **fields})
    (args.out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    report = markdown(result)
    (args.out / "summary.md").write_text(report)
    for filename in ("result.json", "daemon.log", "broker.log"):
        if not (args.out / filename).exists():
            continue
        with (args.out / filename).open("rb") as source, gzip.open(args.out / (filename + ".gz"), "wb") as dest:
            shutil.copyfileobj(source, dest)
    print(report, flush=True)
    return 1 if result["summary"]["passed"] is False else 0


async def paired_matrix(args):
    """One subject and allocation; fresh, independent state for each binary arm."""
    args.out.mkdir(parents=True, exist_ok=False)
    original = repository_info(args.repo)
    inputs = workload(args.repo)
    matrix = {"schema_version": 1, "repository": original, "workload": inputs,
              "affinity": sorted(getattr(os, "sched_getaffinity")(0)), "cases": []}
    for transport in ("standalone", "subc"):
        for views in ("off", "on"):
            for label, binary in (("baseline", args.baseline), ("candidate", args.binary)):
                warm_state = None
                for phase in ("cold-alone", "in-process-warm", "warm-restart"):
                    case = argparse.Namespace(**vars(args))
                    case.baseline = None
                    case.binary, case.label, case.transport, case.views = binary, label, transport, views
                    case.phase = phase
                    case.out = args.out / f"{transport}-{views}-{label}-{phase}"
                    case.state = warm_state if phase == "warm-restart" else None
                    if workload(args.repo) != inputs or git(args.repo, "rev-parse", "HEAD") != original["head"]:
                        raise RuntimeError("paired subject or workload changed")
                    code = await main(case)
                    result = json.loads((case.out / "result.json").read_text())
                    if phase == "in-process-warm":
                        warm_state = Path(result["state"])
                    matrix["cases"].append({"transport": transport, "views": views, "label": label,
                        "phase": phase, "exit_code": code, "path": case.out.name,
                        "binary_sha256": result["binary_sha256"], "summary": result["summary"],
                        "readiness": result.get("readiness"), "binds": result.get("binds"),
                        "phase_peak_rss_bytes": result["phase_peak_rss_bytes"],
                        "views_startup_memory_budget": result["views_startup_memory_budget"]})
                    # Checkpoint after each phase so a failed remote job keeps its completed cases.
                    (args.out / "matrix.json").write_text(json.dumps(matrix, indent=2) + "\n")
    return int(any(c["exit_code"] for c in matrix["cases"]))


class HarnessTests(unittest.TestCase):
    def test_percentiles_keep_tail(self):
        self.assertEqual(distribution([1] * 99 + [25000]),
                         {"p50_ms": 1, "p95_ms": 1, "p99_ms": 1, "max_ms": 25000})

    def fixture(self):
        args = argparse.Namespace(p95_ms=1000, max_ms=5000,
                                  cpu_cores=.8, busy_fraction=.5, rate=300)
        calls = [{"tool": t, "phase": "steady", "latency_ms": 10, "dispatched_s": 0}
                 for t in TOOLS for _ in range(30)]
        calls.append({"tool": "read", "phase": "cold", "latency_ms": 100})
        samples = [{"phase": "steady", "interval_s": 42, "cpu_s": 4.2,
                    "cores": .1, "peak_rss_bytes": 1234}]
        return args, calls, samples

    def test_good_run_passes(self):
        args, calls, samples = self.fixture()
        self.assertTrue(summarize(calls, samples, 42, args)["passed"])

    def test_october_stalls_fail(self):
        args, calls, samples = self.fixture()
        for call in calls:
            call["latency_ms"] = 25000
        result = summarize(calls, samples, 42, args)
        self.assertFalse(result["passed"])
        self.assertIn("read: latency budget exceeded", result["failures"])

    def test_sustained_one_core_fails(self):
        args, calls, samples = self.fixture()
        samples[0].update(cpu_s=42, cores=1)
        self.assertFalse(summarize(calls, samples, 42, args)["passed"])

    def test_cold_refusal_is_measured_not_steady_failure(self):
        args, calls, samples = self.fixture()
        calls.append({"tool": "read", "phase": "cold", "latency_ms": 10,
                      "error": "permission denied"})
        self.assertTrue(summarize(calls, samples, 42, args)["passed"])

    def test_steady_refusal_fails(self):
        args, calls, samples = self.fixture()
        calls[0]["error"] = "permission denied"
        self.assertFalse(summarize(calls, samples, 42, args)["passed"])

    def test_source_selection_ignores_embedded_fixture(self):
        self.assertIsNone(source_function("test_integrity.py", 'text = "fn main() {}"'))
        self.assertIsNone(source_function("src/test_fixture.rs", "fn main() {}"))
        self.assertIsNone(source_function("src/foo.test.ts", "function run() {}"))

    def test_source_selection_uses_declaration_lines(self):
        self.assertEqual(source_function("src/lib.rs", "/// fn wrong() {}\npub(crate) fn actual() {}"), "actual")
        self.assertEqual(source_function("src/a.ts", "export function convert() {}"), "convert")
        self.assertEqual(source_function("src/a.go", "func (r *Reader) Read() {}"), "Read")

    def test_missing_tool_is_not_a_pass(self):
        args, calls, samples = self.fixture()
        calls = [c for c in calls if c["tool"] != "callgraph"]
        self.assertFalse(summarize(calls, samples, 42, args)["passed"])

    def test_queue_and_execution_units(self):
        self.assertEqual(parse_timing("queue_wait=1.5s exec=250µs"),
                          {"queue_wait_ms": 1500, "exec_ms": .25})

    def test_undispatched_backlog_does_not_count_as_arrival(self):
        args, calls, samples = self.fixture()
        for call in calls:
            call.pop("dispatched_s", None)
        self.assertEqual(summarize(calls, samples, 42, args)["steady_calls_per_minute"], 0)

    def test_ready_footer_rejects_filesystem_fallback(self):
        self.assertFalse(indexed_search_ready({"success": True, "text": "trigram: ready\n[index: fallback]"}))
        self.assertFalse(indexed_search_ready({"success": True, "fallback": "filesystem",
                                               "lanes": {"trigram": {"status": "ready"}}}))

    def test_subc_correlation_and_error_preservation(self):
        client = SubcClient(None, Path("."))
        self.assertEqual(client.encode({"id": "23", "command": "tool_call", "session_id": "load-7",
                                       "name": "search", "arguments": {}}),
                         {"id": "23", "op": "tool", "channel": 8, "name": "aft_search", "arguments": {}})
        reply = client.decode({"id": "23", "transport_error": True, "body": {"code": "busy"}})
        self.assertIsNotNone(response_error(reply))


class DeadlineTests(unittest.IsolatedAsyncioTestCase):
    async def test_views_ready_query_ignores_unpinned_legacy_status(self):
        class Client:
            started = time.monotonic()
            events = []

            async def request(self, payload, _timeout):
                self_test.assertEqual(payload["command"], "tool_call")
                return "1", ({"success": True, "text": "Found 10 results.\ntrigram: ready\nsemantic: off"}
                             if payload["name"] == "search" else {"success": True})

        self_test = self
        result = {}
        args = argparse.Namespace(ready_timeout=.01, timeout=.01, views="on", phase="in-process-warm")
        self.assertTrue(await wait_ready(Client(), args, [{}, {"search": {}, "callgraph": {}}], result))
        self.assertEqual(result["readiness"]["ready_s"]["semantic"], "disabled")

    async def test_building_callgraph_is_not_ready(self):
        class Client:
            started = time.monotonic()
            events = []

            async def request(self, payload, _timeout):
                return "1", ({"success": True, "text": "trigram: ready"}
                             if payload["name"] == "search" else
                             {"success": False, "code": "callgraph_building"})

        result = {}
        args = argparse.Namespace(ready_timeout=.01, timeout=.01, views="off", phase="steady")
        self.assertFalse(await wait_ready(Client(), args, [{}, {"search": {}, "callgraph": {}}], result))
        self.assertIsNone(result["readiness"]["ready_s"]["callgraph"])

    async def test_legacy_ready_and_publication_logs_cannot_bless_fallback(self):
        class Client:
            started = time.monotonic()
            events = [{"received_s": .001, "fields": {"kind": "build_ready", "plane": "callgraph"}}]

            async def request(self, payload, _timeout):
                return "1", {"success": True, "search_index": {"status": "ready"},
                             "text": "[index: fallback]\ntrigram: building"}

        result = {}
        args = argparse.Namespace(ready_timeout=.01, timeout=.01, views="off", phase="cold-alone")
        self.assertFalse(await wait_ready(Client(), args, [{}, {"search": {}, "callgraph": {}}], result))
        self.assertIsNone(result["readiness"]["ready_s"]["search"])

    async def test_stdin_backpressure_obeys_request_deadline(self):
        entered = asyncio.Event()

        class Writer:
            def write(self, _data):
                pass

            async def drain(self):
                entered.set()
                await asyncio.Event().wait()

        process = argparse.Namespace(stdin=Writer())
        client = NDJSONClient(process, Path("."))
        request = asyncio.create_task(client.request({"command": "status"}, .01))
        await entered.wait()
        await asyncio.sleep(.05)
        try:
            self.assertTrue(request.done(), "stdin drain escaped the request timeout")
            with self.assertRaises(asyncio.TimeoutError):
                await request
        finally:
            request.cancel()
            await asyncio.gather(request, return_exceptions=True)


if __name__ == "__main__":
    options = arguments()
    if options.self_test:
        print(sys.version, flush=True)
        unittest.main(argv=[sys.argv[0]], verbosity=2)
    else:
        sys.exit(asyncio.run(paired_matrix(options) if options.baseline else main(options)))
