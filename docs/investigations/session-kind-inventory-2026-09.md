# Session-kind inventory: what AFT changes per kind of session (2026-09)

Input for declaring presets in AFT's module manifest. A preset is a named,
immutable bundle a consumer picks per session (tool set, tool descriptions and
behaviour differences). Today the OpenCode and Pi plugins reach the same effect
by *detecting* what kind of session they are in; subc catalog consumers (Broca,
Prefrontal, MCP facades) get one flat list and none of the detection.

This is a read-only inventory of the tree at `f1666ac8`. Line numbers refer to
that commit. Nothing in the code was changed.

Scope searched: `packages/opencode-plugin/src`, `packages/pi-plugin/src`,
`packages/aft-bridge/src`, `crates/aft/src` (including `subc/`, bind trust,
the generated catalog `crates/aft/src/subc_tool_schemas.json`).

---

## 1. The signals in use today

| Signal | Where it is read | Meaning today |
|---|---|---|
| OpenCode `parentID` (via `client.session.get({path:{id}})`) | `packages/opencode-plugin/src/shared/subagent-detect.ts:63-140` | Non-empty `parentID` ⇒ subagent ("worker"). Missing session id, missing `session.get`, or an SDK error ⇒ primary. Result cached per session (LRU 200); errors are not cached. |
| Pi `MAGIC_CONTEXT_PI_SUBAGENT=1` env | `packages/pi-plugin/src/index.ts:292-304`, `packages/pi-plugin/src/tools/bash.ts:54-79` | Set by pi-magic-context in the `pi --print` children it spawns (historian, dreamer, delegated agents). Process-wide. |
| Pi `extCtx.hasUI === false` | `packages/pi-plugin/src/tools/bash.ts:73-79`, `:403`, `:656`; `tools/hoisted.ts:662`, `:826`; `commands/aft-status.ts:21` | Headless `pi -p` / `--mode json` run. RPC and interactive modes have `hasUI: true`. A context with no `hasUI` field at all counts as primary. |
| Pi host = OMP vs Pi | `packages/pi-plugin/src/harness.ts:60-83` | `api.arktype` or `api.registerFileWriteFallback` present ⇒ OMP. Host flavour, not session kind. |
| `OPENCODE_CLIENT === "cli"` and presence of `client.tui.showToast` | `packages/opencode-plugin/src/notifications.ts:43-62`, `:411-426` | TUI vs Desktop notification channel. Host flavour. |
| OpenCode V1 plugin vs V2 server runtime | `packages/opencode-plugin/src/entry/server-runtime.mjs:46-50`, `:99-106` | V2 host has no session UI for startup warnings (they go to the log); the tool context's `client` is the V2 plugin context. |
| subc bind trust (`BindTrust`) | `crates/aft/src/subc/mod.rs:827-892` | `FirstParty` for `Principal::Direct` or a reserved module id in the allowlist (`llm-runner`, `aft`, `broca`, `alfonso-core`, `prefrontal`, `prefrontal-core`); `Untrusted` for any other reserved id, `Unverified`, no principal, **or any harness starting with `fed:`**. |
| subc consumer capability `elicitation` | `crates/aft/src/subc/mod.rs:5614-5623` | Stamped by the facade from the MCP host's advertised capabilities. Absent ⇒ untrusted bash is flatly denied. |
| subc bind `harness` string | `crates/aft/src/subc/mod.rs:5427`, `:5629-5632`; `crates/aft/src/config_resolve.rs:906-961` | Selects the `harnesses.<id>` config override block and the `fed:` trust downgrade. |
| Config keys | see §3 | Many description and behaviour variants are pure config. |

Signals that do **not** exist today: there is no `unattended` permission mode
anywhere in AFT (the only "unattended" hits are comments explaining why a
prompt would stall an unattended session, e.g.
`packages/opencode-plugin/src/tools/permissions.ts:273`,
`crates/aft/src/commands/bash_artifact_owned.rs:9`). OpenCode `opencode run`
and `--mode rpc`/`-p` are not detected by AFT on the OpenCode side; OpenCode
headless behaviour comes only from the host's own permission service answering
AFT's asks.

