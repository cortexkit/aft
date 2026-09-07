# AFT path, environment, and substituted-value audit — 2026-09-06

## Scope and method

This audit covers every runtime role named in the request: supervised `aft --subc`, standalone NDJSON, the `gh` shim, `aft profile`, `aft warmup`, and the bridge/CLI TypeScript path code. A ladder is compared as ordered **(rung, guard)** pairs, not as a set of strings. Named constants were resolved before comparison.

The daemon source used for re-derivation is `subconscious` commit `d5e09914b0791a66f2a5a00a9bb3422860ade95e`, read from the sibling checkout during this audit. The references below are to that checkout.

Severity vocabulary:

- **quiet** — the command succeeds at the wrong location or emits a plausible false fact;
- **red-misattributed** — an error is raised later and names the consequence rather than the bad derivation;
- **red-by-name** — the resolver rejects the bad rung directly and identifies it.

## Daemon reference ladders (read, then cite)

| Question | Ordered daemon `(rung, guard)` pairs | Source evidence |
|---|---|---|
| connection-file candidates | `(explicit, present)`, exclusive; otherwise `(SUBC_CONNECTION_FILE, non-empty)`, exclusive; then `(XDG_RUNTIME_DIR/CONNECTION_FILE_NAME, non-empty XDG)`; `(HOME/PROD_CONNECTION_RELATIVE_PATH, non-empty HOME)`; `(temp_dir/CONNECTION_FILE_NAME, always)` | `crates/subc-transport/src/connection_file.rs:50-96`; constants resolved at `:9-12`. The `ck` wrapper delegates directly at `crates/subc-core/src/bin/ck.rs:1076-1081`. Bootstrap writes the same resolved path into module environment at `crates/subc-core/src/bootstrap.rs:440-464`. |
| config home/default config | `(XDG_CONFIG_HOME/DAEMON_CONFIG_RELATIVE_PATH, non-empty XDG)`; Windows only `(APPDATA/DAEMON_CONFIG_RELATIVE_PATH, non-empty APPDATA)`; Windows only `(USERPROFILE/AppData/Roaming/DAEMON_CONFIG_RELATIVE_PATH, non-empty USERPROFILE)`; `(HOME/.config/DAEMON_CONFIG_RELATIVE_PATH, non-empty HOME)`; `(.config/DAEMON_CONFIG_RELATIVE_PATH, always)` | `crates/subc-core/src/daemon_config.rs:116-159`; `DAEMON_CONFIG_RELATIVE_PATH = cortexkit/subc.jsonc` at `:9`. |
| data home and module directory | `(XDG_DATA_HOME, non-empty)`; Windows only `(APPDATA, non-empty)`; Windows only `(USERPROFILE/AppData/Roaming, non-empty)`; `(HOME/.local/share, non-empty)`; `(.local/share, always)`, then append `(cortexkit/modules/<module-id>, normalized module id)` | `crates/cortexkit-store-types/src/lib.rs:62-100` and `:130-134`. `MODULES_DIR` and `TOOLKIT_DIR` resolve to `modules` and `cortexkit` at `:18-19`; the module id normalization guard is at `:102-128`. |

Positive controls: searching the sibling source for `XDG_RUNTIME_DIR`, `DAEMON_CONFIG_RELATIVE_PATH`, and `XDG_DATA_HOME` found the known first rungs at the cited lines. Therefore a stated missing rung below is not a bare grep absence.

## AFT path ladders by role

### Rust binaries

