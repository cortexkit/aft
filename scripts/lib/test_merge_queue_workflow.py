#!/usr/bin/env python3
"""Merge-queue workflow contracts and descriptor selection on scratch Git refs."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "workflow_gates", Path(__file__).with_name("workflow-gates.py")
)
assert SPEC is not None and SPEC.loader is not None
workflow_gates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_gates)


class MergeQueueWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.workflow = workflow_gates.parse_workflow(str(ROOT / ".github/workflows/tests.yml"))

    def test_required_workflow_runs_for_merge_groups_and_all_main_prs(self) -> None:
        events = self.workflow["on"]
        self.assertEqual(events["merge_group"]["types"], ["checks_requested"])
        self.assertEqual(events["pull_request"]["branches"], ["main"])
        self.assertNotIn("paths", events["pull_request"])
        self.assertNotIn("paths-ignore", events["pull_request"])

    def test_queue_concurrency_is_sha_scoped_and_never_cancels_groups(self) -> None:
        concurrency = self.workflow["concurrency"]
        self.assertEqual(
            concurrency["group"],
            "tests-${{ github.workflow }}-${{ github.event_name }}-${{ github.ref }}-${{ (github.event_name == 'merge_group' || github.ref == 'refs/heads/main') && github.sha || 'latest' }}",
        )
        self.assertEqual(
            concurrency["cancel-in-progress"], "${{ github.event_name != 'merge_group' && github.ref != 'refs/heads/main' }}"
        )

    def test_required_reusable_suites_skip_pr_leaf_jobs_not_callers(self) -> None:
        jobs = self.workflow["jobs"]
        for job_id, name, file in (
            ("unit", "Unit", "_unit-suite.yml"),
            ("e2e", "E2E", "_e2e-suite.yml"),
        ):
            job = jobs[job_id]
            self.assertEqual(job["name"], name)
            self.assertEqual(job["uses"], f"./.github/workflows/{file}")
            self.assertNotIn("if", job)
            called = workflow_gates.parse_workflow(str(ROOT / ".github/workflows" / file))
            self.assertIn("workflow_call", called["on"])
            for nested_id, nested in called["jobs"].items():
                condition = nested.get("if", "")
                if nested_id == "unit-linux-storm-profile-probe":
                    self.assertEqual(job_id, "unit")
                    self.assertEqual(condition, "${{ inputs.storm_profile_probe && github.event_name != 'pull_request' }}")
                    required = (ROOT / ".github/required-checks.txt").read_text().splitlines()
                    self.assertNotIn(f"{name} / {nested['name']}", required)
                    continue
                self.assertIn(condition, (
                    "${{ github.event_name != 'pull_request' }}",
                    "${{ github.event_name != 'pull_request' || inputs.pr_platform_advisory }}",
                    "${{ always() && github.event_name != 'pull_request' }}",
                ))
                self.assertNotIn("github.ref", condition)
        self.assertEqual(jobs["e2e"]["with"]["cargo_profile"], "stage")

    def test_pr_checks_keep_format_lint_types_and_script_guards_without_native_builds(self) -> None:
        job = self.workflow["jobs"]["pr-checks"]
        self.assertEqual(job["if"], "${{ github.event_name == 'pull_request' }}")
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        for command in (
            "cargo fmt --all -- --check",
            "bun run lint",
            "bun run --cwd packages/aft-bridge build",
            "bun run typecheck",
            "python3 -m unittest discover -s scripts/lib -p test_workflow_gates.py -v",
            "python3 -m unittest discover -s scripts/lib -p test_merge_queue_workflow.py -v",
            "bash scripts/train-push.test.sh",
        ):
            self.assertIn(command, commands)
        self.assertNotIn("cargo build", commands)
        self.assertNotIn("cargo test", commands)

    def test_search_quality_prs_validate_descriptors_without_builds_or_corpora(self) -> None:
        jobs = self.workflow["jobs"]
        self.assertEqual(jobs["search-quality-binary"].get("if"), "${{ github.event_name != 'pull_request' }}")
        gate = jobs["search-quality-gate"]
        self.assertEqual(gate["if"], "${{ !cancelled() }}")
        descriptor = next(step for step in gate["steps"] if step.get("name") == "Validate PR search-quality descriptor")
        self.assertEqual(descriptor["if"], "${{ github.event_name == 'pull_request' }}")
        self.assertIn("--base", descriptor["run"])
        self.assertIn("--head", descriptor["run"])
        for step in gate["steps"]:
            if step.get("name") in (
                "Download the shared search-quality AFT",
                "Check the shared search-quality AFT",
                "Provision search-quality corpora at their pinned commits",
                "Evaluate committed search-quality reference",
            ):
                self.assertEqual(step.get("if"), "${{ github.event_name != 'pull_request' }}")
            if step.get("name") == "Build the product binary when the artifact is absent":
                self.assertEqual(step["if"], "${{ github.event_name != 'pull_request' && steps.release-aft.outputs.available != 'true' }}")

    def test_landing_policy_reports_original_commits_on_pr_group_and_main(self) -> None:
        job = self.workflow["jobs"]["landing-policy"]
        self.assertEqual(job["name"], "Landing policy")
        self.assertEqual(job["if"], "${{ github.event_name == 'pull_request' || github.event_name == 'merge_group' || github.event_name == 'push' }}")
        self.assertEqual(job["permissions"], {"contents": "read", "pull-requests": "read"})
        checkout = job["steps"][0]
        self.assertEqual(checkout["with"]["fetch-depth"], "0")
        self.assertEqual(checkout["with"]["ref"], "${{ github.event.pull_request.head.sha || github.sha }}")
        identity = next(step for step in job["steps"] if step.get("name") == "Original PR commit identity and agent attribution")
        self.assertIn("scripts/landing-policy.py", identity["run"])
        aggregate = next(step for step in job["steps"] if step.get("name") == "Governed-docs aggregate input validator")
        self.assertIn("node scripts/release-gate-v049.mjs --check-inputs", aggregate["run"])
        self.assertNotIn("--prepare", aggregate["run"])

    def test_search_gate_pins_queue_base_head_and_explicit_descriptor(self) -> None:
        gate = self.workflow["jobs"]["search-quality-gate"]
        self.assertEqual(gate["if"], "${{ !cancelled() }}")
        step = next(
            step for step in gate["steps"]
            if step.get("name") == "Evaluate committed search-quality reference"
        )
        self.assertEqual(step["env"]["QUEUE_BASE_SHA"], "${{ github.event.merge_group.base_sha }}")
        self.assertEqual(step["env"]["QUEUE_HEAD_SHA"], "${{ github.event.merge_group.head_sha }}")
        run = step["run"]
        self.assertIn('base="$QUEUE_BASE_SHA"', run)
        self.assertIn('"$QUEUE_HEAD_SHA" != "$GITHUB_SHA"', run)
        self.assertIn('python3 scripts/merge-queue-search.py --base "$base" --head "$GITHUB_SHA"', run)
        self.assertIn('--head-ref "$QUEUE_HEAD_REF"', run)
        self.assertIn('--branch "$QUALITY_BRANCH"', run)
        self.assertIn('queue_reference_missing', run)
        self.assertEqual(gate["permissions"]["pull-requests"], "read")


class MergeQueueDescriptorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-merge-queue-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Queue fixture")
        self.git("config", "user.email", "queue-fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.commit_file("README.md", "base\n")
        self.base = self.git("rev-parse", "HEAD")
        self.git("checkout", "-qb", "queue")
        self.prs: dict[str, list[dict]] = {}
        self.tail = 0
        bin_dir = self.repo / ".fake-bin"
        bin_dir.mkdir()
        fake_gh = bin_dir / "gh"
        fake_gh.write_text(
            f"#!{sys.executable}\n"
            "import json, pathlib, sys\n"
            "fixture = json.loads(pathlib.Path('.fixture-prs.json').read_text())\n"
            "if sys.argv[1:2] == ['api'] and '/commits/' in sys.argv[2]:\n"
            "    assert len(sys.argv) == 3, sys.argv\n"
            "    head = sys.argv[2].split('/commits/')[1].split('/')[0]\n"
            "    print(json.dumps(fixture.get(head, [])))\n"
            "elif sys.argv[1:2] == ['api'] and '/pulls/' in sys.argv[2]:\n"
            "    number = int(sys.argv[2].rsplit('/', 1)[1])\n"
            "    pr = next((pr for prs in fixture.values() for pr in prs if pr['number'] == number), None)\n"
            "    print(json.dumps(pr))\n"
            "else:\n"
            "    raise AssertionError(sys.argv)\n"
        )
        fake_gh.chmod(0o755)
        self.env = {**os.environ, "PATH": str(bin_dir) + os.pathsep + os.environ["PATH"]}

    def git(self, *args: str) -> str:
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True).strip()

    def commit_file(self, path: str, text: str) -> None:
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git("add", path)
        self.git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "fixture")

    def member(self, number: int, slice_class: str = "non_ranking", *, descriptor: bool = True, schema: dict | None = None) -> str:
        branch = f"train/member-{number}"
        self.git("checkout", "-qb", branch, self.base)
        if schema is not None:
            self.commit_file("crates/aft/src/subc_tool_schemas.json", json.dumps(schema, indent=2))
        elif slice_class in {"ranking", "engine_unwired"}:
            self.commit_file(f"crates/aft/src/commands/semantic_search/member{number}.rs", "ranking\n")
        else:
            self.commit_file(f"docs/member{number}.md", "delivery\n")
        path = f"benchmarks/aft-search/slice-descriptors/train-member-{number}.json"
        if descriptor:
            value = {
                "slice_class": slice_class,
                "targeted_mechanism": "wrong_lane_nl" if slice_class == "ranking" else "none",
                "kind": "ranking" if slice_class == "ranking" else "harness",
                "fixtures": ["crates/aft/tests/integration/tool_call_parity_test.rs"],
            }
            self.commit_file(path, json.dumps(value))
        head = self.git("rev-parse", "HEAD")
        self.prs[head] = [{"number": number, "head": {"sha": head, "ref": branch},
                          "base": {"ref": "main", "repo": {"full_name": "cortexkit/aft"}}}]
        self.git("checkout", "-q", "queue")
        self.git("-c", "core.hooksPath=/dev/null", "merge", "--no-ff", "-qm", "synthetic merge", branch)
        self.tail = number
        return path

    def select(self, *, base: str | None = None, head: str | None = None, tail: int | None = None) -> subprocess.CompletedProcess[str]:
        (self.repo / ".fixture-prs.json").write_text(json.dumps(self.prs))
        return subprocess.run(
            [sys.executable, str(ROOT / "scripts/merge-queue-search.py"),
             "--base", base or self.base, "--head", head or self.git("rev-parse", "HEAD"),
             "--head-ref", f"refs/heads/gh-readonly-queue/main/pr-{tail or self.tail}-fixture",
             "--repository", "cortexkit/aft", "--plan-only"],
            cwd=self.repo, env=self.env, text=True, capture_output=True, check=False,
        )

    def selected(self, result: subprocess.CompletedProcess[str]) -> dict:
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout.split("merge_queue_search_plan:", 1)[1])

    def test_parallel_non_ranking_members_keep_every_descriptor(self) -> None:
        one = self.member(1)
        two = self.member(2)
        three = self.member(3)
        candidate = self.selected(self.select())
        self.assertEqual(candidate["evaluations"], [one, two, three])
        self.assertEqual([member["number"] for member in candidate["members"]], [1, 2, 3])

    def schema_base(self) -> dict:
        schema = {"search": {"type": "object", "properties": {}}, "zoom": {"type": "object", "properties": {}}}
        self.commit_file("crates/aft/src/subc_tool_schemas.json", json.dumps(schema, indent=2))
        self.base = self.git("rev-parse", "HEAD")
        return schema

    def test_zoom_schema_only_member_and_combined_queue_are_non_ranking(self) -> None:
        schema = self.schema_base()
        schema["zoom"]["description"] = "Batch-only zoom"
        one = self.member(1, schema=schema)
        two = self.member(2)
        result = self.select()
        candidate = self.selected(result)
        self.assertEqual(candidate["evaluations"], [one, two])
        self.assertEqual([member["slice_class"] for member in candidate["members"]], ["non_ranking", "non_ranking"])
        self.assertIn("other_tools_only_or_formatting", result.stderr)

    def test_search_schema_only_member_and_combined_queue_stay_ranking(self) -> None:
        schema = self.schema_base()
        schema["search"]["description"] = "New search guidance"
        ranking = self.member(1, "ranking", schema=schema)
        self.member(2)
        result = self.select()
        candidate = self.selected(result)
        self.assertEqual(candidate["evaluations"], [ranking])
        self.assertEqual(candidate["members"][0]["slice_class"], "ranking")
        self.assertIn("search_or_shared_changed", result.stderr)

    def test_one_ranking_member_retains_its_exact_descriptor(self) -> None:
        self.member(1)
        ranking = self.member(2, "ranking")
        self.member(3, "engine_unwired")
        candidate = self.selected(self.select())
        self.assertEqual(candidate["evaluations"], [ranking])
        self.assertEqual([member["slice_class"] for member in candidate["members"]],
                         ["non_ranking", "ranking", "engine_unwired"])

    def test_two_ranking_members_fail_closed(self) -> None:
        self.member(1, "ranking")
        self.member(2, "ranking")
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("multiple_ranking_queue_members", result.stderr)

    def test_engine_unwired_groups_keep_each_invariance_contract(self) -> None:
        self.member(1)
        one = self.member(2, "engine_unwired")
        two = self.member(3, "engine_unwired")
        self.assertEqual(self.selected(self.select())["evaluations"], [one, two])

    def test_missing_member_descriptor_fails_closed(self) -> None:
        self.member(1, descriptor=False)
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("queue_member_descriptor", result.stderr)

    def test_ambiguous_pr_association_fails_closed(self) -> None:
        self.member(1)
        head = next(iter(self.prs))
        self.prs[head].append({**self.prs[head][0], "number": 99})
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("ambiguous_queue_member", result.stderr)

    def test_moved_pr_head_fails_closed(self) -> None:
        self.member(1)
        next(iter(self.prs.values()))[0]["head"]["sha"] = "a" * 40
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("ambiguous_queue_member", result.stderr)

    def test_queue_ref_must_identify_the_tail_member(self) -> None:
        self.member(1)
        result = self.select(tail=99)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("queue_ref_member_mismatch", result.stderr)

    def test_squash_or_unrecognized_linear_commits_fail_closed(self) -> None:
        self.member(1)
        self.commit_file("unknown.md", "not a queue merge\n")
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("unsupported_queue_topology", result.stderr)

    def test_checked_out_tree_must_equal_the_group_head(self) -> None:
        self.member(1)
        result = self.select(head=self.base)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("queue_checkout_head_mismatch", result.stderr)

    def test_unknown_base_is_an_input_fault(self) -> None:
        self.member(1)
        result = self.select(base="f" * 40)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("unresolvable_queue_diff", result.stderr)

    def test_overwritten_member_descriptor_fails_closed(self) -> None:
        path = self.member(1)
        self.git("checkout", "-qb", "train/member-2", self.base)
        self.commit_file(path, json.dumps({"slice_class": "ranking"}))
        pr_head = self.git("rev-parse", "HEAD")
        self.prs[pr_head] = [{"number": 2, "head": {"sha": pr_head, "ref": "train/member-2"},
                             "base": {"ref": "main", "repo": {"full_name": "cortexkit/aft"}}}]
        self.git("checkout", "-q", "queue")
        self.git("-c", "core.hooksPath=/dev/null", "merge", "--no-ff", "-s", "ours", "-qm", "synthetic merge", "train/member-2")
        self.commit_file(path, json.dumps({"slice_class": "ranking"}))
        # Overwrite within a merge commit so topology remains a valid queue prefix.
        self.git("reset", "--soft", "HEAD~1")
        self.git("-c", "core.hooksPath=/dev/null", "commit", "--amend", "--no-edit", "-q")
        self.tail = 2
        result = self.select()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("queue_descriptor_overwritten", result.stderr)


class MergeQueueEvaluationTests(unittest.TestCase):
    def test_score_replay_occurs_once_and_other_descriptors_use_that_score(self) -> None:
        spec = importlib.util.spec_from_file_location("merge_queue_search", ROOT / "scripts/merge-queue-search.py")
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        candidate = {"base": "a" * 40, "head": "b" * 40,
                     "evaluations": ["one.json", "two.json", "three.json"]}
        with patch.object(module.subprocess, "run") as run:
            module.evaluate(candidate, "evaluate")
        self.assertEqual(run.call_count, 3)
        for call in run.call_args_list:
            self.assertTrue(call.kwargs["check"])
        first, second, third = [call.args[0] for call in run.call_args_list]
        self.assertIn("--search-quality", first)
        self.assertIn("one.json", first)
        for command, descriptor in ((second, "two.json"), (third, "three.json")):
            self.assertIn("--score", command)
            self.assertIn(descriptor, command)
            self.assertIn(candidate["head"], command)
            self.assertNotIn("--search-quality", command)



class SkewedQueueHistoryTests(unittest.TestCase):
    """Merge-queue commits carry committer dates out of order. Ranges must follow
    ancestry, not dates: one queue group reported main's own merge commit as a
    PR commit because `git rev-list base..head` stopped its walk on dates."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-skewed-queue-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Queue fixture")
        self.git("config", "user.email", "queue-fixture@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.commit("01:00:00", "root")
        m1 = self.commit("02:00:00", "m1")
        self.merged_main = self.commit("03:00:00", "merged main commit")
        self.pr_base = self.commit("03:10:00", "pr base")
        self.git("checkout", "-qb", "pr")
        self.pr_head = self.commit("10:00:00", "pr commit")
        self.git("checkout", "-qb", "side", m1)
        self.commit("02:30:00", "side")
        self.git("checkout", "-q", "main")
        # Later main commits dated before everything the PR sits on, more of them
        # than the five extra commits git walks before ending a date-ordered
        # range walk.
        for minute in range(1, 9):
            self.commit(f"00:1{minute}:00", f"queue {minute}")
        self.git("merge", "-q", "--no-ff", "-m", "group base", "side",
                 env=self.dated("00:19:30"))
        self.group_base = self.git("rev-parse", "HEAD")

    def dated(self, clock: str) -> dict:
        stamp = f"2026-01-01T{clock}Z"
        return {**os.environ, "GIT_AUTHOR_DATE": stamp, "GIT_COMMITTER_DATE": stamp}

    def git(self, *args: str, env: dict | None = None) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.repo, env=env, text=True, check=True, capture_output=True
        ).stdout.strip()

    def commit(self, clock: str, message: str) -> str:
        self.git("commit", "-q", "--allow-empty", "-m", message, env=self.dated(clock))
        return self.git("rev-parse", "HEAD")

    def in_repo(self):
        previous = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, previous)

    def test_pr_commits_exclude_main_commits_the_group_base_contains(self) -> None:
        # The naive range lists the PR base and an older main commit too.
        naive = self.git("rev-list", f"{self.group_base}..{self.pr_head}").splitlines()
        self.assertIn(self.merged_main, naive)
        sys.path.insert(0, str(ROOT / "scripts/lib"))
        import landing_policy

        self.in_repo()
        self.assertEqual(landing_policy.commits_between(self.group_base, self.pr_head), [self.pr_head])

    def test_first_parent_chain_stops_exactly_at_base(self) -> None:
        queue = import_queue_search()
        self.in_repo()
        chain = queue.first_parent_between(self.pr_base, self.group_base)
        self.assertEqual(len(chain), 9)
        self.assertEqual(chain[-1], self.group_base)
        self.assertNotIn(self.pr_base, chain)
        with self.assertRaises(queue.InputFault):
            queue.first_parent_between(self.pr_head, self.group_base)


