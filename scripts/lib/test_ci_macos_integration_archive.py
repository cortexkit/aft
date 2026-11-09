#!/usr/bin/env python3
"""Select every enabled non-watcher integration target without rebuilding unit buckets."""

import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location("archive_plan", Path(__file__).resolve().parents[1] / "ci-macos-integration-archive.py")
assert SPEC is not None and SPEC.loader is not None
plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plan)


def metadata():
    return {
        "workspace_members": ["aft", "future"],
        "packages": [
            {"id": "aft", "targets": [
                {"name": "aft", "kind": ["lib"]},
                {"name": "aft", "kind": ["bin"]},
                {"name": "integration", "kind": ["test"]},
                {"name": "engine", "kind": ["test"]},
                {"name": "watcher_integration", "kind": ["test"]},
                {"name": "disabled", "kind": ["test"], "test": False},
            ]},
            {"id": "future", "targets": [{"name": "future_tests", "kind": ["test"]}]},
            {"id": "dependency", "targets": [{"name": "external_tests", "kind": ["test"]}]},
        ],
    }


class MacOSIntegrationArchiveTests(unittest.TestCase):
    def test_all_enabled_workspace_integration_binaries_are_selected(self):
        self.assertEqual(plan.integration_targets(metadata()), ["engine", "future_tests", "integration"])

    def test_archive_and_source_inventory_use_explicit_cargo_test_targets(self):
        for mode in ("archive", "list"):
            args = plan.command(metadata(), mode, "/tmp/archive.tar.zst")
            selected = [args[i + 1] for i, arg in enumerate(args[:-1]) if arg == "--test"]
            self.assertEqual(selected, ["engine", "future_tests", "integration"])
            self.assertNotIn("--tests", args)
            self.assertNotIn("--all-targets", args)
            self.assertIn("--workspace", args)
            self.assertEqual(args[args.index("--features") + 1], "test-hooks")
            self.assertIn(plan.FILTER, args)

    def test_future_integration_targets_are_not_silently_dropped(self):
        data = metadata()
        data["packages"][0]["targets"].append({"name": "new_suite", "kind": ["test"]})
        self.assertIn("new_suite", plan.integration_targets(data))

    def test_empty_or_invalid_plans_fail_instead_of_archiving_nothing(self):
        with self.assertRaisesRegex(ValueError, "no enabled"):
            plan.integration_targets({"workspace_members": [], "packages": []})
        with self.assertRaisesRegex(ValueError, "requires"):
            plan.command(metadata(), "archive")
        with self.assertRaisesRegex(ValueError, "unsupported"):
            plan.command(metadata(), "invalid")


if __name__ == "__main__":
    unittest.main()
