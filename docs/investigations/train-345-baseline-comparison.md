# Train 345: paired baseline comparison and follow-up fixes

This report supersedes the unresolved Rust-failure classifications in [the original assembly record](train-345-verification.md).

## Method and result

Compared **main `d286c3d3000bbd0d52b0a2f45043de332078b1c9`** with the original assembled train **`7761bdad3bd1abd994ce627ac8bf1f695c8e847a`**, then reran the same cases with follow-up fix **`f66d1883e`**. Only this isolated task worktree was switched temporarily to detached main; the parent checkout was never used.

All 40 previously reported failing Rust tests ran individually with exact-name selection on **ck-motor**, using the same command structure, toolchain, sandbox, serial execution (`RUST_TEST_THREADS=1`), and fresh disposable HOME/XDG namespaces. Every selected case executed exactly one test: there were no absent tests, zero-match passes, or skipped cases.

Commands were `cargo test -p agent-file-tools --lib -- <name> --exact --nocapture` or `cargo test -p agent-file-tools --test <integration|rest> -- <name> --exact --nocapture`. Every remote shell command started with `[ "$(uname)" = Linux ] || exit 99`.

HOME and XDG_DATA/CONFIG/STATE/CACHE_HOME were created with `mkdir -p` under a fresh `/motor-home/aft-train345-ci.*` directory, outside the checkout and outside the runner's ordinary temporary-directory exemption. This reproduces CI's non-temp HOME classification without opening the real account database. `AFT_STORAGE_DIR` and `AFT_ALLOW_PRODUCTION_MIGRATION` were unset; existing CARGO_HOME/RUSTUP_HOME were retained for the toolchain. A new namespace was used for each revision so baseline writes could not seed the train's state.

Tools: rustc 1.99.0 (`b940084d7`, 2026-09-28), cargo 1.99.0 (`5f94df478`, 2026-08-27), rustfmt 1.10.0-stable (`b940084d7`, 2026-09-28).

- Main: **23 pass, 17 fail**.
- Train before fixes: **22 pass, 18 fail**.
- Train after fixes: **28 pass, 12 fail**.
- **No remaining main-pass/train-fail case among all 40 tests.**

The five explicitly requested storage-isolation repairs fail on main as well as the original train in this CI-like environment. They were fixed anyway, as required; the storage fences were not weakened. The one additional main-pass/train-fail regression was import reply formatting, hidden by HOME-relative path mismatches in the earlier full-suite run.

## Complete comparison table