| Role / value | Ordered `(rung, guard)` pairs after fixes | Result |
|---|---|---|
| all Rust AFT storage (`bash_background::storage_dir`) | `(AFT_STORAGE_DIR, non-empty)`; `(configured root, present and non-empty)`; `(AFT_CACHE_DIR/aft, non-empty compatibility rung)`; data home: `(XDG_DATA_HOME, non-empty)`, Windows `(LOCALAPPDATA, non-empty)`, Windows `(USERPROFILE/AppData/Local, non-empty)`, `(HOME/.local/share, non-empty)`, `(.local/share, always)`; append `cortexkit/aft` | Matches daemon guards except for the documented Windows rung. AFT retains LOCALAPPDATA/AppData/Local because its indexes, backups, and checkpoints are cache-class storage and existing standalone installs already live there. Relative values use the real cwd when available; failure preserves the original relative spelling instead of inventing a temp path. |
| tilde/relative normalization | `~` uses Windows `USERPROFILE → HOME`, non-Windows `HOME → USERPROFILE`, then OS home if available; `~/x` same; absolute unchanged; relative joins a successfully read cwd | Empty home values are unset. Missing home/cwd preserves the supplied path rather than substituting temp. This syntax expansion is not the daemon data-home ladder. |
| supervised `aft --subc` config home | `(XDG_CONFIG_HOME/cortexkit/aft.jsonc, non-empty absolute XDG)`; `(HOME/.config/cortexkit/aft.jsonc, non-empty HOME)` | **Remaining copy: becomes a call on the config-home wave.** Exact replacement: non-empty XDG; Windows non-empty APPDATA; Windows non-empty USERPROFILE + `AppData/Roaming`; non-empty HOME + `.config`; relative `.config`. Site is dated in `subc_config.rs`/the shared TS copy; no speculative duplicate implementation was added. |
| `--subc` connection | daemon supplies the connection as launch/bootstrap state; AFT authenticates that supplied transport rather than discovering another file | No local discovery ladder in the supervised role. Positive control: `SUBC_CONNECTION_FILE` occurs in bridge config docs and daemon bootstrap, while `run_subc_mode` receives an established channel. |
| standalone NDJSON | storage ladder above; no connection file | Correct by role: standalone does not connect to SUBC. |
| `gh` shim connection | trusted user config path, then JSONC `subc.connection_file` if a non-empty absolute value | **Remaining copy: becomes `subc_transport::connection_file::discover(explicit)` on that wave.** It will replace this with explicit-exclusive; non-empty `SUBC_CONNECTION_FILE` exclusive; non-empty runtime dir; non-empty HOME data path; temp. The dated procedure is beside `configured_connection_file`. |
| `gh` shim state | `(AFT_GH_SHIM_STATE_DIR, non-empty and absolute)`; `(XDG_STATE_HOME, non-empty and absolute)`; `(HOME, non-empty)/.local/state`; temp dir - each `/cortexkit/aft/gh-shim` | Deliberate divergence, recorded (an earlier revision of this audit moved it onto `storage_dir(None)/gh-shim`; reverted before landing). The shim is not the supervised module: it runs in the agent's child process with the operator's environment, and every governed seat's placed manifest, version high-water, rung cache and bypass audit live at this rung, written by the activation ceremony. Re-rooting it would start each seat empty, and an empty state directory reads as unmanifested, which is transparent passthrough under operator credentials. Empty-value guards applied. |
| `aft profile` connection | `gh_shim::configured_connection_file` | Becomes the same discovery call; no third ladder was introduced. |
| `aft profile` dSYM cache | `storage_dir(None)/dsym/<normalized-debug-id>` | Fixed from unconditional `HOME/.local/share/...`; all shared storage/platform rungs now apply. |
| `aft warmup` model cache | preserve non-empty `FASTEMBED_CACHE_DIR`; otherwise publish `storage_dir(None)/models` to the child | Empty assignment is now unset. The embedder itself uses non-empty FASTEMBED, HOME, USERPROFILE, then OS home; if none exists it returns absence/error rather than a temp-home fiction. |
| LSP executable override | `AFT_LSP_BIN_<SERVER-ID>` when non-empty, else PATH/well-known discovery | Empty override is now unset through an injected lookup helper. |
| agent shim directory | existing PATH minus the non-empty previous `AFT_GH_SHIMS_DIR`; generated temp shim directory prepended when enabled | Empty marker is unset. No classifier, manifest, or trust-set change. |
| agent hook directory | hook source selected from the installed package/executable layout; generated hook directory is temporary; child HOME/PATH are inherited/scrubbed, not re-derived as storage | No daemon-equivalent question and no conflicting AFT resolver found. Positive control: `AFT_GH_SHIMS_DIR` found the adjacent shim ladder, so this is a reasoned null. |

Every remaining Rust copy carries, at the site, `last re-derived 2026-09-06 against d5e09914b0791a66f2a5a00a9bb3422860ade95e` and the procedure: compare ordered variables, platform gates, non-empty guards, and then resolve constants. Copies scheduled to become calls additionally list the exact replacement ladder.

### TypeScript

