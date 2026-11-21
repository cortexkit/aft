#!/usr/bin/env python3
"""Hold protocol and Mac isolation controls; never touch the real shared folders."""

from __future__ import annotations

import importlib.util
import json
import os
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import call, patch

SCRIPT_DIR = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("mac_cost_gate_test", SCRIPT_DIR / "mac-cost-gate.py")
assert spec is not None and spec.loader is not None
mac = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mac)
ci_spec = importlib.util.spec_from_file_location("ci_mac_hold_test", SCRIPT_DIR.parent / "ci/macos-hold.py")
assert ci_spec is not None and ci_spec.loader is not None
ci = importlib.util.module_from_spec(ci_spec)
ci_spec.loader.exec_module(ci)
NOW = datetime(2026, 11, 20, 3, 5, tzinfo=timezone.utc)
END = NOW.replace(hour=5, minute=0)


@patch.dict(os.environ, {"GITHUB_RUN_ID": "1001"})
class HoldTests(unittest.TestCase):
    def test_owner_is_per_github_run_and_rejects_a_previous_runs_stale_grant(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = mac.Hold(root, END, root)
            first.request()
            mac.write_json(root / "granted.json", first.identity)
            self.assertEqual(first.identity["owner"], "aft-nightly-1001")
            first.release()
            with patch.dict(os.environ, {"GITHUB_RUN_ID": "1002"}):
                second = mac.Hold(root, END, root)
            second.request()
            self.assertEqual(second.identity["owner"], "aft-nightly-1002")
            self.assertFalse(second.granted(), "a leftover grant with a different run owner must never admit measurement")
            mac.write_json(root / "granted.json", second.identity)
            self.assertTrue(second.granted())
            second.release()

    def test_owner_has_valid_characters_and_at_most_64_characters(self):
        self.assertEqual(len(mac.hold_owner("9" * 52)), 64)
        for invalid in (None, "", "9" * 53, "UPPERCASE", "space here", "bad/path", "123_456"):
            with self.subTest(run_id=invalid), self.assertRaises(ValueError):
                mac.hold_owner(invalid)

    def test_hold_is_a_bounded_regular_record_published_by_same_folder_rename(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            hold = mac.Hold(root, END, root)
            replace = Path.replace

            def publish(source, destination):
                self.assertEqual(source.parent, root)
                self.assertNotEqual(source.name, "hold.json")
                self.assertFalse(destination.exists(), "no partial hold may be visible before the rename")
                self.assertTrue(source.is_file())
                self.assertFalse(source.is_symlink())
                self.assertLessEqual(source.stat().st_size, 4096)
                self.assertEqual(set(json.loads(source.read_text())), {"owner", "expires_at"})
                return replace(source, destination)

            with patch.object(Path, "replace", autospec=True, side_effect=publish) as rename:
                hold.request()
            rename.assert_called_once()
            self.assertEqual(mac.read_hold_record(root / "hold.json"), hold.identity)
            self.assertEqual([path.name for path in root.iterdir()], ["hold.json"])
            hold.release()

    def test_grant_format_is_exact_regular_bounded_and_utc(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "granted.json"
            hold = mac.Hold(root, END, root)
            for expiry in ("2026-11-20T05:00:00Z", "2026-11-20T05:00:00+00:00"):
                mac.write_json(path, hold.identity | {"expires_at": expiry})
                self.assertTrue(hold.granted())
            for invalid in ([], {}, hold.identity | {"extra": True},
                            hold.identity | {"owner": "invalid_owner"},
                            hold.identity | {"owner": "x" * 65},
                            hold.identity | {"expires_at": "2026-11-20T05:00:00+01:00"},
                            hold.identity | {"expires_at": "2026-11-20T05:00:00"},
                            hold.identity | {"expires_at": "2026-11-31T05:00:00Z"}):
                mac.write_json(path, invalid)
                self.assertFalse(hold.granted(), invalid)
            path.write_text('{"owner":"aft-nightly-1001","owner":"aft-nightly-1001","expires_at":"2026-11-20T05:00:00Z"}')
            self.assertFalse(hold.granted(), "duplicate keys are not an exact two-field record")
            contents = json.dumps(hold.identity).encode()
            path.write_bytes(contents + b" " * (4096 - len(contents)))
            self.assertTrue(hold.granted(), "the 4 KiB limit is inclusive")
            path.write_bytes(contents + b" " * (4097 - len(contents)))
            self.assertFalse(hold.granted(), "oversized records are refused")
            path.unlink()
            target = root / "real-record.json"
            mac.write_json(target, hold.identity)
            path.symlink_to(target)
            self.assertFalse(hold.granted(), "a grant symlink is not a regular record")
            path.unlink()
            path.mkdir()
            self.assertFalse(hold.granted(), "a directory is not a regular record")

    def test_distinct_hold_and_grant_paths_are_named(self):
        self.assertEqual(mac.HOLD_DIR, Path("/Users/Shared/motor-macos-hold"))
        self.assertEqual(mac.GRANT_DIR, Path("/Users/Shared/motor-macos-grant"))

    def test_wait_requires_matching_owner_and_expiry_in_grant_folder(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            holds, grants = root / "hold", root / "grant"
            holds.mkdir()
            grants.mkdir()
            hold = mac.Hold(holds, END, grants)
            hold.request()
            self.assertEqual(json.loads((holds / "hold.json").read_text()), {
                "owner": "aft-nightly-1001", "expires_at": "2026-11-20T05:00:00Z"})
            (holds / "granted.json").write_text(json.dumps(hold.identity))
            self.assertFalse(hold.granted(), "the old co-located grant must not admit a run")
            for bad in ({}, {**hold.identity, "owner": "other"},
                        {**hold.identity, "expires_at": "2026-11-21T05:00:00Z"}):
                mac.write_json(grants / "granted.json", bad)
                self.assertFalse(hold.granted())
            mac.write_json(grants / "granted.json", hold.identity | {"diagnostic": "drained"})
            self.assertFalse(hold.granted(), "MOTOR records have exactly two fields")
            mac.write_json(grants / "granted.json", hold.identity)
            with patch.object(mac, "utcnow", return_value=NOW), patch.object(mac.time, "sleep") as sleep:
                self.assertTrue(hold.wait())
            sleep.assert_not_called()
            hold.release()
            self.assertFalse((holds / "hold.json").exists())

    def test_no_grant_waits_to_window_end_not_a_short_drain_timeout(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            hold = mac.Hold(root, END, root / "absent-grant")
            hold.request()
            times = [NOW, NOW, NOW + timedelta(minutes=46), NOW + timedelta(minutes=46), END]
            with patch.object(mac, "utcnow", side_effect=times), patch.object(mac.time, "sleep") as sleep:
                self.assertFalse(hold.wait())
            self.assertEqual(sleep.call_count, 2)
            hold.release()
            self.assertFalse((root / "hold.json").exists())

    def test_existing_foreign_hold_is_never_overwritten_or_deleted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            mac.write_json(root / "hold.json", {"owner": "other"})
            hold = mac.Hold(root, END, root)
            with self.assertRaises(FileExistsError):
                hold.request()
            hold.release()
            self.assertEqual(mac.read_json(root / "hold.json"), {"owner": "other"})

    def test_window_and_conservative_duration_history(self):
        self.assertIsNone(mac.window_end(NOW.replace(hour=2)))
        self.assertIsNone(mac.window_end(END))
        self.assertEqual(mac.window_end(NOW), END)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "durations.json"
            self.assertEqual(mac.expected_duration(path), 1800)
            mac.write_json(path, [9999, 1200, 1600, 1300])
            self.assertEqual(mac.expected_duration(path), 1600)
        with patch.object(mac, "utcnow", return_value=END - timedelta(minutes=39)):
            self.assertFalse(mac.enough_time(END, 1800))
        with patch.object(mac, "utcnow", return_value=END - timedelta(minutes=40)):
            self.assertTrue(mac.enough_time(END, 1800))


class CIHoldTests(unittest.TestCase):
    env = {"GITHUB_RUN_ID": "1001", "GITHUB_RUN_ATTEMPT": "2", "GITHUB_JOB": "unit-macos-libtest-shard"}
    now = NOW.replace(hour=14)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.holds, self.grants = root / "hold", root / "grant"
        self.holds.mkdir()
        self.grants.mkdir()
        self.hold_path = self.holds / "hold.json"
        self.grant_path = self.grants / "granted.json"
        self.owner = "aft-tests-1001-2-unit-macos-libtest-shard-3"
        self.initial_record = {"owner": self.owner, "expires_at": "2026-11-20T17:05:00Z"}

    def test_job_owner_differs_across_runs_attempts_jobs_and_matrix_shards(self):
        self.assertEqual(ci.job_owner(self.env, "3"), self.owner)
        for key, value in (("GITHUB_RUN_ID", "1002"), ("GITHUB_RUN_ATTEMPT", "3"), ("GITHUB_JOB", "other-job")):
            self.assertNotEqual(ci.job_owner(self.env | {key: value}, "3"), self.owner)
        self.assertNotEqual(ci.job_owner(self.env, "4"), self.owner)

    def test_acquire_waits_for_exact_grant(self):
        mac.write_json(self.grant_path, self.initial_record | {"owner": "other-job"})

        def grant_arrives(_delay):
            self.assertEqual(mac.read_hold_record(self.hold_path), self.initial_record)
            mac.write_json(self.grant_path, self.initial_record)

        with patch.object(ci.protocol, "utcnow", return_value=self.now), patch.object(ci.protocol.time, "sleep", side_effect=grant_arrives) as sleep:
            ci.acquire(self.owner, self.holds, self.grants)
        sleep.assert_called_once_with(30)
        self.assertEqual(mac.read_hold_record(self.hold_path), self.initial_record)

    def test_missing_or_foreign_grant_times_out_and_releases_request(self):
        for foreign_grant in (False, True):
            with self.subTest(foreign_grant=foreign_grant):
                if foreign_grant:
                    mac.write_json(self.grant_path, self.initial_record | {"owner": "other-job"})
                deadline = self.now + timedelta(minutes=50)
                near_deadline = deadline - timedelta(seconds=3)
                times = [self.now, self.now, self.now, near_deadline, near_deadline, deadline]
                with patch.object(ci.protocol, "utcnow", side_effect=times), patch.object(ci.protocol.time, "sleep") as sleep:
                    with self.assertRaisesRegex(RuntimeError, "Timed out waiting for MOTOR grant.*refusing to run"):
                        ci.acquire(self.owner, self.holds, self.grants)
                self.assertEqual(sleep.call_args_list, [call(30), call(3)])
                self.assertFalse(self.hold_path.exists())
                self.assertEqual(self.grant_path.exists(), foreign_grant)

    def test_refresh_keeps_owner_and_waits_for_new_expiry_before_old_expires(self):
        mac.write_json(self.hold_path, self.initial_record)
        mac.write_json(self.grant_path, self.initial_record)
        now = self.now + timedelta(hours=2, minutes=30)
        renewed = self.initial_record | {"expires_at": "2026-11-20T19:35:00Z"}

        def acknowledge_renewal(_delay):
            self.assertEqual(mac.read_hold_record(self.hold_path), renewed)
            self.assertEqual(mac.read_hold_record(self.grant_path), self.initial_record)
            mac.write_json(self.grant_path, renewed)

        with patch.object(ci.protocol, "utcnow", return_value=now), patch.object(ci.protocol.time, "sleep", side_effect=acknowledge_renewal) as sleep:
            ci.refresh(self.owner, self.holds, self.grants)
        sleep.assert_called_once_with(30)
        self.assertEqual(mac.read_hold_record(self.hold_path), renewed)

    def test_refresh_refuses_to_revive_expired_hold(self):
        mac.write_json(self.hold_path, self.initial_record)
        mac.write_json(self.grant_path, self.initial_record)
        with patch.object(ci.protocol, "utcnow", return_value=self.now + timedelta(hours=3)):
            with self.assertRaisesRegex(ValueError, "extend a live MOTOR hold"):
                ci.refresh(self.owner, self.holds, self.grants)
        self.assertEqual(mac.read_hold_record(self.hold_path), self.initial_record)

    def test_release_after_job_step_failure_only_removes_this_jobs_hold(self):
        mac.write_json(self.grant_path, self.initial_record)
        with patch.object(ci.protocol, "utcnow", return_value=self.now):
            ci.acquire(self.owner, self.holds, self.grants)
        with (
            patch.object(ci.protocol, "HOLD_DIR", self.holds),
            patch.dict(os.environ, self.env),
            patch.object(sys, "argv", ["macos-hold.py", "release", "--shard", "3"]),
        ):
            with self.assertRaisesRegex(RuntimeError, "job step failed"):
                try:
                    raise RuntimeError("job step failed")
                finally:
                    self.assertEqual(ci.main(), 0)
            self.assertFalse(self.hold_path.exists())
            self.assertEqual(mac.read_hold_record(self.grant_path), self.initial_record)

            foreign = self.initial_record | {"owner": "other-job"}
            mac.write_json(self.hold_path, foreign)
            self.assertEqual(ci.main(), 0)
            self.assertEqual(mac.read_hold_record(self.hold_path), foreign)


class ContaminationTests(unittest.TestCase):
    def test_vm_names_include_motor_backend_not_merely_the_harness(self):
        self.assertEqual(mac.vm_processes("/usr/bin/python3\n/usr/libexec/com.apple.Virtualization.VirtualMachine\n"),
                         ["/usr/libexec/com.apple.Virtualization.VirtualMachine"])
        self.assertEqual(mac.vm_processes("/opt/bin/vfkit\n/opt/bin/qemu-system-aarch64"),
                         ["/opt/bin/vfkit", "/opt/bin/qemu-system-aarch64"])
        self.assertEqual(mac.swap_used_bytes("total = 1024.00M used = 2.50M free = 1021.50M"), 2621440)
        with self.assertRaises(ValueError):
            mac.swap_used_bytes("unknown")

    def test_vm_swap_grant_loss_and_expiry_each_invalidate_measurement(self):
        hold = SimpleNamespace(end=END, granted=lambda: True)
        monitor = mac.IsolationMonitor(hold)
        with patch.object(mac, "utcnow", return_value=NOW), \
                patch.object(mac, "command_output", side_effect=["/usr/bin/python3", "used = 1.00M",
                                                               "com.apple.Virtualization.VirtualMachine", "used = 2.00M"]):
            monitor.sample()
            self.assertEqual(monitor.reasons, [])
            monitor.sample()
        self.assertTrue(any("job VM present" in reason for reason in monitor.reasons))
        self.assertTrue(any("swap grew" in reason for reason in monitor.reasons))
        hold.granted = lambda: False
        with patch.object(mac, "utcnow", return_value=END), \
                patch.object(mac, "command_output", side_effect=["", "used = 1.00M"]):
            monitor.sample()
        self.assertIn("MOTOR grant disappeared or changed during measurement", monitor.reasons)
        self.assertIn("hold window expired during measurement", monitor.reasons)

    def test_unavailable_observation_is_not_judged_as_isolated(self):
        monitor = mac.IsolationMonitor(SimpleNamespace(end=END, granted=lambda: True))
        with patch.object(mac, "utcnow", return_value=NOW), \
                patch.object(mac, "command_output", side_effect=OSError("sysctl missing")):
            monitor.sample()
        self.assertIn("isolation observation unavailable", monitor.reasons[0])


class WrapperTests(unittest.TestCase):
    def exercise(self, root, *, grant=True, fail=False, contaminate=False, now=NOW, build_delay=False):
        root = root.resolve()
        script_dir = root / "checkout" / "scripts" / "telemetry"
        script_dir.mkdir(parents=True)
        baseline = {"schema": 1, "measured_on": {"platform": "linux-x86_64"}, "repos": {}}
        (script_dir / "cost-baselines.json").write_text(json.dumps(baseline))
        home = root / "nightly"
        binary = home / "target" / "release" / "aft"
        binary.parent.mkdir(parents=True)
        binary.write_text("fixture")
        binary.chmod(0o755)
        holds, grants, output = root / "holds", root / "grants", home / "results" / "run"
        holds.mkdir()
        grants.mkdir()
        order = []
        fake_gate = SimpleNamespace(
            REPO_ORDER=("hugo",), load_baseline=lambda _p: baseline,
            prepare_repositories=lambda *_a: order.append("prepare corpus"))
        args = SimpleNamespace(home=home, hold_dir=holds, grant_dir=grants, write_baseline=True, repo="all")
        clock = [now]

        def build(argv, **kwargs):
            self.assertEqual(argv[:2], ["cargo", "build"])
            self.assertEqual(kwargs["env"]["CARGO_TARGET_DIR"], str(home / "target"))
            self.assertFalse((holds / "hold.json").exists())
            order.append("build")
            if build_delay:
                clock[0] = END - timedelta(minutes=39)

        def admit(hold):
            order.append("await grant")
            if grant:
                mac.write_json(grants / "granted.json", hold.identity)
            return grant

        def measured(argv, log, end):
            self.assertTrue((holds / "hold.json").exists())
            self.assertEqual(mac.read_json(grants / "granted.json"), mac.read_json(holds / "hold.json"))
            self.assertIn("--prepared-only", argv)
            self.assertEqual(end, END)
            order.append("capture" if "--write-baseline" in argv else "verify")
            if fail:
                raise RuntimeError("fixture measurement failure")
            working = Path(argv[argv.index("--baseline") + 1])
            mac.write_json(working, {"measured_on": {"platform": "darwin-arm64"}})
            if contaminate:
                (grants / "granted.json").unlink()

        def output_for(argv):
            if argv[0] == "ps":
                return "/usr/bin/python3"
            if argv[0] == "sysctl":
                return "used = 0.00M"
            return "[Excluded] fixture" if argv[1] == "isexcluded" else ""

        with patch.object(mac, "SCRIPT_DIR", script_dir), \
                patch.object(mac, "utcnow", side_effect=lambda: clock[0]), \
                patch.object(mac, "gate_module", return_value=fake_gate), \
                patch.object(mac.subprocess, "run", side_effect=build), \
                patch.object(mac, "command_output", side_effect=output_for), \
                patch.object(mac.Hold, "wait", admit), \
                patch.object(mac, "machine_facts", return_value={"model": "Macmini-test", "cpu": "test CPU", "macos": "test", "power_source": "AC"}), \
                patch.object(mac, "measured_command", side_effect=measured), \
                patch.dict(os.environ, {"AFT_COST_GATE_RESULTS_DIR": str(output), "GITHUB_RUN_ID": "1001"}, clear=True):
            if fail:
                with self.assertRaisesRegex(RuntimeError, "fixture measurement failure"):
                    mac.run(args)
            else:
                self.assertEqual(mac.run(args), 0)
        self.assertFalse((holds / "hold.json").exists(), "hold must be released on every exit")
        self.assertTrue((home / ".metadata_never_index").exists())
        return order, output

    def test_preparation_precedes_hold_and_candidate_follows_independent_verification(self):
        with tempfile.TemporaryDirectory() as temporary:
            order, output = self.exercise(Path(temporary))
            self.assertEqual(order, ["build", "prepare corpus", "await grant", "capture", "verify"])
            self.assertEqual(mac.read_json(output / "night-status.json")["status"], "judged")
            candidate = mac.read_json(output / "cost-baselines.candidate.json")
            self.assertEqual(candidate["measured_on"]["platform"], "darwin-arm64")
            self.assertEqual(candidate["measured_on"]["machine"]["model"], "Macmini-test")

    def test_busy_machine_is_not_a_failure_and_never_measures(self):
        with tempfile.TemporaryDirectory() as temporary:
            order, output = self.exercise(Path(temporary), grant=False)
            self.assertEqual(order, ["build", "prepare corpus", "await grant"])
            self.assertEqual(mac.read_json(output / "night-status.json")["status"], "not run: machine busy")
            self.assertFalse((output / "cost-baselines.candidate.json").exists())

    def test_failure_releases_hold_and_does_not_publish_candidate(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, output = self.exercise(Path(temporary), fail=True)
            self.assertEqual(mac.read_json(output / "night-status.json")["status"], "failed: cost harness")
            self.assertFalse((output / "cost-baselines.candidate.json").exists())

    def test_lost_grant_marks_night_unjudged_even_if_measurements_passed(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, output = self.exercise(Path(temporary), contaminate=True)
            status = mac.read_json(output / "night-status.json")
            self.assertEqual(status["status"], "not judged: machine contaminated")
            self.assertIn("MOTOR grant disappeared or changed during measurement", status["reasons"])
            self.assertFalse((output / "cost-baselines.candidate.json").exists())

    def test_late_start_or_slow_preparation_never_requests_hold(self):
        for late, delay in ((True, False), (False, True)):
            with self.subTest(late=late), tempfile.TemporaryDirectory() as temporary:
                order, output = self.exercise(Path(temporary),
                                              now=END - timedelta(minutes=39) if late else NOW,
                                              build_delay=delay)
                self.assertNotIn("await grant", order)
                self.assertEqual(mac.read_json(output / "night-status.json")["status"], "not run: too late in the window")


if __name__ == "__main__":
    unittest.main(verbosity=2)
