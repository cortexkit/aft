# OpenCode 2 matrix: the real failure set at GA 2.0.11

- Date: 2026-09-20
- Checkout: `d7574617b6623d463468d7bd27b4e507c3e83dff`
- Pinned V2 host: `@opencode/cli@2.0.11` (from `packages/opencode-plugin/test/load-matrix/load-matrix.ts`, read by `tests/docker/opencode2/harness/pin.ts`)
- Pinned V1 host: `opencode-ai@1.18.30` (from `.github/opencode-version.txt`)
- Contract audit this report leans on: `delta-audit-ga-2.0.11.md`

This is the honest list, grouped by mechanism and attributed. It is deliberately not a repair.

An earlier draft of this report argued that the permission rows should be reclassified `applicable` because `permission.create` exists on `@opencode/client`. That was wrong, the reclassification it justified was reverted, and section "Mechanism A" below records why. The fact was right; the inference from it was not.

## What was actually executed

Docker here is `linux/amd64` under emulation on an `aarch64` host, so everything below is slow and everything below was run.

- A `linux/amd64` release build of `agent-file-tools` was produced once (11m34s) and staged as a same-SHA artifact (`aft`, `aft.real`, `build-info.json`, `source: "same-sha-artifact"`). The harness image was then built with `AFT_BINARY_SOURCE=prebuilt`, so it did not recompile.
- Provenance verification and the harness control suite, inside the container at the 2.0.11 pin: provenance verdict `verified` (`version_output "aft 0.56.2 (c23b6073f...)"`, launch exit 0), and 15 of 15 harness controls observed their expected outcomes, including the three provenance negative controls.
- **Three matrix scenarios executed**, against host **2.0.3** (see the caveat below):
  - `safety/T1/checkpoint_restore` — **FAIL**, `projection_unparsed: {"success":false,"code":"permission_denied","message":"The \"edit\" operation was refused because the OpenCode V2 host did not provide a permission request endpoint."}`
  - `ast_replace/T1/happy` — **FAIL**, the same `"edit"` refusal
  - `import/T1/happy` — **FAIL**, `host exited null; accepted 0` (the host process never exited; a different mechanism, section D)
- Direct probes of the real pinned 2.0.11 host binary in a clean `linux/amd64` container under private `HOME`/XDG roots: `--version`, `serve`, and `api` against three routes with correct, wrong, and absent passwords.

**Caveat on those three scenarios.** They ran with the harness pin temporarily moved back to 2.0.3, because at 2.0.11 nothing can run at all (section C). They are therefore evidence about AFT at this checkout on host 2.0.3, not about 2.0.11. They are reported as such and nowhere else in this document is a row described as observed unless it appears above.

Separately, the task-giver supplied live results from a running GA host, reproduced in section A. Those are theirs, not mine, and are labelled where used.

## Mechanism A — a server plugin is not given a client, so no AFT tool can request permission. Upstream's.

This is the dominant mechanism. It explains every denied tool in the live GA evidence and both permission failures I observed.

### The decisive question: full client, narrowed facade, or absent?

**Absent, and not as a narrowing.** Verified against the installed pin (`@opencode/plugin@2.0.11`, `node_modules/.bun/@opencode+plugin@2.0.11+.../dist/effect/plugin.d.ts:25-53`), the Effect `Context` a server plugin receives has twenty-five members: `app`, `location`, `options`, `agent`, `aisdk`, `command`, `event`, `experimental`, `integration`, `mcp`, `model`, `generate`, `permission`, `plugin`, `provider`, `reference`, `rpc`, `session`, `shell`, `skill`, `storage`, `tool`, `vcs`, `websearch`, `worktree`. None of them is `client`.

The context is not a client with members removed. It is a set of separate per-domain facades, each declared as a `Pick<...Api, ...>` sliced from the client's API types, and the permission slice deliberately omits creation: `Pick<PermissionApi<unknown>, "list" | "get" | "reply">` at 2.0.11 (`dist/effect/permission.d.ts:19-21`), the same plus `"rules"` at 2.0.3. The runtime object that populates the context has exactly those top-level keys (`@opencode/core@2.0.11/dist/chunks/snapshot-qgzk9bq2.js:197-503`; permission domain at `:412-417` is `{hook, list, get, reply}`).

So on the object AFT receives, `host.client` is `undefined` and `host.client.permission.create` is not a call that can be made. **Branch (a).** The GA audit's point 8 was right.

What I had verified earlier, and reported as if it settled the question, was the `@opencode/client` `PermissionApi` surface: `create` does exist there, in both 2.0.3 and 2.0.11, with a `PermissionCreateInput` that is field-for-field identical and accepts exactly what `src/permissions/v2.ts:148-155` sends. That client is real, and it is handed to the **TUI** plugin context (`dist/tui/context.d.ts:449`, `readonly client: OpenCodeClient`) — not to the server plugin where AFT's tools execute. Verifying the API surface without verifying who is handed it is the gap that produced the wrong conclusion.

