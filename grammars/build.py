#!/usr/bin/env python3
"""Rebuild the pinned grammar manifest from verified generated C sources."""

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tomllib
import urllib.request
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "crates/aft/tests/fixtures/grammars"
MANIFEST = ROOT / "crates/aft-parse/src/grammar/manifest.json"
GRAMMAR_RELEASE = {"repository": "cortexkit/aft", "tag": "grammars-1"}


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


def parse(cli, module, source, *, cwd, env, language, root_kind=None):
    # The CLI's --lib-path loads native libraries, even with --wasm. Populate
    # only its isolated WASM cache. The configured grammar name selects the
    # tree_sitter_<language> export; compiler discovery is disabled.
    cache = cwd / "cache"
    cache.mkdir(exist_ok=True)
    cached = cache / f"{language}.wasm"
    data = module.read_bytes()
    cached.write_bytes(data)
    parse_env = dict(env, TREE_SITTER_LIBDIR=str(cache), PATH="",
                     WASI_SDK_PATH=str(cwd / "no-compiler"))
    tree = run([cli, "parse", "--wasm", "--xml", source], cwd=cwd, env=parse_env)
    require_equal(cached.read_bytes(), data, "loaded WASM bytes")
    if not tree.strip() or "<ERROR" in tree or "<MISSING" in tree:
        raise ValueError(f"empty or invalid parse: {source}")
    document = ET.fromstring(tree)
    entries = document.findall("source")
    if document.tag != "sources" or len(entries) != 1 or len(entries[0]) != 1:
        raise ValueError(f"expected one {language} source tree")
    if root_kind is not None:
        require_equal(entries[0][0].tag, root_kind, f"{language} root node")
    require_equal(entries[0].get("name"), str(source), "parsed source path")
    # The CLI embeds the checkout's absolute path, which is not part of the AST.
    entries[0].set("name", Path(source).name)
    return ET.tostring(document, encoding="unicode") + "\n"


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


def manifest_entries():
    entries = json.loads(MANIFEST.read_text())["grammars"]
    if not entries or len({entry["language"] for entry in entries}) != len(entries):
        raise ValueError("expected a nonempty manifest with unique languages")
    return entries


def pin_entry(pin):
    """Describe a candidate that can be published before the loader lists it."""
    artifact = pin["artifact"]
    grammar = pin["grammar"]
    repository, tag = pin["publication"]["repository"], pin["publication"]["tag"]
    sha = artifact["sha256"]
    if not re.fullmatch(r"[0-9a-f]{64}", sha):
        raise ValueError("expected a full candidate SHA-256")
    return {"language": pin["language"], "extensions": pin["extensions"],
            "grammar_repository": grammar["repository"], "grammar_commit": grammar["commit"],
            "abi": grammar["abi"], "license": grammar["license"], "sample": artifact["sample"],
            "sha256": sha, "size": artifact["size"],
            "url": f"https://github.com/{repository}/releases/download/{tag}/{sha}.wasm"}


def pin_for(entry):
    language = entry["language"]
    if not re.fullmatch(r"[a-z0-9_]+", language):
        raise ValueError("invalid grammar language name")
    path = ROOT / "grammars" / f"{language}.pin.json"
    pin = json.loads(path.read_text())
    require_equal(pin["publication"], GRAMMAR_RELEASE, "grammar release target")
    grammar = pin["grammar"]
    for key in ("language", "extensions"):
        require_equal(pin[key], entry[key], f"{language} {key}")
    for key in ("repository", "commit", "abi", "license"):
        field = f"grammar_{key}" if key in ("repository", "commit") else key
        require_equal(grammar[key], entry[field], f"{language} {key}")
    repository, tag = pin["publication"]["repository"], pin["publication"]["tag"]
    require_equal(entry["url"],
                  f"https://github.com/{repository}/releases/download/{tag}/{entry['sha256']}.wasm",
                  f"{language} release URL")
    if "artifact" in pin:
        candidate = pin_entry(pin)
        for key in ("sha256", "size", "sample"):
            require_equal(candidate[key], entry.get(key), f"{language} {key}")
    return path, pin


