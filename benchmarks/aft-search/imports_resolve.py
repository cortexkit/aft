#!/usr/bin/env python3
"""Resolve the closed B1 ruling ledger and immutable evidence citations."""
from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

EVIDENCE_SHA = "30d4a64f99b3b15fd88be6cf962fff4b3fe5ea17"
TOKEN = re.compile(r"(?<![A-Za-z0-9_])R([1-9][0-9]*)(?![A-Za-z0-9_])")

class ResolveFault(ValueError): pass


def load(path: Path) -> dict[str, Any]:
    try: value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error: raise ResolveFault(f"missing_or_invalid_pin:{path}") from error
    if not isinstance(value, dict): raise ResolveFault(f"missing_or_invalid_pin:{path}")
    return value


def digest_definitions(definitions: list[dict[str, Any]]) -> str:
    return hashlib.sha256(json.dumps(definitions, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def strip_ignored(text: str) -> str:
    return re.sub(r"<!--\s*imports-resolve:ignore\s*-->.*?<!--\s*imports-resolve:end\s*-->", "", text, flags=re.S)


def observed_tokens(root: Path, spec: Path) -> set[str]:
    paths = [spec]
    paths += sorted((root / "benchmarks/aft-search").rglob("*"))
    paths += [root / "scripts/telemetry/cost-gate.sh", root / "scripts/telemetry/imports-resolve.sh", root / "scripts/telemetry/postA-remeasure.sh"]
    result: set[str] = set()
    for path in paths:
        if not path.is_file() or "imports-resolve-fixtures" in path.parts or path.suffix in {".zip", ".bin", ".pyc"}: continue
        try: text = strip_ignored(path.read_text(encoding="utf-8"))
        except UnicodeDecodeError: continue
        result.update(f"R{value}" for value in TOKEN.findall(text))
    return result


def git_blob(root: Path, snapshot: str, path: str) -> bytes:
    result = subprocess.run(["git", "show", f"{snapshot}:{path}"], cwd=root, capture_output=True, check=False)
    if result.returncode: raise ResolveFault(f"citation_missing:{path}")
    return result.stdout


def resolve(root: Path, spec: Path, snapshot: str) -> dict[str, Any]:
    if snapshot != EVIDENCE_SHA: raise ResolveFault(f"evidence_pin_mismatch:{snapshot}")
    exists = subprocess.run(["git", "cat-file", "-e", f"{snapshot}^{{commit}}"], cwd=root, capture_output=True)
    if exists.returncode: raise ResolveFault(f"snapshot_missing:{snapshot}")
    local = load(root / "benchmarks/aft-search/b-rulings.json")
    refs = [load(root / "benchmarks/aft-search/campaign-a-ref.json"), load(root / "benchmarks/aft-search/campaign-b2-ref.json")]
    classifications: dict[str, list[str]] = {}
    for source_name, payload in [("b1_local", local), ("campaign_a", refs[0]), ("campaign_b2", refs[1])]:
        if payload.get("evidence_sha", payload.get("snapshot_sha")) != snapshot: raise ResolveFault(f"pin_snapshot_mismatch:{source_name}")
        definitions = payload.get("definitions")
        if not isinstance(definitions, list): raise ResolveFault(f"missing_definitions:{source_name}")
        if source_name != "b1_local" and payload.get("definition_index_sha256") != digest_definitions(definitions):
            raise ResolveFault(f"sibling_definition_digest_mismatch:{source_name}")
        for row in definitions:
            token = row.get("ruling"); definition = row.get("definition")
            if not isinstance(token, str) or not isinstance(definition, str): raise ResolveFault(f"invalid_definition:{source_name}")
            if row.get("definition_sha256") != hashlib.sha256(definition.encode()).hexdigest(): raise ResolveFault(f"definition_digest_mismatch:{token}")
            classifications.setdefault(token, []).append(source_name)
    expected = {f"R{number}" for number in range(1, 46)}
    missing = sorted(expected - set(classifications), key=lambda item: int(item[1:]))
    duplicate = sorted((token for token, sources in classifications.items() if len(sources) != 1), key=lambda item: int(item[1:]))
    foreign = sorted(set(classifications) - expected)
    if missing: raise ResolveFault(f"missing_local_or_imported_definition:{','.join(missing)}")
    if duplicate: raise ResolveFault(f"duplicate_imported_classification:{','.join(duplicate)}")
    if foreign: raise ResolveFault(f"definition_outside_R1_R45:{','.join(foreign)}")
    citations = load(root / "benchmarks/aft-search/load-bearing-citations.json")
    if citations.get("evidence_sha") != snapshot: raise ResolveFault("citation_pin_mismatch")
    for citation in citations.get("citations", []):
        data = git_blob(root, snapshot, citation["path"])
        lines = data.splitlines(keepends=True)
        excerpt = b"".join(lines[citation["start_line"] - 1:citation["end_line"]])
        if hashlib.sha256(excerpt).hexdigest() != citation["sha256"]: raise ResolveFault(f"citation_digest_mismatch:{citation['path']}")
    observed = observed_tokens(root, spec)
    unknown = sorted(observed - expected, key=lambda item: int(item[1:]))
    if unknown: raise ResolveFault(f"unknown_ruling:{','.join(unknown)}")
    return {"snapshot": snapshot, "closed_ledger": "R1-R45", "observed": sorted(observed, key=lambda item: int(item[1:])), "definitions": 45, "citations": len(citations["citations"]), "sibling_digests": "valid"}


def main() -> int:
    parser = argparse.ArgumentParser(); parser.add_argument("--spec", required=True); parser.add_argument("--snapshot", required=True)
    args = parser.parse_args(); root = Path(__file__).resolve().parents[2]
    spec = Path(args.spec); spec = spec if spec.is_absolute() else root / spec
    try: result = resolve(root, spec, args.snapshot)
    except ResolveFault as error:
        print(f"imports_resolve_fault:{error}", file=sys.stderr); return 2
    print(json.dumps(result, sort_keys=True)); return 0

if __name__ == "__main__": raise SystemExit(main())
