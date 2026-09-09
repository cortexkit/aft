#!/usr/bin/env python3
"""Author the allowlisted vector pack from AFT's actual pinned-tree chunks."""
from __future__ import annotations

import argparse
import json
import sys
import tempfile
import threading
from pathlib import Path

from embedding_fixture_server import Server
from run_real_query import DEFAULT_BINARY, NdjsonClient, materialized_bundle
from search_quality_lib import EVIDENCE_SHA, InputFault, canonical_json, sha256_file

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    result.add_argument("--binary", default=DEFAULT_BINARY)
    result.add_argument("--output", default=str(HERE / "real-query-vectors.json"))
    result.add_argument("--ready-timeout", type=float, default=600.0)
    result.add_argument("--allow-vector-authoring", action="store_true", required=True)
    return result


def run(args: argparse.Namespace) -> int:
    manifest_path = Path(args.manifest).resolve()
    manifest = json.loads(manifest_path.read_text())
    rows = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    if manifest.get("evidence_sha") != EVIDENCE_SHA or not rows:
        raise InputFault("corpus_vector_model_mismatch:manifest")
    bundle_paths = {str(row["bundle"]) for row in rows}
    if len(bundle_paths) != 1:
        raise InputFault("corpus_vector_model_mismatch:bundle")
    bundle = ROOT / next(iter(bundle_paths))
    output = Path(args.output).resolve()
    current = json.loads(output.read_text()) if output.is_file() else {}
    vectors = dict(current.get("vectors", {}))
    template = str(current.get("embed_template_version", "aft-search-template-v1"))
    model_id = str(current.get("model_id", "aft-search-fixture-v1"))
    query_texts = {str(row["query"]) for row in rows}
    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise InputFault(f"aft_binary_missing:{binary}")

    with tempfile.TemporaryDirectory(prefix="aft-vector-authoring-") as directory, materialized_bundle(bundle) as project_root:
        runtime = Path(directory)
        server = Server(
            ("127.0.0.1", 0),
            vectors,
            template,
            runtime / "requests.log",
            record_missing=True,
            query_texts=query_texts,
        )
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        client = NdjsonClient(binary, project_root, runtime / "storage", runtime / "aft.stderr")
        try:
            endpoint = f"http://127.0.0.1:{server.server_port}"
            client.configure(endpoint, model_id, args.ready_timeout)
            client.wait_ready(args.ready_timeout)
            for row in rows:
                client.search({"query": row["query"], "topK": 1, "includeTests": row["include_tests"]})
        finally:
            client.close()
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    pack = {
        "schema": "aft-search-vector-pack-v1",
        "pinned_sha": EVIDENCE_SHA,
        "embed_template_version": template,
        "model_id": model_id,
        "vectors": vectors,
    }
    output.write_bytes(canonical_json(pack))
    digest = sha256_file(output)
    updated = 0
    for row in rows:
        if (ROOT / str(row["embedding_pack"])).resolve() == output:
            row["embedding_pack_sha256"] = digest
            updated += 1
    if updated != len(rows):
        raise InputFault("corpus_vector_model_mismatch:embedding_pack_path")
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
