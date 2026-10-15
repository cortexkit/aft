#!/usr/bin/env python3
"""Keep landing CI fast and publication bound to shipped-profile E2E artifacts."""

import importlib.util
from pathlib import Path
import unittest


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


def commands(job):
    return "\n".join(step.get("run", "") for step in job["steps"])


class NativeReleaseWorkflowTests(unittest.TestCase):
    def workflow(self, name):
        return parser.parse_workflow(str(ROOT / ".github/workflows" / name))

    def test_main_and_train_native_e2e_always_use_stage(self):
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

    def test_every_publish_job_requires_shipped_e2e_on_release_sha(self):
        jobs = self.workflow("release.yml")["jobs"]
        required = {f"e2e-{platform}" for platform in COVERED}
        for name in ("publish-crates", "publish-npm", "github-release"):
            with self.subTest(job=name):
                self.assertTrue(required.issubset(jobs[name]["needs"]))
                self.assertIn("gate", jobs[name]["needs"])
                self.assertEqual(jobs[name]["if"], "github.event_name != 'workflow_dispatch'")
        for platform, (target, harness, binary) in COVERED.items():
            with self.subTest(platform=platform):
                build = jobs[f"build-{platform}"]
                self.assertGreaterEqual(int(build["timeout-minutes"]), 120)
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


if __name__ == "__main__":
    unittest.main()
