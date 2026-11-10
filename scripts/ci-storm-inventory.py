#!/usr/bin/env python3
"""Require all normal storm cases in the optimized test profile."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

ACK_TEST = "subc_storm_test::subc_storm_slow_config_read_does_not_delay_bind_ack"
READ_TEST = "subc_storm_test::max_paths_single_event"
MAIN_TEST = "subc_storm_test::subc_storm_rebinds_stay_live_under_build_and_tool_traffic"

# Commit 86dc73d moved the reconnect/output-cap regression into the regular
# integration shards: it checks saved-output recovery, not calibrated latency.
# Keep that separation and an explicit storm census so a missing case cannot be
# hidden by an unrelated replacement with the same count.
EXPECTED_TESTS = {f"subc_storm_test::{name}" for name in (
    "generation_supersession_mid_drain",
    "ignore_edit_ordering",
    "max_paths_single_event",
    "overflow_control",
    "pool_size_two",
    "reads_during_maintenance",
    "rename_pair",
    "subc_storm_completion_channel_saturation_does_not_delay_binds",
    "subc_storm_detach_rebind_replays_completion_and_preserves_bash_task",
    "subc_storm_heavy_init_saturation_does_not_delay_fresh_bind",
    "subc_storm_readiness_accepts_only_verified_omitted_roots",
    "subc_storm_readiness_preserves_disabled_semantic_expectation",
    "subc_storm_readiness_rejects_stale_or_unknown_census",
    "subc_storm_readiness_rejects_unready_omitted_planes",
    "subc_storm_rebinds_stay_live_under_build_and_tool_traffic",
    "subc_storm_route_bind_deadline_errors_and_retry_recovers",
    "subc_storm_same_or_lower_epoch_rebind_is_rejected",
    "subc_storm_slow_config_read_does_not_delay_bind_ack",
)}


def check_inventory(document: dict) -> None:
    names = {
        name
        for suite in document["rust-suites"].values()
        for name, case in suite["testcases"].items()
        if not case["ignored"] and case["filter-match"]["status"] == "matches"
    }
    if names != EXPECTED_TESTS:
        raise RuntimeError(f"storm inventory mismatch; missing {sorted(EXPECTED_TESTS - names)}; unexpected {sorted(names - EXPECTED_TESTS)}")
    print(f"Storm inventory: all {len(EXPECTED_TESTS)} calibrated cases selected, including the traffic storm", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-inventory", type=Path, required=True)
    args = parser.parse_args()
    print(f"Python {sys.version.split()[0]}; storm inventory", flush=True)
    check_inventory(json.loads(args.check_inventory.read_text()))


if __name__ == "__main__":
    main()
