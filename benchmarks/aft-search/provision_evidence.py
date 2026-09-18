#!/usr/bin/env python3
"""Provision the pinned AFT real-query evidence tree from the local repository."""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping, Optional, Sequence

from evidence_tree import evidence_tree_sha256, materialize_evidence_tree
from search_quality_lib import EVIDENCE_SHA, InputFault

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DEFAULT_MANIFEST = HERE / "real-query-manifest.json"
JsonObject = dict[str, Any]


def evidence_root(sha: str = EVIDENCE_SHA) -> Path:
    return HERE / ".bench" / "repos" / f"aft-evidence-{sha}"


def run_git(arguments: Sequence[str], repo: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", *arguments],
        cwd=repo,
        text=True,
        capture_output=True,
        check=False,
    )


def ensure_local_commit(repo: Path, sha: str, origin: str = "origin") -> None:
    present = run_git(["cat-file", "-e", f"{sha}^{{commit}}"], repo)
    if present.returncode == 0:
        return
    fetched = run_git(["fetch", "--no-tags", "--depth", "1", origin, sha], repo)
    if fetched.returncode:
        detail = fetched.stderr.strip() or fetched.stdout.strip()
        raise InputFault(f"corpus_provision_failed:aft-evidence:fetch:{detail}")
    present = run_git(["cat-file", "-e", f"{sha}^{{commit}}"], repo)
    if present.returncode:
        raise InputFault(f"corpus_provision_failed:aft-evidence:missing_commit:{sha}")


def manifest_binding(manifest: Mapping[str, Any]) -> tuple[str, str]:
    sha = str(manifest.get("evidence_sha"))
    included = [row for row in manifest.get("rows", []) if "excluded_reason" not in row]
    digests = {str(row.get("evidence_tree_sha256")) for row in included}
    if sha != EVIDENCE_SHA or not included or len(digests) != 1 or "None" in digests:
        raise InputFault("corpus_vector_model_mismatch:manifest")
    return sha, next(iter(digests))


def _tree_matches(destination: Path, expected_sha256: str) -> bool:
    try:
        return evidence_tree_sha256(destination) == expected_sha256
    except (FileNotFoundError, OSError, ValueError):
        return False


def provision(
    manifest_path: Path = DEFAULT_MANIFEST,
    repo: Path = ROOT,
    destination: Optional[Path] = None,
    origin: str = "origin",
) -> Path:
    manifest = json.loads(manifest_path.read_text())
    if not isinstance(manifest, dict):
        raise InputFault("corpus_vector_model_mismatch:manifest")
    sha, expected_sha256 = manifest_binding(manifest)
    target = destination or evidence_root(sha)
    if not _tree_matches(target, expected_sha256):
        ensure_local_commit(repo, sha, origin)
        try:
            materialize_evidence_tree(repo, sha, target, expected_sha256)
        except ValueError as error:
            raise InputFault(f"corpus_vector_model_mismatch:{target}:{error}") from error
    print(f"evidence_tree_ready:{target}:{sha}")
    print(f"evidence_tree_sha256:{expected_sha256}")
    return target


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--manifest", default=str(DEFAULT_MANIFEST))
    result.add_argument("--repo-root", default=str(ROOT))
    result.add_argument("--destination")
    result.add_argument("--origin", default="origin")
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        provision(
            Path(args.manifest).resolve(),
            Path(args.repo_root).resolve(),
            Path(args.destination).resolve() if args.destination else None,
            args.origin,
        )
    except (InputFault, OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
