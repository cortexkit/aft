#!/usr/bin/env python3
"""Report native shell/sandbox prerequisites without weakening the sandbox."""
import ctypes
import os
import platform
import shutil
import subprocess


def main():
    for shell in ("/bin/sh", "/bin/bash"):
        result = subprocess.run([shell, "-c", "echo classic-ok"], text=True, capture_output=True, timeout=10)
        print(f"{shell}: exit={result.returncode} stdout={result.stdout!r} stderr={result.stderr!r}")
        if result.returncode or result.stdout != "classic-ok\n":
            raise SystemExit(f"Shell prerequisite failed: {shell}")
    if platform.machine() not in ("x86_64", "aarch64", "arm64"):
        raise SystemExit("Unknown Landlock syscall ABI for this architecture; add an explicit probe")
    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(444, 0, 0, 1)
    error = ctypes.get_errno()
    print(f"landlock_create_ruleset(VERSION): ABI={abi} errno={error} ({os.strerror(error)})")
    if shutil.which("unshare"):
        result = subprocess.run(["unshare", "-Ur", "true"], text=True, capture_output=True, timeout=10)
        print(f"User namespace diagnostic: exit={result.returncode} stderr={result.stderr!r}")
    if abi < 1:
        raise SystemExit("Required Landlock ABI unavailable: fix the MOTOR kernel/LSM or syscall policy; do not disable AFT's sandbox")


if __name__ == "__main__":
    main()