| Site / value | Ordered `(rung, guard)` pairs after fixes |
|---|---|
| `storage-paths.ts::resolveDataHome` | non-empty XDG; Windows non-empty LOCALAPPDATA; Windows non-empty USERPROFILE + `AppData/Local`; non-empty HOME + `.local/share`; relative `.local/share` |
| `resolveCortexKitStorageRoot` | non-empty `AFT_STORAGE_DIR`; non-empty `AFT_CACHE_DIR/aft`; otherwise data home + `cortexkit/aft` |
| `resolveAftStorageRoot(configured)` | non-empty `AFT_STORAGE_DIR`; non-empty configured; non-empty `AFT_CACHE_DIR/aft`; otherwise data home + `cortexkit/aft` |
| `cache-paths.ts` | call `resolveAftStorageRoot(configured)`, then append the cache-specific subtree |
| `paths.ts` config home | non-empty XDG; otherwise Node `homedir()` + `.config`; dated temporary copy, replaced by the daemon config-home callable with the exact five-rung list above |
| `transport-factory.ts` / `subc-transport.ts` | caller-provided `connectionFile`, required and non-empty by construction; no ambient discovery | Becomes daemon `discover(explicit)` when the callable lands; today the caller's explicit value is exclusive. |
| CLI `lib/paths.ts` | direct call to bridge `resolveCortexKitStorageRoot` | Fixed: removed an independent XDG/HOME copy. |

Rust and TypeScript now agree rung-for-rung on the AFT storage question. Tests pass an environment lookup plus an explicit `windows|other` platform, cwd, and home; neither suite mutates process-global environment, and both platform arms run on this macOS host.

## Path findings and fixes

| Finding | Tier | Difference over `(rung, guard)` pairs | Fix and defending test |
|---|---|---|---|
| P1 AFT Windows data home differs from daemon APPDATA/`AppData/Roaming` | deliberate divergence, recorded | AFT uses LOCALAPPDATA and `USERPROFILE/AppData/Local` with the same non-empty/platform guards | Retained with no migration: indexes, backups, and checkpoints are cache-class storage, Roaming profiles must not sync them, and changing the shipped rung would orphan existing standalone state. Defended by `storage_ladder_matches_daemon_except_for_stable_windows_cache_class_storage` and `matches daemon except for stable Windows cache-class storage and ignores empty values`. |
| P2 TS configured storage ignored legacy `AFT_CACHE_DIR` while Rust honored it | quiet | missing AFT rung | added cache compatibility rung below configured root; plugin tests clear it when asserting configured behavior |
| P3 `gh` state independently used XDG state home | quiet | two AFT resolvers answered the same state-root question differently | derive from shared storage; `state_dir_uses_dedicated_override_then_shared_storage_and_ignores_empty_override` |
| P4 profile dSYM cache hard-coded HOME | red-misattributed | missing all storage override and Windows rungs | call shared storage root; injected-root test |
| P5 empty path variables were accepted at scattered runtime sites | quiet / red-misattributed | present-without-non-empty guard | central non-empty lookup plus resolver-local injected lookups; tests cover storage, LSP, FASTEMBED, Synapse capture, and the shared guard |
| P6 config-home copies lack daemon Windows/final-relative rungs | quiet | missing guarded rungs | intentionally not patched; becomes daemon callable, exact replacement ladder recorded |
| P7 `gh`/profile connection is config-only rather than daemon discovery | red-by-name | missing ambient/runtime/home/temp candidates | intentionally not patched; becomes `discover(explicit)`, exact replacement ladder recorded |
| P8 relative/tilde storage substituted temp when cwd/home lookup failed | quiet | synthetic fallback not present in daemon | preserve supplied relative spelling/absence instead of inventing temp |

Constraints check: gh classifier, manifests, and trust set are unchanged.

## Supervised environment-read audit

The supervisor contract is: inherited daemon environment (which may be launchd-minimal), overlaid by the module `env` block, plus guaranteed `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE`. Therefore only those two identity variables are supervisor-guaranteed; every other variable below is optional unless explicitly present in module config.

Production reads, grouped without omitting dynamic families:

