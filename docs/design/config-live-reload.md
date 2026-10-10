# Live config reload for group A keys (design, as built)

Status: approved and built. The operator's review answers are folded in (§3, §7).

Input: [`docs/investigations/config-live-reload-inventory-2026-09.md`](../investigations/config-live-reload-inventory-2026-09.md) (the "inventory"). Group A is the inventory's §7 A list. Groups B and C keep today's behaviour: they apply at the next connect/configure or at a host restart. The path prefixes `R:` (`crates/aft/src/`), `OC:` (`packages/opencode-plugin/src/`), `PI:` (`packages/pi-plugin/src/`) and `BR:` (`packages/aft-bridge/src/`) mean the same as in the inventory. Code references name the symbol; line numbers are given where they help.

## 0. Summary

- Rust and each TS plugin process watch the config files **independently**. Each applies only the group A keys **it reads**. No new cross-process channel is added, so standalone and subc behave the same way.
- On a change:
  - both files are re-read and re-resolved through the unchanged resolver (same trust boundary, same harness);
  - the result is diffed against the **published** config;
  - only group A leaves are copied into a clone of the published config, which is then published with a compare-and-swap;
  - changed group B and C leaves are logged as deferred and applied by the next ordinary configure.
- Rust runs the reload on a `MaintenanceCommit` job, which holds the actor's read gate. It never uses the `Mutating` writer barrier and never enters `handle_configure`.
- Each request pins one config snapshot when it is admitted (`AppContext::pin_config`, installed in `R:main.rs` `dispatch`). A publication therefore switches over at a request boundary. This is what makes live changes to `restrict_to_project_root` and `sandbox.*` safe.
- The live path is **conservative** and never loosens because of a bad file:
  - a file that is invalid, unreadable **or deleted** keeps the last good config;
  - the error is logged at ERROR and, in the plugins, shown to the user through the existing warning channel;
  - nothing falls back to defaults.

  Connect stays **authoritative** and unchanged: it resolves a missing file as `{}` and parses leniently.

## 1. What is watched, and by whom

### 1.1 Rust engine (`R:config_live.rs`)

**Content gate.** Each root records the texts its last configure (or reload) applied: `ConfigSources`, recorded by `handle_configure` through `stage_configure_sources` / `finish_configure`.
- A reload with the same `(user text, project text)` is a no-op (`ReloadOutcome::Unchanged`).
- Watch events only wake the check, so duplicate or spurious events cost one file read.
- A tier relayed on the wire whose file does not exist (older plugins) is kept as it was. A tier counts as read from its file when its `source` names that file; the daemon's bind relays the files this way.

**User file `~/.config/cortexkit/aft.jsonc`.**
- **Daemon (subc).** One process-level `ConfigFileWatch` (`start_process_user_config_watch`, started in `R:subc/mod.rs` `run_subc_mode`) on the path resolved once at startup. On a change, `request_reload_for_roots` asks every registered actor to reload. The path is also recorded process-wide, so root contexts do not each start a watch, and their `ConfigSources` know where the user file is.
- **Standalone.** One `ConfigFileWatch` per context, on the `cortexkit_user_config_path` configure parameter. There is no watch when the parameter is absent.

**Project file `<root>/.cortexkit/aft.jsonc`.**
- **Primary: the root's project watcher.** `WatcherFilterConfig` carries the root's `ConfigReloadSignal` (`with_config_reload_signal`). The watcher thread raises it for any raw event path under `<root>/.cortexkit/` named `aft.jsonc`, or for an editor sibling of it (`note_config_file_events`). This runs on the raw event, before every corpus filter: the high-churn component filter (which drops any path with a `target`, `node_modules`, `.alfonso`, `.opencode` or `.gsd` component, possibly an ancestor of the root), canonicalization (which would resolve a symlinked file name away) and the ignore rules. Only the root directory of an event path is resolved when comparing it with the canonical root. An overflow/rescan event raises it too. Corpus handling of the path is unchanged.
- **Ignored `.cortexkit`.** `start_project_watcher` adds `<root>/.cortexkit` to the watcher's existing non-recursive `extra_watch_paths` whenever the directory exists. So the file is seen whether or not the project ignores `.cortexkit`, on every backend. This is simpler than computing "covered" from the matcher, as the design first proposed, and is equivalent.
- **Fallback: the root's own `ConfigFileWatch`.** It is used when no project watcher runs (a HOME root, `AFT_TEST_DISABLE_FILE_WATCHER`, a failed watcher), or when `.cortexkit/` did not exist at watcher start.
  - It watches `<root>/.cortexkit/` non-recursively, or `<root>` until `.cortexkit/` appears.
  - It records the watched directory's identity (device and inode on Unix; creation time on Windows, since the file index is not on stable Rust), read before the watch and confirmed after, and re-attaches when the identity changes, checked on every event and every 200 ms tick. A directory renamed aside and replaced under the same name is therefore watched again, and the file is checked at once.
  - An event that names the watched directory itself, or a backend error, forces a re-attach even if the identity looks unchanged: a directory deleted and recreated can get its inode back (ext4 reuses them).
  - `sync_config_watches` starts or drops it after configure and after the watcher maintenance stage.
