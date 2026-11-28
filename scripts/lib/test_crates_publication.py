#!/usr/bin/env python3
"""Keep crates.io failures isolated and retries bound to a shipped tag."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("workflow_gates", Path(__file__).with_name("workflow-gates.py"))
assert SPEC is not None and SPEC.loader is not None
parser = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parser)
SPLIT_CRATES = ("aft-engine", "aft-tools", "aft-subc")
PACKAGES = ["aft-tokenizer", "aft-secret-rules", "aft-store", "aft-base", "aft-db", "aft-parse", *SPLIT_CRATES, "agent-file-tools"]


class CratesPublicationTests(unittest.TestCase):
    def workflow(self, name):
        return parser.parse_workflow(str(ROOT / ".github/workflows" / name))

    def test_github_and_npm_keep_all_gates_without_waiting_on_crates(self):
        jobs = self.workflow("release.yml")["jobs"]
        prerequisites = {"gate", "build-js-packages"} | {
            f"build-{platform}" for platform in
            ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-arm64", "win32-x64")
        } | {"e2e-darwin-arm64", "e2e-win32-x64", "e2e-linux-x64"}
        for name in ("publish-crates", "github-release", "publish-npm"):
            with self.subTest(job=name):
                expected = prerequisites | ({"github-release"} if name == "publish-npm" else set())
                self.assertEqual(set(jobs[name]["needs"]), expected)
                self.assertEqual(jobs[name]["if"], "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')")
                self.assertNotIn("continue-on-error", jobs[name])

    def test_retry_has_only_crates_and_uses_the_requested_tag_source(self):
        workflow = self.workflow("crates-retry.yml")
        self.assertEqual(set(workflow["on"]), {"workflow_dispatch"})
        tag = workflow["on"]["workflow_dispatch"]["inputs"]["tag"]
        self.assertEqual(tag["required"], "true")
        self.assertEqual(tag["type"], "string")
        self.assertEqual(set(workflow["jobs"]), {"publish-crates"})
        job = workflow["jobs"]["publish-crates"]
        self.assertNotIn("needs", job)
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(job["env"]["RETRY_TAG"], "${{ inputs.tag }}")
        steps = job["steps"]
        checkouts = [step for step in steps if step.get("uses") == "actions/checkout@v5"]
        self.assertEqual(checkouts[0]["with"], {"path": "automation"})
        self.assertEqual(checkouts[1]["with"], {"ref": "refs/tags/${{ inputs.tag }}", "path": "release-source"})
        guard = next(step for step in steps if step.get("name") == "Require an existing published GitHub release")
        self.assertLess(steps.index(guard), steps.index(checkouts[1]))
        publish = steps[-1]
        self.assertEqual(publish["working-directory"], "release-source")
        self.assertEqual(publish["run"], 'bash ../automation/scripts/publish-crates.sh "$RETRY_TAG"')
        self.assertEqual(publish["env"]["CARGO_REGISTRY_TOKEN"], "${{ secrets.CARGO_REGISTRY_TOKEN }}")
        for action in ("oven-sh/setup-bun@v2", "dtolnay/rust-toolchain@stable"):
            self.assertTrue(any(step.get("uses") == action for step in steps))

    def fixture(self):
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        return tempfile.TemporaryDirectory(prefix="crates-publication-", dir=scratch)

    def stub(self, root, name, source):
        path = root / "bin" / name
        path.parent.mkdir(exist_ok=True)
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o755)

    def run_publish(self, *, already=(), fail="", mismatch=False, tag="v1.2.3",
                    split_crates=SPLIT_CRATES, missing=(), extra_manifests=()):
        with self.fixture() as directory:
            root = Path(directory)
            manifests = dict(extra_manifests)
            upper = '[package]\nname = "agent-file-tools"\nversion = "0.0.0"\n[dependencies]\n'
            for crate in split_crates:
                upper += f'{crate} = {{ path = "../{crate}", version = "=0.0.0" }}\n'
                if crate not in missing:
                    manifests[f"crates/{crate}/Cargo.toml"] = f'[package]\nname = "{crate}"\nversion = "0.0.0"\n'
            manifests["crates/aft/Cargo.toml"] = upper
            for name, contents in manifests.items():
                manifest = root / name
                manifest.parent.mkdir(parents=True, exist_ok=True)
                manifest.write_text(contents)
            subprocess.run(["git", "init", "-q"], cwd=root, check=True)
            subprocess.run(["git", "add", "--", *manifests], cwd=root, check=True)
            self.stub(root, "node", '''import os, pathlib, sys
assert sys.argv[1:] == ["scripts/version-sync.mjs", "--from-tag"]
with pathlib.Path("events").open("a") as log:
    log.write('["version-sync"]\\n')
version = "9.9.9" if os.environ["MISMATCH"] else os.environ["GITHUB_REF_NAME"][1:]
manifest = pathlib.Path("crates/aft/Cargo.toml")
manifest.write_text(manifest.read_text().replace('version = "0.0.0"', f'version = "{version}"', 1))
''')
            self.stub(root, "cargo", '''import json, os, pathlib, sys
args = sys.argv[1:]
with pathlib.Path("events").open("a") as log:
    log.write(json.dumps(args) + "\\n")
package = args[args.index("--package") + 1]
if package in json.loads(os.environ["ALREADY"]):
    print("already exists")
    sys.exit(1)
if package == os.environ["FAIL"]:
    print("crates.io refused the package", file=sys.stderr)
    sys.exit(23)
''')
            self.stub(root, "sleep", '''import pathlib, sys
assert sys.argv[1:] == ["30"]
with pathlib.Path("events").open("a") as log:
    log.write('["sleep", "30"]\\n')
''')
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/publish-crates.sh"), tag], cwd=root,
                env={**os.environ, "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                     "ALREADY": json.dumps(already), "FAIL": fail, "MISMATCH": "1" if mismatch else ""},
                capture_output=True, text=True,
            )
            events = root / "events"
            return result, [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []

    def publish_trace(self, packages, already=()):
        events = [["version-sync"]]
        for name in packages:
            if name not in ("aft-tokenizer", "aft-secret-rules", "aft-store"):
                events.append(["sleep", "30"])
            events.append(["publish", "--package", name])
            if name in already:
                events.append(["publish", "--package", name, "--dry-run"])
        return events

    def test_partial_publish_skips_existing_crates_and_continues_in_order(self):
        already = (*PACKAGES[:2], "aft-engine", "aft-tools")
        result, events = self.run_publish(already=already)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events, self.publish_trace(PACKAGES, already=already))

    def test_all_existing_crates_are_a_successful_retry(self):
        result, events = self.run_publish(already=PACKAGES)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events, self.publish_trace(PACKAGES, already=PACKAGES))

    def test_pre_engine_tag_retries_without_engine(self):
        result, events = self.run_publish(tag="v0.67.0", split_crates=())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events, self.publish_trace([name for name in PACKAGES if name not in SPLIT_CRATES]))

    def test_engine_only_tag_retries_without_tools_or_daemon(self):
        result, events = self.run_publish(tag="v0.68.0", split_crates=("aft-engine",))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(events, self.publish_trace([name for name in PACKAGES if name not in ("aft-tools", "aft-subc")]))

    def test_engine_tag_without_its_engine_manifest_cannot_publish(self):
        result, events = self.run_publish(tag="v0.68.0", split_crates=("aft-engine",), missing=("aft-engine",))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires aft-engine but is missing crates/aft-engine/Cargo.toml", result.stderr)
        self.assertEqual(events, [])

    def test_split_tag_missing_either_new_manifest_cannot_publish(self):
        for crate in ("aft-tools", "aft-subc"):
            with self.subTest(crate=crate):
                result, events = self.run_publish(missing=(crate,))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"requires {crate} but is missing crates/{crate}/Cargo.toml", result.stderr)
                self.assertEqual(events, [])

    def test_missing_crate_declared_outside_facade_cannot_publish(self):
        declarations = (
            ("aft-tools", "Cargo.toml", '[workspace]\nmembers = ["crates/aft-tools"]\n'),
            ("aft-subc", "crates/aft-engine/Cargo.toml", '''
[target.'cfg(unix)'.dev-dependencies.daemon]
package = "aft-subc"
version = "1.2.3"
'''),
        )
        for crate, filename, contents in declarations:
            with self.subTest(crate=crate, filename=filename):
                result, events = self.run_publish(split_crates=(), extra_manifests=((filename, contents),))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"requires {crate} but is missing crates/{crate}/Cargo.toml", result.stderr)
                self.assertIn(f"declared in {filename}", result.stderr)
                self.assertEqual(events, [])

    def test_real_crates_refusal_stays_red_and_stops_publication(self):
        result, events = self.run_publish(fail="aft-secret-rules")
        self.assertEqual(result.returncode, 23)
        self.assertIn("crates.io refused", result.stderr)
        self.assertEqual(events, self.publish_trace(PACKAGES[:2], already=("aft-secret-rules",)))

    def test_version_mismatch_or_non_tag_input_cannot_publish(self):
        for mismatch, tag in ((True, "v1.2.3"), (False, "main")):
            with self.subTest(mismatch=mismatch, tag=tag):
                result, events = self.run_publish(mismatch=mismatch, tag=tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(events, [["version-sync"]] if mismatch else [])

    def test_retry_refuses_absent_draft_or_invalid_release_before_publishing(self):
        job = self.workflow("crates-retry.yml")["jobs"]["publish-crates"]
        guard = next(step["run"] for step in job["steps"] if step.get("name") == "Require an existing published GitHub release")
        with self.fixture() as directory:
            root = Path(directory)
            self.stub(root, "gh", '''import os, sys
assert sys.argv[1:] == ["release", "view", "v1.2.3", "--json", "isDraft", "--jq", ".isDraft"]
print(os.environ["DRAFT"])
sys.exit(int(os.environ["GH_EXIT"]))
''')
            for tag, draft, code, success in (("v1.2.3", "false", "0", True),
                                             ("v1.2.3", "true", "0", False),
                                             ("v1.2.3", "", "1", False),
                                             ("main", "false", "0", False)):
                with self.subTest(tag=tag, draft=draft, code=code):
                    result = subprocess.run(
                        ["bash", "-e", "-c", guard], cwd=root, capture_output=True, text=True,
                        env={**os.environ, "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                             "RETRY_TAG": tag, "DRAFT": draft, "GH_EXIT": code},
                    )
                    self.assertEqual(result.returncode == 0, success, result.stderr)

    def run_waiter(self, views):
        with self.fixture() as directory:
            root = Path(directory)
            self.stub(root, "gh", '''import json, os, pathlib, sys
args = sys.argv[1:]
if args[:2] == ["auth", "status"]:
    sys.exit(0)
if args[:2] == ["run", "cancel"]:
    pathlib.Path("cancelled").touch()
    sys.exit(0)
assert args[:2] == ["run", "view"], args
counter = pathlib.Path("views")
index = int(counter.read_text()) if counter.exists() else 0
views = json.loads(os.environ["VIEWS"])
print(json.dumps(views[min(index, len(views) - 1)]))
counter.write_text(str(index + 1))
''')
            self.stub(root, "sleep", "pass\n")
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/wait-release.sh"), "v1.2.3", "--run-id", "123"],
                cwd=root, capture_output=True, text=True, timeout=10,
                env={**os.environ, "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                     "VIEWS": json.dumps(views), "MAX_WAIT_SECONDS": "5", "OPERATOR_GH_FALLBACK_PATHS": ""},
            )
            return result, (root / "cancelled").exists(), int((root / "views").read_text())

    def release_view(self, *, completed=True, crates="failure", github: str | None = "success",
                     npm: str | None = "success", build="success"):
        def job(name, conclusion):
            return {"name": name, "status": "completed" if conclusion else "in_progress", "conclusion": conclusion}
        failed = any(value != "success" for value in (crates, github, npm, build))
        return {"status": "completed" if completed else "in_progress",
                "conclusion": ("failure" if failed else "success") if completed else None,
                "jobs": [job("Build Linux x64", build), job("Publish to crates.io", crates),
                         job("Create GitHub Release", github), job("Publish to npm", npm)]}

    def test_waiter_allows_github_and_npm_to_finish_after_crates_failure(self):
        for conclusion in ("failure", "timed_out", "cancelled", "action_required"):
            with self.subTest(conclusion=conclusion):
                result, cancelled, views = self.run_waiter([
                    self.release_view(completed=False, crates=conclusion, github=None, npm=None),
                    self.release_view(crates=conclusion),
                ])
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(cancelled)
                self.assertEqual(views, 2)
                self.assertIn("crates.io publication failed; GitHub release and npm published successfully", result.stdout)
                self.assertIn("gh workflow run crates-retry.yml --ref main -f tag=v1.2.3", result.stdout)

    def test_waiter_still_cancels_other_failures_even_if_crates_failed_first(self):
        result, cancelled, _ = self.run_waiter([
            self.release_view(completed=False, build="failure", github=None, npm=None),
            self.release_view(build="failure", github="cancelled", npm="cancelled"),
        ])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertTrue(cancelled)
        self.assertIn("Job failed: Build Linux x64", result.stdout)
        self.assertNotIn("published successfully", result.stdout)

    def test_waiter_does_not_claim_partial_success_without_both_publications(self):
        for github, npm in (("failure", "skipped"), ("success", "failure"), ("skipped", "skipped")):
            with self.subTest(github=github, npm=npm):
                result, cancelled, _ = self.run_waiter([self.release_view(github=github, npm=npm)])
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(cancelled)
                self.assertIn("Release workflow failed", result.stdout)
                self.assertIn("Publish to crates.io", result.stdout)
                self.assertNotIn("published successfully", result.stdout)

    def test_waiter_still_accepts_a_successful_release(self):
        result, cancelled, _ = self.run_waiter([self.release_view(crates="success")])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(cancelled)
        self.assertIn("Release workflow succeeded", result.stdout)


if __name__ == "__main__":
    unittest.main()
