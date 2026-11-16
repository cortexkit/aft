"""Optional profiling for a cold-index measurement: CPU timeline, stage changes and macOS stack samples.

Enabled with ``run.py run --profile``. Every artifact stays small: the timeline is one CSV
row per second, stage changes are only the moments a status field changes, and each stack
sample is gzip-compressed text from macOS ``sample``.
"""
from __future__ import annotations

import gzip
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import threading
import time

UNRESOLVED = re.compile(r"\?\?\?  \(in (?P<image>[^)]+)\)  load address (?P<load>0x[0-9a-f]+) \+ 0x[0-9a-f]+  \[(?P<address>0x[0-9a-f]+)\]")


def status_fields(response, prefix=""):
    """Flatten the string and boolean fields of the status subsystems that describe build stages."""
    fields = {}
    for key, value in response.items():
        name = f"{prefix}{key}"
        if isinstance(value, dict):
            fields.update(status_fields(value, name + "."))
        elif isinstance(value, (str, bool)) or value is None:
            fields[name] = value
    return fields


def stage_changes(status_path, started_ns, subsystems=("search_index", "callgraph_store")):
    """Each moment a status field under the named subsystems changes, in seconds since the AFT start.

    Counters are left out on purpose: they change every poll and would make the list as long
    as the status log. A field that appears or disappears counts as a change.
    """
    changes, previous = [], {}
    with Path(status_path).open(encoding="utf-8") as handle:
        for line in handle:
            record = json.loads(line)
            response = record["response"]
            current = {}
            for subsystem in subsystems:
                if isinstance(response.get(subsystem), dict):
                    current.update(status_fields(response[subsystem], subsystem + "."))
            seconds = round((record["monotonic_ns"] - started_ns) / 1e9, 2)
            for key in sorted(set(previous) | set(current)):
                if previous.get(key, "<absent>") != current.get(key, "<absent>"):
                    changes.append({"seconds": seconds, "field": key, "value": current.get(key, "<absent>")})
            previous = current
    return changes


def symbolize(text, images, run=subprocess.run):
    """Resolve frames that ``sample`` left as ``???`` with ``atos`` against each image's dSYM.

    ``sample`` already resolves frames when a ``<binary>.dSYM`` sits next to the binary; this is
    the fallback when it does not. ``images`` maps the image name sample prints to its DWARF file.
    """
    wanted = {}
    for match in UNRESOLVED.finditer(text):
        if match["image"] in images:
            wanted.setdefault((match["image"], match["load"]), set()).add(match["address"])
    names = {}
    for (image, load), addresses in wanted.items():
        ordered = sorted(addresses)
        for start in range(0, len(ordered), 2000):
            batch = ordered[start:start + 2000]
            output = run(["atos", "-o", str(images[image]), "-l", load, *batch],
                         check=True, capture_output=True, text=True).stdout.splitlines()
            for address, name in zip(batch, output):
                if not name.startswith("0x"):
                    names[(image, address)] = name.split(" (in ")[0]
    def replace(match):
        name = names.get((match["image"], match["address"]))
        return match[0] if name is None else f"{name}  (in {match['image']})  [{match['address']}]"
    return UNRESOLVED.sub(replace, text), len(names)


class Sampler:
    """Runs macOS ``sample`` on the AFT process at fixed offsets from its start.

    Each sample suspends the process briefly at every tick, so the interval is coarse (10 ms)
    and the window short; the same schedule runs for both versions so they pay the same cost.
    """
    def __init__(self, pid, folder, started, every_s=15.0, first_s=5.0, window_s=3, interval_ms=10, dwarf=None):
        self.pid, self.folder, self.started = pid, Path(folder), started
        self.every_s, self.first_s, self.window_s, self.interval_ms = every_s, first_s, window_s, interval_ms
        self.dwarf = dwarf
        self.stopping = threading.Event()
        self.records = []
        self.thread = threading.Thread(target=self._loop, daemon=True)
        self.available = sys.platform == "darwin" and shutil.which("sample") is not None

    def start(self):
        if self.available:
            self.folder.mkdir(parents=True, exist_ok=True)
            self.thread.start()
        return self

    def _loop(self):
        offset = self.first_s
        while not self.stopping.wait(max(0.0, self.started + offset - time.monotonic())):
            path = self.folder / f"sample-{int(offset):04d}s.txt"
            begun = time.monotonic() - self.started
            # sample resolves symbols on the CPU after each window; nice keeps that
            # work from taking cores away from the build it measures.
            result = subprocess.run(["nice", "-n", "15", "sample", str(self.pid), str(self.window_s), str(self.interval_ms),
                                     "-mayDie", "-file", str(path)], capture_output=True, text=True)
            record = {"offset_seconds": round(begun, 2), "exit_code": result.returncode,
                      "ended_seconds": round(time.monotonic() - self.started, 2)}
            if path.exists():
                text = path.read_text(encoding="utf-8", errors="replace")
                if self.dwarf:
                    try:
                        text, resolved = symbolize(text, self.dwarf)
                        record["atos_resolved"] = resolved
                    except (OSError, subprocess.SubprocessError) as error:
                        record["atos_error"] = str(error)
                record["unresolved_aft_frames"] = sum(1 for match in UNRESOLVED.finditer(text) if match["image"] in (self.dwarf or {}))
                with gzip.open(path.with_suffix(".txt.gz"), "wt", encoding="utf-8") as stream:
                    stream.write(text)
                path.unlink()
            else:
                record["stderr"] = result.stderr[-400:]
            self.records.append(record)
            if result.returncode != 0 and not path.with_suffix(".txt.gz").exists():
                break
            offset += self.every_s

    def stop(self):
        self.stopping.set()
        if self.thread.is_alive():
            self.thread.join()
        return self.records
