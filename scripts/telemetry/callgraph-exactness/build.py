#!/usr/bin/env python3
"""Build a pinned source revision with only the exactness instrumentation overlaid."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def build(revision, output, profile):
    sha = subprocess.check_output(["git", "rev-parse", revision], cwd=REPO, text=True).strip()
    output.mkdir(parents=True, exist_ok=False)
    source = output / "source"
    source.mkdir()
    archive = subprocess.Popen(["git", "archive", sha], cwd=REPO, stdout=subprocess.PIPE)
    with tarfile.open(fileobj=archive.stdout, mode="r|*") as tar:
        tar.extractall(source, filter="data")
    if archive.wait():
        raise RuntimeError("git archive failed")
    for name in ("crates/aft/src/data_layer/checkout/exactness.rs", "crates/aft/examples/callgraph_exactness.rs"):
        path = source / name
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(REPO / name, path)
    checkout = source / "crates/aft/src/data_layer/checkout.rs"
    text = checkout.read_text()
    if "pub mod exactness;" not in text:
        text = text.replace("mod engines;", 'mod engines;\n#[cfg(feature = "test-hooks")]\npub mod exactness;', 1)
    checkout.write_text(text)
    graph = source / "crates/aft/src/data_layer/callgraph.rs"
    text = graph.read_text()
    if "flip_site_for_control" not in text:
        anchor = "file.site_dependencies = dependencies;"
        if text.count(anchor) != 1:
            raise ValueError("candidate needs explicit integration of the test-only flip_site_for_control hook")
        text = text.replace(anchor, anchor + '\n                        #[cfg(feature = "test-hooks")]\n                        super::checkout::exactness::flip_site_for_control(&path, &mut file);')
    graph.write_text(text)
    gate = source / "crates/aft/src/data_layer/dead_code_gate.rs"
    text = gate.read_text()
    if "fn exactness_rows(" not in text:
        local = (REPO / "crates/aft/src/data_layer/dead_code_gate.rs").read_text()
        helper = local.split("/// All dead-code finding rows,", 1)[1].split("/// Where two aggregates differ,", 1)[0]
        text += "\n/// All dead-code finding rows," + helper
    gate.write_text(text)
    manifest = source / "crates/aft/Cargo.toml"
    text = manifest.read_text()
    if 'name = "callgraph_exactness"' not in text:
        text += '\n[[example]]\nname = "callgraph_exactness"\nrequired-features = ["test-hooks"]\n'
    manifest.write_text(text)
    # A scratch source must never populate the shared warm Cargo target:
    # subsequent jobs could reuse its artifacts for a different source tree.
    target = output / "cargo-target"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    command = ["cargo", "build", "--profile", profile, "-p", "agent-file-tools", "--example", "callgraph_exactness", "--features", "test-hooks"]
    subprocess.run(command, cwd=source, env=env, check=True)
    shutil.copyfile(target / profile / "examples/callgraph_exactness", output / "probe")
    (output / "probe").chmod(0o755)
    (output / "source-commit").write_text(sha + "\n")
    print(f"EXACTNESS BUILT {sha}: {output / 'probe'}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("output", type=Path)
    parser.add_argument("--profile", choices=("release", "ci-storm"), default="ci-storm")
    args = parser.parse_args()
    build(args.revision, args.output.resolve(), args.profile)