| Purpose | Variables read | Supply/result |
|---|---|---|
| shared paths | `AFT_STORAGE_DIR`, `AFT_CACHE_DIR`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `APPDATA`, `LOCALAPPDATA`, `HOME`, `USERPROFILE`, `FASTEMBED_CACHE_DIR`, `AFT_SYNAPSE_CAPTURE_DIR`, `AFT_GH_SHIMS_DIR`, `AFT_GH_SHIM_STATE_DIR` | optional inherited/module env; all path rungs now reject empty values where they participate in a ladder |
| process/tool discovery | `PATH`, `PATHEXT`, `SHELL`, `BASH`, `LOCAL`, `ComSpec`, `COMSPEC`, `AFT_DISABLE_WELL_KNOWN_LOOKUP`, `AFT_LSP_BIN_*` | optional; absence invokes explicit discovery/refusal. Empty LSP override is now absent. `AFT_LSP_BIN_*` is the only dynamic path-key family. |
| LSP/semantic runtime | `AFT_LSP_WORKSPACE_ONLY`, `AFT_LSP_TRACE`, `AFT_ORT_DYLIB_PATH`, `ORT_DYLIB_PATH`, `AFT_ORT_STRATEGY`, `AFT_INSPECT_POOL_THREADS`, `AFT_CONTEXT_WORKERS`, `AFT_RUST_SEMANTIC_QUIET_WINDOW_MS`, `AFT_SEMANTIC_QUIET_WINDOW_MS`, `AFT_SEMANTIC_RETRY_BACKOFF_MS`, `AFT_PLATFORM_VERIFIER_TLS_URL`, `AFT_PLATFORM_VERIFIER_TLS_CHILD`, `SSL_CERT_FILE`, and the configured semantic API-key variable name | optional feature configuration; missing values disable/refuse the feature rather than assert a path |
| shell/terminal/sandbox | `AFT_DISABLE_PTY`, `AFT_BASH_KILL_GRACE_MS`, `AFT_BASH_TASK_TTL_SECS`, `AFT_BASH_TASKS_PER_SESSION`, `AFT_BASH_TASKS_GLOBAL`, `AFT_BASH_OUTPUT_BYTES`, `AFT_BASH_PTY_IDLE_SECS`, `TERM`, `TERM_PROGRAM`, `TMUX`, `COLORTERM`, `FORCE_COLOR`, `NO_COLOR`, `CI`, `SSH_AUTH_SOCK` | optional; absence selects documented capability limits or removes optional forwarding |
| git/config identity | `AFT_CONFIG_SOURCE`, `AFT_ACTIVE_PROJECT_ROOT`, `AFT_GIT_WRITE_MODE`, `AFT_GIT_PATH`, `AFT_GIT_TEMPLATE_DIR`, `AFT_GIT_SYSTEM_CONFIG`, `AFT_GIT_GLOBAL_CONFIG`, `AFT_GIT_CONFIG_NOSYSTEM`, `AFT_GIT_PROTOCOL_FROM_USER` | optional; explicit controls or scrubbed child inputs |
| gh credentials/config | `GH_TOKEN`, `GITHUB_TOKEN`, `GH_ENTERPRISE_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GH_HOST`, `GH_CONFIG_DIR`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` | optional; credentials are checked for presence to enforce policy, not interpreted as paths; config paths reject empty values after the shared guard fixes |
| supervised identity | `SUBC_MODULE_ID`, `SUBC_LAUNCH_NONCE`, `AFT_SUBC_AUTH_TAMPER`, `AFT_SUBC_AUTH_DELAY_MS`, `AFT_SUBC_AUTH_POST_WRITE_DELAY_MS` | first two guaranteed; remaining keys are explicit diagnostics/test seams and are never assumed present |
| host/user identity | `USER`, `USERNAME`, `HOSTNAME`, `COMPUTERNAME` | optional. Unix hostname uses the kernel query first; the environment is only a fallback after that true source fails. Missing identity remains explicitly `unknown-host` only in local lock diagnostics, never a discovered filesystem root. |
| compiled diagnostic seams | `AFT_TEST_FORCE_RESERVE_DENY`, `AFT_TEST_HOOK_DELAY_MS`, `AFT_TEST_LSP_WARMUP_DELAY_MS`, `AFT_TEST_RETAIN_FS_CALLBACK`, `AFT_TEST_LSP_STOP_TIMEOUT_MS`, `AFT_TEST_PUBLISH_PARTIAL_OBSERVED`, `AFT_TEST_LOGIN_SHELL_CANDIDATES` | not supervisor inputs; deterministic seams with inactive defaults |

Live measurement: `pgrep -f 'ck-aft --subc'` returned no PID, so no live supervised environment existed to inspect. Positive control `ps -E -p $$` returned PID `57564` and `/bin/bash -c ...`, proving the same `ps -E` mechanism could observe a live process. This null is due to process absence, not a failed query. No claim that launchd supplies HOME/HOSTNAME is made.

## Substituted-value sweep

The production-prefix sweep (each file cut at its first `#[cfg(test)]`) checked `unwrap_or_else(|_|`, `unwrap_or_default()`, `unwrap_or("`, `.ok().unwrap_or_default()`, and `.ok().flatten()`. It found **1,142 raw textual hits** in the delegated exhaustive scan. Classification applied one question only: *does the substituted value leave the process and assert a specific fact?* Local parser defaults, loop-control defaults, log-only text, explicit `unknown` labels, and refusal messages were non-findings.