Nor is there a side route: `App` is `{name, version, channel}` with no endpoint, and `@opencode/core@2.0.11/dist` declares no environment variable carrying the server address.

### Trace

`src/entry/server-runtime.mjs:31` passes the plugin Effect context into `hoistedV2ToolConsumers(context)`. That function's first line is `if (!("client" in host)) return {}` (`src/tools/hoisted/v2.ts:20`), which is always taken, so `consumers.requestPermission` is never set. `runtimeFor`'s `ask` then rejects every request with `The "<permission>" operation was refused because the OpenCode V2 host did not provide a permission request endpoint.` (`src/tools/definitions/v2.ts:191-198`). That string is exactly what both of my executed permission failures returned and is the pre-existing capture in `tests/docker/opencode2/contract/probe/bash-t1-permission-refusal.txt`, which also records the pre-guard form of the same defect as a `TypeError` on `host.client.event`.

### Which rows this reaches

Every row whose scenario invokes a tool that reaches an unconditional `context.ask`. Corrected from code — an earlier draft used `V2_PERMISSION_ASK_INVENTORY` (`src/tools/hoisted/v2.ts:7-16`) as a list of *tools*, which it is not: its entries are permission ids and bash sub-sites, so `"edit"` covers every tool that asks through `askEditPermission`, not just the `edit` tool. The actual ask sites and their callers:

| ask site | permission id | tools that reach it unconditionally |
| --- | --- | --- |
| `hoisted.ts:442` | `read` | `read` |
| `permissions.ts:231` via `askEditPermission` | `edit` | `write`, `edit`, `apply_patch` (`hoisted.ts:573,604,767,899,933,1096`), `aft_safety` restore (`safety.ts:112,160`), `ast_replace` (`ast.ts:151`), `aft_import` (`imports.ts:104`) |
| `hoisted.ts:1196`, `hoisted.ts:1276` | `edit` | `delete`, `move` |
| `permissions.ts:627` via `askGrepPermission` | `grep` | `grep` (`search.ts:178`) |
| `permissions.ts:627` via `askSearchPermission` | `aft_search` | `aft_search` (`semantic.ts:161`) |
| `permissions.ts:686` via `askGlobPermission` | `glob` | `glob` (`search.ts:259`) |
| `bash.ts:182,435` | `bash`, `external_directory` | `bash` |

`assertExternalDirectoryPermission` (`permissions.ts:465`) is called by almost every tool but only asks for targets outside the project, so it does not fire on in-project fixtures and is not a source of failure here.

Cross-referencing that against what each scenario actually calls:

- T1 for `read`, `write`, `edit`, `apply_patch`, `delete`, `move`, `bash`, `glob`, `grep`, `search`, `import`, `safety`, `ast_replace`
- T1 for `bash_kill`, `bash_status`, `bash_watch`, `bash_write` — each of these scenarios calls `bash` first to create the task it then operates on, so each hits the `bash` ask before reaching its own tool
- T3 for `read`, `write`, `edit`, `apply_patch`, `delete`, `move`, `bash`
- T4, T5, T6 for `bash`; T6 for `glob`, `grep`, `search`
- The V2 leg of every corresponding T7

That is the complete set of thirty `expected_fail:37164` rows. **All thirty keep the label.** The reclassification in the reverted commit was wrong on every one of them.

### The live GA evidence is consistent with this, with one open question

The task-giver's GA run denied `read`, `write`, `bash`, `aft_import`, and `aft_safety restore`, and found `aft_conflicts`, `aft_inspect`, `aft_outline`, `aft_zoom` and `aft_safety list/checkpoint/history/undo` working. Every denial is a tool in the table above; every working tool has no unconditional ask site. That is exactly what this mechanism predicts.

The two entries that do not fit are `glob` and `grep`, reported working, while the table says both ask unconditionally. The likely explanation is a name collision rather than a contradiction: `V2_BUILTIN_REPLACEMENTS` is `["read", "edit", "write", "apply_patch"]` (`src/tool-registration.ts:24`), and registration calls `editor.remove(name)` only for those four before `editor.add(definition)` (`:183-184`). AFT registers its own tools named `glob` and `grep` without removing the host's natives, so on a GA host there are two candidates for each name and it is not established from here which one a model call reaches. **This is worth settling before anyone acts on the `glob`/`grep` rows**, because the two answers differ: if the host's native tool wins, AFT's `glob`/`grep` are dead code on V2 and the matrix is exercising something other than what a user reaches; if AFT's wins, the working result needs another explanation. The cheapest settling move is a single GA `glob` call whose output is checked for AFT's truncation trailer, which the native tool does not emit.

