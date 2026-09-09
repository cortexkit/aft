#!/usr/bin/env python3
"""Provision the exact-recall repositories at their immutable commits."""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Mapping, Optional, Sequence

from search_quality_lib import InputFault, canonical_json, sha256_file
from setup_corpus import parse_corpus_toml

HERE = Path(__file__).resolve().parent
DEFAULT_CORPUS = HERE / "corpus/corpus.toml"
PROVISION_COMMAND = "python3 benchmarks/aft-search/provision_corpus.py"
JsonObject = dict[str, Any]


def run_git(arguments: Sequence[str], cwd: Optional[Path] = None) -> str:
    result = subprocess.run(
        ["git", *arguments],
        cwd=cwd,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise InputFault(f"corpus_provision_failed:{' '.join(arguments)}:{detail}")
    return result.stdout.strip()


def clone_root_for(corpus_path: Path, corpus: Mapping[str, Any]) -> Path:
    clone_root = Path(str(corpus.get("clone_root", ".bench/repos")))
    return clone_root if clone_root.is_absolute() else corpus_path.parent.parent / clone_root


def current_commit(path: Path) -> Optional[str]:
    if not (path / ".git").is_dir():
        return None
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=path, text=True, capture_output=True, check=False
    )
    return result.stdout.strip() if result.returncode == 0 else None


def provision_repo(repo: Mapping[str, Any], clone_root: Path) -> JsonObject:
    name = str(repo["name"])
    url = str(repo["url"])
    commit = str(repo["commit"])
    destination = clone_root / name
    if destination.exists() and not (destination / ".git").is_dir():
        raise InputFault(f"corpus_provision_failed:{name}:destination_not_git:{destination}")
    if not destination.exists():
        destination.mkdir(parents=True)
        run_git(["init", "--quiet"], destination)
        run_git(["remote", "add", "origin", url], destination)
    else:
        remotes = run_git(["remote"], destination).splitlines()
        if "origin" not in remotes:
            run_git(["remote", "add", "origin", url], destination)
        elif run_git(["remote", "get-url", "origin"], destination) != url:
            raise InputFault(f"corpus_provision_failed:{name}:origin_mismatch")
    if current_commit(destination) != commit:
        run_git(["fetch", "--depth", "1", "origin", commit], destination)
        run_git(["checkout", "--detach", "--force", "FETCH_HEAD"], destination)
    actual = current_commit(destination)
    if actual != commit:
        raise InputFault(f"corpus_provision_failed:{name}:expected={commit}:actual={actual}")
    shallow = run_git(["rev-parse", "--is-shallow-repository"], destination) == "true"
    print(f"corpus_ready:{name}:{commit}")
    return {"name": name, "url": url, "commit": commit, "actual_commit": actual, "shallow": shallow}


def provision(corpus_path: Path) -> Path:
    corpus, repos = parse_corpus_toml(corpus_path)
    if not repos:
        raise InputFault("corpus_provision_failed:empty_manifest")
    clone_root = clone_root_for(corpus_path, corpus)
    clone_root.mkdir(parents=True, exist_ok=True)
    rows = [provision_repo(repo, clone_root) for repo in repos]
    record = {
        "schema": "aft-search-corpus-provision-v1",
        "corpus_manifest": str(corpus_path.resolve()),
        "corpus_manifest_sha256": sha256_file(corpus_path),
        "repos": rows,
    }
    record_path = clone_root / "provisioned.json"
    record_path.write_bytes(canonical_json(record))
    print(f"corpus_record:{record_path}")
    print(f"corpus_record_sha256:{sha256_file(record_path)}")
    return record_path


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--corpus", default=str(DEFAULT_CORPUS))
    return result


def main() -> int:
    try:
        provision(Path(parser().parse_args().corpus).resolve())
    except (InputFault, OSError, KeyError, ValueError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