---

## 2. Inventory of per-session-kind differences

Legend for "who gets what": **OC-P** OpenCode V1 primary, **OC-S** OpenCode V1
subagent (`parentID`), **OC-V2** OpenCode V2 runtime, **Pi-I** Pi interactive
TUI, **Pi-R** Pi `--mode rpc`, **Pi-H** Pi headless `-p`/json without the env,
**Pi-MC** Pi child with `MAGIC_CONTEXT_PI_SUBAGENT=1` (also headless), **OMP**
oh-my-pi (same as the Pi row it runs as), **Cat-1P** first-party subc catalog
consumer (Broca, Prefrontal), **Cat-U** untrusted subc bind (MCP facade, `fed:`).

### 2.1 bash / bash_watch behaviour keyed on worker vs primary

| # | File:line | Signal | What differs | Who gets which |
|---|---|---|---|---|
| B1 | `packages/aft-bridge/src/bash-hints.ts:12-38` (`resolveWatchTimeoutMs`), used at `packages/opencode-plugin/src/tools/bash_watch.ts:133-141` and `packages/pi-plugin/src/tools/bash.ts:864-870` | role = worker/primary | Default sync `bash_watch` deadline when no timeout is passed: primary 30 000 ms (`DEFAULT_PRIMARY_WATCH_TIMEOUT_MS`), worker = `bash.watch_sync_max_ms` (default 120 000). Both clamped to the cap. | worker default: OC-S, Pi-H, Pi-MC. primary default: OC-P, OC-V2 (see U1), Pi-I, Pi-R. Cat-*: no `bash_watch` tool at all. |
| B2 | `packages/aft-bridge/src/bash-hints.ts:50-59` (`watchTimeoutSteer`), used at `packages/opencode-plugin/src/tools/bash_watch.ts:251-255` and `packages/pi-plugin/src/tools/bash.ts:1418-1421` | role | Text appended when a sync watch times out. Worker: "still running; this is not a failure. Watch again (timeoutMs up to N) and don't report a result until it finishes." Primary: "…Watch again, do other work, or end your turn: the completion reminder wakes you." Pi passes `timeout_ms` as the parameter spelling. | Same split as B1. |
| B3 | `packages/pi-plugin/src/tools/bash.ts:1459-1464` | role | Pi drops the trailing "A completion reminder will be delivered automatically; don't poll." line on a worker's watch timeout so it is not told both "watch again" and "don't poll". OpenCode's watch result never appends that line (`bash_watch.ts:235-269`), so no branch is needed there. | Pi-H, Pi-MC vs Pi-I, Pi-R. |
| B4 | `packages/opencode-plugin/src/tools/bash_watch.ts:96-98`, `:135-136` | OC subagent + `bash.subagent_background === false` | `bash_watch({background:true})` is silently turned into a sync wait for the full cap. | OC-S only when the user set `subagent_background:false`. Pi: not implemented (see I2). |
| B5 | `packages/opencode-plugin/src/tools/bash.ts:421-425` | OC subagent | `pty:true` is refused: "PTY mode is not available in subagent sessions; subagents cannot drive interactive terminals." | OC-S refused. OC-P allowed. Pi: never refused (see I3). Cat-*: allowed by schema, but no `bash_status`/`bash_write` to drive it. |
| B6 | `packages/opencode-plugin/src/tools/bash.ts:426-429`, `:451-456` | OC subagent + `bash.subagent_background === false` | `background:true` and auto-promotion are disabled: the call runs with `block_to_completion: true` for the whole hard timeout. | OC-S with `subagent_background:false`. Default (`true`) leaves OC-S identical to OC-P. Pi documents the key but ignores it (I2). |
| B7 | `packages/opencode-plugin/src/tools/bash.ts:551`, `:704-708` (`subagentGuidance`) | OC subagent + background allowed | When a command is backgrounded (explicitly or auto-promoted) the reply gets "NOTE (subagent session): Continue with other work if you have it. If you don't, call bash_watch({ taskId, timeoutMs: 60000 })… Subagents don't survive turn-end and won't receive the completion reminder." | OC-S only. Pi workers get the plain promotion text (I4). |
| B8 | `packages/opencode-plugin/src/tools/bash.ts:318` (pty param description); generated into `crates/aft/src/subc_tool_schemas.json` → `bash.properties.pty` | none (static text) | Schema text says PTY is "Unavailable in subagent sessions". Same text for every OpenCode session and for every catalog consumer. | OC-P, OC-S, Cat-1P, Cat-U all see it. Pi's schema does not say it. |

