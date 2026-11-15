#!/usr/bin/env python3
"""Regenerate the response oracle from the named main, never from the current checkout.

Run from the repository root. The main copy and its separate Cargo target stay
under target/scratch; no output enters the checkout's shared Cargo target.
Only a cfg(test) callback for deterministic vectors and the recording module
are added to the pinned source. Production code paths remain unchanged.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

MAIN = "1c6563b4c965d48c92937d6675cdbca333f37811"
MODULE = "crates/aft/src/data_layer/main_parity_capture.rs"
MEANING = "crates/aft/src/data_layer/meaning.rs"
HOOK = '''        #[cfg(all(test, target_os = "linux"))]
        if main_parity_capture::uses_constant_vectors(checkout.root()) {
            return self.fill_main_parity_vectors(checkout, config);
        }
'''
DECLARATION = '''
#[cfg(all(test, target_os = "linux"))]
#[path = "main_parity_capture.rs"]
mod main_parity_capture;
'''


def main():
    repo = Path(__file__).resolve().parents[5]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", type=Path, default=repo / "target/scratch/s4-main-parity")
    parser.add_argument("--output", type=Path, default=repo / "crates/aft/tests/fixtures/grammars/main-1c6563b4-responses.golden")
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--recordings", type=Path, default=repo / ".cortexkit/internal-docs/investigations/s4-main-parity-round5")
    args = parser.parse_args()
    scratch = args.scratch.resolve()
    output = args.output.resolve()
    recordings = args.recordings.resolve()
    if not scratch.is_relative_to(repo / "target/scratch") or not output.is_relative_to(repo):
        parser.error("scratch must stay under target/scratch and output inside the current worktree")
    if args.runs < 1 or not recordings.is_relative_to(repo):
        parser.error("runs must be positive and recordings must stay inside the current worktree")
    revision = subprocess.check_output(["git", "rev-parse", MAIN], cwd=repo, text=True).strip()
    if revision != MAIN:
        raise RuntimeError(f"unexpected main revision {revision}")
    source = scratch / "source"
    if source.exists():
        shutil.rmtree(source)
    source.mkdir(parents=True, exist_ok=False)
    archive = scratch / "main.tar"
    with archive.open("wb") as handle:
        subprocess.run(["git", "archive", MAIN], cwd=repo, stdout=handle, check=True)
    with tarfile.open(archive) as handle:
        handle.extractall(source, filter="data")
    archive.unlink()
    shutil.copyfile(repo / MODULE, source / MODULE)
    meaning = source / MEANING
    original = meaning.read_text()
    anchor = "    ) -> Round {\n        if model\n"
    if original.count(anchor) != 1:
        raise RuntimeError("pinned fill_round anchor changed; inspect the baseline rather than guessing")
    meaning.write_text(original.replace(anchor, "    ) -> Round {\n" + HOOK + "        if model\n") + DECLARATION)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(scratch / "cargo-target")
    environment["CARGO_BUILD_JOBS"] = "2"
    environment.pop("AFT_MAIN_PARITY_ACTUAL", None)
    environment["AFT_MAIN_PARITY_FIXTURE"] = str(scratch / "fixed-fixture")
    output.parent.mkdir(parents=True, exist_ok=True)
    recordings.mkdir(parents=True, exist_ok=True)
    print(f"Capturing main {MAIN}; isolated target={environment['CARGO_TARGET_DIR']}", flush=True)
    for run in range(1, args.runs + 1):
        # Rebuild each index against the same physical sources and fixed mtimes.
        for case in ("clean", "invalid-utf8"):
            shutil.rmtree(scratch / "fixed-fixture" / case / "storage", ignore_errors=True)
        recording = recordings / f"main-{run}.golden"
        environment["AFT_MAIN_PARITY_CAPTURE"] = str(recording)
        environment["AFT_MAIN_PARITY_RAW"] = str(recordings / f"main-{run}.raw.golden")
        print(f"Main-only capture {run}/{args.runs}", flush=True)
        subprocess.run(
            ["cargo", "test", "-p", "agent-file-tools", "--lib", "--", "data_layer::meaning::main_parity_capture::luau_s4_main_pinned_serialized_responses_match", "--exact"],
            cwd=source,
            env=environment,
            check=True,
        )
        inputs = {}
        for case in ("clean", "invalid-utf8"):
            for name in ("a.ts", "b.ts", "package.json", "b.rs", ".git/HEAD"):
                path = scratch / "fixed-fixture" / case / "project" / name
                if path.exists():
                    stat = path.stat()
                    inputs[f"{case}/{name}"] = {"sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "mtime_ns": stat.st_mtime_ns, "device": stat.st_dev, "inode": stat.st_ino}
        (recordings / f"main-{run}.inputs.json").write_text(json.dumps(inputs, indent=2) + "\n")
        if run == 1:
            shutil.copyfile(recording, output)
    if not output.is_file() or not output.read_text().startswith(f"# main_commit={MAIN}\n"):
        raise RuntimeError("the named test produced no main-pinned recording")
    print(f"Captured main responses: {output}", flush=True)


if __name__ == "__main__":
    main()
