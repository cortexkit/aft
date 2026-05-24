#!/usr/bin/env python3
"""AFT vs codedb vs ripgrep vs ast-grep — fair search benchmark.

Key design decisions for fairness:
- No include/exclude filters (codedb doesn't support them)
- Queries chosen to match primarily in source code, not docs
- codedb match count extracted from its "N results" header, not line counting
- All tools search from project root with default settings
"""
import subprocess, json, time, sys, os, select, re

# ─── Config ───
CODEDB = os.path.expanduser("~/Work/OSS/codedb/zig-out/bin/codedb")
AFT = os.path.expanduser("~/.cache/aft/bin/v0.8.1/aft")
REPOS = [
    ("opencode-aft", os.path.expanduser("~/Work/OSS/opencode-aft"), "~500 files"),
    ("codedb", os.path.expanduser("~/Work/OSS/codedb"), "~20 Zig files"),
    ("reth", os.path.expanduser("~/Work/OSS/reth"), "~1.2K Rust files"),
]
ITERS = 10
WARMUP = 2

# Queries: no file filters, chosen to primarily match source code
QUERIES = {
    "opencode-aft": [
        ("validate_path", "function name in Rust source"),
        ("BinaryBridge", "class name in TypeScript"),
        ("fn handle_grep", "specific function"),
        ("experimental_search_index", "config key across Rust+TS"),
    ],
    "codedb": [
        ("readLine", "Zig stdlib function call"),
        ("writeResult", "internal function"),
        ("trigram", "index-related identifier"),
        ("MCP_ID", "constant in benchmark"),
    ],
    "reth": [
        ("impl Display for", "trait impl pattern"),
        ("BlockNumber", "domain type name"),
        ("fn execute", "method name pattern"),
        ("EthApiError", "error type"),
    ],
}

W, G, C, D, Y, B, R, N = '\033[1;37m', '\033[0;32m', '\033[0;36m', '\033[0;90m', '\033[0;33m', '\033[0;34m', '\033[0;31m', '\033[0m'


# ─── AFT Client ───
class AftClient:
    def __init__(self, repo, file_count=1000):
        self.proc = subprocess.Popen(
            [AFT], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0)
        self.buf = b""
        self.id = 0
        self._send({"id": "cfg", "command": "configure",
                     "project_root": repo, "experimental_search_index": True})
        self._recv()
        time.sleep(min(30, max(2, file_count / 5000)))
        self._send({"id": "ping", "command": "version"})
        self._recv()

    def _send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def _recv(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if select.select([self.proc.stdout], [], [], 0.1)[0]:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk:
                    self.buf += chunk
            text = self.buf.decode(errors="replace")
            if "\n" in text:
                line, rest = text.split("\n", 1)
                self.buf = rest.encode()
                if line.strip():
                    try: return json.loads(line.strip())
                    except: continue
        return None

    def call(self, command, params=None):
        self.id += 1
        msg = {"id": str(self.id), "command": command}
        if params: msg.update(params)
        self._send(msg)
        return self._recv()

    def close(self):
        self.proc.terminate(); self.proc.wait()

    def rss_mb(self):
        try:
            r = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)], capture_output=True, text=True)
            return int(r.stdout.strip()) / 1024
        except: return 0


