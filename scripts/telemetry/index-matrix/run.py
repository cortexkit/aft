#!/usr/bin/env python3
"""Cold index and guarded call-graph measurements, separate from the nightly gate."""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


# These are the exact transport and kernel/CPU/phase samplers used by cost-gate.
# Importing them, rather than editing their defaults, preserves nightly results.
OSS = load_module("index_matrix_oss", HERE.parent / "oss-matrix.py")


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def disk_bytes(path, *, exclude_git=False, allocated=False):
    command = ["du", "-s", "-B1"] if allocated else ["du", "-sb"]
    if exclude_git:
        command.append("--exclude=.git")
    return int(subprocess.check_output([*command, str(path)], text=True).split()[0])


def isolated_environment(folder, census_path=None):
    # A caller's daemon, remote embedding credentials, and cache overrides must
    # not leak into a measured repository. Runtime/library/proxy paths survive.
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("AFT_") and key not in {
               "HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
               "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "FASTEMBED_CACHE_DIR", "HF_HOME",
               "HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE", "SENTENCE_TRANSFORMERS_HOME"}}
    for key, relative in {"HOME": "home", "XDG_CONFIG_HOME": "xdg/config",
                          "XDG_CACHE_HOME": "xdg/cache", "XDG_DATA_HOME": "xdg/data",
                          "XDG_STATE_HOME": "xdg/state", "XDG_RUNTIME_DIR": "xdg/runtime",
                          "AFT_STORAGE_DIR": "storage", "FASTEMBED_CACHE_DIR": "models",
                          "HF_HOME": "hf"}.items():
        directory = folder / relative
        directory.mkdir(parents=True, exist_ok=True)
        env[key] = str(directory)
    (folder / "xdg/runtime").chmod(0o700)
    env["GIT_LFS_SKIP_SMUDGE"] = "1"
    env["RUST_LOG"] = "info"
    if census_path:
        env["AFT_CALLGRAPH_SHAPE_CENSUS_JSON"] = str(census_path)
    return env


