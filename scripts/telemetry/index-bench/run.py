#!/usr/bin/env python3
"""Cold text-plus-callgraph builds on native runners, with a small Linux proof mode."""
from __future__ import annotations

import argparse
import csv
import importlib.util
import json
import os
from pathlib import Path
import platform
import queue
import re
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time

from native import Child, GIB

HERE = Path(__file__).resolve().parent
MATRIX_DIR = HERE.parent / "index-matrix"
SPEC = importlib.util.spec_from_file_location("native_bench_matrix", MATRIX_DIR / "run.py")
assert SPEC is not None and SPEC.loader is not None
MATRIX = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MATRIX)
PROJECTS = {"vscode": "microsoft/vscode", "kubernetes": "kubernetes/kubernetes",
            "elasticsearch": "elastic/elasticsearch", "linux": "torvalds/linux",
            "chromium": "chromium/chromium", "neovim": "neovim/neovim"}
BASELINE = {"macos": {"kubernetes", "linux"},
            "windows": {"vscode", "kubernetes", "linux", "chromium"}, "linux": {"neovim"}}
FIELDS = ["project", "version", "reported_version", "binary_sha256", "revision", "platform",
          "peak_memory_bytes", "peak_commit_bytes", "wall_seconds", "cpu_seconds", "outcome",
          "store_bytes", "excluded_files", "guard_triggered", "peak_source", "exit_code", "error"]


def host_platform():
    return "windows" if os.name == "nt" else "macos" if sys.platform == "darwin" else "linux"


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def remove(path):
    def writable(function, filename, _):
        os.chmod(filename, stat.S_IWRITE | stat.S_IREAD)
        function(filename)
    if path.exists():
        shutil.rmtree(path, onerror=writable)


def windows_invalid(path):
    """Win32 rejects device basenames even with extensions, in any directory."""
    return any(re.search(r'[<>:"\\|?*\x00-\x1f]', part)
               or part.endswith((".", " "))
               or re.fullmatch(r"(?i:con|prn|aux|nul|com[1-9¹²³]|lpt[1-9¹²³])", part.split(".")[0])
               for part in path.split("/"))


def windows_exclusions(paths):
    counts = {}
    for path in paths:
        counts[path.casefold()] = counts.get(path.casefold(), 0) + 1
    return [path for path in paths if windows_invalid(path) or counts[path.casefold()] > 1]


def sparse_literal(path):
    # Non-cone patterns are gitignore patterns: escape metacharacters in filenames.
    return "!/" + re.sub(r"([\\*?\[\]])", r"\\\1", path)


def checkout(entry, destination, logs, windows=False):
    destination.mkdir(parents=True)
    env = {**os.environ, "GIT_LFS_SKIP_SMUDGE": "1", "GIT_TERMINAL_PROMPT": "0"}
    def git(*args, capture=False, data=None):
        command = ["git", "-c", "core.longpaths=true", "-c", "core.symlinks=false", "-C", str(destination), *args]
        if capture:
            return subprocess.check_output(command, env=env)
        with (logs / "clone.log").open("ab") as log:
            subprocess.run(command, input=data, stdout=log, stderr=log, env=env, check=True, timeout=1800)
        return b""
    git("init")
    git("remote", "add", "origin", entry["clone_url"])
    print(f"fetch {entry['repository']} at {entry['commit']}", flush=True)
    git("fetch", "--depth=1", "origin", entry["commit"])
    excluded = []
    if windows:
        paths = [path for path in git("ls-tree", "-r", "--name-only", "-z", "FETCH_HEAD", capture=True).decode("utf-8").split("\0") if path]
        excluded = windows_exclusions(paths)
        git("sparse-checkout", "init", "--no-cone")
        patterns = "/*\n" + "\n".join(sparse_literal(path) for path in excluded) + "\n"
        git("sparse-checkout", "set", "--no-cone", "--stdin", data=patterns.encode("utf-8"))
    git("checkout", "--detach", "FETCH_HEAD")
    revision = git("rev-parse", "HEAD", capture=True).decode().strip()
    if revision != entry["commit"]:
        raise ValueError(f"checkout identity mismatch: {revision}")
    save(logs / "checkout.json", {**entry, "revision": revision, "excluded_files": len(excluded), "excluded_paths": excluded})
    return len(excluded)


