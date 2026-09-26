# Live config reload for group A keys (design)

Status: design for review. No product code is changed by this note.

Input: [`docs/investigations/config-live-reload-inventory-2026-09.md`](../investigations/config-live-reload-inventory-2026-09.md) (the "inventory"). Group A is the inventory's §7 A list; groups B and C keep today's behaviour and apply at the next connect/configure or host restart. The path prefixes `R:`, `OC:`, `PI:`, `BR:` mean the same as in the inventory. Line numbers were re-checked against base `6dc637eea`.

## 0. Summary

- Rust and each TS plugin process watch the config files **independently**, and each applies only the group A keys **it reads**. No new cross-process channel is added, so standalone and subc behave the same way.
- A change re-reads both files, re-resolves them through the unchanged resolver (same trust boundary, same harness), and diffs the result against the **currently published** config. Only group A leaves are copied into a clone of the published config, which is then published. Group B and C leaves are left untouched, logged as deferred, and applied by the next ordinary configure.
- Rust publishes on a `MaintenanceCommit` job (actor read gate). It never uses the `Mutating` writer barrier and never enters `handle_configure`.
- Each request pins one config snapshot when it is admitted, so a publication switches over at a request boundary. This is what makes live changes to `restrict_to_project_root` and `sandbox.*` safe.
- If a file is invalid or unreadable, the last good config is kept, the error is logged at ERROR, and the TS side reports it through the existing config-warning channel. Nothing falls back to defaults.

## 1. What is watched, and by whom

### 1.1 Rust engine

A reload is identified by a **content gate**: `(user file text, project file text)` compared with the texts last applied to that root. Watch events only wake the check. A spurious or duplicate event, or a save that did not change the text, is therefore a cheap no-op. `handle_configure`'s full path also records the texts it applied, so a reload queued behind a connect that already applied the same content does nothing.

**User file `~/.config/cortexkit/aft.jsonc`.** It gets a new `ConfigFileWatch`: a `notify` watcher, `NonRecursive`, on the **parent directory**. It accepts events whose file name is `aft.jsonc` or an editor sibling of it (`aft.jsonc.*`, `.aft.jsonc*`) and debounces them by 150 ms (trailing). This follows the TUI preferences watcher (`OC:tui/preferences.ts:200-226`), because editors save by rename.
- Daemon (subc): one watch per process on the path resolved once at `R:main.rs:236` (`R:subc_config.rs:48-52`). When it fires, it enqueues a reload for every registered actor (`Executor::actor_entries`, `R:executor/mod.rs:1119`). Unbound or quiesced actors are skipped, because their next bind re-reads the files anyway (`R:subc/mod.rs:5631-5634`).
- Standalone: one watch per `aft` process. The path comes from the `cortexkit_user_config_path` configure parameter (`R:commands/configure.rs:2362-2380`). It is stored on the context at configure and the watch is re-pointed if the path changes. If the parameter is absent (an older plugin), there is no user-file watch; this is logged once.

**Project file `<root>/.cortexkit/aft.jsonc`.**
- **Primary: reuse the root's project watcher.** `filter_canonical_paths` (`R:watcher_filter.rs:1321-1372`) gains a classification, checked *before* the infra and ignore filters, for paths whose parent is `<root>/.cortexkit` and whose name matches as above. These produce a new `WatcherDispatchEvent::ProjectConfigChanged`, modelled on the existing `IgnoreRulesChanged` (`R:watcher_filter.rs:1334-1342`, drained in `R:runtime_drain.rs`). The path's corpus handling is unchanged (it still reaches `changed` only when it is not ignored).
- **When the watcher sees the file.** A root is *covered* when its project watcher is running and the matcher does not ignore `<root>/.cortexkit/aft.jsonc` (`watcher_path_is_ignored_by_matcher`, `R:watcher_filter.rs:1360`). On Linux an ignored `.cortexkit/` gets no inotify watch at all (`R:watcher_backend/mod.rs:21-36`), and the same matcher check catches that case.
- **Fallback 1: ignored `.cortexkit`.** When `.cortexkit/` exists but is ignored, add `<root>/.cortexkit` to the watcher's existing `extra_watch_paths`. `start_project_watcher` builds these at `R:commands/configure.rs:742-762` from `external_ignore_watch_paths` (`:545`), and every backend already watches them non-recursively and outside the exclusion plan (`R:watcher_backend/fsevents.rs:56-61`, `R:watcher_backend/mod.rs:54-58`, `inotify.rs:82`, `windows.rs:467`). Coverage is recomputed whenever the watcher is (re)started, which already happens on a matcher change.
- **Fallback 2: no project watcher.** This covers a HOME root (`R:commands/configure.rs:6184`, `:6214`), `AFT_TEST_DISABLE_FILE_WATCHER` (`:738-740`), a failed watcher, and a `.cortexkit/` created after the watcher started while it is ignored. The root then gets its own `ConfigFileWatch`, the same component as the user file. It watches `<root>/.cortexkit/` non-recursively, or `<root>` itself when `.cortexkit/` does not exist yet, and moves to `.cortexkit/` once the directory appears. The fallback is dropped when the root becomes covered.
- **Idle-evicted roots.** No watch is needed while evicted. `ensure_project_watcher` (`R:commands/configure.rs:774`) runs one content-gate check on reattach and enqueues a reload if the texts differ.

