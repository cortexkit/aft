"""Persist release preparation and tag only the exact green landed merge OID."""

from __future__ import annotations

import argparse
from importlib import import_module
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile

landing = import_module("land_pr")
policy = import_module("landing_policy")
gh, log, save = landing.gh, landing.log, landing.save
ROOT, git = policy.ROOT, policy.git


def state_path(version: str) -> Path:
    if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.]+)?(?:\+[a-zA-Z0-9.]+)?", version) is None:
        raise ValueError("invalid release version")
    return Path(git("rev-parse", "--git-common-dir")) / "aft-release" / f"v{version}.json"


def versions_at(oid: str, version: str) -> None:
    for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
        if json.loads(git("show", f"{oid}:packages/{package}/package.json"))["version"] != version:
            raise ValueError(f"{oid}: {package} version does not match release {version}")


def start(version: str) -> None:
    path = state_path(version)
    if path.exists():
        raise ValueError("release already has a preparation checkpoint")
    git("fetch", "--quiet", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    base = git("rev-parse", "origin/main")
    if git("branch", "--show-current") not in ("main", "") or git("rev-parse", "HEAD") != base:
        raise ValueError("a new release must start from current origin/main")
    branch = f"land/release-{version}"
    git("checkout", "-b", branch, base)
    save(path, {"version": version, "branch": branch, "base": base, "phase": "preparing"})


def checkpoint(version: str) -> None:
    branch = f"land/release-{version}"
    head = git("rev-parse", "HEAD")
    if git("branch", "--show-current") != branch:
        raise ValueError("release preparation must be on its dedicated PR branch")
    versions_at(head, version)
    path = state_path(version)
    state = json.loads(path.read_text())
    if state["phase"] != "preparing" or state["branch"] != branch:
        raise ValueError("release already prepared; resume instead of replacing its source")
    state.update(release_head=head, phase="prepared")
    save(path, state)


def resume(version: str) -> None:
    if git("status", "--porcelain"):
        raise ValueError("cannot resume a release with a dirty worktree")
    state = json.loads(state_path(version).read_text())
    branch = f"land/release-{version}"
    if state["version"] != version or state["branch"] != branch:
        raise ValueError("release checkpoint identity mismatch")
    head = git("rev-parse", branch)
    land_path = Path(git("rev-parse", "--git-common-dir")) / "aft-land" / f"release-{version}.json"
    if state["phase"] == "preparing":
        git("merge-base", "--is-ancestor", state["base"], head)
        git("checkout", branch)
        return
    allowed = {state["release_head"]}
    if land_path.exists():
        allowed.add(json.loads(land_path.read_text())["head"])
    if head not in allowed:
        raise ValueError("release branch changed outside its saved preparation/delivery")
    versions_at(head, version)
    git("checkout", branch)


def publish(version: str, legacy: bool) -> str:
    path = state_path(version)
    state = json.loads(path.read_text())
    resume(version)
    command = [str(ROOT / "scripts/land-pr.sh"), f"release-{version}", "--title", f"release: v{version}"]
    if legacy:
        command.append("--legacy-merge")
    oid = subprocess.check_output(command, text=True).strip()
    if re.fullmatch(r"[0-9a-f]{40}", oid) is None:
        raise ValueError("landing did not return an exact final merge OID")
    if state.get("merge_oid") and state["merge_oid"] != oid:
        raise ValueError("release's final merge OID changed across resume")
    state["merge_oid"] = oid
    save(path, state)
    git("fetch", "--quiet", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    git("merge-base", "--is-ancestor", oid, "origin/main")
    git("merge-base", "--is-ancestor", state["release_head"], oid)
    versions_at(oid, version)
    repo = gh("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    env = {**os.environ, "REPO": repo, "GITHUB_REPOSITORY": repo,
           "WATCH_CI_WORKFLOW": "tests.yml", "WATCH_CI_EVENT": "push", "WATCH_CI_BRANCH": "main"}
    gate_command = [str(ROOT / "scripts/release-ci-gate.sh"), oid]
    # Queue evidence is useful only for the exact merged SHA and the full
    # required set. An incomplete or red queue run keeps the main-push wait.
    queue = subprocess.run([*gate_command, "--queue-only"], env=env, stdout=sys.stderr)
    if queue.returncode:
        subprocess.run([str(ROOT / "scripts/watch-ci.sh"), oid], env=env, stdout=sys.stderr, check=True)
        subprocess.run(gate_command, env=env, stdout=sys.stderr, check=True)
    gate = ROOT / "scripts/release-gate-v049.mjs"
    if gate.exists():
        # The merge may include earlier queued PRs. Freeze publication records
        # from that exact final tree, not just the original version-bump head.
        git("checkout", "--detach", oid)
        try:
            scratch = Path("target/scratch")
            scratch.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryDirectory(prefix="aft-release-publication-", dir=scratch) as directory:
                subprocess.run(["node", str(gate), "--prepare", "--snapshot-dir", directory, "--candidate"],
                               stdout=sys.stderr, check=True)
                subprocess.run(["node", str(gate), "--snapshot-dir", directory, "--candidate"],
                               stdout=sys.stderr, check=True)
        finally:
            git("checkout", state["branch"])
    tag_release(version, oid)
    state["tagged"] = True
    save(path, state)
    log(f"released v{version} at final merge {oid}")
    return oid


def tag_release(version: str, oid: str) -> None:
    tag = f"v{version}"
    remote = git("ls-remote", "--tags", "origin", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}")
    remote_refs = {line.split()[1]: line.split()[0] for line in remote.splitlines()}
    remote_oid = remote_refs.get(f"refs/tags/{tag}^{{}}") or remote_refs.get(f"refs/tags/{tag}")
    if remote_oid and remote_oid != oid:
        raise ValueError("remote release tag points at a different commit")
    local = subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/tags/{tag}"], check=False)
    if local.returncode == 0:
        if git("rev-list", "-n", "1", tag) != oid:
            raise ValueError("local release tag points at a different commit")
    else:
        git("tag", "-a", tag, "-m", f"Release {tag}", oid)
    git("push", "origin", f"refs/tags/{tag}")


def run_isolated(arguments: list[str]) -> int:
    if git("status", "--porcelain"):
        raise ValueError("working tree is not clean — commit or stash changes first")
    retry = import_module("release_retry")
    if arguments and arguments[0] == "--retry-from":
        return retry.run(arguments)
    if not arguments or arguments[0] in ("--help", "-h"):
        print("Usage: scripts/release.sh VERSION [--dry] [--legacy-merge]\n"
              "       scripts/release.sh --retry-from vX.Y.Z X.Y.(Z+1) --fix COMMIT [--fix COMMIT ...] [--notes-file PATH]\n"
              "Retry fixes must already be on origin/main. Work happens in a disposable worktree;\n"
              "release/X.Y is pushed directly, checked by Tests, and tagged only when green.\n"
              "Notes reuse the failed tag's approved notes, renamed to the new version, plus each fix's subject;\n"
              "a patch of a published release passes --notes-file with notes for the fixes only.")
        return 0 if arguments else 2
    version = arguments[0]
    if not state_path(version).exists() and git("branch", "--show-current") != "main":
        raise ValueError("a new release must start on main")
    with retry.worktree("HEAD") as directory:
        # Linked worktrees do not inherit ignored dependencies or bridge dist.
        # The release checks and publication validator need both before running.
        for command in (["bun", "install", "--frozen-lockfile"],
                        ["bun", "run", "--cwd", "packages/aft-bridge", "build"]):
            subprocess.run(command, cwd=directory, stdout=sys.stderr, check=True)
        process = subprocess.Popen(["bash", str(ROOT / "scripts/release.sh"), *arguments], cwd=directory,
                                   env={**os.environ, "AFT_RELEASE_WORKTREE": "1"}, start_new_session=True)
        try:
            return process.wait()
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


def main() -> int:
    if len(sys.argv) > 1 and sys.argv[1] == "run":
        try:
            return run_isolated(sys.argv[2:])
        except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
            log(f"release REFUSED: {error}")
            return 1
        except KeyboardInterrupt:
            log("release interrupted; operator checkout unchanged")
            return 130
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=("start", "checkpoint", "resume", "publish", "phase"))
    parser.add_argument("version")
    parser.add_argument("--legacy-merge", action="store_true")
    args = parser.parse_args()
    try:
        if args.operation == "start":
            start(args.version)
        elif args.operation == "phase":
            print(json.loads(state_path(args.version).read_text())["phase"])
        elif args.operation == "checkpoint":
            checkpoint(args.version)
        elif args.operation == "resume":
            resume(args.version)
        else:
            print(publish(args.version, args.legacy_merge))
        return 0
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        log(f"release REFUSED: {error}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
