#!/usr/bin/env python3
"""Pin default heads and count tracked paths using shallow, blobless Git clones."""
from __future__ import annotations

import argparse
from collections import Counter
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess

PROJECTS = {
    "elastic/elasticsearch": ["Java"],
    "apache/spark": ["Scala", "Java"],
    "microsoft/vscode": ["TypeScript"],
    "apache/superset": ["TSX", "JavaScript", "Python"],
    "nodejs/node": ["JavaScript", "C", "C++"],
    "kubernetes/kubernetes": ["Go"],
    "home-assistant/core": ["Python"],
    "rust-lang/rust": ["Rust"],
    "ethereum/solidity": ["Solidity", "C++"],
    "gitlab-org/gitlab": ["Ruby"],
    "JetBrains/kotlin": ["Kotlin", "Java"],
    "swiftlang/swift": ["Swift", "C++"],
    "magento/magento2": ["PHP"],
    "Perl/perl5": ["Perl", "C"],
    "neovim/neovim": ["Lua", "C"],
    "torvalds/linux": ["C"],
    "llvm/llvm-project": ["C++"],
    "NVIDIA/TensorRT-LLM": ["CUDA"],
    "ml-explore/mlx": ["Metal"],
    "ziglang/zig": ["Zig"],
    "dotnet/runtime": ["C#"],
}
# Counts describe tracked files, including tests and vendored code, not AFT's
# ignore-filtered corpus. Ambiguous .h files are kept separate, not guessed.
EXTENSIONS = {
    "java": "Java", "scala": "Scala", "sc": "Scala", "ts": "TypeScript",
    "mts": "TypeScript", "cts": "TypeScript", "tsx": "TSX", "js": "JavaScript",
    "jsx": "JavaScript", "mjs": "JavaScript", "cjs": "JavaScript", "py": "Python",
    "pyi": "Python", "go": "Go", "rs": "Rust", "sol": "Solidity", "rb": "Ruby",
    "kt": "Kotlin", "kts": "Kotlin", "swift": "Swift", "php": "PHP", "pl": "Perl",
    "pm": "Perl", "t": "Perl", "lua": "Lua", "c": "C", "h": "C/C++ header",
    "cc": "C++", "cpp": "C++", "cxx": "C++", "hpp": "C++", "hh": "C++",
    "hxx": "C++", "cu": "CUDA", "cuh": "CUDA", "metal": "Metal", "zig": "Zig",
    "cs": "C#",
}
def git(*args: str, cwd: Path | None = None) -> str:
    return subprocess.check_output(["git", *args], cwd=cwd, text=True)


def pin(repo: str, languages: list[str], cache: Path) -> dict:
    host = "gitlab.com" if repo == "gitlab-org/gitlab" else "github.com"
    url = f"https://{host}/{repo}.git"
    remote = git("ls-remote", "--symref", url, "HEAD")
    sha = next(line.split()[0] for line in remote.splitlines() if line.endswith("\tHEAD") and not line.startswith("ref:"))
    branch = next(line.split()[1].removeprefix("refs/heads/") for line in remote.splitlines() if line.startswith("ref:"))
    destination = cache / repo.replace("/", "--")
    if not destination.exists():
        git("clone", "--depth", "1", "--filter=blob:none", "--no-checkout", "--single-branch", "--branch", branch, url, str(destination))
    # The branch may advance between ls-remote and clone; the manifest always
    # describes the observed HEAD, not whichever commit clone happened to see.
    git("fetch", "--depth", "1", "--filter=blob:none", "origin", sha, cwd=destination)
    tree = subprocess.check_output(["git", "ls-tree", "-r", "-z", sha], cwd=destination)
    counts: Counter[str] = Counter()
    files = submodules = 0
    for entry in tree.split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        if metadata.split()[1] == b"commit":
            submodules += 1
            continue
        files += 1
        language = EXTENSIONS.get(name.decode("utf-8", "surrogateescape").rsplit(".", 1)[-1])
        if language:
            counts[language] += 1
    return {"repository": repo, "clone_url": url, "commit": sha, "default_branch": branch,
            "pinned_at": datetime.now(timezone.utc).isoformat(), "languages": languages,
            "file_counts_by_language": dict(sorted(counts.items())), "tracked_files": files,
            "checkout_bytes": None, "checkout_size_status": "measured by runner after checkout",
            "submodule_count": submodules}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=Path(__file__).with_name("manifest.json"))
    parser.add_argument("--repository", choices=list(PROJECTS), action="append")
    parser.add_argument("--cache", type=Path, default=Path("target/index-matrix/pin-clones"))
    parser.add_argument("--resume", action="store_true", help="keep already pinned entries unchanged")
    args = parser.parse_args()
    selected = args.repository or list(PROJECTS)
    existing = json.loads(args.output.read_text()) if args.output.exists() else {
        "schema_version": 1,
        "pin_method": "git ls-remote HEAD; shallow blobless clone; complete git ls-tree -r at pinned SHA",
        "checkout_size_definition": "pending until measurement: du -sb checkout excluding .git, including tracked symlinks; submodules and LFS payloads not populated",
        "file_count_definition": "tracked blob paths by extension (case-sensitive), including tests/vendors; .h separate; not AFT's ignore-filtered corpus",
        "repositories": [],
    }
    args.cache.mkdir(parents=True, exist_ok=True)
    records = {row["repository"]: row for row in existing["repositories"]}
    for repo in selected:
        if args.resume and repo in records:
            continue
        print(f"pinning {repo}", flush=True)
        records[repo] = pin(repo, PROJECTS[repo], args.cache)
        existing["repositories"] = [records[name] for name in PROJECTS if name in records]
        args.output.write_text(json.dumps(existing, indent=2) + "\n")
        print(f"pinned {repo}: {records[repo]['commit']} {records[repo]['tracked_files']} files", flush=True)


if __name__ == "__main__":
    main()
