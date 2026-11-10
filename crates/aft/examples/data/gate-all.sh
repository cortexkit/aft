#!/usr/bin/env bash
# Builds the index store's gates once and runs them in turn:
#   - edit: the 430-edit pack through the incremental engine, compared with
#     cold resolutions;
#   - reuse: the pack's commit ends resolved from stored resolutions;
#   - extraction: every file Git tracks in this checkout;
#   - checkout: the pack through two checkouts of one store, one through its
#     handle and one through the runtime, each compared file by file with
#     cold builds.
# The gates that compared the index store with the indexes it replaced
# (call-graph parity, grep and glob, dead code) went with those indexes.
# Exits nonzero if any gate fails, after running them all. A gate that
# compared nothing fails too.
#
#   bash crates/aft/examples/data/gate-all.sh [threads]
#
set -uo pipefail

threads="${1:-8}"
out="${TMPDIR:-/tmp}/gate-all"
mkdir -p "$out"
# Settings that make a gate check less than all of it, or nothing at all,
# while still exiting 0.
unset EDIT_GATE_TIMING_ONLY EDIT_GATE_STOP_AT_FIRST AFT_GATE_ALLOW_DIFFS AFT_GATE_CASES \
  CHECKOUT_GATE_ONLY CHECKOUT_GATE_PUBLISH_EDITS
git log -1 --format=%H
# A failed build must stop here: the target folder may be shared and warm, so
# carrying on would run an older commit's gate binaries.
if ! cargo build --profile ci-storm -p agent-file-tools \
  --example edit_gate --example reuse_gate \
  --example extract_gate --example checkout_gate > "$out/build.txt" 2>&1; then
  tail -30 "$out/build.txt"
  echo "build failed; no gate ran"
  exit 2
fi
grep -E "^ *Finished|Executable" "$out/build.txt" | tail -4
target="${CARGO_TARGET_DIR:-target}"
bin="$target/ci-storm/examples"
binaries=("$bin/edit_gate" "$bin/reuse_gate" "$bin/extract_gate" "$bin/checkout_gate")
ls -l --time-style=+%FT%T "${binaries[@]}" 2>/dev/null || ls -lT "${binaries[@]}"
status=0
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
started=$(date +%s)
"$bin/checkout_gate" crates/aft/examples/data/edit-pack-aft.jsonl.gz "$threads" > "$out/checkout.jsonl" 2> "$out/checkout.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "checkout rc=$rc in $(( $(date +%s) - started ))s"
grep -E "^compared |panicked" -A2 "$out/checkout.err" | head -6
# Each comparison's differences; no comparison, or one that compared no
# file, fails the stage even if the gate exited 0.
python3 - "$out/checkout.jsonl" <<'PY' || status=1
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
engine_keys = ("differing_inputs", "differing_answers", "differing_recordings",
               "differing_fragments", "stale_recordings")
def bad(row):
    engine = row.get("engine") or {}
    return (row["differing_primitives"] or row["missed_text"] or row["not_ready"] or row.get("failure")
            or (row.get("stored_inputs_missing") or 0) or not engine.get("files")
            or any(engine.get(key) for key in engine_keys))
failed = [row for row in rows if bad(row)]
files = [(row.get("engine") or {}).get("files", 0) for row in rows]
print("comparisons", len(rows), "with differences", len(failed),
      "files compared per comparison min/max", min(files, default=0), max(files, default=0))
for row in failed[:5]:
    print(" ", row["commit"], row["checkout"], row["serving"], row["events"][:3], row.get("engine"),
          row["examples"][:6])
sys.exit(1 if not rows or min(files) == 0 else 0)
PY
exit "$status"
