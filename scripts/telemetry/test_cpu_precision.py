#!/usr/bin/env python3
"""Focused checks for fractional CPU collection and reporting."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parent
MEASURED_PROFILE = "index-store-no-inspect-post-edit-refresh-v5"


def load_script(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / filename)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


matrix = load_script("oss_matrix_cpu_test", "oss-matrix.py")
gate = load_script("cost_gate_cpu_test", "cost-gate.py")


class CpuPrecisionTests(unittest.TestCase):
    def test_fractional_cpu_boundary_and_baseline_keep_fraction(self) -> None:
        baseline = {"metrics": {"cpu_seconds": {"value": 3.0, "tolerance_pct": 25.0, "absolute_floor": 1.0}}}

        def run(value: float):
            row = {"cpu_s": f"{value:.2f}", "cpu_precision": "0.01-second"}
            return gate.RunData(Path("fixture.csv"), "synthetic-24k", gate.row_metrics(row), {}, True, row)

        below = run(3.74)
        above = run(3.76)
        self.assertEqual(below.metrics["cpu_seconds"], 3.74)
        self.assertEqual(gate.compare_repo("synthetic-24k", baseline, [below]), [])
        failures = gate.compare_repo("synthetic-24k", baseline, [above])
        self.assertEqual([(failure.metric, failure.observed, failure.limit) for failure in failures], [
            ("cpu_seconds", 3.76, 3.75),
        ])
        writer_metrics = {metric: 0.0 for metric in gate.DEFAULT_METRICS}
        writer_runs = [
            gate.RunData(Path(f"writer-{index}.csv"), "synthetic-24k",
                         writer_metrics | {"cpu_seconds": observed.metrics["cpu_seconds"]}, {}, True,
                         observed.row | {"measurement_profile": MEASURED_PROFILE})
            for index, observed in enumerate((above, below), 1)
        ]
        written = gate.repo_metrics_from_runs("synthetic-24k", baseline, writer_runs)
        persisted = json.loads(json.dumps(written))
        self.assertEqual(persisted["cpu_seconds"]["value"], 3.74)
        line = gate.observation_line("synthetic-24k", [below, above])
        self.assertIn("cpu_seconds[precision=0.01-second]=3.74/3.76", line)

        process = SimpleNamespace(pid=4242, poll=lambda: None)
        legacy_sampler = matrix.ProcessSampler(process)
        with patch.object(matrix.sys, "platform", "darwin"), \
                patch.object(matrix.subprocess, "run", return_value=SimpleNamespace(stdout="100 00:00:04")):
            legacy_sampler._sample_once()
            legacy_precision = legacy_sampler.cpu_precision
        self.assertEqual(legacy_sampler.cpu_seconds, 4.0)
        self.assertEqual(legacy_precision, "whole-second")
        self.assertIn("cpu_precision", matrix.CSV_FIELDS)
        legacy_row = {"cpu_s": f"{legacy_sampler.cpu_seconds:.2f}", "cpu_precision": legacy_precision}
        legacy_run = gate.RunData(Path("legacy.csv"), "synthetic-24k", gate.row_metrics(legacy_row), {}, True, legacy_row)
        self.assertIn("cpu_seconds[precision=whole-second]=4", gate.observation_line("synthetic-24k", [legacy_run]))

    def test_proc_stat_parser_handles_spaces_and_parentheses_in_comm(self) -> None:
        fields = ["S"] + ["0"] * 10 + ["374", "0"]
        stat = f"4242 (worker pool (compile) task) {' '.join(fields)}"
        self.assertEqual(matrix.parse_proc_stat_cpu_seconds(stat, clock_ticks=100), 3.74)

    def test_final_sample_occurs_before_client_close(self) -> None:
        order: list[str] = []

        class Sampler:
            def stop(self) -> None:
                order.append("final sample")

        class Client:
            def close(self) -> None:
                order.append("client close")
                return None

        matrix.close_after_final_sample(Sampler(), Client())
        self.assertEqual(order, ["final sample", "client close"])

    def test_exit_race_is_a_named_gap_not_zero_cpu(self) -> None:
        process = SimpleNamespace(pid=4242, poll=lambda: None)
        sampler = matrix.ProcessSampler(process)

        def read_proc(path: Path, encoding: str = "utf-8") -> str:
            if path.name == "stat":
                raise FileNotFoundError(path)
            return ""

        ps_result = SimpleNamespace(stdout="100 00:00:04")
        with patch.object(matrix.sys, "platform", "linux"), \
                patch.object(matrix.subprocess, "run", return_value=ps_result), \
                patch.object(Path, "read_text", read_proc):
            sampler._sample_once()

        self.assertIsNone(sampler.cpu_seconds)
        self.assertIn("CPU measurement gap: /proc/<pid>/stat unavailable during sampling", sampler.gaps)



class BaselineProvenanceTests(unittest.TestCase):
    """A blessed baseline must say where it was measured."""

    def test_github_run_records_run_image_and_commit(self):
        env = {
            "GITHUB_RUN_ID": "39088711411",
            "GITHUB_WORKFLOW": "OSS index cost gate",
            "ImageOS": "ubuntu24",
            "ImageVersion": "20261012.334.1",
            "GITHUB_SHA": "44f7009c9460ea83fa6bc4d2920d8a1e6367ff8c",
        }
        with patch.dict("os.environ", env, clear=True):
            provenance = gate.write_baseline_provenance()
        for part in env.values():
            self.assertIn(part, provenance)

    def test_local_run_says_it_is_local(self):
        with patch.dict("os.environ", {}, clear=True):
            provenance = gate.write_baseline_provenance()
        self.assertTrue(provenance.startswith("local --write-baseline on "), provenance)


class ColdIndexMeasurementTests(unittest.TestCase):
    def test_measured_config_disables_inspect_without_serializing_indexes(self):
        captured = []
        phase_marks = []

        def configure(command, **kwargs):
            self.assertEqual(command, "configure")
            captured.append(json.loads(kwargs["config"][0]["doc"]))
            return {"success": False, "message": "stop after capturing configure"}

        client = SimpleNamespace(proc=object(), request=configure, close=lambda: None)
        sampler = SimpleNamespace(
            start=lambda: None, stop=lambda: None, gaps=[], peak_rss_kb=0,
            peak_rss_hwm_kb=0, hwm_available=False, cpu_seconds=None, cpu_precision="0.01-second", disk_write_bytes=None,
        )
        phases = SimpleNamespace(start=lambda: None, stop=lambda: None, gaps=[], samples=[],
                                 mark=lambda *states: phase_marks.append(states))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.object(matrix, "repo_is_git", return_value=True), \
                    patch.object(matrix, "repo_shape", return_value=(1, "Go (1)", [])), \
                    patch.object(matrix, "NdjsonClient", return_value=client), \
                    patch.object(matrix, "ProcessSampler", return_value=sampler), \
                    patch.object(matrix, "PhaseSampler", return_value=phases), \
                    patch.object(matrix, "census_metrics", return_value={}):
                row = matrix.run_repo(Path("aft"), SCRIPT_DIR, root, root / "storage", 30)
        self.assertEqual(captured, [{
            "indexes": {"trigram": True, "callgraph": True, "semantic": False},
            "inspect": {"enabled": False},
        }])
        self.assertEqual(phase_marks, [("building", "building")])
        self.assertEqual(row["measurement_profile"], MEASURED_PROFILE)
        self.assertIn("measurement_profile", matrix.CSV_FIELDS)

    def test_millisecond_logs_retain_phase_timestamps(self):
        line = "2026-10-18T08:02:17.561Z [aft] INFO index_event kind=build_started plane=search"
        expected = datetime(2026, 10, 18, 8, 2, 17, 561000, tzinfo=timezone.utc).timestamp()
        self.assertEqual(matrix.log_seconds(line), expected)
        timeline = matrix.build_timeline([line])
        self.assertEqual(timeline.stages, [(expected, {"callgraph": "idle", "search": "started"})])

    def test_whole_second_logs_keep_midpoint_estimate(self):
        line = "2026-10-18T08:02:17Z [aft] INFO index_event kind=build_started plane=search"
        expected = datetime(2026, 10, 18, 8, 2, 17, tzinfo=timezone.utc).timestamp() + 0.5
        self.assertEqual(matrix.log_seconds(line), expected)

    def test_legacy_profile_refuses_comparison_before_launch_even_if_platform_ignored(self):
        baseline = gate.self_test_baseline()
        args = SimpleNamespace(
            self_test=False, baseline=Path("baseline.json"), write_baseline_from=None,
            write_baseline=False, ignore_platform=True, repo="hugo", cache_dir=Path("."), results_dir=Path("."), aft_binary=None,
        )
        with patch.object(gate, "parse_args", return_value=args), \
                patch.object(gate, "load_baseline", return_value=baseline), \
                patch.object(gate, "ensure_release_binary", side_effect=AssertionError("legacy profile reached measurement")) as launch:
            self.assertEqual(gate.main(), 2)
        launch.assert_not_called()

    def test_new_profile_keeps_memory_regression_strict(self):
        baseline = {"measurement_profile": MEASURED_PROFILE, "metrics": {
            "peak_rss_mb": {"value": 177.0, "tolerance_pct": 20.0, "absolute_floor": 128.0},
        }}

        def run(value):
            return gate.RunData(Path("fixture.csv"), "hugo", {"peak_rss_mb": value}, {}, True, {})

        self.assertEqual(gate.compare_repo("hugo", baseline, [run(176.9), run(182.5)]), [])
        failures = gate.compare_repo("hugo", baseline, [run(220.0), run(225.0)])
        self.assertEqual([(failure.metric, failure.observed) for failure in failures], [("peak_rss_mb", 220.0)])

    def test_matching_profile_reaches_both_measurements_and_passes(self):
        baseline = gate.self_test_baseline()
        baseline["repos"]["hugo"].update({
            "measurement_profile": MEASURED_PROFILE,
            "metrics": {"peak_rss_mb": {"value": 177.0, "tolerance_pct": 20.0, "absolute_floor": 128.0}},
        })
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "aft"
            binary.write_bytes(b"fixture")
            args = SimpleNamespace(
                self_test=False, baseline=root / "baseline.json", write_baseline_from=None,
                write_baseline=False, ignore_platform=False, repo="hugo", cache_dir=root / "cache",
                results_dir=root / "results", aft_binary=None, budget_min=15,
            )
            run = gate.RunData(Path("fixture.csv"), "hugo", {"peak_rss_mb": 180.0}, {}, True,
                               {"measurement_profile": MEASURED_PROFILE})
            with patch.object(gate, "parse_args", return_value=args), \
                    patch.object(gate, "load_baseline", return_value=baseline), \
                    patch.object(gate, "current_platform", return_value="linux-x86_64"), \
                    patch.object(gate, "ensure_release_binary", return_value=binary), \
                    patch.object(gate, "prepare_repositories", return_value={}), \
                    patch.object(gate, "run_matrix_once", return_value=([run], root / "fixture.csv")) as measure:
                self.assertEqual(gate.main(), 0)
            self.assertEqual(measure.call_count, 2)

    def test_writers_require_measured_profiles_and_preserve_partial_migration(self):
        profile = MEASURED_PROFILE
        baseline = gate.self_test_baseline()
        unchanged_redox = json.loads(json.dumps(baseline["repos"]["redox"]))
        metrics = {name: 1.0 for name in gate.DEFAULT_METRICS}
        runs = {name: [gate.RunData(Path(f"fixture-{index}.csv"), name, metrics.copy(), {}, True,
                                  {"measurement_profile": profile}) for index in (1, 2)]
                for name in gate.REPO_ORDER}
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "baseline.json"
            binary = root / "aft"
            binary.write_bytes(b"binary fixture")
            with patch.object(gate, "run_command", return_value=SimpleNamespace(stdout="a" * 40)):
                gate.write_baseline(output, baseline, ["hugo"], runs, binary, root)
            written = json.loads(output.read_text())
            self.assertEqual(written["repos"]["hugo"]["measurement_profile"], profile)
            self.assertNotIn("measurement_profile", written["repos"]["redox"])
            self.assertEqual(written["repos"]["redox"], unchanged_redox)
            output.unlink()
            csvs = [root / f"oss-matrix-run-{index}.csv" for index in (1, 2)]
            for path in csvs:
                path.touch()
            csv_rows = {path: [runs[name][index] for name in gate.REPO_ORDER] for index, path in enumerate(csvs)}
            with patch.object(gate, "read_run_csv", side_effect=csv_rows.__getitem__):
                self.assertEqual(gate.write_baseline_from_results(output, baseline, root, "linux-x86_64", "CI fixture"), 0)
            written = json.loads(output.read_text())
            self.assertTrue(all(repo["measurement_profile"] == profile for repo in written["repos"].values()))
            output.unlink()
            for bad_profile in (None, "legacy", ""):
                for index in (0, 1):
                    with self.subTest(profile=bad_profile, run=index + 1):
                        runs["hugo"][index].row["measurement_profile"] = bad_profile
                        with self.assertRaisesRegex(ValueError, "measurement profile"):
                            gate.write_baseline(output, baseline, ["hugo"], runs, binary, root)
                        self.assertFalse(output.exists())
                        with patch.object(gate, "read_run_csv", side_effect=csv_rows.__getitem__):
                            with self.assertRaisesRegex(ValueError, "measurement profile"):
                                gate.write_baseline_from_results(output, baseline, root, "linux-x86_64", "CI fixture")
                        self.assertFalse(output.exists())
                        runs["hugo"][index].row["measurement_profile"] = profile

if __name__ == "__main__":
    unittest.main(verbosity=2)