- **Idle-evicted roots.** `ensure_project_watcher` requests one immediate reload on reattach, and the content gate makes it a no-op when nothing changed.

**Test processes.** Config watches follow `AFT_TEST_DISABLE_FILE_WATCHER=1`, the switch the integration suite already uses because OS watch registration can hang under its load. Unit tests start none unless a test enables them. The reload itself always runs when asked.

**Debounce.** `ConfigReloadSignal` is a trailing 150 ms debounce: every event pushes the due time out again.

### 1.2 TS plugins

A plugin must swap its own snapshot, because the daemon cannot reach a plugin's `ctx.config`.

**Shared component** `BR:config-watch.ts`:
- `watchAftConfigFiles`:
  - watches each file's parent directory, or its nearest existing ancestor until it exists, and retires the ancestor watch once the directory appears;
  - wakes on every event in a watched directory (platforms may coalesce several changes into one event that names a different entry, so filtering by name could lose a config edit);
  - uses a 150 ms trailing debounce, capped at 1 s so steady unrelated activity cannot postpone a check indefinitely;
  - re-arms a watch whose directory was replaced (inode changed) or that reported an error, checked on every event and every 2 s. The identity is read before `fs.watch` and confirmed after, so a directory replaced while its watch is being set up is not recorded under the new identity;
  - records as seen the texts the load actually accepted (the loader returns them), not the texts the check happened to read, and only after the load accepted them. A rejected text (for example a half-written save) is retried on the next event and up to three times on its own, 1 s apart;
  - calls `onChange` only when a file's text changed.
- `applyLiveConfigKeys` and `aftLiveConfigKeys`: one table of the TS group A keys:
  - `configure_warnings_delivery`, `restrict_to_project_root`, `inspect.diagnostics_timeout_ms` and `inspect.tier2_idle_minutes`;
  - `bash.foreground_wait_window_ms`, `bash.host_fallback`, `bash.subagent_background` and `bash.watch_sync_max_ms`.

  Bash keys are read through the host's `resolveBashConfig`. A write turns `bash` into the equivalent object form, so non-live bash settings (`compress`, `background`, ...) resolve exactly as loaded. The swapped config is a new object.
- `startLiveConfigReload`: load, keep last good on error (reported once per distinct message), swap, and log `config reload applied=[...] deferred=[...] (deferred keys apply on next connect/restart)`. Deferred keys are reported relative to the config loaded at startup.
  - **Deletion** is decided from the loader's own record of the files it read (`getConfigLoadSources`, returned with every load). A load that no longer read a file the last accepted load read keeps the last valid config. The startup load's record (`sources` on the bootstrap result) is the starting point, so there are no separate existence probes to race.
  - **Start-up reconciliation.** The watch starts from the texts the startup load read and checks the files once as soon as it is attached, through the same accepted-text and retry path as later checks. An edit made between the host's config load and the watch (OpenCode 2 awaits bridge setup in between) is applied, and a transient rejection at start-up is retried.
  - **Project edits only tighten** `restrict_to_project_root` and `bash.host_fallback` (`aftLiveSecurityKeys`). When the project file's text differs from the text the published values rest on, a change that would loosen either key is held (`held=[...]` in the log) until the next restart. The TS loader cannot re-resolve with the old project text, so a user-file loosening made together with a project edit is held too.

**Per host.** Each host has a loader (`OC:config-live-reload.ts`, `PI:config-live-reload.ts`) that turns the following into `ok: false`:
- a parse failure (`getConfigLoadErrors`);
- a setting that failed validation (the new `getConfigValidationErrors`; a normal load still keeps the rest of the file);
- a `ConfigRejectedError`.

