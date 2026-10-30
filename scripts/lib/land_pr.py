"""Idempotent agent delivery submission and exact queue-attempt observation."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from importlib import import_module
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
from urllib.parse import quote
import zipfile

policy = import_module("landing_policy")
AGENT_ATTRIBUTION_POLICY = policy.AGENT_ATTRIBUTION_POLICY
ROOT = policy.ROOT
check_commits = policy.check_commits
git = policy.git


def log(text: str) -> None:
    print(f"land-pr: {text}", file=sys.stderr, flush=True)


def gh(*args: str) -> str:
    # Removing an inherited administration bypass keeps PR creation bot-routed.
    env = {key: value for key, value in os.environ.items() if key != "GH_SHIM_BYPASS"}
    return subprocess.check_output(["gh", *args], env=env, text=True).strip()


def operator_gh(*args: str) -> str:
    resolver = ROOT / "scripts/lib/operator-gh.sh"
    command = 'source "$1" >/dev/null; printf "%s" "$OPERATOR_GH"'
    path = subprocess.check_output(["bash", "-c", command, "bash", str(resolver)], text=True).strip()
    env = {**os.environ, "GH_SHIM_BYPASS": "operator"}
    return subprocess.check_output([path, *args], env=env, text=True).strip()


def disarm_auto_merge(repo: str, number: int, reason: str) -> None:
    owner, name = repo.split("/")
    query = ("query($owner:String!,$repo:String!,$pr:Int!){repository(owner:$owner,name:$repo){"
             "pullRequest(number:$pr){id isInMergeQueue autoMergeRequest{enabledAt}}}}")
    response = json.loads(operator_gh("api", "graphql", "-f", f"query={query}",
                                      "-F", f"owner={owner}", "-F", f"repo={name}", "-F", f"pr={number}"))
    pull = response.get("data", {}).get("repository", {}).get("pullRequest") or {}
    pull_id = pull.get("id")
    if not pull_id:
        raise ValueError(f"could not identify PR #{number} before disarming auto-merge")
    if pull.get("isInMergeQueue"):
        mutation = ("mutation($id:ID!){dequeuePullRequest(input:{id:$id}){clientMutationId}}")
        operator_gh("api", "graphql", "-f", f"query={mutation}", "-F", f"id={pull_id}")
        log(f"dequeued PR #{number} before {reason}")
    if pull.get("autoMergeRequest") is not None:
        mutation = ("mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){clientMutationId}}")
        operator_gh("api", "graphql", "-f", f"query={mutation}", "-F", f"id={pull_id}")
        log(f"disabled auto-merge for PR #{number} before {reason}")


def unresolved_review_threads(repo: str, number: int) -> int:
    owner, name = repo.split("/")
    query = ("query($owner:String!,$repo:String!,$pr:Int!,$after:String){repository(owner:$owner,name:$repo){"
             "pullRequest(number:$pr){reviewThreads(first:100,after:$after){nodes{isResolved} "
             "pageInfo{hasNextPage endCursor}}}}}")
    cursor = None
    unresolved = 0
    while True:
        args = ["api", "graphql", "-f", f"query={query}",
                "-F", f"owner={owner}", "-F", f"repo={name}", "-F", f"pr={number}"]
        if cursor is not None:
            args.extend(("-F", f"after={cursor}"))
        response = json.loads(operator_gh(*args))
        pull = response.get("data", {}).get("repository", {}).get("pullRequest") or {}
        threads = pull.get("reviewThreads") or {}
        unresolved += sum(not thread.get("isResolved", True) for thread in threads.get("nodes") or [])
        page_info = threads.get("pageInfo") or {}
        if not page_info.get("hasNextPage"):
            return unresolved
        cursor = page_info.get("endCursor")
        if not cursor:
            raise ValueError(f"review-thread pagination returned no cursor for PR #{number}")


def admission_timestamp(current: dict) -> str | None:
    entry = current.get("mergeQueueEntry") or {}
    auto_merge = current.get("autoMergeRequest") or {}
    return entry.get("enqueuedAt") or auto_merge.get("enabledAt")


def verify_governed_gh() -> None:
    shim_dir = os.environ.get("AFT_GH_SHIMS_DIR")
    shim_path = shutil.which("gh")
    if not shim_dir or shim_path is None or not Path(shim_dir).is_absolute():
        raise ValueError("governed gh shim must be on PATH for PR speech")
    expected_shim = Path(shim_dir) / "gh"
    try:
        path_is_shim = os.path.samefile(shim_path, expected_shim)
    except (OSError, TypeError, ValueError):
        path_is_shim = False
    if not path_is_shim:
        raise ValueError("gh on PATH does not resolve to the governed gh shim")
    try:
        status = json.loads(gh("--status"))
    except (json.JSONDecodeError, subprocess.CalledProcessError) as error:
        raise ValueError("governed gh shim --status must return JSON") from error
    if not isinstance(status, dict):
        raise ValueError("governed gh shim --status must return a JSON object")
    image = status.get("executing_image")
    if not isinstance(image, str):
        raise ValueError("governed gh shim status JSON has no executing image")
    try:
        resolves_to_shim = os.path.samefile(expected_shim, image)
    except (OSError, TypeError, ValueError):
        resolves_to_shim = False
    if not resolves_to_shim:
        raise ValueError("gh on PATH does not resolve to the governed gh shim")
    last_rung = status.get("last_rung")
    if not isinstance(last_rung, dict) or last_rung.get("rung") != "R3":
        raise ValueError("governed gh shim must report last_rung R3 for PR speech")
    inputs = last_rung.get("determination_inputs")
    if not isinstance(inputs, dict) or inputs.get("agent_credentials_present") != "absent":
        raise ValueError("governed gh shim reports ambient credentials for PR speech")


def api(path: str) -> dict:
    return json.loads(gh("api", path))


def pages(path: str) -> list[dict]:
    return [item for page in json.loads(gh("api", "--paginate", "--slurp", path)) for item in page]


def snapshot(repo: str, number: int) -> dict:
    owner, name = repo.split("/")
    response = json.loads(gh(
        "api", "graphql", "-f", "query=query($owner:String!,$repo:String!,$pr:Int!){"
        "repository(owner:$owner,name:$repo){pullRequest(number:$pr){"
        "state headRefOid baseRefName isMergeQueueEnabled isInMergeQueue mergeCommit{oid} "
        "autoMergeRequest{enabledAt} mergeQueueEntry{enqueuedAt state headCommit{oid}} "
        "timelineItems(last:1,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){nodes{"
        "... on RemovedFromMergeQueueEvent{createdAt reason beforeCommit{oid}}}}}}}",
        "-F", f"owner={owner}", "-F", f"repo={name}", "-F", f"pr={number}",
    ))
    return response["data"]["repository"]["pullRequest"]


def save(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, sort_keys=True) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def stamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))




def workflow_runs(repo: str, head: str, since: str | None = None) -> list[dict]:
    endpoint = f"repos/{repo}/actions/workflows/tests.yml/runs?event=merge_group&per_page=100"
    if head:
        endpoint += f"&head_sha={quote(head, safe='')}"
    if since:
        endpoint += "&created=" + quote(">=" + stamp(since).strftime("%Y-%m-%dT%H:%M:%SZ"), safe="")
    response = json.loads(gh("api", "--paginate", "--slurp", endpoint))
    return [run for page in response for run in page["workflow_runs"]]


def diagnostic_context(repo: str, run: dict, member_head: str) -> bool:
    response = json.loads(gh("api", "--paginate", "--slurp",
                            f"repos/{repo}/actions/runs/{run['id']}/artifacts?per_page=100"))
    artifacts = [artifact for page in response for artifact in page["artifacts"]
                 if artifact["name"] == f"queue-context-{run['head_sha']}" and not artifact["expired"]]
    if len(artifacts) != 1:
        return False
    env = {key: value for key, value in os.environ.items() if key != "GH_SHIM_BYPASS"}
    archive = subprocess.check_output(
        ["gh", "api", f"repos/{repo}/actions/artifacts/{artifacts[0]['id']}/zip"], env=env,
    )
    with zipfile.ZipFile(io.BytesIO(archive)) as data:
        if data.getinfo("queue-context.json").file_size > 4096:
            raise ValueError("oversized queue diagnostic context")
        context = json.loads(data.read("queue-context.json"))
    base, head = context["base_sha"], context["head_sha"]
    if (head != run["head_sha"] or re.fullmatch(r"[0-9a-f]{40}", head) is None
            or re.fullmatch(r"[0-9a-f]{40}", base) is None
            or not context["head_ref"].removeprefix("refs/heads/").startswith("gh-readonly-queue/main/")):
        return False
    cursor, found = head, False
    # Validate the full event-pinned prefix, not arbitrary main history that
    # could contain the same PR head from an earlier admission.
    for _ in range(100):
        if cursor == base:
            return found
        parents = api(f"repos/{repo}/git/commits/{cursor}")["parents"]
        if len(parents) != 2:
            return False
        found = found or parents[1]["sha"] == member_head
        cursor = parents[0]["sha"]
    return cursor == base and found


def report_removed(repo: str, state: dict, removal: dict | None) -> None:
    admitted_at = state.get("admitted_at")
    heads = list(state.get("groups", [])) if admitted_at else []
    candidate = ((removal or {}).get("beforeCommit") or {}).get("oid")
    if admitted_at and candidate and candidate not in heads:
        cursor = candidate
        for _ in range(100):
            parents = api(f"repos/{repo}/git/commits/{cursor}")["parents"]
            if len(parents) != 2:
                break
            if parents[1]["sha"] == state["head"]:
                heads.append(candidate)
                break
            cursor = parents[0]["sha"]
    runs = {run["id"]: run for head in heads for run in workflow_runs(repo, head)
            if run["head_sha"] == head} if admitted_at else {}
    # A short-lived entry may be missed, and its headCommit need not be the
    # Actions group SHA. Discover recent candidates only with event metadata
    # plus merge-parent proof; a branch-name or timestamp match alone is unsafe.
    # Without an admission time, older merge-group failures cannot be attributed safely.
    if admitted_at:
        for run in workflow_runs(repo, "", admitted_at):
            if (run["event"] == "merge_group" and stamp(run["created_at"]) >= stamp(admitted_at)
                    and run["conclusion"] in {"failure", "timed_out", "cancelled"}
                    and run["id"] not in runs and diagnostic_context(repo, run, state["head"])):
                runs[run["id"]] = run
    found = False
    for run in runs.values():
        if (not admitted_at or run["event"] != "merge_group" or stamp(run["created_at"]) < stamp(admitted_at)
                or run["conclusion"] not in {"failure", "timed_out", "cancelled"}):
            continue
        log(f"failing group {run['head_sha']}: {run['html_url']} ({run['conclusion']})")
        # The jobs endpoint pages are objects ({"total_count", "jobs"}),
        # unlike the pulls endpoint, whose pages are arrays.
        jobs = [job for page in json.loads(gh("api", "--paginate", "--slurp",
                f"repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100"))
                for job in page["jobs"]]
        bad = [job for job in jobs if job["conclusion"] in {"failure", "timed_out", "cancelled"}]
        for job in bad:
            log(f"  {job['name']}: {job['conclusion']}")
        if not bad:
            log("  no failing job was reported; inspect missing checks or queue timeout")
        found = True
    if not found:
        log("removed from queue; exact failing group/run could not be established (no stale run attributed)")


def wait_merged(repo: str, state: dict, path: Path, deadline: float, poll: float) -> str:
    state.setdefault("groups", [])
    state.setdefault("seen_queued", False)
    state.setdefault("removed", False)
    while time.monotonic() < deadline:
        current = snapshot(repo, state["pr"])
        if current["headRefOid"] != state["head"] or current["baseRefName"] != "main":
            raise ValueError("PR head/base changed after admission; refusing stale delivery")
        if current["state"] == "MERGED":
            oid = (current["mergeCommit"] or {}).get("oid", "")
            if re.fullmatch(r"[0-9a-f]{40}", oid) is None:
                raise ValueError("merged PR has no exact final merge OID")
            state["merge_oid"] = oid
            save(path, state)
            log(f"merged #{state['pr']} at {oid}")
            return oid
        if current["state"] != "OPEN":
            raise ValueError("PR closed without merging")
        if current.get("isInMergeQueue") and not state.get("seen_queued"):
            state["seen_queued"] = True
            save(path, state)
        entry = current.get("mergeQueueEntry") or {}
        if entry:
            group = (entry.get("headCommit") or {}).get("oid")
            if group and group not in state["groups"]:
                state["groups"].append(group)
                save(path, state)
        if not state.get("admitted_at"):
            admitted_at = admission_timestamp(current)
            if admitted_at:
                state["admitted_at"] = admitted_at
                save(path, state)
        if unresolved_review_threads(repo, state["pr"]):
            disarm_auto_merge(repo, state["pr"], "unresolved review threads")
            raise ValueError(f"PR #{state['pr']} has unresolved review threads; auto-merge disabled and queue entry removed")
        removals = (current.get("timelineItems") or {}).get("nodes") or []
        admitted_at = state.get("admitted_at")
        removal = removals[-1] if (removals and admitted_at and
                                   stamp(removals[-1]["createdAt"]) >= stamp(admitted_at)) else None
        if not current["isInMergeQueue"] and (state.get("seen_queued") or removal):
            state["removed"] = True
            save(path, state)
            report_removed(repo, state, removal)
            raise ValueError(f"PR #{state['pr']} removed from queue: {(removal or {}).get('reason', 'entry disappeared')}")
        time.sleep(poll)
    raise ValueError(f"timed out waiting for PR #{state['pr']}; rerun to resume this same PR")


def submit(args: argparse.Namespace) -> str:
    os.chdir(git("rev-parse", "--show-toplevel"))
    if re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,99}", args.name) is None:
        raise ValueError("delivery name must be a simple lowercase branch suffix")
    if git("status", "--porcelain"):
        raise ValueError("working tree must be clean before landing")
    branch = f"land/{args.name}"
    path = Path(git("rev-parse", "--git-path", f"aft-land/{args.name}.json"))
    state = json.loads(path.read_text()) if path.exists() else {}
    repo = gh("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    owner = repo.split("/")[0]
    prs = [pr for pr in pages(f"repos/{repo}/pulls?state=all&base=main&head={quote(owner + ':' + branch, safe='')}&per_page=100")
           if pr["head"]["ref"] == branch and pr["head"]["repo"]["full_name"] == repo
           and pr["base"]["ref"] == "main"]
    # A PR closed without merging at another head is an abandoned earlier
    # attempt (for example a stale release PR closed by hand). GitHub can't
    # reopen it once its branch is recreated, so a new PR on the same branch
    # supersedes it. A merged PR, or one closed at this exact head, still
    # refuses below.
    current_head = git("rev-parse", "HEAD")
    prs = [pr for pr in prs
           if not (pr["state"] == "closed" and pr.get("merged_at", "unknown") is None
                   and pr["head"]["sha"] != current_head)]
    if len(prs) > 1:
        raise ValueError("ambiguous existing PRs for delivery branch")
    existing_head = prs[0]["head"]["sha"] if prs else None
    if prs and prs[0]["state"] == "closed":
        current = snapshot(repo, prs[0]["number"])
        if current["state"] != "MERGED" or current["headRefOid"] != git("rev-parse", "HEAD"):
            raise ValueError("delivery branch already closed or merged at a different head")
        state.update(pr=prs[0]["number"], head=current["headRefOid"], repo=repo)
        state["merge_oid"] = current["mergeCommit"]["oid"]
        if re.fullmatch(r"[0-9a-f]{40}", state["merge_oid"]) is None:
            raise ValueError("merged PR has no exact final merge OID")
        save(path, state)
        return state["merge_oid"]
    git("fetch", "--quiet", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    head = git("rev-parse", "HEAD")
    base = git("merge-base", "origin/main", head)
    if not prs and base != git("rev-parse", "origin/main"):
        raise ValueError("first submission must be based on current origin/main")
    if git("branch", "--show-current") != branch:
        exists = subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"], check=False)
        if exists.returncode == 0 and git("rev-parse", branch) != head:
            raise ValueError("local delivery branch already points at another commit")
        git("checkout", branch) if exists.returncode == 0 else git("checkout", "-b", branch)
    quality = import_module("merge-queue-search")
    changed = quality.paths(base, head)
    descriptor_path = Path(f"benchmarks/aft-search/slice-descriptors/land-{args.name}.json")
    if args.descriptor:
        descriptor = json.loads(args.descriptor.read_text())
    elif descriptor_path.exists():
        descriptor = json.loads(descriptor_path.read_text())
    else:
        descriptor = {"slice_class": "non_ranking", "targeted_mechanism": "none",
                      "kind": "harness", "fixtures": ["harness-goldens"]}
    quality.resolve_git_descriptor(descriptor, changed, base, head)
    encoded = json.dumps(descriptor, sort_keys=True, indent=2) + "\n"
    if not descriptor_path.exists() or json.loads(descriptor_path.read_text()) != descriptor:
        descriptor_path.parent.mkdir(parents=True, exist_ok=True)
        descriptor_path.write_text(encoded)
        git("add", str(descriptor_path))
        git("commit", "-m", f"landing: describe {args.name}", "-m",
            f"Co-authored-by: {AGENT_ATTRIBUTION_POLICY['coauthor']}")
    head = git("rev-parse", "HEAD")
    check_commits(base, head, branch)
    # In the governed gh shim's status, R3 means governance succeeded with no agent GitHub credentials detected.
    verify_governed_gh()
    coauthor_line = gh("--co-author-line")
    coauthor = coauthor_line.removeprefix("Co-authored-by: ")
    if not coauthor_line:
        coauthor = os.environ.get("AFT_GIT_CO_AUTHOR", "")
    if coauthor != AGENT_ATTRIBUTION_POLICY["coauthor"]:
        raise ValueError("governed gh shim must provide alfonso-aft co-author identity for PR speech")
    state.update(repo=repo, branch=branch, head=head)
    save(path, state)
    if existing_head is not None and existing_head != head:
        disarm_auto_merge(repo, prs[0]["number"], "pushing a new PR head")
    git("push", "origin", f"HEAD:refs/heads/{branch}")
    created = False
    if not prs:
        body = args.body_file.read_text() if args.body_file else "Accepted agent delivery; branch-keyed search descriptor included."
        try:
            gh("pr", "create", "--base", "main", "--head", branch,
               "--title", args.title or f"land: {args.name}", "--body", body)
            created = True
        except subprocess.CalledProcessError:
            # A concurrent creator may have won; exact re-query still must find
            # one matching PR, rather than treating any create failure as green.
            pass
        prs = pages(f"repos/{repo}/pulls?state=open&base=main&head={quote(owner + ':' + branch, safe='')}&per_page=100")
        prs = [pr for pr in prs if pr["head"]["ref"] == branch and pr["head"]["repo"]["full_name"] == repo]
        if len(prs) != 1:
            raise ValueError("could not establish exactly one created delivery PR")
    state["pr"] = prs[0]["number"]
    save(path, state)
    pull = api(f"repos/{repo}/pulls/{state['pr']}")
    author = pull.get("user") if isinstance(pull, dict) else None
    if not isinstance(author, dict) or (author.get("login"), author.get("type"), author.get("id")) != (
            "aft-alfonso[bot]", "Bot", 318960130):
        raise ValueError(f"PR #{state['pr']} author is not the aft-alfonso app")
    if not created:
        log(f"PR #{state['pr']} already exists; leaving its current title and body unchanged")
    current = snapshot(repo, state["pr"])
    if current["headRefOid"] != head:
        raise ValueError("remote PR head does not match the pushed delivery")
    deadline = time.monotonic() + float(os.environ.get("LAND_PR_TIMEOUT_SECONDS", "14400"))
    poll = float(os.environ.get("LAND_PR_POLL_SECONDS", "30"))
    if not current["isInMergeQueue"] and not current["autoMergeRequest"]:
        date = stamp(api(f"repos/{repo}/commits/{head}")["commit"]["committer"]["date"])
        while (datetime.now(timezone.utc) - date).total_seconds() < 300:
            if time.monotonic() >= deadline:
                raise ValueError("settle timeout; resume the same PR later")
            time.sleep(poll)
        state.update(admitted_at=datetime.now(timezone.utc).isoformat(), groups=[], seen_queued=False, removed=False)
        save(path, state)
        command = [str(ROOT / "scripts/merge-pr.sh"), str(state["pr"]), "--expected-head", head]
        if args.legacy_merge:
            command.append("--legacy-merge")
        subprocess.run(command, stdout=sys.stderr, check=True)
    elif "admitted_at" not in state:
        admitted_at = admission_timestamp(current)
        if admitted_at:
            state["admitted_at"] = admitted_at
            state.setdefault("groups", [])
            state.setdefault("seen_queued", False)
            save(path, state)
        else:
            log(f"PR #{state['pr']} is queued but its admission timestamp is not available yet; continuing to wait")
    return wait_merged(repo, state, path, deadline, poll)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("--descriptor", type=Path)
    parser.add_argument("--title")
    parser.add_argument("--body-file", type=Path)
    parser.add_argument("--legacy-merge", action="store_true")
    try:
        print(submit(parser.parse_args()), flush=True)
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        log(f"REFUSED: {error}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
