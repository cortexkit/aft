#!/usr/bin/env python3
"""Resolve queue members from merge parents and retain per-PR search contracts.

The prototype supports the queue's merge method only. Unknown topology or PR
association is an input fault, not permission to guess which descriptor applies.
"""

from __future__ import annotations

import argparse
from importlib import import_module
import json
from pathlib import Path
import re
import subprocess
import sys
import time
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "benchmarks/aft-search"))
quality = import_module("search_quality")
quality_lib = import_module("search_quality_lib")
descriptor_labels = quality.descriptor_labels
InputFault = quality_lib.InputFault
resolve_descriptor = quality_lib.resolve_descriptor


def git(*args: str) -> str:
    result = subprocess.run(["git", *args], text=True, capture_output=True, check=False)
    if result.returncode:
        raise InputFault(f"unresolvable_queue_diff:{result.stderr.strip()}")
    return result.stdout.strip()


def paths(base: str, head: str) -> list[str]:
    return git("diff", "--name-only", "--no-renames", f"{base}...{head}").splitlines()


def first_parent_between(base: str, head: str) -> list[str]:
    """Commits on head's first-parent chain after base, oldest first.

    Walks the chain from head until it reaches base instead of asking
    `git rev-list base..head`. Merge-queue commits carry committer dates out of
    order (a group commit can be dated before the commits under it), and git's
    date-based range walk then returns commits that base already contains.
    """
    chain = []
    for sha in git("rev-list", "--first-parent", head).splitlines():
        if sha == base:
            return list(reversed(chain))
        chain.append(sha)
    raise InputFault("queue_base_not_on_first_parent_chain")


def resolve_git_descriptor(descriptor: dict, changed: list[str], base: str, head: str) -> tuple[dict, bool]:
    notes: list[str] = []
    try:
        return resolve_descriptor(descriptor, changed, schema_blobs=quality_lib.read_tool_schema_blobs(base, head, changed, root=Path.cwd()), notes=notes)
    finally:
        for note in notes:
            print(note, file=sys.stderr)


PR_LOOKUP_BACKOFF_SECONDS = (5, 10, 15, 30)
# GitHub's commit-to-PR endpoint returns intermittent HTTP 5xx replies; one
# measurement saw 2 failures in 8 calls. Six attempts with 67 seconds of total
# backoff make an exhausted lookup unlikely. Even if every attempt also hits the
# 30-second gh timeout, one lookup ends within about 4 minutes, far below the
# search-quality job's timeout.
PR_API_RETRY_BACKOFF_SECONDS = (2, 5, 10, 20, 30)
# The member lookup requests a single page of this size. A commit is never
# realistically associated with this many PRs, so a full page is refused rather
# than silently treated as the complete answer.
PR_LOOKUP_PAGE_SIZE = 100
GH_API_TIMEOUT_SECONDS = 30


def _transient_gh_error(stderr: str) -> bool:
    definite_status = r"\bHTTP(?:/\d(?:\.\d)?)?\s+(?:401|403|404)\b"
    if re.search(definite_status, stderr, re.IGNORECASE):
        return False
    # gh reports a failed request as, for example, "gh: Bad Gateway (HTTP 502)".
    # A server-side 5xx is GitHub's fault and worth retrying.
    if re.search(r"\bHTTP(?:/\d(?:\.\d)?)?\s+5\d\d\b", stderr, re.IGNORECASE):
        return True
    json_parse_error = (
        r"\b(?:unexpected end of JSON input|unexpected EOF|invalid JSON|"
        r"invalid character .+looking for|error (?:parsing|decoding) JSON)\b"
    )
    if re.search(json_parse_error, stderr, re.IGNORECASE):
        return True
    return re.search(
        r"\b(?:timeout|timed out|connection (?:reset|refused|closed|aborted|error)|"
        r"failed to connect|error connecting|could not resolve|"
        r"temporary failure in name resolution|network is unreachable|no route to host|"
        r"remote end closed connection)\b",
        stderr,
        re.IGNORECASE,
    ) is not None


