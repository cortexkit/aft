"""Own the full process group of a Linux exactness build or probe."""
from contextlib import contextmanager
import os
import signal
import subprocess
import sys
import traceback

_active = set()


def _stop(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=10)
    _active.discard(process)


def kill_children():
    for process in list(_active):
        _stop(process)


def terminal_failure():
    traceback.print_exc()
    try:
        kill_children()
    finally:
        # Do not let interpreter finalizers keep a failed remote job open.
        sys.stdout.flush()
        sys.stderr.flush()
        os._exit(1)


@contextmanager
def child_process(command, **kwargs):
    process = subprocess.Popen(command, start_new_session=True, **kwargs)
    _active.add(process)
    try:
        yield process
    finally:
        # A successful parent can leave a descendant holding the runner open.
        # The group belongs only to this invocation, even after its parent exits.
        _stop(process)
