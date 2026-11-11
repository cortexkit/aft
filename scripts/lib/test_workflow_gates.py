#!/usr/bin/env python3
"""Unit tests for GitHub Actions branch-filter glob semantics."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import textwrap
import unittest
import unittest.mock


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
    def test_worker_payload_probe_label_names_the_explicit_stdio_contract(self) -> None:
        root = Path(__file__).resolve().parents[2]
        source = (root / "crates/aft/tests/sandbox_launch_probe.rs").read_text()
        self.assertTrue("fn task_store_paths_are_denied_while_explicit_stdin_remains_readable()" in source,
                        "the payload probe must name its explicit-stdin contract")
        self.assertFalse("fn task_store_paths_are_denied_while_held_payload_fd_remains_readable()" in source,
                         "the old high-FD inheritance label must not overstate this probe")

    def test_worker_read_landlock_is_required_and_emitted_for_every_main_event(self) -> None:
        root = Path(__file__).resolve().parents[2]
        required = (root / ".github/required-checks.txt").read_text().splitlines()
        self.assertIn("Unit / Worker GitHub reads (required Landlock)", required)
        workflow = workflow_gates.parse_workflow(str(root / ".github/workflows/tests.yml"))
        for event in ("pull_request", "push"):
            config = workflow["on"][event]
            self.assertIn("main", config["branches"])
            self.assertNotIn("paths", config)
            self.assertNotIn("paths-ignore", config)
        self.assertNotIn("if", workflow["jobs"]["unit"])
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        job = suite["jobs"]["worker-gh-read-landlock"]
        self.assertEqual(job["name"], "Worker GitHub reads (required Landlock)")
        # Like every other unit leaf job it skips pull requests and runs in the
        # merge queue and on main, so every commit that lands has passed it.
        self.assertEqual(job["if"], "${{ github.event_name != 'pull_request' }}")
        self.assertEqual(str(job["env"]["AFT_REQUIRE_LANDLOCK"]), "1")

    def test_pr_exclusion_is_an_event_warning_not_a_dead_train_ref_gate(self) -> None:
        self.assertEqual(workflow_gates.classify("${{ github.event_name != 'pull_request' }}"), "warn")

    def test_pr_exclusion_does_not_hide_a_main_only_ref_gate(self) -> None:
        self.assertEqual(workflow_gates.classify(
            "${{ github.event_name != 'pull_request' && github.ref == 'refs/heads/main' }}",
        ), "ref")

    def test_prs_report_all_27_required_contexts_including_the_live_worker_gate(self) -> None:
        root = Path(__file__).resolve().parents[2]
        required = {
            line for line in (root / ".github/required-checks.txt").read_text().splitlines()
            if line.startswith(("Unit / ", "E2E / "))
        }
        self.assertEqual(len(required), 27)
        jobs = {}
        for caller, file in (("Unit", "_unit-suite.yml"), ("E2E", "_e2e-suite.yml")):
            suite = workflow_gates.parse_workflow(str(root / ".github/workflows" / file))
            for job in suite["jobs"].values():
                name = f"{caller} / {job['name']}"
                self.assertNotIn(name, jobs)
                jobs[name] = job
        for context in required:
            with self.subTest(context=context):
                self.assertIn(context, jobs)
                self.assertNotIn("strategy", jobs[context])
                self.assertIn(jobs[context]["if"], (
                    "${{ github.event_name != 'pull_request' }}",
                    "${{ github.event_name != 'pull_request' || inputs.pr_platform_advisory }}",
                    "${{ always() && github.event_name != 'pull_request' }}",
                ))

    def test_static_integration_shards_share_one_partition_action(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        wrappers = []
        for job in suite["jobs"].values():
            steps = job.get("steps", [])
            for step in steps:
                if step.get("uses") == "./.github/actions/integration-shard":
                    wrappers.append((job, step))
        self.assertEqual(len(wrappers), 9)
        expected = {(os, str(shard)) for os in ("linux", "macos", "windows") for shard in (1, 2, 3)}
        actual = {(step["with"]["platform"], step["with"]["shard"]) for _, step in wrappers}
        self.assertEqual(actual, expected)
        for job, step in wrappers:
            platform, shard = step["with"]["platform"], step["with"]["shard"]
            label, runner, timeout = {
                "linux": ("Linux", "ubuntu-22.04", "20"),
                "macos": ("macOS", ["self-hosted", "rented-mac"], "20"),
                "windows": ("Windows", ["self-hosted", "narli"], "25"),
            }[platform]
            with self.subTest(platform=platform, shard=shard):
                self.assertEqual(job["name"], f"Cargo integration shard {shard}/3 ({label})")
                self.assertEqual(job["runs-on"], runner)
                self.assertEqual(job["timeout-minutes"], timeout)
                self.assertEqual(job["needs"], f"unit-{platform}-nextest-archive")
                self.assertNotIn("strategy", job)
                if platform == "linux":
                    self.assertEqual(job["steps"], [{"uses": "actions/checkout@v5"}, step])
                else:
                    self.assertEqual(job["steps"][0], {"uses": "actions/checkout@v5"})
                    self.assertEqual(job["steps"][1]["uses"], "actions/setup-python@v6")
                    self.assertEqual(job["steps"][2], {"uses": "./.github/actions/native-ci-state"})
                    self.assertEqual(job["steps"][3]["uses"], "oven-sh/setup-bun@v2")
                    self.assertEqual(job["steps"][4]["uses"], "actions/setup-node@v5")
                    self.assertEqual(job["steps"][5], step)
                    self.assertEqual(job["steps"][-1]["with"], {"phase": "cleanup"})
        action = workflow_gates.parse_workflow(str(root / ".github/actions/integration-shard/action.yml"))
        self.assertEqual(action["runs"]["using"], "composite")
        steps = action["runs"]["steps"]
        gate_steps = [step for step in steps if "./scripts/rust-test-gate.sh" in step.get("run", "")]
        self.assertEqual(len(gate_steps), 2)
        self.assertEqual({step["if"] for step in gate_steps}, {"inputs.platform == 'windows'", "inputs.platform != 'windows'"})
        for step in gate_steps:
            self.assertEqual(step["shell"], "bash")
            self.assertEqual(step["env"]["AFT_NEXTEST_PARTITION"], "hash:${{ inputs.shard }}/3")
            self.assertEqual(step["env"]["AFT_GATE_PHASES"], "nextest")
            self.assertEqual(step["env"]["AFT_GATE_SKIP_RELEASE_STORM"], "1")
        download = next(step for step in steps if step.get("uses") == "./.github/actions/download-artifact-retry")
        self.assertEqual(download["with"]["name"], "${{ inputs.platform }}-nextest-archive")


class CISpeedWorkflowTests(unittest.TestCase):
    def test_release_binary_cache_requires_exact_commit_and_compiler(self) -> None:
        path = Path(__file__).resolve().parents[2] / ".github/workflows/tests.yml"
        workflow = workflow_gates.parse_workflow(str(path))
        producer = workflow["jobs"]["search-quality-binary"]
        caches = [step for step in producer["steps"] if step.get("id") == "release-binary"]
        self.assertEqual(len(caches), 1)
        cache = caches[0]
        self.assertEqual(cache["uses"], "actions/cache/restore@v4")
        self.assertEqual(cache["with"]["path"], "target/ci-storm/aft")
        self.assertIn("${{ github.sha }}", cache["with"]["key"])
        self.assertIn("${{ steps.rust.outputs.cachekey }}", cache["with"]["key"])
        self.assertNotIn("restore-keys", cache["with"])
        build = next(step for step in producer["steps"] if step.get("name") == "Build shared search-quality AFT")
        self.assertIn("release-binary.outputs.cache-hit", build["if"])
        self.assertEqual(build["run"], "cargo build --profile ci-storm --locked -p agent-file-tools --bin aft")
        # The gate must read the same profile's binary it downloads, and its
        # fallback build must produce it, or the benchmark would score a
        # binary of a different build than the producer measured.
        gate = workflow["jobs"]["search-quality-gate"]
        download = next(step for step in gate["steps"] if step.get("id") == "download-release-aft")
        self.assertEqual(download["with"]["path"], "target/ci-storm")
        fallback = next(step for step in gate["steps"] if step.get("name") == "Build the product binary when the artifact is absent")
        self.assertEqual(fallback["run"], build["run"])
        evaluate = next(step for step in gate["steps"] if step.get("name") == "Evaluate committed search-quality reference")
        self.assertEqual(evaluate["env"]["AFT_BINARY_PATH"], "${{ github.workspace }}/target/ci-storm/aft")

    def test_direct_ci_storm_nextest_run_keeps_all_failures(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action = workflow_gates.parse_workflow(str(root / ".github/actions/storm-suite/action.yml"))
        commands = "\n".join(step.get("run", "") for step in action["runs"]["steps"])
        self.assertIn("cargo nextest run --no-fail-fast", commands)
        self.assertIn("./.github/actions/storm-suite", workflow_job("unit-linux-storm"))

    def test_nextest_install_requires_cache_retry_and_published_checksum(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action_path = root / ".github/actions/setup-nextest/action.yml"
        action = workflow_gates.parse_workflow(str(action_path))
        self.assertEqual(action["inputs"]["version"]["default"], "0.9.138")
        steps = action["runs"]["steps"]

        cache = next(step for step in steps if step.get("id") == "nextest-cache")
        self.assertEqual(cache["uses"], "actions/cache@v4")
        self.assertEqual(cache["with"]["path"], "~/.cargo/bin/cargo-nextest*")
        self.assertIn("cargo-nextest-${{ inputs.version }}-${{ runner.os }}", cache["with"]["key"])
        self.assertIn("${{ runner.arch }}", cache["with"]["key"])

        install = next(step for step in steps if step.get("name") == "Download and install cargo-nextest")
        self.assertEqual(install["if"], "steps.nextest-cache.outputs.cache-hit != 'true'")
        script = install["run"]
        self.assertIn("--retry-all-errors", script)
        self.assertIn("--retry-max-time 180", script)
        self.assertIn('--output "$archive"', script)
        self.assertIn('test -s "$archive"', script)
        self.assertIn("sha256sum --check --status", script)
        self.assertIn("shasum -a 256 --check --status", script)
        for digest in (
            "3793bf0c27607b196f502c39b2108f571de89fcda7586ae6beefa11ee177b216",
            "124baa34d160355fa8a8bed053856270cfcf132659e76a0bc8bbcdfc093266b7",
            "e4606198dd067299f16863dcf5b9be007b9cae041e468c417884fc224760c0f4",
        ):
            self.assertIn(digest, script)

    def test_nextest_tar_uses_relative_archive_paths_for_runner_temp(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action = workflow_gates.parse_workflow(str(root / ".github/actions/setup-nextest/action.yml"))
        install = next(step for step in action["runs"]["steps"] if step.get("name") == "Download and install cargo-nextest")
        script = install["run"]
        tar_calls = [
            line.strip()
            for line in script.splitlines()
            if re.search(r"\btar\s+-(?:t|x)zf\b", line)
        ]
        self.assertEqual(len(tar_calls), 2)
        self.assertIn('cd "$RUNNER_TEMP"', script)
        self.assertLess(script.index('cd "$RUNNER_TEMP"'), script.index("tar -tzf"))
        for call in tar_calls:
            self.assertIn('"$archive_name"', call)
            self.assertNotIn("$RUNNER_TEMP", call)
            self.assertNotIn('"$archive"', call)

    def test_tool_downloads_are_never_piped_to_tar(self) -> None:
        root = Path(__file__).resolve().parents[2] / ".github"
        violations = []
        paths = (
            *root.glob("workflows/*.yml"),
            *root.glob("workflows/*.yaml"),
            *root.glob("actions/**/*.yml"),
            *root.glob("actions/**/*.yaml"),
        )
        for path in paths:
            text = path.read_text().replace("\\\n", " ")
            if re.search(r"(?im)\b(?:curl|wget)\b[^\n]*\|\s*tar\b", text):
                violations.append(str(path.relative_to(root.parent)))
        self.assertEqual(violations, [], "download archives to a file before validating and extracting them")

    def test_nextest_has_one_shared_installer_for_every_workflow_site(self) -> None:
        root = Path(__file__).resolve().parents[2]
        github = root / ".github"
        download_sites = [
            path.relative_to(root).as_posix()
            for path in github.rglob("*.yml")
            if "get.nexte.st/" in path.read_text()
        ]
        self.assertEqual(download_sites, [".github/actions/setup-nextest/action.yml"])

        unit = workflow_gates.parse_workflow(str(github / "workflows/_unit-suite.yml"))
        uses = [
            step.get("uses")
            for job in unit["jobs"].values()
            for step in job.get("steps", [])
        ]
        self.assertEqual(uses.count("./.github/actions/setup-nextest"), 15)
        shard = workflow_gates.parse_workflow(str(github / "actions/integration-shard/action.yml"))
        self.assertEqual(
            sum(step.get("uses") == "./.github/actions/setup-nextest" for step in shard["runs"]["steps"]),
            1,
        )

    def test_pyright_npm_install_retries_transient_registry_errors(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action = workflow_gates.parse_workflow(str(root / ".github/actions/integration-shard/action.yml"))
        install = next(step for step in action["runs"]["steps"] if step.get("name") == "Install Pyright integration dependency")
        command = install["run"]
        self.assertIn("pyright@1.1.411", command)
        self.assertIn("--fetch-retries=5", command)
        self.assertIn("--fetch-retry-mintimeout=10000", command)
        self.assertIn("--fetch-retry-maxtimeout=60000", command)

    def test_library_does_not_embed_binary_build_revision(self) -> None:
        root = Path(__file__).resolve().parents[2] / "crates/aft"
        self.assertFalse((root / "build.rs").exists(), "build scripts disable content-keyed shared builds")
        emitted = {"AFT_BUILD_GIT_SHA"}
        self.assertIn('option_env!("AFT_BUILD_GIT_SHA")', (root / "src/main.rs").read_text())
        violations = []
        for source in (root / "src").rglob("*.rs"):
            if source == root / "src/main.rs":
                continue
            for name in emitted:
                if re.search(rf'(?:option_env|env)\s*!\s*\(\s*"{re.escape(name)}"', source.read_text()):
                    violations.append(f"{source.relative_to(root)}: {name}")
        self.assertEqual(violations, [], "per-commit build data belongs in the binary, not the library")

    def test_stage_card_supplies_the_binary_build_revision(self) -> None:
        root = Path(__file__).resolve().parents[2]
        stage_card = (root / "scripts/stage-card.sh").read_text()
        export = 'export AFT_BUILD_GIT_SHA="$(git rev-parse HEAD)"'
        build = "cargo build --release -p agent-file-tools --bin aft"
        self.assertIn(export, stage_card)
        self.assertLess(stage_card.index(export), stage_card.index(build))

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


class RustPRAdvisoryWorkflowTests(unittest.TestCase):
    def test_advisory_platform_shards_reuse_the_suite_and_are_never_required(self) -> None:
        root = Path(__file__).resolve().parents[2]
        advisory = workflow_gates.parse_workflow(str(root / ".github/workflows/rust-pr-advisory.yml"))
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        required = (root / ".github/required-checks.txt").read_text().splitlines()

        pull_request = advisory["on"]["pull_request"]
        self.assertEqual(pull_request["branches"], ["main"])
        self.assertEqual(set(pull_request["paths"]), {
            "crates/**", "Cargo.toml", "Cargo.lock", "rust-toolchain*",
            ".github/workflows/_unit-suite.yml",
        })
        self.assertEqual(advisory["concurrency"]["group"], "rust-pr-advisory-${{ github.event.pull_request.number }}")
        self.assertEqual(str(advisory["concurrency"]["cancel-in-progress"]).lower(), "true")

        caller = advisory["jobs"]["platform-rust-tests"]
        self.assertEqual(caller["name"], "Rust PR advisory")
        self.assertEqual(caller["uses"], "./.github/workflows/_unit-suite.yml")
        self.assertEqual(str(caller["with"]["pr_platform_advisory"]).lower(), "true")
        self.assertNotIn("if", caller)
        self.assertFalse(any(caller["name"] in name for name in required))
        suite_input = suite["on"]["workflow_call"]["inputs"]["pr_platform_advisory"]
        self.assertEqual(str(suite_input["required"]).lower(), "false")
        self.assertEqual(str(suite_input["default"]).lower(), "false")

        platform_jobs = (
            "unit-macos-nextest-archive", "unit-macos-libtest-archive", "unit-macos-bin-archive",
            "unit-macos-nextest", "unit-macos-nextest-2", "unit-macos-nextest-3",
            "unit-macos-libtest-shard", "unit-windows-nextest-archive", "unit-windows-libtest-shard",
            "unit-windows-cargo", "unit-windows-cargo-2", "unit-windows-cargo-3",
        )
        for job_id in platform_jobs:
            with self.subTest(job=job_id):
                self.assertEqual(
                    suite["jobs"][job_id]["if"],
                    "${{ github.event_name != 'pull_request' || inputs.pr_platform_advisory }}",
                )
        self.assertEqual(
            suite["jobs"]["unit-macos-watcher"]["if"],
            "${{ github.event_name != 'pull_request' }}",
        )
        self.assertEqual(
            suite["jobs"]["unit-windows-watcher"]["if"],
            "${{ github.event_name != 'pull_request' }}",
        )


class LinuxUnitWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        root = Path(__file__).resolve().parents[2]
        self.jobs = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))["jobs"]

    def test_linux_archive_covers_units_and_proves_the_uploaded_inventory(self) -> None:
        archive = self.jobs["unit-linux-nextest-archive"]
        commands = "\n".join(step.get("run", "") for step in archive["steps"])
        self.assertIn("(kind(lib) | kind(bin) | kind(test)) - binary(=watcher_integration)", commands)
        proof = next(step for step in archive["steps"] if step.get("run") == "python3 scripts/rust-test-inventory-proof.py")
        self.assertEqual(proof["env"]["AFT_NEXTEST_ARCHIVE_FILE"], "${{ runner.temp }}/linux-nextest.tar.zst")
        for probe in ("platform_verifier_tls_client_subprocess", "panic_hook_preserves_default_backtrace_behavior_and_persists"):
            steps = [step for step in archive["steps"] if probe in step.get("run", "")]
            self.assertEqual(len(steps), 1)
            self.assertIn('cargo nextest run --archive-file "$LINUX_NEXTEST_ARCHIVE"', steps[0]["run"])
            self.assertNotIn("--partition", steps[0]["run"])

    def test_linux_unit_shards_only_execute_archive_partitions(self) -> None:
        shard = self.jobs["unit-linux-libtest-shard"]
        self.assertEqual(shard["needs"], "unit-linux-nextest-archive")
        self.assertEqual(shard["strategy"]["matrix"]["shard"], ["1", "2", "3"])
        self.assertEqual(shard["strategy"]["fail-fast"], "false")
        self.assertEqual(shard["runs-on"], "ubuntu-24.04")
        gate = next(step for step in shard["steps"] if step.get("run") == "./scripts/rust-test-gate.sh")
        self.assertEqual(gate["env"]["AFT_GATE_PHASES"], "lib-shard")
        self.assertEqual(gate["env"]["AFT_UNIT_TEST_RUNNER"], "nextest")
        self.assertEqual(gate["env"]["AFT_NEXTEST_PARTITION"], "hash:${{ matrix.shard }}/3")
        self.assertEqual(gate["env"]["AFT_NEXTEST_ARCHIVE_FILE"], "${{ runner.temp }}/linux-nextest.tar.zst")
        download = next(step for step in shard["steps"] if step.get("uses") == "./.github/actions/download-artifact-retry")
        self.assertEqual(download["with"]["name"], "linux-nextest-archive")

    def test_required_linux_check_aggregates_shards_and_preserves_isolated_phases(self) -> None:
        summary = self.jobs["unit-linux-summary"]
        self.assertEqual(summary["name"], "Unit tests (Linux)")
        self.assertEqual(summary["if"], "${{ always() && github.event_name != 'pull_request' }}")
        self.assertEqual(set(summary["needs"]), {
            "unit-linux", "unit-linux-nextest-archive", "unit-linux-libtest-shard",
            "unit-linux-nextest", "unit-linux-nextest-2", "unit-linux-nextest-3",
        })
        check = next(step for step in summary["steps"] if "needs" in step.get("env", {}).get("NEEDS", ""))
        self.assertIn("result", check["run"])
        self.assertIn("success", check["run"])
        for result in ("success", "failure", "cancelled", "skipped"):
            with self.subTest(result=result):
                needs = {name: {"result": "success"} for name in summary["needs"]}
                needs["unit-linux-libtest-shard"]["result"] = result
                proc = subprocess.run(
                    ["bash", "-c", check["run"]], capture_output=True, text=True,
                    env={**os.environ, "NEEDS": json.dumps(needs)},
                )
                self.assertEqual(proc.returncode == 0, result == "success", proc.stdout + proc.stderr)
        gate = next(step for step in self.jobs["unit-linux"]["steps"] if step.get("run") == "./scripts/rust-test-gate.sh")
        self.assertEqual(gate["env"]["AFT_GATE_PHASES"], "watcher,conformance")


class WindowsUnitWorkflowTests(unittest.TestCase):
    def test_windows_defender_tuning_is_shared_and_hosted_only(self) -> None:
        root = Path(__file__).resolve().parents[2]
        path = root / ".github/actions/windows-ci-defender/action.yml"
        action = workflow_gates.parse_workflow(str(path))
        self.assertEqual(action["runs"]["using"], "composite")
        steps = action["runs"]["steps"]
        self.assertEqual(len(steps), 1)
        self.assertEqual(steps[0]["shell"], "pwsh")
        self.assertEqual(steps[0]["if"], "runner.environment == 'github-hosted'")
        script = steps[0]["run"]
        self.assertIn("$env:RUNNER_ENVIRONMENT -ne 'github-hosted'", script)
        self.assertLess(script.index("throw"), script.index("Add-MpPreference"))
        for path in ("$env:GITHUB_WORKSPACE", "$env:TEMP", "$env:RUNNER_TEMP", "$env:USERPROFILE"):
            self.assertIn(path, script)
        self.assertIn("'.cargo'", script)
        self.assertIn("Add-MpPreference -ExclusionPath $paths", script)
        self.assertIn("Set-MpPreference -DisableRealtimeMonitoring $true", script)
        self.assertIn("Get-MpComputerStatus", script)
        self.assertNotIn("SilentlyContinue", script)

    def test_every_windows_ci_lane_retains_the_hosted_only_defender_action(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action = "./.github/actions/windows-ci-defender"
        calls = []
        for filename in ("_unit-suite.yml", "_e2e-suite.yml"):
            suite = workflow_gates.parse_workflow(str(root / ".github/workflows" / filename))
            for name, job in suite["jobs"].items():
                steps = job.get("steps", [])
                direct = [i for i, step in enumerate(steps) if step.get("uses") == action]
                if job.get("runs-on") != ["self-hosted", "narli"]:
                    self.assertEqual(direct, [], name)
                    continue
                shared = [step for step in steps if step.get("uses") == "./.github/actions/integration-shard"]
                if shared:
                    self.assertEqual(shared[0]["with"]["platform"], "windows")
                else:
                    self.assertEqual(direct, [5], name)
                    self.assertNotIn("continue-on-error", steps[5])
                    calls.append(name)
        self.assertEqual(set(calls), {
            "unit-windows-nextest-archive", "unit-windows-libtest-shard", "unit-windows-watcher",
            "unit-windows-check", "unit-windows-bash", "e2e-windows",
        })
        shared = workflow_gates.parse_workflow(str(root / ".github/actions/integration-shard/action.yml"))
        step = shared["runs"]["steps"][0]
        self.assertEqual(step["uses"], action)
        self.assertEqual(step["if"], "inputs.platform == 'windows'")
        self.assertNotIn("continue-on-error", step)

    def test_windows_serial_probes_run_once_in_archive_builder(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        archive = suite["jobs"]["unit-windows-nextest-archive"]
        commands = [step.get("run", "") for step in archive["steps"]]
        build = next(i for i, command in enumerate(commands) if command.startswith("cargo nextest archive "))
        proof = commands.index("python scripts/rust-test-inventory-proof.py")

        for probe in ("tls", "backtrace"):
            command = f'python scripts/ci-windows-probes.py --archive-file "$env:WINDOWS_NEXTEST_ARCHIVE" --probe {probe}'
            matches = [i for i, script in enumerate(commands) if command in script.splitlines()]
            self.assertEqual(len(matches), 1, f"archive must run {probe} exactly once")
            self.assertLess(build, matches[0])
            self.assertLess(matches[0], proof)
            for job_id in suite["jobs"]:
                if job_id.startswith("unit-windows-") and job_id != "unit-windows-nextest-archive":
                    self.assertNotIn("ci-windows-probes.py", workflow_job(job_id))
        self.assertFalse(any(line.startswith("cargo test ") for command in commands for line in command.splitlines()))
        python = next(i for i, step in enumerate(archive["steps"]) if step.get("uses") == "actions/setup-python@v6")
        tls = next(i for i, step in enumerate(archive["steps"]) if "--probe tls" in step.get("run", ""))
        self.assertEqual(archive["steps"][python]["with"]["python-version"], "3.14")
        self.assertLess(python, tls)

    def test_windows_libtest_shards_select_only_archive_partitions(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        shard = suite["jobs"]["unit-windows-libtest-shard"]
        gate = next(step for step in shard["steps"] if "./scripts/rust-test-gate.sh" in step.get("run", ""))
        self.assertEqual(gate["env"]["AFT_GATE_PHASES"], "lib-shard")
        self.assertEqual(gate["env"]["AFT_UNIT_TEST_RUNNER"], "nextest")
        self.assertEqual(gate["env"]["AFT_NEXTEST_PARTITION"], "hash:${{ matrix.shard }}/6")
        self.assertEqual(gate["env"]["AFT_NEXTEST_ARCHIVE_FILE"], "${{ env.AFT_CI_ARCHIVE_DIR }}/windows-nextest.tar.zst")
        self.assertEqual(shard["needs"], "unit-windows-nextest-archive")


class UnitShardPartitionCountTests(unittest.TestCase):
    """Each inventory proof must partition lib/bin tests the way its shards do.

    The shard matrix size, the nextest partition spec and the proof's
    AFT_UNIT_PARTITION_COUNT are written separately in the workflow. If they
    disagree, shards may skip tests or the proof may check a split that no
    shard runs. The shard job's displayed name must also show the matrix size.
    """

    # (libtest shard job, job whose step runs the inventory proof)
    LANES = (
        ("unit-linux-libtest-shard", "unit-linux-nextest-archive"),
        ("unit-macos-libtest-shard", "unit-macos-inventory-proof"),
        ("unit-windows-libtest-shard", "unit-windows-nextest-archive"),
    )

    def setUp(self) -> None:
        root = Path(__file__).resolve().parents[2]
        self.jobs = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))["jobs"]

    def test_proof_partition_count_matches_each_shard_matrix(self) -> None:
        for shard_job, proof_job in self.LANES:
            with self.subTest(shard_job=shard_job):
                job = self.jobs[shard_job]
                shards = job["strategy"]["matrix"]["shard"]
                count = len(shards)
                self.assertEqual(shards, [str(index) for index in range(1, count + 1)])
                self.assertEqual(job["strategy"]["max-parallel"], str(count))
                self.assertTrue(
                    job["name"].startswith(f"Cargo libtest shard ${{{{ matrix.shard }}}}/{count} ("),
                    job["name"],
                )
                gate = next(step for step in job["steps"] if "./scripts/rust-test-gate.sh" in step.get("run", ""))
                self.assertEqual(gate["env"]["AFT_NEXTEST_PARTITION"], f"hash:${{{{ matrix.shard }}}}/{count}")
                proof = next(
                    step for step in self.jobs[proof_job]["steps"]
                    if "scripts/rust-test-inventory-proof.py" in step.get("run", "")
                )
                self.assertEqual(proof["env"].get("AFT_UNIT_PARTITION_COUNT"), str(count))

    def _proof_partition_functions(self, list_calls: list[list[str]]) -> dict:
        """Load the proof's partition helpers with cargo replaced by a recorder."""
        import ast

        tree = ast.parse(PROOF_TEXT)
        wanted = {"INTEGRATION_PARTITION_COUNT", "unit_partition_count", "partition_nextest_entries"}
        nodes = [
            node for node in tree.body
            if (isinstance(node, ast.FunctionDef) and node.name in wanted)
            or (isinstance(node, ast.Assign) and any(
                isinstance(target, ast.Name) and target.id in wanted for target in node.targets
            ))
        ]
        self.assertEqual(len(nodes), 3)
        namespace = {
            "os": os,
            "sys": __import__("sys"),
            "NextestEntry": object,
            "run_command": lambda command, merge_stderr=True: list_calls.append(command) or "",
            "parse_nextest_list": lambda output, kinds=None, matching_tests_only=False: [],
        }
        exec(compile(ast.Module(body=nodes, type_ignores=[]), "rust-test-inventory-proof.py", "exec"), namespace)
        return namespace

    def test_proof_lists_one_partition_per_configured_shard(self) -> None:
        for env_value, expected in ((None, 3), ("5", 5), ("6", 6)):
            with self.subTest(env_value=env_value):
                calls: list[list[str]] = []
                proof = self._proof_partition_functions(calls)
                env = {key: value for key, value in os.environ.items() if key != "AFT_UNIT_PARTITION_COUNT"}
                if env_value is not None:
                    env["AFT_UNIT_PARTITION_COUNT"] = env_value
                with unittest.mock.patch.dict(os.environ, env, clear=True):
                    count = proof["unit_partition_count"]()
                self.assertEqual(count, expected)
                partitions = proof["partition_nextest_entries"](["cargo", "nextest", "list", "--", "--skip", "x"], None, count)
                self.assertEqual(len(partitions), expected)
                self.assertEqual(
                    [call[call.index("--partition") + 1] for call in calls],
                    [f"hash:{shard}/{expected}" for shard in range(1, expected + 1)],
                )
                # The partition flag must stay ahead of the libtest `--` arguments.
                self.assertTrue(all(call.index("--partition") < call.index("--") for call in calls))

    def test_proof_integration_partitions_match_the_integration_shard_action(self) -> None:
        calls: list[list[str]] = []
        proof = self._proof_partition_functions(calls)
        self.assertEqual(proof["INTEGRATION_PARTITION_COUNT"], 3)
        proof["partition_nextest_entries"](["cargo", "nextest", "list"])
        self.assertEqual([call[-1] for call in calls], ["hash:1/3", "hash:2/3", "hash:3/3"])

    def test_proof_remaps_every_archive_list_and_partition_to_the_consumer(self) -> None:
        import ast

        calls: list[list[str]] = []
        proof = self._proof_partition_functions(calls)
        consumer = Path("/relocated/consumer-checkout")
        proof["ROOT"] = consumer
        builder = next(
            node for node in ast.walk(ast.parse(PROOF_TEXT))
            if isinstance(node, ast.FunctionDef) and node.name == "archive_command"
        )
        exec(compile(ast.Module(body=[builder], type_ignores=[]), "rust-test-inventory-proof.py", "exec"), proof)
        for bucket, count in (("lib", 5), ("bin", 5), ("integration", 3)):
            command = proof["archive_command"](f"{bucket}.tar.zst")
            calls.append([*command])
            if bucket == "lib":
                command += ["--", "--skip", "platform_verifier_tls_client_subprocess"]
            proof["partition_nextest_entries"](command, None, count)
        self.assertEqual(len(calls), 16)
        for command in calls:
            with self.subTest(command=command):
                self.assertEqual(command[:3], ["cargo", "nextest", "list"])
                self.assertEqual(command.count("--workspace-remap"), 1)
                remap = command.index("--workspace-remap")
                self.assertEqual(command[remap + 1], str(consumer))
                if "--" in command:
                    self.assertLess(remap, command.index("--"))

    def test_proof_rejects_a_non_positive_partition_count(self) -> None:
        proof = self._proof_partition_functions([])
        for value in ("0", "-2", "three"):
            with self.subTest(value=value):
                with unittest.mock.patch.dict(os.environ, {"AFT_UNIT_PARTITION_COUNT": value}):
                    with self.assertRaises(SystemExit) as raised:
                        proof["unit_partition_count"]()
                self.assertEqual(raised.exception.code, 2)


