#!/usr/bin/env python3
"""Prepare the fixed Mac corpus, then measure only under MOTOR's matching grant."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).resolve().parent
HOLD_DIR = Path("/Users/Shared/motor-macos-hold")
GRANT_DIR = Path("/Users/Shared/motor-macos-grant")
OWNER = "aft-nightly"
INITIAL_DURATION_SECONDS = 30 * 60
VM_NAMES = ("com.apple.Virtualization.VirtualMachine", "Virtualization.VirtualMachine",
            "vfkit", "vmware-vmx", "prl_vm_app", "tart")


def utcnow():
    return datetime.now(timezone.utc)


def window_end(now):
    if not 3 <= now.hour < 5:
        return None
    return now.replace(hour=5, minute=0, second=0, microsecond=0)


def write_json(path, payload):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    temporary.replace(path)


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


class Hold:
    def __init__(self, folder, end, grant_folder=GRANT_DIR):
        self.folder = folder
        self.grant_folder = grant_folder
        self.end = end
        self.identity = {"owner": OWNER, "expires_at": end.isoformat(timespec="seconds").replace("+00:00", "Z")}
        self.created = False

    def request(self):
        # Exclusive creation never overwrites a hold left by another owner.
        # MOTOR provisions the shared directory; missing permissions are an
        # operator problem, not a reason to install software or use sudo.
        with (self.folder / "hold.json").open("x", encoding="utf-8") as handle:
            self.created = True
            json.dump(self.identity, handle)
        print(f"MOTOR hold requested: {self.identity}", flush=True)

    def granted(self):
        grant = read_json(self.grant_folder / "granted.json")
        return isinstance(grant, dict) and all(grant.get(key) == value for key, value in self.identity.items())

    def wait(self):
        while utcnow() < self.end:
            if self.granted():
                return True
            print("waiting for MOTOR grant (release/VM jobs may still be draining)", flush=True)
            time.sleep(min(30, max(0, (self.end - utcnow()).total_seconds())))
        return False

    def release(self):
        if self.created:
            current = read_json(self.folder / "hold.json")
            if current == self.identity or current is None:
                (self.folder / "hold.json").unlink(missing_ok=True)
            self.created = False


def command_output(argv):
    result = subprocess.run(argv, text=True, capture_output=True, timeout=30, check=True)
    return result.stdout.strip()


def machine_facts() -> dict[str, Any]:
    return {
        "recorded_at": utcnow().isoformat(),
        "model": command_output(["sysctl", "-n", "hw.model"]),
        "cpu": command_output(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "cpus": command_output(["sysctl", "-n", "hw.logicalcpu"]),
        "memory_bytes": command_output(["sysctl", "-n", "hw.memsize"]),
        "macos": command_output(["sw_vers"]),
        "power_source": command_output(["pmset", "-g", "batt"]),
        "power_settings": command_output(["pmset", "-g", "custom"]),
    }


def swap_used_bytes(text):
    match = re.search(r"\bused\s*=\s*([0-9.]+)([KMGTP])", text)
    if not match:
        raise ValueError(f"cannot read swap usage: {text}")
    return int(float(match[1]) * 1024 ** ("KMGTP".index(match[2]) + 1))


def vm_processes(text):
    return [line.strip() for line in text.splitlines()
            if Path(line.strip()).name in VM_NAMES or Path(line.strip()).name.startswith("qemu-system-")]


class IsolationMonitor:
    """A grant admits the run; this monitor can invalidate it, never admit it."""

    def __init__(self, hold):
        self.hold = hold
        self.reasons = []
        self.initial_swap = None
        self.max_swap = 0
        self.stop_event = threading.Event()
        self.thread = None

    def sample(self):
        try:
            if utcnow() >= self.hold.end:
                self.reasons.append("hold window expired during measurement")
            if not self.hold.granted():
                self.reasons.append("MOTOR grant disappeared or changed during measurement")
            vms = vm_processes(command_output(["ps", "-axww", "-o", "comm="]))
            if vms:
                self.reasons.append("job VM present: " + ", ".join(vms))
            swap = swap_used_bytes(command_output(["sysctl", "vm.swapusage"]))
            if self.initial_swap is None:
                self.initial_swap = swap
            if swap > self.initial_swap:
                self.reasons.append(f"swap grew from {self.initial_swap} to {swap} bytes")
            self.max_swap = max(self.max_swap, swap)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            self.reasons.append(f"isolation observation unavailable: {error}")

    def start(self):
        self.sample()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def loop(self):
        while not self.stop_event.wait(1):
            self.sample()

    def stop(self):
        self.stop_event.set()
        if self.thread:
            self.thread.join()
        self.sample()


def expected_duration(history):
    durations = read_json(history)
    if not isinstance(durations, list) or not durations:
        return INITIAL_DURATION_SECONDS
    return max(float(value) for value in durations[-3:])


def enough_time(end, seconds):
    return (end - utcnow()).total_seconds() >= seconds + 10 * 60


def gate_module():
    spec = importlib.util.spec_from_file_location("nightly_cost_gate", SCRIPT_DIR / "cost-gate.py")
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load nightly cost comparator")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def measured_command(argv, log, end):
    """Keep a heartbeat and stop the whole measurement group at the window end."""
    with log.open("w", encoding="utf-8") as output:
        process = subprocess.Popen(argv, stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while True:
                remaining = (end - utcnow()).total_seconds()
                if remaining <= 0:
                    raise TimeoutError("measurement reached the hold window's end")
                try:
                    code = process.wait(timeout=min(60, remaining))
                    if code:
                        raise RuntimeError(f"cost measurement exited {code}; see {log}")
                    return
                except subprocess.TimeoutExpired:
                    print(f"measuring: {log.name}; deadline {end.isoformat()}", flush=True)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()


def run(args):
    gate = gate_module()
    home = args.home.expanduser().resolve()
    source = SCRIPT_DIR.parents[1]
    if home == source or source in home.parents:
        raise RuntimeError("Mac cost storage must be outside the checkout")
    home.mkdir(parents=True, exist_ok=True)
    (home / ".metadata_never_index").touch()
    # A failure here must be fixed by the runner operator, not ignored. No sudo
    # and no system software installation are needed or attempted by this job.
    command_output(["tmutil", "addexclusion", str(home)])
    excluded = command_output(["tmutil", "isexcluded", str(home)])
    if "[Excluded]" not in excluded:
        raise RuntimeError(f"Time Machine exclusion was not applied: {excluded}")
    output = Path(os.environ.get("AFT_COST_GATE_RESULTS_DIR") or home / "results" / utcnow().strftime("%Y%m%dT%H%M%SZ"))
    output.mkdir(parents=True, exist_ok=True)
    verdict = output / "night-status.json"

    def status(state, **details):
        write_json(verdict, {"status": state, **details})
        print(state, flush=True)
        summary = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary:
            with Path(summary).open("a", encoding="utf-8") as handle:
                handle.write(f"### Mac nightly cost: {state}\n\n")

    end = window_end(utcnow())
    if end is None:
        status("not run: outside 03:00–05:00 UTC window")
        return 0
    duration = expected_duration(home / "measured-durations.json")
    if not enough_time(end, duration):
        status("not run: too late in the window")
        return 0
    baseline = SCRIPT_DIR / "cost-baselines.mac-mini.json"
    if not baseline.exists():
        if not args.write_baseline:
            raise RuntimeError("Mac baseline missing; dispatch write_baseline=true before enabling comparison")
        baseline = SCRIPT_DIR / "cost-baselines.json"
    if args.write_baseline and args.repo != "all":
        raise RuntimeError("Mac baseline capture requires repo=all")
    payload = gate.load_baseline(baseline)
    names = list(gate.REPO_ORDER) if args.repo == "all" else [args.repo]
    binary = home / "target" / "release" / "aft"
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(home / "target")
    print("preparing release binary before requesting MOTOR hold", flush=True)
    subprocess.run(["cargo", "build", "--features", "wasm-grammars", "--release", "-p", "agent-file-tools"],
                   cwd=source, env=environment, check=True, timeout=75 * 60)
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"prepared binary missing: {binary}")
    print("preparing pinned corpus before requesting MOTOR hold", flush=True)
    gate.prepare_repositories(payload, names, home / "corpus")
    # Setup may consume most of the window on a cold build. Do not make fleet
    # jobs wait when there is no longer enough time for a useful measurement.
    if not enough_time(end, duration):
        status("not run: too late in the window")
        return 0
    hold = Hold(args.hold_dir, end, args.grant_dir)
    monitor = None
    try:
        hold.request()
        if not hold.wait():
            status("not run: machine busy")
            return 0
        if not enough_time(end, duration):
            status("not run: too late in the window")
            return 0
        facts = machine_facts()
        facts["hold"] = hold.identity
        facts["exclusion"] = excluded
        write_json(output / "machine.json", facts)
        print("machine: " + json.dumps(facts), flush=True)
        working = output / "working-baseline.json"
        shutil.copyfile(baseline, working)
        common = ["bash", str(SCRIPT_DIR / "cost-gate.sh"), "--baseline", str(working),
                  "--aft-binary", str(binary), "--cache-dir", str(home / "corpus"), "--prepared-only"]
        if args.repo != "all":
            common += ["--repo", args.repo]
        monitor = IsolationMonitor(hold)
        monitor.start()
        started = time.monotonic()
        error = None
        try:
            if not monitor.reasons:
                measured_command(common + ["--results-dir", str(output / "capture")] +
                                 (["--write-baseline"] if args.write_baseline else []), output / "capture.log", end)
                monitor.sample()
                if args.write_baseline and not monitor.reasons:
                    measured_command(common + ["--results-dir", str(output / "verification")],
                                     output / "verification.log", end)
        except (OSError, RuntimeError, TimeoutError, subprocess.SubprocessError) as caught:
            error = caught
        finally:
            monitor.stop()
            elapsed = time.monotonic() - started
            write_json(output / "machine-after.json", machine_facts())
        if monitor.reasons:
            status("not judged: machine contaminated", reasons=sorted(set(monitor.reasons)),
                   initial_swap_bytes=monitor.initial_swap, max_swap_bytes=monitor.max_swap)
            return 0
        if error:
            status("failed: cost harness", reason=str(error))
            raise error
        if args.write_baseline:
            candidate = json.loads(working.read_text(encoding="utf-8"))
            candidate["measured_on"]["machine"] = facts
            write_json(output / "cost-baselines.candidate.json", candidate)
        history = read_json(home / "measured-durations.json") or []
        write_json(home / "measured-durations.json", (history + [elapsed])[-3:])
        status("judged", measured_seconds=elapsed)
        return 0
    finally:
        hold.release()
        # Index payloads are not evidence and accumulate quickly on a fixed
        # host. Keep logs/CSVs, but reclaim only this run's generated stores,
        # after measurement and after releasing the fleet hold.
        for scratch in output.glob("*/aft-cost-gate-results/cost-gate-*/run-*/scratch"):
            shutil.rmtree(scratch)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, default=Path.home() / "aft-nightly-cost")
    parser.add_argument("--hold-dir", type=Path, default=HOLD_DIR, help="MOTOR hold folder (override only for tests)")
    parser.add_argument("--grant-dir", type=Path, default=GRANT_DIR, help="MOTOR grant folder (override only for tests)")
    parser.add_argument("--repo", default="all", choices=("all", "redox", "typescript-eslint", "jupyterlab", "hugo", "synthetic-24k"))
    parser.add_argument("--write-baseline", action="store_true")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("Mac nightly wrapper requires macOS")

    def interrupted(signum, _frame):
        raise RuntimeError(f"interrupted by signal {signum}")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    return run(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"mac-cost-gate: {error}", file=sys.stderr)
        raise SystemExit(1)
