#!/usr/bin/env python3
"""Keep branch-private caches from evicting the shared main baseline."""

import importlib.util
from contextlib import redirect_stdout
from datetime import datetime
import io
import os
from pathlib import Path
import re
import unittest
from unittest.mock import Mock, call, patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("workflow_gates", Path(__file__).with_name("workflow-gates.py"))
assert SPEC is not None and SPEC.loader is not None
parser = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parser)

PRUNE_SPEC = importlib.util.spec_from_file_location("prune_rust_cache", ROOT / "scripts/prune-rust-cache.py")
assert PRUNE_SPEC is not None and PRUNE_SPEC.loader is not None
prune = importlib.util.module_from_spec(PRUNE_SPEC)
PRUNE_SPEC.loader.exec_module(prune)

MAIN_ONLY = "${{ github.ref == 'refs/heads/main' }}"
MAIN_BACKEND = "${{ github.ref == 'refs/heads/main' && 'READ_WRITE' || 'READ_ONLY' }}"
NON_MAIN_REFS = (
    "refs/heads/train/cache-policy",
    "refs/pull/123/merge",
    "refs/heads/gh-readonly-queue/main/pr-123-0123456789abcdef",
    "refs/heads/ci-speed/release-measurement",
    "refs/tags/v0.59.0",
    "refs/heads/main-lookalike",
)


def steps():
    for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
        workflow = parser.parse_workflow(str(path))
        for job_id, job in workflow.get("jobs", {}).items():
            for step in job.get("steps", []):
                yield path.name, job_id, workflow, job, step


class CacheWritePolicyTests(unittest.TestCase):
    def test_rust_cache_saves_only_on_main_without_restricting_restores(self):
        found = 0
        for file, job_id, _, _, step in steps():
            if not step.get("uses", "").startswith("Swatinem/rust-cache@"):
                continue
            found += 1
            with self.subTest(file=file, job=job_id):
                policy = step.get("with", {}).get("save-if", "")
                self.assertEqual(policy, MAIN_ONLY)
                match = re.fullmatch(r"\$\{\{ github\.ref == '([^']+)' \}\}", policy)
                assert match is not None
                target = match.group(1)
                self.assertEqual(target, "refs/heads/main")
                for ref in NON_MAIN_REFS:
                    self.assertNotEqual(ref, target)
                self.assertNotIn("github.ref", step.get("if", ""))
        self.assertGreater(found, 0)

    def test_sccache_backend_is_read_only_off_main_even_with_env_overrides(self):
        found = 0
        for file, job_id, workflow, job, step in steps():
            if not step.get("uses", "").startswith("mozilla-actions/sccache-action@"):
                continue
            found += 1
            with self.subTest(file=file, job=job_id):
                env = {**workflow.get("env", {}), **job.get("env", {}), **step.get("env", {})}
                self.assertEqual(env.get("SCCACHE_GHA_RW_MODE"), MAIN_BACKEND)
                for later in job["steps"]:
                    overrides = later.get("env", {})
                    if "SCCACHE_GHA_RW_MODE" in overrides:
                        self.assertEqual(overrides["SCCACHE_GHA_RW_MODE"], MAIN_BACKEND)
                    self.assertNotIn("SCCACHE_GHA_RW_MODE=", later.get("run", ""))
                self.assertNotIn("github.ref", step.get("if", ""))
        self.assertGreater(found, 0)

    def test_action_cache_saves_only_on_main_and_keeps_restore_keys_identical(self):
        found = 0
        for file, job_id, _, job, step in steps():
            uses = step.get("uses", "")
            with self.subTest(file=file, job=job_id, action=uses):
                self.assertFalse(uses.startswith("actions/cache@"), "composite cache action saves unconditionally")
                if not uses.startswith("actions/cache/save@"):
                    continue
                found += 1
                match = re.fullmatch(
                    r"\$\{\{ github\.ref == 'refs/heads/main' && steps\.([\w-]+)\.outputs\.cache-hit != 'true' \}\}",
                    step.get("if", ""),
                )
                self.assertIsNotNone(match, "cache save must reject every non-main ref")
                assert match is not None
                restore = next(s for s in job["steps"] if s.get("id") == match.group(1))
                self.assertTrue(restore["uses"].startswith("actions/cache/restore@"))
                self.assertEqual(step["with"]["path"], restore["with"]["path"])
                self.assertEqual(step["with"]["key"], "${{ steps." + match.group(1) + ".outputs.cache-primary-key }}")
                self.assertNotIn("github.ref", restore.get("if", ""))
                self.assertGreater(job["steps"].index(step), job["steps"].index(restore))
        self.assertGreater(found, 0)


