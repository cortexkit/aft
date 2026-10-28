#!/usr/bin/env python3
"""Unit tests for GitHub Actions branch-filter glob semantics."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
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
                "macos": ("macOS", "macos-26", "20"),
                "windows": ("Windows", "windows-2022", "25"),
            }[platform]
            with self.subTest(platform=platform, shard=shard):
                self.assertEqual(job["name"], f"Cargo integration shard {shard}/3 ({label})")
                self.assertEqual(job["runs-on"], runner)
                self.assertEqual(job["timeout-minutes"], timeout)
                self.assertEqual(job["needs"], f"unit-{platform}-nextest-archive")
                self.assertNotIn("strategy", job)
                self.assertEqual(job["steps"], [{"uses": "actions/checkout@v5"}, step])
        action = workflow_gates.parse_workflow(str(root / ".github/actions/integration-shard/action.yml"))
        self.assertEqual(action["runs"]["using"], "composite")
        steps = action["runs"]["steps"]
        gate_steps = [step for step in steps if step.get("run") == "./scripts/rust-test-gate.sh"]
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

    def test_every_windows_ci_lane_tunes_defender_before_other_setup(self) -> None:
        root = Path(__file__).resolve().parents[2]
        action = "./.github/actions/windows-ci-defender"
        calls = []
        for filename in ("_unit-suite.yml", "_e2e-suite.yml"):
            suite = workflow_gates.parse_workflow(str(root / ".github/workflows" / filename))
            for name, job in suite["jobs"].items():
                steps = job.get("steps", [])
                direct = [i for i, step in enumerate(steps) if step.get("uses") == action]
                if job.get("runs-on") != "windows-2022":
                    self.assertEqual(direct, [], name)
                    continue
                shared = [step for step in steps if step.get("uses") == "./.github/actions/integration-shard"]
                if shared:
                    self.assertEqual(shared[0]["with"]["platform"], "windows")
                else:
                    self.assertEqual(direct, [1], name)
                    self.assertNotIn("continue-on-error", steps[1])
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
            self.assertEqual(commands.count(command), 1, f"archive must run {probe} exactly once")
            self.assertLess(build, commands.index(command))
            self.assertLess(commands.index(command), proof)
            for job_id in suite["jobs"]:
                if job_id.startswith("unit-windows-") and job_id != "unit-windows-nextest-archive":
                    self.assertNotIn("ci-windows-probes.py", workflow_job(job_id))
        self.assertFalse(any(command.startswith("cargo test ") for command in commands))
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
        self.assertEqual(gate["env"]["AFT_NEXTEST_ARCHIVE_FILE"], "${{ runner.temp }}/windows-nextest.tar.zst")
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
        # 45 minutes: the split build still runs 19-28 min, and a slow
        # macOS runner has reached 30.
        self.assertEqual(job["timeout-minutes"], "45")
        cargo_steps = [step["run"] for step in job["steps"] if "run" in step]
        build_steps = [command for command in cargo_steps if command.startswith("cargo ")]
        self.assertEqual(len(build_steps), 3)
        for command in build_steps:
            self.assertIn('--config "$LIBTEST_CARGO_CONFIG"', command)
        archive = next(command for command in build_steps if "nextest archive" in command)
        self.assertIn("--timings", archive)
        upload = next(step for step in job["steps"] if step.get("with", {}).get("name") == "macos-libtest-archive")
        paths = upload["with"]["path"].splitlines()
        self.assertTrue(all(path.startswith("${{ runner.temp }}/") for path in paths))
        # Timings survive a failure in any step after the archive build.
        timings = next(step for step in job["steps"] if step.get("with", {}).get("name") == "macos-libtest-timings")
        self.assertIn("always()", timings["if"])
        self.assertEqual(timings["with"]["path"], "target/cargo-timings/")
        steps = job["steps"]
        archive_index = next(i for i, step in enumerate(steps) if "nextest archive" in step.get("run", ""))
        self.assertEqual(steps.index(timings), archive_index + 1)

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
            if step.get("run") == "./scripts/rust-test-gate.sh"
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
        self.assertIn('"$test_name" -- --list', timeout_step)
        self.assertIn('"$test_name" -- --exact', timeout_step)
        self.assertIn("expected test missing from integration inventory", timeout_step)
        self.assertIn(
            "cargo test -p agent-file-tools --features test-timing-hooks --test integration -- scope_revocation",
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
            consumer = next(index for index, step in enumerate(steps) if step.get("run") in (
                "./scripts/rust-test-gate.sh", "python3 scripts/rust-test-inventory-proof.py",
            ))
            for artifact, archive in (("macos-libtest-archive", "macos-libtest-nextest.tar.zst"),
                                      ("macos-bin-archive", "macos-bin-nextest.tar.zst")):
                download = next(step for step in steps if step.get("with", {}).get("name") == artifact)
                with self.subTest(job=job, artifact=artifact):
                    self.assertEqual(download["uses"], DOWNLOAD_RETRY)
                    self.assertEqual(download["with"].get("verify-zstd"), archive)
                    self.assertNotIn("continue-on-error", download)
                    self.assertLess(steps.index(download), consumer)

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
        self.assertIn("AFT_BINARY_PATH: ${{ github.workspace }}/target/debug/aft", conformance)


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