class Client(OSS.NdjsonClient):
    """Reuse gate framing, with explicit environment, stderr and an exec-time guard."""
    def __init__(self, binary, root, env, log, guard_bytes, cgroup_parent=None):
        self.log = log.open("ab")
        self.events_path = log.with_suffix(".memory-events")
        self.cgroup = None
        # RLIMIT_AS is a conservative fallback: RSS cannot exceed address space.
        # Its refusal is not evidence that resident memory would exceed 48 GiB.
        command = ["prlimit", f"--as={guard_bytes}", "--", str(binary)]
        if cgroup_parent:
            self.cgroup = cgroup_parent / f"aft-matrix-{os.getpid()}-{time.time_ns()}"
            self.cgroup.mkdir()
            (self.cgroup / "memory.max").write_text(str(guard_bytes))
            (self.cgroup / "memory.swap.max").write_text("0")
            # The shell joins before exec, so the sampler's PID is AFT itself.
            command = ["sh", "-ec", 'printf "%s" "$$" > "$1/cgroup.procs"; shift; exec "$@"',
                       "aft-matrix-guard", str(self.cgroup), str(binary)]
        self.proc = subprocess.Popen(command, cwd=root, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        self._next_id = 1
        self._buffer = b""

    def close(self):
        try:
            return super().close()
        finally:
            self.log.close()
            if self.cgroup:
                self.events_path.write_text((self.cgroup / "memory.events").read_text())
                self.cgroup.rmdir()



def checked(response, operation):
    if not response.get("success"):
        raise OSS.NdjsonError(f"{operation}: {response}")
    return response


def configure(client, root, env, mode, timeout):
    config = {"indexes": {"trigram": mode in ("text", "full"),
                          "semantic": mode in ("meaning", "full"),
                          "callgraph": mode == "full"},
              "inspect": {"enabled": False}, "semantic": {"backend": "fastembed"}}
    return checked(client.request("configure", timeout_s=timeout,
                   project_root=str(root), harness="runner", storage_dir=env["AFT_STORAGE_DIR"],
                   config=[{"tier": "user", "source": "<index-matrix>", "doc": json.dumps(config)}],
                   _bypass_size_limits=True), "configure")


def states(status, mode):
    wanted = {"text": ["search_index"], "meaning": ["semantic_index"],
              "full": ["search_index", "semantic_index", "callgraph_store"]}[mode]
    return {key: status.get(key, {}).get("status", "unknown") for key in wanted}


def wait_ready(client, mode, deadline, output, sampler=None, phases=None):
    last_census = last_heartbeat = 0.0
    with (output / "status.jsonl").open("a") as statuses, (output / "memory-census.jsonl").open("a") as censuses:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise OSS.BudgetExceeded("readiness deadline exceeded")
            status = checked(client.request("status", timeout_s=min(30, remaining)), "status")
            statuses.write(json.dumps({"monotonic_ns": time.monotonic_ns(), "response": status}) + "\n")
            status_states = states(status, mode)
            if phases:
                phases.mark(status_states.get("search_index", "disabled"), status_states.get("callgraph_store", "disabled"))
            now = time.monotonic()
            if now - last_census >= 5 or all(value == "ready" for value in status_states.values()):
                census = client.request("memory.census", timeout_s=min(30, remaining))
                censuses.write(json.dumps({"monotonic_ns": time.monotonic_ns(), "response": census}) + "\n")
                censuses.flush()
                last_census = now
            if now - last_heartbeat >= 30:
                print(f"{mode}: {status_states}", flush=True)
                last_heartbeat = now
            if all(value == "ready" for value in status_states.values()):
                if mode in ("meaning", "full") and status["semantic_index"].get("backend") != "fastembed":
                    raise ValueError("semantic backend is not fastembed")
                return status
            if any(value in {"disabled", "failed", "suspended", "paused", "backend_unavailable", "unavailable"} for value in status_states.values()):
                raise OSS.NdjsonError(f"index not ready: {status_states}")
            time.sleep(min(0.25, max(0, deadline - time.monotonic())))


def census_observed_max(path):
    maxima = {}
    observations = 0
    if not path.exists():
        return observations, maxima
    with path.open() as handle:
        for line in handle:
            response = json.loads(line)["response"]
            if not response.get("success") or not isinstance(response.get("process"), dict):
                continue
            observations += 1
            values = {"process." + key: value for key, value in response["process"].items() if type(value) is int}
            for detail in response.get("roots", {}).values():
                for key, value in detail.get("planes", {}).items():
                    if type(value) is int:
                        values["root.planes." + key] = values.get("root.planes." + key, 0) + value
            for key, value in values.items():
                maxima[key] = max(maxima.get(key, 0), value)
    return observations, maxima


def measure_build(binary, root, folder, mode, args):
    folder.mkdir(parents=True, exist_ok=False)
    census = folder / "call-shape.json" if mode == "full" else None
    env = isolated_environment(folder, census)
    result = {"mode": mode, "outcome": "failed", "guard_bytes": args.guard_gib * 1024 ** 3,
              "guard_kind": "cgroup-v2 memory.max; swap disabled" if args.cgroup_parent else "RLIMIT_AS (address space, conservative RSS upper bound)",
              "gaps": []}
    client = sampler = phases = None
    started = time.monotonic_ns()
    try:
        client = Client(binary, root, env, folder / "stderr.log", result["guard_bytes"], args.cgroup_parent)
        sampler = OSS.ProcessSampler(client.proc)
        phases = OSS.PhaseSampler(client.proc, folder / "storage")
        sampler.start()
        phases.start()
        configure(client, root, env, mode, min(30, args.timeout_s))
        status = wait_ready(client, mode, time.monotonic() + args.timeout_s, folder, sampler, phases)
        result["wall_ns"] = time.monotonic_ns() - started
        if census:
            export_deadline = time.monotonic() + args.timeout_s
            while not census.exists():
                if client.proc.poll() is not None or time.monotonic() >= export_deadline:
                    raise OSS.NdjsonError("full build ready but census export never appeared")
                time.sleep(0.25)
        save(folder / "ready-status.json", status)
        result["outcome"] = "ready"
        store = status.get("disk", {}).get("data_store_dir")
        if store and Path(store).is_dir():
            result["store_bytes"] = disk_bytes(Path(store))
            result["store_allocated_bytes"] = disk_bytes(Path(store), allocated=True)
        else:
            result["gaps"].append("status did not expose an existing data_store_dir")
    except (OSS.NdjsonError, OSS.BudgetExceeded, OSError, ValueError) as error:
        result["reason"] = str(error)
        result["wall_ns"] = time.monotonic_ns() - started
    finally:
        if sampler:
            # Sample while the process is alive: /proc vanishes after shutdown.
            sampler.stop()
            result.update({"vmhwm_bytes": sampler.peak_rss_hwm_kb * 1024 if sampler.hwm_available else None,
                           "sampled_rss_bytes": sampler.peak_rss_kb * 1024,
                           "cpu_s": sampler.cpu_seconds, "cpu_precision": sampler.cpu_precision,
                           "disk_write_bytes": sampler.disk_write_bytes})
            result["gaps"].extend(sampler.gaps)
        if phases:
            phases.stop()
            OSS.write_phase_trace(folder / "phase-memory.csv", phases.samples)
            result["gaps"].extend(phases.gaps)
        if client:
            gap = client.close()
            if gap:
                result["gaps"].append(gap)
        observations, maxima = census_observed_max(folder / "memory-census.jsonl")
        result["census_observations"] = observations
        result["census_observed_max_bytes"] = maxima
        if not observations:
            result["gaps"].append("no successful AFT memory census observation")
        result["storage_bytes"] = disk_bytes(folder / "storage")
        result["model_bytes"] = disk_bytes(folder / "models")
        save(folder / "build.json", result)
    return result


def validate_shape(value):
    if value.get("schema_version") != 1 or not isinstance(value.get("groups"), list) or not isinstance(value.get("top_callers"), list):
        raise ValueError("unsupported call-shape artifact schema")
    seen = set()
    for row in value["groups"]:
        key = row["language"], row["member"]
        if key in seen or not isinstance(row["sites"], int) or row["sites"] < 0 or not isinstance(row["targets"], int) or row["targets"] < 0:
            raise ValueError("invalid or duplicate call-shape group")
        seen.add(key)
    totals = value.get("language_totals", [])
    for total in totals:
        unknown = sum(count for exit_name, count in total["exits"].items() if exit_name.startswith("unknown"))
        grouped = sum(group["sites"] for group in value["groups"] if group["language"] == total["language"])
        if unknown != grouped or total["sites"] != sum(total["exits"].values()):
            raise ValueError("census groups and resolver exits disagree")
    if len(value["top_callers"]) > 10:
        raise ValueError("census top_callers must contain at most ten symbols")
    return value


def measure_queries(binary, root, folder, artifact, args):
    # Every cold request gets a new AFT process over the already-built store.
    # This empties process/query caches, NOT Linux's page cache. OS-cache-cold
    # runs require --drop-caches, an explicit privileged dedicated-server option.
    top = artifact["top_callers"]
    if not top:
        return {"outcome": "not_measured", "reason": "no callable symbols in census"}
    samples = []
    env = isolated_environment(folder)
    guard = args.guard_gib * 1024 ** 3
    for operation in ("callers", "impact", "trace_to"):
        for index, symbol in enumerate(top):
            if args.drop_caches:
                subprocess.run(["sync"], check=True)
                Path("/proc/sys/vm/drop_caches").write_text("3\n")
            client = Client(binary, root, env, folder / "query-stderr.log", guard, args.cgroup_parent)
            try:
                configure(client, root, env, "full", 30)
                wait_ready(client, "full", time.monotonic() + args.timeout_s, folder)
                for repetition in range(args.warm_repetitions + 1):
                    started = time.monotonic_ns()
                    response = client.request("tool_call", timeout_s=args.query_timeout_s,
                                              name="callgraph", arguments={"op": operation, "path": symbol["path"],
                                              "symbol": symbol["symbol"], "includeTests": True})
                    elapsed = time.monotonic_ns() - started
                    checked(response, operation)
                    sample = {"op": operation, "temperature": "cold" if repetition == 0 else "warm",
                              "symbol_rank": index + 1, "path": symbol["path"], "symbol": symbol["symbol"],
                              "callers": symbol["callers"], "repetition": repetition, "latency_ns": elapsed,
                              "response": response}
                    samples.append(sample)
                    with (folder / "queries.jsonl").open("a") as log:
                        log.write(json.dumps(sample) + "\n")
            finally:
                client.close()
            print(f"query {operation}: {index + 1}/{len(top)} symbols", flush=True)
    result = {"outcome": "measured", "samples": len(samples), "symbols": len(top),
              "cold_definition": "new AFT process; " + ("Linux drop_caches=3 before launch" if args.drop_caches else "OS page cache retained"),
              "warm_definition": "repeated identical request in the same AFT process"}
    save(folder / "query-summary.json", result)
    return result


def checkout(entry, folder):
    root = folder / "checkout"
    env = dict(os.environ, GIT_LFS_SKIP_SMUDGE="1", GIT_TERMINAL_PROMPT="0")
    with (folder / "checkout.log").open("a") as log:
        def git(*arguments):
            return subprocess.check_output(["git", *arguments], env=env, stderr=log, text=True).strip()
        if not root.exists():
            git("clone", "--depth", "1", "--no-checkout", "--single-branch", "--branch", entry["default_branch"],
                entry["clone_url"], str(root))
        git("-C", str(root), "fetch", "--depth", "1", "origin", entry["commit"])
        git("-C", str(root), "checkout", "--detach", entry["commit"])
        if git("-C", str(root), "rev-parse", "HEAD") != entry["commit"]:
            raise ValueError("checkout commit mismatch")
        if git("-C", str(root), "status", "--porcelain", "--untracked-files=normal"):
            raise ValueError("measurement checkout is dirty")
    return root


def fingerprint(entry, args):
    return {"repository": entry, "binary_sha256": sha256(args.aft_binary),
            "shape_binary_sha256": sha256(args.shape_binary) if args.shape_binary else None,
            "guard_gib": args.guard_gib, "timeout_s": args.timeout_s,
            "query_timeout_s": args.query_timeout_s, "warm_repetitions": args.warm_repetitions,
            "drop_caches": args.drop_caches, "harness_sha256": sha256(Path(__file__)),
            "aft_source_commit": args.aft_source_commit, "shape_source_commit": args.shape_source_commit,
            "build_profile": args.build_profile, "cgroup_parent": str(args.cgroup_parent) if args.cgroup_parent else None,
            "runtime_sha256": sha256(Path(os.environ["ORT_DYLIB_PATH"])) if os.environ.get("ORT_DYLIB_PATH") else None}


def completed(folder, expected):
    path = folder / "result.json"
    if not path.exists():
        return False
    result = json.loads(path.read_text())
    if result.get("completed") is not True or result.get("fingerprint") != expected:
        return False
    attempt = folder / result["attempt"]
    if any(not (attempt / mode / filename).is_file() for mode in ("text", "meaning")
           for filename in ("build.json", "ready-status.json", "status.jsonl", "memory-census.jsonl", "phase-memory.csv")):
        raise ValueError(f"completed result lost raw artifacts: {folder}; restore them before resuming")
    return True


def run_repository(entry, args):
    folder = args.results / entry["repository"].replace("/", "--")
    folder.mkdir(parents=True, exist_ok=True)
    identity = fingerprint(entry, args)
    if completed(folder, identity):
        print(f"skip completed {entry['repository']}", flush=True)
        return True
    if (folder / "result.json").exists() and json.loads((folder / "result.json").read_text()).get("completed"):
        raise ValueError(f"completed run has different inputs: choose a new --results folder ({folder})")
    root = checkout(entry, folder).resolve()
    attempt = folder / ("attempt-" + str(time.time_ns()))
    attempt.mkdir()
    result = {"schema_version": 1, "repository": entry["repository"], "commit": entry["commit"],
              "fingerprint": identity, "attempt": attempt.name, "completed": False,
              "checkout_bytes": disk_bytes(root, exclude_git=True),
              "checkout_allocated_bytes": disk_bytes(root, exclude_git=True, allocated=True),
              "checkout_with_git_bytes": disk_bytes(root), "host": {
                  "uname": list(os.uname()), "logical_cpus": os.cpu_count(),
                  "meminfo": Path("/proc/meminfo").read_text(),
                  "python": sys.version,
              }}
    for mode in ("text", "meaning"):
        result[mode] = measure_build(args.aft_binary, root, attempt / mode, mode, args)
        save(folder / "result.json", result)
    if args.shape_binary:
        result["full"] = measure_build(args.shape_binary, root, attempt / "full", "full", args)
        if result["full"]["outcome"] == "ready":
            try:
                artifact = validate_shape(json.loads((attempt / "full/call-shape.json").read_text()))
                result["call_shape"] = {"outcome": "measured", "artifact": str(Path(attempt.name) / "full/call-shape.json")}
                result["queries"] = measure_queries(args.aft_binary, root, attempt / "full", artifact, args)
            except (OSError, ValueError, KeyError, OSS.NdjsonError, OSS.BudgetExceeded) as error:
                result.setdefault("call_shape", {"outcome": "not_measured", "reason": str(error)})
                result["queries"] = {"outcome": "not_measured", "reason": str(error)}
        else:
            reason = "full build did not finish under guard: " + result["full"].get("reason", "unknown")
            result["call_shape"] = result["queries"] = {"outcome": "not_measured", "reason": reason}
    else:
        result["call_shape"] = result["queries"] = {"outcome": "not_measured", "reason": "no --shape-binary supplied; feature-gated census required"}
    result["completed"] = all(result[mode]["outcome"] == "ready" for mode in ("text", "meaning"))
    save(folder / "result.json", result)
    return result["completed"]


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=HERE / "manifest.json")
    parser.add_argument("--repository", action="append", help="owner/name; omit to run the entire manifest serially")
    parser.add_argument("--aft-binary", type=Path, default=REPO / "target/release/aft")
    parser.add_argument("--shape-binary", type=Path, help="release binary built with callgraph-shape-census")
    parser.add_argument("--aft-source-commit", required=True, help="full source commit used to build --aft-binary")
    parser.add_argument("--shape-source-commit", help="full source commit used to build --shape-binary")
    parser.add_argument("--build-profile", choices=("release", "ci-storm"), default="release")
    parser.add_argument("--cgroup-parent", type=Path, help="writable delegated cgroup-v2 directory with memory controller (recommended)")
    parser.add_argument("--results", type=Path, default=REPO / "target/index-matrix/results")
    parser.add_argument("--guard-gib", type=int, default=48, choices=range(1, 49))
    parser.add_argument("--timeout-s", type=float, default=14400)
    parser.add_argument("--query-timeout-s", type=float, default=120)
    parser.add_argument("--warm-repetitions", type=int, default=10)
    parser.add_argument("--drop-caches", action="store_true", help="write Linux drop_caches=3 before every cold query (requires privilege; dedicated host only)")
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("measurement requires Linux /proc and prlimit")
    if args.timeout_s <= 0 or args.query_timeout_s <= 0 or args.warm_repetitions < 1:
        parser.error("deadlines and warm repetitions must be positive")
    if any(len(value) != 40 or any(char not in "0123456789abcdef" for char in value) for value in (args.aft_source_commit, args.shape_source_commit or args.aft_source_commit)):
        parser.error("source commits must be full lowercase SHA-1 IDs")
    if args.shape_binary and not args.shape_source_commit:
        parser.error("--shape-source-commit is required with --shape-binary")
    for key in ("aft_binary", "shape_binary", "results", "manifest", "cgroup_parent"):
        value = getattr(args, key)
        if value is not None:
            setattr(args, key, value.resolve())
    for binary in (args.aft_binary, args.shape_binary):
        if binary and (not binary.is_file() or "debug" in binary.parts):
            parser.error(f"expected an existing RELEASE binary, not debug: {binary}")
    return args


def main():
    import fcntl
    args = parse_args()
    manifest = json.loads(args.manifest.read_text())
    entries = manifest["repositories"]
    if args.repository:
        unknown = set(args.repository) - {entry["repository"] for entry in entries}
        if unknown:
            raise ValueError(f"repositories absent from manifest: {sorted(unknown)}")
        entries = [entry for entry in entries if entry["repository"] in args.repository]
    args.results.mkdir(parents=True, exist_ok=True)
    # Separate one-repository invocations must not accidentally run concurrently.
    with (args.results / ".matrix.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        outcomes = [run_repository(entry, args) for entry in entries]
    return 0 if all(outcomes) else 1


if __name__ == "__main__":
    raise SystemExit(main())
