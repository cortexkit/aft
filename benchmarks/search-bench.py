#!/usr/bin/env python3
"""AFT vs codedb vs ripgrep vs ast-grep — search benchmark across multiple codebases."""
import subprocess, json, time, sys, os, select, tempfile

# ─── Config ───
CODEDB = os.path.expanduser("~/Work/OSS/codedb/zig-out/bin/codedb")
AFT = os.path.expanduser("~/.cache/aft/bin/v0.8.1/aft")  # or target/release/aft
REPOS = [
    ("opencode-aft", os.path.expanduser("~/Work/OSS/opencode-aft"), "~500 files"),
    ("reth", os.path.expanduser("~/Work/OSS/reth"), "~1.2K Rust files"),
    # ("chromium", os.path.expanduser("~/Work/OSS/chromium"), "~490K files"),  # too large for codedb
]
ITERS = 10
WARMUP = 2

W = '\033[1;37m'
G = '\033[0;32m'
C = '\033[0;36m'
D = '\033[0;90m'
Y = '\033[0;33m'
B = '\033[0;34m'
R = '\033[0;31m'
N = '\033[0m'

# ─── AFT NDJSON Client ───
class AftClient:
    def __init__(self, repo, file_count=1000):
        self.proc = subprocess.Popen(
            [AFT], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0)
        self.buf = b""
        self.id = 0
        # Configure
        self._send({"id": "cfg", "command": "configure",
                     "project_root": repo, "experimental_search_index": True})
        self._recv()
        # Wait for index to build — scale with repo size
        time.sleep(min(30, max(2, file_count / 5000)))
        # Drain any pending events by sending a ping
        self._send({"id": "ping", "command": "version"})
        self._recv()

    def _send(self, obj):
        line = json.dumps(obj) + "\n"
        self.proc.stdin.write(line.encode())
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
                    try:
                        return json.loads(line.strip())
                    except json.JSONDecodeError:
                        continue
        return None

    def call(self, command, params=None):
        self.id += 1
        msg = {"id": str(self.id), "command": command}
        if params:
            msg.update(params)
        self._send(msg)
        return self._recv()

    def close(self):
        self.proc.terminate()
        self.proc.wait()

    def memory_rss_mb(self):
        """Get RSS memory in MB."""
        try:
            pid = self.proc.pid
            r = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                              capture_output=True, text=True)
            return int(r.stdout.strip()) / 1024
        except:
            return 0


# ─── CodeDB MCP Client ───
class CodedbMcpClient:
    def __init__(self, repo):
        self.proc = subprocess.Popen(
            [CODEDB, "mcp", repo], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, bufsize=0)
        self.id = 0
        self.buf = b""
        self._init()

    def _send(self, obj):
        line = json.dumps(obj) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()

    def _recv(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if select.select([self.proc.stdout], [], [], 0.1)[0]:
                chunk = os.read(self.proc.stdout.fileno(), 65536)
                if chunk:
                    self.buf += chunk
            text = self.buf.decode(errors="replace")
            while "\n" in text:
                line, rest = text.split("\n", 1)
                text = rest
                self.buf = rest.encode()
                line = line.strip()
                if not line:
                    continue
                try:
                    return json.loads(line)
                except json.JSONDecodeError:
                    continue
        return None

    def _init(self):
        self._send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                     "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "bench", "version": "1.0"}}})
        resp = self._recv()
        if not resp:
            raise RuntimeError("codedb MCP init failed - no response")
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        # Wait for codedb to load snapshot/index
        time.sleep(2)

    def call(self, tool, args):
        self.id += 1
        self._send({"jsonrpc": "2.0", "id": self.id, "method": "tools/call",
                     "params": {"name": tool, "arguments": args}})
        return self._recv()

    def close(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=5)
        except:
            self.proc.kill()

    def memory_rss_mb(self):
        try:
            pid = self.proc.pid
            r = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)],
                              capture_output=True, text=True)
            return int(r.stdout.strip()) / 1024
        except:
            return 0