### 2.2 Permission prompting and refusals

| # | File:line | Signal | What differs | Who gets which |
|---|---|---|---|---|
| P1 | `packages/pi-plugin/src/tools/bash.ts:399-408` | Pi `hasUI` / `ui.confirm` | A bash `permission_required` answer from Rust is refused outright: "Permission denied: command approval requires an interactive UI." Interactive: `ui.confirm` per ask (escalation asks titled "Run command unsandboxed on host?", `:410-420`). | Pi-H, Pi-MC refused; Pi-I, Pi-R prompted. |
| P2 | `packages/pi-plugin/src/tools/bash.ts:656-677` | Pi `hasUI` | Host-fallback execution (AFT transport down, `bash.host_fallback: true`) refused headless: "…host fallback execution requires an interactive UI." | Pi-H, Pi-MC refused; Pi-I, Pi-R prompted. |
| P3 | `packages/pi-plugin/src/tools/hoisted.ts:660-670`, `:824-834` | Pi `hasUI` | Publishing / editing a GitHub comment through `write`/`edit` on `issue://`/`pr://` refused headless ("…requires an interactive UI"). | Pi-H, Pi-MC refused. |
| P4 | `packages/opencode-plugin/src/tools/bash.ts:470-525`, `tools/hoisted.ts:580-585`, `permissions/v2.ts:545-580` | none (host decides) | OpenCode always raises the ask through the host (`context.ask` in V1, `permission.create` + event stream in V2). A headless OpenCode host answers per its own rules. AFT never refuses on its own for lack of a UI. | OC-P, OC-S, OC-V2 identical at the AFT level. |
| P5 | `crates/aft/src/subc/mod.rs:6456-6478`, `subc/bash.rs:1319-1325` | trust = Untrusted | Every `bash_*` companion is refused (`bash_denied_untrusted`: "remote/MCP-facade binds cannot run shell commands"); `bash`/`powershell` too unless the consumer declared `elicitation`. | Cat-U. |
| P6 | `crates/aft/src/subc/mod.rs:6542-6580`, `subc/bash.rs:238-272`, `:421-432` | trust = Untrusted + elicitation | `bash`/`powershell` run only after a per-command elicitation round-trip (reverse request to the consumer) built from the permission scanner; PowerShell always gets an exact-command ask. A denied or unanswered ask settles as `bash_denied_untrusted` (`mod.rs:2226-2240`). First-party binds skip this and use the normal Rust permission path. | Cat-U with elicitation. |
| P7 | `crates/aft/src/subc/mod.rs:6422-6454` | trust = Untrusted | `sandbox:"host"` escalation refused: "sandbox host escalation is unavailable to untrusted principals". | Cat-U. |
| P8 | `crates/aft/src/subc/mod.rs:6519-6541` | trust = Untrusted + module draining | Untrusted bash gets the retryable "module draining" error instead of an ask that would span the drain. | Cat-U. |

### 2.3 Other untrusted-bind restrictions (subc)

