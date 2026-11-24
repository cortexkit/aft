"""Offline oracle: parsing rules from .cortexkit/scripts/aft-perf-sweep.sh.

Keep the script's first-match fields, intentional-wait exclusions, floor p95,
WARN normalization and insertion-order tie breaking. JSON exposes the numbers
before the original script formats them as markdown. The per-tool statistics
extend its per-tool counts for the flow digest.
"""
import json
import re
import statistics
import sys

log_path, since = sys.argv[1:3]
with open(log_path, errors="replace") as fh:
    lines = [line for line in fh if line[:19] >= since]
INTENTIONAL_WAITS = ("name=bash_watch ", "name=bash_status ")


def nums(pattern, field, source=lines):
    out = []
    for line in source:
        if pattern in line and not any(w in line for w in INTENTIONAL_WAITS):
            m = re.search(rf"\b{field}=(\d+)", line)
            if m:
                out.append(int(m.group(1)))
    return out


def stat(xs):
    if not xs:
        return None
    return {"n": len(xs), "median": statistics.median(xs),
            "p95": sorted(xs)[int(0.95 * (len(xs) - 1))], "max": max(xs)}


slow = [line for line in lines if "slow tool_call" in line
        and not any(w in line for w in INTENTIONAL_WAITS)]
by_tool = {}
for line in slow:
    m = re.search(r"name=(\S+)", line)
    if m:
        by_tool.setdefault(m.group(1), []).append(line)
real = [line for line in lines if "bash spawned" not in line]
warns = {}
for line in real:
    if " WARN " in line:
        key = re.sub(r"\d+", "N", re.sub(r"\[ses_[^\]]+\] ", "",
                     line.split(" WARN ", 1)[1]))[:90]
        warns[key] = warns.get(key, 0) + 1
stats = {f"slow_{field}": stat(nums("slow tool_call", field))
         for field in ("total", "queue", "exec")}
stats.update({f"configure_{field}": stat(nums("configure prefix slow", field))
              for field in ("total", "cache_key_resolve", "worktree_probe")})
stats["cold_build_wait"] = stat([int(m.group(1)) for line in lines
                                 for m in [re.search(r"slot acquired after (\d+)ms", line)] if m])
stats["semantic_view_fill"] = stat(nums("] semantic view fill root=", "elapsed_ms"))
stats["call_graph_derive"] = stat(nums("view_publication", "derived_ms"))
print(json.dumps({
    "lines_in_window": len(lines), "stats": stats,
    "slow_by_tool": [{"name": name, "count": len(source),
                      **{field: stat(nums("slow tool_call", field, source))
                         for field in ("total", "queue", "exec")}}
                     for name, source in sorted(by_tool.items(), key=lambda kv: -len(kv[1]))[:8]],
    "tools_total": len(by_tool),
    "bash_start_deadline": sum("bash_start_deadline" in line for line in lines),
    "warn": sum(" WARN " in line for line in real),
    "error": sum(" ERROR " in line for line in real),
    "panic": sum("panicked at" in line for line in real),
    "warn_shapes": [{"shape": key.strip(), "count": value}
                    for key, value in sorted(warns.items(), key=lambda kv: -kv[1])[:8]],
    "warn_shapes_total": len(warns),
}))