**Wiring:**
- **OpenCode 1** (`OC:index.ts`): started after `ctx` is built and stopped in the shutdown cleanup. The two startup-captured reads (`inspect.tier2_idle_minutes` for the Tier-2 idle scheduler, and `configure_warnings_delivery` on idle) now read `ctx.config`.
- **OpenCode 2** (`OC:entry/server-runtime.mjs` `bootLocation`): one reload per Location, swapping that Location's `toolContext.config`. It is stopped by the finalizer that releases the Location's bridge.
- **Pi** (`PI:index.ts`): started after `ctx` is built and stopped on `session_shutdown` and process shutdown.
  - `registerPiToolSurface` wraps the surface with `withLiveRestriction`, so the hoisted tools' `restrictToProjectRoot` is read from `ctx.config` at each call.
  - `PI:tools/fs.ts` delete/move read the value once per call.
- **Subc mode**: identical. The plugin process swaps its TS snapshot, and the daemon separately publishes its Rust config from the same files.

A plugin in the startup config-error state starts no watch; that state still needs a restart, as its message says.

## 2. What a change does (Rust)

`reload_config_inner`:
1. **Read** both files (`read_tier`). A read error, invalid UTF-8, or a **deleted** file that existed at the last apply keeps the last good config (§3).
2. **Content gate.** Unchanged texts are a no-op, but a tier that was relayed on the wire and now reads the same text from its file is recorded as file-backed, so a later deletion of it is noticed.
3. **Strict check.** `strict_tier_error` (`R:config_resolve.rs`) is applied to each tier (§3).
4. **Re-resolve** with `resolve_config_onto_with_diagnostics_for_harness` onto a clone of the published config, with the published harness. The trust boundary is unchanged (`record_project_drops`, `merge_project_config`, harness disables only accumulate). Resolver errors keep the last good config.
5. **Diff.** `apply_live_config(published, candidate, connected)`:
   - One explicit table maps each `Config` leaf to a key name, as `live!` or `later!`.
   - `classification_is_exhaustive` destructures `Config` and every mixed sub-struct with no `..`, so a new field does not compile until it is classified.
   - Live leaves that differ from the published config are copied. Deferred leaves that differ from the last configure's resolver output (`connected`) are listed; they stay listed until a connect applies them.
6. **Publish** with `publish_config_if_current`, a compare-and-swap on the `Arc`. If a configure published in between, the resolve is repeated once.
7. **Push setters** (`push_live_setters`):
   - `formatter`/`checker`: `format::clear_tool_cache_for_root`;
   - `backup.max_file_size`: `BackupStore::set_policy`, keeping the configured `enabled`/`max_depth`;
   - `bash.long_running_reminder_*`: `configure_long_running_reminders`;
    - `inspect.enabled` and `inspect.categories`: `reset_tier2_refresh_scheduler`;
    - `lsp.idle_minutes`: read by the next idle-reaper sweep; `"never"` skips that sweep. It uses minutes since the last AFT tool call on that repository, just as the old idle key did. Category workers use their request snapshot; subsequent inspect and automatic refresh submissions skip off categories before opening caches or dispatching jobs.
   - `git.co_author` from `off` to on: `agent_child_env::ensure_git_hooks`, the hook half of `maintain` only (the gh shim is group B);
   - `lsp.diagnostics_on_edit` in the daemon: the maintenance completion carries the new value into `RootMeta`.
7a. **Project edits only tighten.** When the project file's text changed, the files are also resolved with the new user file and the previous project text (`user_only`). Against that floor, `hold_project_loosening` keeps `restrict_to_project_root`, `url_fetch_allow_private`, `sandbox.enabled`, `sandbox.read_deny` and `sandbox.write_allow`, plus the resource budgets `lsp.idle_minutes` (lower only, with `"never"` above every number) and `inspect.categories` (off only), at least as strict: a project edit can add hardening but never remove hardening a project file had put in place, while a user-file change still applies in both directions. A held key keeps its published value, is logged as `held=[...] (not loosened while a project edit is held; the next connect applies them)`, and the previous project text stays the reference so later reloads keep holding it. While any hold is active (`project_hold_active`, cleared by a connect), the published security values are also part of the floor: they may carry hardening from project texts that were added while an earlier edit was held and so were never recorded (project `[A]` → `[B]` → `[]` keeps both A and B). The cost is that, during a hold, a user-file loosening of these keys also waits for the connect. The hold is released when the project file goes back to the recorded text (the held edits no longer exist, so the published config is resolved from the files again) or by a connect. A reload's read-resolve-publish-record sequence and configure's recording of what it applied are serialized (`reload_lock`), so a reload in flight cannot re-arm the hold after a connect cleared it. The next connect resolves the files afresh and is authoritative.
8. **Log** one line: `config reload root=<root> applied=[...] deferred=[...] (deferred keys apply on next connect/restart) dropped=[...] (project may only tighten)`. When sandbox keys change, it adds that running background tasks keep their spawn-time sandbox.