class Client:
    """A reader thread works for Windows pipes; select() on pipes does not."""
    def __init__(self, binary, root, env, folder, guard_gib, deadline):
        self.log = (folder / "stderr.log").open("w", encoding="utf-8")
        try:
            self.child = Child([str(binary)], root, env, self.log, guard_gib)
        except BaseException:
            self.log.close()
            raise
        self.deadline = deadline
        self.frames = queue.Queue()
        self.next_id = 0
        self.folder = folder
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        with (self.folder / "stdout.jsonl").open("w", encoding="utf-8") as log:
            for line in self.child.stdout:
                log.write(line)
                log.flush()
                try:
                    self.frames.put(json.loads(line))
                except json.JSONDecodeError:
                    pass
            self.frames.put(None)

    # The first status after configure got no reply within 30 s on Chromium on
    # NTFS, for both versions, while configure's first walk ran. The run
    # deadline still bounds every request.
    def request(self, command, timeout_s=300, **params):
        self.next_id += 1
        request_id = str(self.next_id)
        payload = {"id": request_id, "command": command, **params}
        with (self.folder / "requests.jsonl").open("a", encoding="utf-8") as log:
            log.write(json.dumps(payload) + "\n")
        self.child.stdin.write(json.dumps(payload) + "\n")
        self.child.stdin.flush()
        deadline = min(self.deadline, time.monotonic() + timeout_s)
        while time.monotonic() < deadline:
            try:
                frame = self.frames.get(timeout=min(0.25, max(0.001, deadline - time.monotonic())))
            except queue.Empty:
                if self.child.done.is_set():
                    raise RuntimeError(f"AFT exited: {self.child.returncode}")
                continue
            if frame is None:
                raise RuntimeError("AFT closed stdout")
            if str(frame.get("id")) == request_id:
                return frame
        if time.monotonic() >= self.deadline:
            raise TimeoutError("index readiness deadline exceeded")
        raise RuntimeError(f"protocol timeout waiting for {command}")

    def close(self):
        measurement = self.child.finish()
        self.reader.join()
        self.child.stdout.close()
        self.log.close()
        return measurement


def store_bytes(path):
    return sum(file.stat().st_size for file in path.rglob("*") if file.is_file())


def measure(binary, root, folder, logs, guard_gib, timeout_s, memory=None):
    folder.mkdir(parents=True)
    logs.mkdir(parents=True)
    env = MATRIX.isolated_environment(folder)
    # Windows configuration discovery also consults these native profile paths.
    for key, relative in {"USERPROFILE": "home", "APPDATA": "appdata", "LOCALAPPDATA": "localappdata"}.items():
        env[key] = str(folder / relative)
        Path(env[key]).mkdir(exist_ok=True)
    user_config = Path(env["XDG_CONFIG_HOME"]) / "cortexkit/aft.jsonc"
    if memory:
        save(user_config, {"memory": memory, "indexes": {"trigram": True, "callgraph": True,
             "semantic": False}, "inspect": {"enabled": False}})
        shutil.copy2(user_config, logs / "user-aft.jsonc")
    started = time.monotonic()
    client = Client(binary, root, env, logs, guard_gib, started + timeout_s)
    outcome, error = "ready", ""
    try:
        if memory:
            MATRIX.checked(client.request("configure", timeout_s=min(60, timeout_s),
                project_root=str(root), harness="runner", storage_dir=env["AFT_STORAGE_DIR"],
                config=[{"tier": "user", "source": str(user_config), "doc": user_config.read_text()}],
                _bypass_size_limits=True), "configure")
        else:
            MATRIX.configure(client, root, env, "callgraph", min(60, timeout_s))
        status = MATRIX.wait_ready(client, "callgraph", started + timeout_s, logs)
        save(logs / "ready-status.json", status)
    except KeyboardInterrupt:
        outcome, error = "interrupted", "benchmark interrupted"
    except (TimeoutError, MATRIX.OSS.BudgetExceeded) as exception:
        outcome, error = "killed_by_timeout", str(exception)
    except Exception as exception:
        outcome, error = "crashed" if client.child.done.is_set() else "failed", str(exception)
    wall = time.monotonic() - started
    measurement = client.close()
    if measurement["guard_triggered"]:
        outcome = "killed_by_guard"
    elif measurement["counter_errors"]:
        outcome = "measurement_error"
    result = {**measurement, "outcome": outcome, "error": error, "wall_seconds": wall, "memory": memory,
              "store_bytes": store_bytes(folder / "storage"), "guard_gib": guard_gib, "timeout_seconds": timeout_s}
    save(logs / "measurement.json", result)
    if outcome == "interrupted":
        raise KeyboardInterrupt
    return result


def release_asset(system, machine):
    arch = "arm64" if machine.lower() in ("arm64", "aarch64") else "x64" if machine.lower() in ("amd64", "x86_64") else None
    if arch is None:
        raise ValueError(f"unsupported architecture: {machine}")
    return f"aft-{'win32' if system == 'windows' else 'darwin' if system == 'macos' else 'linux'}-{arch}" + (".exe" if system == "windows" else "")


