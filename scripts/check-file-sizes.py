#!/usr/bin/env python3
"""Keep tracked, hand-written Rust and TypeScript/JavaScript files under 5,000 lines.

Rust counts exclude in-file #[cfg(test)] modules; all other lines, including
standalone test files, count. Each pin in scripts/file-size-pins.json records an
oversized file's exact counted lines and its exception reason. Lower its pin when
shrinking the file, and remove the pin once the file is below the limit. Never
raise a pin to accommodate growth; split the file instead.
"""

import argparse
import json
import re
import subprocess
import sys
import unittest
from pathlib import Path
from typing import NamedTuple, Optional


LIMIT = 5_000
REPORT_THRESHOLD = 4_000
ROOT = Path(__file__).resolve().parents[1]
PINS_FILE = ROOT / "scripts/file-size-pins.json"
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')

# Skip rules (all other source, including standalone tests/benches, counts):
# - Only crates/ and packages/: these are AFT's compiled workspace source roots.
# - Only .rs/.ts/.mts/.mjs/.js: data such as generated schema/fixture JSON is not source.
# - node_modules/, vendor/, vendored/: third-party dependencies are not hand-written here.
# - dist/, target/: build outputs must not be counted a second time.
# - test-vectors/: corpus data is an input to tests, not hand-written workspace source.
# - grammars/: imported/generated parser sources belong to the grammar toolchain.
# - Generated-file headers: generator-owned source must be changed at its generator.
SOURCE_ROOTS = {"crates", "packages"}
SOURCE_SUFFIXES = {".rs", ".ts", ".mts", ".mjs", ".js"}
SKIP_DIRS = {"node_modules", "vendor", "vendored", "dist", "target", "test-vectors", "grammars"}
GENERATED_HEADER = re.compile(
    r"@generated\b|\bauto[- ]?generated\b|\bautomatically generated\b|"
    r"\bgenerated (?:by|from|file|code)\b|\bdo not edit\b",
    re.IGNORECASE,
)


class Token(NamedTuple):
    text: str
    start: int
    end: int


def tokens(source: str) -> list[Token]:
    """Tokenize enough Rust to match module braces without reading literal contents."""
    result = []
    i = 0
    while i < len(source):
        start = i
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated block comment")
            continue

        raw = RAW_STRING.match(source, i)
        if raw:
            closing = '"' + raw.group(1)
            end = source.find(closing, raw.end())
            if end < 0:
                raise ValueError("unterminated raw string")
            i = end + len(closing)
            result.append(Token("literal", start, i))
            continue
        if source[i] == '"':
            i += 1
            while i < len(source) and source[i] != '"':
                i += 2 if source[i] == "\\" else 1
            if i >= len(source):
                raise ValueError("unterminated string")
            i += 1
            result.append(Token("literal", start, i))
            continue
        if source[i] == "'":
            # A lifetime has no closing quote after one character or escape.
            end = i + 1
            if source.startswith("\\u{", end):
                close = source.find("}", end + 3)
                end = len(source) if close < 0 else close + 1
            elif source.startswith("\\x", end):
                end += 4
            elif source.startswith("\\", end):
                end += 2
            else:
                end += 1
            if end < len(source) and source[end] == "'":
                i = end + 1
                result.append(Token("literal", start, i))
                continue
        if source[i].isalpha() or source[i] == "_":
            if source.startswith("r#", i):
                i += 2  # A raw identifier is not a raw string, but is one module name.
            i += 1
            while i < len(source) and (source[i].isalnum() or source[i] == "_"):
                i += 1
        else:
            i += 1
        result.append(Token(source[start:i], start, i))
    return result


def group_end(items: list[Token], start: int, opening: str, closing: str) -> int:
    depth = 0
    for i in range(start, len(items)):
        if items[i].text == opening:
            depth += 1
        elif items[i].text == closing:
            depth -= 1
            if depth == 0:
                return i + 1
    raise ValueError(f"unclosed {opening}")


