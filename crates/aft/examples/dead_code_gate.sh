#!/usr/bin/env bash
# Dead-code gate on the build server. Each project is run twice: by the gate
# built from an earlier commit that still has the string rollup (`old` mode,
# the oracle), and by this checkout's gate in `compare` mode, which also
# checks stored facts against parsed ones and the file-by-file call graph
# against a snapshot of every call row. Both dump their aggregates; the
# dumps must be byte-identical. Projects: the store-diet corpus, AFT's own
# repository (a copy, since the gate edits files), and with network, the
# repositories named after the baseline.
#   dead_code_gate.sh <out-dir> <baseline-commit> [git-url...]
# The baseline may also be a `.tar.gz` of that commit's tree (from
# `git archive --format=tar.gz <commit>`), for a checkout whose history no
# longer reaches the commit.
# A baseline of `-` skips the earlier build: each project then runs only
# this checkout's gate, which still checks every patched rollup against one
# worked out from nothing.
set -u
OUT=$1
BASE=$2
shift 2
mkdir -p "$OUT"
BIN=${CARGO_TARGET_DIR:-target}/ci-storm/examples/dead_code_gate
BASE_BIN=$TMPDIR/base-target/ci-storm/examples/dead_code_gate
git log -1 --oneline
cargo build --profile ci-storm -p agent-file-tools --example dead_code_gate 2>&1 | grep -E "^error|Finished" | head -20
if [ "$BASE" != - ]; then
  rm -rf "$TMPDIR/base" && mkdir -p "$TMPDIR/base"
  if [ -f "$BASE" ]; then
    tar xzf "$BASE" -C "$TMPDIR/base"
  else
    git archive "$BASE" | (cd "$TMPDIR/base" && tar xf -)
  fi
  (cd "$TMPDIR/base" && CARGO_TARGET_DIR="$TMPDIR/base-target" cargo build --profile ci-storm -p agent-file-tools --example dead_code_gate 2>&1 | grep -E "^error|Finished" | head -20)
fi

# Runs both gates on directory $2 (named $1) with $3 edits and compares
# their dumps.
gate() {
  local name=$1 dir=$2 edits=$3
  mkdir -p "$OUT/dump-base/$name" "$OUT/dump-new/$name"
  local base_exit=-
  if [ "$BASE" != - ]; then
    DEAD_CODE_GATE_DUMP="$OUT/dump-base/$name" /usr/bin/time -v "$BASE_BIN" "$dir" "$TMPDIR/st-base-$name" old "$edits" > "$OUT/$name-base.jsonl" 2> "$OUT/$name-base.err"
    base_exit=$?
  fi
  DEAD_CODE_GATE_DUMP="$OUT/dump-new/$name" /usr/bin/time -v "$BIN" "$dir" "$TMPDIR/st-new-$name" compare "$edits" > "$OUT/$name-new.jsonl" 2> "$OUT/$name-new.err"
  local new_exit=$?
  local verdict=""
  for round in first "after edits"; do
    [ -f "$OUT/dump-base/$name/$round-old.json" ] || continue
    for side in new kept full rows; do
      [ -f "$OUT/dump-new/$name/$round-$side.json" ] || continue
      if cmp -s "$OUT/dump-base/$name/$round-old.json" "$OUT/dump-new/$name/$round-$side.json"; then
        verdict="$verdict $round/$side=same"
      else
        verdict="$verdict $round/$side=DIFFERS"
      fi
    done
  done
  echo "$name base_exit=$base_exit new_exit=$new_exit$verdict $(grep -h 'compared' "$OUT/$name-new.err" | tr -d '\n') rounds_differing=$(grep -c '"round".*"identical":false' "$OUT/$name-new.jsonl") facts_same=$(grep -c '"facts":.*"identical":true' "$OUT/$name-new.jsonl") fallbacks=$(grep -o '"files parsed for want of stored facts":[0-9]*' "$OUT/$name-new.jsonl" | cut -d: -f2 | tr '\n' ',') $(grep -o '"count":[0-9]*,"total_count":[0-9]*' "$OUT/$name-new.jsonl" | head -1)"
}

for d in crates/aft/tests/store_diet/corpus/*/; do
  n=$(basename "$d")
  rm -rf "$TMPDIR/c-$n"; cp -r "$d" "$TMPDIR/c-$n"
  gate "$n" "$TMPDIR/c-$n" 1
done
# A project with a symlinked file and a symlinked folder. The rollup
# resolves regular files' paths without asking the file system, and must
# answer as the baseline, which canonicalizes every path, does.
S=$TMPDIR/symlinks
rm -rf "$S" "$TMPDIR/symlinks-outside" && mkdir -p "$S/src/real" "$TMPDIR/symlinks-outside"
printf '[package]\nname = "symlinks"\nversion = "0.1.0"\nedition = "2021"\n' > "$S/Cargo.toml"
printf 'mod real;\nmod shared;\npub fn api() { real::inner(); shared::shared(); }\n' > "$S/src/lib.rs"
printf 'pub fn inner() {}\npub fn unused_inner() {}\n' > "$S/src/real/mod.rs"
printf 'pub fn shared() {}\npub fn unused_shared() {}\n' > "$TMPDIR/symlinks-outside/shared.rs"
printf 'export function a() {}\nexport function unusedA() {}\n' > "$S/src/a.ts"
ln -s real "$S/src/linked"
ln -s "$TMPDIR/symlinks-outside/shared.rs" "$S/src/shared.rs"
ln -s a.ts "$S/src/b.ts"
gate symlinks "$S" 1
rm -rf "$TMPDIR/aft" && mkdir -p "$TMPDIR/aft"
tar --exclude=./target -cf - . | (cd "$TMPDIR/aft" && tar xf -)
gate aft "$TMPDIR/aft" 3
for url in "$@"; do
  n=$(basename "$url")
  rm -rf "$TMPDIR/r-$n"
  git clone -q --depth 1 "$url" "$TMPDIR/r-$n" && (cd "$TMPDIR/r-$n" && git log -1 --format="$n %H %cs")
  gate "$n" "$TMPDIR/r-$n" 3
done
# What `aft_inspect` spends alone: this checkout's gate in `new` mode.
for name in aft "$@"; do
  name=$(basename "$name")
  dir=$TMPDIR/aft
  [ "$name" = aft ] || dir=$TMPDIR/r-$name
  DEAD_CODE_GATE_UNUSED_EXPORTS=1 /usr/bin/time -v "$BIN" "$dir" "$TMPDIR/st-alone-$name" new 3 > "$OUT/$name-alone.jsonl" 2> "$OUT/$name-alone.err"
  echo "$name alone exit=$?"
  grep -v '"edited"' "$OUT/$name-alone.jsonl" | cut -c1-600
  grep -E "Maximum resident|Elapsed" "$OUT/$name-alone.err"
done
for name in aft "$@"; do
  name=$(basename "$name")
  echo "$name base (old mode):"
  grep -E '"phase"' "$OUT/$name-base.jsonl" | cut -c1-200
  grep -E "Maximum resident|Elapsed" "$OUT/$name-base.err"
done
cd "$OUT/.." && tar czf "$(basename "$OUT").tgz" "$(basename "$OUT")" && ls -la "$(basename "$OUT").tgz"