# ─── Timing helpers ───
def time_aft(client, command, params, iters=ITERS, warmup=WARMUP):
    for _ in range(warmup):
        client.call(command, params)
    start = time.perf_counter()
    result = None
    for _ in range(iters):
        result = client.call(command, params)
    elapsed = (time.perf_counter() - start) / iters * 1000
    return elapsed, result

def time_codedb(client, tool, args, iters=ITERS, warmup=WARMUP):
    for _ in range(warmup):
        client.call(tool, args)
    start = time.perf_counter()
    result = None
    for _ in range(iters):
        result = client.call(tool, args)
    elapsed = (time.perf_counter() - start) / iters * 1000
    return elapsed, result

def time_cmd(args, iters=3):
    subprocess.run(args, capture_output=True)  # warmup
    start = time.perf_counter()
    result = None
    for _ in range(iters):
        result = subprocess.run(args, capture_output=True, text=True)
    elapsed = (time.perf_counter() - start) / iters * 1000
    return elapsed, result

def count_rg_matches(output):
    if not output or not output.strip():
        return 0
    return len([l for l in output.strip().split('\n') if l.strip()])

def count_aft_matches(resp):
    if not resp or not resp.get("success"):
        return 0
    return resp.get("total_matches", 0)

def count_codedb_matches(resp):
    if not resp or "result" not in resp:
        return 0
    content = resp.get("result", {}).get("content", [])
    text = ""
    for item in content:
        if item.get("type") == "text":
            text += item["text"]
    return len([l for l in text.strip().split('\n') if l.strip()]) if text.strip() else 0