| # | File:line | What differs |
|---|---|---|
| T1 | `crates/aft/src/subc/mod.rs:6796-6800`, `:6997-7001`; `context.rs:4663` | Tool calls run under `with_force_restrict`: path access forced to the project root regardless of `restrict_to_project_root`. |
| T2 | `crates/aft/src/subc/mod.rs:6741-6752` | `inspect` and LSP navigation run with the untrusted restriction flag. |
| T3 | `crates/aft/src/subc/mod.rs:6770-6776`, `:6970-6976`, `:7273-7278`; `response_finalize.rs:110-119` | Background-completion notices are not attached to tool results (`allows_bash_observation()` false). |
| T4 | `crates/aft/src/subc/mod.rs:6345-6352` | A bg_events subscription from an untrusted route is ended at once (`subscribe-denied`). |
| T5 | `crates/aft/src/subc/mod.rs:2590-2597` | Only bash-observation-capable (first-party) routes count as live originating sessions. |
| T6 | `crates/aft/src/subc/mod.rs:2915-2927` | An untrusted bind never overwrites a retained first-party session identity. |
| T7 | `crates/aft/src/subc/mod.rs:5428-5448` | Management-surface routes need a first-party principal ("AFT management routes require a first-party principal"). |
| T8 | `crates/aft/src/subc/mod.rs:5673-5677`; `sandbox_spawn.rs:60-65`, `:936-944` | The sandbox spawn principal carries the trust label (it is hashed into the spawn identity). |

### 2.4 Startup, warmup and downloads

| # | File:line | Signal | What differs | Who gets which |
|---|---|---|---|---|
| S1 | `packages/pi-plugin/src/index.ts:499-511` | `MAGIC_CONTEXT_PI_SUBAGENT=1` | Skips ONNX Runtime download/preparation and logs "skipping eager warmup, ONNX Runtime preparation and LSP auto-install; the bridge starts on the first AFT tool call". | Pi-MC only. Pi-H (plain `pi -p`) does full eager startup. |
| S2 | `packages/pi-plugin/src/index.ts:537-555` | same | `lsp.auto_install` forced off: no npm/GitHub LSP installs, and `lsp_auto_install_binaries` sent as `[]` so Rust skips the missing-binary walk (no `lsp_binary_missing` warnings). | Pi-MC only. |
| S3 | `packages/pi-plugin/src/index.ts:448-462` | subc connection file / `AFT_BINARY_PATH` (config, env) | Background binary download is skipped when subc is configured or a binary path is pinned. Not session kind. | all Pi. |
| S4 | OpenCode | – | No per-session startup difference: one plugin instance serves primary and subagents in the same process. V2 differs only in where startup warnings go (log instead of a session, `server-runtime.mjs:48-50`). | – |

### 2.5 Tool registration, descriptions and schemas

