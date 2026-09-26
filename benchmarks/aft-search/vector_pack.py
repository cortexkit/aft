#!/usr/bin/env python3
"""Compact binary vector pack for the real-query gate's embedding fixture.

The gate serves AFT's semantic lane from stored vectors instead of running a
model, so every run sees the same numbers on every machine. The vectors are the
real all-MiniLM-L6-v2 output for each text AFT embeds on the pinned tree,
captured once by `capture_real_query_vectors.py`. At 384 dimensions a JSON pack
would be around 100 MB, so the vectors are stored as little-endian float16 and
indexed by the SHA-256 of the embedded text.

File layout:

    8 bytes   magic b"AFTVECP2"
    4 bytes   header length N, little-endian u32
    N bytes   canonical JSON header (schema, pin, template, model, dimension, count)
    count x 33 bytes   index: kind byte (0 corpus, 1 query) + 32-byte text digest,
                       sorted, one per vector
    count x dimension x 2 bytes   float16 vectors in index order

float16 is not a lossy shortcut for this gate: replaying the paged profile with
the float16 pack gave rows byte-identical to the float32 vectors and to a live
local-model run. Decoding needs only the standard library, so CI does not need
numpy or an ONNX runtime.
"""
from __future__ import annotations

import json
import struct
from pathlib import Path
from typing import Any, Iterator, Mapping, Optional, Sequence

from search_quality_lib import InputFault, canonical_json

MAGIC = b"AFTVECP2"
SCHEMA = "aft-search-vector-pack-v2"
DTYPE = "float16"
KIND_CORPUS = 0
KIND_QUERY = 1
DIGEST_BYTES = 32
INDEX_RECORD_BYTES = 1 + DIGEST_BYTES


def parse_key(key: str, pinned_sha: str, template: str) -> tuple[int, bytes]:
    """Split a fixture-server key into its kind byte and text digest.

    Keys are `query:<sha256>:<template>` and
    `corpus:<pinned sha>:<sha256>:<template>`, the same strings the fixture
    server logs. A key for another pin or template cannot belong to this pack.
    """
    parts = key.split(":")
    if len(parts) == 3 and parts[0] == "query" and parts[2] == template:
        kind, digest = KIND_QUERY, parts[1]
    elif len(parts) == 4 and parts[0] == "corpus" and parts[1] == pinned_sha and parts[3] == template:
        kind, digest = KIND_CORPUS, parts[2]
    else:
        raise KeyError(key)
    try:
        raw = bytes.fromhex(digest)
    except ValueError:
        raise KeyError(key) from None
    if len(raw) != DIGEST_BYTES:
        raise KeyError(key)
    return kind, raw


def format_key(kind: int, digest: bytes, pinned_sha: str, template: str) -> str:
    if kind == KIND_QUERY:
        return f"query:{digest.hex()}:{template}"
    return f"corpus:{pinned_sha}:{digest.hex()}:{template}"


class VectorPack(Mapping[str, list[float]]):
    """Read-only key -> vector mapping decoded lazily from the pack bytes."""

    def __init__(self, data: bytes, offset: int, count: int, dimension: int, pinned_sha: str, template: str):
        self._data = data
        self._dimension = dimension
        self._pinned_sha = pinned_sha
        self._template = template
        self._vector_format = struct.Struct(f"<{dimension}e")
        self._vectors_offset = offset + count * INDEX_RECORD_BYTES
        self._rows: dict[tuple[int, bytes], int] = {}
        for row in range(count):
            start = offset + row * INDEX_RECORD_BYTES
            kind = data[start]
            if kind not in (KIND_CORPUS, KIND_QUERY):
                raise InputFault("corpus_vector_model_mismatch:embedding_pack_index")
            self._rows[(kind, data[start + 1 : start + INDEX_RECORD_BYTES])] = row
        if len(self._rows) != count:
            raise InputFault("corpus_vector_model_mismatch:embedding_pack_duplicate_key")
        if len(data) != self._vectors_offset + count * self._vector_format.size:
            raise InputFault("corpus_vector_model_mismatch:embedding_pack_length")

    def _row(self, key: object) -> Optional[int]:
        if not isinstance(key, str):
            return None
        try:
            return self._rows.get(parse_key(key, self._pinned_sha, self._template))
        except KeyError:
            return None

    def __contains__(self, key: object) -> bool:
        return self._row(key) is not None

    def __getitem__(self, key: str) -> list[float]:
        row = self._row(key)
        if row is None:
            raise KeyError(key)
        start = self._vectors_offset + row * self._vector_format.size
        return list(self._vector_format.unpack_from(self._data, start))

    def __iter__(self) -> Iterator[str]:
        for kind, digest in self._rows:
            yield format_key(kind, digest, self._pinned_sha, self._template)

    def __len__(self) -> int:
        return len(self._rows)


def read_pack(path: Path) -> dict[str, Any]:
    """Load a pack as its header fields plus a `vectors` mapping."""
    data = path.read_bytes()
    if data[: len(MAGIC)] != MAGIC or len(data) < len(MAGIC) + 4:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack_format")
    (header_length,) = struct.unpack_from("<I", data, len(MAGIC))
    header_start = len(MAGIC) + 4
    header = json.loads(data[header_start : header_start + header_length])
    if not isinstance(header, dict) or header.get("schema") != SCHEMA or header.get("dtype") != DTYPE:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    count, dimension = header.get("count"), header.get("dimension")
    if not isinstance(count, int) or not isinstance(dimension, int) or count <= 0 or dimension <= 0:
        raise InputFault("corpus_vector_model_mismatch:embedding_pack")
    vectors = VectorPack(
        data,
        header_start + header_length,
        count,
        dimension,
        str(header["pinned_sha"]),
        str(header["embed_template_version"]),
    )
    return {**header, "vectors": vectors}


def write_pack(path: Path, header: Mapping[str, Any], vectors: Mapping[str, Sequence[float]]) -> None:
    """Write `vectors` (fixture-server key -> float list) as a float16 pack."""
    pinned_sha = str(header["pinned_sha"])
    template = str(header["embed_template_version"])
    if not vectors:
        raise InputFault("vector_pack_empty")
    dimensions = {len(vector) for vector in vectors.values()}
    if len(dimensions) != 1:
        raise InputFault(f"vector_pack_dimension_mismatch:{sorted(dimensions)}")
    dimension = dimensions.pop()
    rows = sorted((parse_key(key, pinned_sha, template), vector) for key, vector in vectors.items())
    full_header = dict(header)
    full_header.update({"schema": SCHEMA, "dtype": DTYPE, "dimension": dimension, "count": len(rows)})
    header_bytes = canonical_json(full_header)
    vector_format = struct.Struct(f"<{dimension}e")
    parts = [MAGIC, struct.pack("<I", len(header_bytes)), header_bytes]
    parts.extend(bytes([kind]) + digest for (kind, digest), _ in rows)
    parts.extend(vector_format.pack(*vector) for _, vector in rows)
    path.write_bytes(b"".join(parts))