### 1.2 TS plugins

A plugin must swap its own snapshot, because the daemon cannot reach a plugin's `ctx.config`, in standalone or in subc (inventory §6). The plugins already read both files themselves (`OC:config.ts:1991-2053`, `PI:config.ts:1988`).

The group A keys the plugins read (the inventory's `+TS` marks):

| Key | TS read sites that must read `ctx.config` at call time |
|---|---|
| `configure_warnings_delivery` | `OC:index.ts:350-362` (already re-reads), `OC:index.ts:900` (startup `aftConfig`: change it to `ctx.config`) |
| `restrict_to_project_root` | `OC:tools/permissions.ts:441`, `:553`; Pi `PI:tools/fs.ts:176`, `:245`, `navigate.ts:162`, `ast.ts`, `safety.ts:213`, `conflicts.ts:101`, `imports.ts:191`, `inspect.ts:99`, `reading.ts:118`; Pi surface flag `PI:tool-registration.ts:167` used at `PI:tools/hoisted.ts:576`, `:680`, `:740`, `:845`, `:896` (captured at registration: change it to a per-call getter) |
| `inspect.diagnostics_timeout_ms` | `OC:tools/inspect.ts:348`, `PI:tools/inspect.ts:480` |
| `inspect.tier2_idle_minutes` | `OC:index.ts:829` (closure over startup `aftConfig`: change it to `ctx.config`) |
| `bash.foreground_wait_window_ms` | `OC:tools/bash.ts:455`, `PI:tools/bash.ts:527` |
| `bash.host_fallback` | `OC:tools/bash.ts:520`, `PI:tools/bash.ts:645-647` |
| `bash.subagent_background` | `OC:tools/bash.ts:438`, `OC:tools/bash_watch.ts:97`, `PI:tools/bash.ts:560` |
| `bash.watch_sync_max_ms` | `OC:tools/bash_watch.ts:133`, `OC:tools/bash.ts:564`, `PI:tools/bash.ts:804`, `:869` |

Phase 2 re-checks each of these sites. Any site that reads a startup-captured object instead of `ctx.config` is converted.

**Shared component.** A new `packages/aft-bridge/src/config-watch.ts` provides:
- `watchAftConfigFiles({ paths, debounceMs: 150, onChange })`, with the same parent-directory, sibling-name, debounce and text-equality shape as `OC:tui/preferences.ts:200-226`;
- `applyLiveConfigKeys(current, next)`. It is driven by one exported table of group A **TS** leaf paths and returns `{ config, applied, deferred }`. `config` is a *new* object: `current` with only the group A leaves replaced from `next`.

Each host supplies its own loader (OC and Pi keep separate `config.ts` copies).

**Swap.** `ctx.config = config` is a reference swap. Every handler captures `const config = ctx.config` **once** at the start of a tool call, so one call never mixes two snapshots. This matters for loops such as `PI:tools/fs.ts:172-178`, which today re-reads `ctx.config` for each file. The TS side never calls `pool.reconfigure`/`BridgePool.reconfigure` (`BR:pool.ts:457-478`) for a reload, because that would run today's full configure.

**Per host:**
- **OpenCode 1.** Start the watch after `ctx` is built (`OC:index.ts:446-454`) on the user path and on the registration root's project path. The paths come from `resolveAftConfigPaths` (`OC:config.ts:1996`). The watch is disposed with the plugin.
- **OpenCode 2.** Start one watch per Location in `bootLocation` (`OC:entry/server-runtime.mjs:46-117`), swap `toolContext.config` (`:99-106`), and dispose the watch when that Location's runtime stops. A Location reload already re-boots everything, so it needs nothing extra.
- **Pi.** Start the watch after `ctx` is built (`PI:index.ts:770-775`) on the user path and the project path for the extension's cwd.
- **Subc mode.** Identical. The plugin process watches and swaps its TS snapshot, and the daemon separately watches and publishes its Rust config. Both read the same files. The split of which side reads which key is in §4.

A plugin in the startup config-error state (`BR:config-error.ts`, `OC:bridge-bootstrap.ts:352-361`) does not start a watch. That state still requires a restart, as its message says.

## 2. What a change does

For each root (Rust) or plugin context (TS), the steps are:

1. **Debounce.** 150 ms trailing after the last event, then apply the content gate (§1.1). Rust records `config_reload_due_at` on the context; the reload job runs at the next maintenance tick at or after that time.
2. **Read and validate** both files (§3). Any failure stops here and keeps the last good config.
3. **Re-resolve** with the unchanged resolver and trust boundary. Rust uses `resolve_config_onto_with_diagnostics_for_harness(&tiers, published.harness.as_ref(), &mut candidate)` (`R:config_resolve.rs:799-826`), with `candidate` cloned from the published config so `carry_process_state` (`:833-847`) keeps the process-state fields. The harness is the published one, which is the same selection the last bind made (`R:config.rs:646`). A project value is still dropped or clamped by `record_project_drops`/`merge_project_config` (`R:config_resolve.rs:1477-1587`, `:1059-1098`), and harness disables only accumulate (`:899-957`). TS uses its host loader (`loadAftConfig`), which applies the TS allowlist (`OC:config.ts:1742-1778`).
4. **Diff and apply group A only.** `apply_live_config(published: &Config, candidate: &Config) -> LiveApply { next, applied: Vec<&'static str>, deferred: Vec<&'static str> }`:
   - One explicit table maps each `Config` field or leaf to a key name and a class: `Live`, `Deferred` or `ProcessState`. For example, `experimental_bash_rewrite` maps to `bash.rewrite` (Live) and `experimental_bash_background` to `bash.background` (Deferred).
   - A test destructures `Config { .. }` with no rest pattern, so adding a field fails to compile until it is classified. The comparison covers the `#[serde(skip)]` fields too (`foreground_wait_window_ms`, `diagnostics_on_edit`, `R:config.rs:583-584`, `:611-612`).
   - `next` is a clone of `published` with only the Live leaves copied. Mixed structs are split by leaf: `semantic.query_*` are Live and the rest of `semantic` is Deferred; `backup.max_file_size` is Live while `enabled` and `max_depth` are not; `sandbox.*`, `idle.*` and `inspect.{enabled,diagnostics_timeout_ms,tier2_pass_timeout_ms,duplicates}` are Live; `git.co_author` is Live and the rest of `github`/`gh_shim` is not.
5. **Publish and push.** If `applied` is not empty, publish `next` with a compare-and-swap on the snapshot: the new `ctx.publish_config_if_current(&expected_arc, next)` is `set_config` guarded by `Arc::ptr_eq`. If the snapshot moved, redo steps 3-5 once. Then call only the setters that the changed keys need, taking their inputs from `next`:
   - `formatter`/`checker`: `crate::format::clear_tool_cache_for_root(Some(root))` (as at `R:commands/configure.rs:6142`);
   - `backup.max_file_size`: `ctx.backup().lock().set_policy(BackupPolicy { enabled, max_depth, max_file_size })`, with `enabled` and `max_depth` from the published (unchanged) config (as at `:3423-3436`);
   - `bash.long_running_reminder_*`: `ctx.bash_background().configure_long_running_reminders(..)` (as at `:6170-6174`);
   - `inspect.enabled`: `ctx.reset_tier2_refresh_scheduler()` (`R:context.rs:7067`, as at `R:commands/configure.rs:3487`);
   - `git.co_author` going from `off` to on: `ensure_managed_git_hooks(&managed_git_hooks_dir(storage_root))` (`R:agent_child_env.rs:289-291`, `:666`). This is only the hook half of `maintain`; the gh shim (group B) is not touched;
   - `lsp.diagnostics_on_edit` (daemon): update `RootMeta.diagnostics_on_edit` the same way bind completion does (`R:subc/mod.rs:5203-5208`; read per call at `:6425-6427`).

   Every other group A key needs only the publication.
6. **Log one line** per root (Rust) or context (TS):
   `config reload root=<root> source=<user|project> applied=[k1,k2] deferred=[k3,k4] (deferred keys apply on next connect/restart) dropped=[k5 (project may only tighten)]`.
   Empty lists are omitted, and if nothing changed the line is written at debug level. The `dropped` list comes from the resolver diagnostics.

**What is not triggered.** The reload never calls `handle_configure`. So it never runs `defer_to_exclusive_configure` (`R:commands/configure.rs:2567-2576`), `agent_child_env::maintain` (`:3238`), the ONNX lookup, `note_configure_warm_key`, artifact drop or reload (`:3560-3664`), hashline binding registration, or the configure-maintenance stages. The configure generation does not change.

**Warm-key overlap.** Four group A keys are part of today's warm key: `callgraph_chunk_size`, `semantic.query_timeout_ms`, `semantic.query_instruction` (inside the whole-`semantic` term) and `inspect.enabled` (`configure_warm_key`, `R:commands/configure.rs:2436-2459`). If they stayed in it, the next ordinary connect after a live change would compare its candidate with the warm key stored from before the change and reload artifacts for no reason. Phase 2 removes these four from `configure_warm_key`, and `handle_configure` calls `reset_tier2_refresh_scheduler` itself when `inspect.enabled` flips. None of the four selects artifacts: chunk size is read at the next cold build (`R:context.rs:5628`), the query fields are read per query (`R:config.rs:252-264`), and `semantic_fingerprint_config_changed` never included them (`R:commands/configure.rs:1396-1404`).

**Deferred keys stay deferred.** A group B or C change leaves the published config unchanged, so the next configure sees a real difference and takes its full path exactly as today. In the daemon, another session binding the root does the same, which is the existing behaviour (inventory §1 finding 2).

## 3. Invalid or unreadable config

The candidate is rejected, and the last good config stays published, when any of these hold:
- a file that exists cannot be read (permission error, not a regular file, invalid UTF-8);
- a file does not strip and parse to a JSON object. Today `parse_tier` silently drops such a tier (`R:config_resolve.rs:863-864` returns `None`), which falls back to defaults. The reload path checks this first;
- a file does not deserialize **strictly** into `RawAftConfig`. Today a failure falls back to `parse_config_partially` (`:878-881`), which quietly drops the bad key to its default. The reload rejects instead. This is stricter than connect, on purpose: a live edit must never reset one key to its default because another key has a typo;
- the resolver returns errors (`ResolveDiagnostics.errors`, `R:config_resolve.rs:810-818`), for example a retired key. This is the same rule as configure (`R:commands/configure.rs:2727-2738`).

A **missing** file (NotFound, still absent after the debounce) is not an error: it resolves as `{}`, exactly as connect does (`R:subc_config.rs:75-91`). Otherwise a live reload and the next connect would disagree about the same file state. *Reviewer decision point:* if deletion should also keep the last good config, it is a one-line change to treat NotFound as unreadable.

Surfacing uses the existing channels. Nothing enters the startup config-error state, because tools keep working on the last good config.
- **Rust:** one ERROR log line naming the file and error, plus "keeping last valid configuration". It is logged once per distinct `(path, error)`. Rust does not push a frame to sessions, so the user is not told twice.
- **TS** (the user-facing surface): the same texts the config-error path uses, `formatConfigParseErrorMessage` (`BR:config-error.ts:70-75`) or the `ConfigRejectedError` message (`OC:index.ts:312-315`), with the restart note replaced by "AFT keeps using the last valid configuration until the file is fixed." The message is logged with `error()`, as `configErrorState` does (`OC:bridge-bootstrap.ts:352-361`), and delivered through the configure-warnings queue:
  - parse failures as kind `config_parse_failed` (`OC:configure-warnings.ts:53-83`, drained on idle at `OC:index.ts:887-901`);
  - rejections as a new kind `config_rejected`, added to the `isConfigureWarning` allowlists in OC and Pi;
  - on OC2 through `notify` (a log warning, `server-runtime.mjs:50`);
  - on Pi through its notify path.

  Each is deduplicated by hint. Note: `loadAftConfig` records parse failures in `configLoadErrors` and returns defaults for that tier (`OC:config.ts:2000-2005`), so the TS reload must check the load errors, not only catch `ConfigRejectedError`.

## 4. Security keys: switch-over and enforcing side

**Per-request pin (Rust).** `dispatch` (`R:main.rs:1025`) is the one entry that both standalone and subc go through (`R:main.rs:237`). At entry it installs a thread-local `ConfigPin(ctx id, Arc<Config>)` taken from `ctx.config()` and releases it on exit. `AppContext::config()` (`R:context.rs:4636-4642`) returns the pinned `Arc` when the pin belongs to this context. `set_config` (`:4645`) also replaces the calling thread's pin, so configure and the reload job see their own publication.

**Switch-over point.** A request admitted after publication sees the new value, and a request already running finishes entirely on the old one.

Threads that a request fans out to, and background maintenance, read the live snapshot. The security readers below all run on the request thread; phase 2 asserts this in the switch-over test.

| Key | Enforced by | Switch-over |
|---|---|---|
| `restrict_to_project_root` | **Rust is authoritative:** `path_restriction_context` (`R:context.rs:8743`), `R:commands/semantic_search/mod.rs:687`. **TS pre-check** decides between denying without a prompt and prompting: `OC:tools/permissions.ts:441`, `:553`; the Pi sites in §1.2. | Rust: the first request admitted after the Rust publication. TS: the first tool call that starts after the TS swap (one captured snapshot per call). |
| `sandbox.enabled`, `sandbox.write_allow`, `sandbox.read_deny` | **Rust only**, per spawn: `R:sandbox_spawn.rs:995`, `:999`, `:1036`, `:1247-1253`, `:1284-1290`; `R:bash_background/mod.rs:226`; `R:commands/bash.rs:180`. No TS reader. | Spawns in requests admitted after publication. Background tasks already running keep the confinement they were spawned with, because a live process cannot be re-confined; the reload log line says so when any are running. |
| `bash.host_fallback` | TS only (`OC:tools/bash.ts:520`, `PI:tools/bash.ts:645-647`). Every use still asks the host for permission. | The first tool call after the TS swap. |
| `url_fetch_allow_private` | Rust only (`R:commands/zoom.rs:88`, `R:commands/outline.rs:212`). | Requests admitted after publication. |

**Rust and TS for `restrict_to_project_root`.** The two processes cannot switch in one atomic step. Both watch the same file and switch within one debounce window of each other. During that gap every request gets the stricter of the old and new values, never a looser one, because each side denies on its own `true`:
- Loosening (`true` to `false`): whichever side has not yet published still denies.
- Tightening (`false` to `true`): Rust denies as soon as it publishes, whatever TS holds.

The only visible artifact is the gap in the tightening direction. TS still prompts and Rust then refuses (the issue #125 shape), and only for calls in that window.

This fail-closed ordering holds only because both sides enforce independently. Phase 2 must not drop either check.

## 5. Concurrency

- **Lane.** The daemon runs the reload as a new maintenance drain kind (`MaintenanceDrainKind::ConfigReload`, next to `ConfigureTail` at `R:subc/mod.rs:7166-7177`). It uses `Lane::MaintenanceCommit` (`R:subc/mod.rs:7141`) and a new coalesce key `MaintenanceCoalesceKey::ConfigReload` (`R:executor/mod.rs:67-76`), so a burst of events queues one job per actor. `MaintenanceCommit` holds only the actor epoch **read** gate (`R:executor/mod.rs:3645-3651`), runs next to `PureRead`s, and never becomes a writer barrier (`Lane::Mutating`, `:46-50`). Standalone runs the reload from the between-request drain `drain_non_configure_runtime_events` (`R:main.rs:653-665`). That drain is skipped while configure maintenance is pending (`R:main.rs:637-648`), so a reload always runs after a configure's tail.
- **In-flight requests.** Publication is one `RwLock` write of an `Arc` (`R:context.rs:4645-4663`), which takes microseconds. Readers already running keep their pinned `Arc` (§4). The new snapshot never waits for them, and they never see it partway through.
- **Configure against reload on one actor.** Configure (`Mutating`, write gate) and the reload (read gate) exclude each other through the epoch lock, so they never overlap. Maintenance is also one-in-flight per actor.
- **Connect racing a file change.** Configure re-reads both files when it executes (`R:commands/configure.rs:2716`; the daemon reads again at bind, `R:subc/mod.rs:5631`), and the reload re-reads when it executes. Neither publishes a candidate computed earlier than its own run, and each diffs against the snapshot published at that moment. Whichever runs last therefore applies the newest file text:
  - reload then configure: configure applies everything, including the deferred keys;
  - configure then reload: the content gate (texts recorded by configure) makes the reload a no-op, or, if the file changed again in between, the reload applies only group A from the newer text;
  - the compare-and-swap in §2 step 5 covers any other `set_config` caller that runs in between.
- **Standing roots and the idle reaper** already read the published actor config on each tick (`R:subc/standing.rs:168-200`) and need nothing extra.

## 6. Tests for phase 2

Each test is designed to fail without the feature. Phase 2 records the mutation reds.

1. **Live A change, no reconnect** (Rust, standalone and daemon; watcher enabled in the dedicated watcher test binary):
   - configure a root with the project file `{"bash": {"enabled": true}}` and run `bash` successfully;
   - rewrite the file to `false`, wait for the reload log line, and send **no** configure: the next `bash` call returns the call-gate refusal (`R:main.rs:1038`);
   - daemon variant: edit the user file and assert that two bound roots both flip.

   Without the feature, `bash` keeps working. Mutations: removing the `ProjectConfigChanged` classification, or classifying `bash.enabled` as Deferred, turns the test red.
2. **B/C deferred and logged, not applied.**
   - Edit `disabled_tools` (C) and `indexes.trigram` (B) together with one A key.
   - Assert the A key is applied, `disabled_tools` and `indexes.trigram` in `ctx.config()` are unchanged, the log line lists both under `deferred=` with "applies on next connect/restart", the configure generation is unchanged, and the resident search index was not dropped.
   - An explicit configure afterwards applies them.

   Mutation: publishing the whole candidate (skipping `apply_live_config`) turns it red.
3. **Invalid edit keeps last good.**
   - Apply `bash.enabled: false` live.
   - Write (a) truncated JSONC, (b) a retired key, and (c) `{"bash": {"enabled": "nope"}, "format_on_edit": false}` (the partial-parse case).
   - After each, `bash.enabled` is still `false` (not the default `true`), `format_on_edit` is unchanged, one ERROR line is logged, and in TS one `config_parse_failed`/`config_rejected` warning is queued.

   Mutation: dropping the parse or strict-deserialize pre-check turns (a) and (c) red.
4. **Restrict and sandbox switch-over.**
   - Use a test hook that parks a request between two path checks inside one handler, and publish `restrict_to_project_root: true` while it is parked.
   - Both checks in the in-flight request use the old value, and a request admitted afterwards is refused.
   - Sandbox: a spawn admitted after publication gets the new `sandbox.read_deny` (asserted on the spawn plan), and a background task spawned before keeps its old plan.
   - Also assert that a `set_config` on a pinned thread updates that thread's pin.

   Mutation: removing the pin in `dispatch` turns the in-flight assertion red.
5. **Project tier cannot loosen.**
   - The user file sets `restrict_to_project_root: true` and `sandbox.enabled: true`. Edit the project file live to `restrict_to_project_root: false` (user-only) and `sandbox.enabled: false` (tighten-only).
   - After the reload both are still `true`, and the log line lists them under `dropped=`.

   Mutation: resolving the project tier through `merge_trusted_config` instead of the project filter turns it red.
6. **TS-read A key reaches the plugin** (OC1, OC2 `bootLocation`, and Pi; with a temporary XDG/HOME and project directory).
   - Edit the user file's `bash.watch_sync_max_ms`: `ctx.config.bash.watch_sync_max_ms` changes and the next `bash_watch` call clamps with the new value (`OC:tools/bash_watch.ts:133`, `PI:tools/bash.ts:804`), while `ctx.config.bash.background` (C) keeps its old value.
   - An invalid edit keeps the value and calls notify once.
   - A Pi `restrict_to_project_root` flip reaches the hoisted edit tool through the per-call getter.

   Mutation: not swapping `ctx.config`, or leaving `PI:tool-registration.ts:167` captured at registration, turns the matching assertion red.

## 7. Open points for the reviewer

- Whether deleting a file resolves as `{}`, the same as connect (proposed), or keeps the last good config (§3).
- Whether reload's strict deserialization should also become connect's rule. This note leaves connect unchanged.
- `worktree.ram_overlay` stays in A as decided. The inventory could not verify whether edits made while it was off are replayed when it turns on (inventory §8). Phase 2 checks this and reports it, and does not move the key.
