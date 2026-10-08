# Train 345 assembly and verification

Follow-up: [paired main/train comparison and fixes](train-345-baseline-comparison.md) reruns all 40 reported Rust failures on main `d286c3d30` and the train. It supersedes the unresolved Rust classifications below: no main-pass/train-fail case remains after the follow-up fixes.

Base: `2eea2a8963e71decce17aea9773c58a4cc6e5446` (main, including trains 340, 343 and 344 and their CI fixes).

This is an assembly of accepted deliveries, not a claim that every full suite is green. The parent must run the 93-row search-quality replay and settle the outstanding environment/fixture failures before landing.

## Commit provenance and order

Delivery histories were selected with `git log --first-parent <dispatch-base>..<tip>`, never a merge-base range. All commit objects resolved locally despite the branch-name warnings in the dispatch brief. The additional startup-admission delivery was inserted after the storage delivery, as requested.

| Assembly commit | Source commit | Delivery |
| --- | --- | --- |
| `244ff6299` | `c10b8e2d1` | Production storage fence and test isolation |
| `35b70fe64` | `26e41f136c3c` | Debug-assertions production migration policy |
| `4f28eeb1c` | `30437424b` | Real startup-maintenance admission regressions (additional delivery) |
| `b53f037b8` | `cc19bdde9` | In-place named import edits |
| `7e5ca08de` | `c9fc42a7c4b7` | LSP idle budget and inspect categories |
| `7c7f459da` | New assembly cleanup | Centralize the ten repeated inspect category-off gates |
| `78fa493f8` | `bb7d822dd` | View chunk-vector reuse |
| `3a7c906ed` | `51eda5281` | Chunk reuse guards and fill accounting |
| `c395a5a97` | `aeda50be5` | Compact binary chunk vectors |
| `23ea015fa` | `25a7ea014` | Explicit runon refusal regressions |
| `aeaf19590` | `ba89297b6` | Explicit runon refuses unavailable executor before start |
| `7b4acd46b` | `7a6c1bc75` | Views-on worktree callgraph route |
| `09b6ed75d` | `5d7e39f5b` | Typed not-indexed callgraph refusal |
| `ec85b464b` | `b240d1c7a` | Dead-code public paths and usage roots |
| `6440666fd` | `715f01b05` | Explicit Go dispatch and interface liveness |
| `ac9a42d4a` | `7306b6953` | Manifest-named Rust library roots |
| `9a90335ea` | `7c6afc5df` | Dead-code corpus evidence and judgments |
| `c8de7fb56` | New assembly alignment | Conflict formatting and regeneration record |
| `2b41201b7` | New assembly alignment | Regenerated tool-provider catalog descriptions/digests |
| `8d89ac8ed` | New assembly adaptation | Reconcile existing regression contracts |

The final descriptor/report/formatting commit and the generated governed-doc alignment commits follow these commits.

## Resolutions and adaptations

- Storage conflicts in `backup.rs`, `bash_background/persistence.rs` and `views/mod.rs`: retain main's private-storage/no-follow directory operations and task I/O fault injection; add the production write refusal before them. Do not revert to the delivery's older ordinary `OpenOptions`/`create_dir_all` implementations.
- Config conflicts: keep main's `RemoteExecConfig` import and all six bash-privacy parity cases alongside the new idle/category settings; remove only the retired LSP TTL imports. Both resolutions are recorded in `c8de7fb56`'s commit message.
- The startup-admission commit applied without a conflict against current `storage_retention.rs`. Its two startup-grace regressions passed in the full library run.
- The requested manager cleanup replaces nine snapshot-config gates and one job-config gate with `inspect_category_is_off`. It preserves the active-category predicate, including the master inspect switch for Metrics, and keeps retired categories on their unsupported-category paths. An exhaustive 13-category test and the existing zero-job regression pass.
- Main's Rust complex-use test expected regeneration to alphabetize `self` after `Display`. The accepted in-place editor intentionally keeps authored order. Only that expected ordering changes; nested-use handling, compilation and explicit organize checks remain. Its initial failing output was `use std::fmt::{self, Display};` versus the old expected order.
- Main's semantic-sharing test treated every chunk in 15 changed/new files as novel (60). Ten changed files retain their `Record` struct chunks, so chunk reuse now correctly sends only `10 * 3 + 5 * 4 = 50` texts to the model. The test pins both the 60-chunk cold fixture and the independent 50-input expectation, and retains cold-query parity. Initial failure: `left: 50`, `right: 60`.
- The new `StatusBarCountValues.disabled_categories` field required an empty set in main's truthful-count seam fixture. Before adaptation the REST target failed with `E0063: missing field disabled_categories`; all four seam tests pass afterward.