**Not triggered:** `handle_configure`, `defer_to_exclusive_configure`, `agent_child_env::maintain`, the ONNX lookup, `note_configure_warm_key`, artifact drop or reload, hashline binding, and the configure maintenance stages. The configure generation does not change.

**Warm key** (`configure_warm_key`, `R:commands/configure.rs:2452`). `callgraph_chunk_size`, `inspect.enabled`, `semantic.query_timeout_ms` and `semantic.query_instruction` were removed from it; none of them selects an artifact. `handle_configure` still resets the Tier-2 scheduler when `inspect.enabled` flips under an equivalent warm key.

## 3. Invalid, unreadable or deleted config

**Live reload is conservative; connect is authoritative.** The live path keeps the last good config when any of these hold:
- a file cannot be read, is not UTF-8, or **was deleted** (it existed when last applied). Deleting a file is not treated as `{}`: a security key must never loosen because a file vanished mid-save or was deleted by mistake. A file that was already absent at connect stays absent without error;
- the text does not parse as a JSON object (`parse_tier` would silently skip the tier);
- a value does not deserialize strictly into `RawAftConfig`. `parse_config_partially` would reset only that key to its default;
- the block for the **active harness** (`harnesses.<id>`) is not an object or does not deserialize strictly. The resolver would ignore the whole block with a warning, dropping any security key it sets. Blocks for other harnesses are not applied and are not checked;
- the resolver rejects the candidate (for example an invalid disabled-tool name).

Retired keys do not reject a load or reload. Rust and the plugins translate them in memory before validation, in the base block and every embedded harness block, then apply the current keys' ordinary trust rules. A project can only tighten protected user settings; translating a retired name does not grant it more authority. The project file's bytes are never changed by loading or reloading, and a notice explains the translation. This includes the retired `experimental_lsp_ty` and `experimental_bash_*` names and the graduated `experimental.bash` feature block: missing experimental bash flags remain false, rather than acquiring the current default-on values.

Only Rust auto-migrates the user file (`~/.config/cortexkit/aft.jsonc`) on disk, using the same comment-preserving mapping as `aft doctor --fix`, with a backup and one migration notice. A read-only file, or a debug build refusing to write the account's production config directory without explicit opt-in, leaves the file untouched and still uses in-memory translation. Plugins neither rewrite config keys nor relocate legacy config files at load; location migration is an explicit operation. An explicit `aft doctor --fix` may repair the invocation project's file, but ordinary loading never does.

Connect keeps today's behaviour: a missing file resolves as `{}`, and a bad value is dropped with the rest of the file applied. The two therefore disagree on purpose. The next connect after a deletion applies the deletion.

**Surfacing:**
- **Rust:** one ERROR line per distinct reason, ending "keeping the last valid configuration". A fixed file clears the dedupe.
- **TS:** `error()` in the plugin log, plus the host's existing user-facing warning channel with "AFT keeps using the last valid configuration until the file is fixed." appended:
  - OpenCode 1 uses `sendWarning` through `deliverConfigMigrationWarnings`;
  - Pi uses `ui.notify`;
  - OpenCode 2 uses its `notify`, which writes to the plugin log.

  Each distinct message is shown once. The configure-warnings queue is not used: it is drained only on session idle and is keyed by `kind`. The startup-warning channel reaches the user immediately.

## 4. Security keys: switch-over and enforcing side

**Per-request pin.** A request pins the published config when it is admitted, as a thread-local `(context, Arc<Config>)`:
- `dispatch` in `R:main.rs` pins at entry (inline standalone requests and every subc path that goes through it);
- `dispatch_outcome` in `R:main.rs` pins before choosing a route, so the deferred and orchestrated routes (inspect, LSP navigation, foreground bash, GitHub reads, offloaded validation, deferred tool calls) share the admission snapshot;
- `run_tool_call` pins before its preflight, and the subc deferred inspect/LSP job pins before `prepare_tool_call`.

Work a request hands to another thread captures `ctx.config()` at admission and installs it on the worker with `pin_config_to`:
- `handle_dispatch_deferred` (semantic search and deferred tool calls) and `dispatch_with_offloaded_validation` in `R:main.rs`;
- the deferred LSP navigation worker (`R:commands/lsp_navigation.rs`);
- the deferred inspect worker (`R:commands/inspect.rs`), including `run_blocking_inspect_body`.