def checksum_for(text, name):
    matches = [parts[0].lower() for line in text.splitlines() if len(parts := line.split()) == 2
               and parts[1].lstrip("*") == name and re.fullmatch(r"[0-9a-fA-F]{64}", parts[0])]
    if len(matches) != 1:
        raise ValueError(f"release checksum missing or ambiguous for {name}")
    return matches[0]


def download(work):
    work.mkdir(parents=True, exist_ok=True)
    (work / ".index-bench-work").touch()
    directory = work / "bin"
    directory.mkdir(exist_ok=True)
    name = release_asset(host_platform(), platform.machine())
    base = "https://github.com/cortexkit/aft/releases/download/v0.65.0/"
    for filename in ("checksums.sha256", name):
        print(f"download {base + filename}", flush=True)
        # curl uses the operating system's certificate store; the python.org
        # macOS build ships without one, so urllib fails TLS verification there.
        subprocess.run(["curl", "--fail", "--silent", "--show-error", "--location", "--max-time", "300",
                        "--output", str(directory / filename), base + filename], check=True)
    expected = checksum_for((directory / "checksums.sha256").read_text(), name)
    actual = MATRIX.sha256(directory / name)
    if actual != expected:
        raise ValueError(f"release checksum mismatch for {name}: expected {expected}, got {actual}")
    binary = directory / ("aft-0.65.0.exe" if os.name == "nt" else "aft-0.65.0")
    (directory / name).replace(binary)
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    save(directory / "release.json", {"tag": "v0.65.0", "asset": name, "url": base + name,
                                       "checksum_url": base + "checksums.sha256", "sha256": actual})
    return binary


def binary_identity(path):
    version = subprocess.check_output([str(path), "--version"], text=True, timeout=30).strip()
    return {"path": str(path), "reported_version": version, "binary_sha256": MATRIX.sha256(path)}


def plan(system, requested=None):
    if requested and len(set(requested)) != len(requested):
        raise ValueError("projects must not contain duplicates")
    projects = requested or (["neovim"] if system == "linux" else
                             ["vscode", "kubernetes", "elasticsearch", "linux"] + (["chromium"] if system == "windows" else []))
    if system == "macos" and "chromium" in projects:
        raise ValueError("Chromium is excluded on the rented Mac to preserve disk space")
    if system == "linux" and projects != ["neovim"]:
        raise ValueError("Linux mode is only the Neovim driving-logic proof; use index-matrix for big Linux projects")
    projects = [project for project in projects if project != "chromium"] + (["chromium"] if "chromium" in projects else [])
    return [(project, list(("0.65.0", "0.66-candidate") if number % 2 == 0 else ("0.66-candidate", "0.65.0")))
            for number, project in enumerate(projects)]


def write_tables(output, rows):
    output.mkdir(parents=True, exist_ok=True)
    with (output / "results.csv").open("w", newline="", encoding="utf-8") as stream:
        writer = csv.DictWriter(stream, fieldnames=FIELDS, extrasaction="ignore")
        writer.writeheader()
        for row in rows:
            writer.writerow({**row, "guard_triggered": ",".join(row.get("guard_triggered", []))})
    def gib(value):
        return "—" if value is None else f"{value / GIB:.3f} GiB"
    def seconds(value):
        return "—" if value is None else f"{value:.2f} s"
    lines = ["# Cold text + call graph", "", "Peak is AFT-only RSS / working set; commit is also shown for Windows.", "",
             "| Project | Version | Peak memory | Peak commit | Wall | CPU | Outcome |",
             "| --- | --- | ---: | ---: | ---: | ---: | --- |"]
    lines += [f"| {row['project']} | {row['version']} | {gib(row.get('peak_memory_bytes'))} | "
              f"{gib(row.get('peak_commit_bytes'))} | {seconds(row.get('wall_seconds'))} | "
              f"{seconds(row.get('cpu_seconds'))} | {row['outcome']} |" for row in rows]
    (output / "results.md").write_text("\n".join(lines) + "\n", encoding="utf-8")


