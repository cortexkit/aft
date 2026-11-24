#!/usr/bin/env python3
"""Check shell's package-local test vectors against the shared originals."""

import json
from pathlib import Path


def unique_entries(pairs):
    entries = dict(pairs)
    if len(entries) != len(pairs):
        raise ValueError("duplicate shell fixture manifest entry")
    return entries


def main():
    root = Path(__file__).resolve().parents[3]
    fixtures = root / "crates/aft-shell/fixtures"
    manifest = fixtures / "shared-inputs.json"
    entries = json.loads(manifest.read_text(encoding="utf-8"), object_pairs_hook=unique_entries)
    listed = {root / copy for copy in entries}
    present = {p for p in fixtures.rglob("*") if p.is_file()} - {manifest}
    if listed != present:
        raise SystemExit(
            f"Shell fixture manifest mismatch: missing files {listed - present}; "
            f"unlisted copies {present - listed}"
        )
    total = 0
    for copy, original in entries.items():
        source = root / original
        if not source.is_file():
            raise SystemExit(f"Shell fixture original missing: {original} (copy: {copy})")
        data = (root / copy).read_bytes()
        if data != source.read_bytes():
            raise SystemExit(f"Shell fixture copy differs from {original}: {copy}")
        total += len(data)
    print(f"Shell fixtures: {len(entries)} exact copies ({total} bytes)")


if __name__ == "__main__":
    main()
