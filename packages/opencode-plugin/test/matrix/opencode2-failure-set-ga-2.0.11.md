# OpenCode 2 matrix: the real failure set

- Date: 2026-09-20
- Checkout: `3e8d686aff651f8d151d8ee23aa7e453b8d90c80`
- V2 host the harness actually ran: `@opencode/cli@2.0.3` (see "A note on which host this measures")
- V2 host the plugin now pins: `@opencode/plugin@2.0.11`
- Pinned V1 host: `opencode-ai@1.18.30` (`.github/opencode-version.txt`)
- Contract audit: `delta-audit-ga-2.0.11.md`

This is the honest list, grouped by mechanism and attributed. It is a report, not a repair.

## A note on which host this measures

The harness reads its pinned host from `v2Version` in `packages/opencode-plugin/test/load-matrix/load-matrix.ts`, and that file is owned by another task. It still says `2.0.3`, so every scenario below ran against host 2.0.3 while the plugin manifest pins 2.0.11. Two consequences, both of which belong to whoever owns that file:

1. The container does not yet run what we pin. Moving `v2Version` to `2.0.11` is the one edit that closes it.
2. `load-matrix.ts:996` asserts the packed manifest's `@opencode/plugin` peer pin is `"2.0.3"`, and `packages/opencode-plugin/package.json` now says `"2.0.11"`. That assertion is a plain literal comparison and will fail wherever the suite runs. It could not be run here to confirm — the suite requires Node ≥ 24 and this environment has v22.23.1 — but the disagreement is not in doubt.

Section D records what happens the moment that pin does move, which was observed before the edit was dropped.

## What was actually executed

Docker here is `linux/amd64` under emulation on an `aarch64` host. Everything below was run.

- A `linux/amd64` release build of `agent-file-tools` (11m34s), staged once as a same-SHA artifact (`aft`, `aft.real`, `build-info.json`, `source: "same-sha-artifact"`). Every image build below used it with `AFT_BINARY_SOURCE=prebuilt` and did not recompile.
- In-container executable provenance: verdict `verified`, `version_output "aft 0.56.2 (3e8d686af…)"`, launch exit 0.
- In-container harness control suite: 15 of 15 controls matched their expected outcomes, including the three provenance negative controls and the four three-state disk transitions.
- **Seven matrix scenarios, each executed individually via `AFT_E2E_SCENARIO`. All seven failed.**

| scenario | result |
| --- | --- |
| `read/T1/happy` | FAIL — `exact comparison failed`; actual was `{"success":false,"code":"permission_denied","message":"The \"read\" operation was refused because the OpenCode V2 host did not provide a permission request endpoint."}` |
| `glob/T1/happy` | FAIL — `projection_unparsed`, `"glob"` refused, same string |
| `grep/T1/happy` | FAIL — `projection_unparsed`, `"grep"` refused, same string |
| `search/T1/happy` | FAIL — `projection_unparsed`, `"aft_search"` refused, same string |
| `safety/T1/checkpoint_restore` | FAIL — `projection_unparsed`, `"edit"` refused, same string |
| `ast_replace/T1/happy` | FAIL — `projection_unparsed`, `"edit"` refused, same string |
| `import/T1/happy` | FAIL — `host exited null; accepted 0`; the host never exited (section C) |

- Direct probes of the real 2.0.11 host binary in a clean `linux/amd64` container under private `HOME`/XDG roots: `--version`, `serve`, and `api` against `/api/health`, `/api/info`, `/api/server` with correct, wrong, and absent passwords.

Not executed: the remaining rows, and any T4/T5/T7 scenario. No row is described below as observed unless it appears in that table.

## Mechanism A — our permission probe tests a member that does not exist. Ours.

Six of the seven executed scenarios failed with the same string, and the string is ours: `src/tools/definitions/v2.ts:193` emits `The "<permission>" operation was refused because the OpenCode V2 host did not provide a permission request endpoint.` The permission id in each message is the one the failing tool asks for — `read`, `glob`, `grep`, `aft_search`, `edit` — which is how each row below is tied to its ask site.

The chain:

