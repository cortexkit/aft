# Train 343 assembly and verification

Base: `45a0c76855fda3468eb333ed89ad0398d124b7cb` (train 340). The final tip is the commit containing this report; the delivery record supplies its full SHA. No merge of main, merge-base range, or push was used. Each accepted delivery commit remains separate, including the diagnostics delivery's empty verification-only commit.

## Delivery commit map

Ranges were enumerated with `git log --reverse --first-parent <dispatch-base>..<accepted-tip>`. The operator supplied unavailable dispatch metadata.

| Delivery | Dispatch range | Source commit → assembled commit, in order |
| --- | --- | --- |
| 1. Foreground bash reply | `1821346c895a..c9a4832c2045` | `c9a4832c2045 → 7a9829dc6808` |
| 2. Delete writer barrier | `1821346c895a..65eb19f1fd48` | `65eb19f1fd48 → 491ae8d5f749` |
| 3. Unreadable task restore | `4cb4e6194b1b..249ef721011f` | `fd98841a9ce0 → d400b2519aa2`; `249ef721011f → 4c180f764a20` |
| 4. Owner-only storage | `b0e38352f973..289e79300d73` | `99b3e51f1222 → 270f883a1188`; `a911738bf67b → f38e8dafdbaa`; `289e79300d73 → a828e71d6c46` |
| 5. Storage retention | `b0e38352f973..1f7daba398ca` | `a182ccdbbd2c → 97b7e100c58d`; `2992e7fce1cb → 9bec9b0f3231`; `1f7daba398ca → 28efb503b48a` |
| 6. Edit continuation only | `c064c7d87f29..91af3c7d39b5` | `3b2369340cec → ee027c094a58`; `a598020df99f → f01e32331eea`; `2964299abc27 → b0bfcd044c5f`; `91af3c7d39b5 → 05a06b622e13` |
| 7. Persisted Rust diagnostics | `6e79f97bf196..1cfca3887b32` | `faad0d757d27 → dd77f508dee2`; `1fdfddc32f7b → bac2fac3306a`; `f9a48e458311 → 6bbf55c5d8d9`; `5c4ea6cdfe84 → ba527fe6df2f`; `ed6e204e864f → e78ca6a0e416`; `09496b55deab → 9069215b9687`; `574df9fd8c6f → f3161e0b9661` (empty); `a9e3718b913e → 9f1ce0baa779`; `041b71e2b673 → 1449e8c2edb5`; `1cfca3887b32 → 13d8f28f245a` |
| 8. Tool-call ledger S1 | `e83411ff728b..72eb931cd905` | `94e77461ee20 → ec61c0cabaa0`; `b74678c7eaa3 → 3f9bd13d218e`; `26c9cb3d4b33 → 73993d13b27a`; `b2732e3bad04 → 998397c5653c`; `72eb931cd905 → 5624cebbcdd1` |
| 9. Semantic perf | `54a3d93e230e..3be6c5015e9c` | `416037aeb5fb → 9719f0dfdee5`; `3be6c5015e9c → eefd4a5cd6c6` |
| 10. Callgraph perf | `54a3d93e230e..b5f405a49ab9` | `a004c6a0beec → 0dff881b454f`; `b5f405a49ab9 → 30b84b0573dc` |
| 11. Worker watch recovery | `0a6eeb56f711..1fec7a0b4a59` | `1fec7a0b4a59 → 8957ebf38c22` |
| 12. Watcher overflow deflake | already contained by tree content | Skipped. The accepted test helper and overflow-rescan loop from `496e1145b87a` remain unchanged in the train's watcher file; unrelated existing batch-metadata optimizations account for the rest of that file's tree difference. |

The raw 23,628-line `storage-retention-live.json` and both delivery-specific search descriptors were excluded as directed. The retention Markdown and census script remain. All 34 imported commits have noreply author and committer addresses. The final assembly commit uses the same noreply address. Commit messages contain neither GitHub closing references nor parenthesized issue-like train references.

## Conflict resolutions and integration changes

Semantic choices were approved by the operator:

