#!/usr/bin/env python3
"""Portable NDJSON frame reading for the standalone-AFT stdin/stdout protocol.

Windows `select.select()` accepts sockets only, so polling a child-process pipe
with it fails with `WinError 10038` before the first response is ever read. A
background reader thread feeding a queue is the portable equivalent: it behaves
identically on every platform and needs no platform branch.

This lives in its own module because every harness stage runs as its own
`__main__` process, so each client class has to get the fix from a shared
import rather than from a patch applied to one caller.
"""

from __future__ import annotations

import json
import os
import queue
import threading
import time
from typing import IO, Any, Dict, Optional

JsonObject = Dict[str, Any]

READ_CHUNK_BYTES = 65536


class NdjsonStream:
    """Parsed NDJSON frames from a child-process pipe, drained off a thread."""

    def __init__(self, stream: IO[bytes]) -> None:
        self._fileno = stream.fileno()
        self._chunks: "queue.Queue[Optional[bytes]]" = queue.Queue()
        self._buffer = b""
        self._eof = False
        self._thread = threading.Thread(target=self._drain, daemon=True)
        self._thread.start()

    def read_frame(self, timeout: float) -> Optional[JsonObject]:
        """The next JSON object, or None if none completed within `timeout`.

        Lines that are not JSON objects are skipped: the transport only carries
        frames, and a stray line is never the response a caller waits for.
        """
        deadline = time.monotonic() + max(0.0, timeout)
        while True:
            frame = self._next_buffered_frame()
            if frame is not None:
                return frame
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self._pull(remaining):
                return None

    def _drain(self) -> None:
        """Read the pipe until it closes; the queue is the only handoff."""
        while True:
            try:
                chunk = os.read(self._fileno, READ_CHUNK_BYTES)
            except (OSError, ValueError):
                chunk = b""
            if not chunk:
                self._chunks.put(None)
                return
            self._chunks.put(chunk)

    def _next_buffered_frame(self) -> Optional[JsonObject]:
        while b"\n" in self._buffer:
            line, self._buffer = self._buffer.split(b"\n", 1)
            line = line.strip()
            if not line:
                continue
            try:
                frame = json.loads(line.decode("utf-8", errors="replace"))
            except json.JSONDecodeError:
                continue
            if isinstance(frame, dict):
                return frame
        return None

    def _pull(self, timeout: float) -> bool:
        """Append one queued chunk; False on timeout or once the pipe is closed."""
        if self._eof:
            return False
        try:
            chunk = self._chunks.get(timeout=max(0.0, timeout))
        except queue.Empty:
            return False
        if chunk is None:
            self._eof = True
            return False
        self._buffer += chunk
        return True