def _gh_api_json(command: list[str], lookup_key: str, *, sleep=None) -> Any:
    delay = time.sleep if sleep is None else sleep
    attempts = len(PR_API_RETRY_BACKOFF_SECONDS) + 1
    for attempt in range(1, attempts + 1):
        try:
            result = subprocess.run(
                command,
                text=True,
                capture_output=True,
                check=False,
                timeout=GH_API_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as error:
            detail = f"gh api timed out after {GH_API_TIMEOUT_SECONDS} seconds"
            if error.stderr:
                timeout_stderr = (
                    error.stderr.decode(errors="replace")
                    if isinstance(error.stderr, bytes)
                    else error.stderr
                )
                detail += f": {timeout_stderr}"
        else:
            if result.returncode:
                detail = result.stderr.strip()
                if not _transient_gh_error(detail):
                    raise InputFault(f"queue_pr_lookup_failed:{lookup_key}:{detail}")
            else:
                try:
                    return json.loads(result.stdout)
                except json.JSONDecodeError as error:
                    detail = (
                        "empty reply"
                        if not result.stdout.strip()
                        else f"invalid JSON reply: {error}"
                    )

        if attempt == attempts:
            raise InputFault(
                f"queue_pr_lookup_failed:{lookup_key}:retries_exhausted:"
                f"attempts={attempts}:{detail}"
            )
        delay(PR_API_RETRY_BACKOFF_SECONDS[attempt - 1])


def lookup_member_prs(repository: str, head: str, *, sleep=None) -> list[dict]:
    # One page, without --paginate/--slurp: with those flags gh hides an HTTP
    # 500 behind "unexpected end of JSON input", so the real status never
    # reaches stderr for the transient-error check.
    prs = _gh_api_json(
        [
            "gh", "api",
            f"repos/{repository}/commits/{head}/pulls?per_page={PR_LOOKUP_PAGE_SIZE}",
        ],
        head,
        sleep=sleep,
    )
    if not isinstance(prs, list):
        raise InputFault(f"queue_pr_lookup_failed:{head}:reply_not_a_list")
    if len(prs) >= PR_LOOKUP_PAGE_SIZE:
        raise InputFault(
            f"queue_pr_lookup_failed:{head}:page_full:count={len(prs)}:"
            "more associated PRs than one page can hold"
        )
    return prs


def lookup_queue_ref_pr(repository: str, number: int, *, sleep=None) -> dict:
    return _gh_api_json(
        ["gh", "api", f"repos/{repository}/pulls/{number}"],
        str(number),
        sleep=sleep,
    )


def is_member_pr(pr: dict, repository: str, head: str) -> bool:
    return (
        pr["head"]["sha"] == head
        and pr["base"]["ref"] == "main"
        and pr["base"]["repo"]["full_name"] == repository
    )


def member_pr(
    repository: str,
    head: str,
    *,
    queue_ref_pr_number: int | None = None,
    lookup=lookup_member_prs,
    queue_ref_lookup=lookup_queue_ref_pr,
    sleep=time.sleep,
) -> dict:
    for attempt in range(1, len(PR_LOOKUP_BACKOFF_SECONDS) + 2):
        matches = [
            pr for pr in lookup(repository, head)
            if is_member_pr(pr, repository, head)
        ]
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise InputFault(
                f"ambiguous_queue_member:{head}:matches={len(matches)}:attempts={attempt}"
            )
        if attempt <= len(PR_LOOKUP_BACKOFF_SECONDS):
            sleep(PR_LOOKUP_BACKOFF_SECONDS[attempt - 1])

    if queue_ref_pr_number is not None:
        pr = queue_ref_lookup(repository, queue_ref_pr_number)
        # The queue ref names only its final PR; its API record is safe to use
        # only when the PR still points at this merge's exact second-parent SHA.
        if (
            pr is not None
            and pr.get("number") == queue_ref_pr_number
            and is_member_pr(pr, repository, head)
        ):
            return pr

    raise InputFault(
        f"ambiguous_queue_member:{head}:matches=0:attempts={len(PR_LOOKUP_BACKOFF_SECONDS) + 1}"
    )


def member_descriptor(head: str, branch: str, group_head: str) -> tuple[str, dict]:
    git("check-ref-format", "--branch", branch)
    candidates = []
    for label in descriptor_labels(branch):
        path = f"benchmarks/aft-search/slice-descriptors/{label}.json"
        exists = subprocess.run(
            ["git", "cat-file", "-e", f"{head}:{path}"],
            capture_output=True, check=False,
        )
        if exists.returncode == 0:
            candidates.append(path)
    if len(candidates) != 1:
        raise InputFault(f"queue_member_descriptor:{branch}:matches={len(candidates)}")
    path = candidates[0]
    descriptor = json.loads(git("show", f"{head}:{path}"))
    if not isinstance(descriptor, dict):
        raise InputFault(f"malformed_queue_descriptor:{path}")
    # A later PR must not replace an earlier PR's descriptor at the same path.
    if descriptor != json.loads(git("show", f"{group_head}:{path}")):
        raise InputFault(f"queue_descriptor_overwritten:{path}")
    return path, descriptor


def queue_members(base: str, head: str, head_ref: str | None, repository: str) -> list[dict]:
    for sha in (base, head):
        if re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            raise InputFault("queue_requires_full_base_and_head_shas")
    if git("rev-parse", "HEAD") != head:
        raise InputFault("queue_checkout_head_mismatch")
    if git("merge-base", base, head) != base:
        raise InputFault("queue_base_not_ancestor")
    tail = None
    if head_ref is not None:
        tail = re.fullmatch(r"(?:refs/heads/)?gh-readonly-queue/main/pr-(\d+)-.+", head_ref)
        if tail is None:
            raise InputFault(f"unknown_queue_ref:{head_ref}")
    members = []
    previous = base
    commits = first_parent_between(base, head)
    for index, commit in enumerate(commits):
        parents = git("show", "-s", "--format=%P", commit).split()
        # Only two-parent merges bind the synthetic commit to an exact PR head.
        if len(parents) != 2 or parents[0] != previous:
            raise InputFault(f"unsupported_queue_topology:{commit}")
        queue_ref_pr_number = int(tail[1]) if tail is not None and index == len(commits) - 1 else None
        pr = member_pr(repository, parents[1], queue_ref_pr_number=queue_ref_pr_number)
        if any(member["number"] == pr["number"] for member in members):
            raise InputFault(f"duplicate_queue_member:{pr['number']}")
        members.append({"number": pr["number"], "branch": pr["head"]["ref"], "head": parents[1]})
        previous = commit
    if not members or (tail is not None and members[-1]["number"] != int(tail[1])):
        raise InputFault("queue_ref_member_mismatch")
    return members


def plan(base: str, head: str, head_ref: str, repository: str) -> dict:
    members = queue_members(base, head, head_ref, repository)
    descriptors = {}
    for member in members:
        path, descriptor = member_descriptor(member["head"], member["branch"], head)
        member_base = git("merge-base", base, member["head"])
        resolved, missing = resolve_git_descriptor(descriptor, paths(member_base, member["head"]), member_base, member["head"])
        if missing:
            raise InputFault(f"missing_queue_descriptor:{member['number']}")
        member.update(descriptor=path, slice_class=resolved["slice_class"])
        descriptors[path] = descriptor
    ranking = [member for member in members if member["slice_class"] == "ranking"]
    if len(ranking) > 1:
        raise InputFault("multiple_ranking_queue_members:land these ranking PRs separately")
    # Keep the single ranking PR's targeted_mechanism and optional exemptions
    # unchanged when evaluating the combined tree. Other descriptors have been
    # validated against their own PR diffs as non_ranking or engine_unwired.
    # Without a ranking PR, evaluate each applicable descriptor separately so
    # one PR's expectations cannot silently replace another's.
    selected = ranking or [member for member in members if member["slice_class"] == "engine_unwired"] or members
    evaluations = list(dict.fromkeys(member["descriptor"] for member in selected))
    group_paths = paths(base, head)
    for path in evaluations:
        resolve_git_descriptor(descriptors[path], group_paths, base, head)
    return {"base": base, "head": head, "members": members, "evaluations": evaluations}



def evaluate(candidate: dict, mode: str) -> None:
    descriptors = candidate["evaluations"]
    common = ["--mode", mode, "--base-ref", candidate["base"], "--head", candidate["head"]]
    first = [str(ROOT / "scripts/telemetry/cost-gate.sh"), "--search-quality", *common]
    if mode == "evaluate":
        first.extend(["--descriptor", descriptors[0]])
    subprocess.run(first, check=True)
    if mode == "evaluate":
        # The score measures the combined queue tree. Compare that same score
        # under each remaining descriptor instead of replaying the corpus; the
        # descriptors may require different search-result comparisons.
        for descriptor in descriptors[1:]:
            subprocess.run(
                [sys.executable, str(ROOT / "benchmarks/aft-search/search_quality.py"),
                 *common, "--descriptor", descriptor, "--score",
                 str(ROOT / "benchmarks/aft-search/.bench/search-quality/score.json")],
                check=True,
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--head-ref", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--mode", choices=("evaluate", "verify"), default="evaluate")
    parser.add_argument("--plan-only", action="store_true")
    parser.add_argument("--plan-output", type=Path)
    args = parser.parse_args()
    try:
        candidate = plan(args.base, args.head, args.head_ref, args.repository)
        text = json.dumps(candidate, sort_keys=True)
        print(f"merge_queue_search_plan:{text}", flush=True)
        if args.plan_output:
            args.plan_output.write_text(text + "\n")
        if not args.plan_only:
            evaluate(candidate, args.mode)
        return 0
    except subprocess.CalledProcessError as error:
        return error.returncode
    except (InputFault, OSError, ValueError, KeyError, TypeError) as error:
        print(f"merge_queue_search_input_fault:{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
