#!/usr/bin/env python3
"""Table the content arms of the semantic embed RSS harness (issue #327).

Each arm writes a samples file; this reads them all and puts the arms beside
each other, which is the only form in which they mean anything — the question
is never "is 0.4 MB per batch a lot" but "does this content class differ from
the others".

Two slopes are reported per arm because they answer different questions:

  embed     fitted across the embed phase with the final batch excluded. The
            index is written to disk once the last batch lands, and that write
            is a large transient that belongs to persistence rather than to the
            embed loop. Including it makes a short run look steeper than a long
            one purely because the spike is amortised over fewer batches.
  overall   fitted across every batch-carrying sample, persistence included.
            Reported so the exclusion above is visible rather than assumed.

The settled resident set is also divided by chunk count, because a slope and a
level are different claims. Two arms can share a slope and still end at very
different resident sets if one content class costs more per stored chunk, and a
level proportional to the corpus is what a plateau looks like.

Usage:
    python3 semantic-embed-rss-content-arms-report.py <results dir> [...]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def fit(points: list[tuple[int, int]]) -> float:
    """Least-squares bytes-per-batch over (batch, rss_bytes) points."""
    if len(points) < 2:
        return 0.0
    count = len(points)
    mean_batch = sum(batch for batch, _ in points) / count
    mean_rss = sum(rss for _, rss in points) / count
    variance = sum((batch - mean_batch) ** 2 for batch, _ in points)
    if not variance:
        return 0.0
    covariance = sum((batch - mean_batch) * (rss - mean_rss) for batch, rss in points)
    return covariance / variance


def arm_row(path: Path) -> dict:
    payload = json.loads(path.read_text(encoding="utf-8"))
    samples = payload["samples"]
    total = max((sample["total_batches"] for sample in samples), default=0)

    seen: dict[int, int] = {}
    for sample in samples:
        if sample["batch"] > 0:
            seen.setdefault(sample["batch"], sample["rss_bytes"])
    points = sorted(seen.items())
    embed_points = [(batch, rss) for batch, rss in points if batch < total]

    corpus = payload["corpus"]
    stub = payload.get("stub", {})
    # Batches are full except the last, so this is the chunk count to within one
    # batch. It is used only to divide a resident-set figure, where that error
    # is far below the differences being compared.
    chunks = max(1, total * 64)
    settled = samples[-1]["rss_bytes"] if samples else 0
    return {
        "label": payload["label"],
        "corpus": corpus["content_class"],
        "sidecar": corpus["sidecar_kind"],
        "corpus_mb": (corpus["spine_bytes"] + corpus["sidecar_bytes"]) / 1e6,
        "batches": total,
        "embed_mb_per_batch": fit(embed_points) / 1e6,
        "overall_mb_per_batch": fit(points) / 1e6,
        "first": embed_points[0] if embed_points else (0, 0),
        "last": embed_points[-1] if embed_points else (0, 0),
        "peak_mb": max((sample["rss_bytes"] for sample in samples), default=0) / 1e6,
        "settled_mb": settled / 1e6,
        "kb_per_chunk": settled / chunks / 1024,
        "requests": payload.get("embed_requests", 0),
        "rejections": payload.get("embed_rejections", 0),
        "max_row_tokens": stub.get("max_row_tokens", 0),
        "max_row_chars": stub.get("max_row_chars", 0),
        "max_row_bytes": stub.get("max_row_bytes", 0),
        "limit": payload.get("reject_over_tokens", 0),
    }


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2

    rows = []
    for directory in argv[1:]:
        for path in sorted(Path(directory).glob("*/*-samples.json")):
            rows.append(arm_row(path))

    if not rows:
        print("no arm samples found")
        return 1

    header = (
        f"{'arm':<20}{'corpus MB':>10}{'batches':>9}{'MB/batch':>10}"
        f"{'(w/ save)':>11}{'peak MB':>9}{'settled':>9}{'KB/chunk':>10}"
        f"{'requests':>10}{'rejected':>10}{'row tok':>9}"
    )
    print(header)
    print("-" * len(header))
    for row in rows:
        print(
            f"{row['label']:<20}{row['corpus_mb']:>10.1f}{row['batches']:>9}"
            f"{row['embed_mb_per_batch']:>10.3f}{row['overall_mb_per_batch']:>11.3f}"
            f"{row['peak_mb']:>9.0f}{row['settled_mb']:>9.0f}{row['kb_per_chunk']:>10.1f}"
            f"{row['requests']:>10}{row['rejections']:>10}"
            f"{row['max_row_tokens']:>9}"
        )

    print()
    for row in rows:
        first_batch, first_rss = row["first"]
        last_batch, last_rss = row["last"]
        print(
            f"{row['label']:<20} embed window batch {first_batch}->{last_batch}, "
            f"{first_rss / 1e6:.1f} -> {last_rss / 1e6:.1f} MB; "
            f"widest row {row['max_row_chars']} chars / {row['max_row_bytes']} bytes / "
            f"{row['max_row_tokens']} tokens "
            f"(limit {row['limit'] or 'none'})"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