| # | File:line | Signal | What differs | Who gets which |
|---|---|---|---|---|
| D1 | `crates/aft/src/subc/manifest.rs:266-335` | none | The catalog is one flat list: `status, bash, [powershell], read, write, edit, apply_patch, grep, glob, search, outline, zoom, inspect, callgraph, conflicts, ast_search, ast_replace, delete, move, import, safety`. **No `bash_status`, `bash_watch`, `bash_write`, `bash_kill`**: they are plumbing only (`manifest.rs:41-104`). | Cat-1P, Cat-U. |
| D2 | `packages/opencode-plugin/src/subc-tool-schemas.ts:70-85`, `:112-207` | none | Catalog descriptions are the OpenCode V1 descriptions rendered once at build time with stub config `{disabled_tools: [], sandbox: {enabled: true}}`. Consequences checked in the generated JSON: `bash` mentions `bash_watch`, `bash_status`, `bash_write`, PTY, "completion reminder wakes you", "A new user message detaches this wait", the `description` param says "shown in OpenCode UI metadata", and the `pty` param says "Unavailable in subagent sessions". No GitHub sentences (github defaults off). | Cat-1P, Cat-U: every session gets OpenCode-primary wording for tools they cannot call. |
| D3 | `crates/aft/src/subc/manifest.rs:201-248`; `subc-tool-schemas.ts:160-175` | none | Consumer-only bash properties (`foreground_orchestrate`, `block_to_completion`, `shell`) are stripped from the catalog. A catalog consumer therefore cannot ask for the "block to completion" worker behaviour of B6. | Cat-*. |
| D4 | `crates/aft/src/subc/manifest.rs:262-273`, `:311` | host has `pwsh` | `powershell` advertised only where `pwsh` runs. Host capability. | Cat-*. |
| D5 | `packages/pi-plugin/src/tool-registration.ts:153-162`, `:67-99` | Pi host registry / `bash.powershell_tool` | Pi registers a `powershell` tool when Pi's own built-in PowerShell is active (or the config fallback says so). Host capability. | Pi, OMP. |
| D6 | `packages/pi-plugin/src/tool-registration.ts:193-215`; `tools/_shared.ts:132-164` | OMP vs Pi, `pi.tool_presentation` | On OMP `promptSnippet`/`promptGuidelines` are folded into `description` and `loadMode: "essential"` is set (when presentation is `top_level`). On Pi the host renders them itself. Host flavour. | OMP vs Pi. |
| D7 | `packages/aft-bridge/src/feature-config.ts:108-114` | adapter | Pi/OMP have no `apply_patch` or `glob`. Adapter capability. | Pi, OMP. |
| D8 | `packages/opencode-plugin/src/tools/bash.ts:376-383`; `packages/pi-plugin/src/tools/bash.ts:510-537` | config `bash.background`, `bash.compress`, `bash.detach_on_user_message`, `aft_search`/`aft_zoom` registered | Bash description sentences (background/PTY/watch paragraph, compression sentence, detach sentence, search steer) and whether `background`/`pty*` params exist. Config, not session kind. | All plugin sessions; catalog frozen at defaults. |
| D9 | `packages/opencode-plugin/src/tools/hoisted.ts:59-66`, `:386-388`, `:551-557`, `:687-690`; `tools/reading.ts:64-73`; Pi `tools/hoisted.ts:100`, `:631`, `:763`, `tools/reading.ts:357-361` | config `github.read` / `github.write` | GitHub `issue://`/`pr://` sentences in read/outline/zoom/write/edit descriptions. Config. | Plugins per config; catalog never has them. |
| D10 | `packages/opencode-plugin/src/tools/bash.ts:355-360`; Pi `tools/bash.ts:163-167`; `subc-tool-schemas.ts:62-69` | none (always declared) | `sandbox: "host"` parameter and its "no-op when sandboxing is disabled" sentence are always present; the runtime decides. Config (`sandbox.enabled`) plus trust (P7). | all. |
| D11 | `packages/opencode-plugin/src/tools/hoisted.ts:551-554` | config `backup.enabled` | write description backup sentence. Config. | all plugin sessions. |
| D12 | Pi vs OpenCode companion schemas (`task_id`/`timeout_ms`/`output_mode` vs `taskId`/`timeoutMs`/`outputMode`) | harness | Parameter spelling differs by harness; B2 passes the spelling into the steer. Harness convention. | Pi vs OC. |
| D13 | `packages/opencode-plugin/src/index.ts:850-861`; `packages/pi-plugin/src/workflow-hints.ts:187-225` | none | Workflow-hints system-prompt block is injected into every session, primary and worker alike. It tells every agent that after backgrounding "the completion reminder delivers the result" (`opencode workflow-hints.ts:148`, `pi workflow-hints.ts:123`); the bash description itself (D2/D8) says "end the turn and let the completion reminder wake you". B2 withholds exactly that advice from workers. | OC-P, OC-S, Pi-*. Catalog consumers get no hints. |

### 2.6 Background completion delivery

| # | File:line | Signal | What differs | Who gets which |
|---|---|---|---|---|
| C1 | `packages/opencode-plugin/src/bg-notifications.ts:1088-1170`, `index.ts:878-892`, `:1026-1029` | none | Completion wakes are delivered by `promptAsync` (synthetic user message) on `session.idle` and appended in-turn after later tool calls. Same code path for primary and subagent; a subagent that already ended its turn has usually been closed, which is why B7 exists. | OC-P, OC-S. |
| C2 | `packages/opencode-plugin/src/wakes/runtime-consumer.ts`, `wakes/session-delivery.ts:39-80` | OpenCode V2 | V2 admits completions through the host's native session inbox. Host version. | OC-V2. |
| C3 | `packages/pi-plugin/src/bg-notifications.ts:660-700` | none | `sendUserMessage(reminder, {deliverAs: "steer"})`. Same for every Pi mode; in a headless run the process ends with the turn, so a later completion is never seen. | Pi-*. |
| C4 | `crates/aft/src/response_finalize.rs:110-119`; `subc/mod.rs` T3/T4 | trust | First-party subc binds get completions attached to later tool results and a bg_events lane; untrusted binds get neither. | Cat-1P vs Cat-U. |

