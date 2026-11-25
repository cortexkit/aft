#!/usr/bin/env python3
"""Export bounded, allowlisted evidence from the private real-daemon fixture."""

import datetime as dt
import os
from pathlib import Path
import re
import stat
import sys


CAP = 128 * 1024
MAX_CASES = 16
MODULES = ("aft", "aft-test-scope-owner", "broca", "thalamus-harness", "claustrum")
TIMESTAMP = re.compile(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z) ")
PHASES = (
    "unit_test_db_fence_disabled", "unit_test_db_fence_enabled", "nonce_capture_start",
    "nonce_capture_done", "executable_source_captured", "storage_lease_start", "storage_lease_done",
    "executable_pin_start", "executable_pin_done", "path_start", "path_done", "rayon_start",
    "rayon_done", "logging_start", "logging_done", "ort_pin_start", "ort_pin_done",
    "context_start", "context_done", "module_runtime_start",
)
STARTUP = re.compile(
    r"\[aft-startup\] pid=([0-9]{1,10}) phase=(" + "|".join(PHASES)
    + r") elapsed_ms=([0-9]{1,10})$"
)
READINESS = re.compile(r"readiness: flipped ready after ([0-9]{1,10}) ms \(")
LOG_LEVEL = re.compile(r"\[aft\] (WARN|ERROR) ")
CASE_NAME = re.compile(r"[a-z0-9_]{1,100}")
CASE_PHASE = re.compile(
    r"([0-9]{13}) (test_start|test_exit [0-9]{1,3}|image_deploy_start|image_deploy_done|"
    r"daemon_start|daemon_connection_published|scope_owner_ready|daemon_route_accepted_aft)"
)
DAEMON_EVENT = re.compile(
    r"[0-9]{13} (?:ERROR|WARN|INFO|DEBUG|TRACE) [A-Za-z0-9_.-]{1,80}:[0-9]{1,8} "
    r"module=(?:aft|aft-test-scope-owner|broca|thalamus-harness|claustrum|-) "
    r"code=(?:module_warming|target_unavailable|module_timeout|module_rejected|unsupported_role_version|"
    r"unknown_module|scope_ended|scope_changed|-) "
    r"reason=(?:supervised_not_registered|declared_not_ready|waiting_for_previous_pid|module_rejected|protocol_none|-) "
    r"(?:configured module registered|module registered|route\.open accepted|route\.open refused|"
    r"route\.open refusing module: not serving|route\.open accepted again after module outage|"
    r"supervised module exited cleanly|supervised module exited abnormally \(crash\)|details omitted)"
)
PANIC = re.compile(
    r"^thread '.{1,180}' \([0-9]{1,10}\) panicked at "
    r"(?:[A-Za-z0-9_.\-/]{1,140}/)?(tool_provider_subc_e2e\.rs):([0-9]{1,6}):([0-9]{1,6}):"
)


