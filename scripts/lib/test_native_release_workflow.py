#!/usr/bin/env python3
"""Keep landing CI fast and publication bound to shipped-profile E2E artifacts."""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import tomllib
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("workflow_gates", Path(__file__).with_name("workflow-gates.py"))
assert SPEC is not None and SPEC.loader is not None
parser = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parser)

COVERED = {
    "darwin-arm64": ("aarch64-apple-darwin", "bash tests/macos-e2e/run.sh", "target/release/aft"),
    "win32-x64": ("x86_64-pc-windows-msvc", "tests/windows-e2e/run.ps1", "target/x86_64-pc-windows-msvc/release/aft.exe"),
    "linux-x64": ("x86_64-unknown-linux-gnu", "docker run --rm --platform linux/amd64",  "target/x86_64-unknown-linux-gnu/release/aft"),
}
PLATFORMS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64")
TAG_PUBLISH_GUARD = "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"


def commands(job):
    return "\n".join(step.get("run", "") for step in job["steps"])


def inline_python(job, name):
    run = next(step["run"] for step in job["steps"] if step.get("name") == name)
    lines = run.strip().splitlines()
    assert lines[0] == "python3 - <<'PY'" and lines[-1] == "PY", name
    # The lightweight workflow parser strips block-scalar indentation. Preserve
    # the real Python indentation when executing the workflow's inline scripts.
    source = (ROOT / ".github/workflows/release.yml").read_text()
    block = re.search(
        rf"(?ms)^      - name: {re.escape(name)}\n.*?^          python3 - <<'PY'\n(.*?)^          PY$",
        source,
    )
    assert block is not None, name
    return compile(textwrap.dedent(block[1]), f"release.yml: {name}", "exec")