### 2.7 UI and notification surfaces (host capability, listed for completeness)

| # | File:line | What differs |
|---|---|---|
| N1 | `packages/opencode-plugin/src/notifications.ts:411-426`, `:434-470`, `:723` | Warnings and version announcements are a TUI toast when `client.tui.showToast` works, otherwise an ignored (model-hidden) session message on Desktop; Desktop cleanup is skipped in TUI mode (`OPENCODE_CLIENT=cli`). |
| N2 | `packages/opencode-plugin/src/index.ts:959-966` | `/aft-status` opens the TUI dialog when a TUI is connected, else posts markdown into the session. |
| N3 | `packages/pi-plugin/src/commands/aft-status.ts:21-24` | `/aft-status` opens a `ui.custom` dialog when `hasUI`, else a plain-text notify. |
| N4 | `packages/pi-plugin/src/index.ts:387-407` | Config-migration warnings go to `ui.notify`, falling back to stderr. |

### 2.8 Config that varies per consumer without being session kind

| # | File:line | What |
|---|---|---|
| K1 | `crates/aft/src/config_resolve.rs:906-961`; `packages/aft-bridge/src/feature-config.ts:415-437` | `harnesses.<id>` override blocks, selected by the harness string (plugins pass `opencode`/`pi`; subc binds pass their own). |
| K2 | `packages/opencode-plugin/src/config.ts:276-283`; `packages/pi-plugin/src/config.ts:298-299`, `:560`; `crates/aft/src/config_resolve.rs:2098-2103` | `bash.subagent_background` (default `true`), plugin-owned; Rust only parses it. |
| K3 | `packages/opencode-plugin/src/config.ts:292-298` | `bash.foreground_wait_window_ms` (default 15 000, floor 5 000) and `bash.watch_sync_max_ms` (default 120 000, 1 000..1 800 000). The worker default in B1 is this cap. |
| K4 | `disabled_tools`, `bash.background`, `bash.compress`, `bash.detach_on_user_message`, `bash.host_fallback`, `github.*`, `backup.enabled`, `sandbox.enabled`, `restrict_to_project_root`, `lsp.auto_install` | All drive registration or description text identically for every session of a plugin process. |

---

## 3. Consumer × behaviour matrix (today)

| | watch default | watch-timeout steer | PTY | background / auto-promote | subagent note on promote | approval asks | eager startup (ONNX, LSP installs) | bash_* companions | description flavour |
|---|---|---|---|---|---|---|---|---|---|
| OC-P | 30 s | primary | yes | yes | no | host asks | yes (process) | yes | OC, config-rendered |
| OC-S | cap | worker | **refused** | yes (no if `subagent_background:false`) | **yes** | host asks | shared process | yes | OC (same text as OC-P) |
| OC-V2 | 30 s (see U1) | primary (see U1) | yes (see U1) | yes | no | host asks (V2 service) | yes | yes | OC |
| Pi-I | 30 s | primary | yes | yes | no | `ui.confirm` | yes | yes | Pi |
| Pi-R | 30 s | primary | yes | yes | no | `ui.confirm` if the RPC client implements it | yes | yes | Pi |
| Pi-H | cap | worker | yes | yes | no | **refused** | **yes** | yes | Pi |
| Pi-MC | cap | worker | yes | yes | no | **refused** | **no** | yes | Pi |
| OMP | as its Pi row | as its Pi row | as Pi | as Pi | no | as Pi | as Pi | yes | Pi, guidance folded, `loadMode: essential` |
| Cat-1P | n/a (no tool) | n/a | schema yes, undrivable | yes, completions ride later results | no | Rust permission path | module-level | **no** | OC-primary text frozen at defaults |
| Cat-U | n/a | n/a | n/a | n/a (bash needs elicitation) | no | elicitation per command, or denied | module-level | denied | same frozen text |

---

## 4. Smallest preset set that reproduces today

