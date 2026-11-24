#!/usr/bin/env python3
"""Check that package-local GitHub test inputs match the shared originals."""

import json
from pathlib import Path


def unique_entries(pairs):
    entries = dict(pairs)
    if len(entries) != len(pairs):
        raise ValueError("duplicate GitHub fixture manifest entry")
    return entries


def main():
    root = Path(__file__).resolve().parents[3]
    fixtures = root / "crates/aft-github/fixtures"
    manifest = fixtures / "shared-inputs.json"
    entries = json.loads(manifest.read_text(encoding="utf-8"), object_pairs_hook=unique_entries)
    listed = {root / copy for copy in entries}
    present = {p for p in fixtures.rglob("*") if p.is_file()} - {manifest}
    if listed != present:
        raise SystemExit(
            f"GitHub fixture manifest mismatch: missing files {listed - present}; "
            f"unlisted copies {present - listed}"
        )
    originals = set(entries.values())
    for directory in ("requests-contract", "replies-contract"):
        contract = root / "crates/aft/tests/fixtures/gh_shim/plexus-contract" / directory
        for entry in contract.iterdir():
            if entry.relative_to(root).as_posix() not in originals:
                raise SystemExit(f"GitHub contract fixture missing from manifest: {entry}")
    total = 0
    for copy, original in entries.items():
        source = root / original
        if not source.is_file():
            raise SystemExit(f"GitHub fixture original missing: {original} (copy: {copy})")
        data = (root / copy).read_bytes()
        if data != source.read_bytes():
            raise SystemExit(f"GitHub fixture copy differs from {original}: {copy}")
        total += len(data)
    print(f"GitHub fixtures: {len(entries)} exact copies ({total} bytes)")


if __name__ == "__main__":
    main()
