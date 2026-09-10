#!/usr/bin/env python3
"""Run every search-quality family, assemble one score, and apply the gate."""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Sequence

from run_exact_recall import clone_root_for
from run_real_query import load_capability
from search_quality_lib import InputFault
from setup_corpus import parse_corpus_toml

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def command(arguments: Sequence[str], *, cwd: Path = ROOT) -> None:
    print("quality_command:" + " ".join(arguments), flush=True)
    result = subprocess.run(list(arguments), cwd=cwd, check=False)
    print(f"quality_exit:{result.returncode}", flush=True)
    if result.returncode:
        raise SystemExit(result.returncode)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    result.add_argument("--mode", choices=("evaluate", "record-reference", "verify"), default="evaluate")
    result.add_argument("--manifest", default=str(HERE / "real-query-manifest.json"))
    result.add_argument("--reference", default=str(HERE / "real-query-baseline.json"))
    result.add_argument("--sidecar", default=str(HERE / "manifest.sha256"))
    result.add_argument("--descriptor")
    result.add_argument("--branch")
    result.add_argument("--base-ref", default="HEAD^")
    result.add_argument("--head", default="HEAD")
    result.add_argument("--manifest-changed", action="store_true")
    result.add_argument("--old-score")
    result.add_argument("--rebaseline", action="store_true")
    result.add_argument("--from-profile")
    result.add_argument("--to-profile")
    result.add_argument("--dry-run", action="store_true")
    result.add_argument("--profile", choices=("single_page", "paged"))
    result.add_argument("--binary", default=os.environ.get("AFT_BINARY_PATH", str(ROOT / "target/release/aft")))
    result.add_argument("--schema", default=str(ROOT / "packages/pi-plugin/src/tools/semantic.ts"))
    result.add_argument("--corpus", default=str(HERE / "corpus/corpus.toml"))
    result.add_argument("--ready-timeout", type=float, default=600.0)
    result.add_argument("--score-output", default=str(HERE / ".bench/search-quality/score.json"))
    result.add_argument("--self-test", action="store_true")
    return result


def selected_profile(args: argparse.Namespace) -> str:
    if args.profile:
        return args.profile
    if args.rebaseline and args.to_profile:
        return args.to_profile
    if load_capability(Path(args.schema)).get("offset_declared") is True:
        return "paged"
    reference = Path(args.reference)
    if args.mode == "evaluate" and reference.is_file():
        value = json.loads(reference.read_text())
        profile = value.get("profile")
        if profile in {"single_page", "paged"}:
            return str(profile)
    return "single_page"


def ensure_binary(path: Path) -> None:
    if path.is_file():
        print(f"quality_binary:{path}", flush=True)
        return
    if os.environ.get("AFT_BINARY_PATH"):
        raise InputFault(f"aft_binary_missing:{path}")
    command(["cargo", "build", "--release", "-p", "agent-file-tools"])
    if not path.is_file():
        raise InputFault(f"aft_binary_missing_after_build:{path}")
    print(f"quality_binary:{path}", flush=True)


def gate_arguments(args: argparse.Namespace, score_path: Path) -> list[str]:
    values = [
        sys.executable,
        str(HERE / "search_quality.py"),
        "--mode",
        args.mode,
        "--manifest",
        args.manifest,
        "--reference",
        args.reference,
        "--sidecar",
        args.sidecar,
        "--base-ref",
        args.base_ref,
        "--head",
        args.head,
        "--score",
        str(score_path),
    ]
    for option, value in (
        ("--descriptor", args.descriptor),
        ("--branch", args.branch),
        ("--old-score", args.old_score),
        ("--from-profile", args.from_profile),
        ("--to-profile", args.to_profile),
    ):
        if value:
            values.extend([option, value])
    for option, enabled in (
        ("--manifest-changed", args.manifest_changed),
        ("--rebaseline", args.rebaseline),
        ("--dry-run", args.dry_run),
    ):
        if enabled:
            values.append(option)
    return values


def run(args: argparse.Namespace) -> int:
    if args.self_test:
        command([sys.executable, str(HERE / "search_quality.py"), "--self-test"])
        command([sys.executable, "-m", "unittest", "-v", "test_run_real_query.py", "test_search_quality.py"], cwd=HERE)
        return 0

    corpus_path = Path(args.corpus).resolve()
    corpus, _repos = parse_corpus_toml(corpus_path)
    clone_root = clone_root_for(corpus_path, corpus)
    command([sys.executable, str(HERE / "run_exact_recall.py"), "--corpus", str(corpus_path), "--check-corpus"])
    print(f"quality_corpus:{clone_root}", flush=True)

    binary = Path(args.binary).resolve()
    ensure_binary(binary)
    work = Path(args.score_output).resolve().parent
    work.mkdir(parents=True, exist_ok=True)
    exact_score = work / "exact.json"
    concept_score = work / "concept.json"
    score_path = Path(args.score_output).resolve()

    command(
        [
            sys.executable,
            str(HERE / "run_exact_recall.py"),
            "--binary",
            str(binary),
            "--corpus",
            str(corpus_path),
            "--out",
            str(exact_score),
            "--ready-timeout",
            str(args.ready_timeout),
        ]
    )
    command([sys.executable, str(HERE / "run_concept_recall.py"), "--output", str(concept_score)])
    command(
        [
            sys.executable,
            str(HERE / "run_real_query.py"),
            "--manifest",
            args.manifest,
            "--profile",
            selected_profile(args),
            "--binary",
            str(binary),
            "--schema",
            args.schema,
            "--exact-score",
            str(exact_score),
            "--concept-score",
            str(concept_score),
            "--reference",
            args.reference,
            "--output",
            str(score_path),
            "--ready-timeout",
            str(args.ready_timeout),
        ]
    )
    command(gate_arguments(args, score_path))
    return 0


def main() -> int:
    try:
        return run(parser().parse_args())
    except (InputFault, OSError, ValueError, json.JSONDecodeError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
