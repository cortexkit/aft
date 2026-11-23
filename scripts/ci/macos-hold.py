#!/usr/bin/env python3
"""Lease the Mac mini from MOTOR, the fleet VM scheduler, for a CI job."""

import argparse
import importlib.util
import os
import sys
from datetime import datetime, timedelta
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "mac_cost_gate", Path(__file__).resolve().parents[1] / "telemetry/mac-cost-gate.py"
)
assert spec is not None and spec.loader is not None
protocol = importlib.util.module_from_spec(spec)
spec.loader.exec_module(protocol)

GRANT_WAIT = timedelta(minutes=50)
RENEWAL_ACK_WAIT = timedelta(minutes=5)


def job_owner(env, shard=""):
    run, attempt, job = (env[key] for key in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB"))
    if not run or not attempt or not job:
        raise ValueError("GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT and GITHUB_JOB must all be set")
    suffix = f"-{shard}" if shard else ""
    return f"aft-tests-{run}-{attempt}-{job}{suffix}"


def require_grant(hold, deadline):
    if not hold.wait(deadline=deadline):
        raise RuntimeError(
            f"Timed out waiting for MOTOR grant {hold.identity} in {hold.grant_folder / 'granted.json'} "
            f"(deadline: {deadline.isoformat()}); refusing to run without exclusive access"
        )
    print(f"MOTOR hold granted: {hold.identity}", flush=True)


def acquire(owner, holds, grants):
    deadline = protocol.utcnow() + GRANT_WAIT
    # The initial lease only needs time to drain the fleet. Start the full
    # lease at the grant so the drain does not consume the build's work time.
    hold = protocol.Hold(holds, deadline, grants, owner=owner)
    hold.request()
    try:
        require_grant(hold, deadline)
        refresh(owner, holds, grants, deadline=deadline)
    except BaseException:
        # If renewal was published but not granted, its expiry has changed.
        protocol.remove_owned_hold(holds, owner)
        raise


def refresh(owner, holds, grants, *, deadline=None):
    record = protocol.read_hold_record(holds / "hold.json")
    if not record or record["owner"] != owner:
        raise RuntimeError(f"No MOTOR hold for {owner} to renew")
    expiry = datetime.fromisoformat(record["expires_at"].replace("Z", "+00:00"))
    hold = protocol.Hold(holds, expiry, grants, owner=owner)
    now = protocol.utcnow()
    hold.refresh(now + protocol.MAX_HOLD_DURATION)
    cutoff = min(expiry, now + RENEWAL_ACK_WAIT)
    require_grant(hold, min(cutoff, deadline) if deadline is not None else cutoff)


def release(owner, holds):
    """Remove this job's hold and say what happened, so a hold left behind is visible."""
    path = holds / "hold.json"
    record = protocol.read_hold_record(path)
    if record is None:
        if path.exists() or path.is_symlink():
            # The fleet stays blocked until the hold expires, so do not pass quietly.
            raise RuntimeError(f"MOTOR hold at {path} is unreadable or malformed; it was left in place")
        print(f"No MOTOR hold to release for {owner}", flush=True)
        return
    if record["owner"] != owner:
        print(f"::warning::MOTOR hold at {path} belongs to {record['owner']}, not {owner}; left in place", flush=True)
        return
    protocol.remove_owned_hold(holds, owner)
    remaining = protocol.read_hold_record(path)
    if remaining is not None and remaining["owner"] == owner:
        raise RuntimeError(f"MOTOR hold at {path} for {owner} is still present after release")
    print(f"Released MOTOR hold for {owner}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("acquire", "refresh", "release"))
    parser.add_argument("--shard", default="", help="Include the matrix shard in the job owner")
    args = parser.parse_args()
    try:
        owner = job_owner(os.environ, args.shard)
        if args.phase == "release":
            release(owner, protocol.HOLD_DIR)
        else:
            action = acquire if args.phase == "acquire" else refresh
            action(owner, protocol.HOLD_DIR, protocol.GRANT_DIR)
    except FileExistsError:
        print(f"::error::MOTOR hold already exists at {protocol.HOLD_DIR / 'hold.json'}; refusing to replace it", file=sys.stderr)
        return 1
    except (KeyError, OSError, RuntimeError, ValueError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