## Mechanism B — AFT asks for permission on every read, including reads inside the project. Ours.

The task-giver flagged `read` being denied as a tell, and it is one. **Yes, our wrapper requests permission for reads unconditionally.**

`createReadTool` calls `context.ask({permission: "read", patterns: [filePath], always: ["*"], metadata: {}})` at `src/tools/hoisted.ts:440-448` on every invocation, after the external-directory check and before reading anything. There is no in-project short-circuit: the ask does not depend on whether `filePath` is inside the project root, on the presence of a saved rule, or on any argument. The `assertExternalDirectoryPermission` call immediately above it (`:433`) is the check that *is* scoped to external paths, and it is a separate gate.

This is a defect independent of mechanism A, and it survives fixing mechanism A. AFT already sets `options.permission: "read"` on the projected tool (`src/tools/definitions/v2.ts:128-130`), which is the declarative label the host evaluates config rules against (audit point 10). Issuing an explicit ask on top of that is a second gate the host's own read tool does not have, so an in-project read that should be silent would prompt — once per session at best, given `always: ["*"]`.

Attribution: ours, in product code, and out of scope for this task to fix. It is the reason `read/T1/happy` — a scenario that reads `sample.txt` from its own fixture — is a permission row at all.

## Mechanism C — the captured host contracts still describe 2.0.3. Harness's.

At the 2.0.11 pin, no scenario runs. The harness refuses until three committed contracts carry the pinned host version, and they carry `2.0.3`.

Observed, verbatim:

```
HarnessError: contract_uncaptured:host_cli_contract must carry the pinned host version and observed run id
    at contractIdentity (/workspace/tests/docker/opencode2/harness/contracts.ts:65:5)
    at loadHostCliContract (/workspace/tests/docker/opencode2/harness/contracts.ts:86:20)
    at async validateHarnessInputs (/workspace/tests/docker/opencode2/harness/validation.ts:858:11)
HarnessError details: {"expected_version":"2.0.11","observed_version":"2.0.3","observed_run_id":"oc2-ga-inspect-t4"}
```

Affected: `contract/host-cli-contract.json`, `contract/host-provider-config.json`, `contract/host-schema-rejection.json`. The failure is unsuppressible and sits inside `validateHarnessInputs`, so `--validate-only` does not get past it either.

Attribution: the harness's, and a direct consequence of moving the pin. It is not a defect — it is the harness correctly refusing to credit 2.0.3 observations to a 2.0.11 host.

**Re-stamping the version is not sufficient.** One of those contracts describes an endpoint 2.0.11 no longer serves. Observed against the real 2.0.11 binary:

| request | 2.0.11 result |
| --- | --- |
| `api --server $EP GET /api/health` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/server` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/info` (correct password) | exit 0, `{"version":"2.0.11","pid":37,"urls":["http://127.0.0.1:4096"],"paths":{"tmp":"/tmp/opencode"}}` |
| `GET /api/info` with a wrong password | exit 1, `Error: Server at <endpoint> did not provide a compatible V2 health response` |
| `GET /api/info` with no password | exit 1, same error |

That matches the packages: `@opencode/protocol@2.0.3/dist/groups/health.js:13` publishes `health.get` at `/api/health` returning `{healthy: true, version, pid}`; at 2.0.11 the health group is gone from `dist/groups/` entirely and `@opencode/protocol@2.0.11/dist/groups/server.js:13` publishes `server.info` at `/api/info` returning `{version, pid, urls, paths:{tmp}}`.

`contract/host-cli-contract.json:101` sets `shared_server_smoke.path` to `/api/health` with `expected_status: 0`, and `harness/host.ts:310-319` fails the run with `host_failed: "shared-server smoke correct attach failed"` when that call's exit code is not 0. So the moment the version stamp is fixed, every shared-server scenario — `bash/T4/abort`, `bash/T5/completion_wake`, `bash/T5/watch_pattern_once`, `inspect/T4/abort` — dies on the smoke instead, and the smoke stops proving attachment.

The serve handoff needs no re-capture: `server listening on http://127.0.0.1:4096` and `server password <redacted>` still match both `stdout_pattern`s, and both password negative controls still exit non-zero with the same stable error strings the contract records.

Next action for whoever repairs this: re-capture the three contracts against 2.0.11, moving `shared_server_smoke` to `GET /api/info` and replacing the `{healthy: true, version}` positive-control assertion with the observed `ServerInfo` body. The provider-config and schema-rejection contracts need real captures rather than a version bump; neither was re-observed here.

