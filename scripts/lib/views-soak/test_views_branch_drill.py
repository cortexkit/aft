#!/usr/bin/env python3
"""Fake AFT client checks for checkout publication after correct branch probes."""

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


class FakeCheckoutClient:
    """Standalone-shaped subject that answers probes and appends stderr on demand."""

    def __init__(
        self,
        *,
        log_path: Path,
        probe: SwitchProbe,
        checkout: Path,
        publication_delay_s: float,
    ) -> None:
        self.log_path = log_path
        self.probe = probe
        self.checkout = checkout
        self.publication_delay_s = publication_delay_s
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
        if self._emitted or self.correct_at is None:
            return
        if time.monotonic() < self.correct_at + self.publication_delay_s:
            return
        stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        with self.log_path.open("a", encoding="utf-8") as handle:
            handle.write(
                f"{stamp} index_event kind=view_publication plane=views root={self.checkout} "
                "outcome=no_op candidates=0 blob_puts=0 pending_paths=0\n"
            )
        self._emitted = True


class CheckoutRowTests(unittest.TestCase):
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
            '{"indexes": {"semantic": true}}\n', encoding="utf-8"
        )
        (storage / "logs").mkdir(parents=True)
        return checkout, source, storage

    def _run_checkout_row(self, client: FakeCheckoutClient, checkout: Path, source: Path, storage: Path):
        origin = time.monotonic()

        def fake_git_text(_repo: Path, *args: str, **_kwargs: Any) -> str:
            return str(args[-1]) if args else "deadbeef"

        def fake_sample(_pid: int) -> ProcessSample:
            return ProcessSample(cpu_s=time.monotonic() - origin, rss_kib=2048)

        with patch.object(branch_drill, "git_text", fake_git_text), patch.object(
            branch_drill, "sample_process", fake_sample
        ), patch.object(branch_drill, "current_generation", return_value="head"), patch.object(
            branch_drill, "manifest_fingerprint", return_value="head-fp"
        ):
            return branch_drill.perform_switch(
                mode="independent-clone",
                checkout=checkout,
                target_sha="abc123def",
                changed_files=10,
                label="HEAD→abc123def",
                probe=PROBE,
                client=client,
                view_dir=storage / "views",
                storage=storage,
                expected_manifest_fingerprint="head-fp",
                standalone_pid=FAKE_PID,
                cold_work_timeout_s=20.0,
            )

    def test_row_waits_for_publication_after_callgraph_correct(self) -> None:
        checkout, source, storage = self._roots()
        client = FakeCheckoutClient(
            log_path=storage / "logs" / f"aft-{FAKE_PID}.log",
            probe=PROBE,
            checkout=checkout,
            publication_delay_s=3.0,
        )
        row = self._run_checkout_row(client, checkout, source, storage)
        self.assertEqual(
            row["row_close_reason"],
            "pending_paths=0",
            "row must wait for publication, not callgraph-correct alone",
        )
        self.assertEqual(row["correctness"], "correct")
        self.assertIsNotNone(row["time_to_correct_ms"])
        self.assertGreaterEqual(
            row["cpu_s"],
            3.0,
            "CPU attribution must include the post-correctness publication interval",
        )

    def test_row_without_embeddable_change_closes_on_no_op_publication(self) -> None:
        checkout, source, storage = self._roots()
        client = FakeCheckoutClient(
            log_path=storage / "logs" / f"aft-{FAKE_PID}.log",
            probe=PROBE,
            checkout=checkout,
            publication_delay_s=0.0,
        )
        row = self._run_checkout_row(client, checkout, source, storage)
        self.assertEqual(
            row["row_close_reason"],
            "pending_paths=0",
            "a no-op publication still confirms that checkout work is complete",
        )
        self.assertEqual(row["correctness"], "correct")
        self.assertEqual(row["publication"], "no_op")
        self.assertEqual(row["embeds"], 0)


if __name__ == "__main__":
    unittest.main()
