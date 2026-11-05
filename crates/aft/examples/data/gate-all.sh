#!/usr/bin/env bash
# Builds the data-layer gates once and runs them in turn: the parity gate on
# this checkout against today's join and database, the edit gate on the
# 430-edit pack, the reuse gate on the pack's commit ends, and the extraction
# gate on every file Git tracks in this checkout. Exits nonzero if any gate
# fails, after running them all.
#
#   bash crates/aft/examples/data/gate-all.sh [sample scale] [threads]
#
# A sample scale above 1 samples the parity gate's queries more sparsely,
# for iterating; milestones use 1.
set -uo pipefail
scale="${1:-1}"
threads="${2:-8}"
out="${TMPDIR:-/tmp}/gate-all"
mkdir -p "$out"
git log -1 --format=%H
# A failed build must stop here: the target folder may be shared and warm, so
# carrying on would run an older commit's gate binaries.
if ! cargo build --profile ci-storm -p agent-file-tools \
  --example resolve_gate --example edit_gate --example reuse_gate \
  --example extract_gate > "$out/build.txt" 2>&1; then
  tail -30 "$out/build.txt"
  echo "build failed; no gate ran"
  exit 2
fi
tail -2 "$out/build.txt"
bin="${CARGO_TARGET_DIR:-target}/ci-storm/examples"
ls -l --time-style=+%FT%T "$bin/resolve_gate" "$bin/edit_gate" "$bin/reuse_gate" "$bin/extract_gate" 2>/dev/null \
  || ls -lT "$bin/resolve_gate" "$bin/edit_gate" "$bin/reuse_gate" "$bin/extract_gate"
status=0
started=$(date +%s)
GATE_SAMPLE_SCALE="$scale" "$bin/resolve_gate" . "$threads" > "$out/parity.txt" 2> "$out/parity.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "parity rc=$rc in $(( $(date +%s) - started ))s"
grep -E "^ *(rows_only|binding_mismatches|dispatch_mismatches|fragment_mismatches|segment_failures|segment_bytes|dead_code_differing)" "$out/parity.txt"
grep -A12 "examples: \[" "$out/parity.txt" | head -14
grep -E "panicked|assertion" -A3 "$out/parity.err" | head -8
started=$(date +%s)
"$bin/edit_gate" crates/aft/examples/data/edit-pack-aft.jsonl.gz "$threads" 5 > "$out/edit.jsonl" 2> "$out/edit.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "edit rc=$rc in $(( $(date +%s) - started ))s"
tail -2 "$out/edit.err"
python3 - "$out/edit.jsonl" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1])]
edits = [row for row in rows[1:] if "total_ms" in row]
def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(round((len(values) - 1) * p)))] if values else None
print("edits", len(edits), "p50/p90/p99/max ms",
      *[round(pct([e["total_ms"] for e in edits], p)) for p in (.5, .9, .99, 1)])
checked = [e for e in edits if e.get("differing_files") is not None]
bad = [e for e in edits if e.get("differing_files") or e.get("differing_recordings") or e.get("undiscovered")]
print("checked", len(checked), "with differences", len(bad))
for e in bad[:3]:
    print(" ", e["commit"], e["seq"], e["path"], e.get("examples", [])[:4])
PY
started=$(date +%s)
"$bin/reuse_gate" crates/aft/examples/data/edit-pack-aft.jsonl.gz "$threads" > "$out/reuse.txt" 2> "$out/reuse.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "reuse rc=$rc in $(( $(date +%s) - started ))s"
tail -2 "$out/reuse.err"
grep -E " diff [0-9]+ [1-9]| diff [1-9]" "$out/reuse.txt" | cut -c1-300 | head -5
started=$(date +%s)
"$bin/extract_gate" repo . > "$out/extract.json" 2> "$out/extract.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "extract rc=$rc in $(( $(date +%s) - started ))s"
# The per-language counts, then any failure the gate names.
head -60 "$out/extract.err"
exit "$status"
