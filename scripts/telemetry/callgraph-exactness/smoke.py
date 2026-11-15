#!/usr/bin/env python3
"""Exercise the actual process probe on a small mixed-language checkout."""
import argparse
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).parent))
from run import island, compare, save


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve() / "checkout"
    root.mkdir(parents=True)
    island(root, 0)
    subprocess.run(["git", "init", "-q", str(root)], check=True)
    binary = str(args.binary.resolve())
    store = args.output.resolve() / "store"
    def run(mode, out, reference="-", changes=None, env=None):
        command = [binary, mode, str(root), str(store if mode == "refresh" else out.with_suffix(".store")), str(out), "1", "8388608", "1", str(reference)]
        if changes:
            command.append(str(changes))
        return subprocess.run(command, env=env, check=True)
    a, b = args.output.resolve() / "reference", args.output.resolve() / "candidate"
    run("cold", a)
    run("cold", b, a)
    compare(a, b)
    # Build the store that will survive process exit before the first edit.
    import shutil
    shutil.copytree(b.with_suffix(".store"), store)
    for round_ in (1, 2):
        changed = island(root, round_)
        changes = args.output.resolve() / f"changes-{round_}.json"
        save(changes, changed)
        cold, refreshed = args.output.resolve() / f"cold-{round_}", args.output.resolve() / f"refresh-{round_}"
        run("cold", cold)
        run("refresh", refreshed, cold, changes)
        compare(cold, refreshed)
    import os
    env = dict(os.environ, AFT_EXACTNESS_FLIP_SITE="exactness_gate/service.ts")
    broken = args.output.resolve() / "broken"
    try:
        run("cold", broken, cold, env=env)
    except subprocess.CalledProcessError:
        print("EXACTNESS negative control failed as expected (see named file above)")
    else:
        raise RuntimeError("broken candidate passed")
    print("EXACTNESS SMOKE PASSED: self equality, two restarted refreshes, broken target rejected")


if __name__ == "__main__":
    main()