class CacheRetentionTests(unittest.TestCase):
    namespace = "aft-rust-v1-" + "a" * 64
    lane = f"{namespace}-linux-archive-test-wasm-hooks-"
    started = datetime.fromisoformat("2026-11-24T12:00:00+00:00")

    def entry(self, cache_id, minute, *, lane=None, environment="aaaaaaaa", ref=prune.MAIN):
        return {
            "id": cache_id,
            "key": f"{lane or self.lane}Linux-x64-{environment}-{cache_id:08x}",
            "ref": ref,
            "created_at": f"2026-11-24T12:{minute:02d}:00Z",
            "last_accessed_at": "2026-11-24T13:00:00Z",
            "size_in_bytes": 100,
        }

    def test_prune_only_older_caches_in_the_same_compatibility_prefix(self):
        entries = [
            self.entry(5, 8), self.entry(1, 2), self.entry(4, 7), self.entry(2, 3),
            self.entry(6, 5, environment="bbbbbbbb"),
            self.entry(7, 6, ref="refs/heads/train/other"),
            self.entry(8, 9, lane=f"{self.namespace}-linux-search-ci-storm-wasm-"),
        ]
        # Supply all entries in arbitrary order, as if the API ignored its
        # prefix/ref filters. No deletion may cross those boundaries anyway.
        api = Mock(side_effect=[{"actions_caches": entries[:4]}, {"actions_caches": entries[4:]},
                                OSError("read-only repository"), None])
        output = io.StringIO()
        with patch.object(prune, "PAGE_SIZE", 4), redirect_stdout(output):
            prune.prune_lane(api, self.lane, self.started)
        self.assertEqual(api.call_args_list, [
            call("GET", "/actions/caches", key=self.lane, ref=prune.MAIN, per_page=4, page=1),
            call("GET", "/actions/caches", key=self.lane, ref=prune.MAIN, per_page=4, page=2),
            call("DELETE", "/actions/caches/2"),
            call("DELETE", "/actions/caches/1"),
        ])
        self.assertIn(f"Kept {entries[0]['key']}", output.getvalue())
        self.assertIn(f"Kept {entries[2]['key']}", output.getvalue())
        self.assertIn(f"Deleted {entries[1]['key']}", output.getvalue())
        self.assertIn("read-only repository", output.getvalue())

    def test_a_prefix_without_a_new_save_is_left_untouched(self):
        api = Mock(return_value={"actions_caches": [self.entry(i, i) for i in (1, 2, 3)]})
        with redirect_stdout(io.StringIO()):
            prune.prune_lane(api, self.lane, datetime.fromisoformat("2026-11-24T12:10:00+00:00"))
        api.assert_called_once_with("GET", "/actions/caches", key=self.lane,
                                    ref=prune.MAIN, per_page=100, page=1)

    def test_non_main_never_queries_the_cache_api(self):
        api = Mock()
        with patch.dict(os.environ, {"GITHUB_REF": "refs/pull/123/merge"}), redirect_stdout(io.StringIO()):
            prune.main(api)
        api.assert_not_called()

    def test_unavailable_api_is_nonfatal(self):
        api = Mock(side_effect=OSError("API unavailable"))
        env = {"GITHUB_REF": prune.MAIN, "RUST_CACHE_NAMESPACE": self.namespace,
               "GITHUB_RUN_ID": "127", "GITHUB_RUN_ATTEMPT": "2"}
        output = io.StringIO()
        with patch.dict(os.environ, env), redirect_stdout(output):
            prune.main(api)
        self.assertEqual(api.call_args_list, [call("GET", "/actions/runs/127/attempts/2"),
                                              call("GET", "/actions/cache/usage")])
        self.assertIn("could not establish run start time: API unavailable", output.getvalue())
        self.assertIn("could not read repository total: API unavailable", output.getvalue())

    def test_workflow_prune_is_main_only_and_not_required(self):
        workflow = ROOT / ".github/workflows/tests.yml"
        jobs = parser.parse_workflow(str(workflow))["jobs"]
        job = jobs["rust-cache-prune"]
        self.assertTrue(parser.is_cache_prune("rust-cache-prune", job))
        self.assertEqual(job["needs"], ["unit", "e2e", "search-quality-binary"])
        self.assertEqual(job["permissions"], {"contents": "read", "actions": "write"})
        self.assertTrue(job["continue-on-error"])
        required = (ROOT / ".github/required-checks.txt").read_text().splitlines()
        self.assertNotIn(job["name"], required)
        self.assertFalse(any("rust-cache-prune" in item for item in parser.scan(str(workflow))[0]))

    def test_each_pruned_lane_has_a_distinct_build_and_image_prefix(self):
        matches = {}
        workflow = parser.parse_workflow(str(ROOT / ".github/workflows/tests.yml"))
        expected = workflow["jobs"]["rust-cache-prune"]["steps"][1]["env"]["RUST_CACHE_NAMESPACE"]
        for file, job_id, _, _, step in steps():
            if not step.get("uses", "").startswith("Swatinem/rust-cache@"):
                continue
            config = step.get("with", {})
            if not config.get("prefix-key", "").startswith("aft-rust-v1-"):
                continue
            lane = config["shared-key"].replace("${{ inputs.cargo_profile }}", "stage")
            with self.subTest(file=file, job=job_id):
                self.assertEqual(config["prefix-key"], expected)
                self.assertEqual(config["env-vars"], "ImageOS")
                self.assertNotIn(lane, matches)
            matches[lane] = job_id
        self.assertEqual(set(matches), set(prune.LANES))


if __name__ == "__main__":
    unittest.main()
