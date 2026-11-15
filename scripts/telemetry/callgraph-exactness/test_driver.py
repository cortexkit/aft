"""Focused comparer and edit-script tests; no network or Cargo required."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("exactness_run", Path(__file__).with_name("run.py"))
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class CompareTests(unittest.TestCase):
    def fixture(self, root):
        (root / "sections").mkdir(parents=True)
        RUN.save(root / "files.json", [{"path": "a.ts", "record": "record", "fragment": "fragment"}])
        RUN.save(root / "health.json", {"stored_slices_failures": [], "full_resolutions": 1})
        for name in ("sections.json", "resolutions.bin", "queries.jsonl", "dead-code.json", "dead-code-findings.json", "sections/0304-0008.bin"):
            (root / name).write_bytes(b"reference bytes")

    def test_identical_artifacts_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = Path(directory) / "a", Path(directory) / "b"
            self.fixture(a)
            self.fixture(b)
            RUN.compare(a, b)

    def test_changed_fragment_names_file(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = Path(directory) / "a", Path(directory) / "b"
            self.fixture(a)
            self.fixture(b)
            RUN.save(b / "files.json", [{"path": "a.ts", "record": "record", "fragment": "broken"}])
            with self.assertRaisesRegex(ValueError, "a.ts: published fragment-key"):
                RUN.compare(a, b)

    def test_every_posting_kind_is_compared(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = Path(directory) / "a", Path(directory) / "b"
            self.fixture(a)
            self.fixture(b)
            for key_space in range(1, 12):
                name = f"sections/0304-{key_space:04x}.bin"
                (a / name).write_bytes(b"reference bytes")
                (b / name).write_bytes(b"different bytes")
                with self.assertRaisesRegex(ValueError, "bytes differ"):
                    RUN.compare(a, b)
                (b / name).write_bytes(b"reference bytes")

    def test_health_failure_is_not_an_empty_success(self):
        with tempfile.TemporaryDirectory() as directory:
            a, b = Path(directory) / "a", Path(directory) / "b"
            self.fixture(a)
            self.fixture(b)
            RUN.save(b / "health.json", {"stored_slices_failures": ["bad a.ts"], "full_resolutions": 1})
            with self.assertRaisesRegex(ValueError, "damaged publication"):
                RUN.compare(a, b)

    def test_island_second_edit_changes_noncalled_interface_member(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            RUN.island(root, 0)
            first = RUN.island(root, 1)
            self.assertIn("exactness_gate/rename_me.ts", first)
            self.assertIn("Extra()", (root / "exactness_gate/iface.go").read_text())
            second = RUN.island(root, 2)
            self.assertIn("exactness_gate/iface.go", second)
            self.assertIn("func (Thing) Extra()", (root / "exactness_gate/iface.go").read_text())

    def test_native_spans_are_zero_based_and_add_a_distinct_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "method.py").write_text("class Owner:\n    def run(self, value):\n        return value\n\n    def keep(self):\n        pass\n")
            (root / "callee.py").write_text("def callee(value):\n    return value\n")
            for name in ("a.py", "b.py"):
                (root / name).write_text("pass\n")
            row = {"file": "method.py", "owner": "Owner", "language": "python", "symbol": {"start_line": 1, "end_line": 2, "scoped_name": "Owner::run"}}
            callee = {"file": "callee.py", "language": "python", "callers": 9, "symbol": {"start_line": 0, "end_line": 1, "scoped_name": "callee"}}
            plan = {"files": ["a.py", "b.py"], "members": [{"member": "run", "sites": 100, "declarations": [row]}], "top_callees": [callee]}
            with patch.object(RUN.subprocess, "run") as validate:
                validate.return_value.returncode = 0
                native, _ = RUN.native_edits(Path("probe"), root, plan, 1441)
            added, signature = native["edits"]
            self.assertIn("class ExactnessGateNative:\n    def run", added["round1"])
            self.assertNotIn("def run", added["round2"])
            self.assertIn("def keep", added["round2"])
            self.assertIn("callee(value_exactness)", signature["round1"])

    def test_signature_edit_skips_rust_receivers_and_reads_past_the_first_line(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "method.py").write_text("class Owner:\n    def run(self, value):\n        return value\n")
            (root / "only_self.rs").write_text("impl S {\n    pub fn len(&self) -> usize {\n        0\n    }\n}\n")
            (root / "callee.rs").write_text(
                "impl S {\n    pub fn put<'a>(\n        &'a mut self,\n        mut value: u32,\n    ) -> u32 {\n        value\n    }\n}\n")
            for name in ("a.py", "b.py"):
                (root / name).write_text("pass\n")
            row = {"file": "method.py", "owner": "Owner", "language": "python", "symbol": {"start_line": 1, "end_line": 2, "scoped_name": "Owner::run"}}
            only_self = {"file": "only_self.rs", "language": "rust", "callers": 20, "symbol": {"start_line": 1, "end_line": 3, "scoped_name": "S::len"}}
            callee = {"file": "callee.rs", "language": "rust", "callers": 9, "symbol": {"start_line": 1, "end_line": 6, "scoped_name": "S::put"}}
            plan = {"files": ["a.py", "b.py"], "members": [{"member": "run", "sites": 100, "declarations": [row]}], "top_callees": [only_self, callee]}
            with patch.object(RUN.subprocess, "run") as validate:
                validate.return_value.returncode = 0
                native, _ = RUN.native_edits(Path("probe"), root, plan, 1441)
            _, signature = native["edits"]
            self.assertEqual(signature["file"], "callee.rs")
            self.assertEqual(
                signature["round1"],
                "impl S {\n    pub fn put<'a>(\n        &'a mut self,\n        mut value_exactness: u32,\n    ) -> u32 {\n        value\n    }\n}\n")


if __name__ == "__main__":
    unittest.main()