def build_entries():
    entries = manifest_entries()
    languages = {entry["language"] for entry in entries}
    for entry in entries:
        yield entry, *pin_for(entry)
    for path in sorted((ROOT / "grammars").glob("*.pin.json")):
        language = path.name.removesuffix(".pin.json")
        if language in languages:
            continue
        pin = json.loads(path.read_text())
        require_equal(pin["language"], language, "candidate pin filename")
        require_equal(pin["publication"], GRAMMAR_RELEASE, "candidate release target")
        if not re.fullmatch(r"[a-z0-9_]+", language):
            raise ValueError("invalid grammar language name")
        if "artifact" not in pin:
            raise ValueError(f"candidate grammar {language} is missing its artifact pin")
        yield pin_entry(pin), path, pin


def sample_for(entry):
    language = entry["language"]
    name = entry.get("sample")
    if not name:
        raise ValueError(f"{language} grammar is missing a sample path")
    sample = (ROOT / name).resolve()
    if Path(name).is_absolute() or not sample.is_relative_to(ROOT / "grammars") or not sample.is_file():
        raise ValueError(f"{language} grammar sample is not a file under grammars/: {name}")
    if sample.suffix.removeprefix(".") not in entry["extensions"]:
        raise ValueError(f"{language} grammar sample has an unlisted extension: {name}")
    if not sample.read_bytes().strip():
        raise ValueError(f"{language} grammar sample is empty: {name}")
    return sample


def verify_artifact(entry, data):
    require_equal(digest(data), entry["sha256"], f"{entry['language']} manifest SHA-256")
    require_equal(len(data), entry["size"], f"{entry['language']} manifest size")


