#!/usr/bin/env python3
"""RSS-against-batch harness for the semantic embed build (issue #327).

Stands a whole daemon up against a synthetic corpus and a stub embedding server
on loopback, then samples the daemon's resident set against the embed batch
number it reports in its own log. Recording against batch number rather than
wall clock is what separates an accumulator tied to work from one tied to time,
and the stub removes the embedding backend as a variable: batch count, batch
latency and corpus size are all set from the command line.

The daemon is driven over the standalone NDJSON protocol, so no plugin, editor
or real backend is needed. Three load shapes can be layered on top of the plain
build, because a quiet build exercises far less of the daemon than a real
session does:

  --calls-per-second   an agent issuing tool calls while the build runs
  --reconfigure-every  configure churn, which is what a reconnecting plugin does
  --delay-ms           per-batch backend latency, to match a real backend's pace

Usage:

    python3 semantic-embed-rss.py \\
        --binary target/debug/aft --workdir /tmp/embed-rss \\
        --files 2000 --symbols 20 --delay-ms 150 --seconds 600

Each sample line carries the batch number, so a leak tied to batches shows as a
rising bytes-per-batch slope. A build with no accumulator has a flat slope equal
to the index's own per-chunk cost, and the resident set returns to its baseline
when the build is dropped.

One setup detail that is easy to lose an afternoon to: `semantic.backend` is a
user-scoped setting. A project-scoped `.cortexkit/aft.jsonc` silently drops it
and the daemon falls back to the local backend, so this script writes the
backend into a user config and passes it as `cortexkit_user_config_path`.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

DIMENSION = 384
DELAY_S = 0.0


class StubEmbedHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        inputs = body.get("input") or []
        if isinstance(inputs, str):
            inputs = [inputs]
        if DELAY_S:
            time.sleep(DELAY_S * max(1, len(inputs)) / 64.0)
        vector = [0.125] * DIMENSION
        payload = json.dumps(
            {"data": [{"embedding": vector, "index": i} for i in range(len(inputs))]}
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args) -> None:
        pass


def start_stub():
    server = ThreadingHTTPServer(("127.0.0.1", 0), StubEmbedHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}"


def write_corpus(root: Path, files: int, symbols: int) -> None:
    src = root / "src"
    src.mkdir(parents=True, exist_ok=True)
    doc = "documentation about what this function does and why it exists " * 4
    for index in range(files):
        parts = []
        for symbol in range(symbols):
            parts.append(
                f"/// {doc}\n"
                f"pub fn symbol_{index}_{symbol}(input: &str, count: usize) -> String {{\n"
                f"    let mut out = String::new();\n"
                f"    for step in 0..count {{\n"
                f'        out.push_str(&format!("{{input}}-{{step}}-{symbol}"));\n'
                f"    }}\n"
                f"    out\n"
                f"}}\n\n"
            )
        (src / f"module_{index}.rs").write_text("".join(parts), encoding="utf-8")


def write_config(
    root: Path, workdir: Path, base_url: str, batch: int, backend: str
) -> Path:
    project = {"semantic_search": True}
    target = root / ".cortexkit"
    target.mkdir(parents=True, exist_ok=True)
    (target / "aft.jsonc").write_text(json.dumps(project, indent=2), encoding="utf-8")

    if backend == "fastembed":
        # The local backend embeds in-process through ONNX. It ignores base_url
        # and only accepts the bundled model, so the stub server sits idle for
        # these runs and batch pacing comes from real inference instead of
        # --delay-ms. This is the only configuration that instantiates the local
        # embedder, and therefore the only one that reads the cgroup CPU quota.
        semantic = {
            "backend": "fastembed",
            "model": "all-MiniLM-L6-v2",
            "max_batch_size": batch,
            "max_files": 50000,
            "timeout_ms": 600000,
        }
    else:
        semantic = {
            "backend": "openai_compatible",
            "model": "stub-embedding",
            "base_url": base_url,
            "max_batch_size": batch,
            "max_files": 50000,
            "timeout_ms": 600000,
            "max_input_tokens": 512,
        }

    # `semantic.backend` is user-scoped. A project-scoped .cortexkit/aft.jsonc
    # drops it without complaint and the daemon falls back to the local backend,
    # so a run meant to measure the remote lane would silently measure the local
    # one. Writing it into a user config and passing the path is what makes the
    # requested backend actually take effect.
    user_path = workdir / "user-aft.jsonc"
    user_path.write_text(json.dumps({"semantic": semantic}, indent=2), encoding="utf-8")
    return user_path


def rss_bytes(pid: int) -> int:
    if sys.platform == "darwin":
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
        ).stdout.strip()
        return int(out) * 1024 if out else 0
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except OSError:
        return 0
    return 0


BATCH_RE = re.compile(r"stage=embed batch=(\d+) total_batches=(\d+)")


class Daemon:
    def __init__(self, binary, root, storage, stderr_path):
        self._stderr = stderr_path.open("w+", encoding="utf-8")
        env = os.environ.copy()
        env["AFT_STORAGE_DIR"] = str(storage)
        env["RUST_LOG"] = "info"
        self.proc = subprocess.Popen(
            [binary],
            cwd=root,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self._stderr,
            bufsize=0,
        )
        self._next = 0
        self._lock = threading.Lock()
        self._responses = {}
        self._buffer = b""
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while True:
            chunk = self.proc.stdout.read(65536)
            if not chunk:
                return
            self._buffer += chunk
            while b"\n" in self._buffer:
                line, self._buffer = self._buffer.split(b"\n", 1)
                try:
                    frame = json.loads(line)
                except json.JSONDecodeError:
                    continue
                with self._lock:
                    self._responses[frame.get("id")] = frame

    def send(self, command, **params):
        with self._lock:
            self._next += 1
            request_id = str(self._next)
        payload = {"id": request_id, "command": command, **params}
        self.proc.stdin.write(json.dumps(payload).encode() + b"\n")
        self.proc.stdin.flush()
        return request_id

    def wait(self, request_id, timeout_s=120.0):
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            with self._lock:
                if request_id in self._responses:
                    return self._responses.pop(request_id)
            time.sleep(0.02)
        return None


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--workdir", required=True)
    parser.add_argument("--files", type=int, default=1500)
    parser.add_argument("--symbols", type=int, default=20)
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument("--delay-ms", type=float, default=0.0)
    parser.add_argument("--seconds", type=float, default=600.0)
    parser.add_argument("--interval", type=float, default=10.0)
    parser.add_argument("--calls-per-second", type=float, default=2.0)
    parser.add_argument("--reconfigure-every", type=float, default=0.0)
    parser.add_argument(
        "--backend",
        choices=["openai_compatible", "fastembed"],
        default="openai_compatible",
        help="fastembed runs the in-process ONNX embedder and needs its model "
        "downloaded once; --delay-ms has no effect there because real "
        "inference sets the pace",
    )
    parser.add_argument("--label", default="agent")
    args = parser.parse_args()

    global DELAY_S
    DELAY_S = args.delay_ms / 1000.0

    workdir = Path(args.workdir).resolve()
    if workdir.exists():
        shutil.rmtree(workdir)
    root = workdir / "project"
    storage = workdir / "storage"
    root.mkdir(parents=True)
    storage.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)

    server, base_url = start_stub()
    write_corpus(root, args.files, args.symbols)
    user_config = write_config(root, workdir, base_url, args.batch_size, args.backend)
    subprocess.run(["git", "add", "-A"], cwd=root, check=True)
    subprocess.run(
        ["git", "-c", "user.email=h@x", "-c", "user.name=h", "commit", "-qm", "corpus"],
        cwd=root,
        check=True,
    )

    stderr_path = workdir / "daemon.log"
    daemon = Daemon(args.binary, root, storage, stderr_path)
    request_id = daemon.send(
        "configure",
        project_root=str(root),
        harness="opencode",
        storage_dir=str(storage),
        cortexkit_user_config_path=str(user_config),
    )
    daemon.wait(request_id, timeout_s=240.0)

    stop = threading.Event()
    call_count = [0]

    def agent_loop():
        session = "soak-session"
        index = 0
        while not stop.is_set():
            index += 1
            tool, arguments = [
                ("aft_search", {"query": f"symbol_{index % 500} formatting helper"}),
                ("read", {"filePath": f"src/module_{index % args.files}.rs"}),
                ("grep", {"pattern": f"symbol_{index % 200}_3", "path": "src"}),
                ("aft_outline", {"target": f"src/module_{index % args.files}.rs"}),
            ][index % 4]
            request_id = daemon.send(
                "tool_call", session_id=session, name=tool, arguments=arguments
            )
            daemon.wait(request_id, timeout_s=60.0)
            call_count[0] += 1
            time.sleep(max(0.0, 1.0 / args.calls_per_second))

    threading.Thread(target=agent_loop, daemon=True).start()

    reconfigure_count = [0]

    def reconfigure_loop():
        while not stop.is_set():
            time.sleep(args.reconfigure_every)
            if stop.is_set():
                break
            rid = daemon.send(
                "configure",
                project_root=str(root),
                harness="opencode",
                storage_dir=str(storage),
                cortexkit_user_config_path=str(user_config),
            )
            daemon.wait(rid, timeout_s=120.0)
            reconfigure_count[0] += 1

    if args.reconfigure_every > 0:
        threading.Thread(target=reconfigure_loop, daemon=True).start()

    samples = []
    deadline = time.monotonic() + args.seconds
    read_pos = 0
    last_batch = 0
    total_batches = 0
    start = time.monotonic()
    try:
        while time.monotonic() < deadline and daemon.proc.poll() is None:
            time.sleep(args.interval)
            with open(stderr_path, "r", encoding="utf-8", errors="replace") as handle:
                handle.seek(read_pos)
                text = handle.read()
                read_pos = handle.tell()
            for match in BATCH_RE.finditer(text):
                last_batch = int(match.group(1))
                total_batches = int(match.group(2))
            sample = {
                "elapsed_s": round(time.monotonic() - start, 1),
                "batch": last_batch,
                "total_batches": total_batches,
                "tool_calls": call_count[0],
                "reconfigures": reconfigure_count[0],
                "rss_bytes": rss_bytes(daemon.proc.pid),
            }
            samples.append(sample)
            print(
                f"{args.label} t={sample['elapsed_s']:>7.1f}s "
                f"batch={sample['batch']:>5}/{sample['total_batches']:<5} "
                f"calls={sample['tool_calls']:>5} cfg={sample['reconfigures']:>3} "
                f"rss={sample['rss_bytes'] / 1e6:>9.1f} MB",
                flush=True,
            )
    finally:
        stop.set()
        time.sleep(0.5)
        daemon.proc.terminate()
        try:
            daemon.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.proc.kill()
        server.shutdown()

    out = workdir / f"{args.label}-samples.json"
    out.write_text(json.dumps(samples, indent=2), encoding="utf-8")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
