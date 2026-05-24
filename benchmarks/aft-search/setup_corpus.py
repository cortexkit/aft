#!/usr/bin/env python3
"""Clone and pin the external Vera-compatible search benchmark corpus."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple


SCRIPT_DIR = Path(__file__).resolve().parent
CORPUS_MANIFEST = SCRIPT_DIR / "corpus" / "corpus.toml"
DEFAULT_CLONE_ROOT = SCRIPT_DIR / ".bench" / "repos"

JsonObject = Dict[str, object]


def parse_corpus_toml(path: Path) -> Tuple[JsonObject, List[JsonObject]]:
    corpus: JsonObject = {}
    repos: List[JsonObject] = []
    current: Optional[JsonObject] = None

    for raw_line in path.read_text().splitlines():
        line = raw_line.split("#", 1)[0].strip()
        if not line:
            continue
        if line == "[corpus]":
            current = corpus
            continue
        if line == "[[repos]]":
            current = {}
            repos.append(current)
            continue
        if "=" not in line or current is None:
            continue
        key, value = [part.strip() for part in line.split("=", 1)]
        if value.startswith('"') and value.endswith('"'):
            current[key] = value[1:-1]
        else:
            try:
                current[key] = int(value)
            except ValueError:
                current[key] = value
    return corpus, repos


def run_git(args: List[str], cwd: Optional[Path] = None, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        ["git", *args],
        cwd=str(cwd) if cwd else None,
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        command = " ".join(["git", *args])
        raise RuntimeError(f"{command} failed: {result.stderr.strip() or result.stdout.strip()}")
    return result


def current_commit(repo_dir: Path) -> Optional[str]:
    if not (repo_dir / ".git").exists():
        return None
    result = run_git(["rev-parse", "HEAD"], cwd=repo_dir, check=False)
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def repo_size(repo_dir: Path) -> Tuple[int, int]:
    files = 0
    bytes_total = 0
    for path in repo_dir.rglob("*"):
        if ".git" in path.parts:
            continue
        if path.is_file():
            files += 1
            try:
                bytes_total += path.stat().st_size
            except OSError:
                pass
    return files, bytes_total


def human_bytes(size: int) -> str:
    value = float(size)
    for suffix in ("B", "KiB", "MiB", "GiB"):
        if value < 1024.0 or suffix == "GiB":
            return f"{value:.1f} {suffix}"
        value /= 1024.0
    return f"{value:.1f} GiB"


def ensure_repo(repo: JsonObject, clone_root: Path) -> bool:
    name = str(repo["name"])
    url = str(repo["url"])
    commit = str(repo["commit"])
    repo_dir = clone_root / name

    try:
        if repo_dir.exists() and current_commit(repo_dir) == commit:
            files, bytes_total = repo_size(repo_dir)
            print(f"{name}: already at {commit[:12]} ({files} files, {human_bytes(bytes_total)})")
            return True

        if not repo_dir.exists():
            print(f"{name}: cloning {url} -> {repo_dir}")
            run_git(["clone", url, str(repo_dir)])
        elif current_commit(repo_dir) is None:
            print(f"{name}: {repo_dir} exists but is not a git checkout; skipping", file=sys.stderr)
            return False

        print(f"{name}: fetching and resetting to {commit}")
        run_git(["fetch", "--all", "--tags", "--prune"], cwd=repo_dir)
        run_git(["reset", "--hard", commit], cwd=repo_dir)
        run_git(["clean", "-fdx"], cwd=repo_dir)
        files, bytes_total = repo_size(repo_dir)
        print(f"{name}: ready at {commit[:12]} ({files} files, {human_bytes(bytes_total)})")
        return True
    except RuntimeError as error:
        print(f"{name}: FAILED: {error}", file=sys.stderr)
        return False


def main() -> int:
    if not CORPUS_MANIFEST.exists():
        raise SystemExit(f"missing corpus manifest: {CORPUS_MANIFEST}")

    corpus, repos = parse_corpus_toml(CORPUS_MANIFEST)
    clone_root = SCRIPT_DIR / str(corpus.get("clone_root", ".bench/repos"))
    if not clone_root.is_absolute():
        clone_root = SCRIPT_DIR / clone_root
    clone_root.mkdir(parents=True, exist_ok=True)

    print(f"corpus manifest: {CORPUS_MANIFEST}")
    print(f"clone root: {clone_root}")

    failed: List[str] = []
    for repo in repos:
        if not ensure_repo(repo, clone_root):
            failed.append(str(repo.get("name", "<unknown>")))

    if failed:
        print(f"failed repos: {', '.join(failed)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
