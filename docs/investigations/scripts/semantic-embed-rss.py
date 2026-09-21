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

Content is a variable too, and for a long time it was the one this harness held
still. Every earlier arm ran uniform ASCII Rust, while the tree that reports the
problem is Cyrillic properties, dense XML and base64 literals. Two knobs move
that:

  --corpus     content class of the semantically indexed spine files, whose
               content reaches the chunker and the embed batches
  --sidecars   content class of files the semantic index does not accept, which
               are still walked, watched and trigram-indexed

A third knob makes the backend behave like the one in the report rather than
like an accommodating stub:

  --reject-over-tokens  answer 400 exceed_context_size_error above this many
                        tokens, which is what drives the overflow recovery
                        (recursive batch bisection plus row shrinking)

The stub charges non-ASCII characters about a token each, because an English
WordPiece vocabulary has no pieces for them. That is the divergence the daemon's
own character-based estimate cannot see.

One setup detail that is easy to lose an afternoon to: `semantic.backend` is a
user-scoped setting. A project-scoped `.cortexkit/aft.jsonc` silently drops it
and the daemon falls back to the local backend, so this script writes the
backend into a user config and passes it as `cortexkit_user_config_path`.
"""

from __future__ import annotations

import argparse
import json
import math
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

# When set, the stub refuses any request carrying a row longer than this many
# tokens, answering with llama.cpp's `exceed_context_size_error` 400. That is
# the reply the daemon's overflow recovery keys on: it bisects the batch and
# then shrinks the offending row. Left at 0 the stub accepts everything, which
# is what every earlier round measured -- so the recovery path had never run
# under the harness at all.
REJECT_OVER_TOKENS = 0

# How the stub turns a row into a token count.
#
# A WordPiece vocabulary built for English (all-MiniLM-L6-v2's is) covers ASCII
# text at roughly three and a half characters per token, and falls back to
# per-byte pieces for anything outside it -- so a Cyrillic character costs about
# a token on its own. That gap is the whole mechanism behind #318: the daemon
# sizes a row by characters at the ASCII ratio, so a row it believes fits 512
# tokens can arrive at the backend as double that. Modelling the two character
# classes separately is what lets a corpus trip the real limit here.
#
# This is an approximation of a tokenizer, not a tokenizer. It does not model
# entropy, so high-entropy ASCII (base64) is charged the ordinary English rate
# even though a real BPE splits it far more finely. Arms that need the overflow
# path without relying on that are run by lowering the limit instead.
ASCII_CHARS_PER_TOKEN = 3.5
NON_ASCII_TOKENS_PER_CHAR = 1.0


def estimate_tokens(text: str) -> int:
    non_ascii = sum(1 for character in text if ord(character) > 127)
    ascii_chars = len(text) - non_ascii
    return math.ceil(
        ascii_chars / ASCII_CHARS_PER_TOKEN + non_ascii * NON_ASCII_TOKENS_PER_CHAR
    )


STUB_STATS = {
    "requests": 0,
    "rejections": 0,
    "rows": 0,
    "max_row_chars": 0,
    "max_row_bytes": 0,
    "max_row_tokens": 0,
}
STUB_LOCK = threading.Lock()


class StubEmbedHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        inputs = body.get("input") or []
        if isinstance(inputs, str):
            inputs = [inputs]

        worst_tokens = 0
        worst_chars = 0
        worst_bytes = 0
        for text in inputs:
            worst_chars = max(worst_chars, len(text))
            worst_bytes = max(worst_bytes, len(text.encode("utf-8")))
            worst_tokens = max(worst_tokens, estimate_tokens(text))

        with STUB_LOCK:
            STUB_STATS["requests"] += 1
            STUB_STATS["rows"] += len(inputs)
            STUB_STATS["max_row_chars"] = max(STUB_STATS["max_row_chars"], worst_chars)
            STUB_STATS["max_row_bytes"] = max(STUB_STATS["max_row_bytes"], worst_bytes)
            STUB_STATS["max_row_tokens"] = max(
                STUB_STATS["max_row_tokens"], worst_tokens
            )
            rejecting = bool(REJECT_OVER_TOKENS) and worst_tokens > REJECT_OVER_TOKENS
            if rejecting:
                STUB_STATS["rejections"] += 1

        if rejecting:
            payload = json.dumps(
                {
                    "error": {
                        "type": "exceed_context_size_error",
                        "message": "input is too large to process",
                        "n_prompt_tokens": worst_tokens,
                        "n_ctx": REJECT_OVER_TOKENS,
                    }
                }
            ).encode()
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

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


# ---------------------------------------------------------------------------
# Corpus content classes
#
# Earlier rounds measured one content class only: uniform ASCII Rust. The
# reporter's tree is none of those things -- it is a BPMN/Java repository full
# of Cyrillic `.properties`, dense XML and base64 literals, which are the same
# content classes that produced #318. Content is therefore the variable these
# generators exist to move, with file count, symbol count and every other knob
# held where the earlier arms had them so a difference is attributable.
#
# Two families of shape are generated separately because they reach different
# code:
#
#   spine files    carry a semantically indexed extension (.rs/.java), so their
#                  content reaches the chunker and the embed batches this
#                  harness measures against.
#   sidecar files  carry an extension the semantic index does not accept
#                  (.properties/.xml/.json). They are still walked, watched and
#                  trigram-indexed, so they exercise every plane except the one
#                  the embed batch counter belongs to.
#
# Everything is generated from the index rather than from a random source, so
# two arms with the same shape produce byte-identical corpora.
# ---------------------------------------------------------------------------

RUSSIAN_WORDS = (
    "процесс задача поток управления шлюз событие подписка участник "
    "документ согласование заявка маршрут исполнитель регламент срок "
    "уведомление вложение подразделение сотрудник резолюция поручение "
    "контроль исполнение отклонение возврат делегирование эскалация "
    "формуляр реквизит справочник значение параметр настройка"
).split()

B64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

# One pool sliced many times, rather than a fresh blob per literal: generating
# tens of millions of characters one at a time in Python costs minutes of the
# run for no measurement value.
B64_POOL = "".join(B64_ALPHABET[(i * 37 + (i // 64) * 11) % 64] for i in range(16384))

ASCII_DOC = "documentation about what this function does and why it exists " * 4


def russian_text(seed: int, words: int) -> str:
    """Deterministic Cyrillic phrase. Two bytes per character in UTF-8, which is
    exactly the property that makes byte length and character count diverge."""
    return " ".join(
        RUSSIAN_WORDS[(seed * 7 + index * 13) % len(RUSSIAN_WORDS)]
        for index in range(words)
    )


def base64_blob(seed: int, length: int) -> str:
    start = (seed * 97) % max(1, len(B64_POOL) - length)
    return B64_POOL[start : start + length]


def dense_xml(seed: int, depth: int = 24, attrs: int = 3) -> str:
    """Deeply nested XML emitted as a single line, so one 'line' is kilobytes
    long. Line-oriented readers see one enormous line rather than many small
    ones, which is the shape the reporter named."""
    parts = []
    for level in range(depth):
        attributes = " ".join(
            f'bpmn:attr{level}_{index}="value-{seed}-{level}-{index}"'
            for index in range(attrs)
        )
        parts.append(f"<bpmn:element{level} {attributes}>")
    parts.append(f"<![CDATA[{base64_blob(seed, 256)}]]>")
    for level in reversed(range(depth)):
        parts.append(f"</bpmn:element{level}>")
    return "".join(parts)


def rust_symbol(file_index: int, symbol: int, _class: str) -> str:
    return (
        f"/// {ASCII_DOC}\n"
        f"pub fn symbol_{file_index}_{symbol}(input: &str, count: usize) -> String {{\n"
        f"    let mut out = String::new();\n"
        f"    for step in 0..count {{\n"
        f'        out.push_str(&format!("{{input}}-{{step}}-{symbol}"));\n'
        f"    }}\n"
        f"    out\n"
        f"}}\n\n"
    )


def java_symbol(file_index: int, symbol: int, content_class: str) -> str:
    """One Java method per symbol. The method's identity, arity and count are
    identical across classes; only the documentation and body content change,
    so chunk count per file does not move with the content class."""
    seed = file_index * 101 + symbol
    if content_class == "cyrillic":
        doc = russian_text(seed, 28)
        lines = [
            f'        values.put("{russian_text(seed + line, 2)}", '
            f'"{russian_text(seed + line * 3, 9)}");'
            for line in range(6)
        ]
        body = "\n".join(lines)
    elif content_class == "xml":
        doc = ASCII_DOC
        body = (
            f'        values.put("template", "'
            f'{dense_xml(seed).replace(chr(34), chr(39))}");'
        )
    elif content_class == "base64":
        doc = ASCII_DOC
        body = f'        values.put("blob", "{base64_blob(seed, 3000)}");'
    else:
        doc = ASCII_DOC
        lines = [
            f'        values.put("key-{seed}-{line}", "value {ASCII_DOC[:60]}");'
            for line in range(6)
        ]
        body = "\n".join(lines)

    return (
        f"    /** {doc} */\n"
        f"    public Map<String, String> symbol_{file_index}_{symbol}"
        f"(String input, int count) {{\n"
        f"        Map<String, String> values = new HashMap<>();\n"
        f"{body}\n"
        f"        return values;\n"
        f"    }}\n\n"
    )


# The mix ratio is an assumption, not a number the issue supplies: the reporter
# names Cyrillic properties, dense XML and base64 literals without proportions,
# so the mixed arm rotates evenly through the three and says so.
MIXED_CLASSES = ("cyrillic", "xml", "base64")
SIDECAR_MIXED_KINDS = ("properties", "xml", "base64")


def write_spine_file(path: Path, file_index: int, symbols: int, content_class: str) -> int:
    if content_class == "rust":
        text = "".join(rust_symbol(file_index, s, content_class) for s in range(symbols))
    else:
        per_symbol = []
        for symbol in range(symbols):
            resolved = (
                MIXED_CLASSES[(file_index + symbol) % len(MIXED_CLASSES)]
                if content_class == "mixed"
                else content_class
            )
            per_symbol.append(java_symbol(file_index, symbol, resolved))
        text = (
            "import java.util.HashMap;\n"
            "import java.util.Map;\n\n"
            f"public class Module{file_index} {{\n\n" + "".join(per_symbol) + "}\n"
        )
    data = text.encode("utf-8")
    path.write_bytes(data)
    return len(data)


def write_sidecar_file(path: Path, file_index: int, kind: str, lines: int) -> int:
    if kind == "properties":
        text = "".join(
            f"{russian_text(file_index * 31 + line, 3).replace(' ', '.')}="
            f"{russian_text(file_index * 17 + line, 12)}\n"
            for line in range(lines)
        )
    elif kind == "xml":
        body = "".join(dense_xml(file_index * 13 + line) for line in range(lines // 4 + 1))
        text = f'<?xml version="1.0" encoding="UTF-8"?>\n{body}\n'
    else:
        entries = ",".join(
            f'"blob{line}":"{base64_blob(file_index * 7 + line, 2000)}"'
            for line in range(max(1, lines // 8))
        )
        text = "{" + entries + "}\n"
    data = text.encode("utf-8")
    path.write_bytes(data)
    return len(data)


SPINE_SUFFIX = {"rust": ".rs"}
SIDECAR_SUFFIX = {"properties": ".properties", "xml": ".xml", "base64": ".json"}


def write_corpus(
    root: Path,
    files: int,
    symbols: int,
    content_class: str = "rust",
    sidecars: str = "none",
    sidecar_ratio: float = 0.0,
    sidecar_lines: int = 40,
) -> dict:
    """Write one arm's corpus and report what was written.

    The returned counts are part of the measurement: a content class changes
    corpus bytes for a fixed file count, and a reader comparing two arms needs
    both numbers to know which one moved.
    """
    src = root / "src"
    src.mkdir(parents=True, exist_ok=True)
    suffix = SPINE_SUFFIX.get(content_class, ".java")

    spine_paths = []
    spine_bytes = 0
    for index in range(files):
        name = f"module_{index}{suffix}" if suffix == ".rs" else f"Module{index}.java"
        path = src / name
        spine_bytes += write_spine_file(path, index, symbols, content_class)
        spine_paths.append(f"src/{name}")

    sidecar_bytes = 0
    sidecar_count = 0
    if sidecars != "none" and sidecar_ratio > 0:
        resources = root / "resources"
        resources.mkdir(parents=True, exist_ok=True)
        total = int(files * sidecar_ratio)
        for index in range(total):
            kind = (
                SIDECAR_MIXED_KINDS[index % len(SIDECAR_MIXED_KINDS)]
                if sidecars == "mixed"
                else sidecars
            )
            path = resources / f"resource_{index}{SIDECAR_SUFFIX[kind]}"
            sidecar_bytes += write_sidecar_file(path, index, kind, sidecar_lines)
            sidecar_count += 1

    return {
        "content_class": content_class,
        "spine_files": files,
        "spine_bytes": spine_bytes,
        "sidecar_kind": sidecars,
        "sidecar_files": sidecar_count,
        "sidecar_bytes": sidecar_bytes,
        "spine_paths": spine_paths,
    }


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


def capture_smaps_rollup(pid: int, out_path: Path, sample: dict) -> None:
    """Append /proc/<pid>/smaps_rollup with the sample it belongs to.

    VmRSS cannot say whether a balloon is anonymous heap or mapped artifacts,
    and that is the distinction the reporter's own two snapshots turned on. A
    rollup taken beside each sample makes the same separation available for any
    arm here that grows, without a second run to go and fetch it.
    """
    try:
        rollup = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except OSError:
        return
    with out_path.open("a", encoding="utf-8") as handle:
        handle.write(
            f"=== elapsed_s={sample['elapsed_s']} batch={sample['batch']} "
            f"rss_bytes={sample['rss_bytes']}\n"
        )
        handle.write(rollup)
        handle.write("\n")


def embed_phase_slope(samples: list) -> dict:
    """Resident-set growth per embed batch, fitted across the embed phase only.

    Samples before the first batch belong to discovery and extraction, and
    including them would charge that phase's memory to the embed loop. Least
    squares over the batch-carrying samples is reported next to the plain
    endpoint difference so a curved run cannot hide behind a fitted line.
    """
    points = [(s["batch"], s["rss_bytes"]) for s in samples if s["batch"] > 0]
    # Collapse repeats so a stalled tail does not weight the fit toward zero.
    seen = {}
    for batch, rss in points:
        seen.setdefault(batch, rss)
    points = sorted(seen.items())
    if len(points) < 2:
        return {}

    first_batch, first_rss = points[0]
    last_batch, last_rss = points[-1]
    count = len(points)
    mean_batch = sum(batch for batch, _ in points) / count
    mean_rss = sum(rss for _, rss in points) / count
    variance = sum((batch - mean_batch) ** 2 for batch, _ in points)
    covariance = sum(
        (batch - mean_batch) * (rss - mean_rss) for batch, rss in points
    )
    fitted = covariance / variance if variance else 0.0
    span = last_batch - first_batch
    endpoint = (last_rss - first_rss) / span if span else 0.0
    return {
        "mb_per_batch": fitted / 1e6,
        "endpoint_mb_per_batch": endpoint / 1e6,
        "first_batch": first_batch,
        "last_batch": last_batch,
        "first_rss_bytes": first_rss,
        "last_rss_bytes": last_rss,
        "points": count,
    }


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
    parser.add_argument(
        "--corpus",
        choices=["rust", "java", "cyrillic", "xml", "base64", "mixed"],
        default="rust",
        help="content class of the semantically indexed spine files. 'rust' is "
        "the uniform ASCII corpus every earlier round measured; the rest write "
        "Java with the named content in the method bodies, holding file count "
        "and symbol count fixed so chunk count does not move with the class",
    )
    parser.add_argument(
        "--sidecars",
        choices=["none", "properties", "xml", "base64", "mixed"],
        default="none",
        help="content class of files written with an extension the semantic "
        "index does not accept. They are walked, watched and trigram-indexed "
        "but never chunked, so they isolate every plane except the embed one",
    )
    parser.add_argument("--sidecar-ratio", type=float, default=0.0)
    parser.add_argument("--sidecar-lines", type=int, default=40)
    parser.add_argument(
        "--reject-over-tokens",
        type=int,
        default=0,
        help="make the stub answer 400 exceed_context_size_error for any row "
        "over this many tokens, the reply llama.cpp sends and the one the "
        "overflow recovery from #318 keys on. 0 accepts everything",
    )
    parser.add_argument(
        "--ascii-chars-per-token",
        type=float,
        default=3.5,
        help="stub token model: ASCII characters per token, the ratio the "
        "daemon itself applies to every character regardless of script",
    )
    parser.add_argument(
        "--non-ascii-tokens-per-char",
        type=float,
        default=1.0,
        help="stub token model: tokens charged per non-ASCII character. An "
        "English WordPiece vocabulary has no pieces for Cyrillic and falls "
        "back to per-byte ones, so a character costs about a token",
    )
    parser.add_argument(
        "--smaps",
        action="store_true",
        help="capture /proc/<pid>/smaps_rollup beside every sample (Linux only). "
        "VmRSS cannot separate anonymous heap from mapped artifacts and that "
        "distinction is what says whether a balloon is allocation",
    )
    args = parser.parse_args()

    global DELAY_S, REJECT_OVER_TOKENS
    global ASCII_CHARS_PER_TOKEN, NON_ASCII_TOKENS_PER_CHAR
    DELAY_S = args.delay_ms / 1000.0
    REJECT_OVER_TOKENS = args.reject_over_tokens
    ASCII_CHARS_PER_TOKEN = args.ascii_chars_per_token
    NON_ASCII_TOKENS_PER_CHAR = args.non_ascii_tokens_per_char

    workdir = Path(args.workdir).resolve()
    if workdir.exists():
        shutil.rmtree(workdir)
    root = workdir / "project"
    storage = workdir / "storage"
    root.mkdir(parents=True)
    storage.mkdir(parents=True)
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)

    server, base_url = start_stub()
    corpus = write_corpus(
        root,
        args.files,
        args.symbols,
        content_class=args.corpus,
        sidecars=args.sidecars,
        sidecar_ratio=args.sidecar_ratio,
        sidecar_lines=args.sidecar_lines,
    )
    spine_paths = corpus.pop("spine_paths")
    print(
        f"{args.label} corpus={corpus['content_class']} "
        f"spine={corpus['spine_files']} files/{corpus['spine_bytes'] / 1e6:.1f} MB "
        f"sidecar={corpus['sidecar_kind']} {corpus['sidecar_files']} files/"
        f"{corpus['sidecar_bytes'] / 1e6:.1f} MB",
        flush=True,
    )
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
            spine = spine_paths[index % len(spine_paths)]
            tool, arguments = [
                ("aft_search", {"query": f"symbol_{index % 500} formatting helper"}),
                ("read", {"filePath": spine}),
                ("grep", {"pattern": f"symbol_{index % 200}_3", "path": "src"}),
                ("aft_outline", {"target": spine}),
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
    smaps_path = workdir / f"{args.label}-smaps-rollup.txt"
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
            with STUB_LOCK:
                stub = dict(STUB_STATS)
            sample = {
                "elapsed_s": round(time.monotonic() - start, 1),
                "batch": last_batch,
                "total_batches": total_batches,
                "tool_calls": call_count[0],
                "reconfigures": reconfigure_count[0],
                "embed_requests": stub["requests"],
                "embed_rejections": stub["rejections"],
                "rss_bytes": rss_bytes(daemon.proc.pid),
            }
            samples.append(sample)
            if args.smaps:
                capture_smaps_rollup(daemon.proc.pid, smaps_path, sample)
            print(
                f"{args.label} t={sample['elapsed_s']:>7.1f}s "
                f"batch={sample['batch']:>5}/{sample['total_batches']:<5} "
                f"calls={sample['tool_calls']:>5} cfg={sample['reconfigures']:>3} "
                f"req={sample['embed_requests']:>6} "
                f"rej={sample['embed_rejections']:>6} "
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

    result = {
        "label": args.label,
        "corpus": corpus,
        "reject_over_tokens": args.reject_over_tokens,
        "ascii_chars_per_token": args.ascii_chars_per_token,
        "non_ascii_tokens_per_char": args.non_ascii_tokens_per_char,
        "stub": dict(STUB_STATS),
        "embed_requests": STUB_STATS["requests"],
        "embed_rejections": STUB_STATS["rejections"],
        "embed_rows": STUB_STATS["rows"],
        "slope": embed_phase_slope(samples),
        "samples": samples,
    }
    out = workdir / f"{args.label}-samples.json"
    out.write_text(json.dumps(result, indent=2), encoding="utf-8")
    slope = result["slope"]
    if slope:
        print(
            f"{args.label} SLOPE {slope['mb_per_batch']:.3f} MB/batch "
            f"over batch {slope['first_batch']}->{slope['last_batch']} "
            f"({slope['first_rss_bytes'] / 1e6:.1f} -> "
            f"{slope['last_rss_bytes'] / 1e6:.1f} MB), "
            f"embed requests={result['embed_requests']} "
            f"rejections={result['embed_rejections']}",
            flush=True,
        )
    else:
        print(f"{args.label} SLOPE unavailable: fewer than two embed samples")
    print(f"wrote {out}")
    print(
        f"{args.label} ROWS max_chars={STUB_STATS['max_row_chars']} "
        f"max_bytes={STUB_STATS['max_row_bytes']} "
        f"max_tokens={STUB_STATS['max_row_tokens']} "
        f"(limit={args.reject_over_tokens or 'none'})",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