## Generated artifacts and search-quality fence

- `bun run --cwd packages/aft-bridge build` passed before plugin checks (TypeScript 5.9.3).
- `bun run --cwd packages/opencode-plugin schema`: generated the JSON schema, zero diff.
- `bun scripts/capture-config-parity.ts`: generated 140 cases, zero diff; the Rust parity test passes.
- `bun run --cwd packages/opencode-plugin build:tool-schemas`: generated 24 tool schemas, enabled-only bash schemas and governed edit schemas, zero diff.
- Linux `cargo test -p agent-file-tools --lib regenerate_tool_provider_catalog -- --ignored`: 1 passed. Imported the exact emitted generator diff for both catalog fixture files into the local worktree. The full catalog golden/digest test passed in the library run. Remote writes do not automatically return to the local worktree; rustfmt output was likewise applied locally and checked again.
- `train-345.json` declares `engine_unwired`, `targeted_mechanism: none`. The actual diff against `origin/main...HEAD` derives to `ranking` using the repository's `RANKING_FENCE_PREFIXES`. Fenced paths are `crates/aft/src/lib.rs`, `crates/aft/src/semantic_index.rs`, and `crates/aft/src/subc_tool_schemas.json`.
- TOML/YAML exclusion can move benchmark rows. No benchmark or network-dependent replay ran here. The descriptor explicitly assigns the 93-row replay to the parent; unchanged-ranking parity is not presumed.
- Python 3.9.6: validated one descriptor with the real `resolve_descriptor` implementation and confirmed all three `fenced_files` against the actual diff.

## Gates and counts

Rust commands ran on Linux via `runon: "linux"`, each starting with `[ "$(uname)" = Linux ] || exit 99`. Tools: rustc 1.99.0 (`b940084d7`, 2026-09-28), cargo 1.99.0 (`5f94df478`, 2026-08-27), rustfmt 1.10.0-stable (`b940084d7`, 2026-09-28).

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Final exit 0 (silent-success gate). Earlier formatting diffs were applied locally. |
| `cargo check --target x86_64-pc-windows-gnu --tests -p agent-file-tools` | Unavailable: remote target is not installed; E0463 cannot find `core`/`std`. No Windows compile pass claimed. |
| `cargo test -p agent-file-tools --lib` | 5415 passed, 18 failed, 48 ignored (5481 total). |
| `cargo test -p agent-file-tools --bin aft` | 120 passed, 0 failed. |
| `cargo test -p agent-file-tools --test integration` | Initial run: 2193 passed, 19 failed, 28 ignored (2240 total). Two contract mismatches corrected as described above; only impacted tests rerun. |
| `cargo test -p agent-file-tools --test rest` | Initial E0063 corrected; subsequent full run: 247 passed, 3 failed, 6 ignored (256 total). |
| `cargo test -p agent-file-tools --test semantic` | 52 passed, 0 failed, 2 ignored. |
| `cargo test -p agent-file-tools --test list_envelope` | 175 passed, 0 failed. |
| `cargo test -p agent-file-tools --test integration config_parity` | 1 passed, verifies the parity fixture corpus. |
| `cargo test -p agent-file-tools --test integration rust_complex_use_lists_survive_edits_and_compile` | Final restored run: 1 passed. |
| `cargo test -p agent-file-tools --test integration embed_counts_views_and_sessions_share_identical_content` | Final restored run: 1 passed. |
| `cargo test -p agent-file-tools --test rest status_counts_inspect_seams` | 4 passed. |
| `cargo test -p agent-file-tools --lib category_off_helper_preserves_active_and_retired_categories` | 1 passed; also passed in the full lib run. |
| `cargo test -p agent-file-tools --lib disabled_inspect_category_starts_zero_jobs` | 1 passed; also passed in the full lib run. |

Local tools: Bun 1.4.2, TypeScript 5.9.3, Biome 2.4.7. All local Bun test commands start with a fresh HOME and XDG_DATA/CONFIG/STATE/CACHE_HOME created by `mkdir -p` under a unique `/private/tmp/aft-train345-*` directory outside any checkout, and `AFT_STORAGE_DIR` and production-migration opt-in unset. The accepted test preload additionally isolates shared child spawns into its disposable worktree-local target directory; the live-database canary is a separate read-only process. The checkout itself is not under `/tmp`.

