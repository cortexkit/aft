#!/usr/bin/env python3
"""Deterministic materialization and hashing for the pinned AFT evidence tree."""
from __future__ import annotations

import hashlib
import io
import json
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import Mapping

DROP_PARTS = {".git", "target", "node_modules", "dist", "build"}
MAX_MEMBER_BYTES = 2 * 1024 * 1024


def _canonical_json(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode()


def evidence_tree_sha256(root: Path) -> str:
    """Hash every relative path and file digest in a materialized evidence tree."""
    members: dict[str, str] = {}
    if not root.is_dir():
        raise FileNotFoundError(root)
    for path in sorted(root.rglob("*")):
        relative_path = path.relative_to(root)
        relative = relative_path.as_posix()
        if ".git" in relative_path.parts:
            if path.is_symlink():
                raise ValueError(f"invalid_evidence_tree_member:{relative}")
            continue
        if path.is_symlink() or (not path.is_file() and not path.is_dir()):
            raise ValueError(f"invalid_evidence_tree_member:{relative}")
        if path.is_file():
            members[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
    return hashlib.sha256(_canonical_json(members)).hexdigest()


def evidence_tree_sha256_from_contents(contents: Mapping[str, str]) -> str:
    members = {
        relative: hashlib.sha256(text.encode()).hexdigest()
        for relative, text in sorted(contents.items())
    }
    return hashlib.sha256(_canonical_json(members)).hexdigest()


def pinned_text_contents(repo: Path, sha: str) -> dict[str, str]:
    """Read the authoring projection from one local commit archive."""
    result = subprocess.run(
        ["git", "archive", "--format=tar", sha],
        cwd=repo,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        raise ValueError(f"evidence_archive_failed:{sha}:{detail}")

    contents: dict[str, str] = {}
    with tarfile.open(fileobj=io.BytesIO(result.stdout), mode="r:") as archive:
        for member in sorted(archive.getmembers(), key=lambda value: value.name):
            relative = PurePosixPath(member.name)
            if member.isdir() or any(part in DROP_PARTS for part in relative.parts):
                continue
            if not member.isfile() or member.size > MAX_MEMBER_BYTES:
                continue
            extracted = archive.extractfile(member)
            if extracted is None:
                continue
            raw = extracted.read()
            try:
                contents[relative.as_posix()] = raw.decode("utf-8")
            except UnicodeDecodeError:
                continue
    return contents


def materialize_evidence_tree(
    repo: Path,
    sha: str,
    destination: Path,
    expected_sha256: str,
) -> str:
    """Replace destination with the pinned, authoring-eligible file projection."""
    contents = pinned_text_contents(repo, sha)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=f".{destination.name}-", dir=destination.parent))
    try:
        for relative, text in contents.items():
            target = temporary / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(text.encode())
        actual = evidence_tree_sha256(temporary)
        if actual != expected_sha256:
            raise ValueError(
                f"evidence_tree_digest_mismatch:expected={expected_sha256}:actual={actual}"
            )
        if destination.exists():
            shutil.rmtree(destination)
        temporary.replace(destination)
        return actual
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)