| Target / exact test name | Main d286c3d30 | 345 before | 345 fixed |
| --- | --- | --- | --- |
| `lib: commands::bash::tests::permission_retry_reclassified_as_first_party_resolves_native_plan` | pass | pass | pass |
| `lib: commands::outline::tests::outline_portability_goldens_stay_lf_with_autocrlf` | fail | fail | fail |
| `lib: commands::semantic_search::tests::blackholed_backend_never_blocks_status_or_search` | fail | fail | fail |
| `lib: fs_lock::tests::cross_host_lock_is_not_stolen_before_extended_stale_threshold` | fail | fail | fail |
| `lib: fs_lock::tests::live_pid_with_wrong_boot_id_is_reclaimed` | fail | fail | fail |
| `lib: fs_lock::tests::live_pid_with_wrong_start_time_is_reclaimed` | fail | fail | fail |
| `lib: fs_lock::tests::stale_heartbeat_from_live_pid_blocks` | fail | fail | fail |
| `lib: gh_shim::relay_client::tests::a_changed_binding_generation_triggers_a_new_check` | fail | fail | pass |
| `lib: gh_shim::relay_client::tests::a_live_ticket_relays_the_governed_envelope_and_prints_the_url` | fail | fail | pass |
| `lib: gh_shim::relay_client::tests::assertion_refusals_retry_once_for_a_fresh_mint` | fail | fail | pass |
| `lib: gh_shim::relay_client::tests::transient_refusals_retry_twice_with_the_same_nonce` | fail | fail | pass |
| `lib: gh_shim::tests::producer_edit_last_vectors_pin_consumer_wire_request_and_refusals` | fail | fail | fail |
| `lib: logging::tests::recycled_pid_log_is_reaped_while_the_live_owner_is_kept` | fail | fail | fail |
| `lib: logging::tests::recycled_pid_now_owned_by_another_user_is_reaped` | fail | fail | fail |
| `lib: sandbox_spawn::policy_tests::native_sandbox_predicate_controls_spawn_and_rewrite` | pass | pass | pass |
| `lib: standing_roots::tests::daemon_startup_empty_pass_marks_existing_snapshot_strict_on_first_configured_pass` | fail | fail | pass |
| `lib: storage_permissions_tests::storage_debug_archive_ignores_public_member_modes` | fail | fail | fail |
| `lib: semantic_index::tests::platform_verifier_tls_client_subprocess` | fail | fail | fail |
| `integration: bash_reply_timing_contract_test::background_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::foreground_without_wait_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::block_to_completion_answers_timed_out_within_timeout_plus_margin` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::promoted_foreground_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_background_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_block_to_completion_answers_timed_out_within_timeout_plus_margin` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_foreground_without_wait_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_promoted_foreground_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_pty_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::subc_wait_true_answers_timed_out_within_timeout_plus_margin` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::wait_true_answers_timed_out_within_timeout_plus_margin` | pass | pass | pass |
| `integration: bash_reply_timing_contract_test::pty_is_killed_at_its_timeout` | pass | pass | pass |
| `integration: import_test::rust_complex_use_lists_survive_edits_and_compile` | pass | pass | pass |
| `integration: per_checkout_7::the_census_sees_an_aft_executable_under_a_path_with_spaces` | pass | pass | pass |
| `integration: per_checkout_semantic::embed_counts_views_and_sessions_share_identical_content` | pass | pass | pass |
| `integration: sandbox_native_test::native_read_floor_splits_project_denies_and_skips_home_symlinks` | fail | fail | fail |
| `integration: subc_format_test::callgraph_format_matches_typescript_golden_fixtures` | pass | pass | pass |
| `integration: subc_format_test::safety_format_matches_typescript_golden_fixtures` | pass | pass | pass |
| `integration: subc_format_test::subc_format_matches_typescript_golden_fixtures` | pass | fail | pass |
| `rest: fake_helper_cache_test::installation_never_write_opens_a_running_executable` | pass | pass | pass |
| `rest: callgraph_store_test::root_keyed_migration_uses_sqlite_backup_for_only_current_legacy_generation` | pass | pass | pass |
| `rest: callgraph_store_test::writer_access_serves_legacy_fallback_while_background_migration_and_refresh_converge` | pass | pass | pass |

## Fixes and storage sweep

### Relay fixture invalidation storage

The relay fixtures already isolate their connection file and shim state, but successful issue mutations also invalidate the shared GitHub-read database. That callback resolved process-default storage, causing the four successful/retrying relay cases to trip the fence.

Added a private explicit-storage dispatcher seam, `dispatch_r3_with_relay_at`; the production wrapper retains its ordinary shared-root resolution. All four relay test dispatch sites now supply storage beneath their retained fixture TempDir. The live-ticket test also asserts that the fixture database was actually created, so the fix cannot pass by skipping invalidation.

### Standing roots initial empty pass

The restarted StandingRoots instance has no observed namespace yet. Its first empty reconciliation must receive the fixture storage rather than process defaults. Its later configured pass still proves restart strictness.

The separate `configuration_add_modify_and_remove_mint_boundaries_and_delete_rows` test deliberately keeps its unset-config removal: that instance has already observed explicit temporary storage, and the production logic reuses that namespace. Changing it to explicit storage would erase the existing fallback-retention claim. It passes unchanged.

### Binary contexts omitted by library-only cfg(test) isolation

Library AppContext constructors already replace unset storage with a retained private per-App TempDir when compiled with cfg(test). Binary and integration tests link the library without that cfg, so they need explicit fixture configs.

The sweep found 17 `AppContext::new` sites in main's test modules and two unset `from_app` sites. They now use the existing `tests/helpers/context_storage.rs` retained-TempDir helper, preserving any explicit namespace. Production main constructors are unchanged. The standalone signal-handler test now asserts explicit, existing fixture storage. The helper's two own tests join the binary target, bringing it to 122 tests.

AST/lexical inspection of `crates/aft/tests/` found the remaining constructors already wrapped by the helper or given explicit temporary storage (including the qualified from_app sites). Checked direct database-open/default-storage sites in lib tests; the remaining default-storage exercises either deliberately install temporary XDG under the environment lock or are should-panic fence tests. BackupStore::new is non-persistent until configured; checkpoint fixtures configure their temporary namespace before I/O. No additional integration constructor needed a change.

### Preserve legacy import response contracts

The accepted import renderer assumed missing `remaining_names` meant zero and omitted the former scope/name lines. The paired test isolated two regressions:

- `import_remove_not_present`: omitted `scope entire import`.
- `import_remove_removed_name`: fabricated `(0 names remain)` instead of the legacy `removed pkg` / `name alpha` summary.

Responses lacking both `remaining_names` and `only_name` now retain their legacy summary. Responses carrying those fields still render the new accurate named-removal/whole-import detail. No golden fixture or assertion was rewritten; the rich-summary unit test and old parity test both pass.

## Legacy migration scheduling

Both specifically named migration tests **pass on main and on the train before and after fixes** in the paired environment. The earlier full-suite failures do not reproduce in isolated serial runs, and there is no evidence of a views-route regression. The views route and migration scheduling code were left unchanged; no assertion or capability gate was relaxed to obtain these passes.

## Verification and mutation controls

All Rust checks used the same Linux/isolated-HOME rules above:

- Relay unit filter: **24 passed**.
- Standing-roots filter (including DB standing-root tests): **18 passed**.
- Subc formatter filter: **34 passed**.
- Full binary target: **122 passed**, including a final run after mutation restoration.
- Existing storage-fence filter: **9 passed**, including the account-default rejection controls.
- Import-format parity integration test: **1 passed**, including final paired replay.
- Final 40-case replay: **28 passed, 12 baseline failures**, no missing cases and no main-pass/train-fail cases.
- Final `cargo fmt --all -- --check`: exit 0, rustfmt 1.10.0-stable.
- Root `bun run lint`: **706 files checked**, Biome 2.4.7, no fixes needed.
- Remote `rustup target list --installed`: only `x86_64-unknown-linux-gnu` (rustup 1.29.1); Windows compile remains unavailable.

Four staged `NON-VACUITY BREAK` controls each caused only its intended named test to fail; every mutated path was restored with `git checkout -- <path> && touch <path>` and had an empty `git diff --stat` afterward:

1. Reintroduce default storage at relay dispatch: live-ticket test rejects the default root.
2. Reintroduce default storage at initial standing-root reconciliation: startup-empty-pass test rejects the default root.
3. Remove binary signal-handler fixture isolation: its test fails `binary fixture storage is explicit`.
4. Disable legacy import compatibility: the parity test fails the two legacy import cases, while `remove_reply_names_the_removed_specifier_and_import_scope` still passes.

No TypeScript, package manifest, lockfile, tool argument or generated tool schema changed in this follow-up. Their prior checks remain in the assembly record; no install or new native plugin test was needed. The Windows target remains unavailable on the remote runner. The original search-quality descriptor and the parent's required 93-row replay are unchanged.

## Twelve remaining baseline failures

These fail on both main and train under the same runner settings and were left unchanged, as requested:

- Outline portability: `fatal: detected dubious ownership in repository`.
- Blackholed semantic backend: `Network is unreachable (os error 101)` instead of a timeout.
- Four fs_lock identity tests: missing process start-time/boot identity (`None` or `current process should have a start-time identity`).
- Producer edit-last wire fixture: `wire request must carry a repository string`.
- Two logging PID-census tests: observed `0` instead of `1`, and `pid 1's start time must be readable from an unprivileged process`.
- Storage debug archive fixture: `No such file or directory`.
- TLS subprocess fixture: localhost lookup fails with `Temporary failure in name resolution` instead of reaching certificate verification.
- Native read-floor fixture: `/etc/hostname` does not exist on the runner.

The table supplies the exact names. This is a baseline comparison, not a claim that these tests are universally healthy or that every full-suite failure is reproducible serially.