class MacOSUnitWorkflowTests(unittest.TestCase):
    def test_macos_conformance_job_has_room_for_a_cold_build(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        job = suite["jobs"]["unit-macos-conformance"]
        # A cold-cache build alone has taken 15 minutes on this runner, and
        # 30-minute runs were cancelled while their tests were still passing.
        self.assertEqual(job["timeout-minutes"], "45")

    def test_libtest_codegen_is_split_without_rebuilding_dependencies(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        job = suite["jobs"]["unit-macos-libtest-archive"]
        self.assertEqual(
            job["env"].get("LIBTEST_CARGO_CONFIG"),
            "profile.test.package.agent-file-tools.codegen-units=256",
        )
        self.assertEqual(job["env"]["CARGO_INCREMENTAL"], "0")
        self.assertNotIn("CARGO_PROFILE_TEST_CODEGEN_UNITS", job["env"])
        # A cold build must still have room to finish on the dedicated runner.
        self.assertEqual(job["timeout-minutes"], "90")
        cargo_steps = [step["run"] for step in job["steps"] if "run" in step]
        build_steps = [command for command in cargo_steps if re.search(r"\bcargo (nextest|test)\b", command)]
        self.assertEqual(len(build_steps), 3)
        for command in build_steps:
            self.assertIn('--config "$LIBTEST_CARGO_CONFIG"', command)
        archive = next(command for command in build_steps if "nextest archive" in command)
        self.assertIn("--timings", archive)
        self.assertIn('/usr/bin/time -l cargo nextest archive', archive)
        upload = next(step for step in job["steps"] if step.get("with", {}).get("name") == "macos-libtest-archive")
        paths = upload["with"]["path"].splitlines()
        self.assertTrue(all(path.startswith("${{ env.AFT_CI_ARCHIVE_DIR }}/") for path in paths))
        # Timings survive a failure in any step after the archive build.
        timings = next(step for step in job["steps"] if step.get("with", {}).get("name") == "macos-libtest-timings")
        self.assertIn("always()", timings["if"])
        self.assertEqual(timings["with"]["path"], "target/cargo-timings/")
        steps = job["steps"]
        archive_index = next(i for i, step in enumerate(steps) if "nextest archive" in step.get("run", ""))
        self.assertEqual(steps.index(timings), archive_index + 1)

    def test_macos_test_jobs_use_the_dedicated_runner_without_changing_release(self) -> None:
        # Tests reuse the rented Mac's local Cargo outputs. Publication retains
        # its existing runner choices.
        suite = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows/_unit-suite.yml"))
        release = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows/release.yml"))
        jobs = suite["jobs"]
        producer = jobs["unit-macos-libtest-archive"]
        self.assertEqual(producer["name"], "Cargo libtest archive (macOS)")
        for name in ("unit-macos-libtest-archive", "unit-macos-libtest-shard", "unit-macos-bin-archive", "unit-macos-nextest-archive", "unit-macos-inventory-proof"):
            self.assertEqual(jobs[name]["runs-on"], ["self-hosted", "rented-mac"], name)
        for name in ("build-darwin-arm64", "build-darwin-x64"):
            self.assertEqual(release["jobs"][name]["runs-on"], ["self-hosted", "mac-mini"], name)
        self.assertEqual(release["jobs"]["e2e-darwin-arm64"]["runs-on"], "macos-26")
        for workflow in (suite, release):
            self.assertEqual(
                [name for name, job in workflow["jobs"].items() if "blacksmith" in str(job.get("runs-on", "")).lower()],
                [],
            )
        shards = jobs["unit-macos-libtest-shard"]
        self.assertEqual(shards["name"], "Cargo libtest shard ${{ matrix.shard }}/5 (macOS)")
        self.assertEqual(shards["strategy"]["matrix"]["shard"], ["1", "2", "3", "4", "5"])
        for job in (producer, shards):
            self.assertIn("github.event_name", job["if"])
            self.assertNotIn("github.ref", job["if"])

    def test_macos_libtest_timing_summaries_survive_success_and_failure(self) -> None:
        suite = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows/_unit-suite.yml"))
        for job_id, invocation, heading, runner in (
            ("unit-macos-libtest-archive", "/usr/bin/time -l cargo nextest archive", "Build duration", "rented-mac"),
            ("unit-macos-libtest-shard", "./scripts/rust-test-gate.sh", "Test duration", "rented-mac"),
        ):
            step = next(step for step in suite["jobs"][job_id]["steps"] if invocation in step.get("run", ""))
            self.assertEqual(step["shell"], "bash")
            prelude = step["run"].split(invocation)[0]
            for status in (0, 23):
                with self.subTest(job=job_id, status=status), tempfile.TemporaryDirectory() as tmp:
                    summary = Path(tmp) / "summary.md"
                    result = subprocess.run(
                        ["bash", "-e", "-o", "pipefail", "-c", prelude + f"exit {status}\n"],
                        env={**os.environ, "GITHUB_STEP_SUMMARY": str(summary), "RUNNER_NAME": "test-runner", "AFT_NEXTEST_PARTITION": "hash:2/5"},
                        capture_output=True, text=True,
                    )
                    self.assertEqual(result.returncode, status, result.stderr)
                    text = summary.read_text()
                    self.assertIn(f"Runner: {runner} (test-runner)", text)
                    self.assertRegex(text, rf"{heading}: \d+s")

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
            "unit-macos-nextest-2",
            "unit-macos-nextest-3",
            "unit-macos-watcher",
            "unit-macos-conformance",
        }

        self.assertIn("name: Unit tests (macOS)", summary)
        self.assertIn("if: ${{ always() && github.event_name != 'pull_request' }}", summary)
        for lane in expected_lanes:
            self.assertIn(f"- {lane}", summary)
            self.assertIn(f"needs.{lane}.result", summary)
        self.assertIn('if [[ "$result" != "success" ]]', summary)
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        aggregate = suite["jobs"]["unit-macos"]["steps"][0]
        loop = re.search(r"for lane in (.*); do", aggregate["run"])
        self.assertIsNotNone(loop)
        self.assertEqual(
            set(loop.group(1).split()),
            {key.removesuffix("_RESULT") for key in aggregate["env"]},
        )

    def test_macos_rust_buckets_and_bun_suite_keep_separate_coverage(self) -> None:
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
        self.assertIn("max-parallel: 5", libtest)
        self.assertIn("AFT_NEXTEST_ARCHIVE_BIN_FILE", libtest)
        self.assertIn("AFT_GATE_PHASES: ${{ matrix.shard == 1 && 'lib,lib-shard' || 'lib-shard' }}", libtest)
        self.assertIn("AFT_NEXTEST_PARTITION: hash:${{ matrix.shard }}/5", libtest)
        self.assertIn("AFT_NEXTEST_EXTRACT_TO: ${{ github.workspace }}", libtest)

        integration = workflow_job("unit-macos-nextest")
        self.assertIn("./.github/actions/integration-shard", integration)
        self.assertIn("platform: macos", integration)
        self.assertIn('shard: "1"', integration)

        watcher = workflow_job("unit-macos-watcher")
        self.assertIn("AFT_GATE_PHASES: watcher", watcher)

        conformance = workflow_job("unit-macos-conformance")
        self.assertIn("AFT_GATE_PHASES: conformance", conformance)
        self.assertIn("bun run test:unit", conformance)

    def test_bash_timeout_contract_reuses_the_macos_conformance_build(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        watcher_steps = suite["jobs"]["unit-macos-watcher"]["steps"]
        self.assertFalse(any(
            step.get("name") == "Bash reply timeout contract (test-timing-hooks)"
            for step in watcher_steps
        ))

        conformance_steps = suite["jobs"]["unit-macos-conformance"]["steps"]
        gate_index = next(
            index for index, step in enumerate(conformance_steps)
            if "./scripts/rust-test-gate.sh" in step.get("run", "").splitlines()
            and step.get("env", {}).get("AFT_GATE_PHASES") == "conformance"
        )
        timeout_index = next(
            index for index, step in enumerate(conformance_steps)
            if step.get("name") == "Bash reply timeout contract (test-timing-hooks)"
        )
        self.assertLess(gate_index, timeout_index)
        timeout_step = conformance_steps[timeout_index]["run"]
        self.assertIn("--features test-timing-hooks --test integration", timeout_step)
        self.assertIn(
            'test_name="bash_reply_timing_contract_test::subc_promoted_foreground_is_killed_at_its_timeout"',
            timeout_step,
        )
        self.assertIn('"$test_name" --target-dir "$AFT_CI_TARGET_DIR" -- --list', timeout_step)
        self.assertIn('"$test_name" --target-dir "$AFT_CI_TARGET_DIR" -- --exact', timeout_step)
        self.assertIn("expected test missing from integration inventory", timeout_step)
        self.assertIn(
            "cargo test --features test-hooks -p agent-file-tools --features test-timing-hooks --test integration -- scope_revocation",
            GATE_TEXT,
        )

    def test_bash_timeout_inventory_guard_rejects_a_renamed_test(self) -> None:
        root = Path(__file__).resolve().parents[2]
        suite = workflow_gates.parse_workflow(str(root / ".github/workflows/_unit-suite.yml"))
        steps = suite["jobs"]["unit-macos-conformance"]["steps"]
        timeout_step = next(
            step for step in steps
            if step.get("name") == "Bash reply timeout contract (test-timing-hooks)"
        )
        guard = re.search(
            r'(?ms)^[ \t]*if \[\[ "\$listing" != \*"\$test_name: test"\* \]\]; then\n.*?^[ \t]*fi[ \t]*$',
            timeout_step["run"],
        )
        assert guard is not None, "the integration inventory must reject a missing expected test"
        test_name = "bash_reply_timing_contract_test::subc_promoted_foreground_is_killed_at_its_timeout"

        for listing, expected_status in (
            (f"{test_name}: test", 0),
            ("bash_reply_timing_contract_test::renamed_timeout_test: test", 1),
        ):
            script = (
                "set -euo pipefail\n"
                f'test_name="{test_name}"\n'
                f'listing="{listing}"\n'
                f"{guard.group(0)}\n"
            )
            result = subprocess.run(["bash", "-c", script], capture_output=True, text=True, check=False)
            with self.subTest(listing=listing):
                self.assertEqual(result.returncode, expected_status, result.stdout + result.stderr)
                if expected_status:
                    self.assertIn("expected test missing from integration inventory", result.stderr)

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
        extract_helper = GATE_TEXT[GATE_TEXT.index("extract_nextest_archive_for_prewarm() {"):prewarm_start]
        self.assertLess(extract_helper.index('df -h "$extract_to"'), extract_helper.index("cargo nextest list"))

    def test_macos_libtest_downloads_verify_before_extract(self) -> None:
        suite = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows/_unit-suite.yml"))
        for job in ("unit-macos-libtest-shard", "unit-macos-inventory-proof"):
            steps = suite["jobs"][job]["steps"]
            consumer = next(index for index, step in enumerate(steps) if any(command in step.get("run", "") for command in (
                "./scripts/rust-test-gate.sh", "python3 scripts/rust-test-inventory-proof.py",
            )))
            for artifact, archive in (("macos-libtest-archive", "macos-libtest-nextest.tar.zst"),
                                      ("macos-bin-archive", "macos-bin-nextest.tar.zst")):
                download = next(step for step in steps if step.get("with", {}).get("name") == artifact)
                with self.subTest(job=job, artifact=artifact):
                    self.assertEqual(download["uses"], DOWNLOAD_RETRY)
                    self.assertEqual(download["with"].get("verify-zstd"), archive)
                    self.assertNotIn("continue-on-error", download)
                    self.assertLess(steps.index(download), consumer)

    def test_macos_backtrace_probe_runs_once_in_archive_builder_not_shards(self) -> None:
        probe = "cargo test -p agent-file-tools --features test-hooks --lib panic_hook_preserves_default_backtrace_behavior_and_persists"
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
        self.assertIn("lib_partitions = partition_nextest_entries(\n        lib_command, {\"lib\"}, unit_partitions_total\n    )", PROOF_TEXT)
        self.assertIn("bin_partitions = partition_nextest_entries(\n        bin_command, {\"bin\"}, unit_partitions_total\n    )", PROOF_TEXT)
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
        self.assertIn("AFT_BINARY_PATH: ${{ env.AFT_CI_TARGET_DIR }}/debug/aft", conformance)


class NativeSelfHostedWorkflowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.suites = [workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows" / filename))
                       for filename in ("_unit-suite.yml", "_e2e-suite.yml")]
        spec = importlib.util.spec_from_file_location("native_ci_state", REPO_ROOT / "scripts/ci-native-state.py")
        assert spec is not None and spec.loader is not None
        self.state = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.state)

    def native_jobs(self):
        return [(name, job) for suite in self.suites for name, job in suite["jobs"].items()
                if job.get("runs-on") in (["self-hosted", "rented-mac"], ["self-hosted", "narli"])]

    def test_required_native_check_names_are_unchanged(self) -> None:
        names = {f"{prefix} / {job['name']}" for prefix, suite in zip(("Unit", "E2E"), self.suites)
                 for job in suite["jobs"].values()}
        required = {
            "Unit / Unit tests (macOS)", "Unit / Cargo test archive (macOS)",
            "E2E / E2E (macOS native)", "Unit / Bash permission e2e (Windows)",
            "Unit / Cargo check deny-warnings (Windows)", "Unit / Cargo libtest + watcher (Windows)",
            "Unit / Cargo test archive (Windows)", "E2E / E2E (Windows native)",
        }
        required.update(f"Unit / Cargo integration shard {shard}/3 ({platform})"
                        for platform in ("macOS", "Windows") for shard in (1, 2, 3))
        self.assertEqual(required - names, set())

    def test_every_native_job_prepares_persistent_state_and_always_cleans_processes(self) -> None:
        jobs = self.native_jobs()
        self.assertEqual(len(jobs), 20)
        for name, job in jobs:
            with self.subTest(job=name):
                self.assertEqual(job["steps"][1], {"uses": "actions/setup-python@v6", "if": "runner.environment == 'github-hosted'",
                                                    "with": {"python-version": "3.14"}})
                self.assertEqual(job["steps"][2], {"uses": "./.github/actions/native-ci-state"})
                cleanup = job["steps"][-1]
                self.assertEqual(cleanup["uses"], "./.github/actions/native-ci-state")
                self.assertEqual(cleanup["if"], "always()")
                self.assertEqual(cleanup["with"]["phase"], "cleanup")
                env = job.get("env", {})
                self.assertNotIn("SCCACHE_GHA_ENABLED", env)
                self.assertNotIn("RUSTC_WRAPPER", env)
                self.assertNotIn("CARGO_TARGET_DIR", env)
                self.assertNotIn("CARGO_BUILD_TARGET_DIR", env)

    def test_native_jobs_and_nested_actions_never_use_github_cache_or_artifacts(self) -> None:
        def check(steps, owner):
            for step in steps:
                action = step.get("uses", "")
                # A hosted-only composite gates every nested transfer as well.
                if "runner.environment == 'github-hosted'" in step.get("if", ""):
                    continue
                if action == "oven-sh/setup-bun@v2":
                    self.assertIn(step["with"]["no-cache"], ("true", "${{ runner.environment == 'self-hosted' }}"))
                if action == "actions/setup-node@v5":
                    self.assertIn(step["with"]["package-manager-cache"], ("false", "${{ runner.environment == 'github-hosted' }}"))
                if action == "actions/setup-python@v6":
                    self.assertNotIn("cache", step.get("with", {}))
                self.assertFalse(any(service in action for service in (
                    "actions/cache", "Swatinem/rust-cache", "sccache-action", "upload-artifact", "download-artifact",
                )), f"{owner}: ungated remote transfer {action}")
                if action.startswith("./.github/actions/"):
                    composite = workflow_gates.parse_workflow(str(REPO_ROOT / action / "action.yml"))
                    check(composite["runs"]["steps"], action)
        for name, job in self.native_jobs():
            check(job["steps"], name)

    def test_native_jobs_prepare_host_tools_and_rust_components_before_use(self) -> None:
        shard = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/integration-shard/action.yml"))
        for name, job in self.native_jobs():
            with self.subTest(job=name):
                steps = job["steps"]
                self.assertEqual(steps[3]["uses"], "oven-sh/setup-bun@v2")
                self.assertEqual(steps[4]["uses"], "actions/setup-node@v5")
                rust = [step for step in steps if step.get("uses") == "dtolnay/rust-toolchain@stable"]
                if not rust:
                    platform = next(step["with"]["platform"] for step in steps
                                    if step.get("uses") == "./.github/actions/integration-shard")
                    rust = [step for step in shard["runs"]["steps"]
                            if step.get("uses") == "dtolnay/rust-toolchain@stable"
                            and step.get("if", "").startswith(f"inputs.platform == '{platform}'")]
                self.assertEqual(len(rust), 1)
                self.assertEqual(set(rust[0]["with"]["components"].split(", ")),
                                 {"rustfmt", "clippy", "rust-analyzer"})
                if job["runs-on"] == ["self-hosted", "narli"]:
                    self.assertIn("runner.environment == 'github-hosted'", rust[0].get("if", ""))
                if rust[0] in steps:
                    first_use = next((i for i, step in enumerate(steps)
                                      if re.search(r"\bcargo\b|scripts/check-path-deps.py", step.get("run", ""))), len(steps))
                    self.assertLess(steps.index(rust[0]), first_use)
        conformance = self.suites[0]["jobs"]["unit-macos-conformance"]["steps"]
        gh = next(step for step in conformance if step.get("name") == "Install GitHub CLI for pinned helper downloads")
        fetch = next(step for step in conformance if step.get("name") == "Fetch pinned subc-core")
        self.assertIn("brew install gh", gh["run"])
        self.assertLess(conformance.index(gh), conformance.index(fetch))

    def test_local_archive_consumers_keep_run_scoped_paths(self) -> None:
        for name, job in self.native_jobs():
            for step in job["steps"]:
                for key, value in step.get("env", {}).items():
                    if "ARCHIVE" in key or "INVENTORY" in key:
                        self.assertTrue(value.startswith("${{ env.AFT_CI_ARCHIVE_DIR }}/"), (name, key, value))
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/integration-shard/action.yml"))
        for step in action["runs"]["steps"]:
            if "AFT_NEXTEST_ARCHIVE_FILE" in step.get("env", {}):
                self.assertIn("env.AFT_CI_ARCHIVE_DIR || runner.temp", step["env"]["AFT_NEXTEST_ARCHIVE_FILE"])

    def test_native_test_steps_clear_cargo_target_overrides_and_builds_scope_the_warm_path(self) -> None:
        checked = 0
        for name, job in self.native_jobs():
            for step in job["steps"]:
                run = step.get("run", "")
                env = step.get("env", {})
                if "CARGO_TARGET_DIR" in env:
                    self.assertEqual(env["CARGO_TARGET_DIR"], "${{ env.AFT_CI_TARGET_DIR }}", name)
                    self.assertRegex(run, r"cargo (build|check|nextest (archive|list)|test)|ci-macos-integration-archive.py", name)
                if any(command in run for command in (
                    "rust-test-gate.sh", "ci-windows-probes.py", "bun run test:unit", "bun test ",
                    "tests/macos-e2e/run.sh", "tests/windows-e2e/run.ps1", "cargo test",
                )) and not ("-- --list" in run and "-- --exact" not in run):
                    checked += 1
                    self.assertNotIn("CARGO_TARGET_DIR", env, name)
                    self.assertNotIn("CARGO_BUILD_TARGET_DIR", env, name)
                    shell = step.get("shell", "pwsh" if job["runs-on"] == ["self-hosted", "narli"] else "bash")
                    clear = ("Remove-Item Env:CARGO_TARGET_DIR, Env:CARGO_BUILD_TARGET_DIR"
                             if shell == "pwsh" else "unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR")
                    self.assertIn(clear, run, name)
        self.assertGreater(checked, 10)
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/integration-shard/action.yml"))
        for step in action["runs"]["steps"]:
            if "rust-test-gate.sh" in step.get("run", ""):
                self.assertIn("unset CARGO_TARGET_DIR CARGO_BUILD_TARGET_DIR", step["run"])

    def test_nextest_is_on_native_path_before_install_reuse_or_cargo_invocation(self) -> None:
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/setup-nextest/action.yml"))
        steps = action["runs"]["steps"]
        path = next(step for step in steps if step.get("name") == "Expose native cargo-nextest install on PATH")
        install = next(step for step in steps if step.get("name") == "Download and install cargo-nextest")
        self.assertLess(steps.index(path), steps.index(install))
        self.assertEqual(path["if"], "runner.environment == 'self-hosted'")
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            path_file = home / "github-path"
            result = subprocess.run(["bash", "-e", "-c", path["run"]], capture_output=True, text=True,
                                    env={**os.environ, "HOME": str(home), "GITHUB_PATH": str(path_file)})
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(path_file.read_text().strip(), (home / ".cargo/bin").as_posix())

    def test_finalize_waits_for_both_suites_and_skips_prs(self) -> None:
        workflow = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows/tests.yml"))
        cleanup = workflow["jobs"]["native-ci-cleanup"]
        self.assertEqual(cleanup["needs"], ["unit", "e2e"])
        self.assertEqual(cleanup["if"], "${{ always() && github.event_name != 'pull_request' }}")
        self.assertEqual(cleanup["strategy"]["matrix"]["machine"], ["rented-mac", "narli"])
        self.assertEqual(cleanup["steps"][1], {"uses": "actions/setup-python@v6", "if": "runner.environment == 'github-hosted'",
                                              "with": {"python-version": "3.14"}})
        self.assertEqual(cleanup["steps"][-1]["with"], {
            "phase": "${{ needs.unit.result == 'success' && needs.e2e.result == 'success' && 'finalize' || 'cleanup' }}",
        })

    def test_interleaved_runs_preserve_each_others_archives_and_warm_target(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "aft-ci"
            env_file = Path(tmp) / "env"
            with unittest.mock.patch.dict(os.environ, {"GITHUB_ENV": str(env_file), "GITHUB_PATH": str(Path(tmp) / "path")}), \
                 unittest.mock.patch.object(self.state, "setup_windows_tool_paths", return_value={}):
                first = self.state.run_directory(root, "100", "1")
                self.state.setup(root, first)
                archive = first / "macos-nextest.tar.zst"
                archive.write_bytes(b"first run")
                compiled = root / "target" / "warm-dependency"
                compiled.write_bytes(b"keep")
                second = self.state.run_directory(root, "101", "1")
                self.state.setup(root, second)
                self.assertEqual(archive.read_bytes(), b"first run")
                self.assertEqual(compiled.read_bytes(), b"keep")
                self.assertIn(f"AFT_CI_TARGET_DIR={(root / 'target').as_posix()}", env_file.read_text())
                self.assertIn(f"AFT_CI_ARCHIVE_DIR={second.as_posix()}", env_file.read_text())
                self.assertNotIn("CARGO_TARGET_DIR=", env_file.read_text())
                self.assertNotIn("CARGO_BUILD_TARGET_DIR=", env_file.read_text())
                self.assertNotIn("RUSTC_WRAPPER=", env_file.read_text())
                self.assertNotIn("SCCACHE_GHA_ENABLED=", env_file.read_text())
                self.assertNotIn("SHELL=", env_file.read_text())
            with unittest.mock.patch.object(self.state.Path, "home", return_value=Path(tmp)), \
                 unittest.mock.patch.object(self.state, "cleanup_processes"), \
                 unittest.mock.patch.object(sys, "argv", ["ci-native-state.py", "finalize"]), \
                 unittest.mock.patch.dict(os.environ, {"RUNNER_ENVIRONMENT": "self-hosted", "GITHUB_RUN_ID": "101", "GITHUB_RUN_ATTEMPT": "1"}):
                self.state.main()
            self.assertFalse(second.exists())
            self.assertEqual(archive.read_bytes(), b"first run")
            self.assertEqual(compiled.read_bytes(), b"keep")
            self.assertEqual(first, self.state.run_directory(root, "100", "2"))
            with self.assertRaises(ValueError):
                self.state.run_directory(root, "../target", "1")

    def test_cleanup_kills_native_test_descendants_but_not_runner_or_unrelated_processes(self) -> None:
        table = {
            1: (0, "/runner/Runner.Listener"), 2: (1, "/runner/Runner.Worker"),
            3: (2, "/usr/bin/python3"),
            10: (2, "/home/runner/aft-ci/target/debug/deps/integration-123"),
            11: (10, "/bin/bash"), 12: (11, "/bin/sleep"),
            20: (1, "/usr/bin/unrelated"),
        }
        self.assertEqual(self.state.leaked_processes(table, [Path("/home/runner/aft-ci")], 3), {10, 11, 12})
        windows = {3: (2, "C:\\Python\\python.exe"), 2: (1, "C:\\runner\\Runner.Worker.exe"),
                   10: (2, "C:\\Users\\Runner\\aft-ci\\target\\debug\\aft.exe"), 11: (10, "C:\\Windows\\cmd.exe")}
        self.assertEqual(self.state.leaked_processes(windows, [Path("c:/users/runner/aft-ci")], 3), {10, 11})

    def test_windows_git_path_matches_hosted_order_without_changing_shell(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "custom Git install"
            expected = [root / part for part in ("bin", "cmd", "ucrt64/bin", "usr/bin")]
            for path in expected:
                path.mkdir(parents=True, exist_ok=True)
            (root / "cmd/git.exe").touch()
            path_file = Path(tmp) / "github-path"
            exec_path = root / "ucrt64/libexec/git-core"
            self.assertEqual(self.state.windows_git_path_entries(exec_path), expected)
            self.state.publish_windows_git_path(exec_path, path_file)
            self.assertEqual(path_file.read_text().splitlines(), [path.as_posix() for path in reversed(expected)])
            self.assertEqual(self.state.windows_git_path_entries(root / "mingw64/libexec/git-core"), expected)

    def test_windows_git_path_rejects_missing_hosted_coreutils_directories(self) -> None:
        for missing in ("bin", "cmd", "ucrt64/bin", "usr/bin"):
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                for part in ("bin", "cmd", "ucrt64/bin", "usr/bin"):
                    if part != missing:
                        (root / part).mkdir(parents=True, exist_ok=True)
                for marker in ("cmd/git.exe", "bin/bash.exe"):
                    if (root / marker).parent.is_dir():
                        (root / marker).touch()
                with self.assertRaisesRegex(RuntimeError, "PATH directories missing"):
                    self.state.windows_git_path_entries(root / "ucrt64/libexec/git-core")

    @staticmethod
    def managed_windows_rust(root):
        cargo = root / "cargo"
        rustup = root / "rustup"
        toolchain = rustup / "toolchains/stable-x86_64-pc-windows-msvc"
        for name in ("cargo", "rustc", "rustdoc", "rustup", "rustfmt", "cargo-clippy", "rust-analyzer"):
            binary = cargo / "bin" / f"{name}.exe"
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_text("administrator-managed proxy")
        for name in ("cargo", "rustc", "rustdoc"):
            binary = toolchain / "bin" / f"{name}.exe"
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_text("administrator-managed toolchain")
        source = toolchain / "lib/rustlib/src/rust/library/core/src/lib.rs"
        source.parent.mkdir(parents=True)
        source.write_text("administrator-managed standard library source")
        return cargo, rustup, source

    def test_windows_bootstrap_keeps_rustup_inherited_and_cargo_state_user_writable(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo, rustup, _ = self.managed_windows_rust(root / "managed")
            user = root / "user"
            git = root / "custom Git"
            for part in ("bin", "cmd", "ucrt64/bin", "usr/bin"):
                (git / part).mkdir(parents=True, exist_ok=True)
            (git / "cmd/git.exe").touch()
            path_file = root / "github-path"
            with unittest.mock.patch.object(self.state.Path, "home", return_value=user), \
                 unittest.mock.patch.object(self.state.subprocess, "check_output", return_value=str(git / "ucrt64/libexec/git-core")), \
                 unittest.mock.patch.dict(os.environ, {"CARGO_HOME": str(cargo), "RUSTUP_HOME": str(rustup), "GITHUB_PATH": str(path_file)}):
                values = self.state.setup_windows_tool_paths()
                self.assertEqual(os.environ["RUSTUP_HOME"], str(rustup))
            self.assertEqual(values, {"CARGO_HOME": user / ".cargo", "RUSTUP_TOOLCHAIN": "stable-x86_64-pc-windows-msvc",
                                      "RUSTUP_AUTO_INSTALL": "0"})
            self.assertTrue((user / ".cargo/bin").is_dir())
            self.assertEqual(path_file.read_text().splitlines()[-2:], [(cargo / "bin").as_posix(), (user / ".cargo/bin").as_posix()])
            self.assertFalse((cargo / "registry").exists())

    def test_windows_bootstrap_refuses_missing_rust_or_a_writable_home_inside_the_installation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo, rustup, source = self.managed_windows_rust(root / "managed")
            with self.assertRaisesRegex(RuntimeError, "outside the shared Rust installation"):
                self.state.windows_rust_environment(cargo, rustup, cargo / "mutable")
            self.assertFalse((cargo / "mutable").exists())
            source.unlink()
            with self.assertRaisesRegex(RuntimeError, re.escape(str(source))):
                self.state.windows_rust_environment(cargo, rustup, root / "user-cargo")
            self.assertFalse((root / "user-cargo").exists())

    def test_windows_toolchain_probe_is_read_only_and_rejects_missing_requirements(self) -> None:
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/native-ci-state/action.yml"))
        probe = next(step for step in action["runs"]["steps"] if step.get("name") == "Verify installed Windows Rust toolchain (read-only)")
        self.assertNotRegex(probe["run"], r"rustup\s+(update|install|default|toolchain\s+(install|add)|target\s+add|component\s+add)")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scripts = {
                "rustup": '#!/bin/sh\ncase "$*" in\nshow) echo stable-x86_64-pc-windows-msvc;;\n"target list --installed") printf "%s\\n" "$FAKE_TARGETS";;\n"component list --installed") printf "%s\\n" "$FAKE_COMPONENTS";;\n*) echo "Mutation or unexpected rustup command: $*" >&2; exit 99;;\nesac\n',
                "rustc": '#!/bin/sh\necho "rustc 1.99.0 (fixture)"\n',
                "cargo": '#!/bin/sh\necho "cargo 1.99.0 (fixture)"\n',
            }
            for name, text in scripts.items():
                path = root / name
                path.write_text(text)
                path.chmod(0o755)
            components = "rustfmt-x86_64-pc-windows-msvc\nclippy-x86_64-pc-windows-msvc\nrust-analyzer-x86_64-pc-windows-msvc\nrust-src"
            for target, installed, available, version, error in (
                ("x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc", components, "1.99.0", ""),
                ("aarch64-pc-windows-msvc", "aarch64-pc-windows-msvc", components, "1.99.0", ""),
                ("aarch64-pc-windows-msvc", "x86_64-pc-windows-msvc", components, "", "Missing target"),
                ("x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc", components.replace("\nrust-src", ""), "", "Missing component rust-src"),
                ("x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc", components, "1.98.0", "differs from release producer"),
            ):
                with self.subTest(target=target, error=error):
                    result = subprocess.run(["bash", "-c", probe["run"]], capture_output=True, text=True,
                                            env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                                                 "FAKE_TARGETS": installed, "FAKE_COMPONENTS": available,
                                                 "AFT_CI_RUST_TARGET": target, "AFT_CI_RUST_VERSION": version,
                                                 "RUSTUP_HOME": str(root / "managed-rustup")})
                    if error:
                        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertIn(error, result.stdout + result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_windows_jq_preflight_fails_before_tests_when_the_managed_helper_is_unavailable(self) -> None:
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/native-ci-state/action.yml"))
        step = next(step for step in action["runs"]["steps"] if step.get("name") == "Verify administrator-installed Windows jq")
        self.assertEqual(step["if"], "runner.os == 'Windows' && runner.environment == 'self-hosted' && inputs.phase == 'setup'")
        self.assertNotRegex(step["run"], r"(?i)\b(winget|choco|scoop|Install-Package|curl)\b")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            jq = root / "jq"
            for output, code, message in ((None, 0, "jq is missing"), ("broken", 7, "cannot report its version"),
                                           ("not a jq version", 0, "Unexpected jq version response"), ("jq-1.8.1", 0, "")):
                with self.subTest(output=output, code=code):
                    if output is not None:
                        jq.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(output) + f"\nexit {code}\n")
                        jq.chmod(0o755)
                    result = subprocess.run(["/bin/bash", "-c", step["run"]], capture_output=True, text=True,
                                            env={**os.environ, "PATH": str(root)})
                    if message:
                        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertIn(message, result.stdout + result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertIn("Managed jq: jq-1.8.1", result.stdout)

    def test_self_hosted_workflows_validate_system_python_instead_of_running_its_installer(self) -> None:
        suites = self.suites + [workflow_gates.parse_workflow(str(REPO_ROOT / ".github/workflows" / name))
                                for name in ("tests.yml", "release.yml")]
        checked = 0
        for suite in suites:
            for name, job in suite["jobs"].items():
                if "self-hosted" not in job.get("runs-on", []):
                    continue
                checked += 1
                setup = next(i for i, step in enumerate(job["steps"])
                             if step.get("uses") == "./.github/actions/native-ci-state")
                for i, step in enumerate(job["steps"]):
                    if step.get("uses") == "actions/setup-python@v6":
                        self.assertEqual(step["if"], "runner.environment == 'github-hosted'", name)
                    if re.search(r"\bpython(?:3)?\s", step.get("run", "")):
                        self.assertLess(setup, i, name)
        self.assertEqual(checked, 26)
        action = workflow_gates.parse_workflow(str(REPO_ROOT / ".github/actions/native-ci-state/action.yml"))
        steps = action["runs"]["steps"]
        self.assertEqual(steps[0]["name"], "Validate system Python on the native runner")
        self.assertEqual(steps[0]["if"], "runner.environment == 'self-hosted'")
        self.assertIn('python="$AFT_CI_PYTHON"', steps[1]["run"])
        self.assertEqual(steps[1]["if"], "inputs.phase != 'python'")

    def test_system_python_probe_rejects_313_and_accepts_314_or_newer(self) -> None:
        match = re.search(r"probe='(.*?)'\n", self.system_python_preflight(), re.DOTALL)
        assert match is not None
        code = match.group(1)
        for version, passes in (((3, 13, 9), False), ((3, 14, 0), True), ((3, 15, 0), True)):
            with self.subTest(version=version):
                result = subprocess.run([sys.executable, "-B", "-c",
                                         f"import sys; sys.version_info = {version!r}\n" + code],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, passes, result.stdout + result.stderr)
                if not passes:
                    self.assertIn("older than required 3.14", result.stderr)

    @staticmethod
    def system_python_preflight():
        # The small workflow parser strips nested scalar indentation. Execute
        # the literal YAML body so Python's indented version check is preserved.
        source = (REPO_ROOT / ".github/actions/native-ci-state/action.yml").read_text()
        match = re.search(r"      run: \|\n((?:        [^\n]*\n|\n)+)", source)
        assert match is not None
        return textwrap.dedent(match.group(1))

    def run_system_python_preflight(self, mode):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tools = root / "tools"
            installation = root / "system Python"
            tools.mkdir()
            installation.mkdir()
            (installation / "Scripts").mkdir()
            command_directory = installation if mode == "launcher" else tools
            for name in ("python3.14", "python", "python3"):
                if mode == "launcher" and name == "python3.14":
                    continue
                path = command_directory / name
                prefix = "another installation" if mode == "mismatched" and name == "python3" else "same installation"
                identity = (root / prefix).as_posix()
                executable = (installation / "python.exe").as_posix() if mode == "launcher" else path.as_posix()
                text = "#!/bin/sh\n"
                if mode == "old":
                    text += 'echo "Python 3.13.9 is older than required 3.14" >&2\nexit 1\n'
                elif mode == "missing" and name == "python3":
                    continue
                else:
                    text += "printf '%s\\r\\n' " + " ".join(shlex.quote(part) for part in (
                        executable, identity, "3.14.1 (administrator-managed build)",
                    )) + "\n"
                path.write_text(text)
                path.chmod(0o755)
            if mode == "launcher":
                launcher = tools / "py"
                launcher.write_text("#!/bin/sh\nprintf '%s\\n' " + " ".join(shlex.quote(part) for part in (
                    (installation / "python.exe").as_posix(), (root / "same installation").as_posix(),
                    "3.14.1 (administrator-managed build)",
                )) + "\n")
                launcher.chmod(0o755)
            env_file, path_file = root / "github-env", root / "github-path"
            result = subprocess.run(["/bin/bash", "-c", self.system_python_preflight()],
                                    capture_output=True, text=True,
                                    env={**os.environ, "PATH": str(tools), "RUNNER_OS": "Windows" if mode == "launcher" else "macOS",
                                         "GITHUB_ENV": str(env_file), "GITHUB_PATH": str(path_file)})
            return result, env_file.read_text() if env_file.exists() else "", path_file.read_text() if path_file.exists() else ""

    def test_system_python_preflight_publishes_validated_aliases_or_fails_before_exporting(self) -> None:
        for mode, error in (("matching", ""), ("launcher", ""), ("old", "System Python 3.14+ is required"),
                            ("missing", "Missing python3"), ("mismatched", "different Python installation or build")):
            with self.subTest(mode=mode):
                result, env, path = self.run_system_python_preflight(mode)
                if error:
                    self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn(error, result.stdout + result.stderr)
                    self.assertEqual(env, "")
                    self.assertEqual(path, "")
                else:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                    self.assertIn("AFT_CI_PYTHON=", env)
                    self.assertTrue(path.strip())
                    self.assertIn("System python:", result.stdout)
                    self.assertIn("System python3:", result.stdout)

    def test_cleanup_includes_the_injected_e2e_executable_cache(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            local = home / "localappdata"
            binary = (local / "aft/bin/v1/aft.exe" if os.name == "nt"
                      else home / ".cache/aft/bin/v1/aft")
            with unittest.mock.patch.object(self.state.Path, "home", return_value=home), \
                 unittest.mock.patch.object(self.state, "process_table", return_value={10: (0, str(binary))}), \
                 unittest.mock.patch.object(self.state.os, "kill") as kill, \
                 unittest.mock.patch.object(self.state.subprocess, "run") as run, \
                 unittest.mock.patch.dict(os.environ, {"GITHUB_WORKSPACE": str(home / "workspace"), "LOCALAPPDATA": str(local)}):
                self.state.cleanup_processes(home / "aft-ci")
            if os.name == "nt":
                self.assertEqual(run.call_args.args[0], ["taskkill", "/T", "/F", "/PID", "10"])
            else:
                kill.assert_called_once_with(10, self.state.signal.SIGKILL)


class CortexkitTrackingGateTests(unittest.TestCase):
    """Private .cortexkit/ files must never be tracked, and CI must say so."""

    ROOT = Path(__file__).resolve().parents[2]
    SCRIPT = ROOT / "scripts/check-cortexkit-tracking.sh"
    # Hide the user's and system's Git config so a personal global excludes
    # file cannot change what the fixture repository ignores.
    GIT_ENV = {**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}

    def _git(self, repo: Path, *args: str) -> None:
        subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, env=self.GIT_ENV)

    def _check(self, repo: Path) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(self.SCRIPT), str(repo)],
            capture_output=True, text=True, check=False, env=self.GIT_ENV,
        )

    def _fixture(self, root: Path) -> Path:
        """A fresh repository carrying this checkout's real .gitignore.

        A fresh `git init` has no local excludes, so it sees exactly what a CI
        clone sees: only the committed ignore rules.
        """
        repo = root / "repo"
        repo.mkdir()
        self._git(repo, "init", "-q")
        (repo / ".gitignore").write_text((self.ROOT / ".gitignore").read_text())
        return repo

    def test_landing_policy_job_runs_the_cortexkit_tracking_check(self) -> None:
        workflow = workflow_gates.parse_workflow(str(self.ROOT / ".github/workflows/tests.yml"))
        steps = workflow["jobs"]["landing-policy"]["steps"]
        matches = [step for step in steps if step.get("run") == "scripts/check-cortexkit-tracking.sh"]
        self.assertEqual(len(matches), 1, "Landing policy must run the .cortexkit tracking check once")
        self.assertNotIn("if", matches[0], "the tracking check must run on every Landing policy event")
        self.assertNotIn("continue-on-error", matches[0])

    def test_committed_ignore_rules_keep_private_cortexkit_files_out(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = self._fixture(Path(tmp))

            def ignored(path: str) -> bool:
                result = subprocess.run(
                    ["git", "-C", str(repo), "check-ignore", "-q", "--no-index", path],
                    check=False, env=self.GIT_ENV,
                )
                self.assertIn(result.returncode, (0, 1), path)
                return result.returncode == 0

            for path in (
                ".cortexkit/alfonso/drafts/plan.md",
                ".cortexkit/alfonso/reports/audit.md",
                ".cortexkit/internal-docs/investigations/note.md",
                ".cortexkit/scripts/tool.sh",
                ".cortexkit/new-private-folder/file.txt",
            ):
                with self.subTest(private=path):
                    self.assertTrue(ignored(path))
            for path in (
                ".cortexkit/release-notes/v1.0.0.md",
                ".cortexkit/skills/example/SKILL.md",
                ".cortexkit/rules/example.md",
                ".cortexkit/WORKER.md",
                ".cortexkit/aft.jsonc",
            ):
                with self.subTest(shared=path):
                    self.assertFalse(ignored(path))

    def test_check_fails_listing_a_force_added_file_and_passes_once_untracked(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = self._fixture(Path(tmp))
            draft = repo / ".cortexkit/alfonso/drafts/plan.md"
            draft.parent.mkdir(parents=True)
            draft.write_text("private\n")
            (repo / ".cortexkit/WORKER.md").write_text("guide\n")
            self._git(repo, "add", ".gitignore", ".cortexkit/WORKER.md")
            self._git(repo, "add", "-f", ".cortexkit/alfonso/drafts/plan.md")

            red = self._check(repo)
            self.assertEqual(red.returncode, 1, red.stdout + red.stderr)
            self.assertIn(".cortexkit/alfonso/drafts/plan.md", red.stderr)
            self.assertNotIn(".cortexkit/WORKER.md", red.stderr)

            self._git(repo, "rm", "--cached", "-q", ".cortexkit/alfonso/drafts/plan.md")
            green = self._check(repo)
            self.assertEqual(green.returncode, 0, green.stdout + green.stderr)
            self.assertTrue(draft.exists(), "git rm --cached must keep the working copy")


REPO_ROOT = Path(__file__).resolve().parents[2]
UPLOAD_RETRY = "./.github/actions/upload-artifact-retry"
DOWNLOAD_RETRY = "./.github/actions/download-artifact-retry"
RETRY_ACTIONS = {
    UPLOAD_RETRY: "actions/upload-artifact@v4",
    DOWNLOAD_RETRY: "actions/download-artifact@v4",
}
DIRECT_ARTIFACT_PREFIXES = ("actions/upload-artifact@", "actions/download-artifact@")

# Direct, single-attempt artifact steps that a required check may still run,
# keyed by (file, job, step id). Each one must be continue-on-error and have a
# fallback, so the artifact service failing cannot fail the check.
SINGLE_ATTEMPT_ALLOWED = {
    # The gate builds the binary itself when this download fails.
    (".github/workflows/tests.yml", "search-quality-gate", "download-release-aft"),
}

# Artifacts whose contents a later required job reads. Their uploads must keep
# failing the producing job when a file is missing.
REQUIRED_DATA_UPLOADS = {
    "linux-nextest-archive",
    "macos-nextest-archive",
    "macos-libtest-archive",
    "macos-bin-archive",
    "windows-nextest-archive",
    "e2e-linux-${{ inputs.cargo_profile }}-binary",
    "e2e-opencode2-${{ inputs.cargo_profile }}-binary",
    "search-quality-aft-${{ github.sha }}",
    "queue-context-${{ github.sha }}",
}


def _local_target(uses: str) -> Path | None:
    """Map a local `uses:` reference to the file GitHub would load for it."""
    if not uses.startswith("./"):
        return None
    target = REPO_ROOT / uses[2:]
    return target if target.suffix in (".yml", ".yaml") else target / "action.yml"


def required_check_steps() -> list[tuple[str, str, dict]]:
    """Every step a merge-queue run of tests.yml can execute, as
    (repo-relative file, job id, step). Reusable workflows and local composite
    actions are followed, so a new suite or shared action is covered without
    anyone remembering to list it here. Composite steps report job "(composite)".
    """
    pending = [REPO_ROOT / ".github/workflows/tests.yml"]
    seen: set[Path] = set()
    found: list[tuple[str, str, dict]] = []
    while pending:
        path = pending.pop()
        if path in seen:
            continue
        seen.add(path)
        document = workflow_gates.parse_workflow(str(path))
        relative = str(path.relative_to(REPO_ROOT))
        groups: list[tuple[str, list]] = []
        jobs = document.get("jobs")
        if isinstance(jobs, dict):
            for job_id, job in jobs.items():
                if not isinstance(job, dict):
                    continue
                target = _local_target(str(job.get("uses") or ""))
                if target is not None:
                    pending.append(target)
                groups.append((job_id, job.get("steps") or []))
        runs = document.get("runs")
        if isinstance(runs, dict):
            groups.append(("(composite)", runs.get("steps") or []))
        for job_id, steps in groups:
            for step in steps:
                if not isinstance(step, dict):
                    continue
                found.append((relative, job_id, step))
                target = _local_target(str(step.get("uses") or ""))
                if target is not None:
                    pending.append(target)
    return found


class ArtifactRetryTests(unittest.TestCase):
    """GitHub's artifact service can time out for minutes at a time. An upload
    or download that a required check runs must retry instead of failing a run
    whose tests passed, and must still fail closed once the retries run out.
    The only exceptions are listed in SINGLE_ATTEMPT_ALLOWED: steps whose
    failure the check survives through a fallback."""

    def setUp(self) -> None:
        self.steps = required_check_steps()

    def calls(self, action: str) -> list[tuple[str, str, dict]]:
        return [entry for entry in self.steps if entry[2].get("uses") == action]

    def test_traversal_reaches_the_unit_and_e2e_suites_and_shard_action(self) -> None:
        files = {relative for relative, _, _ in self.steps}
        for expected in (
            ".github/workflows/tests.yml",
            ".github/workflows/_unit-suite.yml",
            ".github/workflows/_e2e-suite.yml",
            ".github/actions/integration-shard/action.yml",
            ".github/actions/upload-artifact-retry/action.yml",
            ".github/actions/download-artifact-retry/action.yml",
        ):
            self.assertIn(expected, files)

    def test_download_verification_rejects_a_truncated_zstd_frame(self) -> None:
        document = workflow_gates.parse_workflow(str(_local_target(DOWNLOAD_RETRY)))
        steps = document["runs"]["steps"]
        verifications = [step for step in steps if str(step.get("id", "")).startswith("verify-")]
        self.assertEqual(len(verifications), 3)
        self.assertEqual([step.get("continue-on-error") for step in verifications], ["true", "true", None])
        for index, verify in enumerate(verifications, 1):
            with self.subTest(attempt=index):
                self.assertEqual(verify["id"], f"verify-{index}")
                self.assertIn(f"steps.attempt-{index}.outcome == 'success'", verify["if"])
                self.assertIn("inputs.verify-zstd != ''", verify["if"])
                self.assertEqual(verify["env"]["ARCHIVE_PATH"], "${{ inputs.path }}/${{ inputs.verify-zstd }}")
                self.assertLess(steps.index(next(step for step in steps if step.get("id") == f"attempt-{index}")),
                                steps.index(verify))
                with tempfile.TemporaryDirectory() as temp:
                    archive = Path(temp) / "archive with spaces.tar.zst"
                    compressed = subprocess.run(["zstd", "-q", "-c"], input=b"nextest test executable" * 1000,
                                                capture_output=True, check=True).stdout
                    env = dict(os.environ, ARCHIVE_PATH=str(archive))
                    for payload, valid in ((compressed, True), (compressed[:-1], False)):
                        archive.write_bytes(payload)
                        result = subprocess.run(["bash", "-c", verify["run"]], env=env,
                                                text=True, capture_output=True, check=False)
                        self.assertEqual(result.returncode == 0, valid, result.stdout + result.stderr)

    def test_required_check_artifact_steps_use_the_retrying_actions(self) -> None:
        wrappers = {str(_local_target(action).relative_to(REPO_ROOT)) for action in RETRY_ACTIONS}
        offenders = []
        allowed_seen = set()
        for relative, job_id, step in self.steps:
            uses = str(step.get("uses") or "")
            if relative in wrappers or not uses.startswith(DIRECT_ARTIFACT_PREFIXES):
                continue
            key = (relative, job_id, step.get("id"))
            if key in SINGLE_ATTEMPT_ALLOWED:
                allowed_seen.add(key)
                self.assertEqual(step.get("continue-on-error"), "true", key)
                continue
            offenders.append(f"{relative} job {job_id}: {step.get('name') or uses}")
        self.assertEqual(
            offenders,
            [],
            "artifact steps in required checks must use "
            f"{UPLOAD_RETRY} or {DOWNLOAD_RETRY}, not a single attempt",
        )
        self.assertEqual(allowed_seen, SINGLE_ATTEMPT_ALLOWED, "stale single-attempt allowance")
        self.assertGreaterEqual(len(self.calls(UPLOAD_RETRY)), len(REQUIRED_DATA_UPLOADS))
        self.assertGreaterEqual(len(self.calls(DOWNLOAD_RETRY)), 8)

    def test_retry_call_sites_are_never_continue_on_error(self) -> None:
        for action in RETRY_ACTIONS:
            for relative, job_id, step in self.calls(action):
                with self.subTest(file=relative, job=job_id, name=step.get("with", {}).get("name")):
                    self.assertNotIn("continue-on-error", step)

    def test_required_data_uploads_still_fail_on_missing_files(self) -> None:
        by_name = {
            step.get("with", {}).get("name"): step for _, _, step in self.calls(UPLOAD_RETRY)
        }
        for name in sorted(REQUIRED_DATA_UPLOADS):
            with self.subTest(artifact=name):
                self.assertIn(name, by_name)
                self.assertEqual(by_name[name]["with"].get("if-no-files-found"), "error")

    def test_retry_actions_try_three_times_with_growing_pauses(self) -> None:
        for action, inner in RETRY_ACTIONS.items():
            with self.subTest(action=action):
                document = workflow_gates.parse_workflow(str(_local_target(action)))
                self.assertEqual(document["runs"]["using"], "composite")
                steps = document["runs"]["steps"]
                transfer_steps = [step for step in steps if not str(step.get("id", "")).startswith("verify-")]
                kinds = ["attempt" if step.get("uses") == inner else step.get("run") for step in transfer_steps]
                self.assertEqual(kinds, ["attempt", "sleep 60", "attempt", "sleep 120", "attempt"])
                attempts = [step for step in steps if step.get("uses") == inner]
                self.assertEqual([step["id"] for step in attempts], ["attempt-1", "attempt-2", "attempt-3"])
                self.assertEqual(attempts[0]["if"], "${{ !cancelled() }}")
                for index in (1, 2):
                    failure = f"steps.attempt-{index}.outcome == 'failure'"
                    if action == DOWNLOAD_RETRY:
                        failure = f"({failure} || steps.verify-{index}.outcome == 'failure')"
                    retry_if = f"${{{{ !cancelled() && {failure} }}}}"
                    self.assertEqual(attempts[index]["if"], retry_if)
                    self.assertEqual(transfer_steps[2 * index - 1]["if"], retry_if)
                    self.assertEqual(transfer_steps[2 * index - 1]["shell"], "bash")
                # Only the last attempt may fail the action: that is what keeps
                # a missing artifact failing the job.
                self.assertEqual([step.get("continue-on-error") for step in attempts], ["true", "true", None])
                inputs = document["inputs"]
                for step in attempts:
                    for name in inputs:
                        if name == "verify-zstd":
                            continue
                        self.assertEqual(step["with"][name], f"${{{{ inputs.{name} }}}}")
                if action == UPLOAD_RETRY:
                    self.assertEqual(inputs["if-no-files-found"]["default"], "warn")
                    self.assertNotIn("overwrite", attempts[0]["with"])
                    self.assertEqual([step["with"].get("overwrite") for step in attempts[1:]], ["true", "true"])


# Time a job must keep after its inner caps for a retrying artifact transfer:
# the retry actions pause 60 s + 120 s and make up to three attempts.
RETRY_MARGIN_MINUTES = 5.0

# The word `timeout` followed by a duration, as the coreutils command is
# written (a `--timeout` flag does not match), with its options. `-k`/`-s`
# take their value as a separate word. This is a text match, not a shell
# parse, so a quoted "timeout 5m" would also count.
_SHELL_TIMEOUT = re.compile(
    r"(?<![-\w])timeout\s+"
    r"((?:(?:-k|-s|--kill-after|--signal)\s+\S+\s+|-\S+\s+)*)"
    r"(\d+(?:\.\d+)?)([smhd]?)(?=\s)"
)
_KILL_AFTER = re.compile(r"(?:--kill-after=|-k\s+)(\d+(?:\.\d+)?)([smhd]?)")
_MINUTES_PER_UNIT = {"": 1 / 60, "s": 1 / 60, "m": 1.0, "h": 60.0, "d": 1440.0}


def step_cap_minutes(step: dict) -> float:
    """A step's declared cap in minutes. The `timeout` commands found in its
    script are summed (they run one after another), each with any
    --kill-after grace added; when the step also sets `timeout-minutes`, the
    smaller of the two applies. 0 means the step declares no cap."""
    step_limit = float(step["timeout-minutes"]) if "timeout-minutes" in step else 0.0
    shell_limit = 0.0
    run = step.get("run")
    if isinstance(run, str):
        for options, number, unit in _SHELL_TIMEOUT.findall(run):
            shell_limit += float(number) * _MINUTES_PER_UNIT[unit]
            kill = _KILL_AFTER.search(options)
            if kill:
                shell_limit += float(kill.group(1)) * _MINUTES_PER_UNIT[kill.group(2)]
    if step_limit and shell_limit:
        return min(step_limit, shell_limit)
    return step_limit or shell_limit


def expanded_steps(steps: list) -> list[dict]:
    """A job's steps with local composite actions replaced by their own steps,
    except the retry actions, which stay as single steps to be found."""
    expanded: list[dict] = []
    for step in steps:
        if not isinstance(step, dict):
            continue
        uses = str(step.get("uses") or "")
        target = _local_target(uses)
        if uses in RETRY_ACTIONS or target is None or target.name != "action.yml":
            expanded.append(step)
            continue
        runs = workflow_gates.parse_workflow(str(target)).get("runs") or {}
        expanded.extend(expanded_steps(runs.get("steps") or []))
    return expanded


def retry_job_budgets() -> list[tuple[str, str, float, float]]:
    """(file, job, job timeout, inner caps before the job's last retrying
    artifact step) for every job that uses a retry action, in tests.yml and
    the reusable workflows it calls."""
    files = sorted({
        relative for relative, _, _ in required_check_steps()
        if relative.startswith(".github/workflows/")
    })
    budgets = []
    for relative in files:
        jobs = workflow_gates.parse_workflow(str(REPO_ROOT / relative)).get("jobs") or {}
        for job_id, job in jobs.items():
            if not isinstance(job, dict):
                continue
            steps = expanded_steps(job.get("steps") or [])
            retries = [i for i, step in enumerate(steps) if step.get("uses") in RETRY_ACTIONS]
            if not retries:
                continue
            caps = sum(step_cap_minutes(step) for step in steps[: retries[-1]])
            # GitHub's default job timeout is 360 minutes.
            timeout = float(job.get("timeout-minutes", 360))
            budgets.append((relative, job_id, timeout, caps))
    return budgets


class ArtifactRetryTimeBudgetTests(unittest.TestCase):
    """A job that hits its own timeout is cancelled before its later steps run,
    so a retrying upload placed after a capped test step must still fit inside
    the job's timeout when that step uses its whole cap."""

    def test_jobs_with_retrying_transfers_leave_time_beyond_their_inner_caps(self) -> None:
        short = [
            f"{relative} job {job_id}: timeout-minutes {timeout:g} < inner caps "
            f"{caps:g} + {RETRY_MARGIN_MINUTES:g}"
            for relative, job_id, timeout, caps in retry_job_budgets()
            if timeout < caps + RETRY_MARGIN_MINUTES
        ]
        self.assertEqual(short, [], "raise these jobs' timeout-minutes; do not shrink their test caps")

    def test_budget_scan_sees_the_capped_opencode2_matrix(self) -> None:
        budgets = {(relative, job_id): caps for relative, job_id, _, caps in retry_job_budgets()}
        self.assertGreaterEqual(len(budgets), 15)
        self.assertAlmostEqual(budgets[(".github/workflows/_e2e-suite.yml", "e2e-opencode2-linux")], 22.5)
        # The integration-shard action's download is found through the composite.
        self.assertIn((".github/workflows/_unit-suite.yml", "unit-macos-nextest"), budgets)

    def test_step_caps_read_timeout_commands_and_step_limits(self) -> None:
        for step, minutes in (
            ({"run": "timeout --signal=TERM --kill-after=30s 22m bash run.sh"}, 22.5),
            ({"run": "timeout -k 60 10m make\ntimeout 90 make check"}, 12.5),
            ({"run": "time bun test --timeout 30000 src/"}, 0.0),
            ({"run": "timeout 1h make", "timeout-minutes": "15"}, 15.0),
            ({"timeout-minutes": "7"}, 7.0),
            ({"uses": "actions/checkout@v5"}, 0.0),
        ):
            with self.subTest(step=step):
                self.assertAlmostEqual(step_cap_minutes(step), minutes)


if __name__ == "__main__":
    unittest.main()