- Keep the existing privacy-session note, then warn and retain the live task if post-spawn metadata persistence fails. Returning before registration would orphan a live child.
- Combine spawn receipts and request-receipt deadlines with authenticated principal, remote policy, ledger spawn linkage, and catalog-derived watch availability. Preserve both parser test counters.
- Keep refusal logging with root/channel and the existing shell selector. Use elicitation-aware admission and provider-role server completion after an explicit permission grant.
- Keep migration 14 and `remote_exec_policies` untouched. Ledger migration is 15; migration inventory, supported version, downgrade assertions, and remote-policy migration assertions agree on 15.
- Keep current inspect PARTIAL/scoped-FRESH contracts and the diagnostics delivery's fingerprint, LRU, and cleanup behavior. Inspect/LSP patches otherwise applied cleanly.
- Use the mandatory wake-bound deferred response API for off-barrier delete; retain cancellation. Adapt new callers/fixtures to existing cancellation and promotion signatures. Limit Unix-only imports appropriately for the Windows warning gate.
- Budget storage-retention health details inside the existing 12 KiB ceiling, after root-detail and ledger compaction. Preserve aggregate counters and disclose the exact omitted-report count.
- Retain the manifest alongside a derived database that another generation references, because incremental assembly needs that manifest. Reclaim its unreferenced trigram. Once unreferenced, reclaim both database and manifest. Missing owner manifests alone force a cold rebuild, with the existing WARN path naming the owner; other manifest errors still propagate.

### Permission boundary trace

`tool_provider::admission_trusted` permits an untrusted shell only when it is keyed and its consumer declares elicitation. That Boolean is passed only to admission validation; it does not rewrite `identity.trust` or `bind_trust`. `handle_tool_call` still checks `if matches!(bind_trust, BindTrust::Untrusted)` and returns a reverse permission request instead of spawning. `handle_bash_elicitation_reply` calls the spawn path only for a Response for which `bash_elicitation_reply_is_allow` is true, passing `Some(pending.grants)`. Denial, malformed/error replies, and expiration call `settle_pending_bash_ask_denied`. The deferred spawn still refuses an untrusted call without permission grants. Provider conformance proves approval runs once, denial/error never creates a shell task, missing elicitation refuses before ledger admission, and keyless untrusted shell remains refused.

## Tests and assertions changed during assembly

Accepted deliveries' original tests are preserved except for the explicitly listed integration adaptations below. No failing assertion was silently weakened.

- Rename `migration_v14_installs_call_ledger_from_v13` to `migration_v15_installs_call_ledger_from_v13`, expecting 15. The original assertion was observed failing with actual 15 versus expected 14. Keep its installation assertion.
- Add `migration_v15_preserves_v14_remote_exec_policies`: start from literal version 14, assert ledger absent, migrate to 15, and verify the remote-policy table, index, sole row, params, and timestamp survive.
- Update `downgrade_refused` supported-version assertions and the remote-policy migration test's resulting-version assertion from 14 to 15. The future-schema producer remains `CURRENT_SCHEMA_VERSION + 1`.
- Future-task fixtures in `commands/bash_status.rs` and health now use `SCHEMA_VERSION + 1`, not literal 7: train 340 already supports bash schema 7. Preserve task-scoped refusal, byte identity, removal recovery, and health assertions. Four old fixtures were observed failing before this correction.
- Replace the obsolete immediate age-only configure sweep test with `configure_scheduled_storage_sweep_reaps_dead_inspect_scope_after_grace`. Exercise the actual scheduled worker, prove initial preservation/observation, advance the durable observed clock without sleeping for a week, and require eventual deletion and a removed-root count of one. The old immediate-deletion assertion failed; the operator approved observation grace rather than reinstating unsafe age-only eviction.
- Add `health_retention_reports_fit_budget_with_exact_omissions_and_totals`. A literal 128-report fixture must fit 12 KiB, report exact omissions, and retain literal aggregate totals. The existing cached-health budget test failed before the production budget fix and passes afterward.
- Change `storage_retention_shared_derived_owner_does_not_retain_obsolete_manifest_or_trigram` into `storage_retention_shared_derived_owner_keeps_manifest_until_unreferenced`: the manifest-existence assertion is intentionally reversed because publication references it; trigram deletion remains asserted. Add eventual database-and-manifest deletion after all references disappear. The operator explicitly approved this contract reconciliation.
- Add `missing_shared_owner_manifest_falls_back_to_a_cold_build`: remove only the referenced owner's manifest, require a cold-build counter, absence of the clone marker, and every-table parity with an independently materialized cold database. The existing semantic-fill/next-publication integration test remains unchanged and passes.
- Mark `repository_fingerprint_completes_and_reuses_unchanged_hashes` with `#[ignore = "measures the live checkout; run manually"]`, with its manual invocation documented. Observed cold capture exhausted the unchanged 15 s production budget at approximately 14.91 s / 2,448 examined entries on the loaded machine. The operator directed that this live-checkout wall-clock measurement not be a gate. Production cold=15 s and serving=2 s deadlines, hashing/parser implementation, and all deterministic authority controls remain unchanged. Experimental perf edits were discarded. The actual final inventory is 31 deterministic passes plus this one ignored measurement (32 tests total), not 32 deterministic passes.
- Add provider tests `s1_untrusted_keyed_shell_without_elicitation_is_refused_at_admission` and `s1_untrusted_keyed_shell_denial_and_elicitation_errors_never_spawn`; allow the fixture bind helper to explicitly omit elicitation. The latter covers deny, malformed JSON, and Error replies, and requires zero shell rows and three settled refusal rows.
- In `s1_database_unavailable_refuses_keyed_read_and_shell_but_not_keyless_read`, change only `retryable` from false to true, with a comment: current persistence retries reopening on the next call, and refusal preceded durable admission/execution, so retry cannot duplicate work. Keep unavailable code, keyless readability, no-spawn, and zero-ledger-row assertions.
- Add neutral `ledger_key: None` and remote-policy arguments to existing pending-response/spawn test fixtures to match the combined APIs; assertions remain unchanged.

