#!/usr/bin/env python3
"""all-MiniLM-L6-v2 embedder that reproduces AFT's local backend, for authoring only.

`capture_real_query_vectors.py` uses this to fill the gate's vector pack with
the vectors AFT's default local backend (`crates/aft/src/local_embed.rs`) would
compute. It needs onnxruntime, tokenizers and numpy, which the gate itself
never imports; run it through, for example,
`uv run --with onnxruntime==1.24.4 --with tokenizers==0.22.2 --with numpy`,
matching the ONNX Runtime AFT's plugin installs and the tokenizers version in
Cargo.lock.

Each step mirrors local_embed.rs, because a different rounding anywhere would
make the stored vectors differ from what users get:
  - tokenizer.json as shipped (including its fixed padding to 128 tokens),
    truncation raised to 512, special tokens added;
  - int64 input_ids / attention_mask / token_type_ids (zeros), the first
    model output as [batch, seq, dim];
  - mean pool summed in float32 in token order over masked positions, divided
    by the masked count;
  - L2 normalise with the norm summed in float32 in dimension order and
    divided by (norm + 1e-12).
Texts are embedded one at a time. On the capture machine, embedding a text
alone and inside a padded batch gave bit-identical vectors, and so did one
intra-op thread versus several.
"""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
from typing import Any, Optional, Sequence

from search_quality_lib import InputFault

MODEL_REPO = "Qdrant/all-MiniLM-L6-v2-onnx"
MODEL_ID = "all-MiniLM-L6-v2"
# The snapshot and file digests the checked-in pack was captured from. A
# different download could change every vector, so capture refuses it.
MODEL_REVISION = "5f1b8cd78bc4fb444dd171e59b18f3a3af89a079"
MODEL_SHA256 = "bbd7b466f6d58e646fdc2bd5fd67b2f5e93c0b687011bd4548c420f7bd46f0c5"
TOKENIZER_SHA256 = "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0"
MAX_LENGTH = 512
# The model cache AFT's plugin manages; FASTEMBED_CACHE_DIR overrides it, as it
# does for AFT itself.
MANAGED_MODEL_CACHE = Path.home() / ".local/share/cortexkit/aft/semantic/models"


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def model_snapshot(cache_dir: Optional[Path] = None) -> Path:
    cache = cache_dir or Path(os.environ.get("FASTEMBED_CACHE_DIR") or MANAGED_MODEL_CACHE)
    snapshot = cache / "models--Qdrant--all-MiniLM-L6-v2-onnx" / "snapshots" / MODEL_REVISION
    model, tokenizer = snapshot / "model.onnx", snapshot / "tokenizer.json"
    if not model.is_file() or not tokenizer.is_file():
        raise InputFault(f"local_model_missing:{snapshot}")
    if _sha256(model) != MODEL_SHA256 or _sha256(tokenizer) != TOKENIZER_SHA256:
        raise InputFault(f"local_model_digest_mismatch:{snapshot}")
    return snapshot


class MiniLmEmbedder:
    def __init__(self, snapshot: Optional[Path] = None):
        import numpy
        import onnxruntime
        import tokenizers

        self._np = numpy
        snapshot = snapshot or model_snapshot()
        options = onnxruntime.SessionOptions()
        # GraphOptimizationLevel::Level3 in ort is ORT_ENABLE_ALL.
        options.graph_optimization_level = onnxruntime.GraphOptimizationLevel.ORT_ENABLE_ALL
        options.intra_op_num_threads = max(1, min(-(-(os.cpu_count() or 1) // 2), 8))
        self._session = onnxruntime.InferenceSession(
            str(snapshot / "model.onnx"), options, providers=["CPUExecutionProvider"]
        )
        self._tokenizer = tokenizers.Tokenizer.from_file(str(snapshot / "tokenizer.json"))
        self._tokenizer.enable_truncation(max_length=MAX_LENGTH)
        self._wants_token_type_ids = any(item.name == "token_type_ids" for item in self._session.get_inputs())
        self.source: dict[str, Any] = {
            "model_repo": MODEL_REPO,
            "model_revision": MODEL_REVISION,
            "model_onnx_sha256": MODEL_SHA256,
            "tokenizer_json_sha256": TOKENIZER_SHA256,
            "onnxruntime": onnxruntime.__version__,
            "tokenizers": tokenizers.__version__,
        }

    def embed(self, texts: Sequence[str]) -> list[list[float]]:
        return [self._embed_one(text) for text in texts]

    def _embed_one(self, text: str) -> list[float]:
        np = self._np
        encoding = self._tokenizer.encode(text, add_special_tokens=True)
        length = max(1, len(encoding.ids))
        ids = np.zeros((1, length), np.int64)
        mask = np.zeros((1, length), np.int64)
        ids[0, : len(encoding.ids)] = encoding.ids
        mask[0, : len(encoding.attention_mask)] = encoding.attention_mask
        feeds = {"input_ids": ids, "attention_mask": mask}
        if self._wants_token_type_ids:
            feeds["token_type_ids"] = np.zeros((1, length), np.int64)
        hidden = self._session.run(None, feeds)[0]
        if hidden.ndim != 3:
            raise InputFault(f"local_model_output_rank:{hidden.ndim}")
        hidden = hidden.astype(np.float32)
        total = np.zeros(hidden.shape[2], np.float32)
        valid = np.float32(0)
        for column in range(hidden.shape[1]):
            if mask[0, column] == 1:
                valid += np.float32(1)
                total += hidden[0, column]
        pooled = total / (valid if valid else np.float32(1))
        squared = np.float32(0)
        for value in pooled * pooled:
            squared = np.float32(squared + value)
        normalised = pooled / np.float32(np.sqrt(squared) + np.float32(1e-12))
        return [float(value) for value in normalised]
