#!/usr/bin/env bash
# Native Linux contracts that CI runs outside the ordinary archive inventory.
set -euo pipefail
export AFT_REQUIRE_LANDLOCK=1
unset AFT_STORAGE_DIR AFT_CACHE_DIR AFT_TEST_AFT_BINARY NEXTEST_BIN_EXE_aft CARGO_BIN_EXE_aft
cargo test -p agent-file-tools --lib -- gh_shim --test-threads=1
binary_listing="$(cargo test -p agent-file-tools --bin aft -- --list)"
[[ "$binary_listing" == *'gh_shim_broker_seatbelt_root_uses_the_shared_namespace_definition: test'* ]]
cargo test -p agent-file-tools --bin aft -- gh_shim_broker_seatbelt_root_uses_the_shared_namespace_definition --test-threads=1
listing="$(cargo test -p agent-file-tools --test worker_jail -- --list)"
for test in \
  gh_shim_router_host_read_uses_served_catalog_arguments \
  gh_shim_router_host_lookup_transport_failures_record_their_stage \
  worker_gh_read_revocation_suppresses_reply_to_surviving_observer \
  worker_gh_read_common_git_config_is_write_denied \
  worker_gh_read_t8_first_bound_read_uses_broker_with_credentials_denied \
  worker_gh_read_t8_live_scope_rebind_refuses_before_upstream \
  worker_gh_read_t9_policy_matrix_keeps_repository_and_search_fences \
  worker_gh_read_t10_terminal_revocation_unlinks_endpoint \
  worker_gh_read_t10_retained_descendants_fail_after_end_cancel_timeout_and_restart \
  worker_gh_read_t10_promotion_keeps_the_original_command_grant \
  worker_gh_read_t10_cancel_midflight_terminates_reader_without_output \
  worker_gh_read_t11_cross_command_capability_file_and_socket_isolation \
  worker_gh_read_t12_missing_and_stale_endpoint_fail_without_native_gh \
  worker_gh_read_t12_malformed_peer_reply_and_endpoint_replacement_fail_closed \
  worker_jail_temp_denies_other_task_directory; do
  [[ "$listing" == *"$test: test"* ]] || { echo "missing native test: $test"; exit 1; }
done
cargo test -p agent-file-tools --test worker_jail -- gh_shim_router_host --test-threads=1 --nocapture
cargo test -p agent-file-tools --test worker_jail -- worker_gh_read --test-threads=1 --nocapture
cargo test -p agent-file-tools --test worker_jail -- worker_jail_temp_denies_other_task_directory --exact --test-threads=1 --nocapture
probe_listing="$(cargo test -p agent-file-tools --test sandbox_launch_probe -- --list)"
for test in launcher_closes_late_inherited_listener_above_marker_slots \
  launcher_fd_probe_required_mode_cannot_silently_skip launcher_failure_marker_cannot_be_forged_by_jailed_exec; do
  [[ "$probe_listing" == *"$test: test"* ]] || { echo "missing native test: $test"; exit 1; }
done
cargo test -p agent-file-tools --test sandbox_launch_probe -- --test-threads=1 --nocapture
bash scripts/prepush/bash-timeout.sh