def token_estimate(text):
    return max(1, len(str(text)) // 4)

def format_ms(ms):
    if ms < 1:
        return f"{ms:.3f}"
    elif ms < 100:
        return f"{ms:.2f}"
    elif ms < 1000:
        return f"{ms:.1f}"
    else:
        return f"{ms:.0f}"


# ─── Search Queries per repo ───
QUERIES = {
    "opencode-aft": [
        ("literal", "validate_path", "*.rs"),
        ("literal", "BinaryBridge", "*.ts"),
        ("literal", "fn handle_", None),
        ("regex", r"fn\s+\w+_test", "*.rs"),
        ("regex", r"async\s+function", "*.ts"),
    ],
    "reth": [
        ("literal", "impl Display for", "*.rs"),
        ("literal", "async fn", "*.rs"),
        ("literal", "todo!", None),
        ("regex", r"pub\s+struct\s+\w+<", "*.rs"),
        ("regex", r"#\[derive\(.*Clone", "*.rs"),
    ],
    "chromium": [
        ("literal", "WebContents", "*.cc"),
        ("literal", "DCHECK", "*.cc"),
        ("literal", "#include", "*.h"),
        ("regex", r"void\s+\w+::On\w+", "*.cc"),
        ("regex", r"std::unique_ptr<", "*.cc"),
    ],
}


# ─── Main ───
def main():
    global AFT
    # Check binaries
    if not os.path.exists(AFT):
        # Try target/release
        alt = os.path.join(os.path.dirname(os.path.dirname(__file__)), "target", "release", "aft")
        if os.path.exists(alt):
            AFT = alt
        else:
            print(f"{R}AFT binary not found at {AFT}{N}")
            sys.exit(1)

    if not os.path.exists(CODEDB):
        print(f"{R}codedb binary not found at {CODEDB}{N}")
        sys.exit(1)

    cpu = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"],
                        capture_output=True, text=True).stdout.strip()
    ram = int(subprocess.run(["sysctl", "-n", "hw.memsize"],
                            capture_output=True, text=True).stdout.strip()) // (1024**3)

    print(f"\n{W}{'═'*80}{N}")
    print(f"{W}  AFT vs codedb vs ripgrep vs ast-grep — Search Benchmark{N}")
    print(f"{W}{'═'*80}{N}")
    print(f"{D}  Machine:  {cpu}{N}")
    print(f"{D}  RAM:      {ram}GB{N}")
    print(f"{D}  Date:     {time.strftime('%Y-%m-%d %H:%M')}{N}")
    print(f"{D}  Warmup:   {WARMUP} iterations{N}")
    print(f"{D}  Measured:  {ITERS} iterations avg{N}")
    print(f"{D}  AFT:      {AFT}{N}")
    print(f"{D}  codedb:   {CODEDB}{N}")
    print()

    all_results = []

    for repo_name, repo_path, desc in REPOS:
        if not os.path.exists(repo_path):
            print(f"{Y}  Skipping {repo_name} — {repo_path} not found{N}\n")
            continue

        # Count files
        file_count = subprocess.run(["rg", "--files", repo_path],
                                    capture_output=True, text=True).stdout.count('\n')

        print(f"{W}{'━'*80}{N}")
        print(f"{W}  {repo_name} ({desc}, {file_count:,} searchable files){N}")
        print(f"{W}{'━'*80}{N}")
        print()

        queries = QUERIES.get(repo_name, QUERIES["opencode-aft"])
        repo_results = {"name": repo_name, "files": file_count, "tests": []}

        # ── 0. Index Build Time ──
        print(f"{C}  0. Index Build Time{N}")

        # AFT — time the configure + index build
        aft_start = time.perf_counter()
        aft_client = AftClient(repo_path, file_count)
        aft_index_ms = (time.perf_counter() - aft_start) * 1000
        aft_mem = aft_client.memory_rss_mb()
        print(f"     {B}AFT{N}       {W}{format_ms(aft_index_ms):>10} ms{N}  RSS: {aft_mem:.1f} MB  (trigram index + watcher)")

        # codedb — time the MCP init (which includes indexing)
        cdb_client = None
        try:
            cdb_start = time.perf_counter()
            cdb_client = CodedbMcpClient(repo_path)
            cdb_index_ms = (time.perf_counter() - cdb_start) * 1000
            cdb_mem = cdb_client.memory_rss_mb()
            print(f"     {G}codedb{N}    {W}{format_ms(cdb_index_ms):>10} ms{N}  RSS: {cdb_mem:.1f} MB  (trigram + word + outline)")
        except Exception as e:
            cdb_index_ms = 0
            cdb_mem = 0
            print(f"     {G}codedb{N}    {R}CRASHED{N} ({e})")

        repo_results["aft_index_ms"] = aft_index_ms
        repo_results["cdb_index_ms"] = cdb_index_ms
        repo_results["aft_mem_mb"] = aft_mem
        repo_results["cdb_mem_mb"] = cdb_mem
        print()

        # ── Run each query ──
        for i, (qtype, pattern, include) in enumerate(queries, 1):
            label = f"{'regex' if qtype == 'regex' else 'literal'}: '{pattern}'"
            if include:
                label += f" ({include})"
            print(f"{C}  {i}. {label}{N}")

            test_result = {"query": pattern, "type": qtype, "include": include}

            # Ground truth: ripgrep
            rg_args = ["rg", "-nH", "--no-heading", "--hidden"]
            if include:
                rg_args.extend(["--glob", include])
            rg_args.extend([pattern, repo_path])
            rg_ms, rg_result = time_cmd(rg_args)
            rg_matches = count_rg_matches(rg_result.stdout if rg_result else "")
            rg_tokens = token_estimate(rg_result.stdout if rg_result else "")
            print(f"     {D}ripgrep{N}   {W}{format_ms(rg_ms):>10} ms{N}  matches: {rg_matches:>6}  ~{rg_tokens:>7} tokens")
            test_result["rg_ms"] = rg_ms
            test_result["rg_matches"] = rg_matches

            # AFT indexed grep
            aft_params = {"pattern": pattern}
            if include:
                aft_params["include"] = [include]
            aft_ms, aft_resp = time_aft(aft_client, "grep", aft_params)
            aft_matches = count_aft_matches(aft_resp)
            aft_status = aft_resp.get("index_status", "?") if aft_resp else "?"
            aft_text = aft_resp.get("text", "") if aft_resp else ""
            aft_tokens = token_estimate(aft_text)
            speedup = rg_ms / aft_ms if aft_ms > 0 else 0
            match_ok = "✓" if abs(aft_matches - rg_matches) <= max(5, rg_matches * 0.1) else "✗"
            print(f"     {B}AFT{N}       {W}{format_ms(aft_ms):>10} ms{N}  matches: {aft_matches:>6}  ~{aft_tokens:>7} tokens  [{aft_status}]  {speedup:.1f}x vs rg  {match_ok}")
            test_result["aft_ms"] = aft_ms
            test_result["aft_matches"] = aft_matches
            test_result["aft_status"] = aft_status

            # codedb search
            if cdb_client:
                try:
                    cdb_ms, cdb_resp = time_codedb(cdb_client, "codedb_search", {"query": pattern})
                    cdb_matches = count_codedb_matches(cdb_resp)
                    cdb_text = json.dumps(cdb_resp) if cdb_resp else ""
                    cdb_tokens = token_estimate(cdb_text)
                    speedup_cdb = rg_ms / cdb_ms if cdb_ms > 0 else 0
                    print(f"     {G}codedb{N}    {W}{format_ms(cdb_ms):>10} ms{N}  matches: {cdb_matches:>6}  ~{cdb_tokens:>7} tokens  {speedup_cdb:.1f}x vs rg")
                except Exception:
                    cdb_ms, cdb_matches = 0, 0
                    print(f"     {G}codedb{N}    {R}CRASHED{N}")
                    cdb_client = None
            else:
                cdb_ms, cdb_matches = 0, 0
                print(f"     {G}codedb{N}    {D}skipped (crashed earlier){N}")
            test_result["cdb_ms"] = cdb_ms
            test_result["cdb_matches"] = cdb_matches

            # ast-grep (only for patterns that make sense structurally)
            if qtype == "literal" and not any(c in pattern for c in '(){}[]<>!#'):
                sg_args = ["sg", "scan", "--pattern", pattern, repo_path]
                sg_ms, sg_result = time_cmd(sg_args)
                sg_matches = count_rg_matches(sg_result.stdout if sg_result else "")
                print(f"     {Y}ast-grep{N}  {W}{format_ms(sg_ms):>10} ms{N}  matches: {sg_matches:>6}")
                test_result["sg_ms"] = sg_ms
            else:
                print(f"     {Y}ast-grep{N}  {D}n/a (regex pattern){N}")

            repo_results["tests"].append(test_result)
            print()

        # Cleanup
        aft_client.close()
        if cdb_client:
            cdb_client.close()
        all_results.append(repo_results)

    # ═══════════════════════════════════════════
    # Summary
    # ═══════════════════════════════════════════
    print(f"\n{W}{'═'*80}{N}")
    print(f"{W}  Summary{N}")
    print(f"{W}{'═'*80}{N}\n")

    for repo in all_results:
        print(f"  {W}{repo['name']}{N} ({repo['files']:,} files)")
        print(f"  Index build: AFT {format_ms(repo['aft_index_ms'])}ms / codedb {format_ms(repo['cdb_index_ms'])}ms")
        print(f"  Memory:      AFT {repo['aft_mem_mb']:.1f}MB / codedb {repo['cdb_mem_mb']:.1f}MB")
        print()
        print(f"  {'Query':<45} {'ripgrep':>8} {'AFT':>8} {'codedb':>8} {'AFT vs rg':>10}")
        print(f"  {'─'*45} {'─'*8} {'─'*8} {'─'*8} {'─'*10}")
        for t in repo["tests"]:
            q = t["query"][:40]
            if t.get("include"):
                q += f" ({t['include']})"
            q = q[:45]
            rg = f"{format_ms(t['rg_ms'])}ms"
            aft = f"{format_ms(t['aft_ms'])}ms"
            cdb = f"{format_ms(t['cdb_ms'])}ms"
            speedup = t['rg_ms'] / t['aft_ms'] if t['aft_ms'] > 0 else 0
            print(f"  {q:<45} {rg:>8} {aft:>8} {cdb:>8} {speedup:>8.1f}x")
        print()

    # Save JSON results
    results_path = os.path.join(os.path.dirname(__file__), "search-bench-results.json")
    with open(results_path, "w") as f:
        json.dump(all_results, f, indent=2)
    print(f"{D}  Results saved to {results_path}{N}")


if __name__ == "__main__":
    main()
