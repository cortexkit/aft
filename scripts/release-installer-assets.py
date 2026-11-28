#!/usr/bin/env python3
"""Validate and assemble AFT's installer assets for the GitHub release."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import stat
import subprocess
from zipfile import BadZipFile, ZipFile


BINARIES = ("ck-aft",)
TARGETS = ("darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "windows-arm64", "windows-x64")
REQUIRES_CORE = "0.24.8"


def check_archives(directory: Path) -> list[Path]:
    archives = {
        directory / f"{binary}-{target}.zip": (target, binary)
        for target in TARGETS
        for binary in BINARIES
    }
    sidecars = {path: path.with_suffix(".zip.sha256") for path in archives}
    expected = {path.name for path in (*archives, *sidecars.values())}
    actual = {path.name for path in directory.iterdir()}
    if expected != actual:
        raise ValueError(f"wrong asset inventory: missing {sorted(expected - actual)}; unexpected {sorted(actual - expected)}")

    for path, (target, binary) in archives.items():
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        sidecar = sidecars[path]
        if sidecar.read_bytes() != f"{digest.hexdigest()}  {path.name}\n".encode("ascii"):
            raise ValueError(f"{sidecar}: checksum does not match the ZIP")
        windows = target.startswith("windows-")
        member = f"{binary}.exe" if windows else binary
        with ZipFile(path) as archive:
            entries = archive.infolist()
            if len(entries) != 1 or entries[0].filename != member or not entries[0].file_size:
                raise ValueError(f"{path}: expected only a nonempty {member} at the ZIP root")
            if not windows:
                mode = entries[0].external_attr >> 16
                if not stat.S_ISREG(mode) or not mode & stat.S_IXUSR:
                    raise ValueError(f"{path}: {member} must be a regular executable")
            if archive.testzip() is not None:
                raise ValueError(f"{path}: ZIP integrity check failed")
    return [*archives, *sidecars.values()]


def check_manifest(path: Path) -> None:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict):
        raise ValueError("manifest must be a JSON object")
    unknown = manifest.keys() - {"requires_core", "binaries"}
    if unknown:
        raise ValueError(f"unknown manifest keys: {', '.join(sorted(unknown))}")
    floor = manifest.get("requires_core")
    if not isinstance(floor, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", floor):
        raise ValueError("requires_core must be a MAJOR.MINOR.PATCH string")

    binaries = manifest.get("binaries", {})
    if not isinstance(binaries, dict):
        raise ValueError("binaries must be a JSON object")
    for name, entry in binaries.items():
        if not isinstance(entry, dict) or entry.keys() != {"reports"}:
            raise ValueError(f"binaries.{name} must contain only a reports key")
        if not isinstance(entry["reports"], str) or not entry["reports"].strip():
            raise ValueError(f"binaries.{name}.reports must be a non-empty string")


def assemble(directory: Path, output: Path, tag: str) -> None:
    assets = check_archives(directory)
    # Renaming the packaged executable to ck-aft does not change its version output.
    reports = subprocess.check_output([output / "aft-linux-x64", "--version"], text=True, timeout=15).rstrip("\n")
    expected = f"aft {tag.removeprefix('v')}"
    if reports != expected:
        raise ValueError(f"release binary reports {reports!r}, expected {expected!r}")

    path = output / "release-manifest.json"
    manifest = {"requires_core": REQUIRES_CORE, "binaries": {"ck-aft": {"reports": reports}}}
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    check_manifest(path)
    for asset in assets:
        shutil.copyfile(asset, output / asset.name)
    print(f"Verified {len(assets) // 2} installer ZIPs and their sidecars; wrote {path}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    assembly = commands.add_parser("assemble")
    assembly.add_argument("directory", type=Path)
    assembly.add_argument("output", type=Path)
    assembly.add_argument("tag")
    check = commands.add_parser("check-manifest")
    check.add_argument("manifest", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "assemble":
            assemble(args.directory, args.output, args.tag)
        else:
            check_manifest(args.manifest)
            print(f"Validated {args.manifest}")
    except (BadZipFile, OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"release assets: {error}\n")


if __name__ == "__main__":
    main()
