#!/usr/bin/env python3
"""Add constructed symbol-range attribution rows to the pinned AFT corpus.

Each row asks for content whose file rank depends on which lines the engine
attributes to a symbol: an approximate span that runs past the next
declaration, or a comment block that is not part of the declaration below it.
The rows are built from pinned repository content, not from telemetry, and
every one of them carries the file:line evidence for its answer.
"""
from __future__ import annotations

import json
from pathlib import Path

from search_quality_lib import canonical_json

HERE = Path(__file__).resolve().parent
KIND_FIELD = "range_attribution_kind"


def main() -> None:
    path = HERE / "real-query-manifest.json"
    manifest = json.loads(path.read_text())
    manifest["rows"] = [row for row in manifest["rows"] if KIND_FIELD not in row]
    template = next(row for row in manifest["rows"] if row["episode_id"] == "followup-census:7972")
    bindings = ("repo", "sha", "embedding_pack", "embedding_pack_sha256", "evidence_tree_sha256")

    def row(number: int, kind: str, query: str, answer: str, basis: str, *, shape: str, stratum: str, include_tests: bool, answer_kind: str) -> dict:
        return {
            **{key: template[key] for key in bindings},
            "episode_id": f"followup-census:{number}",
            KIND_FIELD: kind,
            "query": query,
            "opened_file": answer,
            "answer_kind": answer_kind,
            "answer_key_basis": basis,
            "row_source": "constructed from pinned AFT repository content; not telemetry",
            "include_tests": include_tests,
            "include_tests_source": "recorded" if include_tests else "default",
            "pinned_shape": shape,
            "mechanism": "other",
            "census_stratum": stratum,
        }

    rows = [
        row(
            940001, "approximate_span",
            "useState",
            "crates/aft/src/commands/add_import.rs",
            "Promotes the reviewed concept-recall case useState (fixtures.json), keeping its non-test answer. At the pin add_import.rs:156-165 documents the add_import request, naming useState as the example named import at :161, and :863-869 tests a bare useState name; AFT has no useState definition. imports/mod.rs also uses useState as a specifier example, but the reviewed answer key names add_import.rs, the command a user would call with it.",
            shape="identifier", stratum="identifier", include_tests=False, answer_kind="concept",
        ),
        row(
            940002, "detached_comment",
            "where are the tree-sitter symbol extraction tests for Rust source files",
            "crates/aft/src/parser.rs",
            "At the pin parser.rs:8127-8128 opens the parser test module; :9331-9333 and :9491-9494 run symbol extraction over tests/fixtures/sample.rs and assert the extracted Rust symbols. sample.rs:1 is only the fixture's description line, separated by a blank line from the unrelated public_function at :3-5.",
            shape="natural_language", stratum="nl", include_tests=True, answer_kind="concept",
        ),
        row(
            940003, "detached_comment",
            "which tests check that tree-sitter extracts every symbol from a sample Rust module",
            "crates/aft/src/parser.rs",
            "At the pin parser.rs:9491-9500 rs_extracts_all_symbols lists the symbols of tests/fixtures/sample.rs and asserts each one. sample.rs:1 describes the fixture; the fixture asserts nothing.",
            shape="natural_language", stratum="nl", include_tests=True, answer_kind="concept",
        ),
        row(
            940004, "detached_comment",
            "add_import on a Rust file reports whether the response was formatted",
            "crates/aft/tests/integration/format_test.rs",
            "At the pin format_test.rs:794 documents format_integration_add_import_with_format, which :796-835 runs add_import on a .rs file and asserts the formatted field. The documentation sits one blank line above the test attribute.",
            shape="mixed", stratum="nl", include_tests=True, answer_kind="concept",
        ),
        row(
            940005, "detached_comment",
            "edit_symbol on a Rust file verifies the formatted field in the response",
            "crates/aft/tests/integration/format_test.rs",
            "At the pin format_test.rs:837 documents format_integration_edit_symbol_with_format, which :839-873 runs edit_symbol on a .rs file and asserts the formatted field. The documentation sits one blank line above the test attribute.",
            shape="mixed", stratum="nl", include_tests=True, answer_kind="concept",
        ),
    ]
    manifest["rows"].extend(rows)
    path.write_bytes(canonical_json(manifest))
    print(f"range_attribution_rows:{len(rows)}")


if __name__ == "__main__":
    main()
