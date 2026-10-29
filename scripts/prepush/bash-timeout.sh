#!/usr/bin/env bash
set -euo pipefail
test_name=bash_reply_timing_contract_test::subc_promoted_foreground_is_killed_at_its_timeout
listing="$(cargo test --locked -p agent-file-tools --features test-timing-hooks --test integration "$test_name" -- --list 2>&1)"
printf '%s\n' "$listing"
[[ "$listing" == *"$test_name: test"* ]] || { echo "missing native test: $test_name"; exit 1; }
cargo test --locked -p agent-file-tools --features test-timing-hooks --test integration "$test_name" -- --exact
