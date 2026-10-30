"""Prepare isolated patch retries and verify their ancestry before publication."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import fnmatch
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
from urllib.parse import quote

from land_pr import gh, log, operator_gh, save
from landing_policy import ROOT, git

VERSION = re.compile(r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
VERSION_FILES = {
    "bun.lock", "Cargo.lock", "crates/aft/Cargo.toml",
    "crates/aft-tokenizer/Cargo.toml", "crates/secret-rules/Cargo.toml",
    *(f"packages/{name}/package.json" for name in
      ("aft-bridge", "aft-cli", "opencode-plugin", "pi-plugin")),
    *(f"packages/npm/{name}/package.json" for name in
      ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64")),
}


@contextmanager
def worktree(ref: str):
    scratch = ROOT / "target/scratch"
    scratch.mkdir(parents=True, exist_ok=True)
    # Do not leave the operator on a preparation branch, even on SIGTERM or an
    # interrupted CI wait. Git refs/checkpoints survive; disposable files do not.
    def interrupted(signum, frame):
        raise KeyboardInterrupt

    handlers = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        with tempfile.TemporaryDirectory(prefix="aft-release-", dir=scratch) as directory:
            git("worktree", "add", "--detach", directory, ref)
            try:
                yield Path(directory)
            finally:
                git("worktree", "remove", "--force", directory)
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)


def remote_tag(tag: str) -> str:
    if not tag.startswith("v") or VERSION.fullmatch(tag) is None:
        raise ValueError("retry base must be a vX.Y.Z release tag")
    refs = dict(line.split()[::-1] for line in
                git("ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}").splitlines())
    oid = refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}")
    if not oid or oid != git("rev-parse", f"{tag}^{{commit}}"):
        raise ValueError(f"base tag {tag} is absent on origin or differs from its remote peeled SHA")
    return oid


def ancestor(base: str, head: str) -> bool:
    return subprocess.run(["git", "merge-base", "--is-ancestor", base, head],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def patch_id(oid: str) -> str:
    parents = git("rev-list", "--parents", "-n", "1", oid).split()
    if len(parents) != 2:
        raise ValueError(f"fix {oid} must be a non-merge commit")
    diff = git("diff", f"{oid}^", oid, "--binary", "--no-ext-diff")
    value = git("patch-id", "--stable", input=diff)
    if not value:
        raise ValueError(f"fix {oid} is empty")
    return value.split()[0]


def fix_notes(tag: str, version: str, fixes: list[str]) -> str:
    """Notes for a retry: the failed tag's approved notes with its version
    renamed to the new one, plus one line per fix. Provenance checks constrain
    code only, so the verifier requires the notes to exist but not this text."""
    notes = git("show", f"{tag}:.alfonso/release-notes/{tag}.md")
    if not notes.strip():
        raise ValueError("failed tag has no approved release notes")
    return notes.replace(tag[1:], version).rstrip() + "\n\n" + "\n".join(
        f"- {git('show', '-s', '--format=%s', fix)} ({fix[:12]})" for fix in fixes) + "\n"


def verify_lineage(oid: str, line: str, seen: set[str] | None = None) -> None:
    seen = set() if seen is None else seen
    if oid in seen:
        raise ValueError("cyclic release provenance")
    seen.add(oid)
    message = git("show", "-s", "--format=%B", oid)
    match = re.fullmatch(r"release: v([0-9]+\.[0-9]+\.[0-9]+)\n+Release-retry-from: (v[0-9]+\.[0-9]+\.[0-9]+)\s*", message)
    if not match:
        raise ValueError("release head is not a version/notes retry bump")
    version, tag = match.groups()
    prior = VERSION.fullmatch(tag)
    assert prior is not None
    major, minor, patch = map(int, prior.groups())
    if line != f"{major}.{minor}" or version != f"{major}.{minor}.{patch + 1}":
        raise ValueError("retry must increment only the failed tag's patch version")
    base = remote_tag(tag)
    if not ancestor(base, "origin/main"):
        verify_lineage(base, line, seen)
    if not ancestor(base, oid):
        raise ValueError("release retry does not descend from its base tag")
    commits = git("rev-list", "--reverse", f"{base}..{oid}").splitlines()
    fixes = []
    for pick in commits[:-1]:
        trailer = re.findall(r"^\(cherry picked from commit ([0-9a-f]{40})\)$",
                             git("show", "-s", "--format=%B", pick), re.MULTILINE)
        if len(trailer) != 1 or not ancestor(trailer[0], "origin/main") or patch_id(pick) != patch_id(trailer[0]):
            raise ValueError(f"{pick} is not an unchanged cherry-pick of a fix already on main")
        fixes.append(trailer[0])
    if not fixes or len(git("rev-list", "--parents", "-n", "1", oid).split()) != 2:
        raise ValueError("retry requires fixes followed by one non-merge version/notes bump")
    for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
        if json.loads(git("show", f"{oid}:packages/{package}/package.json"))["version"] != version:
            raise ValueError(f"retry head has not bumped {package} to {version}")
    notes_path = f".alfonso/release-notes/v{version}.md"
    changed = set(git("diff-tree", "--no-commit-id", "--name-only", "-r", oid).splitlines())
    if not changed <= VERSION_FILES | {notes_path} or notes_path not in changed:
        raise ValueError("retry bump changes files other than versions and notes")
    for path in changed - {notes_path}:
        before = git("show", f"{oid}^:{path}")
        after = git("show", f"{oid}:{path}")
        if before.replace(tag[1:], version) != after:
            raise ValueError(f"retry bump changes more than the version in {path}")
    if not git("show", f"{oid}:{notes_path}").strip():
        raise ValueError("retry notes are empty")


def verify(oid: str) -> str:
    for ref in git("for-each-ref", "--format=%(refname:short)", "refs/remotes/origin/release/").splitlines():
        match = re.fullmatch(r"origin/release/([0-9]+\.[0-9]+)", ref)
        if match and ancestor(oid, ref):
            verify_lineage(oid, match[1])
            return ref.removeprefix("origin/")
    raise ValueError("commit is not on a fetched origin/release/X.Y branch")


def preflight(repo: str, branch: str) -> None:
    tests = Path(".github/workflows/tests.yml").read_text()
    push = re.search(r"^  push:\n(.*?)(?=^  [a-z_]+:|\Z)", tests, re.MULTILINE | re.DOTALL)
    if not push or not re.search(r"^\s+-\s+[\"']?release/\*\*?[\"']?\s*$", push[1], re.MULTILINE):
        raise ValueError("tests.yml must enable release/** in on.push.branches; dispatch skips required Landing policy")
    release = Path(".github/workflows/release.yml").read_text()
    if "refs/heads/release/" not in release:
        raise ValueError("release.yml gate must fetch refs/heads/release/*:refs/remotes/origin/release/* before checking retry tags")
    if "lib/release_retry.py" not in Path("scripts/release-ci-gate.sh").read_text():
        raise ValueError("retry branch needs the release gate with release-branch provenance support; cherry-pick the tooling fix too")
    rules = json.loads(operator_gh("api", f"repos/{repo}/rules/branches/{quote(branch, safe='')}"))
    blocked = {rule["type"] for rule in rules} & {
        "creation", "update", "pull_request", "merge_queue", "required_status_checks", "required_signatures", "workflows"}
    owner, name = repo.split("/")
    classic = json.loads(operator_gh("api", "graphql", "-f", "query=" +
        'query { repository(owner: "' + owner + '", name: "' + name + '") { branchProtectionRules(first: 100) {'
        ' pageInfo { hasNextPage } nodes { pattern requiresApprovingReviews requiresStatusChecks restrictsPushes requiresCommitSignatures lockBranch } } } }'))
    protection = classic["data"]["repository"]["branchProtectionRules"]
    if protection["pageInfo"]["hasNextPage"]:
        raise ValueError("cannot prove release branch push policy: more than 100 classic protection rules")
    for rule in protection["nodes"]:
        if fnmatch.fnmatchcase(branch, rule["pattern"]):
            blocked.update(key for key, value in rule.items() if key != "pattern" and value)
    if blocked:
        raise ValueError(f"{branch} blocks direct retry pushes ({', '.join(sorted(blocked))}); an administrator must exclude release/* from these requirements; no merge queue may apply")


def run(arguments: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--retry-from", required=True)
    parser.add_argument("version")
    parser.add_argument("--fix", action="append", required=True, help="one non-merge commit already on origin/main; repeat in cherry-pick order")
    args = parser.parse_args(arguments)
    tag, version = args.retry_from, args.version
    previous = VERSION.fullmatch(tag)
    if previous is None or not tag.startswith("v"):
        raise ValueError("retry base must be vX.Y.Z")
    major, minor, patch = map(int, previous.groups())
    if version != f"{major}.{minor}.{patch + 1}":
        raise ValueError("retry version must be the failed tag's next patch")
    branch = f"release/{major}.{minor}"
    git("fetch", "--quiet", "origin", "+refs/heads/main:refs/remotes/origin/main", f"refs/tags/{tag}:refs/tags/{tag}")
    base = remote_tag(tag)
    fixes = [git("rev-parse", "--verify", f"{fix}^{{commit}}") for fix in args.fix]
    if len(set(fixes)) != len(fixes):
        raise ValueError("fix commits must be distinct")
    for fix in fixes:
        if not ancestor(fix, "origin/main"):
            raise ValueError(f"fix {fix} is not already on origin/main")
        if ancestor(fix, base):
            raise ValueError(f"fix {fix} is already in the failed tag")
        patch_id(fix)
    if not ancestor(base, "origin/main"):
        verify_lineage(base, f"{major}.{minor}")
    repo = gh("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    from release_queue import state_path, tag_release, versions_at
    path = state_path(version).resolve()
    identity = {"retry_from": tag, "fixes": fixes, "branch": branch, "version": version}
    state = json.loads(path.read_text()) if path.exists() else None
    if state and any(state.get(key) != value for key, value in identity.items()):
        raise ValueError("retry checkpoint differs; refusing to replace its source")
    caller = Path.cwd()
    with worktree(base) as directory:
        try:
            os.chdir(directory)
            if state:
                if git("rev-parse", branch) != state["release_head"]:
                    raise ValueError("retry branch changed outside its checkpoint")
                git("checkout", branch)
                preflight(repo, branch)
            else:
                local = subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"])
                if local.returncode == 0 and git("rev-parse", branch) != base:
                    raise ValueError(f"existing {branch} is not at the failed tag; refusing to reset it")
                for fix in fixes:
                    git("cherry-pick", "-x", fix)
                preflight(repo, branch)
                notes = Path(f".alfonso/release-notes/v{version}.md")
                notes.write_text(fix_notes(tag, version, fixes))
                for command in (["node", "scripts/version-sync.mjs", version],
                                ["node", "scripts/check-release-notes.mjs", str(notes)],
                                ["bun", "install", "--silent"],
                                ["cargo", "update", "-w", "--offline", "--quiet"]):
                    subprocess.run(command, stdout=sys.stderr, check=True)
                git("add", "--update")
                git("add", str(notes))
                git("commit", "-m", f"release: v{version}", "-m", f"Release-retry-from: {tag}")
                git("checkout", "-B", branch)
                state = {**identity, "release_head": git("rev-parse", "HEAD"), "phase": "prepared"}
                save(path, state)
            oid = state["release_head"]
            verify_lineage(oid, f"{major}.{minor}")
            versions_at(oid, version)
            try:
                git("push", "origin", f"HEAD:refs/heads/{branch}")
            except subprocess.CalledProcessError as error:
                raise ValueError(f"push to {branch} refused; inspect release/* creation/update, required-PR/check and merge-queue protection; no tag created") from error
            env = {**os.environ, "REPO": repo, "GITHUB_REPOSITORY": repo,
                   "WATCH_CI_WORKFLOW": "tests.yml", "WATCH_CI_EVENT": "push", "WATCH_CI_BRANCH": branch}
            subprocess.run([str(ROOT / "scripts/watch-ci.sh"), oid], env=env, stdout=sys.stderr, check=True)
            subprocess.run([str(ROOT / "scripts/release-ci-gate.sh"), oid], env=env, stdout=sys.stderr, check=True)
            tag_release(version, oid)
            state["tagged"] = True
            save(path, state)
            log(f"released v{version} from {branch} at {oid}")
        finally:
            os.chdir(caller)
    return 0


if __name__ == "__main__":
    try:
        if len(sys.argv) != 4 or sys.argv[1] != "verify":
            raise ValueError("usage: release_retry.py verify OID REPO")
        print(verify(sys.argv[2]))
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        log(f"release provenance REFUSED: {error}")
        sys.exit(1)
