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


def check_inventory(document: dict) -> None:
    names = {
        name
        for suite in document["rust-suites"].values()
        for name, case in suite["testcases"].items()
        if not case["ignored"] and case["filter-match"]["status"] == "matches"
    }
    if len(names) != 19 or not {MAIN_TEST, ACK_TEST, READ_TEST}.issubset(names):
        raise RuntimeError(f"expected all 19 calibrated storm cases, including the traffic storm and deadline tests; got {sorted(names)}")
    print("Storm inventory: all 19 calibrated cases selected, including the traffic storm", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-inventory", type=Path, required=True)
    args = parser.parse_args()
    print(f"Python {sys.version.split()[0]}; storm inventory", flush=True)
    check_inventory(json.loads(args.check_inventory.read_text()))


if __name__ == "__main__":
    main()