## Mechanism D — `aft_import` does not terminate the host. Ours or the harness's; undetermined.

`import/T1/happy` failed differently from every other permission row: `host exited null; accepted 0`. The host process did not exit and was killed at the scenario timeout, rather than returning the permission refusal that `safety/T1` and `ast_replace/T1` returned from the same `askEditPermission` site.

`aft_import` asks with permission id `edit` at `src/tools/imports.ts:104`, so mechanism A should produce a clean refusal here as it does for its two siblings. It does not. Something on the `aft_import` path either swallows the rejection or leaves a handle open. One observation is not enough to attribute this between our code and the harness's scenario, and it was not reproduced.

This row was previously `expected_fail:37164`, and the label is retained, but note that the label does not describe what was observed: the observed failure is a hang, not a refusal.

## Mechanism E — T7's V1 leg. Host's, on the V1 line.

`https://github.com/anomalyco/opencode/issues/48340` is readable and is *not* what the label's placement suggests. It is titled "Plugin dispose is not called after one-shot run final stop", it is open, and it is reported against **`opencode-ai@1.18.29`** — the V1 host, not any `@opencode/*` package. Its claim: after `opencode run ... --auto` emits the final `step_finish` with `reason: "stop"`, the process stays alive and never invokes the plugin's `dispose`, so plugin-owned handles are never released and the CLI does not exit.

T7 is the only trajectory that runs the V1 host. It is materialized from each tool's T1 scenario (`harness/scenario-loader.ts:299-308`), run once on each host, and its projected text compared (`harness/driver.ts:1050-1096`); the V1 leg uses `OPENCODE1_BIN`, which the image installs at `opencode-ai@1.18.30`. A V1 host that does not exit after a one-shot run cannot yield a projected text to compare, so the label describes a real defect on a path T7 genuinely uses.

Two honest qualifications:

1. **No V2 contract point proves it, and none can.** 48340 is a V1 defect. The 2.0.11 pin move neither fixes nor worsens it, because the V1 host is pinned separately in `.github/opencode-version.txt` and did not move. Asking whether 48340 "still holds at the new pin" has the answer: the new pin is not the pin that governs it.
2. **It was not re-verified at 1.18.30.** The issue names 1.18.29; the harness pins 1.18.30; 1.18.31 exists upstream. An open issue is not proof that a particular later build still hangs, and mechanism C prevented running a T7 scenario at the current pin to check. The label is retained on the strength of the issue and the path, not on an observation at the pinned version. Re-verifying it is worth doing once mechanism C is cleared; if the V1 leg does terminate at 1.18.30, all 23 T7 rows are mislabelled.

Note also that T7 inherits T1: for every tool in mechanism A, the T7 V2 leg fails on the permission refusal before parity is ever compared, so 48340 is not the only thing failing those rows and never was.

## Summary

| Mechanism | Attribution | Rows reached | Evidence |
| --- | --- | --- | --- |
| A. No client on the server plugin context; permission facade excludes `create` | Upstream | All 30 `37164` rows | `@opencode/plugin@2.0.11/dist/effect/plugin.d.ts:25-53`, `dist/effect/permission.d.ts:19-21`, `@opencode/core@2.0.11/.../snapshot-qgzk9bq2.js:412-417`; observed refusals in `safety/T1` and `ast_replace/T1`; task-giver's GA run |
| B. Unconditional permission ask on every read | Ours | `read` at T1/T3/T7, and the shape of every read-side prompt | `src/tools/hoisted.ts:440-448` against `src/tools/definitions/v2.ts:128-130` |
| C. Host contracts pinned at 2.0.3; `/api/health` removed at 2.0.11 | Harness | All rows at the 2.0.11 pin, plus the four shared-server scenarios a second time | Observed `contract_uncaptured`; observed 404 on `/api/health`; `@opencode/protocol@2.0.11/dist/groups/server.js:13` |
| D. `aft_import` hangs instead of refusing | Undetermined | `import/T1` | Observed `host exited null`; one run, not reproduced |
| E. V1 host does not dispose after a one-shot run | Host (V1 line) | All 23 T7 rows | Upstream 48340 against `opencode-ai@1.18.29`; V1 pinned at 1.18.30; not re-verified |

## Open questions worth settling before acting

1. Does a GA model call to `glob` or `grep` reach AFT's tool or the host's native one? Decides whether those rows measure anything a user reaches.
2. Does `aft_import` hang reproducibly, and where? Mechanism D is one observation.
3. Does `opencode-ai@1.18.30` still fail to dispose? Decides all 23 T7 rows.
4. Which mechanism should replace the absent `permission.create` — the `permission.hook` the context does expose, or a route to the HTTP endpoint that a server plugin can actually obtain? Mechanism A is a design question, not a wiring fix.