class PullRequestBaseBehindMergedMainTest(unittest.TestCase):
    """A PR event's base SHA can predate a main the branch later merged in.
    Main's own landed commits must not count as the PR's commits."""

    COAUTHOR = "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-pr-base-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Owner")
        self.git("config", "user.email", "1+owner@users.noreply.github.com")
        self.git("config", "commit.gpgsign", "false")
        self.commit("root")
        self.event_base = self.commit("main before the PR")
        self.git("checkout", "-qb", "land/feature")
        self.commit(f"agent change\n\n{self.COAUTHOR}")
        self.git("checkout", "-q", "main")
        # Landed later on main by the owner, without the agent trailer.
        self.landed = self.commit("owner change landed on main")
        self.git("update-ref", "refs/remotes/origin/main", self.landed)
        self.git("checkout", "-q", "land/feature")
        self.git("merge", "-q", "--no-ff", "-m", f"Merge main\n\n{self.COAUTHOR}", "main")
        self.head = self.git("rev-parse", "HEAD")

    def git(self, *args: str) -> str:
        # Agent shells inject a commit hook (GIT_CONFIG_* plus AFT_GIT_CO_AUTHOR)
        # that adds the agent trailer to every commit; this fixture must control
        # which commits carry it.
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("GIT_CONFIG_") and key != "AFT_GIT_CO_AUTHOR"}
        return subprocess.run(
            ["git", "-c", "core.hooksPath=/dev/null", *args],
            cwd=self.repo, env=env, text=True, check=True, capture_output=True,
        ).stdout.strip()

    def commit(self, message: str) -> str:
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def payload(self) -> dict:
        return {"pull_request": {"base": {"sha": self.event_base},
                                 "head": {"sha": self.head, "ref": "land/feature"}}}

    def test_main_commits_merged_into_the_branch_are_not_the_prs(self) -> None:
        sys.path.insert(0, str(ROOT / "scripts/lib"))
        import landing_policy

        previous = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, previous)
        # The agent commit and the merge commit are the PR's; main's is not.
        self.assertEqual(landing_policy.check_event("pull_request", self.payload()), 2)

    def test_without_fetched_main_the_event_base_alone_still_refuses(self) -> None:
        sys.path.insert(0, str(ROOT / "scripts/lib"))
        import landing_policy

        self.git("update-ref", "-d", "refs/remotes/origin/main")
        previous = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, previous)
        with self.assertRaisesRegex(ValueError, "missing alfonso-aft co-author trailer"):
            landing_policy.check_event("pull_request", self.payload())


def import_queue_search():
    spec = importlib.util.spec_from_file_location(
        "merge_queue_search", ROOT / "scripts/merge-queue-search.py"
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()
