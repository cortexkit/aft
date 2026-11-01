#!/usr/bin/env python3
"""Build only pinned generated C; verify repeatability and Zed parse parity."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tomllib
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "crates/aft/tests/fixtures/grammars"
PIN = ROOT / "grammars/luau.pin.json"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def require_equal(actual, expected, description):
    if actual != expected:
        raise ValueError(f"{description} differs; investigate, do not update pins automatically")


def fetch(url, sha256, destination, limit=512 * 1024 * 1024):
    if not url.startswith("https://"):
        raise ValueError("downloads require HTTPS")
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        require_equal(digest(destination.read_bytes()), sha256, str(destination))
        return destination
    temporary = destination.with_suffix(destination.suffix + ".partial")
    try:
        request = urllib.request.Request(url, headers={"User-Agent": "AFT-grammar-build/1",
                                                      "Cache-Control": "no-cache"})
        with urllib.request.urlopen(request, timeout=60) as response, temporary.open("wb") as output:
            if not response.url.startswith("https://"):
                raise ValueError("download redirected away from HTTPS")
            size = 0
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > limit:
                    raise ValueError(f"download exceeds cap: {url}")
                output.write(chunk)
        require_equal(digest(temporary.read_bytes()), sha256, url)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    return destination


def archive_member(archive, name, limit=8 * 1024 * 1024):
    # Only data members are read from extension archives, never extension.wasm.
    with tarfile.open(archive) as tar:
        members = [m for m in tar.getmembers() if m.name == name]
        if len(members) != 1 or not members[0].isfile() or members[0].size > limit:
            raise ValueError(f"expected one size-capped regular member: {name}")
        return tar.extractfile(members[0]).read()


def uleb(value):
    result = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        result.append(byte | (128 if value else 0))
        if not value:
            return bytes(result)


def variant(module):
    name = b"aft.fixture.identity"
    section = uleb(len(name)) + name + b"B"
    return module + b"\0" + uleb(len(section)) + section


def clean_environment(work):
    home = work / "home"
    home.mkdir(exist_ok=True)
    return {"PATH": "/usr/bin:/bin", "HOME": str(home), "XDG_CONFIG_HOME": str(home),
            "XDG_CACHE_HOME": str(home), "LC_ALL": "C", "NO_COLOR": "1",
            "SOURCE_DATE_EPOCH": "0"}


def run(args, *, cwd, env):
    return subprocess.check_output([str(a) for a in args], cwd=cwd, env=env, text=True)


def parse(cli, module, source, *, cwd, env):
    # --lib-path bypasses grammar discovery/building. The empty working directory
    # and isolated config also prevent an installed developer grammar being used.
    tree = run([cli, "parse", "--wasm", "--lib-path", module, "--lang-name", "luau",
                "--xml", source], cwd=cwd, env=env)
    if not tree.strip() or "<ERROR" in tree or "<MISSING" in tree:
        raise ValueError(f"empty or invalid parse: {source}")
    return tree


def manifest(pin, module, role):
    sha = digest(module)
    return {"schema_version": 1, "language": pin["language"], "extensions": pin["extensions"],
            "grammar_repository": pin["grammar"]["repository"],
            "grammar_commit": pin["grammar"]["commit"], "abi": pin["grammar"]["abi"],
            "license": pin["grammar"]["license"], "sha256": sha, "size": len(module),
            "file": f"{sha}.wasm", "role": role,
            "url": f"https://github.com/{pin['publication']['repository']}/releases/download/"
                   f"{pin['publication']['tag']}/{sha}.wasm",
            "test_only": True}


def json_write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def build(output, write_fixtures=False):
    pin = json.loads(PIN.read_text())
    grammar = pin["grammar"]
    if not re.fullmatch(r"[0-9a-f]{40}", grammar["commit"]):
        raise ValueError("grammar revision must be a full commit")
    work = output / "work"
    work.mkdir(parents=True, exist_ok=True)
    env = clean_environment(work)
    downloads = work / "downloads"
    source = work / "source"
    repo = grammar["repository"].removeprefix("https://github.com/")
    for name, sha in grammar["files"].items():
        fetch(f"https://raw.githubusercontent.com/{repo}/{grammar['commit']}/{name}",
              sha, source / name, limit=8 * 1024 * 1024)
    parser = (source / "src/parser.c").read_text()
    abi = int(re.search(r"#define LANGUAGE_VERSION (\d+)", parser)[1])
    require_equal(abi, grammar["abi"], "grammar ABI")
    require_equal(pin["build"]["sources"], ["src/parser.c", "src/scanner.c"], "C input allowlist")
    sdk_pin = pin["wasi_sdk"]
    sdk_archive = fetch(sdk_pin["url"], sdk_pin["sha256"], downloads / "wasi-sdk.tar.gz")
    sdk_dir = work / "sdk"
    if sdk_dir.exists():
        shutil.rmtree(sdk_dir)
    sdk_dir.mkdir()
    with tarfile.open(sdk_archive) as tar:
        # Python's data filter rejects escaping links, paths and special files.
        tar.extractall(sdk_dir, filter="data")
    clang = sdk_dir / sdk_pin["root"] / "bin/clang"
    compiler_version = run([clang, "--version"], cwd=work, env=env).strip()
    modules = []
    for name in ("build-a", "build-b"):
        directory = work / name
        if directory.exists():
            shutil.rmtree(directory)
        shutil.copytree(source, directory)
        run([clang, *pin["build"]["flags"], "-o", "luau.wasm", "-I", pin["build"]["include"],
             *pin["build"]["sources"]], cwd=directory, env=env)
        modules.append((directory / "luau.wasm").read_bytes())
    require_equal(modules[0], modules[1], "independent C builds")
    a = modules[0]
    b = variant(a)
    artifacts = output / "artifacts"
    if artifacts.exists():
        shutil.rmtree(artifacts)
    artifacts.mkdir()
    manifests = [manifest(pin, a, "A"), manifest(pin, b, "B")]
    for item, data in zip(manifests, (a, b)):
        (artifacts / item["file"]).write_bytes(data)
        json_write(artifacts / f"manifest-{item['role'].lower()}.json", item)

    zed = pin["zed"]
    archive = fetch(zed["url"], zed["sha256"], downloads / "zed.tar.gz", limit=16 * 1024 * 1024)
    zed_metadata = tomllib.loads(archive_member(archive, "./extension.toml").decode())
    require_equal(zed_metadata["grammars"]["luau"]["repository"], grammar["repository"], "Zed source repository")
    require_equal(zed_metadata["grammars"]["luau"]["rev"], grammar["commit"], "Zed source revision")
    zed_module = archive_member(archive, zed["grammar_member"])
    require_equal(digest(zed_module), zed["grammar_sha256"], "Zed grammar hash")
    zed_path = work / "zed-luau.wasm"
    zed_path.write_bytes(zed_module)
    if zed["comparison"] == "bytes":
        require_equal(zed["wasi_sdk"], pin["wasi_sdk"], "Zed compiler pin")
        require_equal(zed["build"], pin["build"], "Zed flags")
        require_equal(a, zed_module, "Zed module bytes")
    elif zed["comparison"] != "parse_trees" or not zed.get("exemption"):
        raise ValueError("Zed comparison requires matching toolchain or reviewed exemption")
    verifier = pin["verifier"]
    cli_archive = fetch(verifier["url"], verifier["sha256"], downloads / "tree-sitter.gz")
    cli = work / "tree-sitter"
    cli.write_bytes(gzip.decompress(cli_archive.read_bytes()))
    cli.chmod(0o755)
    verifier_version = run([cli, "--version"], cwd=work, env=env).strip()
    empty = work / "parse"
    empty.mkdir(exist_ok=True)
    corpus = json.loads((ROOT / "grammars/corpus/manifest.json").read_text())
    if not corpus["files"]:
        raise ValueError("empty parse corpus")
    for entry in corpus["files"]:
        path = ROOT / "grammars/corpus" / entry["file"]
        require_equal(digest(path.read_bytes()), entry["sha256"], "corpus bytes")
        trees = [parse(cli, module, path, cwd=empty, env=env) for module in
                 (artifacts / manifests[0]["file"], artifacts / manifests[1]["file"], zed_path)]
        require_equal(trees[0], trees[1], f"A/B parse tree: {entry['file']}")
        require_equal(trees[0], trees[2], f"A/Zed parse tree: {entry['file']}")
        oracle = ROOT / "grammars/corpus" / entry["tree"]
        if write_fixtures:
            oracle.write_text(trees[0])
        else:
            require_equal(trees[0], oracle.read_text(), f"checked-in tree: {entry['file']}")
    if write_fixtures:
        FIXTURES.mkdir(parents=True, exist_ok=True)
        for path in artifacts.iterdir():
            shutil.copyfile(path, FIXTURES / path.name)
        shutil.copyfile(source / "LICENSE", FIXTURES / "LICENSE.luau")
    else:
        for path in artifacts.iterdir():
            require_equal(path.read_bytes(), (FIXTURES / path.name).read_bytes(), f"fixture {path.name}")
    report = {"pin_sha256": digest(PIN.read_bytes()), "compiler": compiler_version,
              "verifier": verifier_version, "builds_equal": True,
              "zed_comparison": zed["comparison"], "corpus_files": len(corpus["files"]),
              "parse_trees_equal": True, "artifacts": manifests,
              "provenance_notes": pin["provenance_notes"]}
    json_write(artifacts / "verification.json", report)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/grammar-build")
    parser.add_argument("--write-fixtures", action="store_true", help="explicit reviewed fixture update; never used in CI")
    options = parser.parse_args()
    build(options.output.resolve(), options.write_fixtures)
