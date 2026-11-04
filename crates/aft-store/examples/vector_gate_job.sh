#!/usr/bin/env bash
# The vector-plane gate on real data, for a Linux build server with network
# access (models and Python packages are downloaded):
#
#   1. cut this checkout into today's semantic chunks with a copy of AFT's
#      own `export_chunks` example (see the build step);
#   2. embed every chunk, and the real aft_search query samples, with real
#      models (`vector_gate_embed.py`);
#   3. run `vector_gate` on each model's vectors, and on the benchmark's
#      MiniLM vector packs.
#
# Every AFT path runs with a throwaway HOME, XDG directories and storage
# directory; nothing reads or writes live AFT storage. Builds go to their own
# target directory so the shared warm build never holds these outputs.
#
# Usage, from the repository root:
#   bash crates/aft-store/examples/vector_gate_job.sh
# Environment: MODELS (default "minilm static bge-base"), BGE_SAMPLE (4000).
set -uo pipefail
cd "$(dirname "$0")/../../.."
git log -1 --oneline 2>/dev/null || true

OUT=target/vector-gate
# A build server may keep this folder from an earlier job; start clean so
# every report comes from this run.
rm -rf "$OUT"
mkdir -p "$OUT"
T=$(mktemp -d)
mkdir -p "$T/home" "$T/config" "$T/data" "$T/state" "$T/cache" "$T/storage"
export HOME="$T/home" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" \
  XDG_STATE_HOME="$T/state" XDG_CACHE_HOME="$T/cache" AFT_STORAGE_DIR="$T/storage" \
  HF_HOME="$T/hf" FASTEMBED_CACHE_DIR="$T/models" FASTEMBED_CACHE_PATH="$T/fastembed"
TD="${VECTOR_GATE_TARGET:-${TMPDIR:-/tmp}/vec-target}"
MODELS="${MODELS:-minilm static bge-base}"
stage() { echo "== $1 ($(date -u +%H:%M:%S))"; }

stage "build"
# The checked-in `export_chunks` example is behind the index format it reads:
# it does not decode SymbolKind::Kernel (byte 10), so it stops at the first
# CUDA kernel chunk, and it treats the skipped-rows trailer the index now
# writes after its entries as trailing garbage. The job builds a copy that
# decodes the kind and allows the trailer, as a temporary example removed
# when the job ends; the copy differs from the original only in those lines.
EXPORTER=crates/aft/examples/vector_gate_export_chunks.rs
trap 'rm -f "$EXPORTER"' EXIT
sed -e 's/        9 => Ok(SymbolKind::FileSummary),/        9 => Ok(SymbolKind::FileSummary),\n        10 => Ok(SymbolKind::Kernel),/' \
  -e 's/    if cursor.position() != bytes.len() as u64 {/    if cursor.position() > bytes.len() as u64 {/' \
  crates/aft/examples/export_chunks.rs >"$EXPORTER"
grep -q 'SymbolKind::Kernel' "$EXPORTER" && grep -q 'cursor.position() > bytes.len()' "$EXPORTER" ||
  { echo "could not patch the exporter copy"; exit 1; }
if ! cargo build --profile stage -p agent-file-tools --example vector_gate_export_chunks \
  --target-dir "$TD" >"$OUT/build-aft.log" 2>&1; then
  echo "export_chunks build failed"; tail -30 "$OUT/build-aft.log"; exit 1
fi
if ! cargo build --release -p aft-store --example vector_gate --target-dir "$TD" \
  >"$OUT/build-gate.log" 2>&1; then
  echo "vector_gate build failed"; tail -30 "$OUT/build-gate.log"; exit 1
fi
GATE="$TD/release/examples/vector_gate"

stage "chunks"
if ! "$TD/stage/examples/vector_gate_export_chunks" . "$T/chunks-aft.jsonl" >"$OUT/export.log" 2>&1; then
  echo "chunk export failed"; tail -5 "$OUT/export.log"; exit 1
fi
tail -1 "$OUT/export.log"
gzip -1 "$T/chunks-aft.jsonl"
CHUNKS="$T/chunks-aft.jsonl.gz"

stage "pack gate"
/usr/bin/time -v "$GATE" pack "$T/store-pack" benchmarks/aft-search/real-query-vectors.bin \
  benchmarks/aft-search/concept-query-vectors.bin benchmarks/aft-search/split-query-vectors.bin \
  >"$OUT/gate-pack.json" 2>"$OUT/gate-pack.log"
echo "gate pack exit=$?"
grep -E "Maximum resident|Elapsed|DIFFERENCE|panicked" "$OUT/gate-pack.log" | head -20

stage "python"
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR="$T/uv" UV_NO_MODIFY_PATH=1 sh \
    >"$OUT/uv.log" 2>&1
  export PATH="$T/uv:$PATH"
fi
# MiniLM runs AFT's own runtime versions; the other models need fastembed,
# whose pins could conflict with those, so each gets its own environment.
uv venv -q -p 3.12 "$T/venv-minilm" >>"$OUT/uv.log" 2>&1
uv pip install -q --python "$T/venv-minilm/bin/python" numpy onnxruntime==1.24.4 tokenizers==0.22.2 \
  huggingface_hub >"$OUT/pip-minilm.log" 2>&1 || { echo "pip (minilm) failed"; tail -20 "$OUT/pip-minilm.log"; }
uv venv -q -p 3.12 "$T/venv-other" >>"$OUT/uv.log" 2>&1
uv pip install -q --python "$T/venv-other/bin/python" numpy fastembed safetensors tokenizers \
  huggingface_hub >"$OUT/pip-other.log" 2>&1 || { echo "pip (other) failed"; tail -20 "$OUT/pip-other.log"; }

QUERIES=(--queries crates/aft-store/examples/data/vector-gate-queries-sample200.json
  --queries benchmarks/aft-search/real-query-manifest.json)
for model in $MODELS; do
  stage "embed $model"
  py="$T/venv-other/bin/python"
  extra=()
  [ "$model" = minilm ] && py="$T/venv-minilm/bin/python"
  [ "$model" = bge-base ] && extra=(--sample "${BGE_SAMPLE:-4000}")
  timeout 3000 "$py" crates/aft-store/examples/vector_gate_embed.py --chunks "$CHUNKS" "${QUERIES[@]}" \
    --model "$model" "${extra[@]}" --workers "$(nproc)" --out "$T/emb-$model" >"$OUT/embed-$model.log" 2>&1
  echo "embed $model exit=$?"
  tail -3 "$OUT/embed-$model.log"
  stage "gate $model"
  /usr/bin/time -v "$GATE" dir "$T/store-$model" "$T/emb-$model" "$CHUNKS" \
    >"$OUT/gate-$model.json" 2>"$OUT/gate-$model.log"
  echo "gate $model exit=$?"
  grep -E "Maximum resident|Elapsed|DIFFERENCE|panicked" "$OUT/gate-$model.log" | head -20
done
stage "done"
ls -la "$OUT"