class NativeReleaseWorkflowTests(unittest.TestCase):
    def workflow(self, name):
        return parser.parse_workflow(str(ROOT / ".github/workflows" / name))

    def test_main_and_train_native_e2e_use_the_staging_selector(self):
        self.assertEqual(self.workflow("tests.yml")["jobs"]["e2e"]["with"]["cargo_profile"], "stage")
        suite = self.workflow("_e2e-suite.yml")
        self.assertEqual(suite["on"]["workflow_call"]["inputs"]["cargo_profile"]["default"], "stage")
        self.assertFalse(any(job.startswith("release-library-") for job in suite["jobs"]))
        for suffix, name in (("macos", "E2E (macOS native)"), ("windows", "E2E (Windows native)")):
            job = suite["jobs"][f"e2e-{suffix}"]
            self.assertEqual(job["name"], name)
            self.assertNotIn("needs", job)
            self.assertEqual(job["if"], "${{ github.event_name != 'pull_request' }}")
            self.assertNotIn("pack-binary", commands(job))

    def test_macos_landing_e2e_uses_debug_without_changing_publication(self):
        job = self.workflow("_e2e-suite.yml")["jobs"]["e2e-macos"]
        self.assertEqual(job["env"]["CARGO_PROFILE"], "${{ inputs.cargo_profile == 'stage' && 'dev' || 'release' }}")
        self.assertEqual(job["env"]["CARGO_PROFILE_DIR"], "${{ inputs.cargo_profile == 'stage' && 'debug' || 'release' }}")
        self.assertEqual(job["env"]["CARGO_PROFILE_DEV_DEBUG"], "line-tables-only")
        self.assertIn('cargo build --profile "$CARGO_PROFILE"', commands(job))
        for command in ('codesign -f -s -', 'codesign --verify --strict', '"target/$CARGO_PROFILE_DIR/aft" --version'):
            self.assertIn(command, commands(job))
        run = next(step for step in job["steps"] if step.get("run") == "bash tests/macos-e2e/run.sh")
        self.assertEqual(run["env"]["AFT_BINARY_PATH"], "${{ github.workspace }}/target/${{ env.CARGO_PROFILE_DIR }}/aft")

    def test_every_publish_job_requires_shipped_e2e_on_release_sha(self):
        jobs = self.workflow("release.yml")["jobs"]
        required = {f"e2e-{platform}" for platform in COVERED}
        for name in ("publish-crates", "publish-npm", "github-release"):
            with self.subTest(job=name):
                self.assertTrue(required.issubset(jobs[name]["needs"]))
                self.assertIn("gate", jobs[name]["needs"])
                self.assertEqual(jobs[name]["if"], TAG_PUBLISH_GUARD)
        for platform, (target, harness, binary) in COVERED.items():
            with self.subTest(platform=platform):
                build = jobs[f"build-{platform}"]
                self.assertGreater(int(build["timeout-minutes"]), 0)
                self.assertIn("build --release", commands(build))
                self.assertNotIn("select-native-release-artifact", commands(build))
                self.assertIn("pack-binary", commands(build))
                e2e = jobs[f"e2e-{platform}"]
                self.assertEqual(e2e["needs"], f"build-{platform}")
                self.assertNotIn("if", e2e)
                self.assertNotIn("continue-on-error", e2e)
                self.assertNotIn("cargo build", commands(e2e))
                self.assertIn(harness, commands(e2e))
                download = next(step for step in e2e["steps"] if step.get("uses") == "actions/download-artifact@v4")
                self.assertEqual(download["with"]["name"], f"shipped-{platform}-${{{{ github.sha }}}}")
                self.assertNotIn("run-id", download["with"])
                self.assertIn("unpack-binary", commands(e2e))
                if platform != "darwin-arm64":
                    self.assertIn(target, commands(e2e))
                self.assertEqual(e2e["steps"][0]["uses"], "actions/checkout@v5")
                self.assertNotIn("with", e2e["steps"][0])
                test_step = next(step for step in e2e["steps"] if step.get("name") == "Run shipped E2E suite")
                self.assertIn(binary, test_step["env"]["AFT_BINARY_PATH"].replace("\\", "/"))
                self.assertNotIn("if", test_step)
                self.assertNotIn("continue-on-error", test_step)
        profile = (ROOT / "Cargo.toml").read_text()
        self.assertIn("lto = true", profile)
        self.assertIn("codegen-units = 1", profile)

    def test_published_artifacts_are_verified_tested_bytes_not_rebuilds(self):
        jobs = self.workflow("release.yml")["jobs"]
        for platform in COVERED:
            with self.subTest(platform=platform):
                e2e = jobs[f"e2e-{platform}"]
                steps = e2e["steps"]
                run_index = next(i for i, step in enumerate(steps) if step.get("name") == "Run shipped E2E suite")
                verify_index = next(i for i, step in enumerate(steps) if step.get("name") == "Verify tested bytes are unchanged")
                self.assertIn(" verify-binary ", steps[verify_index]["run"])
                upload_index = next(i for i, step in enumerate(steps) if step.get("uses") == "actions/upload-artifact@v4" and step["with"]["name"].startswith("tested-"))
                self.assertLess(run_index, verify_index)
                self.assertLess(verify_index, upload_index)
                self.assertNotIn("if", steps[verify_index])
                self.assertNotIn("if", steps[upload_index])
                self.assertNotIn("continue-on-error", steps[verify_index])
                self.assertEqual(steps[upload_index]["with"]["name"], f"tested-{platform}-${{{{ github.sha }}}}")
                self.assertEqual(steps[upload_index]["with"]["path"], "${{ runner.temp }}/shipped-" + platform + "/release.tar")
                download = next(step for step in steps if step.get("uses") == "actions/download-artifact@v4")
                self.assertEqual(download["with"]["path"] + "/release.tar", steps[upload_index]["with"]["path"])
                for publish in ("publish-npm", "github-release"):
                    job = jobs[publish]
                    tested_download = next(step for step in job["steps"] if step.get("uses") == "actions/download-artifact@v4" and step.get("with", {}).get("name") == f"tested-{platform}-${{{{ github.sha }}}}")
                    self.assertNotIn("run-id", tested_download["with"])
                    verification = next(step for step in job["steps"] if "unpack-published" in step.get("run", "") and f"tested-{platform}/release.tar" in step["run"])
                    self.assertNotIn("if", verification)
                    self.assertNotIn("continue-on-error", verification)
                    expected_destination = f"packages/npm/{platform}/bin/aft" if publish == "publish-npm" else f"release-assets/aft-{platform}"
                    self.assertIn(expected_destination, verification["run"])
                    self.assertNotIn("cargo build", commands(job))

    def test_every_path_dependency_of_a_published_crate_is_published_first(self):
        # crates.io refuses a crate whose dependencies it can't resolve, and it
        # does so only at release time, after every build has passed: 0.63.2
        # failed this way when a path dependency was marked `publish = false`.
        run = next(
            step["run"] for step in self.workflow("release.yml")["jobs"]["publish-crates"]["steps"]
            if "cargo publish --package agent-file-tools" in step.get("run", "")
        )
        sync = (ROOT / "scripts/version-sync.mjs").read_text()

        def path_dependencies(manifest_dir):
            manifest = tomllib.loads((manifest_dir / "Cargo.toml").read_text())
            tables = [manifest.get("dependencies", {}), manifest.get("build-dependencies", {})]
            tables += [t.get("dependencies", {}) for t in manifest.get("target", {}).values()]
            tables += [t.get("build-dependencies", {}) for t in manifest.get("target", {}).values()]
            for table in tables:
                for key, spec in table.items():
                    if isinstance(spec, dict) and "path" in spec:
                        yield key, spec, (manifest_dir / spec["path"]).resolve()

        published_before = run.index("cargo publish --package agent-file-tools")
        pending, seen = [ROOT / "crates/aft"], set()
        while pending:
            for key, spec, dep_dir in path_dependencies(pending.pop()):
                if dep_dir in seen:
                    continue
                seen.add(dep_dir)
                pending.append(dep_dir)
                package = tomllib.loads((dep_dir / "Cargo.toml").read_text())["package"]
                name = package["name"]
                with self.subTest(dependency=key, package=name):
                    self.assertNotEqual(package.get("publish"), False, f"{name} must be publishable")
                    self.assertIn("version", spec, f"{key} needs a version pin to publish")
                    publish = f"cargo publish --package {name} "
                    self.assertIn(publish, run, f"release.yml must publish {name}")
                    self.assertLess(run.index(publish), published_before, f"{name} must publish before agent-file-tools")
                    relative = dep_dir.relative_to(ROOT).parts
                    self.assertIn(
                        ", ".join(f'"{part}"' for part in relative), sync,
                        f"version-sync.mjs must keep {name}'s version in lockstep",
                    )
        self.assertTrue(seen, "agent-file-tools has no path dependencies; the check found nothing")


class ReleaseRehearsalWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflow = parser.parse_workflow(str(ROOT / ".github/workflows/release.yml"))
        self.jobs = self.workflow["jobs"]
        self.rehearsal = self.jobs["rehearse-publish"]

    def test_nightly_reuses_dispatch_builds_without_the_tag_ci_gate(self):
        self.assertEqual(self.workflow["on"]["schedule"], [{"cron": "17 2 * * *"}])
        self.assertIn("workflow_dispatch", self.workflow["on"])
        self.assertEqual(self.workflow["on"]["push"], {"tags": ["v*"]})
        self.assertEqual(self.jobs["gate"]["if"], "github.event_name == 'push'")
        self.assertEqual(
            self.rehearsal["if"],
            "github.event_name == 'schedule' || github.event_name == 'workflow_dispatch'",
        )
        required = {name for name in self.jobs if name.startswith(("build-", "e2e-"))}
        self.assertEqual(set(self.rehearsal["needs"]), required)
        self.assertNotIn("gate", self.rehearsal["needs"])
        for name in required:
            with self.subTest(job=name):
                self.assertNotIn("if", self.jobs[name])
                self.assertNotIn("continue-on-error", self.jobs[name])
        self.assertEqual(self.rehearsal["permissions"], {"contents": "read", "actions": "read"})
        self.assertNotIn("secrets.", json.dumps(self.rehearsal))

    def test_schedule_and_dispatch_skip_every_external_publication_step(self):
        external_jobs = set()
        for name, job in self.jobs.items():
            for step in job.get("steps", []):
                run = step.get("run", "")
                publishes = any(
                    "--dry-run" not in line
                    for line in re.findall(r"(?m)^\s*(?:cargo|npm) publish\b[^\n]*", run)
                )
                mutates_release = re.search(r"\b(?:gh release (?:create|upload|edit)|git (?:push|tag))\b", run)
                external_action = "github-releases-to-discord@" in step.get("uses", "")
                if publishes or mutates_release or external_action:
                    external_jobs.add(name)
                    with self.subTest(job=name, step=step.get("name")):
                        expected = ("success() && " if name == "discord-announce" else "") + TAG_PUBLISH_GUARD
                        self.assertEqual(job.get("if"), expected)
        self.assertEqual(external_jobs, {"publish-crates", "publish-npm", "github-release", "discord-announce"})

    def test_all_jobs_have_explicit_budgets_and_builds_have_measured_headroom(self):
        for name, job in self.jobs.items():
            with self.subTest(job=name):
                self.assertGreater(int(job["timeout-minutes"]), 0)
                self.assertLessEqual(int(job["timeout-minutes"]), 360)
        peaks = {
            "darwin-arm64": 16.4, "darwin-x64": 18.3,
            "linux-arm64": 27.2, "linux-x64": 25.4,
            "win32-arm64": 100.5, "win32-x64": 48.7,
        }
        for platform, minutes in peaks.items():
            with self.subTest(platform=platform):
                self.assertGreaterEqual(int(self.jobs[f"build-{platform}"]["timeout-minutes"]), 2 * minutes)

    def test_scheduled_internal_transfers_have_short_retention(self):
        uploads = []
        for job in self.jobs.values():
            if "github.event_name == 'push'" in job.get("if", ""):
                continue
            uploads.extend(step for step in job.get("steps", []) if step.get("uses") == "actions/upload-artifact@v4")
        self.assertGreater(len(uploads), 0)
        for step in uploads:
            with self.subTest(artifact=step["with"]["name"]):
                self.assertEqual(step["with"]["retention-days"], "${{ github.event_name == 'schedule' && 2 || 90 }}")

    def test_crates_dry_run_selects_all_siblings_together_and_verifies_them(self):
        steps = self.rehearsal["steps"]
        sync = next(step["run"] for step in steps if step.get("name") == "Set an unpublished rehearsal version")
        self.assertIn("-rehearsal.${process.env.GITHUB_RUN_NUMBER}.${process.env.GITHUB_RUN_ATTEMPT}", sync)
        self.assertIn('node scripts/version-sync.mjs "$version"', sync)
        self.assertIn("cargo update -w", sync)
        self.assertNotIn("--offline", sync)
        self.assertIn("dtolnay/rust-toolchain@stable", [step.get("uses") for step in steps])
        publish = next(step["run"] for step in steps if step.get("name") == "Rehearse crates.io publishing")
        self.assertEqual(
            publish,
            "cargo publish --dry-run --locked --allow-dirty -p aft-tokenizer -p aft-secret-rules -p agent-file-tools",
        )
        self.assertNotIn("--no-verify", publish)
        names = [step.get("name") for step in steps]
        self.assertLess(names.index("Rehearse crates.io publishing"), names.index("Check packaged crate sizes"))

    def test_npm_rehearsal_prepares_the_same_ten_packages_and_tested_binaries(self):
        published = self.jobs["publish-npm"]
        def transfers(job):
            return [step["with"] for step in job["steps"] if step.get("uses") == "actions/download-artifact@v4"]
        self.assertEqual(transfers(self.rehearsal), transfers(published))
        for platform in COVERED:
            name = {"darwin-arm64": "macOS", "win32-x64": "Windows", "linux-x64": "Linux"}[platform]
            step_name = f"Verify tested {name} binary for npm"
            real = next(step["run"] for step in published["steps"] if step.get("name") == step_name)
            dry = next(step["run"] for step in self.rehearsal["steps"] if step.get("name") == step_name)
            self.assertEqual(dry, real)
        run = commands(self.rehearsal)
        self.assertIn("bun run --cwd packages/aft-bridge build", run)
        self.assertIn("bun run --filter '*' build", run)
        self.assertIn("node scripts/validate-packages.mjs", run)
        self.assertIn("--candidate --require-platform-artifacts", run)
        pack = next(step["run"] for step in self.rehearsal["steps"] if step.get("name") == "Rehearse every npm package")
        expected = [f"./packages/npm/{platform}" for platform in PLATFORMS]
        expected += [f"./packages/{package}" for package in ("aft-bridge", "opencode-plugin", "pi-plugin", "aft-cli")]
        # Execute the actual loops with a shell function instead of calling npm.
        result = subprocess.run(["bash", "-e", "-c", 'npm() { printf "%s %s %s\\n" "$1" "$2" "$3"; };\n' + pack], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [f"pack --dry-run {package}" for package in expected])

    def test_glibc_audit_uses_both_cross_release_outputs(self):
        for platform, target in (("linux-x64", "x86_64-unknown-linux-gnu"), ("linux-arm64", "aarch64-unknown-linux-gnu")):
            job = self.jobs[f"build-{platform}"]
            self.assertIn(f"cross build --release --target {target}", commands(job))
            cross = next(step for step in job["steps"] if step.get("name") == "Install cross")
            self.assertEqual(cross["with"]["tool"], "cross@0.2.5")
        script = inline_python(self.rehearsal, "Audit Linux glibc floor")
        below_floor = "  1: 0 0 FUNC GLOBAL DEFAULT UND f@GLIBC_2.2.5\n  2: 0 0 FUNC GLOBAL DEFAULT UND g@GLIBC_2.23"
        with mock.patch("subprocess.check_output", side_effect=[below_floor, below_floor]) as readelf:
            exec(script, {})
        self.assertEqual(readelf.call_args_list, [
            mock.call(["readelf", "--dyn-syms", "--wide", f"packages/npm/{platform}/bin/aft"], text=True)
            for platform in ("linux-x64", "linux-arm64")
        ])

    def test_glibc_audit_rejects_above_floor_on_either_architecture_and_empty_symbols(self):
        script = inline_python(self.rehearsal, "Audit Linux glibc floor")
        good = "  1: 0 0 FUNC GLOBAL DEFAULT UND f@GLIBC_2.23"
        for index, platform in enumerate(("linux-x64", "linux-arm64")):
            for version in ("2.24", "2.28", "2.100", "PRIVATE"):
                with self.subTest(platform=platform, version=version):
                    outputs = [good] * index + [f"  1: 0 0 FUNC GLOBAL DEFAULT UND statx@GLIBC_{version}"]
                    with mock.patch("subprocess.check_output", side_effect=outputs):
                        with self.assertRaisesRegex(SystemExit, f"{platform} requires GLIBC_{version}"):
                            exec(script, {})
        with mock.patch("subprocess.check_output", return_value=""):
            with self.assertRaisesRegex(SystemExit, "no required GLIBC versions"):
                exec(script, {})
        with mock.patch("subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "readelf")):
            with self.assertRaises(subprocess.CalledProcessError):
                exec(script, {})

    def test_crate_size_check_accepts_small_archives_and_rejects_limit_or_missing_crates(self):
        script = inline_python(self.rehearsal, "Check packaged crate sizes")
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            target = Path(temporary)
            directory = target / "package"
            directory.mkdir()
            packages = [{"name": name, "version": "0.63.5-rehearsal.1.1"} for name in ("aft-tokenizer", "aft-secret-rules", "agent-file-tools")]
            files = [directory / f"{package['name']}-{package['version']}.crate" for package in packages]
            for file in files:
                file.write_bytes(b"crate")
            metadata = json.dumps({"target_directory": str(target), "packages": packages})
            with mock.patch("subprocess.check_output", return_value=metadata):
                exec(script, {})
                for size in (10 * 1024 * 1024, 10 * 1024 * 1024 + 1):
                    with self.subTest(size=size):
                        with files[-1].open("wb") as archive:
                            archive.truncate(size)
                        with self.assertRaisesRegex(SystemExit, "must be under 10485760 bytes"):
                            exec(script, {})
                files[-1].unlink()
                with self.assertRaisesRegex(AssertionError, "agent-file-tools"):
                    exec(script, {})

    def test_crate_size_check_finds_multi_package_archives_and_checks_every_copy(self):
        script = inline_python(self.rehearsal, "Check packaged crate sizes")
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            target = Path(temporary)
            packages = [{"name": name, "version": "0.63.5-rehearsal.1.1"} for name in ("aft-tokenizer", "aft-secret-rules", "agent-file-tools")]
            metadata = json.dumps({"target_directory": str(target), "packages": packages})
            with mock.patch("subprocess.check_output", return_value=metadata):
                copies = []
                for folder in ("tmp-crate", "tmp-registry"):
                    directory = target / "package" / folder
                    directory.mkdir(parents=True)
                    for package in packages:
                        archive = directory / f"{package['name']}-{package['version']}.crate"
                        archive.write_bytes(b"crate")
                        if package["name"] == "agent-file-tools":
                            copies.append(archive)
                exec(script, {})
                with copies[-1].open("wb") as archive:
                    archive.truncate(10 * 1024 * 1024)
                with self.assertRaisesRegex(SystemExit, "must be under 10485760 bytes"):
                    exec(script, {})
                for archive in copies:
                    archive.unlink()
                (target / "package" / "agent-file-tools-0.1.0.crate").write_bytes(b"stale")
                with self.assertRaisesRegex(AssertionError, "missing packaged crates: agent-file-tools"):
                    exec(script, {})
                for archive in target.rglob("*.crate"):
                    archive.unlink()
                with self.assertRaisesRegex(AssertionError, "missing packaged crates:.*aft-secret-rules.*aft-tokenizer.*agent-file-tools"):
                    exec(script, {})

    def test_tag_canceller_catches_first_failure_and_timeouts_without_waiting_for_other_builds(self):
        job = self.jobs["cancel-failed-tag-builds"]
        self.assertEqual(job["if"], TAG_PUBLISH_GUARD)
        self.assertNotIn("needs", job)
        self.assertEqual(job["permissions"]["actions"], "write")
        self.assertGreater(int(job["timeout-minutes"]), max(int(self.jobs[f"build-{platform}"]["timeout-minutes"]) for platform in PLATFORMS))
        self.assertEqual(job["steps"][0]["env"]["GH_TOKEN"], "${{ github.token }}")
        script = inline_python(job, "Watch builds and cancel on failure")
        environment = {"GITHUB_REPOSITORY": "cortexkit/aft", "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}
        for conclusion in ("failure", "timed_out", "cancelled"):
            with self.subTest(conclusion=conclusion):
                pages = [{"jobs": [
                    {"name": "Build Linux x64", "status": "completed", "conclusion": conclusion},
                    {"name": "Build Windows ARM64", "status": "in_progress", "conclusion": None},
                ]}]
                with mock.patch.dict(os.environ, environment), \
                     mock.patch("subprocess.check_output", side_effect=[json.dumps(pages)]) as api, \
                     mock.patch("subprocess.run") as cancel, mock.patch("time.sleep") as sleep, \
                     mock.patch("builtins.print"):
                    namespace = {}
                    exec(script, namespace)
                self.assertEqual(namespace["build_names"], {job["name"] for name, job in self.jobs.items() if name.startswith("build-")})
                self.assertIn("repos/cortexkit/aft/actions/runs/123/attempts/2/jobs?per_page=100", api.call_args.args[0])
                self.assertIn("--paginate", api.call_args.args[0])
                cancel.assert_called_once_with(["gh", "run", "cancel", "123", "--repo", "cortexkit/aft"], check=True)
                sleep.assert_not_called()

    def test_tag_canceller_waits_for_all_builds_and_exits_on_success(self):
        script = inline_python(self.jobs["cancel-failed-tag-builds"], "Watch builds and cancel on failure")
        environment = {"GITHUB_REPOSITORY": "cortexkit/aft", "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}
        complete = [
            {"name": job["name"], "status": "completed", "conclusion": "success"}
            for name, job in self.jobs.items() if name.startswith("build-")
        ]
        pending = json.dumps([{"jobs": complete[:1]}])
        done = json.dumps([{"jobs": complete[:3]}, {"jobs": complete[3:]}])
        with mock.patch.dict(os.environ, environment), \
             mock.patch("subprocess.check_output", side_effect=[pending, done]), \
             mock.patch("subprocess.run") as cancel, mock.patch("time.sleep") as sleep:
            exec(script, {})
        sleep.assert_called_once_with(30)
        cancel.assert_not_called()


if __name__ == "__main__":
    unittest.main()
