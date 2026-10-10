#!/usr/bin/env python3
"""Embed fixture query texts the main pack lacks, preserving real-query-vectors.bin.

The gate pack (split-query-vectors.bin) holds the prose and joined texts of
the gate's query/pattern rows, and the query text of every gate row the main
pack has no vector for. The tuning pack (split-tuning-vectors.bin,
--tuning-only) holds the tuning rows' texts. Each run rebinds only its own
manifest, and vectors already in the pack keep their exact bytes.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from embedding_fixture_server import query_key
from minilm_embedder import MiniLmEmbedder
from run_real_query import ROOT, HERE
from search_quality_lib import canonical_json, sha256_file
from vector_pack import read_pack, write_pack


def without_surrounding_quotes(query: str) -> str:
    """The text AFT searches for a quoted code literal: one matched pair of
    surrounding quotes or backticks removed, as `strip_surrounding_quotes` in
    crates/aft/src/commands/semantic_search/mod.rs does."""
    trimmed = query.strip()
    if len(trimmed) >= 2 and trimmed[0] in "\"'`" and trimmed[0] == trimmed[-1]:
        return trimmed[1:-1]
    return query


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-vector-authoring", action="store_true", required=True)
    parser.add_argument(
        "--tuning-only",
        action="store_true",
        help="write split-tuning-vectors.bin for the tuning manifest alone, leaving the gate's pack and manifest untouched",
    )
    args = parser.parse_args()
    if not args.allow_vector_authoring:
        parser.error("authoring must be explicit")
    path = HERE / "real-query-manifest.json"
    output_name = "split-query-vectors.bin"
    if args.tuning_only:
        path = HERE / "split-tuning-manifest.json"
        output_name = "split-tuning-vectors.bin"
    manifest = json.loads(path.read_text())
    pack = read_pack(HERE / "real-query-vectors.bin")
    template = pack["embed_template_version"]
    rows = [row for row in manifest["rows"] if "excluded_reason" not in row]
    texts = {row["query"] for row in rows if "pattern" in row}
    texts.update(row["query"] + " " + row["pattern"] for row in rows if "pattern" in row and row["pattern"].strip())
    if not args.tuning_only:
        # A query-only row whose shape runs no semantic lane on the recording
        # engine (an identifier, a code literal) never had its text embedded,
        # so the main pack has no vector for it. Store one here, so an engine
        # that routes the same text to the semantic lane is measured instead
        # of faulting with vector_missing. A quoted code literal is searched
        # without its quotes, so that form is stored too.
        for row in rows:
            if "pattern" in row:
                continue
            for text in (row["query"], without_surrounding_quotes(row["query"])):
                if query_key(text, template) not in pack["vectors"]:
                    texts.add(text)
    output = HERE / output_name
    # Vectors already in the pack keep their exact bytes, including those of
    # texts no current row names (the first tuning rows' vectors live in the
    # gate pack); only texts the pack lacks are embedded.
    previous = read_pack(output) if output.is_file() else None
    vectors = {key: previous["vectors"][key] for key in previous["vectors"]} if previous else {}
    missing = {query_key(text, template): text for text in sorted(texts)}
    missing = {key: text for key, text in missing.items() if key not in vectors}
    embedder = MiniLmEmbedder() if missing else None
    for key, text in missing.items():
        vectors[key] = embedder.embed([text])[0]
    metadata = {key: value for key, value in pack.items() if key not in {"vectors", "count", "schema", "dtype"}}
    if embedder is not None:
        metadata["source"] = embedder.source
    elif previous is not None:
        metadata["source"] = previous["source"]
    write_pack(output, metadata, vectors)
    binding = {"path": output.relative_to(ROOT).as_posix(), "sha256": sha256_file(output)}
    manifest["split_query_pack"] = binding
    path.write_bytes(canonical_json(manifest))
    print(f"split_query_vectors:{len(vectors)} embedded:{len(missing)} sha256:{binding['sha256']}")


if __name__ == "__main__":
    main()