# ─── CodeDB MCP Client ───
class CodedbClient:
    def __init__(self, repo):
        self.proc = subprocess.Popen(
            [CODEDB, "mcp", repo], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0)
        self.id = 0
        self.buf = b""
        # Initialize MCP
        self._send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "bench", "version": "1.0"}}})
        resp = self._recv()
        if not resp:
            raise RuntimeError("codedb init failed")
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        # Wait for index load (codedb indexes synchronously during MCP start)
        time.sleep(3)

    def _send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def _recv(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if select.select([self.proc.stdout], [], [], 0.1)[0]:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk: self.buf += chunk
            text = self.buf.decode(errors="replace")
            while "\n" in text:
                line, rest = text.split("\n", 1)
                text = rest
                self.buf = rest.encode()
                line = line.strip()
                if not line: continue
                try: return json.loads(line)
                except: continue
        return None

    def call(self, tool, args):
        self.id += 1
        self._send({"jsonrpc": "2.0", "id": self.id, "method": "tools/call",
                     "params": {"name": tool, "arguments": args}})
        return self._recv()

    def close(self):
        try: self.proc.terminate(); self.proc.wait(timeout=5)
        except: self.proc.kill()

    def rss_mb(self):
        try:
            r = subprocess.run(["ps", "-o", "rss=", "-p", str(self.proc.pid)], capture_output=True, text=True)
            return int(r.stdout.strip()) / 1024
        except: return 0


# ─── Helpers ───
def time_fn(fn, iters=ITERS, warmup=WARMUP):
    for _ in range(warmup): fn()
    start = time.perf_counter()
    result = None
    for _ in range(iters): result = fn()
    return (time.perf_counter() - start) / iters * 1000, result

def rg_count(output):
    if not output or not output.strip(): return 0
    return len([l for l in output.strip().split('\n') if l.strip()])

def aft_parse(resp):
    """Extract match count and self-reported search_ms from AFT response."""
    if not resp or not resp.get("success"): return 0, 0.0
    return resp.get("total_matches", 0), resp.get("search_ms", 0.0)

def codedb_parse(resp):
    """Extract match count and self-reported timing from codedb response."""
    count = 0
    self_ms = 0.0
    if not resp or "result" not in resp: return count, self_ms
    for item in resp.get("result", {}).get("content", []):
        if item.get("type") == "text":
            clean = re.sub(r'\033\[[0-9;]*m', '', item["text"])
            m = re.search(r'(\d+)\s+results?\s+for', clean)
            if m: count = int(m.group(1))
            # Extract self-reported timing like "⚡ 1.5ms" or "⚡ 722.0µs"
            t = re.search(r'⚡\s*([\d.]+)(ms|µs|ns)', clean)
            if t:
                val = float(t.group(1))
                unit = t.group(2)
                if unit == 'µs': val /= 1000
                elif unit == 'ns': val /= 1_000_000
                self_ms = val
    return count, self_ms

def fmt(ms):
    if ms < 1: return f"{ms:.3f}"
    elif ms < 100: return f"{ms:.2f}"
    elif ms < 1000: return f"{ms:.1f}"
    else: return f"{ms:.0f}"


# ─── Main ───
def main():
    global AFT
    if not os.path.exists(AFT):
        alt = os.path.join(os.path.dirname(os.path.dirname(__file__)), "target", "release", "aft")
        if os.path.exists(alt): AFT = alt
        else: print(f"{R}AFT not found{N}"); sys.exit(1)

    cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()
    ram = int(subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout.strip()) // (1024**3)

    print(f"\n{W}{'═'*90}{N}")
    print(f"{W}  AFT vs codedb vs ripgrep vs ast-grep — Search Benchmark{N}")
    print(f"{W}{'═'*90}{N}")
    print(f"{D}  Machine:   {cpu} / {ram}GB RAM{N}")
    print(f"{D}  Method:    {WARMUP} warmup + {ITERS} measured iterations, avg ms{N}")
    print(f"{D}  Fairness:  No file filters (codedb doesn't support them){N}")
    print()

    for repo_name, repo_path, desc in REPOS:
        if not os.path.exists(repo_path):
            print(f"{Y}  Skipping {repo_name}{N}\n"); continue

        file_count = subprocess.run(["rg", "--files", repo_path], capture_output=True, text=True).stdout.count('\n')
        queries = QUERIES.get(repo_name, QUERIES["opencode-aft"])

        print(f"{W}{'━'*90}{N}")
        print(f"{W}  {repo_name} ({desc}, {file_count:,} files){N}")
        print(f"{W}{'━'*90}{N}\n")

        # Index build
        print(f"{C}  Index Build & Memory{N}")
        aft_start = time.perf_counter()
        aft = AftClient(repo_path, file_count)
        aft_build = (time.perf_counter() - aft_start) * 1000
        print(f"     {B}AFT{N}       build: {fmt(aft_build):>8}ms   RSS: {aft.rss_mb():.0f}MB")

        cdb = None
        try:
            cdb_start = time.perf_counter()
            cdb = CodedbClient(repo_path)
            cdb_build = (time.perf_counter() - cdb_start) * 1000
            print(f"     {G}codedb{N}    build: {fmt(cdb_build):>8}ms   RSS: {cdb.rss_mb():.0f}MB")
        except Exception as e:
            cdb_build = 0
            print(f"     {G}codedb{N}    {R}FAILED: {e}{N}")
        print()

        # Table header
        print(f"  {'Query':<24} {'rg':>7} {'AFT':>7} {'aft-db':>7} {'cdb':>7} {'cdb-db':>7} {'sg':>7}  {'AFT':>5} {'Match':>8}")
        print(f"  {'':24} {'(ms)':>7} {'e2e':>7} {'self':>7} {'e2e':>7} {'self':>7} {'(ms)':>7}  {'vs rg':>5} {'parity':>8}")
        print(f"  {'─'*24} {'─'*7} {'─'*7} {'─'*7} {'─'*7} {'─'*7} {'─'*7}  {'─'*5} {'─'*8}")

        for query, desc in queries:
            # ripgrep (ground truth)
            rg_ms, rg_r = time_fn(lambda q=query: subprocess.run(
                ["rg", "-nH", "--no-heading", "--hidden", q, repo_path],
                capture_output=True, text=True), iters=3)
            rg_n = rg_count(rg_r.stdout if rg_r else "")

            # AFT
            aft_ms, aft_r = time_fn(lambda q=query: aft.call("grep", {"pattern": q}))
            aft_n, aft_self_ms = aft_parse(aft_r)
            aft_status = aft_r.get("index_status", "?") if aft_r else "?"
            speedup = rg_ms / aft_ms if aft_ms > 0 else 0

            # codedb
            cdb_self_ms = 0.0
            if cdb:
                try:
                    cdb_ms, cdb_r = time_fn(lambda q=query: cdb.call("codedb_search", {"query": q}))
                    cdb_n, cdb_self_ms = codedb_parse(cdb_r)
                    cdb_str = f"{fmt(cdb_ms):>9}"
                    cdb_self_str = f"{fmt(cdb_self_ms):>9}"
                except:
                    cdb_ms, cdb_n, cdb_str, cdb_self_str = 0, 0, f"{'CRASH':>9}", f"{'—':>9}"
                    cdb = None
            else:
                cdb_ms, cdb_n, cdb_str, cdb_self_str = 0, 0, f"{'—':>9}", f"{'—':>9}"

            # ast-grep (only for identifiers, not multi-word patterns)
            if ' ' not in query and not any(c in query for c in '(){}[]<>!#\\'):
                sg_ms, sg_r = time_fn(lambda q=query: subprocess.run(
                    ["sg", "scan", "--pattern", q, repo_path],
                    capture_output=True, text=True), iters=3)
                sg_str = f"{fmt(sg_ms):>10}"
            else:
                sg_str = f"{'n/a':>10}"

            # Match parity check
            parity = "✓" if abs(aft_n - rg_n) <= max(5, rg_n * 0.15) else "✗"

            aft_self_str = f"{fmt(aft_self_ms):>7}"
            print(f"  {query:<24} {fmt(rg_ms):>7} {fmt(aft_ms):>7} {aft_self_str} {fmt(cdb_ms) if cdb_ms else '—':>7} {fmt(cdb_self_ms) if cdb_self_ms else '—':>7} {sg_str:>7}  {speedup:>4.0f}x {rg_n:>4}/{aft_n:<4}{parity}")

        print()
        aft.close()
        if cdb: cdb.close()

    print(f"{W}{'═'*90}{N}")
    print(f"{D}  Notes:{N}")
    print(f"{D}  - AFT and ripgrep respect .gitignore; codedb indexes all files including .sisyphus/, docs{N}")
    print(f"{D}  - codedb match counts from its 'N results' header (may differ from line-level rg counts){N}")
    print(f"{D}  - ast-grep n/a for multi-word patterns and regex{N}")
    print(f"{D}  - AFT [Ready] = trigram index active; [Building/Fallback] = direct scan{N}")
    print()


if __name__ == "__main__":
    main()