1. `src/entry/server-runtime.mjs:31` passes the plugin Effect context into `hoistedV2ToolConsumers(context)`.
2. `src/tools/hoisted/v2.ts:20` probes `if (!("client" in host)) return {}`.
3. The GA context has no `client` member — not a narrowed one, none. Verified against the installed pin (`dist/effect/plugin.d.ts:25-53`): twenty-five members, `app`, `location`, `options`, `agent`, `aisdk`, `command`, `event`, `experimental`, `integration`, `mcp`, `model`, `generate`, `permission`, `plugin`, `provider`, `reference`, `rpc`, `session`, `shell`, `skill`, `storage`, `tool`, `vcs`, `websearch`, `worktree`. The context is a set of per-domain `Pick<…Api, …>` facades, and the permission facade is `Pick<PermissionApi, "list" | "get" | "reply"> & { hook }` (`dist/effect/permission.d.ts:19-21`), with no `create`. Core's runtime object has the same keys (`snapshot-qgzk9bq2.js:197-503`, permission domain at `:412-417`).
4. So the probe is always false, `consumers.requestPermission` is never set, and `runtimeFor`'s `ask` rejects at `definitions/v2.ts:191-198`.

**No host interaction occurs.** Nothing is requested and denied upstream; the refusal is manufactured inside the plugin on a membership test for a key this contract has never defined, in 2.0.3 or 2.0.11.

### Why these rows are `applicable` and not `expected_fail:37164`

Upstream 37164 asks the host to expose a permission-request capability inside the V1-style `tool.execute.before` hook. Whatever its merits, it cannot be the justification for a failure that happens before the host is consulted. The thirty rows that carried it are now `applicable`: `expected_fail` has to earn its place and this one did not, and `n/a` would be false because the operation is real and attempted. `applicable` is not a claim that these rows pass — every one of them fails, and seven of them were watched doing it.

**Explicit dependency.** Whether a supported GA mechanism exists for a plugin to initiate a permission decision — the `permission.hook` the context does expose, or another route to `POST /api/session/:sessionID/permission` — is being determined by the task that owns `src/permissions/v2.ts`, `src/tools/hoisted/v2.ts` and `src/tools/definitions/v2.ts`. If that task concludes no supported mechanism exists, these rows may belong back under `expected_fail` with an issue that describes the real gap. They should not return to 37164.

For the record, and so it is not mistaken for a rebuttal: `permission.create` does exist on the full `@opencode/client` in both 2.0.3 and 2.0.11, with a `PermissionCreateInput` that is field-for-field identical and accepts exactly what `src/permissions/v2.ts:148-155` sends. That client is handed to the **TUI** context (`dist/tui/context.d.ts:449`), which runs in a different process from the tools. Audit points 2 and 4 hold both halves.

### Which rows this reaches

Every row whose scenario invokes a tool that reaches an unconditional `context.ask`. The ask sites and their callers, traced from the call sites rather than from `V2_PERMISSION_ASK_INVENTORY` — whose entries are permission ids, not tool names, so `"edit"` covers everything asking through `askEditPermission`:

| ask site | permission id | tools reaching it unconditionally |
| --- | --- | --- |
| `hoisted.ts:442` | `read` | `read` ✔ observed |
| `permissions.ts:231` via `askEditPermission` | `edit` | `write`, `edit`, `apply_patch` (`hoisted.ts:573,604,767,899,933,1096`), `aft_safety` restore (`safety.ts:112,160`) ✔ observed, `ast_replace` (`ast.ts:151`) ✔ observed, `aft_import` (`imports.ts:104`) |
| `hoisted.ts:1196`, `:1276` | `edit` | `delete`, `move` |
| `permissions.ts:627` via `askGrepPermission` | `grep` | `grep` (`search.ts:178`) ✔ observed |
| `permissions.ts:627` via `askSearchPermission` | `aft_search` | `aft_search` (`semantic.ts:161`) ✔ observed |
| `permissions.ts:686` via `askGlobPermission` | `glob` | `glob` (`search.ts:259`) ✔ observed |
| `bash.ts:182,435` | `bash`, `external_directory` | `bash` |

`assertExternalDirectoryPermission` (`permissions.ts:465`) is called by nearly every tool but only asks for targets outside the project, so it does not fire on in-project fixtures.

Against what each scenario actually calls, that is: T1 for `read`, `write`, `edit`, `apply_patch`, `delete`, `move`, `bash`, `glob`, `grep`, `search`, `import`, `safety`, `ast_replace`; T1 for `bash_kill`, `bash_status`, `bash_watch`, `bash_write`, each of which calls `bash` first to create the task it operates on; T3 for the seven mutators; T4, T5, T6 for `bash`; T6 for `glob`, `grep`, `search`; and the V2 leg of every corresponding T7. That is the complete set of thirty rows.

### A divergence from the live GA evidence, worth resolving