Other threads a request starts carry no config reads that matter for the switch-over: the inspect Tier-2 category workers read `inspect.tier2_pass_timeout_ms` from the manager's own snapshot (not a security key), `read`'s image worker reads no config, and background bash spawn decisions (`sandbox.*`) are made on the request thread before the child starts. Background maintenance (watcher drains, builds, Tier-2 refresh) is not a request and reads the live snapshot.

Also:
- `AppContext::config()` returns the pinned snapshot on that thread.
- configure's `set_config` replaces the calling thread's own pin, so configure sees its own publication.
- `update_config` (every setter, such as `set_bash_compress_enabled`) builds its snapshot from the one published at that moment, under the configuration write lock, so it can never overwrite a newer publication (a live reload) with values it read earlier. Its closure receives only `&mut Config`. Reading the configuration or publishing from inside the closure would wait on that lock forever, so both panic ("config read inside update_config closure would deadlock"). The only production closure is `set_bash_compress_enabled`'s, which assigns one field; every other `update_config` call is in tests. The reload publishes by compare-and-swap. Only configure's `set_config` replaces the calling thread's own pin; a setter inside a request leaves the request on its admitted snapshot.
- Nested dispatch (tool calls) keeps the outer pin.

**Switch-over point.** A request admitted after publication sees the new value. A request already running finishes on the old one.

| Key | Enforced by | Switch-over |
|---|---|---|
| `restrict_to_project_root` | **Rust is authoritative:** `path_restriction_context` and the semantic search root check. **The TS pre-check** decides between denying without a prompt and prompting: `OC:tools/permissions.ts`; in Pi, the per-call `ctx.config` sites and the live surface getter. | Rust: the first request admitted after publication. TS: the first tool call after the swap. Each call reads the value once. |
| `sandbox.enabled`, `sandbox.write_allow`, `sandbox.read_deny` | **Rust only**, read per spawn (`native_sandbox_enforced`, `sandbox_spawn.rs`, `bash_background`). | Spawns in requests admitted after publication. Background tasks already running keep their spawn-time confinement. |
| `bash.host_fallback` | TS only; every fallback still asks the host for permission. | The first tool call after the TS swap. |
| `url_fetch_allow_private` | Rust only. | Requests admitted after publication. |

**Stricter of both for `restrict_to_project_root`.** The two processes cannot switch in one atomic step. Both watch the same file and switch within one debounce window of each other. Each side denies on its own `true`, so during the gap every request gets the stricter of the two values:
- **Loosening:** whichever side has not published yet still denies.
- **Tightening:** Rust denies as soon as it publishes. The only visible artifact is TS prompting before Rust refuses, and only for calls inside that window.

Both checks must stay independent for this to hold. In OpenCode the read path used to hand every external read to the server under the restriction, so during the gap (TS true, Rust false) an ordinary external read passed unchecked; now only a read the server positively confirms as a session-owned bash artifact passes, and the worktree shortcut does not apply under the restriction.

## 5. Concurrency

- **Daemon.** The reload is the `MaintenanceDrainKind::ConfigReload` drain. It is probed with `config_live().reload_due()`, runs on `Lane::MaintenanceCommit` (actor epoch **read** gate), and is coalesced by `MaintenanceCoalesceKey::ConfigReload`. It runs alongside `PureRead`s and never becomes a writer barrier. It runs only while the root is still bound at the generation the job was queued for (`run_if_subc_bound_generation`); otherwise the request stays pending and the next bind reads the files anyway.
- **Standalone.** The reload runs in the between-request drain (`drain_non_configure_runtime_events`), which is skipped while configure maintenance is pending.
- **In-flight requests** keep their pinned `Arc`. Publication is one `RwLock` write.
- **Configure racing a reload.** They exclude each other through the epoch lock (configure is `Mutating`). Each re-reads the files when it runs, and the reload publishes by compare-and-swap. Whichever runs last applies the newest text. A reload that runs after a configure which already applied the same text is a no-op through the content gate.

## 6. Tests