| Command | Result |
| --- | --- |
| `bun run lint` | Passed; both initial and final runs checked 706 files (Biome 2.4.7). |
| `bun run typecheck` | Passed for aft-bridge, aft-cli, opencode-plugin and pi-plugin (all four package scripts exit 0). |
| `bun run --cwd packages/aft-bridge test:unit` | 807 pass, 3 skip, 1 fail; 811 tests across 68 files. |
| `bun run --cwd packages/opencode-plugin test:unit` | 1614 pass, 3 skip, 12 fail; 1629 tests across 133 files. |
| `bun run --cwd packages/pi-plugin test:unit` | 841 pass, 1 skip, 1 fail; 843 tests across 84 files. |
| Bridge unit script with name pattern `test storage isolation\|feature-config policy\|inspect and LSP removed keys\|applyLiveConfigKeys\|startLiveConfigReload\|watchAftConfigFiles` | 34 pass, 0 fail. |
| OpenCode unit script with name pattern `loadAftConfig\|inspect categories\|LSP idle\|private HOME\|off inspect categories` | 75 pass, 0 fail. |
| Pi unit script with the same config/harness pattern | 58 pass, 0 fail. |

Native plugin/bridge E2E suites were not run: this local worktree has no `target/debug/aft` for macOS, and the brief requires Rust builds remotely. This also prevents the unit suites' real-binary fixture tests from verifying the assembled engine. Package manifests/lockfiles did not change; no new install was required. Scoped `aft_inspect` was PARTIAL (Biome server not ready for Pi config and checkout callgraph unavailable); root tsc and Biome commands are the authoritative static gates.

## Outstanding full-suite failures (exact names and excerpts)

These were investigated and left visible rather than relaxing tests or changing product behavior outside the assembly scope. No clean baseline rerun was performed, so environment explanations below are observations/inferences, not a claimed baseline pass.

Library:

- `commands::bash::tests::permission_retry_reclassified_as_first_party_resolves_native_plan`: `sandbox_unavailable ... read_allow path does not exist ... No such file or directory` (concurrent temporary fixture disappears).
- `commands::outline::tests::outline_portability_goldens_stay_lf_with_autocrlf`: `fatal: detected dubious ownership in repository` (remote checkout ownership).
- `commands::semantic_search::tests::blackholed_backend_never_blocks_status_or_search`: backend returns `Network is unreachable (os error 101)` and `semantic_backend_unavailable`, rather than the test's expected timeout shape.
- `fs_lock::tests::cross_host_lock_is_not_stolen_before_extended_stale_threshold`: `assertion left != right failed; left: None, right: None`.
- `fs_lock::tests::live_pid_with_wrong_boot_id_is_reclaimed`, `fs_lock::tests::live_pid_with_wrong_start_time_is_reclaimed`, `fs_lock::tests::stale_heartbeat_from_live_pid_blocks`: `current process should have a start-time identity` (remote /proc identity unavailable).
- `gh_shim::relay_client::tests::a_changed_binding_generation_triggers_a_new_check`, `gh_shim::relay_client::tests::a_live_ticket_relays_the_governed_envelope_and_prints_the_url`, `gh_shim::relay_client::tests::assertion_refusals_retry_once_for_a_fresh_mint`, `gh_shim::relay_client::tests::transient_refusals_retry_twice_with_the_same_nonce`, and `standing_roots::tests::daemon_startup_empty_pass_marks_existing_snapshot_strict_on_first_configured_pass`: the newly accepted test-storage fence catches existing ambient-default test contexts: `test context cannot use the live default AFT storage root: /motor-home/.local/share/cortexkit/aft`. These are not silently called unrelated passes.
- `gh_shim::tests::producer_edit_last_vectors_pin_consumer_wire_request_and_refusals`: `wire request must carry a repository string`.
- `logging::tests::recycled_pid_log_is_reaped_while_the_live_owner_is_kept`: `left: 0, right: 1`.
- `logging::tests::recycled_pid_now_owned_by_another_user_is_reaped`: `pid 1's start time must be readable from an unprivileged process`.
- `sandbox_spawn::policy_tests::native_sandbox_predicate_controls_spawn_and_rewrite`: `cannot split read root /motor-home/tmp: directory entry changed while inspecting ... No such file or directory`.
- `storage_permissions_tests::storage_debug_archive_ignores_public_member_modes`: `Os { code: 2, kind: NotFound, message: "No such file or directory" }`.
- `semantic_index::tests::platform_verifier_tls_client_subprocess`: localhost TLS child instead fails at DNS: `Temporary failure in name resolution`.

Integration (remaining after the two adaptations):

