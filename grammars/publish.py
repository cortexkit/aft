#!/usr/bin/env python3
"""Publish one content-addressed grammar, never replacing a release asset."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile

from build import digest, require_equal


def gh(args):
    return subprocess.run(["gh", *args], text=True, capture_output=True, check=False)


def checked(args):
    result = gh(args)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout


def publish(artifact, entry, repository, tag, scratch):
    data = artifact.read_bytes()
    require_equal(digest(data), entry["sha256"], "publication hash")
    require_equal(len(data), entry["size"], "publication size")
    name = f"{digest(data)}.wasm"
    require_equal(artifact.name, name, "publication filename")
    result = gh(["release", "view", tag, "--repo", repository, "--json", "assets"])
    if result.returncode:
        # Authentication and network failures must not be mistaken for a new release.
        if "release not found" not in result.stderr.lower():
            raise RuntimeError(result.stderr)
        # Grammar assets must not displace application versions as "latest".
        checked(["release", "create", tag, "--repo", repository, "--target", "main",
                 "--latest=false", "--prerelease",
                 "--title", "Grammar artifacts", "--notes", "Immutable content-addressed grammar assets."])
        assets = []
    else:
        assets = json.loads(result.stdout)["assets"]
    if any(asset["name"] == name for asset in assets):
        scratch.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            checked(["release", "download", tag, "--repo", repository, "--pattern", name,
                     "--dir", directory])
            require_equal((Path(directory) / name).read_bytes(), data, "existing release asset bytes")
        print(f"verified existing {name}; uploaded nothing")
        return "verified_existing"
    # No --clobber: a concurrent publisher can cause a refusal, never replacement.
    checked(["release", "upload", tag, str(artifact), "--repo", repository])
    print(f"uploaded {name}")
    return "uploaded"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--pin", type=Path, default=Path(__file__).with_name("luau.pin.json"))
    args = parser.parse_args()
    pin = json.loads(args.pin.read_text())
    entry = json.loads((args.artifacts / "manifest-a.json").read_text())
    publish(args.artifacts / entry["file"], entry, pin["publication"]["repository"],
            pin["publication"]["tag"], args.artifacts / "publish-scratch")
