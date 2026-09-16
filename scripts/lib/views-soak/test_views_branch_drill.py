#!/usr/bin/env python3
"""Fake-subject checks for views-off branch-drill row close conditions."""

from __future__ import annotations

import tempfile
import time
import unittest
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping
from unittest.mock import patch

import branch_drill
from common import ProcessSample, SwitchProbe


PROBE = SwitchProbe(path="src/foo.ts", symbol="alphaProbe", token="alphaProbe")
FAKE_PID = 4242


class FakeLegacyClient:
    """Standalone-shaped subject that answers probes and appends stderr on demand."""

    def __init__(
        self,
        *,
        log_path: Path,
        probe: SwitchProbe,
        refresh_delay_s: float | None,
    ) -> None:
        self.log_path = log_path
        self.probe = probe
        self.refresh_delay_s = refresh_delay_s
        self.correct_at: float | None = None
        self._emitted = False
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log_path.touch()

    def status(self, timeout_s: float = 300.0) -> dict[str, Any]:
        del timeout_s
        self._maybe_emit()
        return {
            "search_index": {"status": "ready"},
            "semantic_index": {
                "status": "ready",
                "refreshing_count": 0,
                "pending_paths": 0,
            },
            "callgraph_store": {"status": "ready"},
        }

    def tool(
        self,
        name: str,
        arguments: Mapping[str, Any],
        timeout_s: float = 240.0,
    ) -> dict[str, Any]:
        del arguments, timeout_s
        self._maybe_emit()
        if name == "search":
            return {
                "success": True,
                "status": "ready",
                "results": [{"name": self.probe.token}],
            }
        if name == "callgraph":
            if self.correct_at is None:
                self.correct_at = time.monotonic()
            return {"success": True, "text": f"caller of {self.probe.symbol}"}
        return {"success": True}

    def _maybe_emit(self) -> None:
        if self.refresh_delay_s is None or self._emitted or self.correct_at is None:
            return
        if time.monotonic() < self.correct_at + self.refresh_delay_s:
            return
        stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with self.log_path.open("a", encoding="utf-8") as handle:
            handle.write(
                f"{stamp} [aft] semantic refresh: 3 changed, 0 new, 0 deleted, 3 total processed\n"
            )
        self._emitted = True


class LegacyArmRowTests(unittest.TestCase):
    def _roots(self) -> tuple[Path, Path, Path]:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        checkout = root / "checkout"
        source = root / "source"
        storage = root / "storage"
        checkout.mkdir()
        source.mkdir()
        (source / ".cortexkit").mkdir()
        (source / ".cortexkit" / "aft.jsonc").write_text(
            '{"views": {"enabled": true}}\n', encoding="utf-8"
        )
        (storage / "logs").mkdir(parents=True)
        return checkout, source, storage

    def _run_legacy_row(self, client: FakeLegacyClient, checkout: Path, source: Path, storage: Path):
        origin = time.monotonic()

        def fake_git_text(_repo: Path, *args: str, **_kwargs: Any) -> str:
            return str(args[-1]) if args else "deadbeef"

        def fake_sample(_pid: int) -> ProcessSample:
            return ProcessSample(cpu_s=time.monotonic() - origin, rss_kib=2048)

        with patch.object(branch_drill, "git_text", fake_git_text), patch.object(
            branch_drill, "sample_process", fake_sample
        ):
            return branch_drill.perform_switch(
                mode="views-off",
                checkout=checkout,
                source_root=source,
                target_sha="abc123def",
                changed_files=10,
                label="HEAD→abc123def",
                probe=PROBE,
                client=client,
                views_on=False,
                view_dir=storage / "views",
                storage=storage,
                expected_manifest_fingerprint=None,
                standalone_pid=FAKE_PID,
                cold_work_timeout_s=20.0,
            )

    def test_legacy_row_waits_for_semantic_refresh_after_callgraph_correct(self) -> None:
        checkout, source, storage = self._roots()
        client = FakeLegacyClient(
            log_path=storage / "logs" / f"aft-{FAKE_PID}.log",
            probe=PROBE,
            refresh_delay_s=3.0,
        )
        row = self._run_legacy_row(client, checkout, source, storage)
        self.assertEqual(
            row["row_close_reason"],
            "semantic_refresh_complete",
            "legacy row must close on semantic refresh, not callgraph-correct alone",
        )
        self.assertEqual(row["correctness"], "correct")
        self.assertIsNotNone(row["time_to_correct_ms"])
        self.assertIsNotNone(row["semantic_settle_ms"])
        self.assertGreaterEqual(
            row["semantic_settle_ms"] - row["time_to_correct_ms"],
            2500,
            "row must stay open for the post-correctness semantic refresh interval",
        )
        self.assertGreaterEqual(
            row["cpu_s"],
            3.0,
            "CPU attribution must include the post-correctness semantic refresh interval",
        )

    def test_legacy_row_without_embeddable_change_closes_on_quiet_window(self) -> None:
        checkout, source, storage = self._roots()
        client = FakeLegacyClient(
            log_path=storage / "logs" / f"aft-{FAKE_PID}.log",
            probe=PROBE,
            refresh_delay_s=None,
        )
        with patch.object(branch_drill, "SEMANTIC_QUIET_WINDOW_S", 1.0):
            row = self._run_legacy_row(client, checkout, source, storage)
        self.assertEqual(
            row["row_close_reason"],
            "quiet_window_elapsed",
            "legacy row with no embeddable change must close on quiet-window elapse",
        )
        self.assertEqual(row["correctness"], "correct")
        self.assertIsNotNone(row["semantic_settle_ms"])
        self.assertGreaterEqual(row["semantic_settle_ms"], 900)
        self.assertFalse(client._emitted)


if __name__ == "__main__":
    unittest.main()