Per-session behaviour today collapses to four bundles. Trust and host
capability are layered on top and stay outside presets (see §5).

1. **`interactive`**: OC-P, OC-V2, Pi-I, Pi-R.
   Primary watch default (30 s) and primary steer; PTY allowed; background and
   auto-promotion allowed; no subagent note; interactive approval; eager
   startup; bash companions registered; descriptions advertise "end your turn,
   the reminder wakes you".
2. **`delegated`**: OC-S, Pi-H.
   Worker watch default (= cap) and worker steer; the "don't poll" line dropped
   on a worker watch timeout. Background follows `subagent_background` (OpenCode
   only today). On OpenCode: PTY refused and the subagent note appended on
   promotion. Approval: host policy on OpenCode, refusal on Pi. Eager startup.
3. **`auxiliary`** (short-lived headless helper): Pi-MC.
   `delegated` plus lazy startup (no warmup, ONNX, LSP install or
   `lsp_auto_install_binaries`).
4. **`catalog`**: Cat-1P, Cat-U.
   Today this is the `interactive` description text with the bash companions
   removed and the consumer-only bash flags stripped. It is the only bundle that
   is incoherent on its own: its text points at tools it does not contain (see
   §6, I7). Reproducing today exactly needs it as a separate preset; a preset
   design would more likely map catalog consumers onto `interactive` or
   `delegated` with the companions included, or onto a new `oneshot` bundle
   that forces `block_to_completion` and drops PTY/background wording.

If OpenCode and Pi were aligned (§6), `delegated` would be one bundle across
both hosts. If `subagent_background:false` becomes a preset choice rather than
config, it defines a fifth bundle (**`oneshot`**: worker + forced foreground +
no PTY + no background wording), which is also the natural fit for catalog
consumers that cannot wait.

### Fields a preset needs

| Field | Values | Reproduces |
|---|---|---|
| `role` | `primary` \| `worker` | B1, B2, B3 (watch default, timeout steer, poll line) |
| `tools` | tool set, explicitly including or excluding `bash_status`/`bash_watch`/`bash_write`/`bash_kill` | D1 |
| `bash.background` | `allowed` \| `forced_foreground` | B4, B6 (today `subagent_background`, `block_to_completion`) |
| `bash.pty` | `allowed` \| `refused` | B5, B8 |
| `bash.promote_note` | none \| worker note (text) | B7 |
| `watch.default_timeout` | `short` (30 s) \| `cap` | B1 (could fold into `role`) |
| `descriptions` | variant key: `primary` \| `worker` \| `oneshot`, controlling the background/watch paragraph, the "end your turn" advice, the PTY caveat and host-UI wording (`OpenCode UI metadata`) | D2, D13, B8 |
| `workflow_hints` | variant matching `descriptions`, or none | D13 |
| `approval` | `interactive` \| `host_policy` \| `refuse` | P1–P4 |
| `startup` | `eager` \| `lazy` | S1, S2 |
| `completion_delivery` | `wake` \| `inline_next_call` \| `none` | C1–C4 (today it varies by host and trust, not by session; a `oneshot`/`auxiliary` preset may want `none` stated explicitly) |

---

## 5. Items that are not per-session choices (keep outside presets)

- **Trust boundary.** Everything in P5–P8 and T1–T8 keys on `BindTrust`, which the
  daemon derives from the authenticated principal and the `fed:` harness prefix
  (`subc/mod.rs:853-892`). A preset must not be able to select or relax it; a
  preset applies within whatever trust the bind already has. Consumer
  `elicitation` capability (`mod.rs:5614`) is also a transport fact, not a
  choice.
- **Host capabilities.** PowerShell availability (D4, D5), OMP folding and
  `loadMode` (D6), adapter-unimplemented tools (D7), TUI vs Desktop notices (N1,
  N2), Pi `hasUI` dialogs (N3, N4), OpenCode V1 vs V2 delivery (C2), parameter
  spelling (D12). Whether a host *can* prompt (`hasUI`, `ui.confirm`) is a
  capability; whether this session *should* prompt is the preset's `approval`.
