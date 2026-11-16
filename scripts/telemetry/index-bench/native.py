"""Child-process measurements; no helper process is included in the AFT peak."""
from __future__ import annotations

import ctypes
import os
import signal
from pathlib import Path
import subprocess
import sys
import threading
import time

GIB = 1024 ** 3


def ps_cpu_seconds(text):
    """Parse ``ps -o time=`` ([[dd-]hh:]mm:ss[.ss]) into seconds."""
    text = text.strip()
    days = 0
    if "-" in text:
        day_text, text = text.split("-", 1)
        days = int(day_text)
    seconds = 0.0
    for part in text.split(":"):
        seconds = seconds * 60 + float(part)
    return days * 86400 + seconds


def guard_metrics(platform, rss, commit, limit):
    """Commit can exhaust Windows before its resident working set reaches the cap."""
    return [name for name, value in (("peak_working_set_bytes" if platform == "windows" else "rss_bytes", rss),
                                     ("peak_commit_bytes", commit)) if value is not None and value >= limit]


class WindowsCounters:
    def __init__(self, pid):
        from ctypes import wintypes

        class Memory(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
                (name, ctypes.c_size_t) for name in ("PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage", "PagefileUsage",
                "PeakPagefileUsage", "PrivateUsage")]

        self.Memory = Memory
        windll = getattr(ctypes, "WinDLL")
        self.win_error = getattr(ctypes, "WinError")
        self.last_error = getattr(ctypes, "get_last_error")
        self.kernel = windll("kernel32", use_last_error=True)
        self.psapi = windll("psapi", use_last_error=True)
        self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        self.kernel.OpenProcess.restype = wintypes.HANDLE
        self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        self.psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
        self.kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
        self.FileTime = wintypes.FILETIME
        self.handle = self.kernel.OpenProcess(0x0400 | 0x0010, False, pid)
        if not self.handle:
            raise self.win_error(self.last_error())

    def sample(self):
        value = self.Memory()
        value.cb = ctypes.sizeof(value)
        if not self.psapi.GetProcessMemoryInfo(self.handle, ctypes.byref(value), value.cb):
            raise self.win_error(self.last_error())
        return value.PeakWorkingSetSize, value.PeakPagefileUsage

    def cpu(self):
        creation, exit_time, kernel, user = [self.FileTime() for _ in range(4)]
        if not self.kernel.GetProcessTimes(self.handle, *[ctypes.byref(v) for v in (creation, exit_time, kernel, user)]):
            raise self.win_error(self.last_error())
        return sum((v.dwHighDateTime << 32) | v.dwLowDateTime for v in (kernel, user)) / 10_000_000

    def close(self):
        self.kernel.CloseHandle(self.handle)


