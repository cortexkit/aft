#!/usr/bin/env python3
"""Keep compile-time paths in the layered crates inside their own packages."""
from __future__ import annotations

import argparse
import json
from pathlib import Path, PureWindowsPath
import re
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent
PACKAGED_CRATES = ("aft-edit", "aft-github", "aft-index", "aft-inspect", "aft-shell", "aft-engine", "aft-tools", "aft-subc")
CRATES = ("aft-base", "aft-db", "aft-parse", *PACKAGED_CRATES)
lexer = runpy.run_path(str(ROOT / "scripts/check-file-sizes.py"))
tokens, group_end = lexer["tokens"], lexer["group_end"]
CLOSING = {"(": ")", "[": "]", "{": "}"}


def requires_test(predicate: list[str]) -> bool:
    if predicate == ["test"]:
        return True
    if len(predicate) < 3 or predicate[1] != "(" or predicate[-1] != ")":
        return False
    if predicate[0] not in {"all", "any"}:
        return False
    clauses, start, depth = [], 2, 0
    for index, word in enumerate(predicate[2:-1], start=2):
        if word == "(":
            depth += 1
        elif word == ")":
            depth -= 1
        elif word == "," and depth == 0:
            clauses.append(predicate[start:index])
            start = index + 1
    if start < len(predicate) - 1:
        clauses.append(predicate[start:-1])
    checks = [requires_test(clause) for clause in clauses]
    return bool(checks) and (any(checks) if predicate[0] == "all" else all(checks))


def attributes(items, words):
    index = 0
    while index < len(items):
        if words[index:index + 2] not in (["#", "["], ["#", "!"]):
            index += 1
            continue
        inner = words[index + 1] == "!"
        start = index + 2 if inner else index + 1
        if words[start:start + 1] != ["["]:
            index += 1
            continue
        end = group_end(items, start, "[", "]")
        yield index, start + 1, end - 1, inner
        index = end


def item_start(items, words, index):
    if words[index:index + 1] == ["pub"]:
        index += 1
        if words[index:index + 1] == ["("]:
            index = group_end(items, index, "(", ")")
    while words[index:index + 1] in (["async"], ["unsafe"], ["extern"]):
        index += 1
        if words[index:index + 1] == ["literal"]:
            index += 1
    if words[index:index + 2] == ["const", "fn"]:
        index += 1
    return index


def item_extent(items, words, index):
    kind = words[index]
    cursor = index + 1
    while cursor < len(items):
        word = words[cursor]
        if word == ";":
            return cursor + 1
        if word == "}":
            break
        if word in CLOSING:
            cursor = group_end(items, cursor, word, CLOSING[word])
            if word == "{" and kind in {"mod", "fn"}:
                return cursor
        else:
            cursor += 1
    return index + 1


def literal_path(text: str) -> str:
    raw = re.fullmatch(r'r(#*)"(.*)"\1', text, flags=re.DOTALL)
    value = raw[2] if raw else json.loads(text)
    if not isinstance(value, str):
        raise ValueError("path is not a string")
    return value.replace("\\", "/")


def violations(crate: Path, path: Path, source: str) -> list[str]:
    items = tokens(source)
    words = [item.text for item in items]
    attrs = list(attributes(items, words))
    guards, path_attrs, module_paths = [], [], {}
    offset = 0
    cursor = 0
    while cursor < len(attrs):
        first, _, _, inner = attrs[cursor]
        end = first
        test_only, is_test, paths = False, False, []
        while cursor < len(attrs) and attrs[cursor][0] == end and attrs[cursor][3] == inner:
            _, start, stop, _ = attrs[cursor]
            meta = words[start:stop]
            if meta[:2] == ["cfg", "("] and meta[-1:] == [")"]:
                test_only |= requires_test(meta[2:-1])
            is_test |= meta == ["test"]
            if meta[:1] in (["path"], ["cfg_attr"]):
                paths.extend(index + 2 for index in range(start, stop - 1) if words[index:index + 2] == ["path", "="])
            cursor += 1
            end = stop + 1
        path_attrs.extend(paths)
        if inner:
            if first == offset:
                if test_only:
                    return []
                offset = end
            continue
        item = item_start(items, words, end)
        kind = words[item:item + 1]
        if kind == ["mod"] and paths:
            module_paths[item] = paths
        if (test_only and kind in (["mod"], ["fn"], ["const"], ["static"])) or (is_test and kind == ["fn"]):
            guards.append((first, item_extent(items, words, item)))

    def unit_only(index):
        return any(start <= index < end for start, end in guards)

    def path_dirs(index):
        modules = [i for i in range(index - 2) if words[i] == "mod" and words[i + 2] == "{" and index < item_extent(items, words, i)]
        dirs = {path.parent}
        if modules and path.name not in {"lib.rs", "main.rs", "mod.rs"}:
            dirs = {path.parent / path.stem}
        for module in modules:
            overrides = [literal_path(source[items[p].start:items[p].end]) for p in module_paths.get(module, ())]
            dirs = {directory / name for directory in dirs for name in [words[module + 1], *overrides]}
        return dirs

    def inspect(index, directive, spellings, bases):
        if unit_only(index):
            return None
        line = source.count("\n", 0, items[index].start) + 1
        try:
            if len(spellings) != 1 or words[spellings[0]] != "literal":
                raise ValueError("expected one literal path inside the crate")
            token = items[spellings[0]]
            value = literal_path(source[token.start:token.end])
            if PureWindowsPath(value).drive or Path(value).is_absolute() or any(
                not (base / value).resolve().is_relative_to(crate) for base in bases()
            ):
                raise ValueError(f"path outside the crate: {value}")
        except (ValueError, OSError) as error:
            return f"{path}:{line}: {directive}: {error}"
        return None

    findings = []
    for index, word in enumerate(words):
        if word in {"include_str", "include_bytes"} and words[index + 1:index + 2] == ["!"]:
            start = index + 2
            end = group_end(items, start, words[start], CLOSING[words[start]]) - 1
            if words[end - 1] == ",":
                end -= 1
            findings.append(inspect(index, word + "!", list(range(start + 1, end)), lambda: [path.parent]))
    for index in path_attrs:
        findings.append(inspect(index, "#[path]", [index], lambda: path_dirs(index)))
    return [finding for finding in findings if finding is not None]