def read_bounded(path):
    """Read only regular fixture files, never a link or an arbitrary device."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        return ""
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode):
            return ""
        if info.st_size <= CAP:
            return source.read(CAP).decode("utf-8", "replace")
        # Keep startup and the last failure, dropping partial lines at the cut.
        head = source.read(CAP // 2).rsplit(b"\n", 1)[0]
        source.seek(info.st_size - CAP // 2)
        tail = source.read(CAP // 2).partition(b"\n")[2]
        return (head + b"\n" + tail).decode("utf-8", "replace")


def write_log(path, rows):
    # Head/tail are retained in the same order. All callers provide only
    # normalized rows, never lines copied from raw child stderr.
    data = ("\n".join(rows) + "\n").encode()
    if len(data) > CAP:
        data = (data[:CAP // 2 - 32].rsplit(b"\n", 1)[0] + b"\n... records omitted ...\n"
                + data[-CAP // 2:].partition(b"\n")[2])
    path.write_bytes(data)


def test_stderr(raw):
    for line in raw.splitlines():
        if match := PANIC.match(line):
            yield "panic at {}:{}:{} (message omitted)".format(*match.groups())
        if "module_warming" in line:
            yield "route.open: module_warming"
        if "live=false" in line:
            yield "route.open: live=false"
        for phrase in ("embedded daemon did not publish its connection file",
                       "test scope owner did not register its workdir",
                       "embedded daemon exited during startup"):
            if phrase in line:
                yield phrase


def child_log(raw, module, stream):
    for line in raw.splitlines():
        stamp = TIMESTAMP.match(line)
        if not stamp:
            continue
        try:
            time = int(dt.datetime.fromisoformat(stamp[1].replace("Z", "+00:00")).timestamp() * 1000)
        except ValueError:
            continue
        message = line[stamp.end():]
        # Daemon capture adds a timestamp to AFT's own timestamped log lines.
        if inner := TIMESTAMP.match(message):
            message = message[inner.end():]
        if module == "aft" and (start := STARTUP.fullmatch(message)):
            pid, phase, elapsed = start.groups()
            yield time, f"aft_{phase} pid={pid} elapsed_ms={elapsed}"
        elif module == "aft" and message.startswith("[aft] INFO subc attach: authenticated to daemon via "):
            yield time, "aft_authenticated"
        elif module == "aft" and message == "[aft] INFO subc attach: registered (HelloAck received)":
            yield time, "aft_hello_ack"
        elif module == "aft" and (ready := READINESS.match(message.removeprefix("[aft] INFO "))):
            yield time, f"aft_ready readiness_ms={ready[1]}"
        elif level := LOG_LEVEL.match(message):
            yield time, f"{module}_{stream} {level[1]} (details omitted)"


def export_case(fixture, output):
    if fixture.is_symlink() or not (case := CASE_NAME.fullmatch(read_bounded(fixture / "case-name").strip())):
        return
    if not re.fullmatch(r"test\.[a-zA-Z0-9]{8}", fixture.name):
        return
    case_output = output / f"{case[0]}-{fixture.name[5:]}"
    case_output.mkdir(parents=True, exist_ok=True)
    timeline = []
    for line in read_bounded(fixture / "timeline").splitlines():
        if phase := CASE_PHASE.fullmatch(line):
            timeline.append((int(phase[1]), phase[2]))
    raw = read_bounded(fixture / "test.stderr")
    write_log(case_output / "test.stderr.log", [
        *test_stderr(raw), f"Captured {len(raw.splitlines())} input lines; other text omitted."
    ])
    # The in-process tracing Layer writes only fixed messages, source
    # sites and allowlisted fields, never the daemon's formatted output.
    daemon = [line for line in read_bounded(fixture / "daemon.log").splitlines()
              if DAEMON_EVENT.fullmatch(line)]
    write_log(case_output / "daemon.log", daemon or ["No daemon events recorded."])
    child_rows = []
    scratch = fixture / "tmp"
    for home in [] if scratch.is_symlink() else sorted(scratch.glob(".tmp*"))[:2]:
        logs = home / "logs"
        if home.is_symlink() or logs.is_symlink():
            continue
        for module in MODULES:
            for stream in ("stdout", "stderr"):
                raw = read_bounded(logs / f"{module}.{stream}.log")
                if not raw:
                    continue
                records = list(child_log(raw, module, stream))
                child_rows.append(f"{module}.{stream}: {len(raw.splitlines())} input lines; other text omitted.")
                child_rows.extend(f"{time} {message}" for time, message in records)
                timeline.extend(records)
    write_log(case_output / "modules.log", child_rows or ["No module output recorded."])
    if timeline:
        start = min(time for time, _ in timeline)
        write_log(case_output / "timeline.log", [
            f"{time} +{time - start}ms {message}" for time, message in sorted(timeline)
        ])
    else:
        write_log(case_output / "timeline.log", ["No timeline events recorded."])


if __name__ == "__main__":
    mode, source, out = sys.argv[1:]
    source, out = Path(source), Path(out)
    if mode == "case":
        export_case(source, out)
    elif mode == "remaining":
        for fixture in sorted(source.glob("test.*"))[:MAX_CASES]:
            export_case(fixture, out)
    else:
        raise SystemExit(f"invalid mode: {mode}")
