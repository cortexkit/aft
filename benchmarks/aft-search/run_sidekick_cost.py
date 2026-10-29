"""Concurrent public-search cost probe on the pinned real-query evidence tree.

Build aft with test-timing-hooks for phase records. Embeddings are recorded real
MiniLM vectors, not inference timings. The optional identity-score HTTP reranker
exercises candidate-text preparation, not cross-encoder inference. Neither
fixture assesses search quality; run run_search_quality.py --mode evaluate
separately to compare the real-query rows with their checked-in reference.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import threading
import time

from run_concept_recall import vector_server
from run_real_query import FIXTURE_PROVIDER_MODEL, load_inputs, runtime_evidence_tree

HERE = Path(__file__).resolve().parent


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def distribution(values):
    values = sorted(values)
    if not values:
        raise RuntimeError("empty measurement population")
    return {"n": len(values), "p50": values[(len(values) - 1) // 2],
            "p95": values[min(len(values) - 1, int(len(values) * .95))]}


@contextmanager
def rerank_server():
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            reply = json.dumps({"results": [{"index": i, "relevance_score": .5}
                                             for i in range(len(body["documents"]))]}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(reply)))
            self.end_headers()
            self.wfile.write(reply)

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


class Broker:
    def __init__(self, binary, broker, runtime, root):
        self.log = (runtime / "broker.log").open("w")
        self.proc = subprocess.Popen([str(broker), str(binary), str(runtime), str(root)],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.log, env={**os.environ, "AFT_SEARCH_COST_TRACE": "1"})
        self.buffer = b""
        try:
            self.pid = self.read()["pid"]
        except BaseException:
            self.proc.kill()
            self.proc.wait()
            self.log.close()
            print((runtime / "broker.log").read_text()[-4000:])
            raise

    def read(self):
        deadline = time.monotonic() + 1800
        while time.monotonic() < deadline:
            if b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                return json.loads(line)
            if select.select([self.proc.stdout], [], [], max(0, deadline - time.monotonic()))[0]:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("broker exited; inspect broker.log and daemon.log")
                self.buffer += chunk
        raise TimeoutError("public search probe deadline")

    def send(self, op, **args):
        self.proc.stdin.write((json.dumps({"op": op, **args}) + "\n").encode())
        self.proc.stdin.flush()

    def call(self, op, **args):
        self.send(op, **args)
        return self.read()

    def close(self):
        if self.proc.poll() is None:
            self.send("quit")
            try:
                self.proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def arguments(row):
    result = {"query": row["query"], "includeTests": row["include_tests"], "topK": 10}
    # Parallel readers often send prose together with a code pattern. Preserve
    # recorded patterns, and use a code word for prose-only rows so both inputs
    # reach the same query-plus-pattern search path.
    if "pattern" in row and row["pattern"].strip():
        result["pattern"] = row["pattern"]
    else:
        words = [word.strip("?,.()\"'") for word in row["query"].split()]
        result["pattern"] = next((word for word in reversed(words)
                                  if len(word) >= 5 and word.isidentifier()), "search")
    return result


def run(args):
    output = Path(args.out).resolve()
    output.mkdir(parents=True, exist_ok=False)
    scratch = output / "scratch"
    scratch.mkdir()
    tempfile.tempdir = str(scratch)
    manifest, tree, _, pack = load_inputs(HERE / "real-query-manifest.json")
    selected = []
    seen = set()
    for row in manifest["rows"]:
        if row.get("excluded_reason") or len(row["query"].split()) < 4:
            continue
        request = arguments(row)
        identity = json.dumps(request, sort_keys=True)
        if identity not in seen:
            selected.append(request)
            seen.add(identity)
    selected = selected[:args.queries]
    assert len(selected) >= args.concurrency, len(selected)
    records = []
    cpu_by_repeat = []
    with runtime_evidence_tree(tree) as root, \
            vector_server(pack["vectors"], str(pack["embed_template_version"]), output / "vectors.log") as vectors, \
            rerank_server() as reranker:
        config = {
            "views": {"enabled": False},
            "indexes": {"trigram": True, "semantic": True, "callgraph": False},
            "lsp": {"auto_install": False},
            "semantic": {"backend": "openai_compatible", "model": FIXTURE_PROVIDER_MODEL,
                         "base_url": f"http://127.0.0.1:{vectors.server_port}",
                         "timeout_ms": 1800000, "query_timeout_ms": 60000, "max_files": 20000},
        }
        if args.rerank:
            config["search"] = {"rerank": {"backend": "remote", "endpoint": reranker,
                                           "model": "identity-cost-probe", "timeout_ms": 60000}}
        for relative in ["config/cortexkit/aft.jsonc", "home/.config/cortexkit/aft.jsonc"]:
            path = output / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(config))
        for relative in [".cortexkit/aft.jsonc", ".cortexkit/aft.opencode.jsonc"]:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(config))
        with (root / ".aftignore").open("a") as ignore:
            ignore.write("\n/.cortexkit/aft.jsonc\n/.cortexkit/aft.opencode.jsonc\n")
        broker = Broker(Path(args.binary).resolve(), Path(args.broker).resolve(), output, root)
        try:
            for channel in range(1, args.concurrency + 1):
                broker.call("bind", channel=channel, root=str(root))
            deadline = time.monotonic() + 1800
            while True:
                ready = []
                for channel in range(1, args.concurrency + 1):
                    status = broker.call("tool", channel=channel, name="status", arguments={})
                    if status.get("isError"):
                        raise RuntimeError(status)
                    status = status.get("structuredContent", status)
                    if status.get("semantic_index", {}).get("status") == "failed":
                        raise RuntimeError(status)
                    ready.append(status.get("semantic_index", {}).get("status") == "ready"
                                 and status.get("search_index", {}).get("status") == "ready")
                if all(ready):
                    break
                if time.monotonic() > deadline:
                    raise TimeoutError(f"index readiness: {status}")
                time.sleep(.2)
            cpu_start = cpu_seconds(broker.pid)
            for repeat in range(args.repeats):
                repeat_cpu = cpu_seconds(broker.pid)
                for offset in range(0, len(selected), args.concurrency):
                    batch = selected[offset:offset + args.concurrency]
                    broker.send("batch", requests=[{"id": f"{repeat}:{offset+i}", "channel": i + 1,
                                                    "name": "search", "arguments": request}
                                                   for i, request in enumerate(batch)])
                    for _ in batch:
                        record = broker.read()
                        response = record.pop("response")
                        if response.get("isError"):
                            raise RuntimeError(response)
                        body = response.get("structuredContent", response)
                        if body.get("semantic_status") != "ready":
                            raise RuntimeError(f"semantic lane not served: {body}")
                        # Preserve rows and text for a direct before/after check;
                        # only the temporary corpus prefix differs between arms.
                        record["response"] = json.loads(json.dumps(body).replace(str(root), "<root>"))
                        record["repeat"] = repeat
                        records.append(record)
                cpu_by_repeat.append((cpu_seconds(broker.pid) - repeat_cpu) / len(selected))
            cpu = cpu_seconds(broker.pid) - cpu_start
        finally:
            broker.close()
        if vectors.refused:
            raise RuntimeError(f"unknown embedding texts: {vectors.refused}")
    traces = []
    for line in (output / "daemon.log").read_text().splitlines():
        if line.startswith("AFT_SEARCH_COST "):
            traces.append(json.loads(line.removeprefix("AFT_SEARCH_COST ")))
    assert len(traces) == len(records), (len(traces), len(records))
    phases = defaultdict(list)
    per_repeat = [defaultdict(list) for _ in range(args.repeats)]
    for index, trace in enumerate(traces):
        totals = defaultdict(float)
        for name, elapsed in trace["phases"]:
            totals[name] += elapsed
        for name in ["embedding", "ann", "lexical", "exact_and_path", "fusion", "rerank", "snippet", "matching_line"]:
            phases[name].append(totals[name])
            per_repeat[index // len(selected)][name].append(totals[name])
    result = {"concurrency": args.concurrency, "queries": selected, "rerank_fixture": args.rerank,
              "ready_index": {name: status.get(name) for name in ("semantic_index", "search_index")},
              "cpu_seconds_per_query": cpu / len(records), "requests": len(records),
              "cpu_seconds_per_query_by_repeat": cpu_by_repeat,
              "latency_ms": distribution([row["latency_ms"] for row in records]),
              "phase_ms": {name: distribution(values) for name, values in phases.items()},
              "phase_ms_by_repeat": [{name: distribution(values) for name, values in repeat.items()}
                                     for repeat in per_repeat],
              "exec_ms": distribution([row["total_ms"] for row in traces]),
              "records": sorted(records, key=lambda row: row["id"]), "traces": traces}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key not in ("records", "traces", "queries")}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--broker", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--concurrency", type=int, choices=range(8, 17), default=8)
    parser.add_argument("--queries", type=int, default=24)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--rerank", action="store_true")
    run(parser.parse_args())