def compile_pinned(entry, pin, work):
    grammar = pin["grammar"]
    commit = entry["grammar_commit"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("grammar revision must be a full commit")
    work.mkdir(parents=True, exist_ok=True)
    env = clean_environment(work)
    downloads = work / "downloads"
    source = work / "source"
    repository = re.fullmatch(r"https://github\.com/([\w.-]+/[\w.-]+)", entry["grammar_repository"])
    if repository is None:
        raise ValueError("expected a GitHub grammar repository")
    repo = repository[1]
    for name, sha in grammar["files"].items():
        if Path(name).is_absolute() or ".." in Path(name).parts:
            raise ValueError(f"invalid pinned source path: {name}")
        fetch(f"https://raw.githubusercontent.com/{repo}/{commit}/{name}",
              sha, source / name, limit=8 * 1024 * 1024)
    parser = (source / "src/parser.c").read_text()
    abi = int(re.search(r"#define LANGUAGE_VERSION (\d+)", parser)[1])
    require_equal(abi, entry["abi"], "grammar ABI")
    allowed_sources = ["src/parser.c"]
    if "src/scanner.c" in grammar["files"]:
        allowed_sources.append("src/scanner.c")
    require_equal(pin["build"]["sources"], allowed_sources, "C input allowlist")
    if not all(name in grammar["files"] for name in allowed_sources):
        raise ValueError("all C sources must be checksum-pinned")
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
    expected_export = f"-Wl,--export=tree_sitter_{entry['language']}"
    if expected_export not in pin["build"]["flags"]:
        raise ValueError(f"grammar build must require the export {expected_export}")
    modules = []
    for name in ("build-a", "build-b"):
        directory = work / name
        if directory.exists():
            shutil.rmtree(directory)
        shutil.copytree(source, directory)
        filename = f"{entry['language']}.wasm"
        run([clang, *pin["build"]["flags"], "-o", filename, "-I", pin["build"]["include"],
              *pin["build"]["sources"]], cwd=directory, env=env)
        modules.append((directory / filename).read_bytes())
    require_equal(modules[0], modules[1], "independent C builds")
    verify_artifact(entry, modules[0])
    return modules[0], compiler_version


def sample_verifier(entry, pin, work):
    language = entry["language"]
    env = clean_environment(work)
    verifier = pin["verifier"]
    cli_archive = fetch(verifier["url"], verifier["sha256"], work / "downloads/tree-sitter.gz")
    cli = work / "tree-sitter"
    cli.write_bytes(gzip.decompress(cli_archive.read_bytes()))
    cli.chmod(0o755)
    version = run([cli, "--version"], cwd=work, env=env).strip()
    empty = work / "parse"
    if empty.exists():
        shutil.rmtree(empty)
    (empty / "src").mkdir(parents=True, exist_ok=True)
    json_write(empty / "src/grammar.json", {"name": language})
    json_write(empty / "tree-sitter.json", {"grammars": [{"name": language, "scope": f"source.{language}",
               "path": ".", "file-types": entry["extensions"]}], "metadata": {"version": "0.0.0",
               "license": entry["license"], "description": "Pinned artifact verification", "authors": [],
               "links": {"repository": "https://github.com/cortexkit/aft"}}})
    shutil.copyfile(work / "source/src/parser.c", empty / "src/parser.c")
    # The loader compares the parser's timestamp to its cache before loading.
    # No scanner or compiler is present in this verification directory.
    os.utime(empty / "src/parser.c", (0, 0))
    return cli, empty, env, version


def verify_luau(a, compiler_version, pin_path, pin, work, artifacts, write_fixtures, verifier):
    grammar = pin["grammar"]
    source = work / "source"
    downloads = work / "downloads"
    cli, empty, env, verifier_version = verifier
    b = variant(a)
    manifests = [manifest(pin, a, "A"), manifest(pin, b, "B")]
    fixtures = []
    for item, data in zip(manifests, (a, b)):
        module_path = artifacts / item["file"]
        module_path.write_bytes(data)
        manifest_path = artifacts / f"manifest-{item['role'].lower()}.json"
        json_write(manifest_path, {"grammars": [item]})
        fixtures.extend((module_path, manifest_path))

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
    corpus = json.loads((ROOT / "grammars/corpus/manifest.json").read_text())
    if not corpus["files"]:
        raise ValueError("empty parse corpus")
    for entry in corpus["files"]:
        path = ROOT / "grammars/corpus" / entry["file"]
        require_equal(digest(path.read_bytes()), entry["sha256"], "corpus bytes")
        trees = [parse(cli, module, path, cwd=empty, env=env, language="luau", root_kind="chunk") for module in
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
        for path in fixtures:
            shutil.copyfile(path, FIXTURES / path.name)
        shutil.copyfile(source / "LICENSE", FIXTURES / "LICENSE.luau")
    else:
        for path in fixtures:
            require_equal(path.read_bytes(), (FIXTURES / path.name).read_bytes(), f"fixture {path.name}")
    report = {"pin_sha256": digest(pin_path.read_bytes()), "compiler": compiler_version,
              "verifier": verifier_version, "builds_equal": True,
              "zed_comparison": zed["comparison"], "corpus_files": len(corpus["files"]),
              "parse_trees_equal": True, "artifacts": manifests,
              "provenance_notes": pin["provenance_notes"]}
    json_write(artifacts / "verification.json", report)
    print(json.dumps(report, indent=2))


def build(output, write_fixtures=False):
    artifacts = output / "artifacts"
    if artifacts.exists():
        shutil.rmtree(artifacts)
    artifacts.mkdir(parents=True)
    for entry, pin_path, pin in build_entries():
        language = entry["language"]
        sample = sample_for(entry)
        work = output / "work" / language
        module, compiler = compile_pinned(entry, pin, work)
        verifier = sample_verifier(entry, pin, work)
        cli, directory, env, _ = verifier
        parse(cli, work / "build-a" / f"{language}.wasm", sample,
              cwd=directory, env=env, language=language)
        if language == "luau":
            verify_luau(module, compiler, pin_path, pin, work, artifacts, write_fixtures, verifier)
        else:
            (artifacts / f"{entry['sha256']}.wasm").write_bytes(module)
            print(f"verified {language}: {entry['sha256']}.wasm ({entry['size']} bytes)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/grammar-build")
    parser.add_argument("--write-fixtures", action="store_true", help="explicit reviewed fixture update; never used in CI")
    options = parser.parse_args()
    build(options.output.resolve(), options.write_fixtures)
