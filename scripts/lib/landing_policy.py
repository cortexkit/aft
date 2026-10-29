"""Read-only identity rules for original PR commits, never queue merge authors."""

from __future__ import annotations

from importlib import import_module
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

# Agent delivery branches and the bot author are the attribution source of
# truth. The owner also commits by hand without the agent hook, so owner and
# other human authors are exempt off land/*, but never on an agent delivery PR.
AGENT_ATTRIBUTION_POLICY = {
    "branch_prefix": "land/",
    "bot_name": "alfonso-aft",
    "bot_email": "289616620+alfonso-aft@users.noreply.github.com",
    "coauthor": "alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>",
}


def git(*args: str, input: str | None = None) -> str:
    return subprocess.check_output(["git", *args], input=input, text=True).strip()


def commits_between(base: str, head: str, *also_known: str) -> list[str]:
    """Commits reachable from head but not from base (or `also_known`), newest first.

    Compares full ancestry sets instead of asking `git rev-list base..head`.
    Merge-queue commits carry committer dates out of order, and git's
    date-based range walk then lists commits that base already contains (a
    queue group once reported main's own merge commit as a PR commit).
    """
    known = set(git("rev-list", base, *also_known).splitlines())
    return [sha for sha in git("rev-list", head).splitlines() if sha not in known]


def check_commits(base: str, head: str, branch: str, also_known: tuple[str, ...] = ()) -> int:
    for sha in (base, head, *also_known):
        if re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            raise ValueError("identity policy requires full base/head SHAs")
        git("cat-file", "-e", f"{sha}^{{commit}}")
    count = 0
    for sha in commits_between(base, head, *also_known):
        name, author, committer, message = git(
            "show", "-s", "--format=%an%x00%ae%x00%ce%x00%B", sha
        ).split("\0", 3)
        for role, email in (("author", author), ("committer", committer)):
            if not (re.fullmatch(r"[^@\s]+@users\.noreply\.github\.com", email)
                    or email == "noreply@github.com"):
                # Do not repeat a private email in public CI logs.
                raise ValueError(f"{sha}: {role} email must be GitHub noreply")
        agent = (branch.startswith(AGENT_ATTRIBUTION_POLICY["branch_prefix"])
                 or name == AGENT_ATTRIBUTION_POLICY["bot_name"]
                 or author == AGENT_ATTRIBUTION_POLICY["bot_email"])
        trailers = git("interpret-trailers", "--parse", input=message).splitlines()
        attributed = any(
            line.partition(":")[0].lower() == "co-authored-by"
            and line.partition(":")[2].strip() == AGENT_ATTRIBUTION_POLICY["coauthor"]
            for line in trailers
        )
        if agent and not attributed:
            raise ValueError(f"{sha}: {branch} agent commit missing alfonso-aft co-author trailer")
        count += 1
    return count


def fetched_main() -> tuple[str, ...]:
    """The fetched `origin/main` commit, or nothing when it isn't fetched."""
    try:
        return (git("rev-parse", "--verify", "--quiet", "origin/main^{commit}"),)
    except subprocess.CalledProcessError:
        return ()


def check_event(event: str, payload: dict) -> int:
    queue = import_module("merge-queue-search")
    if event == "pull_request":
        pr = payload["pull_request"]
        head = pr["head"]["sha"]
        if git("rev-parse", "HEAD") != head:
            raise ValueError("PR identity checkout does not match the original head")
        # The event's base SHA can predate the main a PR branch later merged
        # in, which would count main's own landed commits as the PR's. Commits
        # already on main passed this policy when they landed, so exclude
        # them. Without a fetched main, keep the stricter event base alone.
        return check_commits(pr["base"]["sha"], head, pr["head"]["ref"], fetched_main())
    if event == "merge_group":
        group = payload["merge_group"]
        members = queue.queue_members(group["base_sha"], group["head_sha"],
                                      group["head_ref"], payload["repository"]["full_name"])
        return sum(check_commits(group["base_sha"], member["head"], member["branch"])
                   for member in members)
    if event == "push":
        head = payload["after"]
        if git("rev-parse", "HEAD") != head:
            raise ValueError("identity checkout does not match the push SHA")
        if payload["ref"] != "refs/heads/main":
            # Keep legacy train validation during migration. A branch's first
            # push has an all-zero before SHA, so classify its main-based range.
            base = git("merge-base", "origin/main", head)
            return check_commits(base, head, payload["ref"].removeprefix("refs/heads/"))
        base = payload["before"]
        commits = queue.first_parent_between(base, head)
        if commits and all(len(git("show", "-s", "--format=%P", sha).split()) == 2 for sha in commits):
            members = queue.queue_members(base, head, None, payload["repository"]["full_name"])
            return sum(check_commits(base, member["head"], member["branch"]) for member in members)
        # Pre-cutover direct pushes still get the universal email check. Once
        # queue protection is enabled, main arrives through the PR path above.
        return check_commits(base, head, "main")
    raise ValueError(f"unsupported landing identity event: {event}")