- All twelve timeout tests in `bash_reply_timing_contract_test` fail with `grandchild <pid> survived the timeout kill; the process group was not killed`: `background_is_killed_at_its_timeout`, `foreground_without_wait_is_killed_at_its_timeout`, `block_to_completion_answers_timed_out_within_timeout_plus_margin`, `promoted_foreground_is_killed_at_its_timeout`, `pty_is_killed_at_its_timeout`, `wait_true_answers_timed_out_within_timeout_plus_margin`, and their six `subc_` counterparts. The sandbox's process census/identity limitations also affect lib tests; no timeout assertion was weakened.
- `per_checkout_7::the_census_sees_an_aft_executable_under_a_path_with_spaces`: `Os { code: 26, kind: ExecutableFileBusy, message: "Text file busy" }`.
- `sandbox_native_test::native_read_floor_splits_project_denies_and_skips_home_symlinks`: `cat: /etc/hostname: No such file or directory`; actual status `failed`, expected `completed`.
- `subc_format_test::callgraph_format_matches_typescript_golden_fixtures`, `subc_format_test::safety_format_matches_typescript_golden_fixtures`, `subc_format_test::subc_format_matches_typescript_golden_fixtures`: remote HOME-relative formatting produces `~/tmp/aft-test-scratch-.../aft-subc-parity/project/src/main.ts` where fixtures normalize `<PROJECT_ROOT>/src/main.ts` (7, 3 and 12 mismatches respectively). Not an inspect-description mismatch.

REST:

- `fake_helper_cache_test::installation_never_write_opens_a_running_executable`: `ExecutableFileBusy: Text file busy`.
- `callgraph_store_test::root_keyed_migration_uses_sqlite_backup_for_only_current_legacy_generation`: `background legacy migration did not publish and install a root-keyed store`.
- `callgraph_store_test::writer_access_serves_legacy_fallback_while_background_migration_and_refresh_converge`: `writer-capable fallback access must schedule migration on the background lane`. The accepted views-route change retains writer-backed fallback; no evidence justified changing the product route or weakening these assertions.

TypeScript:

- Bridge: `subc rig retains module and daemon stderr across failed startup retries`: expected child status `0`, received `null`; child timed out at 10 seconds.
- OpenCode: six `BinaryBridge lifecycle` tests (`spawns binary and ping returns pong`, `multiple sequential requests return correct responses (ID correlation)`, `bridge recovers via lazy respawn after external SIGKILL`, `shutdown cleans up child process (no orphans)`, `request to dead bridge after max retries rejects with error`, `multiple parallel first calls share one configure (no race)`) and four `Tool round-trips` tests (`aft_outline tool returns tree text for fixture file with known symbols`, `aft_outline directory and array preserve the real structure golden`, `edit_symbol replaces a function and returns backup_id and syntax_valid`, `undo restores the file after edit_symbol`) cannot start a native fixture binary: `BridgeTransportUnavailableError: stdin not writable for command "configure"` (or bridge shutting down).
- OpenCode: `permission audit regressions > external_directory ask expands ~/ before containment`: expected ask length `1`, received `0`; HOME/XDG are isolated by the suite. No permission behavior changed to accommodate the fixture.
- OpenCode: `auto-update-checker/checker > getCachedVersion and updatePinnedVersion > reads cached version from OpenCode's scoped package cache layout`: expected `"0.17.2"`, received `null` under isolated HOME/XDG.
- Pi: `bash tool adapter > transport-dead fallback recovers on the next successful module call`: `this test timed out after 20000ms`.

## Mutation controls

Three assembly-local mechanism controls were staged, mutated with `NON-VACUITY BREAK`, run narrowly, and restored with `git checkout -- <path> && touch <path>`. Each captured a non-empty mutation diff followed by an empty working diff. Only the named test failed in each focused red run:

1. Neutralize `inspect_category_is_off`: `inspect::manager::guard_tests::disabled_inspect_category_starts_zero_jobs` rejects the now-admitted Todos job.
2. Bypass Rust in-place named removal: `import_test::rust_complex_use_lists_survive_edits_and_compile` rejects regenerated `use std::fmt::{Display, self};`.
3. Force chunk-cache misses: `per_checkout_semantic::embed_counts_views_and_sessions_share_identical_content` reports `left: 60, right: 50` at the older-branch assertion; both identical-view and identical-session checks before it still succeed.

The unchanged source-delivery mutation campaigns remain documented in their original commits. The new seam initializer is defended loudly by compilation (E0063), not a silent invariant. No mutation remains in the final tree.
