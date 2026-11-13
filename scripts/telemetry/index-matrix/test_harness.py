"""Focused, offline harness tests. Run with unittest discover in this directory."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    assert spec is not None and spec.loader is not None
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    spec.loader.exec_module(value)
    return value


RUN = module("matrix_run_test", "run.py")
TABLES = module("matrix_tables_test", "tables.py")
PIN = module("matrix_pin_test", "pin.py")


class HarnessTests(unittest.TestCase):
    def setUp(self):
        scratch = HERE.parents[2] / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=scratch)
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def test_environment_isolated_and_does_not_inherit_embedding_overrides(self):
        with patch.dict(os.environ, {"HOME": "/private/home", "AFT_STORAGE_DIR": "/private/storage",
                                     "AFT_CALLGRAPH_SHAPE_CENSUS_JSON": "/private/census",
                                     "FASTEMBED_CACHE_DIR": "/private/model", "HF_HOME": "/private/hf",
                                     "ORT_DYLIB_PATH": "/opt/runtime.so"}):
            env = RUN.isolated_environment(self.root)
        self.assertEqual(env["HOME"], str(self.root / "home"))
        self.assertEqual(env["AFT_STORAGE_DIR"], str(self.root / "storage"))
        self.assertEqual(env["FASTEMBED_CACHE_DIR"], str(self.root / "models"))
        self.assertNotIn("AFT_CALLGRAPH_SHAPE_CENSUS_JSON", env)
        self.assertEqual(env["ORT_DYLIB_PATH"], "/opt/runtime.so")
        self.assertEqual((self.root / "xdg/runtime").stat().st_mode & 0o777, 0o700)

    def test_config_builds_only_requested_index_and_fastembed(self):
        class Fake:
            def request(self, command, **kwargs):
                self.arguments = kwargs
                return {"success": True}
        for mode, enabled in (("text", "trigram"), ("meaning", "semantic")):
            client = Fake()
            RUN.configure(client, self.root, {"AFT_STORAGE_DIR": str(self.root / "storage")}, mode, 10)
            config = json.loads(client.arguments["config"][0]["doc"])
            self.assertEqual(config["indexes"], {name: name == enabled for name in ("trigram", "semantic", "callgraph")})
            self.assertEqual(config["semantic"]["backend"], "fastembed")
            self.assertFalse(config["inspect"]["enabled"])
            self.assertTrue(client.arguments["_bypass_size_limits"])

    def test_missing_index_state_is_not_ready(self):
        self.assertEqual(RUN.states({}, "meaning"), {"semantic_index": "unknown"})

    def test_readiness_waits_for_configured_semantic_worker_installation(self):
        class Fake:
            polls = 0
            def request(self, command, **kwargs):
                if command == "memory.census":
                    return {"success": True, "process": {"rss_bytes": 123}, "roots": {}}
                self.polls += 1
                return {"success": True, "features": {"semantic_search": True},
                        "semantic_index": {"status": "disabled" if self.polls == 1 else "ready", "backend": "fastembed"}}
        client = Fake()
        with patch.object(RUN.time, "sleep"):
            status = RUN.wait_ready(client, "meaning", RUN.time.monotonic() + 10, self.root)
        self.assertEqual(client.polls, 2)
        self.assertEqual(status["semantic_index"]["status"], "ready")

    def test_census_component_maxima_only_label_byte_fields_as_bytes(self):
        path = self.root / "census.jsonl"
        path.write_text(json.dumps({"response": {"success": True, "process": {"rss_bytes": 101, "allocator_observation_age_ms": 2},
                                                "roots": {"/project": {"planes": {"search": 17, "semantic": 3}}}}}) + "\n")
        observations, maxima = RUN.census_observed_max(path)
        self.assertEqual(observations, 1)
        self.assertEqual(maxima, {"process.rss_bytes": 101, "root.planes.search": 17, "root.planes.semantic": 3})

    def test_shape_rejects_estimated_or_duplicate_groups(self):
        value = {"schema_version": 1, "top_callers": [], "groups": [{"language": "lua", "member": "get", "sites": 7, "targets": 2}]}
        self.assertIs(RUN.validate_shape(value), value)
        value["groups"].append(value["groups"][0])
        with self.assertRaises(ValueError):
            RUN.validate_shape(value)
        value["groups"] = [{"language": "lua", "member": "get", "sites": 7.2, "targets": 2}]
        with self.assertRaises(ValueError):
            RUN.validate_shape(value)

    def test_resume_requires_matching_measurement_identity(self):
        for mode in ("text", "meaning"):
            for filename in ("build.json", "ready-status.json", "status.jsonl", "memory-census.jsonl", "phase-memory.csv"):
                RUN.save(self.root / "attempt" / mode / filename, {})
        RUN.save(self.root / "result.json", {"completed": True, "attempt": "attempt", "fingerprint": {"sha": "a"}})
        self.assertTrue(RUN.completed(self.root, {"sha": "a"}))
        self.assertFalse(RUN.completed(self.root, {"sha": "b"}))
        (self.root / "attempt/text/build.json").unlink()
        with self.assertRaisesRegex(ValueError, "lost raw artifacts"):
            RUN.completed(self.root, {"sha": "a"})
        RUN.save(self.root / "result.json", {"completed": False, "fingerprint": {"sha": "a"}})
        self.assertFalse(RUN.completed(self.root, {"sha": "a"}))

    def test_nearest_rank_keeps_raw_integer_nanoseconds(self):
        self.assertEqual(TABLES.percentile([9, 1, 5, 3], 50), 3)
        self.assertEqual(TABLES.percentile(list(range(1, 101)), 95), 95)
        self.assertEqual(TABLES.percentile([1234567890123456789], 99), 1234567890123456789)
        self.assertIsNone(TABLES.percentile([], 50))

    def test_tables_preserve_missing_as_empty_and_one_row_per_repository(self):
        manifest = {"repositories": [{"repository": "owner/repo", "commit": "a" * 40, "languages": ["Lua"]}]}
        counts = TABLES.generate(manifest, self.root / "results", self.root / "tables")
        self.assertEqual(counts, (1, 1, 6))
        value = json.loads((self.root / "tables/tables.json").read_text())
        self.assertIsNone(value["repositories"][0]["text_vmhwm_bytes"])
        self.assertIsNone(value["call_shape"][0]["unknown_sites"])
        self.assertEqual(value["repositories"][0]["query_outcome"], "not_measured")

    def test_tables_normalize_language_and_do_not_round_groups(self):
        directory = self.root / "results/owner--repo"
        RUN.save(directory / "attempt/full/call-shape.json", {"groups": [
            {"language": "lua", "member": "get", "sites": 11, "targets": 9},
            {"language": "lua", "member": "set", "sites": 2, "targets": 1}]})
        RUN.save(directory / "result.json", {"attempt": "attempt", "call_shape": {"outcome": "measured", "artifact": "attempt/full/call-shape.json"}})
        TABLES.generate({"repositories": [{"repository": "owner/repo", "commit": "a" * 40, "languages": ["Lua"]}]},
                        self.root / "results", self.root / "tables")
        value = json.loads((self.root / "tables/tables.json").read_text())["call_shape"]
        self.assertEqual(len(value), 1)
        self.assertEqual(value[0]["unknown_sites"], 13)
        self.assertEqual(value[0]["distinct_language_member_groups"], 2)
        self.assertEqual(value[0]["sites_per_group_p50"], 2)
        self.assertEqual(value[0]["sites_per_group_p99"], 11)

    def test_pinning_uses_tree_at_observed_sha_and_canonical_gitlab(self):
        outputs = ["ref: refs/heads/main\tHEAD\n" + "a" * 40 + "\tHEAD\n", "", ""]
        self.root.joinpath("gitlab-org--gitlab").mkdir()
        tree = b"100644 blob abc\tlib/main.rb\x00100644 blob def\ttest/test.rb\x00160000 commit ghi\tsubmodule\x00"
        with patch.object(PIN, "git", side_effect=outputs) as git, patch.object(PIN.subprocess, "check_output", return_value=tree) as listing:
            result = PIN.pin("gitlab-org/gitlab", ["Ruby"], self.root)
        self.assertEqual(git.call_args_list[0].args[2], "https://gitlab.com/gitlab-org/gitlab.git")
        self.assertEqual(listing.call_args.args[0][-1], "a" * 40)
        self.assertEqual(result["file_counts_by_language"], {"Ruby": 2})
        self.assertEqual(result["submodule_count"], 1)
        self.assertIsNone(result["checkout_bytes"])

    def test_cgroup_joins_before_exec_and_disables_swap(self):
        parent = self.root / "cgroups"
        parent.mkdir()
        with patch.object(RUN.subprocess, "Popen") as spawn:
            client = RUN.Client(Path("/opt/aft"), self.root, {}, self.root / "stderr.log", 1024, parent)
        self.addCleanup(client.log.close)
        self.assertEqual((client.cgroup / "memory.max").read_text(), "1024")
        self.assertEqual((client.cgroup / "memory.swap.max").read_text(), "0")
        command = spawn.call_args.args[0]
        self.assertIn("exec", command[2])
        self.assertEqual(command[-1], "/opt/aft")


if __name__ == "__main__":
    unittest.main()
