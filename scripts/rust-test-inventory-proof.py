#!/usr/bin/env python3
"""Prove that every Rust test lands in exactly one split-gate bucket.

By default, this script captures the current workspace inventory from `cargo test
--workspace -- --list`, then compares it against the split buckets used by
`scripts/rust-test-gate.sh`:

* `cargo nextest list --workspace --lib --bins`
* `cargo nextest list --workspace -E 'kind(test) - binary(=watcher_integration)'`
* `cargo test -p agent-file-tools --test watcher_integration`

When `AFT_NEXTEST_ARCHIVE_FILE` is set, the archived test inventory is also
compared by test identity with the non-watcher nextest bucket. macOS can provide
separate lib, bin, and integration archives, source inventories captured with
those builds, and watcher inventory file; that mode checks that each archive
matches its source inventory and that its hash partitions are exact without
rebuilding every target in one inventory-proof job.
Doctests are checked as a separate cargo bucket only when the baseline inventory
contains doctest cases.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HASH_SUFFIX_RE = re.compile(r"-[0-9a-f]+(?:\.exe)?$")
RUNNING_RE = re.compile(r"^\s*Running (.+) \((.+)\)$")
COUNT_RE = re.compile(r"^\d+ tests?, \d+ benchmarks$")
TEST_LINE_RE = re.compile(r"^(.*): test$")
# A rustc diagnostic's source excerpt: `8845 |  code` and `|  ^^^ label` lines.
WARNING_EXCERPT_RE = re.compile(r"^(?:\d+\s*)?\|\s")


@dataclass(frozen=True)
class CargoEntry:
    kind: str
    suite: str
    source: str
    name: str

    @property
    def key(self) -> str:
        if self.kind in {"lib", "bin", "test"}:
            return f"{self.kind}|{self.suite}|{self.name}"
        return f"{self.kind}|{self.suite}|{self.source}|{self.name}"


@dataclass(frozen=True)
class NextestEntry:
    kind: str
    suite: str
    name: str

    @property
    def key(self) -> str:
        return f"{self.kind}|{self.suite}|{self.name}"


def run_command(command: list[str], merge_stderr: bool = True) -> str:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        # Cargo writes each `Running ...` suite boundary to stderr but writes
        # that suite's test names to stdout. Merge the streams to preserve their
        # ordering, then let the parser ignore ordinary compile progress. JSON
        # listings keep stderr apart, because replayed compiler warnings would
        # otherwise precede the JSON document.
        stderr=subprocess.STDOUT if merge_stderr else subprocess.PIPE,
        text=True,
        check=False,
        # ANSI color wraps cargo's output and silently breaks the line parsers
        # below when a CI environment exports CARGO_TERM_COLOR=always.
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
    )
    filtered_output = completed.stdout or ""
    if completed.returncode != 0:
        sys.stderr.write(f"command failed ({completed.returncode}): {' '.join(command)}\n")
        sys.stderr.write(filtered_output)
        sys.stderr.write(completed.stderr or "")
        sys.exit(completed.returncode)
    return filtered_output

def suite_name_from_artifact(artifact_path: str) -> str:
    artifact = Path(artifact_path).name
    return HASH_SUFFIX_RE.sub("", artifact)


def parse_cargo_list(output: str) -> list[CargoEntry]:
    current_kind: str | None = None
    current_suite: str | None = None
    current_source: str | None = None
    entries: list[CargoEntry] = []

    for raw_line in output.splitlines():
        line = raw_line.rstrip("\n")
        stripped = line.strip()
        if not stripped or stripped.startswith(
            (
                "Compiling ",
                "Checking ",
                "Finished `",
                "Blocking waiting for file lock",
                "Updating ",
                "Downloading ",
                "Downloaded ",
                "Locking ",
                "Adding ",
            )
        ):
            continue

        # Compiler warnings (for example the macOS linker's `__eh_frame section too
        # large` note once the lib test binary passes 16 MB of unwind data) print
        # a `warning:` line plus `-->`, source-excerpt, `|` and `= note:`
        # continuation lines before the listing. sccache prints its own
        # `sccache: warning:` lines when its server restarts mid-build. None of
        # them carry test entries.
        if (
            stripped.startswith(("warning:", "= note:", "= help:", "sccache: ", "--> ", "..."))
            or stripped == "|"
            or WARNING_EXCERPT_RE.match(stripped)
        ):
            continue

        if stripped.startswith("Doc-tests "):
            current_kind = "doc"
            current_suite = stripped.removeprefix("Doc-tests ")
            current_source = current_suite
            continue

        running_match = RUNNING_RE.match(line)
        if running_match:
            source = running_match.group(1)
            artifact_path = running_match.group(2)
            suite = suite_name_from_artifact(artifact_path)
            kind = "test"
            if source.startswith("unittests "):
                # Cargo preserves native separators in test source labels on Windows.
                unit_source = source.removeprefix("unittests ").replace("\\", "/")
                is_library = unit_source == "src/lib.rs" or unit_source.endswith("/src/lib.rs")
                kind = "lib" if is_library else "bin"
                source = unit_source
            current_kind = kind
            current_suite = suite
            current_source = source
            continue

        if COUNT_RE.match(stripped):
            continue

        test_match = TEST_LINE_RE.match(stripped)
        if test_match and current_kind and current_suite and current_source:
            entries.append(
                CargoEntry(
                    kind=current_kind,
                    suite=current_suite,
                    source=current_source,
                    name=test_match.group(1),
                )
            )
            continue

        raise RuntimeError(f"unrecognized cargo list line: {line}")

    return entries


def verify_cargo_list_windows_source_paths() -> None:
    output = (
        "    Updating crates.io index\n"
        "    Downloading serde v1.0.0\n"
        "    Locking 1 package to latest compatible version\n"
        "Running unittests src\\lib.rs (target/debug/deps/aft-0123456789abcdef.exe)\n"
        "library_path_case: test\n"
        "Running unittests src\\main.rs (target/debug/deps/aft-0123456789abcdef.exe)\n"
        "binary_path_case: test\n"
    )
    entries = parse_cargo_list(output)
    actual = {(entry.kind, entry.suite, entry.name) for entry in entries}
    expected = {
        ("lib", "aft", "library_path_case"),
        ("bin", "aft", "binary_path_case"),
    }
    if actual != expected:
        raise RuntimeError(
            "Cargo test-list target classification failed for Windows source paths: "
            f"expected {sorted(expected)}, got {sorted(actual)}"
        )


verify_cargo_list_windows_source_paths()


def parse_nextest_list(
    output: str,
    allowed_suite_kinds: set[str] | None = None,
    matching_tests_only: bool = False,
) -> list[NextestEntry]:
    allowed_suite_kinds = allowed_suite_kinds or {"test"}
    payload = json.loads(output)
    entries: list[NextestEntry] = []
    for suite_data in payload.get("rust-suites", {}).values():
        suite_kind = suite_data.get("kind")
        if suite_kind not in allowed_suite_kinds:
            continue
        suite = suite_data["binary-name"]
        for name, testcase in suite_data.get("testcases", {}).items():
            if testcase.get("kind") != "test":
                continue
            if (
                matching_tests_only
                and testcase.get("filter-match", {}).get("status") != "matches"
            ):
                continue
            entries.append(NextestEntry(kind=suite_kind, suite=suite, name=name))
    return entries


# Integration shards run through .github/actions/integration-shard, which pins
# three hash partitions on every platform.
INTEGRATION_PARTITION_COUNT = 3


def unit_partition_count() -> int:
    """Return how many hash partitions the calling platform's lib/bin shards use.

    Each platform runs a different number of libtest shard jobs, so the workflow
    step that runs this proof passes that number in AFT_UNIT_PARTITION_COUNT.
    The proof must partition the same way the shard jobs do, or it would prove
    coverage for a split that never runs.
    """
    raw = os.environ.get("AFT_UNIT_PARTITION_COUNT", "3")
    try:
        count = int(raw)
    except ValueError:
        count = 0
    if count < 1:
        print(
            f"AFT_UNIT_PARTITION_COUNT must be a positive integer, got {raw!r}",
            file=sys.stderr,
        )
        sys.exit(2)
    return count


def partition_nextest_entries(
    command: list[str],
    allowed_suite_kinds: set[str] | None = None,
    partition_count: int = INTEGRATION_PARTITION_COUNT,
) -> list[list[NextestEntry]]:
    entries = []
    for shard in range(1, partition_count + 1):
        partition = f"hash:{shard}/{partition_count}"
        command_args = [*command]
        if "--" in command_args:
            separator = command_args.index("--")
            command_args[separator:separator] = ["--partition", partition]
        else:
            command_args.extend(["--partition", partition])
        entries.append(
            parse_nextest_list(
                run_command(command_args, merge_stderr=False),
                allowed_suite_kinds,
                matching_tests_only=True,
            )
        )
    return entries


def partition_coverage_errors(
    expected_entries: list[NextestEntry], partitions: list[list[NextestEntry]]
) -> tuple[list[str], list[str], list[str]]:
    expected = Counter(entry.key for entry in expected_entries)
    actual = Counter(entry.key for partition in partitions for entry in partition)
    missing = sorted(key for key, count in expected.items() if actual[key] < count)
    duplicate = sorted(key for key, count in actual.items() if count > expected[key])
    extra = sorted(key for key in actual if key not in expected)
    return missing, duplicate, extra


def summarize(counter: Counter[str]) -> list[str]:
    return [f"{bucket}={counter[bucket]}" for bucket in sorted(counter)]


def verify_split_archives(
    lib_archive_path: str,
    bin_archive_path: str,
    integration_archive_path: str,
    lib_source_inventory_path: str,
    bin_source_inventory_path: str,
    integration_source_inventory_path: str,
    watcher_inventory_path: str,
    doc_inventory_path: str,
) -> None:
    def archive_command(archive_path: str) -> list[str]:
        return [
            "cargo",
            "nextest",
            "list",
            "--archive-file",
            archive_path,
            "--profile",
            "gate",
            "-T",
            "json",
        ]

    lib_source_output = Path(lib_source_inventory_path).read_text()
    lib_entries = parse_nextest_list(lib_source_output, {"lib"})
    bin_source_output = Path(bin_source_inventory_path).read_text()
    bin_entries = parse_nextest_list(bin_source_output, {"bin"})
    integration_source_output = Path(integration_source_inventory_path).read_text()
    integration_entries = parse_nextest_list(integration_source_output, {"test"})

    lib_archive_all = parse_nextest_list(
        run_command(archive_command(lib_archive_path), merge_stderr=False),
        {"lib", "bin", "test"},
    )
    bin_archive_all = parse_nextest_list(
        run_command(archive_command(bin_archive_path), merge_stderr=False),
        {"lib", "bin", "test"},
    )
    integration_archive_all = parse_nextest_list(
        run_command(archive_command(integration_archive_path), merge_stderr=False),
        {"lib", "bin", "test"},
    )
    lib_archive_entries = [entry for entry in lib_archive_all if entry.kind == "lib"]
    bin_archive_entries = [entry for entry in bin_archive_all if entry.kind == "bin"]
    integration_archive_entries = [
        entry for entry in integration_archive_all if entry.kind == "test"
    ]
    unexpected_archives = {
        "lib": [entry for entry in lib_archive_all if entry.kind != "lib"],
        "bin": [entry for entry in bin_archive_all if entry.kind != "bin"],
        "integration": [
            entry for entry in integration_archive_all if entry.kind != "test"
        ],
    }
    archive_errors = {
        "lib": partition_coverage_errors(lib_entries, [lib_archive_entries]),
        "bin": partition_coverage_errors(bin_entries, [bin_archive_entries]),
        "integration": partition_coverage_errors(
            integration_entries, [integration_archive_entries]
        ),
    }
    watcher_in_nextest = [
        entry.key
        for entry in integration_archive_entries
        if entry.suite == "watcher_integration"
    ]

    unit_entries = [*lib_entries, *bin_entries]
    serial_tls_candidates = [
        entry
        for entry in unit_entries
        if entry.name.endswith("platform_verifier_tls_client_subprocess")
    ]
    serial_tls_entries = [entry for entry in serial_tls_candidates if entry.kind == "lib"]
    serial_backtrace_candidates = [
        entry
        for entry in unit_entries
        if entry.name.endswith("panic_hook_preserves_default_backtrace_behavior_and_persists")
    ]
    serial_backtrace_entries = [
        entry for entry in serial_backtrace_candidates if entry.kind == "lib"
    ]
    serial_unit_keys = {
        entry.key for entry in serial_tls_entries + serial_backtrace_entries
    }

    lib_command = [
        *archive_command(lib_archive_path),
        "-E",
        "kind(lib)",
        "--",
        "--skip",
        "platform_verifier_tls_client_subprocess",
        "--skip",
        "panic_hook_preserves_default_backtrace_behavior_and_persists",
    ]
    bin_command = [*archive_command(bin_archive_path), "-E", "kind(bin)"]
    integration_command = [*archive_command(integration_archive_path), "-E", "kind(test)"]
    lib_shardable_entries = [
        entry
        for entry in parse_nextest_list(
            lib_source_output, {"lib"}, matching_tests_only=True
        )
        if entry.key not in serial_unit_keys
    ]
    bin_shardable_entries = parse_nextest_list(
        bin_source_output, {"bin"}, matching_tests_only=True
    )
    unit_partitions_total = unit_partition_count()
    lib_partitions = partition_nextest_entries(
        lib_command, {"lib"}, unit_partitions_total
    )
    bin_partitions = partition_nextest_entries(
        bin_command, {"bin"}, unit_partitions_total
    )
    unit_partitions = [
        [*lib_partitions[index], *bin_partitions[index]]
        for index in range(unit_partitions_total)
    ]
    unit_shardable_entries = [*lib_shardable_entries, *bin_shardable_entries]
    integration_shardable_entries = parse_nextest_list(
        integration_source_output, {"test"}, matching_tests_only=True
    )
    integration_partitions = partition_nextest_entries(integration_command)

    watcher_output = Path(watcher_inventory_path).read_text()
    watcher_entries = [
        entry for entry in parse_cargo_list(watcher_output) if entry.kind == "test"
    ]
    doc_output = Path(doc_inventory_path).read_text()
    doc_entries = [entry for entry in parse_cargo_list(doc_output) if entry.kind == "doc"]
    all_entries: list[NextestEntry | CargoEntry] = [
        *unit_entries,
        *integration_entries,
        *watcher_entries,
        *doc_entries,
    ]
    all_counts = Counter(entry.key for entry in all_entries)
    duplicate_inventory_entries = sorted(
        key for key, count in all_counts.items() if count > 1
    )
    baseline_by_bucket = Counter(entry.kind for entry in all_entries)

    serial_tests_in_unit_partitions = sorted(
        entry.key
        for partition in unit_partitions
        for entry in partition
        if entry.key in serial_unit_keys
    )
    unit_partition_errors = partition_coverage_errors(
        unit_shardable_entries, unit_partitions
    )
    integration_partition_errors = partition_coverage_errors(
        integration_shardable_entries, integration_partitions
    )
    watcher_integration_partitions = [
        entry.key
        for partition in integration_partitions
        for entry in partition
        if entry.suite == "watcher_integration"
    ]
    partition_errors = unit_partition_errors + integration_partition_errors
    source_archive_errors = [error for errors in archive_errors.values() for error in errors]

    print(
        "Baseline counts (source inventory):",
        ", ".join(summarize(baseline_by_bucket)),
    )
    print(
        "Split counts:",
        ", ".join(
            [
                f"nextest-lib={len(lib_entries)}",
                f"nextest-bin={len(bin_entries)}",
                f"cargo-doc={len(doc_entries)}",
                f"nextest-integration={len(integration_entries)}",
                f"cargo-watcher={len(watcher_entries)}",
            ]
        ),
    )
    print(f"Watcher tests in integration archive: {len(watcher_in_nextest)}")
    print(
        "Serial-probe inventory: "
        f"keychain={len(serial_tls_entries)}, "
        f"backtrace={len(serial_backtrace_entries)} (excluded from hash shards)"
    )
    print(f"Doctest cases in split inventory: {len(doc_entries)}")
    print(
        f"Hash partition counts (lib/bin hash:K/{unit_partitions_total}, "
        f"integration hash:K/{INTEGRATION_PARTITION_COUNT}): "
        f"nextest-lib={','.join(str(len(partition)) for partition in lib_partitions)}; "
        f"nextest-bin={','.join(str(len(partition)) for partition in bin_partitions)}; "
        f"nextest-integration={','.join(str(len(partition)) for partition in integration_partitions)}"
    )
    print(
        "Archive inventory entries: "
        f"lib={len(lib_archive_all)}, bin={len(bin_archive_all)}, "
        f"integration={len(integration_archive_all)}"
    )

    if (
        any(unexpected_archives.values())
        or watcher_in_nextest
        or not watcher_entries
        or not lib_entries
        or not bin_entries
        or not integration_entries
        or duplicate_inventory_entries
        or any(source_archive_errors)
        or len(serial_tls_candidates) != 1
        or len(serial_tls_entries) != 1
        or len(serial_backtrace_candidates) != 1
        or len(serial_backtrace_entries) != 1
        or serial_tests_in_unit_partitions
        or watcher_integration_partitions
        or any(partition_errors)
    ):
        print("\nSplit archive inventory proof failed.")
        for label, entries in unexpected_archives.items():
            if entries:
                print(f"  unexpected {label} archive targets: {len(entries)}")
        if watcher_in_nextest:
            print(f"  watcher leaked into integration archive: {len(watcher_in_nextest)}")
        if not watcher_entries:
            print("  watcher inventory is empty or missing")
        for label, entries in (
            ("lib", lib_entries),
            ("bin", bin_entries),
            ("integration", integration_entries),
        ):
            if not entries:
                print(f"  {label} source inventory is empty")
        if duplicate_inventory_entries:
            print(f"  repeated source inventory entries: {len(duplicate_inventory_entries)}")
        for label, errors in archive_errors.items():
            for category, keys in zip(("uncovered", "repeated", "extra"), errors):
                if keys:
                    print(f"  {label} archive {category} ({len(keys)}):")
                    for key in keys[:20]:
                        print(f"    {key}")
        if len(serial_tls_candidates) != 1 or len(serial_tls_entries) != 1:
            print(
                "  serial platform-verifier TLS test entries: "
                f"expected one lib target, found {len(serial_tls_candidates)} targets "
                f"({len(serial_tls_entries)} lib entries)"
            )
        if len(serial_backtrace_candidates) != 1 or len(serial_backtrace_entries) != 1:
            print(
                "  serial symbolized-backtrace test entries: "
                f"expected one lib target, found {len(serial_backtrace_candidates)} targets "
                f"({len(serial_backtrace_entries)} lib entries)"
            )
        if serial_tests_in_unit_partitions:
            print(
                "  serial-only tests present in unit partitions: "
                f"{len(serial_tests_in_unit_partitions)}"
            )
        if watcher_integration_partitions:
            print(
                "  watcher leaked into integration partitions: "
                f"{len(watcher_integration_partitions)}"
            )
        for label, errors in (
            ("unit lib/bin partitions", unit_partition_errors),
            ("integration partitions", integration_partition_errors),
        ):
            for category, keys in zip(("uncovered", "repeated", "extra"), errors):
                if keys:
                    print(f"  {label} {category} ({len(keys)}):")
                    for key in keys[:20]:
                        print(f"    {key}")
        sys.exit(1)

    print(
        "\nInventory proof passed: every source test is archived and covered exactly once, "
        "with watcher and serial probes kept separate."
    )


lib_archive_path = os.environ.get("AFT_NEXTEST_ARCHIVE_LIB_FILE")
bin_archive_path = os.environ.get("AFT_NEXTEST_ARCHIVE_BIN_FILE")
integration_archive_path = os.environ.get("AFT_NEXTEST_ARCHIVE_INTEGRATION_FILE")
lib_source_inventory_path = os.environ.get("AFT_NEXTEST_SOURCE_INVENTORY_LIB_FILE")
bin_source_inventory_path = os.environ.get("AFT_NEXTEST_SOURCE_INVENTORY_BIN_FILE")
integration_source_inventory_path = os.environ.get(
    "AFT_NEXTEST_SOURCE_INVENTORY_INTEGRATION_FILE"
)
watcher_inventory_path = os.environ.get("AFT_WATCHER_TEST_INVENTORY_FILE")
doc_inventory_path = os.environ.get("AFT_DOCTEST_INVENTORY_FILE")
if any(
    (
        lib_archive_path,
        bin_archive_path,
        integration_archive_path,
        lib_source_inventory_path,
        bin_source_inventory_path,
        integration_source_inventory_path,
        watcher_inventory_path,
        doc_inventory_path,
    )
):
    if (
        lib_archive_path is None
        or bin_archive_path is None
        or integration_archive_path is None
        or lib_source_inventory_path is None
        or bin_source_inventory_path is None
        or integration_source_inventory_path is None
        or watcher_inventory_path is None
        or doc_inventory_path is None
    ):
        print(
            "split archive proof requires lib, bin, and integration archives, "
            "their source inventories, "
            "and the watcher and doctest inventory files",
            file=sys.stderr,
        )
        sys.exit(2)
    verify_split_archives(
        lib_archive_path,
        bin_archive_path,
        integration_archive_path,
        lib_source_inventory_path,
        bin_source_inventory_path,
        integration_source_inventory_path,
        watcher_inventory_path,
        doc_inventory_path,
    )
    sys.exit(0)


baseline_output = run_command(["cargo", "test", "--workspace", "--", "--list"])
baseline_entries = parse_cargo_list(baseline_output)

unit_bin_command = [
    "cargo",
    "nextest",
    "list",
    "--profile",
    "gate",
    "--workspace",
    "--lib",
    "--bins",
    "-T",
    "json",
    "--cargo-quiet",
]
unit_bin_output = run_command(unit_bin_command, merge_stderr=False)
unit_bin_entries = parse_nextest_list(unit_bin_output, {"lib", "bin"})
unit_bin_partition_command = [
    *unit_bin_command,
    "--",
    "--skip",
    "platform_verifier_tls_client_subprocess",
    "--skip",
    "panic_hook_preserves_default_backtrace_behavior_and_persists",
]
unit_bin_partition_output = run_command(unit_bin_partition_command, merge_stderr=False)
unit_bin_shardable_entries = parse_nextest_list(
    unit_bin_partition_output, {"lib", "bin"}, matching_tests_only=True
)
unit_bin_partitions_total = unit_partition_count()
unit_bin_partitions = partition_nextest_entries(
    unit_bin_partition_command, {"lib", "bin"}, unit_bin_partitions_total
)
serial_tls_candidates = [
    entry
    for entry in unit_bin_entries
    if entry.name.endswith("platform_verifier_tls_client_subprocess")
]
serial_tls_entries = [entry for entry in serial_tls_candidates if entry.kind == "lib"]
serial_backtrace_candidates = [
    entry
    for entry in unit_bin_entries
    if entry.name.endswith("panic_hook_preserves_default_backtrace_behavior_and_persists")
]
serial_backtrace_entries = [
    entry for entry in serial_backtrace_candidates if entry.kind == "lib"
]
serial_unit_keys = {entry.key for entry in serial_tls_entries + serial_backtrace_entries}

doc_baseline_entries = [entry for entry in baseline_entries if entry.kind == "doc"]
doc_entries: list[CargoEntry] = []
if doc_baseline_entries:
    doc_output = run_command(["cargo", "test", "--workspace", "--doc", "--", "--list"])
    doc_entries = [entry for entry in parse_cargo_list(doc_output) if entry.kind == "doc"]

nextest_command = [
    "cargo",
    "nextest",
    "list",
    "--profile",
    "gate",
    "--workspace",
    "-E",
    "kind(test) - binary(=watcher_integration)",
    "-T",
    "json",
    "--cargo-quiet",
]
nextest_output = run_command(nextest_command, merge_stderr=False)
nextest_entries = parse_nextest_list(nextest_output)
nextest_shardable_entries = parse_nextest_list(nextest_output, matching_tests_only=True)
nextest_partitions = partition_nextest_entries(nextest_command)

archive_path = os.environ.get("AFT_NEXTEST_ARCHIVE_FILE")
archive_entries: list[NextestEntry] | None = None
if archive_path:
    archive_output = run_command(
        [
            "cargo",
            "nextest",
            "list",
            "--archive-file",
            archive_path,
            "--profile",
            "gate",
            "-E",
            "(kind(lib) | kind(bin) | kind(test)) - binary(=watcher_integration)",
            "-T",
            "json",
        ],
        merge_stderr=False,
    )
    archive_entries = parse_nextest_list(archive_output, {"lib", "bin", "test"})

watcher_output = run_command([
    "cargo",
    "test",
    "-p",
    "agent-file-tools",
    "--test",
    "watcher_integration",
    "--",
    "--list",
])
watcher_entries = [entry for entry in parse_cargo_list(watcher_output) if entry.kind == "test"]

baseline_by_bucket = Counter(entry.kind for entry in baseline_entries)
coverage: dict[str, list[str]] = {}

for entry in unit_bin_entries:
    coverage.setdefault(entry.key, []).append("nextest-lib-bin")
for entry in doc_entries:
    coverage.setdefault(entry.key, []).append("cargo-doc")
for entry in nextest_entries:
    coverage.setdefault(entry.key, []).append("nextest-integration")
for entry in watcher_entries:
    coverage.setdefault(entry.key, []).append("cargo-watcher")

baseline_keys = {entry.key for entry in baseline_entries}
covered_keys = set(coverage)
uncovered = sorted(key for key in baseline_keys if key not in covered_keys)
double_covered = sorted(
    (key, buckets) for key, buckets in coverage.items() if key in baseline_keys and len(buckets) > 1
)
extra_keys = sorted(key for key in covered_keys if key not in baseline_keys)
watcher_in_nextest = sorted(
    entry.key for entry in nextest_entries if entry.suite == "watcher_integration"
)
unit_partition_missing, unit_partition_duplicate, unit_partition_extra = partition_coverage_errors(
    unit_bin_shardable_entries, unit_bin_partitions
)
serial_tests_in_unit_partitions = sorted(
    entry.key
    for partition in unit_bin_partitions
    for entry in partition
    if entry.key in serial_unit_keys
)
nextest_partition_missing, nextest_partition_duplicate, nextest_partition_extra = partition_coverage_errors(
    nextest_shardable_entries, nextest_partitions
)
watcher_in_nextest_partitions = sorted(
    entry.key
    for partition in nextest_partitions
    for entry in partition
    if entry.suite == "watcher_integration"
)
archive_inventory_errors = ([], [], [])
if archive_entries is not None:
    archive_inventory_errors = partition_coverage_errors(
        unit_bin_entries + nextest_entries, [archive_entries]
    )

print("Baseline counts:", ", ".join(summarize(baseline_by_bucket)))
print(
    "Split counts:",
    ", ".join(
        [
            f"nextest-lib-bin={len(unit_bin_entries)}",
            f"cargo-doc={len(doc_entries)}",
            f"nextest-integration={len(nextest_entries)}",
            f"cargo-watcher={len(watcher_entries)}",
        ]
    ),
)
print(f"Watcher tests in nextest inventory: {len(watcher_in_nextest)}")
print(f"Doctest cases in baseline inventory: {len(doc_baseline_entries)}")
print(
    f"Hash partition counts (lib/bin hash:K/{unit_bin_partitions_total}, "
    f"integration hash:K/{INTEGRATION_PARTITION_COUNT}): "
    f"nextest-lib-bin={','.join(str(len(partition)) for partition in unit_bin_partitions)}; "
    f"nextest-integration={','.join(str(len(partition)) for partition in nextest_partitions)}"
)
if archive_entries is not None:
    print(f"Archive inventory entries: {len(archive_entries)}")

if (
    uncovered
    or double_covered
    or extra_keys
    or watcher_in_nextest
    or len(serial_tls_candidates) != 1
    or len(serial_tls_entries) != 1
    or len(serial_backtrace_candidates) != 1
    or len(serial_backtrace_entries) != 1
    or serial_tests_in_unit_partitions
    or unit_partition_missing
    or unit_partition_duplicate
    or unit_partition_extra
    or nextest_partition_missing
    or nextest_partition_duplicate
    or nextest_partition_extra
    or watcher_in_nextest_partitions
    or any(archive_inventory_errors)
):
    print("\nInventory proof failed.")
    if uncovered:
        print(f"  uncovered ({len(uncovered)}):")
        for key in uncovered[:20]:
            print(f"    {key}")
    if double_covered:
        print(f"  double-covered ({len(double_covered)}):")
        for key, buckets in double_covered[:20]:
            print(f"    {key} -> {', '.join(buckets)}")
    if extra_keys:
        print(f"  extra split-only entries ({len(extra_keys)}):")
        for key in extra_keys[:20]:
            print(f"    {key}")
    if watcher_in_nextest:
        print(f"  watcher leaked into nextest ({len(watcher_in_nextest)}):")
        for key in watcher_in_nextest[:20]:
            print(f"    {key}")
    if len(serial_tls_candidates) != 1 or len(serial_tls_entries) != 1:
        print(
            "  serial platform-verifier TLS test entries: "
            f"expected one lib target, found {len(serial_tls_candidates)} targets "
            f"({len(serial_tls_entries)} lib entries)"
        )
    if len(serial_backtrace_candidates) != 1 or len(serial_backtrace_entries) != 1:
        print(
            "  serial symbolized-backtrace test entries: "
            f"expected one lib target, found {len(serial_backtrace_candidates)} targets "
            f"({len(serial_backtrace_entries)} lib entries)"
        )
    if serial_tests_in_unit_partitions:
        print(
            "  serial-only tests present in lib/bin partitions "
            f"({len(serial_tests_in_unit_partitions)}):"
        )
        for key in serial_tests_in_unit_partitions[:20]:
            print(f"    {key}")
    for label, missing, duplicate, extra in (
        (
            "nextest lib/bin partitions",
            unit_partition_missing,
            unit_partition_duplicate,
            unit_partition_extra,
        ),
        (
            "nextest integration partitions",
            nextest_partition_missing,
            nextest_partition_duplicate,
            nextest_partition_extra,
        ),
    ):
        for category, keys in (("uncovered", missing), ("repeated", duplicate), ("extra", extra)):
            if keys:
                print(f"  {label} {category} ({len(keys)}):")
                for key in keys[:20]:
                    print(f"    {key}")
    if watcher_in_nextest_partitions:
        print(f"  watcher leaked into nextest partitions ({len(watcher_in_nextest_partitions)}):")
        for key in watcher_in_nextest_partitions[:20]:
            print(f"    {key}")
    for category, keys in zip(("uncovered", "repeated", "extra"), archive_inventory_errors):
        if keys:
            print(f"  archive inventory {category} ({len(keys)}):")
            for key in keys[:20]:
                print(f"    {key}")
    sys.exit(1)

print("\nInventory proof passed: every baseline test is covered exactly once.")
