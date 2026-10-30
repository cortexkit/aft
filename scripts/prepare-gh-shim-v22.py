#!/usr/bin/env python3
"""Prepare unsigned v22 from the reviewed unsigned v21 payload, without signing.

v22 adds bot speech on pull requests (a title/body `pr edit` of the bot's own
pull request, review-thread replies, resolving and unresolving review threads,
and the pull request stack REST writes), operator administration for gh
extension management, `gh stack merge` and the asynchronous merge endpoint,
the gh-stack subcommands that would write as the operator, mechanical rows
for gh-stack reads, local navigation and `extension list`, and one repository
binding. `pr edit` moves from the admin tier to the governed tier; every other
v21 row and binding stays byte-for-byte. The baseline digest and formatting
checks keep every unrelated v21 byte stable.
"""
import argparse
import hashlib
import json
from pathlib import Path

BASELINE_SHA256 = "6131d4d087e6eb18c9f18122bd9d4aaaf8160e8ba1a763cc8aa2a82384f22239"
ADDITIONS_PATH = Path(__file__).parent.parent / "crates/aft/tests/fixtures/gh_shim/v22-additions.json"
ADDITION_KEYS = {
    "manifest_version",
    "tiers",
    "removed_from_admin",
    "api_rules",
    "canonicalization",
    "bindings",
}


def encode(value):
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def prepare(v21_path: Path, output: Path, issued_at_unix_secs: int) -> bytes:
    raw = v21_path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != BASELINE_SHA256:
        raise ValueError(f"expected reviewed v21 baseline sha256 {BASELINE_SHA256}, got {digest}")
    text = raw.decode("utf-8")
    manifest = json.loads(text)
    if manifest["manifest_version"] != 21:
        raise ValueError("expected the reviewed unsigned v21 baseline")
    if encode(manifest) != text:
        raise ValueError("baseline formatting differs; review before regenerating")
    if issued_at_unix_secs <= manifest["issued_at_unix_secs"]:
        raise ValueError("issued_at_unix_secs must be later than the v21 baseline")

    additions = json.loads(ADDITIONS_PATH.read_text(encoding="utf-8"))
    if set(additions) != ADDITION_KEYS or additions["manifest_version"] != 22:
        raise ValueError(f"v22 additions must contain exactly {sorted(ADDITION_KEYS)}")
    if not set(additions["tiers"]) <= {"governed", "admin", "mechanical"}:
        raise ValueError("v22 additions name an unknown tier")

    # `pr edit` leaves the admin tier before the governed row is added, so the
    # result never declares one tuple in two tiers.
    for tuple_name in additions["removed_from_admin"]:
        before = len(manifest["tiers"]["admin"])
        manifest["tiers"]["admin"] = [
            row for row in manifest["tiers"]["admin"] if row["tuple"] != tuple_name
        ]
        if len(manifest["tiers"]["admin"]) != before - 1:
            raise ValueError(f"baseline admin tier does not declare {tuple_name} exactly once")

    existing_tuples = {
        row["tuple"] for tier_rows in manifest["tiers"].values() for row in tier_rows
    }
    for tier, rows in additions["tiers"].items():
        for row in rows:
            if row["tuple"] in existing_tuples:
                raise ValueError(f"baseline or additions already declares {row['tuple']}")
            existing_tuples.add(row["tuple"])
        manifest["tiers"].setdefault(tier, []).extend(rows)

    for rule in additions["api_rules"]:
        if any(
            existing["method"] == rule["method"] and existing["path_glob"] == rule["path_glob"]
            for existing in manifest["api_rules"]
        ):
            raise ValueError(f"baseline already declares {rule['method']} {rule['path_glob']}")
    manifest["api_rules"].extend(additions["api_rules"])

    for tuple_name, canonical in additions["canonicalization"].items():
        if tuple_name in manifest["canonicalization"]:
            raise ValueError(f"baseline already canonicalizes {tuple_name}")
        manifest["canonicalization"][tuple_name] = canonical

    original_bindings = dict(manifest["bindings"])
    for repository, agent in additions["bindings"].items():
        if repository in manifest["bindings"]:
            raise ValueError(f"baseline already binds {repository}")
        manifest["bindings"][repository] = agent
    if any(manifest["bindings"].get(repo) != agent for repo, agent in original_bindings.items()):
        raise AssertionError("v22 preparation changed an existing v21 binding")

    manifest["manifest_version"] = 22
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
    parser.add_argument(
        "v21_unsigned",
        type=Path,
        nargs="?",
        default=Path(".cortexkit/internal-docs/ceremonies/gh-shim-v22/unsigned-v21.json"),
        help="reviewed unsigned v21 payload bytes",
    )
    parser.add_argument(
        "output",
        type=Path,
        nargs="?",
        default=Path(".cortexkit/internal-docs/ceremonies/gh-shim-v22/unsigned-v22.json"),
        help="unsigned output path (default: the git-ignored ceremony directory)",
    )
    parser.add_argument("--issued-at-unix-secs", type=int, required=True)
    args = parser.parse_args()
    prepare(args.v21_unsigned, args.output, args.issued_at_unix_secs)
