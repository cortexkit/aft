#!/usr/bin/env python3
"""Embeds AFT's own chunk texts and real aft_search queries with real models,
for the vector-plane gate (`vector_gate.rs`).

Nothing here is random: every vector is a model's output for a real text.

Chunks come from AFT's `export_chunks` example (`crates/aft/examples`), which
cuts a checkout into today's semantic chunks and writes each chunk's file,
exported flag and exact embedded text, one JSON object per line, in the order
today's index enumerates them.

Models:
  minilm    all-MiniLM-L6-v2, 384 dimensions, through
            `benchmarks/aft-search/minilm_embedder.py`, which reproduces AFT's
            default local backend step for step (pinned ONNX snapshot,
            tokenizer, mean pooling and normalization in f32).
  static    sentence-transformers/static-retrieval-mrl-en-v1, 1,024
            dimensions: a static model (the mean of token embeddings), fast
            enough on a CPU for every chunk. Not normalized.
  bge-base  BAAI/bge-base-en-v1.5, a 768-dimension transformer, through
            fastembed. Too slow on a CPU for every chunk, so it embeds an
            evenly spread sample (`--sample`), texts cut to 600 characters.

Queries are JSON files holding either a list of query strings or a
benchmark manifest (`rows[].query`); duplicates are dropped, first one kept.

Output, in `--out`:
  meta.json     model, dimensions, sources, and which chunks were embedded
  vectors.f32   little-endian f32 rows, one per embedded chunk, in chunk order
  queries.f32   little-endian f32 rows, one per query
  queries.json  the query strings, in row order

Usage:
  vector_gate_embed.py --chunks chunks.jsonl.gz --queries a.json [--queries b.json] \
      --model minilm|static|bge-base [--sample N] [--workers N] --out DIR
"""
from __future__ import annotations

import argparse
import gzip
import json
import os
import struct
import sys
import time
from multiprocessing import Pool
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
BENCH = ROOT / "benchmarks" / "aft-search"


def load_chunks(path: Path) -> list[dict]:
    with gzip.open(path, "rt", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def load_queries(paths: list[Path]) -> tuple[list[str], list[str]]:
    seen: set[str] = set()
    queries: list[str] = []
    sources: list[str] = []
    for path in paths:
        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data, dict):
            texts = [str(row["query"]) for row in data.get("rows", []) if "query" in row]
        else:
            texts = [str(text) for text in data]
        added = 0
        for text in texts:
            if text.strip() and text not in seen:
                seen.add(text)
                queries.append(text)
                added += 1
        sources.append(f"{path.name}: {added} new of {len(texts)}")
    return queries, sources


def write_rows(path: Path, rows) -> int:
    count = 0
    with path.open("wb") as handle:
        for row in rows:
            handle.write(struct.pack(f"<{len(row)}f", *row))
            count += 1
    return count


# MiniLM: AFT's local backend, one process per core with one ONNX thread each.
_EMBEDDER = None


def _minilm_init(snapshot: str) -> None:
    global _EMBEDDER
    sys.path.insert(0, str(BENCH))
    # The embedder sizes its ONNX thread pool from the core count; one thread
    # per worker process uses every core without oversubscribing. The
    # embedder's notes record that the thread count does not change vectors.
    os.cpu_count = lambda: 1
    from minilm_embedder import MiniLmEmbedder

    _EMBEDDER = MiniLmEmbedder(Path(snapshot))


def _minilm_embed(text: str) -> list[float]:
    return _EMBEDDER.embed([text])[0]


def minilm(texts: list[str], queries: list[str], workers: int) -> tuple[list, list, dict]:
    sys.path.insert(0, str(BENCH))
    import minilm_embedder
    from huggingface_hub import snapshot_download

    cache = Path(os.environ["FASTEMBED_CACHE_DIR"])
    snapshot_download(
        repo_id=minilm_embedder.MODEL_REPO,
        revision=minilm_embedder.MODEL_REVISION,
        cache_dir=str(cache),
        allow_patterns=["model.onnx", "tokenizer.json"],
    )
    snapshot = minilm_embedder.model_snapshot(cache)
    with Pool(workers, initializer=_minilm_init, initargs=(str(snapshot),)) as pool:
        corpus = pool.map(_minilm_embed, texts, chunksize=32)
        query_vectors = pool.map(_minilm_embed, queries, chunksize=8)
    info = {
        "name": minilm_embedder.MODEL_ID,
        "revision": minilm_embedder.MODEL_REVISION,
        "repo": minilm_embedder.MODEL_REPO,
        "dimensions": 384,
        "normalized": True,
        "how": "benchmarks/aft-search/minilm_embedder.py (AFT's local backend, reproduced)",
    }
    return corpus, query_vectors, info


