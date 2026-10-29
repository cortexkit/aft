#!/usr/bin/env python3
"""Prepare an unsigned v23 draft from the SHA-pinned v22 baseline.

Only the two governed queue entries and their canonicalizations are added.
All prior rows, bindings and other fields stay unchanged. This writes a draft
and digest; it neither signs nor installs the manifest.
"""
import argparse
import hashlib
import json
from pathlib import Path

BASELINE_SHA256 = "223adba389f8727abdecc3faae2b6497395279f5ff2f8dd011c63c6c3cbfadad"
ADDITIONS_PATH = Path(__file__).parent.parent / "crates/aft/tests/fixtures/gh_shim/v23-additions.json"
QUEUE_TUPLES = {"pr enqueue", "pr dequeue"}


def encode(value):
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def prepare(v22_path: Path, output: Path, issued_at_unix_secs: int) -> bytes:
    raw = v22_path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != BASELINE_SHA256:
        raise ValueError(f"expected reviewed v22 baseline sha256 {BASELINE_SHA256}, got {digest}")
    manifest = json.loads(raw)
    if manifest["manifest_version"] != 22 or encode(manifest).encode("utf-8") != raw:
        raise ValueError("expected the reviewed unsigned v22 baseline and formatting")
    if issued_at_unix_secs <= manifest["issued_at_unix_secs"]:
        raise ValueError("issued_at_unix_secs must be later than the v22 baseline")
    additions = json.loads(ADDITIONS_PATH.read_text(encoding="utf-8"))
    if (set(additions) != {"manifest_version", "tiers", "canonicalization"}
            or additions["manifest_version"] != 23
            or set(additions["tiers"]) != {"governed"}
            or {row["tuple"] for row in additions["tiers"]["governed"]} != QUEUE_TUPLES
            or len(additions["tiers"]["governed"]) != 2
            or set(additions["canonicalization"]) != QUEUE_TUPLES):
        raise ValueError("v23 additions must declare exactly the two governed queue entries")
    existing = {row["tuple"] for rows in manifest["tiers"].values() for row in rows}
    if existing & QUEUE_TUPLES or set(manifest["canonicalization"]) & QUEUE_TUPLES:
        raise ValueError("baseline already declares a queue entry")
    for row in additions["tiers"]["governed"]:
        if row["platform"] != ["macos", "linux", "windows"]:
            raise ValueError("queue entries must cover all platforms")
    manifest["tiers"]["governed"].extend(additions["tiers"]["governed"])
    manifest["canonicalization"].update(additions["canonicalization"])
    manifest["manifest_version"] = 23
    manifest["issued_at_unix_secs"] = issued_at_unix_secs
    payload = encode(manifest).encode("utf-8")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(payload)
    digest = hashlib.sha256(payload).hexdigest()
    output.with_suffix(".sha256").write_text(f"{digest}  {output.name}\n", encoding="utf-8")
    print(f"{len(payload)} bytes  {digest}  {output}")
    return payload


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    ceremony = Path(".cortexkit/internal-docs/ceremonies/gh-shim-v23")
    parser.add_argument("v22_unsigned", type=Path, nargs="?", default=ceremony / "unsigned-v22.json")
    parser.add_argument("output", type=Path, nargs="?", default=ceremony / "unsigned-v23.json")
    parser.add_argument("--issued-at-unix-secs", type=int, required=True)
    args = parser.parse_args()
    prepare(args.v22_unsigned, args.output, args.issued_at_unix_secs)
