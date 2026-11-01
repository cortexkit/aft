import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import build
import publish


class BuildTests(unittest.TestCase):
    def setUp(self):
        scratch = build.ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)

    def test_variant_appends_exactly_one_custom_section(self):
        original = b"\0asm\x01\0\0\0"
        # The section has id 0, a 22-byte payload, a 20-byte name, then 'B'.
        expected = original + b"\x00\x16\x14aft.fixture.identityB"
        self.assertEqual(build.variant(original), expected)
        self.assertNotEqual(build.digest(original), build.digest(expected))

    def test_uleb_boundary(self):
        self.assertEqual(build.uleb(0), b"\0")
        self.assertEqual(build.uleb(127), b"\x7f")
        self.assertEqual(build.uleb(128), b"\x80\x01")

    def test_cached_download_rechecks_hash(self):
        path = self.work / "cached"
        path.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "differs"):
            build.fetch("https://example.invalid/artifact", build.digest(b"good"), path)

    def test_download_rejects_http(self):
        with self.assertRaisesRegex(ValueError, "HTTPS"):
            build.fetch("http://example.invalid/artifact", "0" * 64, self.work / "artifact")

    def test_download_identifies_client_and_avoids_cached_expired_redirects(self):
        response = io.BytesIO(b"verified")
        response.url = "https://example.invalid/archive"
        with patch.object(build.urllib.request, "urlopen", return_value=response) as urlopen:
            path = build.fetch(response.url, build.digest(b"verified"), self.work / "archive")
        self.assertEqual(path.read_bytes(), b"verified")
        request = urlopen.call_args.args[0]
        self.assertEqual(request.get_header("User-agent"), "AFT-grammar-build/1")
        self.assertEqual(request.get_header("Cache-control"), "no-cache")

    def test_archive_refuses_symlink_instead_of_following_it(self):
        archive = self.work / "extension.tar"
        with tarfile.open(archive, "w") as tar:
            member = tarfile.TarInfo("./grammars/luau.wasm")
            member.type = tarfile.SYMTYPE
            member.linkname = "../../extension.wasm"
            tar.addfile(member)
        with self.assertRaisesRegex(ValueError, "regular member"):
            build.archive_member(archive, "./grammars/luau.wasm")

    def test_archive_refuses_duplicate_members(self):
        archive = self.work / "extension.tar"
        with tarfile.open(archive, "w") as tar:
            for _ in range(2):
                member = tarfile.TarInfo("./grammars/luau.wasm")
                member.size = 1
                tar.addfile(member, io.BytesIO(b"x"))
        with self.assertRaisesRegex(ValueError, "one size-capped"):
            build.archive_member(archive, "./grammars/luau.wasm")

    def test_parsing_uses_isolated_wasm_cache_with_compilers_disabled(self):
        module = self.work / "module.wasm"
        module.write_bytes(b"exact module bytes")
        with patch.object(build, "run", return_value='<sources><source name="input.luau"><chunk/></source></sources>') as run:
            build.parse("cli", module, "input.luau", cwd=self.work, env={})
        self.assertEqual(run.call_args.args[0], ["cli", "parse", "--wasm", "--xml", "input.luau"])
        self.assertEqual((self.work / "cache/luau.wasm").read_bytes(), b"exact module bytes")
        self.assertEqual(run.call_args.kwargs["env"]["PATH"], "")
        self.assertEqual(run.call_args.kwargs["env"]["WASI_SDK_PATH"], str(self.work / "no-compiler"))

    def test_verifier_refuses_a_replaced_cached_module(self):
        module = self.work / "module.wasm"
        module.write_bytes(b"exact module bytes")

        def replace_cache(*args, **kwargs):
            (self.work / "cache/luau.wasm").write_bytes(b"other bytes")
            return "<chunk/>\n"

        with patch.object(build, "run", side_effect=replace_cache):
            with self.assertRaisesRegex(ValueError, "loaded WASM bytes"):
                build.parse("cli", module, "source", cwd=self.work, env={})

    def test_empty_or_error_trees_are_not_parity(self):
        module = self.work / "module.wasm"
        module.write_bytes(b"exact module bytes")
        for tree in ("", "<chunk><ERROR/></chunk>", "<chunk><MISSING/></chunk>"):
            with self.subTest(tree=tree), patch.object(build, "run", return_value=tree):
                with self.assertRaises(ValueError):
                    build.parse("cli", module, "source", cwd=self.work, env={})

    def test_tree_oracle_normalizes_only_checkout_path(self):
        module = self.work / "module.wasm"
        module.write_bytes(b"exact module bytes")
        tree = '<sources><source name="/checkout/input.luau"><chunk srow="0"><identifier>x</identifier></chunk></source></sources>'
        with patch.object(build, "run", return_value=tree):
            actual = build.parse("cli", module, "/checkout/input.luau", cwd=self.work, env={})
        self.assertEqual(actual, '<sources><source name="input.luau"><chunk srow="0"><identifier>x</identifier></chunk></source></sources>\n')

    def test_different_trees_fail(self):
        with self.assertRaisesRegex(ValueError, "differs"):
            build.require_equal("<chunk><function/></chunk>", "<chunk/>", "parse tree")

    def test_pin_uses_only_reviewed_c_inputs(self):
        pin = json.loads(build.PIN.read_text())
        self.assertEqual(pin["build"]["sources"], ["src/parser.c", "src/scanner.c"])
        self.assertEqual(pin["extensions"], ["luau"])
        self.assertRegex(pin["grammar"]["commit"], r"^[0-9a-f]{40}$")
        self.assertTrue(pin["zed"]["exemption"])


