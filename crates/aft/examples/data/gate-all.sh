#!/usr/bin/env bash
# Builds the data-layer gates once and runs them in turn:
#   - parity: this checkout's resolution against today's join and database;
#   - edit: the 430-edit pack through the incremental engine;
#   - reuse: the pack's commit ends resolved from stored resolutions;
#   - extraction: every file Git tracks in this checkout;
#   - checkout: the pack through two checkouts of one store, one through its
#     handle and one through the runtime, each compared file by file with
#     cold builds;
#   - grep/glob: a copy of this checkout answered by today's path and by the
#     data layer, reply by reply;
#   - inspect: aft_inspect's dead code with and without the data layer.
# Exits nonzero if any gate fails, after running them all. A gate that
# compared nothing fails too.
#
#   bash crates/aft/examples/data/gate-all.sh [sample scale] [threads]
#
# A sample scale above 1 samples the parity gate's queries more sparsely,
# and AFT_GATE_LIMIT keeps the grep gate's first N grep cases, for
# iterating; a full run sets neither.
set -uo pipefail
scale="${1:-1}"
threads="${2:-8}"
out="${TMPDIR:-/tmp}/gate-all"
mkdir -p "$out"
# Settings that make a gate check less than all of it, or nothing at all,
# while still exiting 0.
unset EDIT_GATE_TIMING_ONLY EDIT_GATE_STOP_AT_FIRST AFT_GATE_ALLOW_DIFFS AFT_GATE_CASES \
  CHECKOUT_GATE_ONLY CHECKOUT_GATE_PUBLISH_EDITS
git log -1 --format=%H
# The grep and glob gate is a library test that needs debug assertions (see
# its module documentation), built in a profile of its own.
gate_profile=(--config 'profile.gate.inherits="release"' --config 'profile.gate.lto=false'
  --config 'profile.gate.codegen-units=16' --config 'profile.gate.debug-assertions=true'
  --config 'profile.gate.strip=false' --config 'profile.gate.debug=0'
  --config 'profile.gate.split-debuginfo="off"' --profile gate)
# A failed build must stop here: the target folder may be shared and warm, so
# carrying on would run an older commit's gate binaries.
if ! cargo build --profile ci-storm -p agent-file-tools --bin aft \
  --example resolve_gate --example edit_gate --example reuse_gate \
  --example extract_gate --example checkout_gate > "$out/build.txt" 2>&1 \
  || ! cargo test "${gate_profile[@]}" -p agent-file-tools --lib --no-run >> "$out/build.txt" 2>&1; then
  tail -30 "$out/build.txt"
  echo "build failed; no gate ran"
  exit 2
fi
grep -E "^ *Finished|Executable" "$out/build.txt" | tail -4
target="${CARGO_TARGET_DIR:-target}"
bin="$target/ci-storm/examples"
binaries=("$bin/resolve_gate" "$bin/edit_gate" "$bin/reuse_gate" "$bin/extract_gate" "$bin/checkout_gate" "$target/ci-storm/aft")
ls -l --time-style=+%FT%T "${binaries[@]}" 2>/dev/null || ls -lT "${binaries[@]}"
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
started=$(date +%s)
AFT_DATA_LAYER=1 AFT_GATE_ROOT="$PWD" AFT_GATE_REPORT="$out/grep.json" \
  cargo test "${gate_profile[@]}" -p agent-file-tools --lib -- --ignored --nocapture \
  --exact data_layer::tools::grep::gate::grep_and_glob_answer_alike_from_the_data_layer \
  > "$out/grep.txt" 2> "$out/grep.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "grep/glob rc=$rc in $(( $(date +%s) - started ))s"
grep -E "test result|panicked" -A3 "$out/grep.txt" "$out/grep.err" | head -8
# The report holds every replay; a missing report (the test filter matched
# nothing, so nothing ran) or a replay with no case fails the stage.
python3 - "$out/grep.json" <<'PY' || status=1
import json, sys
try:
    report = json.load(open(sys.argv[1]))
except OSError as error:
    print("no grep/glob report, so the gate did not run:", error)
    sys.exit(1)
replays = report["summary"]["replays"]
for name, replay in replays.items():
    print(" ", name, {key: replay[key] for key in ("cases", "identical", "different", "by_class", "data_layer_answered")})
for difference in [d for d in report["differences"] if d["class"] not in (
        "stale path disclosed only when it is a candidate",
        "today's alternation filter merge drops candidates",
        "capped count is a floor that depends on the files read")][:3]:
    print("  unexplained:", difference["class"], difference["params"], difference["first_difference"])
sys.exit(1 if not replays or any(replay["cases"] == 0 for replay in replays.values()) else 0)
PY
started=$(date +%s)
python3 crates/aft/examples/data/inspect_gate.py "$target/ci-storm/aft" "$out/inspect" --root . --control \
  > "$out/inspect.txt" 2> "$out/inspect.err"
rc=$?; [ "$rc" -eq 0 ] || status=1
echo "inspect rc=$rc in $(( $(date +%s) - started ))s"
tail -5 "$out/inspect.txt"
tail -3 "$out/inspect.err"
exit "$status"