The task-giver's GA run reported `glob` and `grep` **working**. In the harness they fail, with AFT's refusal string naming AFT's own permission ids. So the harness exercises **AFT's** `glob` and `grep`, not the host's natives — which settles a question an earlier draft of this report left open, and settles it against the collision theory: AFT registers tools named `glob` and `grep` without removing the host's (`tool-registration.ts:24,183-184` removes only `read`, `edit`, `write`, `apply_patch`), and in the harness AFT's win.

Two possibilities for the GA result, and I cannot choose between them from here: the collision resolves the other way on that host, or that host ran a plugin build without these ask sites. It matters, because if a real GA user's `glob` reaches the native tool then AFT's `glob`/`grep` are dead code on V2 and their matrix rows measure something nobody reaches. A single GA `glob` call checked for AFT's truncation trailer — which the native tool does not emit — distinguishes them.

## Mechanism B — AFT asks permission on every read, including reads inside the project. Ours.

`read` being denied is a tell, and the answer is yes: our wrapper requests permission for reads unconditionally.

`createReadTool` calls `context.ask({permission: "read", patterns: [filePath], always: ["*"], metadata: {}})` at `src/tools/hoisted.ts:440-448` on every invocation, after the external-directory check and before reading anything. There is no in-project short-circuit — the ask does not depend on whether `filePath` is inside the project root, on a saved rule, or on any argument. The `assertExternalDirectoryPermission` call directly above it (`:433`) is the gate that *is* scoped to external paths, and it is separate.

`read/T1/happy` reads `sample.txt` from its own fixture and still hits it, which is what the executed run shows.

This is independent of mechanism A and survives fixing it. AFT already sets `options.permission: "read"` on the projected tool (`definitions/v2.ts:128-130`), the declarative label the host evaluates config rules against (audit point 10). An explicit ask on top of that is a second gate the host's own read tool does not have, so an in-project read that should be silent would prompt — once per session at best, given `always: ["*"]`.

Attribution: ours, in product code now owned by the task that owns `src/tools/`. Named here, not fixed.

## Mechanism C — `aft_import` hangs instead of refusing. Undetermined.

`import/T1/happy` failed unlike every other permission row: `host exited null; accepted 0`. The host process never exited and was killed at the scenario timeout, rather than returning the refusal that `safety/T1` and `ast_replace/T1` returned from the same `askEditPermission` site.

`aft_import` asks with permission id `edit` (`src/tools/imports.ts:104`), so mechanism A should produce a clean refusal here as it does for its two siblings. It does not. Something on the `aft_import` path either swallows the rejection or leaves a handle open. One observation, not reproduced, and not attributable from here between our code and the harness scenario.

## Mechanism D — the captured host contracts will block the pin move. Harness's.

Not currently firing, because the harness pin was reverted out of this task's scope and still reads 2.0.3, matching the contracts. It fires the moment `v2Version` moves to `2.0.11`. Observed while the edit was in place:

```
HarnessError: contract_uncaptured:host_cli_contract must carry the pinned host version and observed run id
    at contractIdentity (/workspace/tests/docker/opencode2/harness/contracts.ts:65:5)
    at loadHostCliContract (/workspace/tests/docker/opencode2/harness/contracts.ts:86:20)
    at async validateHarnessInputs (/workspace/tests/docker/opencode2/harness/validation.ts:858:11)
HarnessError details: {"expected_version":"2.0.11","observed_version":"2.0.3","observed_run_id":"oc2-ga-inspect-t4"}
```

Affected: `contract/host-cli-contract.json`, `contract/host-provider-config.json`, `contract/host-schema-rejection.json`. The failure is unsuppressible and sits inside `validateHarnessInputs`, so `--validate-only` does not get past it either. It is not a defect — it is the harness correctly refusing to credit 2.0.3 observations to a 2.0.11 host.

**Re-stamping the version is not sufficient.** One contract describes an endpoint 2.0.11 no longer serves. Observed against the real 2.0.11 binary:

| request | 2.0.11 result |
| --- | --- |
| `api --server $EP GET /api/health` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/server` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/info` (correct password) | exit 0, `{"version":"2.0.11","pid":37,"urls":["http://127.0.0.1:4096"],"paths":{"tmp":"/tmp/opencode"}}` |
| `GET /api/info`, wrong password | exit 1, `Error: Server at <endpoint> did not provide a compatible V2 health response` |
| `GET /api/info`, no password | exit 1, same error |