class PublicationTests(unittest.TestCase):
    def setUp(self):
        scratch = build.ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        self.data = b"verified grammar bytes"
        sha = build.digest(self.data)
        self.artifact = self.work / f"{sha}.wasm"
        self.artifact.write_bytes(self.data)
        self.entry = {"sha256": sha, "size": len(self.data)}
        self.remote = None
        self.release_exists = True
        self.calls = []

    def gh(self, args):
        self.calls.append(args)
        operation = args[1]
        if operation == "view":
            if not self.release_exists:
                return subprocess.CompletedProcess(args, 1, "", "release not found")
            assets = [] if self.remote is None else [{"name": self.artifact.name}]
            return subprocess.CompletedProcess(args, 0, json.dumps({"assets": assets}), "")
        if operation == "create":
            self.release_exists = True
        if operation == "download":
            directory = Path(args[args.index("--dir") + 1])
            (directory / self.artifact.name).write_bytes(self.remote)
        if operation == "upload":
            self.assertNotIn("--clobber", args)
            self.remote = self.data
        return subprocess.CompletedProcess(args, 0, "", "")

    def invoke(self):
        with patch.object(publish, "gh", side_effect=self.gh):
            return publish.publish(self.artifact, self.entry, "cortexkit/aft", "grammars-1", self.work)

    def test_second_publish_verifies_existing_bytes_and_uploads_nothing(self):
        self.assertEqual(self.invoke(), "uploaded")
        self.calls.clear()
        self.assertEqual(self.invoke(), "verified_existing")
        self.assertEqual([call[1] for call in self.calls], ["view", "download"])

    def test_new_grammar_release_is_prerelease_and_never_latest(self):
        self.release_exists = False
        self.assertEqual(self.invoke(), "uploaded")
        self.assertEqual([call[1] for call in self.calls], ["view", "create", "upload"])
        self.assertEqual(self.calls[1], [
            "release", "create", "grammars-1", "--repo", "cortexkit/aft", "--target", "main",
            "--latest=false", "--prerelease", "--title", "Grammar artifacts", "--notes",
            "Immutable content-addressed grammar assets.",
        ])

    def test_existing_different_bytes_fail_without_replacement(self):
        self.remote = b"different bytes"
        with self.assertRaisesRegex(ValueError, "existing release asset bytes"):
            self.invoke()
        self.assertEqual([call[1] for call in self.calls], ["view", "download"])

    def test_wrong_hash_fails_before_contacting_github(self):
        self.artifact.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "publication hash"):
            self.invoke()
        self.assertEqual(self.calls, [])

    def test_network_failure_does_not_create_release(self):
        with patch.object(publish, "gh", return_value=subprocess.CompletedProcess([], 1, "", "network error")) as gh:
            with self.assertRaisesRegex(RuntimeError, "network error"):
                publish.publish(self.artifact, self.entry, "cortexkit/aft", "grammars-1", self.work)
        self.assertEqual(gh.call_count, 1)


if __name__ == "__main__":
    unittest.main()
