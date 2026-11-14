#!/usr/bin/env python3
"""Pull finished-project evidence over SSH; credentials stay on the collector."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import shlex
import subprocess
import time

FILTERS = ["--exclude=checkout/", "--exclude=models/", "--exclude=hf/", "--exclude=xdg/", "--exclude=home/",
           "--include=*/", "--include=*.json", "--include=*.jsonl", "--include=*.csv", "--include=*.md",
           "--include=*.log", "--include=*.memory-events", "--exclude=*"]


def ssh_command(args):
    return ["ssh", "-F", "/dev/null", "-i", str(args.identity), "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", f"UserKnownHostsFile={args.known_hosts}",
            "-o", "StrictHostKeyChecking=yes", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20"]


def collect_once(args, copied):
    ssh = ssh_command(args)
    response = subprocess.run([*ssh, args.server, "cat " + shlex.quote(args.remote_results + "/matrix-state.json")],
                              capture_output=True, text=True, check=True)
    state = json.loads(response.stdout)
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "matrix-state.json").write_text(response.stdout)
    def pull(relative, destination):
        destination.mkdir(parents=True, exist_ok=True)
        subprocess.run(["rsync", "-az", "-e", shlex.join(ssh), *FILTERS,
                        args.server + ":" + args.remote_results + "/" + relative, str(destination) + "/"], check=True)
    for name, row in state["repositories"].items():
        if row.get("status") not in ("completed", "failed") or copied.get(name) == row.get("finished_at"):
            continue
        slug = name.replace("/", "--")
        pull(slug + "/", args.output / slug)
        copied[name] = row["finished_at"]
        (args.output / "collection.json").write_text(json.dumps(copied, indent=2) + "\n")
        print(f"copied {name}: {row['status']} full={row.get('full_outcome')}", flush=True)
    pull("tables/", args.output / "tables")
    subprocess.run(["rsync", "-az", "-e", shlex.join(ssh), args.server + ":" + args.remote_results + "/matrix-plan.json", str(args.output)], check=True)
    return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", required=True, help="SSH destination, e.g. root@public-address")
    parser.add_argument("--identity", type=Path, required=True, help="local throwaway key path; never copied")
    parser.add_argument("--known-hosts", type=Path, required=True)
    parser.add_argument("--remote-results", required=True, help="absolute path, without spaces, on the server")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--watch", action="store_true")
    parser.add_argument("--interval-s", type=float, default=30)
    parser.add_argument("--stop-after", type=int, help="exit after copying this many distinct finished projects")
    args = parser.parse_args()
    if not args.remote_results.startswith("/") or any(char.isspace() for char in args.remote_results):
        parser.error("--remote-results must be an absolute path without whitespace")
    if args.interval_s <= 0 or (args.stop_after is not None and args.stop_after < 1):
        parser.error("interval and stop-after must be positive")
    path = args.output / "collection.json"
    copied = json.loads(path.read_text()) if path.exists() else {}
    while True:
        try:
            state = collect_once(args, copied)
            if not args.watch or state.get("finished_at") or (args.stop_after and len(copied) >= args.stop_after):
                return 0
        except (subprocess.CalledProcessError, json.JSONDecodeError) as error:
            if not args.watch:
                raise
            print(f"collection will retry: {error}", flush=True)
        time.sleep(args.interval_s)


if __name__ == "__main__":
    raise SystemExit(main())
