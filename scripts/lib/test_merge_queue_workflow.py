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
            for nested in called["jobs"].values():
                condition = nested.get("if", "")
                self.assertIn(condition, (
                    "${{ github.event_name != 'pull_request' }}",
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
                "Download the shared release AFT",
                "Check the shared release AFT",
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
            "assert sys.argv[1:4] == ['api', '--paginate', '--slurp']\n"
            "head = sys.argv[4].split('/commits/')[1].split('/')[0]\n"
            "print(json.dumps([json.loads(pathlib.Path('.fixture-prs.json').read_text()).get(head, [])]))\n"
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

    def member(self, number: int, slice_class: str = "non_ranking", *, descriptor: bool = True) -> str:
        branch = f"train/member-{number}"
        self.git("checkout", "-qb", branch, self.base)
        if slice_class in {"ranking", "engine_unwired"}:
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



if __name__ == "__main__":
    unittest.main()