- **Config.** `disabled_tools`, `bash.background`, `bash.compress`,
  `bash.detach_on_user_message`, `bash.host_fallback`, `bash.watch_sync_max_ms`,
  `bash.foreground_wait_window_ms`, `github.*`, `backup.enabled`,
  `sandbox.enabled`, `restrict_to_project_root`, `lsp.*`, `harnesses.<id>`
  (D8–D11, K1–K4). These apply to every session of a project; a preset may
  narrow them (drop a tool) but should not be where they are set.
  `bash.subagent_background` is the borderline case: it is config today but only
  means something per session kind, so it is the one config key that would move
  into a preset field (`bash.background`).
- **Binary resolution / downloads** keyed on subc connection or
  `AFT_BINARY_PATH` (S3) are deployment facts.

---

## 6. OpenCode vs Pi differences for the same kind of session with no clear reason

- **I1. Two different "worker" signals inside Pi.** `bash_watch` treats
  `hasUI === false` *or* the env as worker (`tools/bash.ts:73-79`), but startup
  skipping uses the env only (`index.ts:500`). A plain headless `pi -p` is a
  worker for watch timeouts yet still downloads ONNX Runtime and installs LSPs.
- **I2. `bash.subagent_background` is documented in Pi and ignored.**
  `packages/pi-plugin/src/config.ts:298-299` says "Allow subagents to use
  background bash; when false, requests block to completion", the value is
  resolved (`:560`), but `tools/bash.ts:572` computes
  `blockToCompletion = backgroundDisabled || requestedWait` with no worker term,
  and Pi's `bash_watch` never forces async to sync.
- **I3. PTY refused for OpenCode subagents, allowed for Pi workers**
  (`opencode tools/bash.ts:421-425`; no Pi counterpart). A headless Pi worker has
  no way to drive the PTY across turns either.
- **I4. Subagent note on promotion only on OpenCode** (`opencode
  tools/bash.ts:551`, `:704-708`). Its advice also conflicts with B1: it tells
  the subagent to pass `timeoutMs: 60000`, which *shortens* a worker's watch
  below the default it would get by passing nothing (the cap, 120 000 by
  default).
- **I5. Headless approval: OpenCode defers to the host, Pi refuses outright.**
  (P1–P3 vs P4.) Arguably host-driven (Pi `-p` has no permission service), but a
  Pi worker cannot run any command whose scan produces an ask, while an OpenCode
  subagent can if the host's rules allow it.
- **I6. Workflow hints are role-blind in both** (D13) while bash_watch output is
  role-aware (B2), so a worker's system prompt and bash description promise a
  completion reminder ("end the turn and let the completion reminder wake you")
  while its watch output says "don't report a result until it finishes".
- **I7. Catalog text describes tools the catalog does not ship.** The catalog
  `bash` description and `pty` parameter point at `bash_watch`, `bash_status`
  and `bash_write` and at "OpenCode UI metadata" (D1, D2), and say PTY is
  "Unavailable in subagent sessions", a rule the catalog path never enforces.

## 7. Not verified

- **U1. OpenCode V2 subagent detection.** The V2 runtime passes the V2 plugin
  context as `toolContext.client` (`entry/server-runtime.mjs:101`) and
  `resolveIsSubagent` only uses `client.session.get`. If the V2 context has no
  `session.get`, every V2 session is cached as primary
  (`subagent-detect.ts:86-96`), so V2 subagents would get B1/B2/B5/B7 primary
  behaviour. I did not confirm the V2 context shape.
- **U2.** Whether an untrusted-bind `bash` call that elicitation approved may
  still use `background:true`/`pty:true`. No trust check on those flags was
  found; with every `bash_*` companion denied and no completions attached (T3),
  such a task could not be observed.
- **U3.** How the plugins' own subc binds are classified (`Principal::Direct` vs
  reserved `aft`). The daemon stamps the principal; the plugin side passes no
  principal in production (`packages/aft-bridge/src/transport-factory.ts:55`).
  Either way they are first-party.
- **U4.** Pi RPC clients: `hasUI` is true, but whether a given RPC front end
  implements `ui.confirm` decides whether P1–P3 prompt or refuse.
