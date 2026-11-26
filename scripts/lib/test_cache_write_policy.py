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
    namespace = "aft-rust-v1"
    old_hashed_namespace = namespace + "-" + "a" * 64
    lane = "linux-archive-test-wasm-hooks"
    started = datetime.fromisoformat("2026-11-24T12:00:00+00:00")

    def entry(self, cache_id, minute, *, namespace=None, lane=None, environment="aaaaaaaa", ref=prune.MAIN):
        return {
            "id": cache_id,
            "key": f"{namespace or self.namespace}-{lane or self.lane}-Linux-x64-{environment}-{cache_id:08x}",
            "ref": ref,
            "created_at": f"2026-11-24T12:{minute:02d}:00Z",
            "last_accessed_at": "2026-11-24T13:00:00Z",
            "size_in_bytes": 100,
        }

    def test_prune_keeps_two_current_lane_caches_and_removes_superseded_prefixes(self):
        entries = [
            self.entry(5, 8), self.entry(1, 2), self.entry(4, 7), self.entry(2, 3),
            self.entry(6, 5, environment="bbbbbbbb"),
            self.entry(7, 6, ref="refs/heads/train/other"),
            self.entry(8, 9, namespace=self.old_hashed_namespace, lane="linux-search-ci-storm-wasm"),
            self.entry(9, 10, namespace=self.old_hashed_namespace),
            self.entry(10, 11, namespace="aft-rust-v0"),
            self.entry(11, 12, namespace="aft-rust-v2"),
            self.entry(12, 13, lane=self.lane + "-thin"),
        ]
        # A page overlap must not count a cache twice. The listing can contain
        # other lanes or a newer namespace whose run is still in flight.
        api = Mock(side_effect=[
            {"actions_caches": entries[:6]},
            {"actions_caches": [entries[0], *entries[6:11]]},
            {"actions_caches": entries[11:]},
            OSError("read-only repository"), None, None, None, None,
        ])
        output = io.StringIO()
        with patch.object(prune, "PAGE_SIZE", 6), redirect_stdout(output):
            listed = prune.list_main_caches(api)
            prune.prune_lane(api, self.namespace, self.lane, listed, self.started)
        self.assertEqual(api.call_args_list, [
            call("GET", "/actions/caches", key="aft-rust-v", ref=prune.MAIN, per_page=6, page=1),
            call("GET", "/actions/caches", key="aft-rust-v", ref=prune.MAIN, per_page=6, page=2),
            call("GET", "/actions/caches", key="aft-rust-v", ref=prune.MAIN, per_page=6, page=3),
            call("DELETE", "/actions/caches/10"),
            call("DELETE", "/actions/caches/9"),
            call("DELETE", "/actions/caches/6"),
            call("DELETE", "/actions/caches/2"),
            call("DELETE", "/actions/caches/1"),
        ])
        self.assertIn(f"Kept {entries[0]['key']}", output.getvalue())
        self.assertIn(f"Kept {entries[2]['key']}", output.getvalue())
        self.assertIn(f"Deleted {entries[1]['key']}", output.getvalue())
        self.assertIn(f"Deleted {entries[7]['key']}", output.getvalue())
        self.assertIn("read-only repository", output.getvalue())

    def test_a_prefix_without_a_new_save_is_left_untouched(self):
        api = Mock()
        entries = [self.entry(1, 1), self.entry(2, 2), self.entry(3, 11, namespace=self.old_hashed_namespace)]
        with redirect_stdout(io.StringIO()):
            prune.prune_lane(api, self.namespace, self.lane, entries,
                             datetime.fromisoformat("2026-11-24T12:10:00+00:00"))
        api.assert_not_called()

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

    def test_failed_listing_is_nonfatal_and_still_reports_usage(self):
        api = Mock(side_effect=[
            {"run_started_at": "2026-11-24T12:00:00Z"},
            {"actions_caches": [self.entry(3, 3)]},
            OSError("list unavailable"),
            {"active_caches_count": 3, "active_caches_size_in_bytes": 300},
        ])
        env = {"GITHUB_REF": prune.MAIN, "RUST_CACHE_NAMESPACE": self.namespace,
               "GITHUB_RUN_ID": "127", "GITHUB_RUN_ATTEMPT": "2"}
        output = io.StringIO()
        with patch.object(prune, "PAGE_SIZE", 1), patch.dict(os.environ, env), redirect_stdout(output):
            prune.main(api)
        self.assertTrue(all(args[0] == "GET" for args, _ in api.call_args_list))
        self.assertIn("could not list main Rust caches: list unavailable", output.getvalue())
        self.assertIn("Repository cache total: 3 entries, 300 bytes", output.getvalue())

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
        self.assertEqual(expected, "aft-rust-v1")
        for file, job_id, _, _, step in steps():
            if not step.get("uses", "").startswith("Swatinem/rust-cache@"):
                continue
            config = step.get("with", {})
            if not config.get("prefix-key", "").startswith("aft-rust-v"):
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