Matching the packages: `@opencode/protocol@2.0.3/dist/groups/health.js:13` publishes `health.get` at `/api/health` returning `{healthy, version, pid}`; at 2.0.11 the health group is gone from `dist/groups/` entirely and `@opencode/protocol@2.0.11/dist/groups/server.js:13` publishes `server.info` at `/api/info` returning `{version, pid, urls, paths:{tmp}}`.

`contract/host-cli-contract.json:101` sets `shared_server_smoke.path` to `/api/health` with `expected_status: 0`, and `harness/host.ts:310-319` fails the run with `host_failed: "shared-server smoke correct attach failed"` when that call's exit code is not 0. So fixing only the version stamp moves the wall to the smoke and kills every shared-server scenario — `bash/T4/abort`, `bash/T5/completion_wake`, `bash/T5/watch_pattern_once`, `inspect/T4/abort`.

The serve handoff needs no re-capture: `server listening on http://127.0.0.1:4096` and `server password <redacted>` still match both `stdout_pattern`s, and both password negative controls still exit non-zero with the recorded stable error strings.

Whoever repairs this: re-capture all three contracts against 2.0.11, move `shared_server_smoke` to `GET /api/info`, and replace the `{healthy: true, version}` positive-control assertion with the observed `ServerInfo` body. The provider-config and schema-rejection contracts need real captures, not a version bump; neither was re-observed here.

## Mechanism E — T7's V1 leg. Host's, on the V1 line.

`https://github.com/anomalyco/opencode/issues/48340` is readable and is not what its placement suggests. It is titled "Plugin dispose is not called after one-shot run final stop", it is open, and it is filed against **`opencode-ai@1.18.29`** — the V1 host, not any `@opencode/*` package. Its claim: after `opencode run … --auto` emits the final `step_finish` with `reason: "stop"`, the process stays alive and never invokes the plugin's `dispose`.

T7 is the only trajectory that runs the V1 host. It is materialized from each tool's T1 scenario (`harness/scenario-loader.ts:299-308`), run once on each host, and its projected text compared (`harness/driver.ts:1050-1096`); the V1 leg uses `OPENCODE1_BIN`, installed at `opencode-ai@1.18.30`. A V1 host that does not exit cannot yield a text to compare, so the label describes a real defect on a path T7 uses. The 23 T7 rows keep it.

Two qualifications:

1. **No V2 contract point proves it, and none can.** 48340 is a V1 defect; the V1 host is pinned separately and did not move. "Does it still hold at the new pin" has the answer: the new pin is not the pin that governs it.
2. **It was not re-verified at 1.18.30.** The issue names 1.18.29; the harness pins 1.18.30; 1.18.31 exists. An open issue is not proof a later build still hangs, and no T7 scenario was executed. The label is retained on the strength of the issue and the path, not an observation at the pinned version.

T7 also inherits T1: for every tool in mechanism A the T7 V2 leg fails on the refusal before parity is compared, so 48340 was never the only thing failing those rows.

## Summary

| Mechanism | Attribution | Rows reached | Strongest evidence |
| --- | --- | --- | --- |
| A. `"client" in host` probes a member the GA contract never defines | Ours | All 30 former `37164` rows | Six executed refusals naming our own permission ids; `dist/effect/plugin.d.ts:25-53`; `hoisted/v2.ts:20`; `definitions/v2.ts:193` |
| B. Unconditional permission ask on every read | Ours | `read` at T1/T3/T7 | `read/T1/happy` executed; `hoisted.ts:440-448` against `definitions/v2.ts:128-130` |
| C. `aft_import` hangs instead of refusing | Undetermined | `import/T1` | `host exited null`, one run, not reproduced |
| D. Host contracts pinned at 2.0.3; `/api/health` removed at 2.0.11 | Harness | All rows once the pin moves, plus the four shared-server scenarios again | Observed `contract_uncaptured`; observed 404; `protocol@2.0.11/dist/groups/server.js:13` |
| E. V1 host does not dispose after a one-shot run | Host (V1 line) | All 23 T7 rows | Upstream 48340 against `opencode-ai@1.18.29`; not re-verified at 1.18.30 |

## Open questions

1. Does a GA model call to `glob`/`grep` reach AFT's tool or the host's native? The harness says AFT's; the live GA run implies otherwise. Decides whether those rows measure anything a user reaches.
2. Does a supported GA mechanism exist for a plugin to initiate a permission decision? Owned by the task holding `src/permissions/v2.ts`. Decides whether the thirty rows stay `applicable`.
3. Does `aft_import` hang reproducibly, and where?
4. Does `opencode-ai@1.18.30` still fail to dispose? Decides all 23 T7 rows.
