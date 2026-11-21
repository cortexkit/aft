"""Focused comparer and edit-script tests; no network or Cargo required."""
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
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

    def test_java_native_edits_splice_parser_byte_ranges(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            declaration = "@Check public <T> T run(\r\n        List<T> input\r\n    ) { return null; }"
            source = f"class Owner {{ int café; {declaration} void keep() {{}} }}\r\n".encode()
            callee = "class Callee { int café; void call(@Check String name) {} }\r\n".encode()
            (root / "Owner.java").write_bytes(source)
            (root / "Callee.java").write_bytes(callee)
            for name in ("A.java", "B.java"):
                (root / name).write_text("class Stub {}\n")
            row = {"file": "Owner.java", "language": "java", "symbol": {"name": "run", "scoped_name": "Owner::run", "end_line": 2, "end_col": 24}}
            top = {"file": "Callee.java", "language": "java", "symbol": {"name": "call", "scoped_name": "Callee::call", "end_line": 0, "end_col": 57}, "callers": 9}
            plan = {"files": ["A.java", "B.java"],
                    "members": [{"member": "run", "sites": 10, "declarations": [{**row, "file": "Busy.java"}, row]}],
                    "top_callees": [{**top, "file": "Busy.java", "callers": 100}, top],
                    "java_reader_limit": 10, "java_file_readers": {"Busy.java": 11, "Owner.java": 6, "Callee.java": 4}}
            start = source.index(b"@Check")
            parameter = callee.index(b"name")
            spans = [json.dumps({"member": [start, start + len(declaration.encode())]}),
                     json.dumps({"parameter": [parameter, parameter + 4]})]
            with (patch.object(RUN.subprocess, "check_output", side_effect=spans) as locate,
                  patch.object(RUN.subprocess, "run") as validate):
                validate.return_value.returncode = 0
                native, _ = RUN.native_edits(Path("probe"), root, plan, 1441)
            self.assertEqual(locate.call_args_list[0].args[0], ["probe", "java-edit-spans", str(root / "Owner.java"), "run", "2", "24"])
            added, signature = native["edits"]
            self.assertEqual(added["round1"], source.decode() + "\nclass ExactnessGateNative {\n" + declaration + "\n}\n")
            self.assertEqual(added["round2"], source.decode().replace(declaration, ""))
            self.assertEqual(signature["round1"], callee.decode().replace("name", "name_exactness"))
            self.assertEqual(validate.call_count, 3)
            self.assertEqual((root / "Owner.java").read_bytes(), source)
            self.assertEqual(native["java_file_readers"], {"Owner.java": 6, "Callee.java": 4})

    def test_refresh_requires_confirmation_of_the_selected_guard_override(self):
        args = SimpleNamespace(cache_bytes=1, sample=1, build_cache_bytes=1, spill_bytes=1,
                               timeout_s=1, disable_refresh_memory_guard=True)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(RUN, "child_process") as spawn:
                spawn.return_value.__enter__.return_value.wait.return_value = 0
                with self.assertRaisesRegex(ValueError, "expected refresh memory guard off"):
                    RUN.probe(args, Path("probe"), "refresh", root, root / "store", root / "unconfirmed", 1)
                self.assertEqual(spawn.call_args.kwargs["env"]["AFT_EXACTNESS_DISABLE_REFRESH_MEMORY_GUARD"], "1")

    def rust_signature_edit(self, first_source):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "method.py").write_text("class Owner:\n    def run(self, value):\n        return value\n")
            (root / "first.rs").write_text(first_source)
            (root / "fallback.rs").write_text("fn fallback(value: u32) {}\n")
            for name in ("a.py", "b.py"):
                (root / name).write_text("pass\n")
            row = {"file": "method.py", "owner": "Owner", "language": "python", "symbol": {"start_line": 1, "end_line": 2, "scoped_name": "Owner::run"}}
            callees = [{"file": name, "language": "rust", "callers": 9, "symbol": {"start_line": 0, "end_line": len(source.splitlines()) - 1, "scoped_name": "callee"}}
                       for name, source in (("first.rs", first_source), ("fallback.rs", "fn fallback(value: u32) {}\n"))]
            plan = {"files": ["a.py", "b.py"], "members": [{"member": "run", "sites": 100, "declarations": [row]}], "top_callees": callees}
            with patch.object(RUN.subprocess, "run") as validate:
                validate.return_value.returncode = 0
                native, _ = RUN.native_edits(Path("probe"), root, plan, 1441)
            return native["edits"][1]

    def test_signature_edit_ignores_braces_and_colons_in_rust_docs_and_attributes(self):
        for prefix in ("/// Example { brace }\n", "/// Arguments:\n", '#[doc = "Example { brace }:"]\n'):
            with self.subTest(prefix=prefix):
                source = prefix + "pub fn callee(value: u32) {}\n"
                signature = self.rust_signature_edit(source)
                self.assertEqual(signature["file"], "first.rs")
                self.assertEqual(signature["round1"], prefix + "pub fn callee(value_exactness: u32) {}\n")

    def test_signature_edit_never_renames_a_lone_rust_receiver(self):
        for receiver in ("self", "mut self", "&self", "&mut self", "&'a self", "self: Box<Self>"):
            with self.subTest(receiver=receiver):
                signature = self.rust_signature_edit(f"pub fn callee<'a>({receiver}) {{}}\n")
                self.assertEqual(signature["file"], "fallback.rs")
                self.assertEqual(signature["round1"], "fn fallback(value_exactness: u32) {}\n")

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
