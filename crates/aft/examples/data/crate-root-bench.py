#!/usr/bin/env python3
"""Standalone NDJSON timings on a disposable copy of AFT's tracked checkout.

Usage: crate-root-bench.py BINARY CHECKOUT OUTPUT LABEL
OUTPUT contains the disposable checkout, store, stderr and JSONL observations.
"""
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import time

binary, source, output, label = sys.argv[1:]
binary = str(Path(binary).resolve())
source, output = Path(source).resolve(), Path(output).resolve()
output.mkdir(parents=True, exist_ok=True)
project = output / "project"
project.mkdir()
for name in subprocess.check_output(["git", "ls-files", "-z"], cwd=source).decode().split("\0"):
    if not name:
        continue
    src, dst = source / name, project / name
    if src.is_file():
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
store = output / "store"
results = open(output / "results.jsonl", "w")


def emit(step, start, reply, tries=1):
    row = dict(label=label, step=step, seconds=round(time.monotonic()-start, 4),
               tries=tries, reply=reply)
    results.write(json.dumps(row) + "\n")
    results.flush()
    print(json.dumps({k: v for k, v in row.items() if k != "reply"}), flush=True)


class Aft:
    def __init__(self, tag):
        self.stderr = open(output / (tag + ".stderr"), "w")
        self.p = subprocess.Popen([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=self.stderr, bufsize=0)
        self.buf, self.n = b"", 0

    def call(self, command, **fields):
        self.n += 1
        rid = str(self.n)
        self.p.stdin.write((json.dumps(dict(id=rid, command=command, **fields)) + "\n").encode())
        deadline = time.monotonic() + 240
        while time.monotonic() < deadline:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                frame = json.loads(line)
                if frame.get("id") == rid:
                    return frame
            if select.select([self.p.stdout], [], [], 0.5)[0]:
                chunk = os.read(self.p.stdout.fileno(), 1 << 20)
                if not chunk:
                    raise RuntimeError("AFT exited")
                self.buf += chunk
        raise TimeoutError(command)

    def tool(self, name, **arguments):
        return self.call("tool_call", name=name, session_id="bench", arguments=arguments)

    def configure(self):
        return self.call("configure", harness="opencode", project_root=str(project),
                         storage_dir=str(store), config={"indexes": {"semantic": False}})

    def stop(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()
        self.stderr.close()


def body(frame):
    return (frame.get("text") or json.dumps(frame.get("data", frame))).lower()


def callers(aft):
    return aft.tool("callgraph", op="callers", path="crates/aft/src/fuzzy_match.rs", symbol="find_all_fuzzy")


def ready(aft):
    deadline = time.monotonic()+240
    tries = 0
    while time.monotonic() < deadline:
        tries += 1
        reply = callers(aft)
        if "find_all_fuzzy" in body(reply) and "building" not in body(reply)[:800]:
            return reply, tries
        time.sleep(0.1)
    raise TimeoutError("callgraph ready")


aft = Aft("run")
t = time.monotonic()
aft.configure()
r, n = ready(aft)
emit("cold", t, r, n)
# Let the initial background publication finish before measuring edits.
time.sleep(3)
for i, rel in enumerate(["crates/aft/src/commands/outline.rs", "crates/aft/src/edit.rs"]):
    name = f"bench_ordinary_{i}"
    t = time.monotonic()
    aft.tool("edit", path=rel, appendContent=f'\npub fn {name}() -> usize {{\n    crate::fuzzy_match::find_all_fuzzy("abc", "b").len()\n}}\n')
    n = 0
    while time.monotonic()-t < 180:
        n += 1
        r = aft.tool("callgraph", op="call_tree", path=rel, symbol=name)
        if "find_all_fuzzy" in body(r):
            break
        time.sleep(0.05)
    else:
        raise TimeoutError(name)
    emit(f"ordinary_{i}", t, r, n)

t = time.monotonic()
aft.tool("write", path="crates/aft/src/bench_probe_target.rs", content="pub fn bench_probe_target() -> u32 {\n    7\n}\n")
aft.tool("write", path="crates/aft/src/bench_probe_caller.rs", content="pub fn bench_probe_caller() -> u32 {\n    crate::bench_probe_target::bench_probe_target()\n}\n")
aft.tool("edit", path="crates/aft/src/lib.rs", appendContent="\npub mod bench_probe_target;\npub mod bench_probe_caller;\n")
n = 0
while time.monotonic()-t < 180:
    n += 1
    r = aft.tool("callgraph", op="callers", path="crates/aft/src/bench_probe_target.rs", symbol="bench_probe_target")
    if "bench_probe_caller" in body(r):
        break
    time.sleep(0.1)
else:
    raise TimeoutError("module addition")
emit("module_addition", t, r, n)
# Kill immediately after the answer to exercise an unpublished edit on restart.
aft.p.kill()
aft.p.wait()
aft.stderr.close()
aft = Aft("restart")
t = time.monotonic()
aft.configure()
r, n = ready(aft)
emit("restart", t, r, n)
aft.stop()
results.close()