Positive controls were `subc_format.rs`'s former `{}` serialization fallbacks and `health_digest.rs`'s former empty view identity. A search for the same patterns in the production prefix found those known examples, so the null classifications are not bare absence. `health.rs`, `fleet_status.rs`, and the monolithic `memory.rs` census return unavailable/null or prior snapshots for failed reads; no healthy-looking memory count substitution was found in the current checkout.

`commands/status.rs` is a null result: its legacy numeric projection is guarded by `StatusBarCountValues::legacy_projection`, which returns `None` unless every independent producer supplied a value; the status payload emits JSON null rather than clean-looking zeros. Positive control: response finalization tests that supplied only Tier-2 counts previously expected `E0 W0` and now assert absence.

| Finding/site | What crossed the boundary | Tier | Fix / test |
|---|---|---|---|
| S1 response finalization legacy tests assumed missing diagnostics were `E0 W0` | an incomplete fleet/status segment asserted a clean diagnostic count | quiet | production already suppresses the legacy segment until every count is present; tests now assert absence/empty publication rather than canonizing zero |
| S2 `commands/health_digest.rs` defaulted a missing view scope to `""` | an artifact-generation ticket with a false identity | quiet | omit the entire ticket unless both generation and non-empty identity exist; `missing_view_identity_omits_the_ticket_instead_of_substituting_an_empty_name` |
| S3 callgraph handlers used `serde_json::to_value(...).unwrap_or_default()` | serialization failure became successful `null` result or empty candidate data | red-misattributed | shared serialization helper returns a named `serialization_error`; `serialization_failure_returns_an_error_instead_of_a_successful_null` |
| S4 `subc_format.rs` used `{}`/empty collections/default paths for malformed successful responses | rendered output asserted no records, an update operation, source/destination paths, or empty JSON | red-misattributed | explicit formatting-failure text and required-collection preflight; `serialization_failure_is_explicit_instead_of_substituting_an_empty_object`, `missing_patch_and_move_facts_are_reported_instead_of_substituted`, `missing_callgraph_collection_is_reported_instead_of_rendered_as_empty` |
| S5 profile raw-sample timestamp defaulted clock failure to zero | a filename asserted Unix second zero | red-misattributed | clock failure is absence/error before writing; `pre_epoch_timestamp_is_rejected_instead_of_becoming_zero` |
| S6 storage/profile/embed paths used cwd/temp/HOME stand-ins | emitted paths asserted a location that was never resolved | quiet / red-misattributed | shared true-source ladders; missing cwd preserves relative spelling, missing embed home returns error, profile derives shared root; injected path tests listed under P1–P5 |

No outbound factual substituted-value finding remains. Values that remain visibly labeled `unknown`, `unavailable`, or `serialization failed` are absence/refusal text, not plausible facts.

## Mutation-red ledger

The tests above were mutation-checked with the required stage → mutate → non-empty diff-stat → named test red → checkout restore → touch → empty diff-stat sequence. Exact captured reds are recorded here after verification and mirrored in the delivery result:

