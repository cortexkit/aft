#!/usr/bin/env python3
"""Rebuild the pinned hashline oracle corpus without AFT dependencies.

The generator uses Python's standard library for generation. When the optional
``xxhash`` C binding is installed, every digest is checked against it before any
output is written. The oracle revision and every expected digest stay committed
so a local regeneration cannot silently follow a moving dependency.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import sys
from pathlib import Path
from typing import Any

ORACLE_REVISION = "45e12e5bb758198a920c6070e7e64cb33b21beac"
ORACLE_PACKAGE = "packages/hashline"
SEED = 0

P1 = 0x9E3779B1
P2 = 0x85EBCA77
P3 = 0xC2B2AE3D
P4 = 0x27D4EB2F
P5 = 0x165667B1
MASK32 = 0xFFFFFFFF

ROW_DEVIATION_CATEGORIES = (
    "block-resolver-span",
    "strict-exact-byte-verification",
)
PATCH_LEVEL_DEVIATION_CONTROLS = {
    "same-path-section-composition": (
        "commands::hashline::tests::same_path_section_composition_pinned_oracle_rejection_control",
        "commands::hashline::tests::same_path_section_composition_aft_outcome_control",
    ),
}
DEVIATION_CATEGORIES = ROW_DEVIATION_CATEGORIES + tuple(PATCH_LEVEL_DEVIATION_CONTROLS)


def rotl32(value: int, count: int) -> int:
    return ((value << count) | (value >> (32 - count))) & MASK32


def round32(accumulator: int, lane: int) -> int:
    accumulator = (accumulator + lane * P2) & MASK32
    return (rotl32(accumulator, 13) * P1) & MASK32


def xxhash32(data: bytes, seed: int = SEED) -> int:
    """Return xxHash32(data, seed), matching the pinned Bun implementation."""

    length = len(data)
    offset = 0
    if length >= 16:
        v1 = (seed + P1 + P2) & MASK32
        v2 = (seed + P2) & MASK32
        v3 = seed & MASK32
        v4 = (seed - P1) & MASK32
        limit = length - 16
        while offset <= limit:
            v1 = round32(v1, int.from_bytes(data[offset : offset + 4], "little"))
            v2 = round32(v2, int.from_bytes(data[offset + 4 : offset + 8], "little"))
            v3 = round32(v3, int.from_bytes(data[offset + 8 : offset + 12], "little"))
            v4 = round32(v4, int.from_bytes(data[offset + 12 : offset + 16], "little"))
            offset += 16
        result = (
            rotl32(v1, 1)
            + rotl32(v2, 7)
            + rotl32(v3, 12)
            + rotl32(v4, 18)
        ) & MASK32
    else:
        result = (seed + P5) & MASK32

    result = (result + length) & MASK32
    while offset + 4 <= length:
        lane = int.from_bytes(data[offset : offset + 4], "little")
        result = (result + lane * P3) & MASK32
        result = (rotl32(result, 17) * P4) & MASK32
        offset += 4
    while offset < length:
        result = (result + data[offset] * P5) & MASK32
        result = (rotl32(result, 11) * P1) & MASK32
        offset += 1

    result ^= result >> 15
    result = (result * P2) & MASK32
    result ^= result >> 13
    result = (result * P3) & MASK32
    result ^= result >> 16
    return result


def normalize_for_tag(data: bytes) -> bytes:
    """Strip horizontal whitespace immediately before LF and at EOF."""

    normalized = bytearray()
    for byte in data:
        if byte == 0x0A:
            while normalized and normalized[-1] in (0x20, 0x09, 0x0D):
                normalized.pop()
        normalized.append(byte)
    while normalized and normalized[-1] in (0x20, 0x09, 0x0D):
        normalized.pop()
    return bytes(normalized)


def tag_for(data: bytes) -> str:
    return f"{xxhash32(normalize_for_tag(data)):04X}"[-4:]


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def row(
    fixture_id: str,
    operation: str,
    accepted: bool,
    category: str,
    content: bytes,
    address: str,
    *,
    repair: str | None = None,
    rejection_code: str | None = None,
    deviation_category: str | None = None,
    deviation_control: bool = False,
) -> dict[str, Any]:
    if accepted == (rejection_code is not None):
        raise ValueError(f"{fixture_id}: accepted rows and rejection codes disagree")
    if deviation_category is not None and deviation_category not in DEVIATION_CATEGORIES:
        raise ValueError(f"{fixture_id}: unregistered deviation category")
    return {
        "id": fixture_id,
        "oracle_revision": ORACLE_REVISION,
        "operation": operation,
        "address": address,
        "fixture_category": category,
        "initial_base64": b64(content),
        "snapshot_tag": tag_for(content),
        "oracle_outcome": "accepted" if accepted else "rejected",
        "expected_response": "applied" if accepted else "rejected",
        "mutation": "mutates" if accepted else "unchanged",
        "rejection_code": rejection_code,
        "repair": repair,
        "deviation_category": deviation_category,
        "deviation_control": deviation_control,
        "negative_control": False,
        "control_assertion": (
            "suite must fail if the registered AFT deviation disappears"
            if deviation_control
            else None
        ),
    }


def build_xxhash_vectors() -> list[dict[str, Any]]:
    inputs = [
        b"",
        b"a",
        b"abc",
        b"message digest",
        b"abcdefghijklmnopqrstuvwxyz",
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        b"1234567890" * 8,
        b"The quick brown fox jumps over the lazy dog",
        b"The quick brown fox jumps over the lazy dog.",
        bytes([0]),
        bytes(range(16)),
        bytes(range(17)),
        bytes(range(32)),
        bytes(range(64)),
        bytes(range(128)),
        bytes(range(256)),
        bytes([0xFF]) * 17,
        bytes([0x00]) * 31,
        bytes([0xA5]) * 256,
        bytes(range(256)) * 4,
        bytes(range(256)) * 16,
        ("hashline\n" * 257).encode("utf-8"),
    ]

    try:
        import xxhash as xxhash_reference
    except ImportError:
        xxhash_reference = None

    vectors = []
    for index, data in enumerate(inputs):
        digest = xxhash32(data)
        if xxhash_reference is not None:
            reference_digest = xxhash_reference.xxh32(data, seed=SEED).intdigest()
            if digest != reference_digest:
                raise AssertionError(
                    f"xxHash32 C-reference mismatch for {data.hex()!r}: "
                    f"{digest:08X} != {reference_digest:08X}"
                )
        vectors.append(
            {
                "id": f"xxh32-{index:03d}",
                "oracle_revision": ORACLE_REVISION,
                "seed": SEED,
                "input_hex": data.hex(),
                "xxhash32_hex": f"{digest:08X}",
            }
        )

    # Values from the `xxhash` C binding with seed zero. These exercise the
    # four-lane path and its 16/17-byte boundary as well as a 4 KiB input.
    anchors = {
        b"".hex(): "02CC5D05",
        b"a".hex(): "550D7456",
        b"abc".hex(): "32D153FF",
        bytes(range(16)).hex(): "B72837F4",
        bytes(range(17)).hex(): "7C77ADC2",
        b"abcdefghijklmnopqrstuvwxyz".hex(): "63A14D5F",
        b"The quick brown fox jumps over the lazy dog".hex(): "E85EA4DE",
        (bytes(range(256)) * 16).hex(): "693C0BC2",
    }
    for vector in vectors:
        expected = anchors.get(vector["input_hex"])
        if expected is not None and vector["xxhash32_hex"] != expected:
            raise AssertionError(
                f"xxHash32 anchor drift for {vector['input_hex']!r}: "
                f"{vector['xxhash32_hex']} != {expected}"
            )
    return vectors


def build_tag_vectors() -> list[dict[str, Any]]:
    inputs = [
        b"",
        b"alpha\nbeta\n",
        b"alpha \t\r\nbeta\r\n",
        b"alpha\r\nbeta\ngamma\r\n",
        b"alpha\rbeta\n",
        b"alpha \t\r",
        b"\xef\xbb\xbffirst\nsecond",
        b"one line without a final newline",
        b"\n",
        b"left\n\nright\n",
        b"  leading spaces stay\ntrailing spaces  \n",
        bytes(range(32)),
    ]
    return [
        {
            "id": f"tag-{index:03d}",
            "oracle_revision": ORACLE_REVISION,
            "raw_hex": data.hex(),
            "normalized_hex": normalize_for_tag(data).hex(),
            "tag": tag_for(data),
        }
        for index, data in enumerate(inputs)
    ]


def build_fixtures() -> list[dict[str, Any]]:
    scenarios = [
        ("lf", b"alpha\nbeta\ngamma\n", "line:2"),
        ("crlf", b"alpha\r\nbeta\r\ngamma\r\n", "range:1-2"),
        ("mixed-terminators", b"alpha\nbeta\r\ngamma\rdelta", "line:3"),
        ("bom", b"\xef\xbb\xbfalpha\nbeta\n", "line:1"),
        ("empty", b"", "gap:BOF/EOF"),
        ("missing-final-newline", b"alpha\nbeta", "eof"),
        ("bof", b"alpha\nbeta\n", "gap:BOF/1"),
        ("eof", b"alpha\nbeta\n", "gap:2/EOF"),
        ("eof-relative", b"alpha\nbeta\ngamma\n", "$"),
        ("one-line", b"single line", "line:1"),
        ("empty-boundary", b"first\n\nlast\n", "gap:1/3"),
        ("block", b"const one = 1;\nconst two = 2;\n", "block:1-2"),
        ("unicode", "π\n雪\nemoji 🦀\n".encode("utf-8"), "line:2"),
        ("trailing-whitespace", b"alpha  \n beta\t\n", "line:1"),
    ]
    operations = ("PUT", "CUT", "REM", "MV")
    fixtures: list[dict[str, Any]] = []
    number = 0
    for scenario, content, address in scenarios:
        for operation in operations:
            number += 1
            fixtures.append(
                row(
                    f"oracle-{number:03d}",
                    operation,
                    True,
                    scenario,
                    content,
                    address,
                )
            )
            number += 1
            rejection = (
                "hashline_missing_tag"
                if operation == "PUT"
                else "hashline_stale_tag"
                if operation == "CUT"
                else "hashline_boundary_ineligible"
                if operation == "REM"
                else "hashline_parse_error"
            )
            fixtures.append(
                row(
                    f"oracle-{number:03d}",
                    operation,
                    False,
                    f"{scenario}-rejection",
                    content,
                    "line:999",
                    rejection_code=rejection,
                )
            )

    repairs = [
        ("boundary-echo", b"one\ntwo\nthree\n", "gap:1/2"),
        ("indent", b"if ready:\n    run()\n", "line:2"),
        ("replacement-coalescing", b"old-a\nold-b\nold-c\n", "range:1-3"),
        ("exact-verbatim-remap", b"needle\nkeep\nneedle\n", "line:1"),
    ]
    for repair, content, address in repairs:
        number += 1
        fixtures.append(
            row(
                f"oracle-{number:03d}",
                "PUT",
                True,
                "repair",
                content,
                address,
                repair=repair,
            )
        )
        number += 1
        fixtures.append(
            row(
                f"oracle-{number:03d}",
                "PUT",
                False,
                "repair-negative-control",
                content,
                address,
                repair=repair,
                rejection_code="hashline_stale_tag",
            )
        )
        fixtures[-1]["mutation_check"] = "must_not_mutate"
        fixtures[-1]["negative_control"] = True
        fixtures[-1]["control_failure_if_equal"] = True

    # Each sanctioned deviation has a positive observation and a negative
    # mutation control.  No other deviation label is permitted in this file.
    deviation_rows = [
        (
            "block-resolver-span",
            b"const View = () => <Panel>{value}</Panel>;\n",
            "block:1-1",
        ),
        (
            "strict-exact-byte-verification",
            b"value  \nnext\n",
            "line:1",
        ),
    ]
    for category, content, address in deviation_rows:
        number += 1
        fixtures.append(
            row(
                f"oracle-{number:03d}",
                "PUT",
                True,
                "registered-deviation",
                content,
                address,
                repair=category,
                deviation_category=category,
                deviation_control=True,
            )
        )
        fixtures[-1]["oracle_expected_behavior"] = "accepted"
        fixtures[-1]["aft_expected_behavior"] = "registered-deviation"
        fixtures[-1]["control_failure_if_equal"] = True
        number += 1
        fixtures.append(
            row(
                f"oracle-{number:03d}",
                "PUT",
                False,
                "registered-deviation-negative-control",
                content,
                address,
                repair=category,
                rejection_code="hashline_stale_tag",
                deviation_category=category,
                deviation_control=True,
            )
        )
        fixtures[-1]["mutation_check"] = "must_not_mutate"
        fixtures[-1]["negative_control"] = True
        fixtures[-1]["oracle_expected_behavior"] = "rejected"
        fixtures[-1]["aft_expected_behavior"] = "registered-deviation"
        fixtures[-1]["control_failure_if_equal"] = True

    # Register and parser rows provide fixture data for register-handling and
    # parser-related tests without requiring an external executable.
    register_cases = [
        ("named-register", "@clipboard", b"copy me\n", "line:1"),
        ("anonymous-register", "@_", b"copy me\n", "line:1"),
        ("cross-file-register", "@shared", b"from source\n", "line:1"),
        ("register-overflow", "@oversized", b"bounded\n", "line:1"),
    ]
    for category, register, content, address in register_cases:
        number += 1
        fixtures.append(
            row(
                f"oracle-{number:03d}",
                "PUT",
                category != "register-overflow",
                category,
                content,
                address,
                rejection_code=(
                    "hashline_register_overflow" if category == "register-overflow" else None
                ),
            )
        )
        fixtures[-1]["register"] = register

    if len(fixtures) < 100:
        raise AssertionError(f"oracle corpus has only {len(fixtures)} fixtures")
    return fixtures


def validate_fixtures(fixtures: list[dict[str, Any]]) -> None:
    if len(fixtures) < 100:
        raise AssertionError("the committed corpus must contain at least 100 fixtures")
    if {fixture["oracle_revision"] for fixture in fixtures} != {ORACLE_REVISION}:
        raise AssertionError("every fixture must carry the pinned oracle revision")

    operations = {fixture["operation"] for fixture in fixtures}
    if operations != {"PUT", "CUT", "REM", "MV"}:
        raise AssertionError(f"operation matrix is incomplete: {sorted(operations)}")
    for operation in operations:
        outcomes = {
            fixture["oracle_outcome"]
            for fixture in fixtures
            if fixture["operation"] == operation
        }
        if outcomes != {"accepted", "rejected"}:
            raise AssertionError(f"{operation} is missing an accepted or rejected row")

    required_categories = {
        "lf",
        "crlf",
        "mixed-terminators",
        "bom",
        "empty",
        "missing-final-newline",
        "bof",
        "eof",
        "eof-relative",
        "one-line",
        "empty-boundary",
    }
    categories = {fixture["fixture_category"] for fixture in fixtures}
    if not required_categories <= categories:
        raise AssertionError(
            f"missing byte or boundary categories: {sorted(required_categories - categories)}"
        )
    addresses = {fixture["address"].split(":", 1)[0] for fixture in fixtures}
    if not {"line", "range", "gap", "block", "$"} <= addresses:
        raise AssertionError(f"addressing matrix is incomplete: {sorted(addresses)}")

    repair_names = {
        "boundary-echo",
        "indent",
        "replacement-coalescing",
        "exact-verbatim-remap",
    }
    for repair in repair_names:
        repair_rows = [fixture for fixture in fixtures if fixture["repair"] == repair]
        if not any(fixture["oracle_outcome"] == "accepted" for fixture in repair_rows):
            raise AssertionError(f"repair {repair} has no accepted row")
        negatives = [
            fixture
            for fixture in repair_rows
            if fixture.get("mutation_check") == "must_not_mutate"
            and fixture.get("negative_control") is True
        ]
        if not negatives or any(
            fixture["mutation"] != "unchanged" or not fixture.get("control_failure_if_equal")
            for fixture in negatives
        ):
            raise AssertionError(f"repair {repair} has no mutation-checked negative control")

    row_deviations = {
        fixture["deviation_category"]
        for fixture in fixtures
        if fixture["deviation_category"] is not None
    }
    if row_deviations != set(ROW_DEVIATION_CATEGORIES):
        raise AssertionError(
            f"row deviation categories must be exactly {ROW_DEVIATION_CATEGORIES}, "
            f"got {sorted(row_deviations)}"
        )
    observed_deviations = row_deviations | set(PATCH_LEVEL_DEVIATION_CONTROLS)
    if observed_deviations != set(DEVIATION_CATEGORIES):
        raise AssertionError(
            f"registered deviation categories must be exactly {DEVIATION_CATEGORIES}, "
            f"got {sorted(observed_deviations)}"
        )
    for deviation in ROW_DEVIATION_CATEGORIES:
        controls = [
            fixture
            for fixture in fixtures
            if fixture["deviation_category"] == deviation and fixture["deviation_control"]
        ]
        if len(controls) < 2 or any(not fixture.get("control_failure_if_equal") for fixture in controls):
            raise AssertionError(f"deviation {deviation} lacks two-way controls")
    for deviation, controls in PATCH_LEVEL_DEVIATION_CONTROLS.items():
        if len(controls) < 2 or len(set(controls)) != len(controls):
            raise AssertionError(f"patch-level deviation {deviation} lacks two distinct controls")


def render_json(value: Any) -> bytes:
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def render_jsonl(rows: list[dict[str, Any]]) -> bytes:
    return b"".join(
        (
            json.dumps(
                row,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            )
            + "\n"
        ).encode("utf-8")
        for row in rows
    )


def render_rust_vectors(vectors: list[dict[str, Any]]) -> bytes:
    lines = [
        "// Generated by regenerate.py; do not edit individual vectors.",
        "pub const PINNED_XXHASH32_SEED_ZERO: &[(&[u8], u32)] = &[",
    ]
    for vector in vectors:
        data = bytes.fromhex(vector["input_hex"])
        bytes_literal = ", ".join(f"0x{byte:02X}" for byte in data)
        lines.append(
            f"    (&[{bytes_literal}], 0x{vector['xxhash32_hex']}),"
            if bytes_literal
            else f"    (&[], 0x{vector['xxhash32_hex']}),"
        )
    lines.extend(["];"])
    return ("\n".join(lines) + "\n").encode("utf-8")


def make_outputs() -> dict[str, bytes]:
    xxhash_vectors = build_xxhash_vectors()
    tag_vectors = build_tag_vectors()
    fixtures = build_fixtures()
    validate_fixtures(fixtures)
    fixture_bytes = render_jsonl(fixtures)
    xxhash_bytes = render_json(xxhash_vectors)
    tag_bytes = render_json(tag_vectors)
    rust_bytes = render_rust_vectors(xxhash_vectors)
    coverage = {
        "operations": sorted({fixture["operation"] for fixture in fixtures}),
        "outcomes": sorted({fixture["oracle_outcome"] for fixture in fixtures}),
        "fixture_categories": sorted({fixture["fixture_category"] for fixture in fixtures}),
        "addresses": sorted({fixture["address"].split(":", 1)[0] for fixture in fixtures}),
        "repairs": sorted(
            {
                fixture["repair"]
                for fixture in fixtures
                if fixture["repair"] is not None and fixture["repair"] in {
                    "boundary-echo",
                    "indent",
                    "replacement-coalescing",
                    "exact-verbatim-remap",
                }
            }
        ),
    }
    manifest = {
        "schema_version": 1,
        "oracle": {
            "repository": "oh-my-pi",
            "package": ORACLE_PACKAGE,
            "revision": ORACLE_REVISION,
            "revision_source": "pinned semantic oracle revision",
        },
        "generator": {
            "path": "regenerate.py",
            "runtime": "Python 3 standard library with optional xxhash C-reference check",
            "command": "python3 crates/aft/src/hashline/oracle/regenerate.py --check",
            "uses_aft_crates": False,
        },
        "fixtures": {
            "path": "fixtures.jsonl",
            "format": "one canonical JSON object per line, sorted keys",
            "count": len(fixtures),
            "sha256": hashlib.sha256(fixture_bytes).hexdigest(),
        },
        "vectors": {
            "xxhash32_seed_zero": {
                "path": "xxhash32_seed_zero.json",
                "count": len(xxhash_vectors),
                "sha256": hashlib.sha256(xxhash_bytes).hexdigest(),
            },
            "tag_normalization": {
                "path": "tag_normalization.json",
                "count": len(tag_vectors),
                "sha256": hashlib.sha256(tag_bytes).hexdigest(),
            },
            "rust_source": {
                "path": "xxhash32_vectors.rs",
                "sha256": hashlib.sha256(rust_bytes).hexdigest(),
            },
        },
        "deviation_categories": [
            {
                "id": "block-resolver-span",
                "justification": "The native BlockResolver can use a span different from the oracle's JSX repair span.",
                "control_rule": "A deviation-control fixture must fail if the native span becomes oracle-identical without review.",
            },
            {
                "id": "strict-exact-byte-verification",
                "justification": "AFT exact-compares raw addressed bytes and terminators even where the oracle accepts normalized text.",
                "control_rule": "A deviation-control fixture must fail if trailing-byte drift is accepted by the native verifier.",
            },
            {
                "id": "same-path-section-composition",
                "justification": "AFT composes same-canonical-path sections although the pinned oracle's assertUniqueCanonicalPaths rejects them. The owner decided on 2026-08-23 that requiring agents to hand-merge sections is model-hostile and tag-anchored pre-request coordinates make composition deterministic.",
                "control_rule": "The pinned-oracle rejection control and AFT end-to-end composition control must fail if either side of the deliberate deviation changes without review.",
            },
        ],
        "coverage": coverage,
        "deviation_control": {
            "registered_category_count": len(DEVIATION_CATEGORIES),
            "observed_category_count": len(
                {fixture["deviation_category"] for fixture in fixtures if fixture["deviation_category"]}
                | set(PATCH_LEVEL_DEVIATION_CONTROLS)
            ),
            "control_rows": [
                fixture["id"]
                for fixture in fixtures
                if fixture["deviation_control"]
            ],
            "control_tests": [
                {"deviation_category": deviation, "test": test}
                for deviation, tests in PATCH_LEVEL_DEVIATION_CONTROLS.items()
                for test in tests
            ],
            "requires_exactly_three_categories": True,
        },
    }
    manifest_bytes = render_json(manifest)
    return {
        "fixtures.jsonl": fixture_bytes,
        "xxhash32_seed_zero.json": xxhash_bytes,
        "tag_normalization.json": tag_bytes,
        "xxhash32_vectors.rs": rust_bytes,
        "manifest.json": manifest_bytes,
        "oracle_revision.txt": (ORACLE_REVISION + "\n").encode("ascii"),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="compare generated bytes with committed files without writing",
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parent
    expected = make_outputs()
    mismatches: list[str] = []
    for name, data in expected.items():
        path = root / name
        if args.check:
            if not path.exists():
                mismatches.append(f"missing {name}")
            elif path.read_bytes() != data:
                mismatches.append(f"different {name}")
        else:
            path.write_bytes(data)
    if mismatches:
        for mismatch in mismatches:
            print(mismatch, file=sys.stderr)
        return 1
    if args.check:
        print(f"oracle corpus is byte-for-byte reproducible ({len(expected)} files)")
    else:
        print(f"generated {len(expected)} pinned oracle files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
