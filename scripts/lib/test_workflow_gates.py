#!/usr/bin/env python3
"""Unit tests for GitHub Actions branch-filter glob semantics."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import unittest


MODULE_PATH = Path(__file__).with_name("workflow-gates.py")
SPEC = importlib.util.spec_from_file_location("workflow_gates", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
workflow_gates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_gates)

WORKFLOW_TEXT = (
    Path(__file__).resolve().parents[2] / ".github/workflows/_unit-suite.yml"
).read_text()
GATE_TEXT = (Path(__file__).resolve().parents[2] / "scripts/rust-test-gate.sh").read_text()
PROOF_TEXT = (Path(__file__).resolve().parents[2] / "scripts/rust-test-inventory-proof.py").read_text()


def workflow_job(job_id: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(job_id)}:\n(.*?)(?=^  [a-z0-9_-]+:\n|\Z)",
        WORKFLOW_TEXT,
    )
    if match is None:
        raise AssertionError(f"workflow job {job_id!r} is missing")
    return match.group(1)


class CacheSaveRefGateTests(unittest.TestCase):
    """A main-only cache write is policy; a main-only check or job is dead code."""

    def _scan(self, steps_yaml: str) -> list[str]:
        import tempfile

        text = (
            "on:\n  push:\n    branches: [main, 'train/**']\n"
            "concurrency:\n  group: x-${{ github.ref }}\n"
            "jobs:\n  unit:\n    runs-on: ubuntu-latest\n    steps:\n" + steps_yaml
        )
        with tempfile.NamedTemporaryFile("w", suffix=".yml", delete=False) as handle:
            handle.write(text)
        refs, _warns, _group = workflow_gates.scan(handle.name)
        return refs

    def test_main_only_cache_save_is_not_a_dead_ref_gate(self) -> None:
        refs = self._scan(
            "      - name: Save cache on main\n"
            "        if: ${{ github.ref == 'refs/heads/main' }}\n"
            "        uses: actions/cache/save@v4\n"
            "        with: {path: x, key: y}\n"
        )
        self.assertEqual(refs, [])

    def test_main_only_ordinary_step_is_still_refused(self) -> None:
        refs = self._scan(
            "      - name: Check only on main\n"
            "        if: ${{ github.ref == 'refs/heads/main' }}\n"
            "        run: ./check.sh\n"
        )
        self.assertEqual(len(refs), 1, refs)

    def test_main_only_cache_restore_is_still_refused(self) -> None:
        refs = self._scan(
            "      - name: Restore only on main\n"
            "        if: ${{ github.ref == 'refs/heads/main' }}\n"
            "        uses: actions/cache/restore@v4\n"
            "        with: {path: x, key: y}\n"
        )
        self.assertEqual(len(refs), 1, refs)


class BranchPatternTests(unittest.TestCase):
    def test_single_star_does_not_match_slash(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/fix", ["train/*"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/feature/fix", ["train/*"]))

    def test_double_star_matches_nested_branches(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/feature/fix", ["train/**"]))

    def test_question_mark_matches_exactly_one_non_slash_character(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/a", ["train/?"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/ab", ["train/?"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/a/b", ["train/?"]))

    def test_character_class_and_range(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("release/7", ["release/[0-9]"]))
        self.assertFalse(workflow_gates.branch_patterns_include("release/a", ["release/[0-9]"]))

    def test_leading_negation_excludes_then_later_positive_reincludes(self) -> None:
        patterns = ["train/**", "!train/private/**"]
        self.assertFalse(workflow_gates.branch_patterns_include("train/private/secret", patterns))
        self.assertTrue(workflow_gates.branch_patterns_include(
            "train/private/approved", [*patterns, "train/private/approved"],
        ))


class LightPREventGateTests(unittest.TestCase):
    def test_pr_exclusion_is_an_event_warning_not_a_dead_train_ref_gate(self) -> None:
        self.assertEqual(workflow_gates.classify("${{ github.event_name != 'pull_request' }}"), "warn")

    def test_pr_exclusion_does_not_hide_a_main_only_ref_gate(self) -> None:
        self.assertEqual(workflow_gates.classify(
            "${{ github.event_name != 'pull_request' && github.ref == 'refs/heads/main' }}",
        ), "ref")


class CISpeedWorkflowTests(unittest.TestCase):
    def test_release_binary_cache_requires_exact_commit_and_compiler(self) -> None:
        path = Path(__file__).resolve().parents[2] / ".github/workflows/tests.yml"
        workflow = workflow_gates.parse_workflow(str(path))
        producer = workflow["jobs"]["search-quality-binary"]
        caches = [step for step in producer["steps"] if step.get("id") == "release-binary"]
        self.assertEqual(len(caches), 1)
        cache = caches[0]
        self.assertEqual(cache["uses"], "actions/cache/restore@v4")
        self.assertEqual(cache["with"]["path"], "target/release/aft")
        self.assertIn("${{ github.sha }}", cache["with"]["key"])
        self.assertIn("${{ steps.rust.outputs.cachekey }}", cache["with"]["key"])
        self.assertNotIn("restore-keys", cache["with"])
        build = next(step for step in producer["steps"] if step.get("name") == "Build shared release AFT")
        self.assertIn("release-binary.outputs.cache-hit", build["if"])
        self.assertEqual(build["run"], "cargo build --release --locked -p agent-file-tools --bin aft")

    def test_direct_ci_storm_nextest_run_keeps_all_failures(self) -> None:
        self.assertIn("cargo nextest run --no-fail-fast", workflow_job("unit-linux-storm"))

    def test_library_does_not_embed_per_commit_build_script_environment(self) -> None:
        root = Path(__file__).resolve().parents[2] / "crates/aft"
        emitted = set(re.findall(r"cargo:rustc-env=([A-Z0-9_]+)=", (root / "build.rs").read_text()))
        self.assertIn("AFT_BUILD_GIT_SHA", emitted)
        violations = []
        for source in (root / "src").rglob("*.rs"):
            if source == root / "src/main.rs":
                continue
            for name in emitted:
                if re.search(rf'(?:option_env|env)\s*!\s*\(\s*"{re.escape(name)}"', source.read_text()):
                    violations.append(f"{source.relative_to(root)}: {name}")
        self.assertEqual(violations, [], "per-commit build data belongs in the binary, not the library")

    def test_test_builds_keep_symbolication_without_full_debug_types(self) -> None:
        workflow = workflow_gates.parse_workflow(str(
            Path(__file__).resolve().parents[2] / ".github/workflows/_unit-suite.yml",
        ))
        self.assertEqual(workflow["env"].get("CARGO_PROFILE_TEST_DEBUG"), "line-tables-only")
        self.assertEqual(workflow["env"].get("CARGO_PROFILE_DEV_DEBUG"), "line-tables-only")
        self.assertNotIn("CARGO_PROFILE_RELEASE_DEBUG", workflow["env"])
        self.assertNotIn("CARGO_PROFILE_STAGE_DEBUG", workflow["env"])

    def test_long_compiles_keep_sccache_server_alive(self) -> None:
        root = Path(__file__).resolve().parents[2]
        for name in ("tests.yml", "_unit-suite.yml", "_e2e-suite.yml"):
            with self.subTest(workflow=name):
                workflow = workflow_gates.parse_workflow(str(root / ".github/workflows" / name))
                self.assertEqual(str(workflow.get("env", {}).get("SCCACHE_IDLE_TIMEOUT")), "0")

    def test_measurement_pushes_run_the_full_tests_workflow(self) -> None:
        path = Path(__file__).resolve().parents[2] / ".github/workflows/tests.yml"
        workflow = workflow_gates.parse_workflow(str(path))
        for branch in ("main", "train/nested/fix", "ci-speed/sccache", "ci-speed/workspace"):
            with self.subTest(branch=branch):
                self.assertTrue(workflow_gates.push_matches(workflow, branch))
        self.assertNotIn("paths", workflow_gates.on_section(workflow)["push"])


class MacOSUnitWorkflowTests(unittest.TestCase):
    def test_unit_archive_compilation_isolated_and_real_cli_is_retained(self) -> None:
        lib = workflow_job("unit-macos-libtest-archive")
        binary = workflow_job("unit-macos-bin-archive")
        self.assertIn("--workspace --lib --test sandbox_launch_probe", lib)
        self.assertIn("--workspace --bins --test sandbox_launch_probe", binary)
        self.assertIn("-E 'kind(lib)'", lib)
        self.assertIn("-E 'kind(bin)'", binary)

    def test_required_macos_check_aggregates_every_lane(self) -> None:
        summary = workflow_job("unit-macos")
        expected_lanes = {
            "unit-macos-nextest-archive",
            "unit-macos-libtest-archive",
            "unit-macos-bin-archive",
            "unit-macos-inventory-proof",
            "unit-macos-libtest-shard",
            "unit-macos-nextest",
            "unit-macos-watcher",
            "unit-macos-conformance",
        }

        self.assertIn("name: Unit tests (macOS)", summary)
        self.assertRegex(summary, r"(?m)^    if: always\(\)$")
        for lane in expected_lanes:
            self.assertIn(f"- {lane}", summary)
            self.assertIn(f"needs.{lane}.result", summary)
        self.assertIn('if [[ "$result" != "success" ]]', summary)

    def test_macos_rust_buckets_and_bun_suite_run_in_parallel_lanes(self) -> None:
        integration_archive = workflow_job("unit-macos-nextest-archive")
        self.assertIn("scripts/ci-macos-integration-archive.py archive", integration_archive)
        self.assertIn("scripts/ci-macos-integration-archive.py list", integration_archive)
        self.assertIn('SCCACHE_IDLE_TIMEOUT: "0"', integration_archive)
        self.assertIn("macos-integration-inventory.json", integration_archive)
        self.assertIn("macos-doctest-inventory.txt", integration_archive)

        libtest_archive = workflow_job("unit-macos-libtest-archive")
        self.assertIn("kind(lib)", libtest_archive)
        self.assertNotIn("kind(bin)", libtest_archive)
        self.assertIn('SCCACHE_IDLE_TIMEOUT: "0"', libtest_archive)
        self.assertIn("macos-libtest-inventory.json", libtest_archive)

        bin_archive = workflow_job("unit-macos-bin-archive")
        self.assertIn("kind(bin)", bin_archive)
        self.assertIn('SCCACHE_IDLE_TIMEOUT: "0"', bin_archive)
        self.assertIn("macos-bin-inventory.json", bin_archive)

        inventory = workflow_job("unit-macos-inventory-proof")
        self.assertIn("AFT_NEXTEST_ARCHIVE_LIB_FILE", inventory)
        self.assertIn("AFT_NEXTEST_ARCHIVE_BIN_FILE", inventory)
        self.assertIn("AFT_NEXTEST_ARCHIVE_INTEGRATION_FILE", inventory)
        self.assertIn("AFT_NEXTEST_SOURCE_INVENTORY_LIB_FILE", inventory)
        self.assertIn("AFT_NEXTEST_SOURCE_INVENTORY_BIN_FILE", inventory)
        self.assertIn("AFT_NEXTEST_SOURCE_INVENTORY_INTEGRATION_FILE", inventory)
        self.assertIn("AFT_WATCHER_TEST_INVENTORY_FILE", inventory)
        self.assertIn("scripts/rust-test-inventory-proof.py", inventory)

        libtest = workflow_job("unit-macos-libtest-shard")
        self.assertIn("unit-macos-libtest-archive", libtest)
        self.assertIn("unit-macos-bin-archive", libtest)
        self.assertIn("macos-libtest-archive", libtest)
        self.assertIn("macos-bin-archive", libtest)
        self.assertIn("max-parallel: 3", libtest)
        self.assertIn("AFT_NEXTEST_ARCHIVE_BIN_FILE", libtest)
        self.assertIn("AFT_GATE_PHASES: ${{ matrix.shard == 1 && 'lib,lib-shard' || 'lib-shard' }}", libtest)
        self.assertIn("AFT_NEXTEST_PARTITION: hash:${{ matrix.shard }}/3", libtest)
        self.assertIn("AFT_NEXTEST_EXTRACT_TO: ${{ github.workspace }}", libtest)

        integration = workflow_job("unit-macos-nextest")
        self.assertIn("max-parallel: 3", integration)
        self.assertIn("AFT_GATE_PHASES: nextest", integration)
        self.assertIn("AFT_NEXTEST_PARTITION: hash:${{ matrix.shard }}/3", integration)
        self.assertIn("macos-nextest-archive", integration)

        watcher = workflow_job("unit-macos-watcher")
        self.assertIn("AFT_GATE_PHASES: watcher", watcher)

        conformance = workflow_job("unit-macos-conformance")
        self.assertIn("AFT_GATE_PHASES: conformance", conformance)
        self.assertIn("bun run test:unit", conformance)

    def test_macos_libtest_archives_are_prewarmed_before_keychain_test(self) -> None:
        prewarm_start = GATE_TEXT.index("if phase_enabled lib-shard")
        prewarm = GATE_TEXT.index("extract nextest archive for macOS libtest prewarming")
        warm = GATE_TEXT.index("./scripts/warm-macos-nextest-archive.sh", prewarm_start)
        keychain = GATE_TEXT.index("if phase_enabled lib; then")

        self.assertLess(prewarm_start, prewarm)
        self.assertLess(prewarm, keychain)
        self.assertLess(warm, keychain)
        self.assertIn("extract_nextest_archive_for_prewarm", GATE_TEXT[prewarm_start:keychain])
        self.assertIn("AFT_NEXTEST_ARCHIVE_BIN_FILE", GATE_TEXT[prewarm_start:keychain])

    def test_macos_backtrace_probe_runs_once_in_archive_builder_not_shards(self) -> None:
        probe = "cargo test -p agent-file-tools --lib panic_hook_preserves_default_backtrace_behavior_and_persists"
        archive = workflow_job("unit-macos-libtest-archive")
        shard = workflow_job("unit-macos-libtest-shard")

        self.assertEqual(archive.count(probe), 1)
        self.assertNotIn(probe, shard)
        self.assertIn(probe, GATE_TEXT)
        self.assertIn('"$(uname)" != "Darwin"', GATE_TEXT)
        self.assertNotIn(
            "cargo nextest run --archive-file $AFT_NEXTEST_ARCHIVE_FILE -E kind(lib) panic_hook_preserves_default_backtrace_behavior_and_persists",
            GATE_TEXT,
        )
        self.assertIn(
            "serial_backtrace_candidates",
            PROOF_TEXT,
        )
        self.assertIn("serial_tests_in_unit_partitions", PROOF_TEXT)
        self.assertIn("Serial-probe inventory:", PROOF_TEXT)

    def test_macos_inventory_proof_checks_source_archives_and_partitions(self) -> None:
        inventory = workflow_job("unit-macos-inventory-proof")
        for variable in (
            "AFT_NEXTEST_ARCHIVE_LIB_FILE",
            "AFT_NEXTEST_ARCHIVE_BIN_FILE",
            "AFT_NEXTEST_ARCHIVE_INTEGRATION_FILE",
            "AFT_NEXTEST_SOURCE_INVENTORY_LIB_FILE",
            "AFT_NEXTEST_SOURCE_INVENTORY_BIN_FILE",
            "AFT_NEXTEST_SOURCE_INVENTORY_INTEGRATION_FILE",
            "AFT_WATCHER_TEST_INVENTORY_FILE",
            "AFT_DOCTEST_INVENTORY_FILE",
        ):
            self.assertIn(variable, inventory)
        self.assertIn("AFT_NEXTEST_ARCHIVE_LIB_FILE", inventory)
        self.assertIn("AFT_NEXTEST_ARCHIVE_BIN_FILE", inventory)
        self.assertIn('"lib": partition_coverage_errors(lib_entries, [lib_archive_entries])', PROOF_TEXT)
        self.assertIn('"bin": partition_coverage_errors(bin_entries, [bin_archive_entries])', PROOF_TEXT)
        self.assertIn('lib_partitions = partition_nextest_entries(lib_command, {"lib"})', PROOF_TEXT)
        self.assertIn('bin_partitions = partition_nextest_entries(bin_command, {"bin"})', PROOF_TEXT)
        self.assertIn("any(unexpected_archives.values())", PROOF_TEXT)
        self.assertIn("or any(source_archive_errors)", PROOF_TEXT)
        self.assertIn("any(partition_errors)", PROOF_TEXT)
        self.assertIn("unit_partition_errors = partition_coverage_errors(", PROOF_TEXT)
        self.assertIn("unit_shardable_entries, unit_partitions", PROOF_TEXT)
        self.assertIn("integration_partition_errors = partition_coverage_errors(", PROOF_TEXT)
        self.assertIn("integration_shardable_entries, integration_partitions", PROOF_TEXT)

    def test_macos_conformance_starts_with_its_own_build_not_archive_artifacts(self) -> None:
        conformance = workflow_job("unit-macos-conformance")
        dependencies = conformance.split("    steps:", 1)[0]

        self.assertNotIn("needs:", dependencies)
        self.assertNotIn("actions/download-artifact", conformance)
        self.assertIn("python3 scripts/check-path-deps.py", conformance)
        self.assertIn("cargo build -p agent-file-tools", conformance)
        self.assertIn("AFT_BINARY_PATH: ${{ github.workspace }}/target/debug/aft", conformance)


if __name__ == "__main__":
    unittest.main()