### Explicitly pinned callgraph fixtures

The operator approved protecting superseded sources instead of assuming previous generations survive retention. Existing assertions remain unchanged:

1. `root_keyed_configure_migrates_newest_superseded_legacy_generation`: pin the old source while publishing its replacement so migration can intentionally select the stale source.
2. `writer_access_serves_legacy_fallback_while_background_migration_and_refresh_converge`: pin the stale source before copying the migration fixture; preserve deferred refresh and legacy-byte invariants.
3. `store_cold_rebuilds_when_concurrent_clone_root_still_exists`: retain a reader on A while B rebuilds so the final assertion can inspect A's immutable old database rather than reopen a reclaimed path as an empty SQLite file.

`root_keyed_migration_mid_crash_cleans_partial_and_preserves_legacy_source` receives the same explicit source pin during replacement publication. Its crash, cleanup, and source-preservation assertions are unchanged. All four fixtures failed without protection and pass with it.

## Verification results

Storage isolation: throwaway HOME and all XDG directories created with `mkdir -p` under ignored `target/train343-verification`; real CARGO_HOME/RUSTUP_HOME retained; AFT_STORAGE_DIR unset. Every Cargo invocation was serial. After the fleet load request, every Cargo command had `CARGO_BUILD_JOBS=4`, and every test invocation had `--test-threads 4`.

Tools: cargo 1.99.0 (`5f94df478`); rustc 1.99.0 (`b940084d7`); rustfmt 1.10.0-stable; Bun 1.4.2; TypeScript 5.9.3; Biome 2.4.7; Python 3.9.6; actionlint 1.7.12; Bash 3.2.57.

| Command/check | Final observed result |
| --- | --- |
| `cargo fmt --all -- --check` | Exit 0 (documented silent success) |
| `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= RUSTFLAGS="-D warnings -A deprecated" cargo check --target x86_64-pc-windows-gnu --tests -p agent-file-tools` | Finished dev profile, exit 0 |
| `cargo test -p agent-file-tools --lib` | Initial full run: 5328 pass / 16 fail / 61 ignored; see corrections and load controls above |
| `cargo test -p agent-file-tools --lib -- --test-threads 4` | 5338 pass / 6 fail / 62 ignored, before final view-GC additions; only environment/load failures listed below remained |
| Final lib `db::` | 76 pass / 2 ignored |
| Final lib `lsp::completed_rust_check::tests` | 31 pass / 1 manually ignored |
| Final lib `views::` | 184 pass / 7 ignored, including final GC and cold-fallback reconciliation |
| Final lib `subc::tool_provider` | 41 pass / 1 ignored |
| Final lib future-task, health-retention budget, scheduled retention controls | 6 + 1 + 1 pass |
| Existing cached-health bound | 1 pass after budgeting fix |
| Integration `bash_background` / `bash_watch` | 88 / 20 pass |
| Integration `db_` / `migration` / `inspect` / `semantic` | 52 / 10 / 282 / 68 pass; inspect 6 ignored, semantic 4 ignored |
| Integration `callgraph` | 96 pass / 10 known linked-worktree failures / 2 ignored |
| `cargo test -p agent-file-tools --test semantic -- --test-threads 4` | 52 pass / 3 ignored |
| Rest `callgraph` after explicit pins | 85 pass / 6 ignored |
| Feature-enabled rest `tool_provider_conformance` | 19 pass; Prepared/Authorized crash hooks actually ran |
| Rest `tool_provider_subc_e2e` | NOT a pass: 0 pass / 1 ignored; requires a real ck-subc 0.29+ daemon and AFT_E2E_SUBC_BIN |
| `cargo test -p agent-file-tools --bin aft -- --test-threads 4` | 121 pass |
| Mandatory real analyzer restart test with `AFT_TEST_REQUIRE_RUST_ANALYZER=1` | 1 pass, not a silently skipped server test |
| `cargo test -p agent-file-tools --test list_envelope -- --test-threads 4` | 175 pass |
| `python3 benchmarks/aft-search/run_search_quality.py --self-test` | Goldens OK; 123 unittest cases / 1 platform skip |
| Bridge build, typecheck, unit | `bun run --cwd packages/aft-bridge build`, `typecheck`: exit 0; `test:unit`: 797 pass / 0 fail / 3 skip across 67 files |
| `bun run lint` | Checked 699 files, exit 0 |
| `bun run lint:workflows` / `bash -n scripts/rust-test-gate.sh` | Exit 0; actionlint 1.7.12 invoked on 10 workflow files; Bash parsed 1 changed script |
| Scoped bridge diagnostics | Authoritative diagnostics for 54 files: 0 errors / 1 pre-existing unused ImportMeta warning; Tier-2 analysis PARTIAL |