- Rust Windows storage divergence, mutation `LOCALAPPDATA → APPDATA`: `storage_ladder_matches_daemon_except_for_stable_windows_cache_class_storage` failed with `/wrong-roaming-data/cortexkit/aft` instead of the relative fallback; no other test failed.
- TS Windows storage divergence, mutation `LOCALAPPDATA → APPDATA`: `storage path ladder > matches daemon except for stable Windows cache-class storage and ignores empty values` failed with `wrong-roaming-data/cortexkit/aft` instead of the relative fallback; no other test failed.
- gh state, mutation inserted `wrong-root`: `state_dir_uses_dedicated_override_then_shared_storage_and_ignores_empty_override` failed with the wrong subtree; no other test failed.
- shared empty guard, predicate inverted: `injected_lookup_treats_empty_environment_values_as_unset` failed with `Some("")` versus `None`; no other test failed.
- LSP empty guard, predicate inverted: `empty_lsp_binary_override_is_unset_without_mutating_the_process_environment` failed with `Some("")` versus `None`; no other test failed.
- embed cache empty guard, predicate inverted: `empty_fastembed_and_home_rungs_are_unset_with_an_injected_lookup` failed with `Some("")` versus `/profile/.cache/fastembed`; no other test failed.
- warmup FASTEMBED guard, predicate inverted: `empty_fastembed_cache_assignment_is_unset_with_an_injected_lookup` failed its unset assertion; no other test failed.
- managed ONNX override guard, predicate inverted: `empty_onnx_runtime_override_is_unset_with_an_injected_lookup` failed its unset assertion; no other test failed.
- Synapse capture guard, predicate inverted: `empty_synapse_capture_directory_is_unset_with_an_injected_lookup` failed with `Some("")` versus `None`; no other test failed.
- profile cache, subtree changed to `wrong-cache`: `debug_cache_is_derived_from_the_injected_shared_storage_root` failed with `wrong-cache/ABCD` versus `dsym/ABCD`; no other test failed.
- profile timestamp, absent time changed back to zero: `unavailable_profile_timestamp_is_not_substituted_into_an_outbound_path` failed after receiving `aft-profile-42-0.unsymbolicated.txt`; no other test failed.
- health identity, missing identity changed back to `""`: `missing_view_identity_omits_the_ticket_instead_of_substituting_an_empty_name` failed with an `ArtifactGeneration { identity: "" }` ticket; no other test failed.
- command serialization, errors changed back to successful null: `serialization_failure_returns_an_error_instead_of_a_successful_null` failed at `assertion failed: !response.success`; no other test failed.
- formatter serialization, error text changed back to `{}`: `serialization_failure_is_explicit_instead_of_substituting_an_empty_object` failed with `{}` versus the named serialization error; no other test failed.
- callgraph collection preflight disabled: `missing_callgraph_collection_is_reported_instead_of_rendered_as_empty` failed with `3 paths · 0 entry points / No entry paths found`; no other test failed.
- patch path absence changed back to `Updated (file)`: `missing_patch_and_move_facts_are_reported_instead_of_substituted` failed against the explicit omission message; no other test failed.

The placeholders are replaced with captured test names and output before commit.

## Integration gate

The required bare command was run after the audit changes:

```text
cargo test -p agent-file-tools --test integration
test result: FAILED. 1682 passed; 20 failed; 12 ignored; 0 measured; 0 filtered out; finished in 195.69s
```

Status-related failures exposed assertions that still treated unavailable diagnostics as `E0 W0`, exposed the temporary attempt to publish partial worktree Tier-2 state, or waited for a stale bit inside an unavailable status bar. The status command retains its existing all-producers-ready gate; SUBC assertions now expect status absence when only Tier-2 data is available, and the watcher assertion accepts an absent bar until all producers report. All five affected tests pass individually:

- `linked_worktree_skips_automatic_tier2_and_leaves_parent_gate_open`
- `linked_worktree_explicit_inspect_keeps_parent_aggregates_byte_identical`
- `subc_bridge_discovered_status_line_publishes_over_consumer_connection`
- `subc_bridge_response_finalizer_status_bar_and_bg_completion_once_per_epoch`
- `subc_bridge_watcher_stale_maintenance_push`

The only remaining failures are 19 callgraph tests that target fixtures inside this linked task worktree and receive the worktree read-only `callgraph_unavailable` response. Positive control: `callgraph_unknown_symbol_error` reproduces that environmental limitation in isolation.

No parity fixture moved. `cargo test -p agent-file-tools --test integration tool_call_parity_test` passed all 12 tests exercising the 69 pinned fixtures: `12 passed; 0 failed; 1702 filtered out`.
