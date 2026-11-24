#!/usr/bin/env python3
"""Keep landing CI fast and publication bound to shipped-profile E2E artifacts."""

import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import tomllib
import unittest
from unittest import mock

from release_retry import VERSION_FILES


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
RELEASE_CRATES = ("aft-tokenizer", "aft-secret-rules", "aft-store", "aft-base", "aft-db", "aft-parse", "aft-edit", "aft-index", "aft-inspect", "aft-github", "aft-engine", "aft-tools", "aft-subc", "agent-file-tools")
REHEARSAL_VERSION = "0.63.5-rehearsal.1.1"


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


def write_rehearsal_crate(path, pin=f"={REHEARSAL_VERSION}"):
    if not path.name.startswith("agent-file-tools-"):
        path.write_bytes(b"crate")
        return
    contents = f'[dependencies.aft-engine]\nversion = "{pin}"\n'.encode()
    with tarfile.open(path, "w:gz") as archive:
        member = tarfile.TarInfo(f"{path.stem}/Cargo.toml")
        member.size = len(contents)
        archive.addfile(member, io.BytesIO(contents))


class NativeReleaseWorkflowTests(unittest.TestCase):
    def test_macos_npm_signing_uses_shared_hardened_policy_after_strip(self):
        jobs = self.workflow("release.yml")["jobs"]
        for job_name, binary in (
            ("build-darwin-arm64", "target/release/aft"),
            ("build-darwin-x64", "target/x86_64-apple-darwin/release/aft"),
        ):
            with self.subTest(job=job_name):
                steps = jobs[job_name]["steps"]
                strip = next(i for i, step in enumerate(steps) if step.get("name") == "Strip binary")
                sign = next(i for i, step in enumerate(steps) if f"ck_aft_sign_npm {binary}" in step.get("run", ""))
                self.assertLess(strip, sign)
                self.assertIn("source scripts/lib/ck-aft-signature.sh", steps[sign]["run"])
                self.assertIn(f"ck_aft_check_npm_signature {binary} release", steps[sign]["run"])
                self.assertNotIn("codesign -f -s -", commands(jobs[job_name]))

    def test_macos_npm_smokes_share_card_checks_on_unchanged_shipped_bytes(self):
        job = self.workflow("release.yml")["jobs"]["e2e-darwin-arm64"]
        steps = job["steps"]
        smoke = next(i for i, step in enumerate(steps) if step.get("name") == "Verify hardened ONNX and Pulley loads")
        before = "\n".join(step.get("run", "") for step in steps[:smoke])
        after = "\n".join(step.get("run", "") for step in steps[smoke + 1:])
        self.assertIn("unpack-binary", before)
        self.assertIn("ck_aft_check_npm_signature target/release/aft", before)
        self.assertIn("bun run --cwd packages/aft-bridge build", before)
        self.assertIn("verify-binary", after)
        run = steps[smoke]["run"]
        for expected in (
            "ensureOnnxRuntime", 'dir.startsWith(`${root}/onnxruntime/`)',
            'ck_aft_smoke_onnx "$PWD/target/release/aft"',
            'ck_aft_smoke_grammar "$PWD/target/release/aft"',
        ):
            self.assertIn(expected, run)
        stage_card = (ROOT / "scripts/stage-card.sh").read_text()
        self.assertIn('ck_aft_smoke_onnx "$TMP" stage-card', stage_card)
        self.assertIn('ck_aft_smoke_grammar "$TMP" stage-card', stage_card)

    def test_self_hosted_release_builds_prepare_node_and_system_python_before_invoking_them(self):
        jobs = self.workflow("release.yml")["jobs"]
        native = {name: job for name, job in jobs.items() if "self-hosted" in job.get("runs-on", [])}
        self.assertEqual(set(native), {"build-darwin-arm64", "build-darwin-x64",
                                       "build-win32-arm64", "build-win32-x64", "e2e-win32-x64"})
        for name, job in native.items():
            with self.subTest(job=name):
                steps = job["steps"]
                for action, tool, version in (("actions/setup-node@v5", "node", "24"),
                                              ("actions/setup-python@v6", "python3", "3.14")):
                    pattern = r"\bpython(?:3)?\s" if tool == "python3" else r"\bnode\s"
                    uses = [i for i, step in enumerate(steps) if re.search(pattern, step.get("run", ""))]
                    if not uses:
                        self.assertEqual(tool, "python3")
                        continue
                    setup = next(i for i, step in enumerate(steps) if step.get("uses") == action)
                    self.assertLess(setup, uses[0])
                    key = "node-version" if tool == "node" else "python-version"
                    self.assertEqual(steps[setup]["with"][key], version)
                    if tool == "python3":
                        self.assertEqual(steps[setup]["if"], "runner.environment == 'github-hosted'")
                native = next(i for i, step in enumerate(steps) if step.get("uses") == "./.github/actions/native-ci-state")
                first_use = next(i for i, step in enumerate(steps) if re.search(r"\bnode\s", step.get("run", "")))
                self.assertLess(native, first_use)
                if name.startswith("build-darwin-"):
                    self.assertEqual(steps[native]["with"]["phase"], "python")

    def test_windows_release_jobs_reuse_narli_without_mutating_rust_or_the_artifact_contract(self):
        jobs = self.workflow("release.yml")["jobs"]
        for job_id, target, name in (("build-win32-arm64", "aarch64-pc-windows-msvc", "Build Windows ARM64"),
                                      ("build-win32-x64", "x86_64-pc-windows-msvc", "Build Windows x64"),
                                      ("e2e-win32-x64", "x86_64-pc-windows-msvc", "Shipped E2E (Windows x64)")):
            with self.subTest(job=job_id):
                job = jobs[job_id]
                self.assertEqual(job["name"], name)
                self.assertEqual(job["runs-on"], ["self-hosted", "narli"])
                steps = job["steps"]
                self.assertEqual(steps[1]["uses"], "actions/setup-python@v6")
                self.assertEqual(steps[2]["uses"], "./.github/actions/native-ci-state")
                self.assertEqual(steps[2]["with"]["rust-target"], target)
                for step in steps:
                    action = step.get("uses", "")
                    if action.startswith(("dtolnay/rust-toolchain@", "mozilla-actions/sccache-action@", "Swatinem/rust-cache@")):
                        self.assertEqual(step["if"], "runner.environment == 'github-hosted'")
                    if action == "oven-sh/setup-bun@v2":
                        self.assertEqual(step["with"]["no-cache"], "true")
                    if step.get("run", "").startswith("cargo build"):
                        self.assertEqual(step["env"]["CARGO_TARGET_DIR"], "${{ env.AFT_CI_TARGET_DIR }}")
                self.assertNotIn("RUSTC_WRAPPER", job.get("env", {}))
                self.assertNotIn("SCCACHE_GHA_ENABLED", job.get("env", {}))
                self.assertEqual(steps[-1], {"uses": "./.github/actions/native-ci-state", "if": "always()",
                                             "with": {"phase": "cleanup"}})
                if job_id.startswith("build-"):
                    stage = next(i for i, step in enumerate(steps)
                                 if step.get("name") == "Stage the built binary for the existing artifact contract")
                    self.assertIn(f'$env:AFT_CI_TARGET_DIR/{target}/release/aft.exe', steps[stage]["run"])
                    self.assertIn("Get-FileHash $source", steps[stage]["run"])
                    upload = next(i for i, step in enumerate(steps) if step.get("uses") == "actions/upload-artifact@v4")
                    self.assertLess(stage, upload)
        arm = commands(jobs["build-win32-arm64"])
        self.assertIn("Hostx64/arm64/cl.exe", arm)
        self.assertIn("lib/arm64/vcruntime.lib", arm)
        e2e = jobs["e2e-win32-x64"]
        self.assertEqual(e2e["steps"][2]["with"]["rust-version"], "${{ needs.build-win32-x64.outputs.rust_version }}")
        run = next(step for step in e2e["steps"] if step.get("name") == "Run shipped E2E suite")
        self.assertIn("Remove-Item Env:CARGO_TARGET_DIR, Env:CARGO_BUILD_TARGET_DIR", run["run"])

    def test_windows_arm64_release_validates_managed_clang_and_actual_coff_target_before_building(self):
        steps = self.workflow("release.yml")["jobs"]["build-win32-arm64"]["steps"]
        probe = next(i for i, step in enumerate(steps)
                     if step.get("name") == "Verify administrator-installed LLVM ARM64 support")
        build = next(i for i, step in enumerate(steps) if step.get("name") == "Build")
        self.assertLess(probe, build)
        self.assertNotIn("if", steps[probe])
        self.assertNotIn("continue-on-error", steps[probe])
        self.assertEqual(steps[probe]["shell"], "pwsh")
        run = steps[probe]["run"]
        self.assertIn("Get-Command clang -CommandType Application", run)
        self.assertIn('throw "clang is missing from the runner service PATH', run)
        self.assertIn("& $clang.Source --version", run)
        self.assertIn("--target=aarch64-pc-windows-msvc -c $source -o $object", run)
        self.assertIn("$LASTEXITCODE -ne 0", run)
        self.assertIn("$env:AFT_CI_JOB_DIR", run)
        self.assertIn("$bytes[0] -ne 0x64 -or $bytes[1] -ne 0xaa", run)
        self.assertIn("finally {", run)
        self.assertIn("Remove-Item -LiteralPath $source, $object", run)
        self.assertNotRegex(run, r"(?i)\b(winget|choco|scoop|Install-Package|Invoke-WebRequest)\b")

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
        for command in ('codesign -f -s -', 'codesign --verify --strict', '"$AFT_CI_TARGET_DIR/$CARGO_PROFILE_DIR/aft" --version'):
            self.assertIn(command, commands(job))
        run = next(step for step in job["steps"] if "bash tests/macos-e2e/run.sh" in step.get("run", "").splitlines())
        self.assertEqual(run["env"]["AFT_BINARY_PATH"], "${{ env.AFT_CI_TARGET_DIR }}/${{ env.CARGO_PROFILE_DIR }}/aft")

    def test_macos_e2e_reuses_coreutils_without_write_access_to_homebrew(self):
        source = (ROOT / "tests/macos-e2e/run.sh").read_text()
        start = source.index('echo "── Installing GNU coreutils')
        end = source.index("# ---- Install OpenCode", start)
        for executable in ("timeout", "gtimeout"):
            with self.subTest(executable=executable), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                tools = root / "bin"
                tools.mkdir()
                for name in ("bash", "mkdir", "ln"):
                    installed = shutil.which(name)
                    assert installed is not None
                    (tools / name).symlink_to(installed)
                executables = {
                    tools / executable: '#!/bin/sh\nprintf "fixture timeout\\n"\n',
                    tools / "brew": '#!/bin/sh\necho "brew must not run, even for prefix queries" >&2\nexit 99\n',
                }
                for path, content in executables.items():
                    path.write_text(content)
                    path.chmod(0o755)
                result = subprocess.run(
                    [str(tools / "bash"), "-e", "-c", source[start:end] + "\ntimeout --version\n"],
                    capture_output=True, text=True,
                    env={**os.environ, "PATH": str(tools), "RUNNER_TEMP": str(root / "runner-temp")},
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("fixture timeout", result.stdout)
                expected = tools / "timeout" if executable == "timeout" else root / "runner-temp/aft-e2e-tools/timeout"
                self.assertIn(str(expected), result.stdout)

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
        publish = self.workflow("release.yml")["jobs"]["publish-crates"]
        self.assertIn('bash scripts/publish-crates.sh "$GITHUB_REF_NAME"', commands(publish))
        run = (ROOT / "scripts/publish-crates.sh").read_text()
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

        # Check every edge, not only each dependency's position before AFT:
        # siblings can all precede AFT while still preceding their own dependencies.
        dry_run = next(
            step["run"] for step in self.workflow("release.yml")["jobs"]["rehearse-publish"]["steps"]
            if step.get("name") == "Rehearse crates.io publishing"
        )
        package_order = re.search(
            r"release_crates=\(([^)]+)\)", (ROOT / "scripts/release.sh").read_text()
        ).group(1).split()
        dry_order = re.findall(r"-p ([\w-]+)", dry_run)
        pending, seen = [ROOT / "crates/aft"], set()
        while pending:
            manifest_dir = pending.pop()
            parent = tomllib.loads((manifest_dir / "Cargo.toml").read_text())["package"]["name"]
            for key, spec, dep_dir in path_dependencies(manifest_dir):
                if dep_dir not in seen:
                    seen.add(dep_dir)
                    pending.append(dep_dir)
                package = tomllib.loads((dep_dir / "Cargo.toml").read_text())["package"]
                name = package["name"]
                with self.subTest(dependency=key, package=name):
                    self.assertNotEqual(package.get("publish"), False, f"{name} must be publishable")
                    self.assertIn("version", spec, f"{key} needs a version pin to publish")
                    publish = f"cargo publish --package {name} "
                    self.assertIn(publish, run, f"publish-crates.sh must publish {name}")
                    self.assertLess(run.index(publish), run.index(f"cargo publish --package {parent} "), f"{name} must publish before {parent}")
                    for label, order in (("release.sh", package_order), ("publish dry-run", dry_order)):
                        self.assertIn(name, order, f"{label} must include {name}")
                        self.assertIn(parent, order, f"{label} must include {parent}")
                        self.assertLess(order.index(name), order.index(parent), f"{label}: {name} must precede {parent}")
                    relative = dep_dir.relative_to(ROOT).parts
                    self.assertIn(
                        ", ".join(f'"{part}"' for part in relative), sync,
                        f"version-sync.mjs must keep {name}'s version in lockstep",
                    )
        self.assertTrue(seen, "agent-file-tools has no path dependencies; the check found nothing")

    def test_version_sync_updates_layer_versions_and_all_path_pins(self):
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        version = "0.65.1-rehearsal.1.1"
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            fixture = Path(temporary)
            (fixture / "scripts").mkdir()
            shutil.copy2(ROOT / "scripts/version-sync.mjs", fixture / "scripts/version-sync.mjs")
            packages = ["aft-bridge", "opencode-plugin", "pi-plugin", "aft-cli"]
            packages += [f"npm/{platform}" for platform in ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64")]
            for package in packages:
                manifest = fixture / "packages" / package / "package.json"
                manifest.parent.mkdir(parents=True)
                manifest.write_text(json.dumps({"version": "0.65.0"}))
            crates = ("aft", "aft-base", "aft-db", "aft-edit", "aft-engine", "aft-github", "aft-index", "aft-inspect", "aft-parse", "aft-store", "aft-subc", "aft-tokenizer", "aft-tools", "secret-rules")
            for crate in crates:
                manifest = fixture / "crates" / crate / "Cargo.toml"
                manifest.parent.mkdir(parents=True)
                shutil.copy2(ROOT / "crates" / crate / "Cargo.toml", manifest)
            sync = ["node", str(fixture / "scripts/version-sync.mjs"), version]
            manifests = [fixture / "crates" / crate / "Cargo.toml" for crate in crates]
            before = [path.read_bytes() for path in manifests]
            engine = tomllib.loads((fixture / "crates/aft-engine/Cargo.toml").read_text())
            current = engine["package"]["version"]
            dry_run = subprocess.run([*sync, "--dry-run"], check=True, capture_output=True, text=True).stdout
            for change in (
                f"crates/aft-edit/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  aft-edit dep version: {current} → {version}",
                f"crates/aft-index/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  aft-index dep version: {current} → {version}",
                f"crates/aft-inspect/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-inspect/Cargo.toml:\n  aft-index dep version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  aft-inspect dep version: {current} → {version}",
                f"crates/aft-github/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-github/Cargo.toml:\n  aft-edit dep version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  aft-github dep version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-engine/Cargo.toml:\n  aft-base dep version: {current} → {version}",
                f"crates/aft-tools/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-tools/Cargo.toml:\n  aft-engine dep version: ={current} → ={version}",
                f"crates/aft-subc/Cargo.toml:\n  version: {current} → {version}",
                f"crates/aft-subc/Cargo.toml:\n  aft-tools dep version: ={current} → ={version}",
                f"crates/aft/Cargo.toml:\n  aft-engine dep version: ={current} → ={version}",
                f"crates/aft/Cargo.toml:\n  aft-subc dep version: ={current} → ={version}",
            ):
                self.assertIn(change, dry_run)
            self.assertNotIn("WARNING:", dry_run)
            self.assertEqual([path.read_bytes() for path in manifests], before)
            subprocess.run(sync, check=True, capture_output=True, text=True)
            changed = {path.relative_to(fixture).as_posix() for path, original in zip(manifests, before)
                       if path.read_bytes() != original}
            self.assertEqual(changed - VERSION_FILES, set(), "retry guard rejects version-sync changes")
            for crate in crates:
                manifest = tomllib.loads((fixture / "crates" / crate / "Cargo.toml").read_text())
                self.assertEqual(manifest["package"]["version"], version, crate)
                for table in ("dependencies", "dev-dependencies", "build-dependencies"):
                    for name, spec in manifest.get(table, {}).items():
                        if isinstance(spec, dict) and "path" in spec:
                            exact = name in {"aft-engine", "aft-tools", "aft-subc"}
                            expected = f"={version}" if exact else version
                            self.assertEqual(spec["version"], expected, f"{crate}: {table}.{name}")


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
                ) or "bash scripts/publish-crates.sh" in run
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
            "cargo publish --dry-run --locked --allow-dirty " + " ".join(f"-p {name}" for name in RELEASE_CRATES),
        )
        self.assertNotIn("--no-verify", publish)
        materialize = next(step["run"] for step in steps if step.get("name") == "Materialize crate archives for inspection")
        self.assertEqual(
            materialize,
            "cargo package --locked --allow-dirty --no-verify " + " ".join(f"-p {name}" for name in RELEASE_CRATES),
        )
        names = [step.get("name") for step in steps]
        self.assertLess(names.index("Rehearse crates.io publishing"), names.index("Materialize crate archives for inspection"))
        self.assertLess(names.index("Materialize crate archives for inspection"), names.index("Check packaged crate sizes"))

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
            packages = [{"name": name, "version": REHEARSAL_VERSION} for name in RELEASE_CRATES]
            files = [directory / f"{package['name']}-{package['version']}.crate" for package in packages]
            for file in files:
                write_rehearsal_crate(file)
            metadata = json.dumps({"target_directory": str(target), "packages": packages})
            with mock.patch("subprocess.check_output", return_value=metadata):
                exec(script, {})
                upper = directory / f"agent-file-tools-{REHEARSAL_VERSION}.crate"
                write_rehearsal_crate(upper, pin=REHEARSAL_VERSION)
                with self.assertRaisesRegex(SystemExit, "must require aft-engine =" + re.escape(REHEARSAL_VERSION)):
                    exec(script, {})
                write_rehearsal_crate(upper)
                for name in ("agent-file-tools", "aft-parse", "aft-edit", "aft-index", "aft-inspect", "aft-github", "aft-engine", "aft-tools", "aft-subc"):
                    archive = next(file for file in files if file.name.startswith(name + "-"))
                    for size in (10 * 1024 * 1024, 10 * 1024 * 1024 + 1):
                        with self.subTest(package=name, size=size):
                            with archive.open("wb") as contents:
                                contents.truncate(size)
                            with self.assertRaisesRegex(SystemExit, "must be under 10485760 bytes"):
                                exec(script, {})
                    archive.unlink()
                    with self.assertRaisesRegex(AssertionError, name):
                        exec(script, {})
                    write_rehearsal_crate(archive)

    def test_crate_size_check_finds_multi_package_archives_and_checks_every_copy(self):
        script = inline_python(self.rehearsal, "Check packaged crate sizes")
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            target = Path(temporary)
            packages = [{"name": name, "version": REHEARSAL_VERSION} for name in RELEASE_CRATES]
            metadata = json.dumps({"target_directory": str(target), "packages": packages})
            with mock.patch("subprocess.check_output", return_value=metadata):
                copies = []
                for folder in ("tmp-crate", "tmp-registry"):
                    directory = target / "package" / folder
                    directory.mkdir(parents=True)
                    for package in packages:
                        archive = directory / f"{package['name']}-{package['version']}.crate"
                        write_rehearsal_crate(archive)
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
                with self.assertRaisesRegex(AssertionError, "missing packaged crates:.*aft-secret-rules.*aft-store.*aft-tokenizer.*agent-file-tools"):
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