def check_crate(crate: Path) -> tuple[int, list[str]]:
    if crate.name in PACKAGED_CRATES:
        # Excluded test modules can share workspace fixtures. The compiler-input
        # audit separately checks the native test and opt-in library inputs.
        names = subprocess.check_output([
            "cargo", "package", "--locked", "-p", crate.name, "--list",
        ], cwd=ROOT, text=True).splitlines()
        if "src/lib.rs" not in names:
            raise ValueError(f"{crate.name} package listing is missing src/lib.rs")
        sources = sorted(crate / name for name in names if name.endswith(".rs"))
    else:
        sources = sorted(path for path in crate.rglob("*.rs")
                         if "target" not in path.relative_to(crate).parts[:-1])
    findings = []
    for path in sources:
        findings.extend(violations(crate, path, path.read_text(encoding="utf-8")))
    return len(sources), findings


class SourcePathTests(unittest.TestCase):
    crate = Path(__file__).resolve().parent / "test-crate"

    def check(self, source):
        return violations(self.crate, self.crate / "src/lib.rs", source)

    def test_rejects_external_paths_by_directive(self):
        for directive, source in [
            ("include_str!", 'const S: &str = include_str!("../../outside.txt");'),
            ("include_bytes!", 'const B: &[u8] = include_bytes!(r#"../../outside.bin"#);'),
            ("#[path]", '#[path = "../../outside.rs"] mod external;'),
            ("#[path]", '#[cfg_attr(unix, path = "../../outside.rs")] mod external;'),
            ("include_str!", '#[cfg(feature = "test-support")] mod test_support { const S: &str = include_str!("../../outside.txt"); }'),
        ]:
            with self.subTest(directive=directive, source=source):
                findings = self.check(source)
                self.assertEqual(len(findings), 1, findings)
                self.assertIn(f"{directive}: path outside the crate", findings[0])

    def test_any_with_test_support_does_not_prove_test_only(self):
        self.assertEqual(len(self.check('''
            #[cfg(any(test, feature = "test-support"))]
            const S: &str = include_str!("../../outside.txt");
        ''')), 1)

    def test_proven_test_items_allow_external_fixtures_only_within_the_item(self):
        source = '''
            #[cfg(all(test, unix))]
            #[path = "../../tests.rs"] mod unit;
            #[cfg(test)] const FIXTURE: &str = include_str!("../../outside.txt");
            #[test] fn round_trip() { let _ = include_bytes!("../../outside.bin"); }
            const S: &str = include_str!("../../outside.txt");
        '''
        findings = self.check(source)
        self.assertEqual(len(findings), 1, findings)
        self.assertIn(":6: include_str!:", findings[0])

    def test_local_paths_and_comment_examples(self):
        self.assertEqual(self.check('''
            // include_str!("../../outside.txt")
            const DOC: &str = r#"#[path = "../../outside.rs"]"#;
            const B: &[u8] = include_bytes!("../tests/local.bin",);
            mod inline { #[path = "../../tests/local.rs"] mod local; }
        '''), [])

    def test_computed_paths_fail_closed(self):
        findings = self.check('const S: &str = include_str!(concat!("..", "/outside.txt"));')
        self.assertEqual(len(findings), 1)
        self.assertIn("expected one literal path inside the crate", findings[0])

    def test_listed_crates_check_packaged_sources_but_not_excluded_test_fixtures(self):
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        for name in ("aft-engine", "aft-tools", "aft-subc"):
            with self.subTest(crate=name), tempfile.TemporaryDirectory(dir=scratch) as temporary:
                self.assertIn(name, CRATES)
                crate = Path(temporary) / name
                source = crate / "src/lib.rs"
                source.parent.mkdir(parents=True)
                external = 'const FIXTURE: &str = include_str!("../../outside.txt");'
                (source.parent / "fixture_tests.rs").write_text(external)
                source.write_text("")
                with mock.patch("subprocess.check_output", return_value="Cargo.toml\nsrc/lib.rs\n") as package:
                    self.assertEqual(check_crate(crate), (1, []))
                    package.assert_called_once_with([
                        "cargo", "package", "--locked", "-p", name, "--list",
                    ], cwd=ROOT, text=True)
                    source.write_text(external)
                    checked, findings = check_crate(crate)
                    self.assertEqual(checked, 1)
                    self.assertEqual(len(findings), 1)
                    self.assertIn("src/lib.rs:1: include_str!: path outside the crate", findings[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    if parser.parse_args().self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(SourcePathTests))
        return 0 if result.wasSuccessful() else 1
    checked = 0
    findings = []
    for name in CRATES:
        crate = ROOT / "crates" / name
        if not (crate / "Cargo.toml").is_file():
            sys.exit(f"crate source paths: missing {crate.relative_to(ROOT)}/Cargo.toml")
        try:
            count, crate_findings = check_crate(crate.resolve())
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            sys.exit(f"crate source paths: {error}")
        checked += count
        findings.extend(crate_findings)
    if findings:
        print("\n".join(findings), file=sys.stderr)
        return 1
    print(f"crate source paths: ok ({checked} Rust files in {len(CRATES)} crates)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