def test_module_spans(source: str) -> list[tuple[int, int]]:
    items = tokens(source)
    spans = []
    i = 0
    attribute = ["#", "[", "cfg", "(", "test", ")", "]"]
    while i < len(items):
        if [item.text for item in items[i : i + 7]] != attribute:
            i += 1
            continue
        start = i
        i += 7
        # Other outer attributes and visibility do not make a test module production code.
        while [item.text for item in items[i : i + 2]] == ["#", "["]:
            i = group_end(items, i + 1, "[", "]")
        if i < len(items) and items[i].text == "pub":
            i += 1
            if i < len(items) and items[i].text == "(":
                i = group_end(items, i, "(", ")")
        if i + 2 >= len(items) or items[i].text != "mod":
            continue
        i += 2  # Skip `mod` and the module name.
        if items[i].text != "{":
            continue  # A path module (`mod tests;`) removes no lines from this file.
        end = group_end(items, i, "{", "}")
        spans.append((items[start].start, items[end - 1].end))
        i = end
    return spans


def count_source_lines(source: str) -> int:
    spans = iter(test_module_spans(source))
    span = next(spans, None)
    count = 0
    offset = 0
    for line in source.splitlines(keepends=True):
        end = offset + len(line)
        cursor = offset
        remaining = []
        touched = False
        while span is not None and span[0] < end:
            touched = True
            remaining.append(source[cursor : max(cursor, span[0])])
            cursor = min(end, span[1])
            if span[1] > end:
                break
            span = next(spans, None)
        remaining.append(source[cursor:end])
        # Preserve production code sharing a line with a test module, and blank lines outside it.
        if not touched or "".join(remaining).strip():
            count += 1
        offset = end
    return count


def is_source_path(path: str) -> bool:
    parts = Path(path).parts
    return (
        len(parts) > 1
        and parts[0] in SOURCE_ROOTS
        and Path(path).suffix in SOURCE_SUFFIXES
        and not any(part in SKIP_DIRS for part in parts[:-1])
    )


def has_generated_header(source: str) -> bool:
    """Recognize generator markers only in the leading comment header, not code."""
    i = 1 if source.startswith("\ufeff") else 0
    if source.startswith("#!", i) and not source.startswith("#![", i):
        end = source.find("\n", i)
        i = len(source) if end < 0 else end
    while i < len(source):
        if source[i].isspace():
            i += 1
            continue
        start = i
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
        elif source.startswith("/*", i):
            end = source.find("*/", i + 2)
            i = len(source) if end < 0 else end + 2
        else:
            return False
        if GENERATED_HEADER.search(source[start:i]):
            return True
    return False


def source_counts(path: str, source: str) -> Optional[tuple[int, int]]:
    if not is_source_path(path) or has_generated_header(source):
        return None
    raw = len(source.splitlines())
    counted = count_source_lines(source) if Path(path).suffix == ".rs" else raw
    return counted, raw


def validate_pins(value: object) -> dict[str, dict]:
    if not isinstance(value, dict):
        raise ValueError("pins must be an object mapping source paths to lines and reason")
    for path, pin in value.items():
        if not is_source_path(path) or Path(path).as_posix() != path or ".." in Path(path).parts:
            raise ValueError(f"{path}: pin must name a checked source path")
        if not isinstance(pin, dict) or set(pin) != {"lines", "reason"}:
            raise ValueError(f"{path}: pin must contain exactly lines and reason")
        if type(pin["lines"]) is not int or pin["lines"] < LIMIT:
            raise ValueError(f"{path}: pin must be an integer of at least {LIMIT}; remove smaller pins")
        if not isinstance(pin["reason"], str) or not pin["reason"].strip():
            raise ValueError(f"{path}: pin must have a nonempty reason")
    return value


def size_message(path: str, count: int, pins: dict[str, dict]) -> tuple[bool, str]:
    prefix = f"{path}: {count} hand-written lines"
    if path in pins:
        pin = pins[path]["lines"]
        if count < LIMIT:
            return True, f"{prefix} (limit: under {LIMIT}); remove its pin."
        if count > pin:
            return True, f"{prefix} (pinned limit: {pin}); split it by area; do not raise the pin."
        if count < pin:
            return True, f"{prefix} (pinned limit: {pin}); lower the pin to {count}."
    elif count >= LIMIT:
        return True, f"{prefix} (limit: under {LIMIT}); split it by area."
    return False, ""


