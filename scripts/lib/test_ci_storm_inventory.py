#!/usr/bin/env python3
"""Guard the optimized storm profile and its normal test inventory."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("storm_inventory", ROOT / "scripts/ci-storm-inventory.py")
assert SPEC is not None and SPEC.loader is not None
inventory = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inventory)
GATE_SPEC = importlib.util.spec_from_file_location("workflow_gates", ROOT / "scripts/lib/workflow-gates.py")
assert GATE_SPEC is not None and GATE_SPEC.loader is not None
gates = importlib.util.module_from_spec(GATE_SPEC)
GATE_SPEC.loader.exec_module(gates)


class StormInventoryTests(unittest.TestCase):
    def test_inventory_rejects_missing_or_debug_ignored_traffic_storm(self):
        names = [inventory.MAIN_TEST, inventory.ACK_TEST, inventory.READ_TEST] + [f"case{i}" for i in range(16)]
        cases = {name: {"ignored": False, "filter-match": {"status": "matches"}} for name in names}
        document = {"rust-suites": {"integration": {"testcases": cases}}}
        with patch("builtins.print"):
            inventory.check_inventory(document)
        cases[inventory.MAIN_TEST]["ignored"] = True
        with self.assertRaises(RuntimeError):
            inventory.check_inventory(document)

    def test_storm_profile_does_not_relax_bounds_or_change_production(self):
        manifest = (ROOT / "Cargo.toml").read_text()
        profile = manifest.split("[profile.ci-storm]\n", 1)[1].split("\n[", 1)[0]
        for setting in ('inherits = "release"', 'opt-level = 3', 'lto = false', 'codegen-units = 16', 'debug-assertions = false'):
            self.assertIn(setting, profile)
        production = manifest.split("[profile.release]\n", 1)[1].split("\n[", 1)[0]
        self.assertIn("lto = true", production)
        self.assertIn("codegen-units = 1", production)
        source = (ROOT / "crates/aft/tests/integration/subc_storm_test.rs").read_text()
        self.assertIn("const BIND_ACK_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);", source)
        self.assertIn("const WATCHER_SLICE_READ_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);", source)


class StormWorkflowTests(unittest.TestCase):
    def test_required_storm_job_uses_release_derived_validation_on_free_runner(self):
        suite = gates.parse_workflow(str(ROOT / ".github/workflows/_unit-suite.yml"))
        job = suite["jobs"]["unit-linux-storm"]
        self.assertEqual(job["name"], "Release storm (Linux)")
        self.assertEqual(job["runs-on"], "ubuntu-22.04")
        self.assertEqual(job["timeout-minutes"], "40")
        self.assertTrue(any(step.get("uses") == "./.github/actions/storm-suite" for step in job["steps"]))
        self.assertEqual(job["env"]["CARGO_PROFILE_CI_STORM_LTO"], "false")

    def test_both_lto_options_run_the_same_normal_inventory(self):
        suite = gates.parse_workflow(str(ROOT / ".github/workflows/_unit-suite.yml"))
        probe = suite["jobs"]["unit-linux-storm-profile-probe"]
        self.assertEqual(probe["if"], "${{ inputs.storm_profile_probe && github.event_name != 'pull_request' }}")
        self.assertEqual(probe["env"]["CARGO_PROFILE_CI_STORM_LTO"], "thin")
        self.assertEqual(probe["runs-on"], "ubuntu-22.04")
        self.assertTrue(any(step.get("uses") == "./.github/actions/storm-suite" for step in probe["steps"]))
        action = gates.parse_workflow(str(ROOT / ".github/actions/storm-suite/action.yml"))
        commands = "\n".join(step.get("run", "") for step in action["runs"]["steps"])
        self.assertIn("--profile ci-storm --locked -p agent-file-tools --test integration --no-run --timings", commands)
        self.assertIn("--profile default --cargo-profile ci-storm", commands)
        self.assertIn("test(subc_storm)", commands)
        self.assertIn("--check-inventory", commands)
        self.assertNotIn("--features", commands)


if __name__ == "__main__":
    unittest.main()