def run(args):
    system = host_platform()
    if not args.memory_profiles and args.baseline is None:
        raise ValueError("--baseline is required without --memory-profiles")
    work, output = args.work.resolve(), args.output.resolve()
    work.mkdir(parents=True, exist_ok=True)
    (work / ".index-bench-work").touch()
    output.mkdir(parents=True, exist_ok=True)
    if (output / "results.csv").exists():
        raise ValueError("use a fresh results directory for each comparison")
    binaries = ({mode: args.candidate.resolve() for mode in ("lean", "balanced", "fast")}
                if args.memory_profiles else
                {"0.65.0": args.baseline.resolve(), "0.66-candidate": args.candidate.resolve()})
    identities = {name: binary_identity(binary) for name, binary in binaries.items()}
    if not args.memory_profiles and not re.search(r"\b0\.65\.0\b", identities["0.65.0"]["reported_version"]):
        raise ValueError("baseline does not report version 0.65.0")
    release = work / "bin/release.json"
    if not args.memory_profiles and (not release.exists() or MATRIX.sha256(binaries["0.65.0"]) != json.loads(release.read_text())["sha256"]):
        raise ValueError("baseline must match the checksum-verified download in --work/bin")
    if not args.memory_profiles:
        shutil.copy2(release, output / "release.json")
        shutil.copy2(work / "bin/checksums.sha256", output / "release-checksums.sha256")
    entries = {entry["repository"]: entry for entry in json.loads((MATRIX_DIR / "manifest.json").read_text())["repositories"]}
    selected = ([("vscode", ["lean", "balanced", "fast"])] if args.memory_profiles
                else plan(system, args.projects))
    if args.memory_profiles and args.projects != ["vscode"]:
        raise ValueError("memory-profile measurement requires --projects vscode")
    guard_gib = {"windows": 26, "macos": 13, "linux": 13}[system]
    save(output / "identity.json", {"binaries": identities, "source_commit": subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=HERE, text=True).strip(), "host": platform.uname()._asdict(),
        "logical_cpus": os.cpu_count(), "python": sys.version, "runner_name": os.environ.get("RUNNER_NAME"),
        "run_id": os.environ.get("GITHUB_RUN_ID"), "guard_gib": guard_gib,
        "plan": selected, "free_disk_bytes": shutil.disk_usage(work).free,
        "manifest_sha256": MATRIX.sha256(MATRIX_DIR / "manifest.json"),
        "harness_sha256": {name: MATRIX.sha256(HERE / name) for name in ("run.py", "native.py")}})
    save(output / "pins.json", [entries[PROJECTS[project]] for project, _ in selected])
    rows = []
    write_tables(output, rows)
    for project, versions in selected:
        logs = output / project
        logs.mkdir()
        entry = entries[PROJECTS[project]]
        root = work / "checkout"
        try:
            excluded = checkout(entry, root, logs, windows=system == "windows")
        except Exception as error:
            for version in versions:
                rows.append({"project": project, "version": version, **identities[version], "revision": entry["commit"],
                             "platform": system, "outcome": "checkout_failed", "error": str(error)})
            write_tables(output, rows)
            remove(root)
            continue
        try:
            for version in versions:
                row = {"project": project, "version": version, **identities[version], "revision": entry["commit"],
                       "platform": system, "excluded_files": excluded}
                if version == "0.65.0" and project not in BASELINE[system]:
                    row["outcome"] = "skipped: exceeds machine"
                else:
                    print(f"start {project} {version}", flush=True)
                    folder = work / "run"
                    try:
                        row.update(measure(binaries[version], root, folder, logs / version, guard_gib,
                                           5400 if project == "chromium" else 2700,
                                           version if args.memory_profiles else None))
                    except Exception as error:
                        row.update(outcome="failed", error=str(error))
                    finally:
                        remove(folder)
                rows.append(row)
                write_tables(output, rows)
                print(f"{project} {version}: {row['outcome']}", flush=True)
        finally:
            remove(root)
    return 0 if all(row["outcome"] in ("ready", "skipped: exceeds machine") for row in rows) else 1


def main():
    def interrupt(*_):
        raise KeyboardInterrupt
    for name in ("SIGTERM", "SIGBREAK"):
        if hasattr(signal, name):
            signal.signal(getattr(signal, name), interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("download", "cleanup", "run"):
        child = sub.add_parser(command)
        child.add_argument("--work", type=Path, required=True)
        if command == "run":
            child.add_argument("--output", type=Path, required=True)
            child.add_argument("--baseline", type=Path)
            child.add_argument("--candidate", type=Path, required=True)
            child.add_argument("--projects", nargs="+", choices=PROJECTS)
            child.add_argument("--memory-profiles", action="store_true")
    args = parser.parse_args()
    if args.command == "download":
        download(args.work.resolve())
        return 0
    if args.command == "cleanup":
        if args.work.exists() and not (args.work / ".index-bench-work").is_file():
            raise ValueError(f"refusing to delete an unmarked work directory: {args.work}")
        remove(args.work)
        return 0
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