Rust unit tests (`R:config_live_tests.rs`):
- `live_edit_applies_a_group_a_key_without_reconnect`
- `user_file_edit_applies_live_too`
- `deferred_keys_are_listed_and_not_applied_until_the_next_configure`
- `reload_log_line_names_applied_and_deferred_keys`
- `invalid_edits_keep_the_last_good_config` (truncated JSONC, a non-object, a retired key, one bad value next to good ones)
- `deleted_config_file_keeps_the_last_good_config`
- `absent_file_that_was_absent_at_connect_is_not_an_error`
- `unchanged_file_text_is_a_no_op`
- `project_tier_still_cannot_loosen_on_live_reload`
- `request_pins_its_config_while_a_reload_publishes` (restrict and sandbox)
- `reload_does_not_overwrite_a_configure_that_published_meanwhile`
- `a_user_file_edit_asks_every_live_root_to_reload`
- `signal_debounces_until_quiet`
- `config_file_watch_sees_edits_and_a_directory_created_later`
- `root_without_a_project_watcher_watches_its_config_files_itself`
- `project_watcher_filter_recognises_the_config_file`
- `project_watcher_signals_a_config_edit_under_a_target_ancestor`
- `invalid_active_harness_block_keeps_the_last_good_config`
- `project_edit_cannot_remove_project_hardening_until_the_next_connect`, `project_edit_cannot_turn_off_a_sandbox_the_project_turned_on`, `a_user_edit_can_still_loosen_what_the_user_set`
- `a_tier_file_that_appears_with_the_relayed_text_is_then_file_backed`
- `successive_held_project_edits_keep_all_published_project_hardening`, `successive_held_project_edits_keep_a_project_enabled_sandbox`
- `a_replaced_config_directory_is_watched_again`
- `a_setter_racing_a_reload_does_not_undo_the_reload`, `configure_publication_updates_its_own_pin_but_a_setter_does_not`
- `reading_config_inside_update_config_panics_instead_of_deadlocking`, `publishing_inside_update_config_panics_instead_of_deadlocking`
- `a_directory_replaced_under_the_same_name_is_attached_again`, `a_forced_reattach_watches_the_same_directory_again`
- `reverting_the_project_file_releases_the_hold`

Other Rust tests:
- `R:main.rs` `config_pin_tests::dispatch_keeps_the_config_it_was_admitted_with`;
- `R:commands/configure.rs` `warm_key_ignores_keys_a_live_config_reload_may_change`;
- `R:subc/mod.rs` `deferred_navigation_worker_keeps_the_admitted_config`;
- `crates/aft/tests/watcher_integration/config_live_reload_test.rs`: a real `aft` process and the real project watcher, with no reconnect:
  - `project_config_edit_applies_live_through_the_project_watcher`
  - `project_config_edit_applies_live_when_cortexkit_is_gitignored`
  - `user_config_edit_applies_live`

TS tests:
- `BR:__tests__/config-watch.test.ts`: key table, bash form, invalid files, deletion from the loader's record, a file that appears later, watch recovery after directory replacement, no starvation under unrelated activity, start-up reconciliation, re-arm when the directory is replaced during watch setup, retry of a rejected text, the accepted (not the read) text recorded, a transient start-up rejection retried;
- `OC:__tests__/config-live-reload.test.ts`: the `bash_watch` cap reaches the next call while `bash.background` is deferred; restrict; invalid parse; an invalid value; deletion; project tier clamped; a project edit cannot turn host fallback on but can turn it off;
- `OC:__tests__/permission-layer-audit.test.ts`: under the restriction an ordinary external read is denied unless the server confirms a session-owned artifact, and a worktree path outside the session directory is denied;
- `OC:__tests__/v2-bridge-bootstrap.test.ts`: an OpenCode 2 Location swaps its tool context config and stops with the Location;
- `PI:__tests__/config-live-reload.test.ts`: restrict reaches the registered hoisted tools; bash wait setting live and `compress` deferred; an invalid value.

## 7. Review answers and findings

- **Deletion** keeps the last good config (operator decision; §3).
- **Strictness** applies to the live path only; connect keeps lenient parsing (operator decision).
- **Warm key**: the four keys were dropped, and `inspect.enabled` still resets Tier-2 (operator decision).
- **Project monotonicity** (operator decision after review): a live reload never makes a security key looser than published because of a project edit; §2 step 7a and §1.2.
- **`worktree.ram_overlay`** stays in group A. Finding: an edit made while the overlay is off is **not replayed** when it turns on. The watcher drain skips borrow-only RAM updates while it is off (`R:runtime_drain.rs`, `apply_ram_search_updates`; test `ram_overlay_is_on_by_default_and_transport_independent`). After a live switch to on, search reflects only later edits, or a rescan, until those files change again.
