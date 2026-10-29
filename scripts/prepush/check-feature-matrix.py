#!/usr/bin/env python3
"""Fail closed when CI starts selecting a Rust feature absent from preflight."""
import re
from pathlib import Path


MATRIX_FEATURES = {"test-timing-hooks", "test-sandbox-launcher", "test-conformance-result-inputs"}


def main():
    sources = sorted(Path(".github/workflows").glob("*.yml"))
    sources += sorted(Path(".github/actions").glob("*/action.yml"))
    sources.append(Path("scripts/rust-test-gate.sh"))
    selected = set()
    for path in sources:
        text = path.read_text()
        if "--all-features" in text or "--no-default-features" in text:
            raise SystemExit(f"Review new Cargo feature mode in {path}; extend fast-compile.sh")
        for match in re.finditer(r"--features(?:=|\s+)([^\n]+)", text):
            value = match.group(1).lstrip()
            if value.startswith(("'", '"')):
                value = value[1:].split(value[0], 1)[0]
            else:
                value = value.split()[0]
            features = set(filter(None, re.split(r"[,\s]+", value)))
            if not features <= MATRIX_FEATURES:
                raise SystemExit(f"Uncovered/dynamic CI feature set {value!r} in {path}; extend fast-compile.sh")
            selected.update(features)
    manifest = Path("crates/aft/Cargo.toml").read_text()
    dev_edge = re.search(r'agent-file-tools\s*=\s*\{[^\n]*features\s*=\s*\[([^]]+)\]', manifest)
    if dev_edge is None:
        raise SystemExit("Review changed self dev-dependency feature edge; extend fast-compile.sh")
    implicit = set(re.findall(r'"([\w-]+)"', dev_edge.group(1)))
    if not implicit <= MATRIX_FEATURES:
        raise SystemExit(f"Uncovered implicit test features: {sorted(implicit - MATRIX_FEATURES)}")
    feature_section = manifest.split("[features]", 1)[1].split("\n[", 1)[0]
    if re.search(r"^default\s*=", feature_section, re.MULTILINE):
        raise SystemExit("Review new package default features; extend fast-compile.sh")
    print(f"Checked {len(sources)} CI/gate sources; explicit={sorted(selected)}, implicit={sorted(implicit)}")


if __name__ == "__main__":
    main()
