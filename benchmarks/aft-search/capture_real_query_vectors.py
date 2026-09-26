#!/usr/bin/env python3
"""Capture the real-query vector pack from AFT's actual pinned-tree chunks.

AFT indexes the pinned evidence tree against the fixture server, and every
text it asks to embed (each chunk, each real-query row, the index probe) is
embedded with the real all-MiniLM-L6-v2 model through `minilm_embedder`, which
reproduces AFT's default local backend. The result is written as a float16
pack and bound into every manifest row.

The pack is rebuilt from nothing on each capture, so no text can keep a vector
from an older chunk format. This is an authoring command: it needs the model
cache and onnxruntime/tokenizers/numpy, none of which the gate uses. Run it as

    uv run --with onnxruntime==1.24.4 --with tokenizers==0.22.2 --with numpy \\
      python3 capture_real_query_vectors.py --allow-vector-authoring
"""
from __future__ import annotations

import argparse
import json
import sys
import tempfile
import threading
from pathlib import Path

from embedding_fixture_server import Server
from minilm_embedder import MODEL_ID, MiniLmEmbedder
from run_real_query import (
    DEFAULT_BINARY,
    FIXTURE_PROVIDER_MODEL,
    NdjsonClient,
    load_manifest_and_tree,
    runtime_evidence_tree,
)
from search_quality_lib import EVIDENCE_SHA, InputFault, canonical_json, sha256_file
from vector_pack import write_pack

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
TEMPLATE = "aft-search-template-v1"


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    result.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--output", default=str(HERE / "real-query-vectors.bin"))
    result.add_argument("--ready-timeout", type=float, default=7200.0)
    result.add_argument("--allow-vector-authoring", action="store_true", required=True)
    return result


def run(args: argparse.Namespace) -> int:
    manifest_path = Path(args.manifest).resolve()
    manifest, provisioned_tree, _, _ = load_manifest_and_tree(manifest_path)
    rows = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    output = Path(args.output).resolve()
    query_texts = {str(row["query"]) for row in rows}
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise InputFault(f"aft_binary_missing:{binary}")
    embedder = MiniLmEmbedder()
    vectors: dict[str, list[float]] = {}

    with tempfile.TemporaryDirectory(prefix="aft-vector-authoring-") as directory, runtime_evidence_tree(provisioned_tree) as project_root:
        runtime = Path(directory)
        server = Server(
            ("127.0.0.1", 0),
            vectors,
            TEMPLATE,
            runtime / "requests.log",
            recorder=lambda text: embedder.embed([text])[0],
            query_texts=query_texts,
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        client = NdjsonClient(binary, project_root, runtime / "storage", runtime / "aft.stderr")
        try:
            endpoint = f"http://127.0.0.1:{server.server_port}"
            client.configure(endpoint, FIXTURE_PROVIDER_MODEL, args.ready_timeout)
            client.wait_ready(args.ready_timeout)
            for row in rows:
                client.search({"query": row["query"], "topK": 1, "includeTests": row["include_tests"]})
        finally:
            client.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    header = {
        "pinned_sha": EVIDENCE_SHA,
        "embed_template_version": TEMPLATE,
        "model_id": MODEL_ID,
        "source": embedder.source,
    }
    write_pack(output, header, vectors)
    digest = sha256_file(output)
    updated = 0
    for row in rows:
        row["embedding_pack"] = output.relative_to(ROOT).as_posix()
        row["embedding_pack_sha256"] = digest
        updated += 1
    manifest_path.write_bytes(canonical_json(manifest))
    print(f"captured_vectors:{len(vectors)}")
    print(f"captured_vector_pack:{output}")
    print(f"captured_vector_pack_sha256:{digest}")
    print(f"captured_manifest_rows:{updated}")
    return 0


def main() -> int:
    try:
        return run(parser().parse_args())
    except (InputFault, OSError, KeyError, ValueError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