class SelfTests(unittest.TestCase):
    def test_raw_strings_and_nested_module(self):
        source = '''fn production() {}
#[cfg(test)]
mod tests {
    const JSON: &str = r#"{"brace": "}"}"#;
    const SQL: &str = r"select '}'";
    const MORE: &str = r###"}"## }"###;
    const BYTES: &[u8] = br##"} "# }"##;
    const C: &std::ffi::CStr = cr#"}"#;
    // } and #[cfg(test)] mod fake {
    /* } /* nested { */ } */
    mod nested {
        fn chars() { let _ = ('}', '{', '\\'', '\\\\', '\\u{7d}', b'}'); }
    }
}
fn after() {}
'''
        self.assertEqual(count_source_lines(source), 2)

    def test_test_module_at_end(self):
        source = '''fn production() {}

#[cfg(test)]
mod tests {
    mod nested {
        fn check() { assert!(true); }
    }
}
'''
        self.assertEqual(count_source_lines(source), 2)

    def test_path_module_removes_nothing(self):
        source = "#[cfg(test)] mod x;\nmod production {\n    fn f() {}\n}\n"
        self.assertEqual(count_source_lines(source), 4)

    def test_non_test_module_and_blank_lines(self):
        source = "// comment\n\nmod production {\n    fn f() {}\n}\n"
        self.assertEqual(count_source_lines(source), 5)

    def test_strings_comments_and_lifetimes_outside_tests(self):
        source = '''const TEXT: &str = "#[cfg(test)] mod fake { \\" }";
/* #[cfg(test)] mod fake { } */
fn borrow<'a>(s: &'a str) -> &'a str { s }
#[cfg(test)]
mod tests { const S: &str = "} \\" {"; }
'''
        self.assertEqual(count_source_lines(source), 3)

    def test_attributes_visibility_and_multiline_cfg(self):
        source = "#[cfg(\n test\n)]\n#[allow(dead_code)]\npub(crate) mod checks {\n}\n"
        self.assertEqual(count_source_lines(source), 0)
        self.assertEqual(count_source_lines("#[cfg(test)] mod r#type {}\n"), 0)

    def test_multiple_modules_and_shared_lines(self):
        source = "#[cfg(test)] mod one {}\n\nmod prod { #[cfg(test)] mod two {} }\n"
        self.assertEqual(count_source_lines(source), 2)

    def test_empty_and_no_final_newline(self):
        self.assertEqual(count_source_lines(""), 0)
        self.assertEqual(count_source_lines("fn f() {}"), 1)
        self.assertEqual(count_source_lines("#[cfg(test)] mod tests {}"), 0)

    def test_unterminated_literals_fail_closed(self):
        for source in ('const S: &str = r#"{', 'const S: &str = "{', '/* unterminated'):
            with self.subTest(source=source), self.assertRaises(ValueError):
                count_source_lines(source)
        with self.assertRaises(ValueError):
            count_source_lines("#[cfg(test)] mod tests {")

    def test_source_paths_and_skip_rules(self):
        for path in (
            "crates/a/tests/x.rs", "crates/a/benches/x.rs", "crates/a/src/foo_tests.rs",
            "crates/a/src/test_support.rs", "packages/a/src/x.ts", "packages/a/x.mts",
            "packages/a/x.mjs", "packages/a/x.js", "packages/a/x.test.ts",
            "packages/a/src/__tests__/x.ts",
        ):
            with self.subTest(path=path):
                self.assertTrue(is_source_path(path))
        for directory in SKIP_DIRS:
            with self.subTest(directory=directory):
                self.assertFalse(is_source_path(f"packages/a/{directory}/x.ts"))
        for path in ("scripts/x.js", "packages/a/schema.json", "crates/a/fixture.json"):
            with self.subTest(path=path):
                self.assertFalse(is_source_path(path))

    def test_generated_header(self):
        for header in (
            "// Auto-generated by build.rs from ai-tokenizer Claude encoding.\n",
            "// Generated by regenerate.py; do not edit individual vectors.\n",
            "/* License */\n\n/** @generated */\n",
            "#!/usr/bin/env node\n// DO NOT EDIT\n",
            "\ufeff// This file is automatically generated.\n",
        ):
            for path in ("crates/a/src/x.rs", "packages/a/src/x.ts"):
                with self.subTest(header=header, path=path):
                    self.assertIsNone(source_counts(path, header + "const X = 1;\n"))
        for source in (
            'const HEADER = "// @generated";\n',
            "fn f() {}\n// Generated by a test fixture\n",
            "// Keep generated output next to the source.\nfn f() {}\n",
            "#![allow(dead_code)]\nfn f() {}\n",
        ):
            with self.subTest(source=source):
                self.assertFalse(has_generated_header(source))

    def test_typescript_counts_the_whole_file(self):
        source = '// #[cfg(test)] mod tests {\nconst x = "}";\n// }\n'
        for suffix in ("ts", "mts", "mjs", "js"):
            with self.subTest(suffix=suffix):
                self.assertEqual(source_counts(f"packages/a/src/__tests__/x.test.{suffix}", source), (3, 3))
        self.assertEqual(source_counts("crates/a/tests/x.rs", "#[cfg(test)] mod tests {}\nfn f() {}\n"), (1, 2))

    def test_limit_and_pins(self):
        path = "crates/self-test.rs"
        self.assertFalse(size_message(path, LIMIT - 1, {})[0])
        self.assertTrue(size_message(path, LIMIT, {})[0])
        pins = {path: {"lines": LIMIT + 10, "reason": "Split by area pending."}}
        self.assertEqual(size_message(path, LIMIT + 10, pins), (False, ""))
        self.assertTrue(size_message(path, LIMIT + 11, pins)[0])
        failed, message = size_message(path, LIMIT, pins)
        self.assertTrue(failed)
        self.assertIn("lower the pin", message)
        failed, message = size_message(path, LIMIT - 1, pins)
        self.assertTrue(failed)
        self.assertIn("remove its pin", message)

    def test_pins_require_exact_shape_and_reason(self):
        path = "crates/self-test.rs"
        valid = {path: {"lines": LIMIT, "reason": "Split by area pending."}}
        self.assertEqual(validate_pins(valid), valid)
        for value in (
            [], {path: LIMIT}, {path: {"lines": LIMIT}},
            {path: {"lines": LIMIT, "reason": " "}},
            {path: {"lines": LIMIT - 1, "reason": "Split pending."}},
            {"scripts/x.rs": {"lines": LIMIT, "reason": "Split pending."}},
            {"crates/../x.rs": {"lines": LIMIT, "reason": "Split pending."}},
        ):
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_pins(value)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", action="store_true", help="list files with at least 4,000 counted lines, largest first")
    parser.add_argument("--report-path", action="append", default=[], metavar="PATH", help="also report this file below 4,000 lines (repeatable)")
    parser.add_argument("--self-test", action="store_true", help="run the scanner's regression tests")
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1

    try:
        pins = validate_pins(json.loads(PINS_FILE.read_text(encoding="utf-8")))
        tracked = subprocess.check_output(["git", "ls-files", "-z", "--", "crates/", "packages/"], cwd=ROOT)
        paths = sorted(path for path in tracked.decode().split("\0") if is_source_path(path))
        failed = False
        counts = {}
        generated = 0
        for path in paths:
            try:
                count = source_counts(path, (ROOT / path).read_text(encoding="utf-8"))
            except (OSError, ValueError) as error:
                print(f"{path}: cannot count source lines: {error}")
                failed = True
                continue
            if count is None:
                generated += 1
                continue
            counts[path] = count
            violation, message = size_message(path, count[0], pins)
            failed |= violation
            if message:
                print(message)
        for path in sorted(pins.keys() - counts.keys()):
            print(f"{path}: not a checked source file; remove its pin.")
            failed = True
        for path in sorted(set(args.report_path) - counts.keys()):
            print(f"{path}: requested report path is not a checked source file.")
            failed = True
        if args.report or args.report_path:
            print("Counted lines / raw lines / path (Rust excludes in-file cfg(test) modules):")
            for path, (count, raw) in sorted(counts.items(), key=lambda item: (-item[1][0], item[0])):
                if (args.report and count >= REPORT_THRESHOLD) or path in args.report_path:
                    print(f"{count:6}  {raw:6}  {path}")
        print(f"File size check {'FAILED' if failed else 'passed'}: {len(counts)} source files checked, {generated} generated files skipped, {len(pins)} pins (limit: under {LIMIT}).")
        return 1 if failed else 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"File size check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