class Child:
    """Only the waiter reaps POSIX children, preserving wait4's per-child rusage."""
    def __init__(self, command, cwd, env, stderr, guard_gib, timeline=None):
        self.platform = "windows" if os.name == "nt" else "macos" if sys.platform == "darwin" else "linux"
        self.started = time.monotonic()
        self.started_wall = time.time()
        self.proc = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=stderr, text=True, encoding="utf-8", bufsize=1)
        assert self.proc.stdout is not None and self.proc.stdin is not None
        self.stdout, self.stdin = self.proc.stdout, self.proc.stdin
        self.done = threading.Event()
        self.stopping = threading.Event()
        self.peak_rss = 0
        self.peak_commit = None
        self.cpu_seconds = None
        self.returncode = None
        self.guard_triggered = []
        self.guard_peak = None
        self.gaps = []
        # With a timeline path, one CSV row per second records RSS, process CPU seconds and
        # (on Linux) the thread count, so wall time can be split into busy and idle stretches.
        self.timeline = Path(timeline).open("w", encoding="utf-8") if timeline else None
        self.last_row = -1.0
        if self.timeline:
            self.timeline.write("seconds,rss_bytes,cpu_seconds,threads\n")
        try:
            self.windows = WindowsCounters(self.proc.pid) if self.platform == "windows" else None
        except BaseException:
            self.proc.kill()
            self.proc.wait()
            raise
        self.limit = guard_gib * GIB
        self.waiter = threading.Thread(target=self._wait, daemon=True)
        self.sampler = threading.Thread(target=self._monitor, daemon=True)
        self.waiter.start()
        self.sampler.start()

    def _sample(self):
        cpu = threads = None
        if self.windows:
            rss, commit = self.windows.sample()
            if self.timeline:
                cpu = self.windows.cpu()
        elif self.platform == "linux":
            fields = dict(line.split(":", 1) for line in Path(f"/proc/{self.proc.pid}/status").read_text().splitlines() if ":" in line)
            rss = max(int(fields.get(name, "0 kB").split()[0]) for name in ("VmRSS", "VmHWM")) * 1024
            commit = None
            if self.timeline:
                stat = Path(f"/proc/{self.proc.pid}/stat").read_text().rsplit(")", 1)[1].split()
                cpu = (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
                threads = int(fields.get("Threads", "0").strip())
        else:
            value = subprocess.check_output(["ps", "-o", "rss=,time=", "-p", str(self.proc.pid)], text=True).split()
            if len(value) != 2:
                raise ValueError(f"unexpected ps output: {value}")
            rss, commit = int(value[0]) * 1024, None
            cpu = ps_cpu_seconds(value[1])
        self.peak_rss = max(self.peak_rss, rss)
        if commit is not None:
            self.peak_commit = max(self.peak_commit or 0, commit)
        seconds = time.monotonic() - self.started
        if self.timeline and seconds - self.last_row >= 1.0:
            self.last_row = seconds
            self.timeline.write(f"{seconds:.2f},{rss},{'' if cpu is None else f'{cpu:.2f}'},{'' if threads is None else threads}\n")
            self.timeline.flush()
        return rss, commit

    def kill(self):
        try:
            if self.platform == "windows":
                self.proc.kill()
            else:
                # Popen.kill() polls waitpid first and could steal wait4's rusage.
                os.kill(self.proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    def _monitor(self):
        while not self.done.is_set() and not self.stopping.is_set():
            try:
                rss, commit = self._sample()
                triggers = guard_metrics(self.platform, rss, commit, self.limit)
                if triggers:
                    self.guard_triggered = triggers
                    self.guard_peak = {"peak_memory_bytes": self.peak_rss, "peak_commit_bytes": self.peak_commit}
                    self.kill()
                    return
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                if not self.done.wait(0.05) and not self.stopping.is_set():
                    self.gaps.append(str(error))
                    # Do not leave an unguarded indexer running after a counter failure.
                    self.kill()
                    return
            self.done.wait(0.25)

    def _wait(self):
        if self.platform == "windows":
            self.returncode = self.proc.wait()
        else:
            _, status, usage = os.wait4(self.proc.pid, 0)
            self.returncode = os.waitstatus_to_exitcode(status)
            self.proc.returncode = self.returncode
            self.usage = usage
        self.exited = time.monotonic()
        self.done.set()

    def finish(self):
        self.stopping.set()
        if not self.done.is_set():
            self.kill()
        self.waiter.join()
        self.sampler.join()
        if self.windows:
            try:
                self._sample()
            except OSError as error:
                self.gaps.append(str(error))
            try:
                self.cpu_seconds = self.windows.cpu()
            except OSError as error:
                self.gaps.append(str(error))
            finally:
                self.windows.close()
        else:
            # Darwin reports bytes, whereas Linux reports KiB.
            self.peak_rss = max(self.peak_rss, self.usage.ru_maxrss * (1 if self.platform == "macos" else 1024))
            self.cpu_seconds = self.usage.ru_utime + self.usage.ru_stime
        if self.timeline:
            self.timeline.close()
        if self.proc.stdin:
            self.proc.stdin.close()
        usage = getattr(self, "usage", None)
        rusage = None if usage is None else {
            "user_seconds": usage.ru_utime, "system_seconds": usage.ru_stime,
            "voluntary_context_switches": usage.ru_nvcsw, "involuntary_context_switches": usage.ru_nivcsw,
            "block_input_operations": usage.ru_inblock, "block_output_operations": usage.ru_oublock,
            "major_page_faults": usage.ru_majflt, "minor_page_faults": usage.ru_minflt}
        return {"peak_memory_bytes": self.peak_rss, "peak_commit_bytes": self.peak_commit,
                "cpu_seconds": self.cpu_seconds, "process_wall_seconds": self.exited - self.started,
                "started_unix_seconds": self.started_wall, "rusage": rusage,
                "exit_code": self.returncode, "guard_triggered": self.guard_triggered, "peak_at_guard": self.guard_peak,
                "counter_errors": self.gaps,
                "peak_source": "GetProcessMemoryInfo" if self.windows else "wait4.ru_maxrss"}