The three load-sensitive tests `finished_tasks_release_their_io_descriptors`, `semantic_corpus_refresh_rechecks_tree_after_quiet_window`, and `publication_busy_readiness_uses_backoff_and_recovers_after_unlock` each passed five isolated runs and the full four-thread run. Initial first-load subprocess, database replay, inspect-budget/shutdown, and package-cache timing failures passed their narrower/final reruns. No timing assertion was widened.

### Remaining local failures and limitations

Operator-approved baseline/environment limitations, not hidden passes:

- `gh_shim::relay_client::tests::a_changed_binding_generation_triggers_a_new_check` — unchanged fixture rejected by the default-storage guard under this throwaway local environment.
- `gh_shim::relay_client::tests::a_live_ticket_relays_the_governed_envelope_and_prints_the_url` — same default-storage guard.
- `gh_shim::relay_client::tests::assertion_refusals_retry_once_for_a_fresh_mint` — same default-storage guard.
- `gh_shim::relay_client::tests::transient_refusals_retry_twice_with_the_same_nonce` — same default-storage guard.
- `standing_roots::tests::daemon_startup_empty_pass_marks_existing_snapshot_strict_on_first_configured_pass` — same default-storage guard. The operator confirmed train 340's normal-checkout CI lib gates were green and directed no unrelated fixture edits.
- `watcher_filter::tests::busy_folder_that_is_not_ignored_is_never_excluded` — unchanged timing failure in the full four-thread run; isolated rerun passed.
- `callgraph_test::{callgraph_aliased_import_resolution,callgraph_callers_cross_file,callgraph_callers_empty_result,callgraph_callers_recursive,callgraph_cross_file_tree,callgraph_depth_limit_truncates,callgraph_impact_multi_caller,callgraph_impact_symbol_not_found,callgraph_ops_return_building_then_ready_async,callgraph_unknown_symbol_error}` — all static in-tree fixtures classified as linked-worktree/read-only; return `callgraph_unavailable/read_only_store_not_built`. The operator confirmed this known limitation; canonical writer-checkout CI is their gate. Production classification was not changed.

## Search-quality contract

`train-343.json` is copied from the most recent engine_unwired train descriptor (340). Class derived from the actual base-to-train path diff against RANKING_FENCE_PREFIXES. Matches: `commands/semantic_search/memo.rs`, `lib.rs`, `search_index.rs`, and `semantic_index.rs`, all under `crates/aft/src`. Class is `engine_unwired`, targeted_mechanism is `none`; all 93 rows must be byte-identical in CI.

An actual local replay was attempted, not simulated with reference scores. It refused before evaluation: `corpus_missing:ripgrep:run=python3 benchmarks/aft-search/provision_corpus.py`. Direct real-query replay also requires actual exact-family output. The operator directed delivery with this fixture limitation and identified the CI engine_unwired byte-identity gate as the real gate. Local 93-row equality is NOT claimed.

## Mutation evidence

All controls staged the live implementation first, confirmed empty unstaged diff, captured a non-empty mutant diff, ran the named test, restored with `git checkout -- <paths> && touch <paths>`, and captured an empty unstaged diff. No NON-VACUITY BREAK remained. Detailed machine-readable evidence is in the delivery record.

- Omit ledger migration 15: only `migration_v15_preserves_v14_remote_exec_policies` fails.
- Disable retention detail budgeting: only `health_retention_reports_fit_budget_with_exact_omissions_and_totals` fails.
- Disable dead-cache removal: only `configure_scheduled_storage_sweep_reaps_dead_inspect_scope_after_grace` fails.
- Treat every elicitation response as allow: only `s1_untrusted_keyed_shell_denial_and_elicitation_errors_never_spawn` fails; the no-elicitation admission test stays green.
- Stop retaining a referenced owner's manifest: only `storage_retention_shared_derived_owner_keeps_manifest_until_unreferenced` fails.
- Disable NotFound-only cold fallback: only `missing_shared_owner_manifest_falls_back_to_a_cold_build` fails.
