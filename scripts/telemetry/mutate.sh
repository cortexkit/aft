#!/usr/bin/env bash
# Run destructive resolver controls only in a disposable git worktree.
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
ref="${1:-HEAD}"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/aft-search-mutate.XXXXXX")"
cleanup() { git -C "$root" worktree remove --force "$scratch" >/dev/null 2>&1 || true; rm -rf "$scratch"; }
trap cleanup EXIT
git -C "$root" worktree add --detach "$scratch" "$ref" >/dev/null
spec=.cortexkit/alfonso/drafts/2026-09-09-aft-search-quality-b1-real-query-benchmark-manifest-gate-baseline-and-remeasure.md
sha=30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17
run_red() {
  local name="$1" output status applied
  applied="$(git -C "$scratch" diff --stat)"
  [[ -n "$applied" ]] || { printf '%s mutation produced empty diff\n' "$name" >&2; exit 1; }
  set +e
  output="$(cd "$scratch" && scripts/telemetry/imports-resolve.sh --spec "$spec" --snapshot "$sha" 2>&1)"
  status=$?
  set -e
  if [[ $status -ne 2 ]]; then printf '%s expected exit 2, got %s: %s\n' "$name" "$status" "$output" >&2; exit 1; fi
  printf '%s: applied=[%s] exit 2: %s\n' "$name" "$applied" "$output"
}
restore_clean() {
  git -C "$scratch" checkout -- "$1"
  local restored
  restored="$(git -C "$scratch" diff --stat)"
  [[ -z "$restored" ]] || { printf 'restore left diff: %s\n' "$restored" >&2; exit 1; }
  printf 'restored=[empty]\n'
}
printf '\n# NON-VACUITY BREAK: R99 production citation\n' >> "$scratch/benchmarks/aft-search/search_quality.py"
run_red unknown_production_citation
restore_clean benchmarks/aft-search/search_quality.py
python3 - "$scratch" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[1])/"benchmarks/aft-search/b-rulings.json"; data=json.loads(p.read_text()); data["NON-VACUITY BREAK"]="missing local definition"; data["definitions"]=[r for r in data["definitions"] if r["ruling"]!="R45"]; p.write_text(json.dumps(data,indent=2,sort_keys=True)+"\n")
PY
run_red missing_local_definition
restore_clean benchmarks/aft-search/b-rulings.json
python3 - "$scratch" <<'PY'
import hashlib,json,sys
from pathlib import Path
root=Path(sys.argv[1]); a=json.loads((root/"benchmarks/aft-search/campaign-a-ref.json").read_text()); p=root/"benchmarks/aft-search/campaign-b2-ref.json"; b=json.loads(p.read_text()); b["NON-VACUITY BREAK"]="duplicate imported classification"; b["definitions"].append(a["definitions"][0]); b["definition_index_sha256"]=hashlib.sha256(json.dumps(b["definitions"],sort_keys=True,separators=(",",":")).encode()).hexdigest(); p.write_text(json.dumps(b,indent=2,sort_keys=True)+"\n")
PY
run_red duplicate_imported_classification
restore_clean benchmarks/aft-search/campaign-b2-ref.json