def static(texts: list[str], queries: list[str]) -> tuple[list, list, dict]:
    import numpy as np
    from huggingface_hub import hf_hub_download, list_repo_files
    from safetensors.numpy import load_file
    from tokenizers import Tokenizer

    repo = "sentence-transformers/static-retrieval-mrl-en-v1"
    files = list_repo_files(repo)
    weights_name = next(f for f in files if f.endswith(".safetensors"))
    tokenizer_name = next(f for f in files if f.endswith("tokenizer.json"))
    weights = load_file(hf_hub_download(repo, weights_name))
    matrix = next(iter(weights.values())).astype(np.float32)
    tokenizer = Tokenizer.from_file(hf_hub_download(repo, tokenizer_name))
    tokenizer.no_truncation()

    def embed(batch_texts: list[str]) -> list:
        out = []
        for start in range(0, len(batch_texts), 512):
            encodings = tokenizer.encode_batch(batch_texts[start : start + 512], add_special_tokens=False)
            for encoding in encodings:
                ids = encoding.ids
                row = matrix[ids].mean(axis=0) if ids else np.zeros(matrix.shape[1], np.float32)
                out.append(row.astype(np.float32).tolist())
        return out

    info = {
        "name": repo,
        "revision": weights_name,
        "repo": repo,
        "dimensions": int(matrix.shape[1]),
        "normalized": False,
        "how": "mean of the model's token embeddings (a static model), no normalization",
    }
    return embed(texts), embed(queries), info


def bge_base(texts: list[str], queries: list[str], workers: int) -> tuple[list, list, dict]:
    from fastembed import TextEmbedding

    name = "BAAI/bge-base-en-v1.5"
    model = TextEmbedding(name, threads=workers)
    corpus = [list(map(float, v)) for v in model.passage_embed([t[:600] for t in texts], batch_size=32)]
    query_vectors = [list(map(float, v)) for v in model.query_embed(queries)]
    info = {
        "name": name,
        "revision": "fastembed default",
        "repo": name,
        "dimensions": len(corpus[0]),
        "normalized": True,
        "how": "fastembed passage_embed (texts cut to 600 characters) and query_embed",
    }
    return corpus, query_vectors, info


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--chunks", required=True, type=Path)
    parser.add_argument("--queries", required=True, type=Path, action="append")
    parser.add_argument("--model", required=True, choices=["minilm", "static", "bge-base"])
    parser.add_argument("--sample", type=int, default=0)
    parser.add_argument("--workers", type=int, default=os.cpu_count() or 1)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()

    chunks = load_chunks(args.chunks)
    indices = list(range(len(chunks)))
    if args.sample and args.sample < len(chunks):
        # Spread over the whole checkout rather than its first files.
        step = len(chunks) / args.sample
        indices = sorted({int(i * step) for i in range(args.sample)})
    texts = [chunks[i]["embed_text"] for i in indices]
    queries, query_sources = load_queries(args.queries)

    started = time.time()
    if args.model == "minilm":
        corpus, query_vectors, info = minilm(texts, queries, args.workers)
    elif args.model == "static":
        corpus, query_vectors, info = static(texts, queries)
    else:
        corpus, query_vectors, info = bge_base(texts, queries, args.workers)
    seconds = time.time() - started

    args.out.mkdir(parents=True, exist_ok=True)
    rows = write_rows(args.out / "vectors.f32", corpus)
    query_rows = write_rows(args.out / "queries.f32", query_vectors)
    (args.out / "queries.json").write_text(json.dumps(queries), encoding="utf-8")
    meta = {
        "model": info,
        "chunk_file": args.chunks.name,
        "chunks_total": len(chunks),
        "chunks_embedded": rows,
        "chunk_indices": indices if len(indices) != len(chunks) else None,
        "queries": query_rows,
        "query_sources": query_sources,
        "embed_seconds": round(seconds, 1),
    }
    (args.out / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
    summary = {key: value for key, value in meta.items() if key != "chunk_indices"}
    print(json.dumps(summary), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
