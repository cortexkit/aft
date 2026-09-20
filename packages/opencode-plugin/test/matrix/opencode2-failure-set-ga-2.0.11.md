# OpenCode 2 matrix: the real failure set at GA 2.0.11

- Date: 2026-09-20
- Checkout: `c23b6073f15682e798a54261dc1dfa890734c2e0`
- Pinned V2 host: `@opencode/cli@2.0.11` (from `packages/opencode-plugin/test/load-matrix/load-matrix.ts`, read by `tests/docker/opencode2/harness/pin.ts`)
- Pinned V1 host: `opencode-ai@1.18.30` (from `.github/opencode-version.txt`)
- Contract audit this report leans on: `delta-audit-ga-2.0.11.md`

This is the honest list, grouped by mechanism and attributed. It is deliberately not a repair.

## What was actually executed

Docker here is `linux/amd64` under emulation on an `aarch64` host, so everything below is slow and everything below was run.

- A `linux/amd64` release build of `agent-file-tools` was produced once (11m34s) and staged as a same-SHA artifact (`aft`, `aft.real`, `build-info.json`, `source: "same-sha-artifact"`). The harness image was then built with `AFT_BINARY_SOURCE=prebuilt`, so it did not recompile.
- `tests/docker/run-opencode2-test.sh` with `AFT_E2E_SCENARIO=read/T1`, `AFT_E2E_CONCURRENCY=1`. Inside the container it completed, in order: executable provenance verification and the harness control suite. It then stopped in input validation, before any scenario ran.
  - Provenance verdict `verified`: `executable_path /aft-artifact/aft`, `observed_sha256` equal to the producer sidecar, `checkout_git_sha` equal to HEAD, `version_output "aft 0.56.2 (c23b6073f15682e798a54261dc1dfa890734c2e0)"`, launch exit 0.
  - Harness control suite: 15 of 15 controls observed their expected outcomes, including the three provenance negative controls and the four three-state disk transitions.
- Direct probes of the real pinned host binary (`@opencode/cli-linux-x64@2.0.11`) in a clean `linux/amd64` container, under private `HOME` and XDG roots: `--version`, `serve`, and `api` against three routes with correct, wrong, and absent passwords.

**Zero matrix scenarios executed.** Not one. Mechanism A below is why, and it blocks every row of every tool, so no per-row pass/fail measurement exists at this pin. Where this report says a row fails, it says so from code and package evidence and labels it as such; it never reports a row as observed.

## Mechanism A — the captured host contracts still describe 2.0.3. Harness's.

The harness refuses to run until three committed contracts carry the pinned host version. They carry `2.0.3`.

Observed, verbatim:

```
HarnessError: contract_uncaptured:host_cli_contract must carry the pinned host version and observed run id
    at contractIdentity (/workspace/tests/docker/opencode2/harness/contracts.ts:65:5)
    at loadHostCliContract (/workspace/tests/docker/opencode2/harness/contracts.ts:86:20)
    at async validateHarnessInputs (/workspace/tests/docker/opencode2/harness/validation.ts:858:11)
HarnessError details: {"expected_version":"2.0.11","observed_version":"2.0.3","observed_run_id":"oc2-ga-inspect-t4"}
```

Affected: `contract/host-cli-contract.json`, `contract/host-provider-config.json`, `contract/host-schema-rejection.json`, each `"host_version": "2.0.3"`. The failure is unsuppressible and sits in `validateHarnessInputs`, so `--validate-only` does not get past it either.

Attribution: the harness's, and squarely a consequence of moving the pin. It is not a defect; it is the harness correctly refusing to credit 2.0.3 observations to a 2.0.11 host.

**Re-stamping the version is not sufficient.** One of those contracts describes an endpoint the 2.0.11 host no longer serves. Observed against the real 2.0.11 binary:

| request | 2.0.11 result |
| --- | --- |
| `api --server $EP GET /api/health` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/server` (correct password) | exit 1, `HTTP 404 Not Found` |
| `api --server $EP GET /api/info` (correct password) | exit 0, `{"version":"2.0.11","pid":37,"urls":["http://127.0.0.1:4096"],"paths":{"tmp":"/tmp/opencode"}}` |
| `GET /api/info` with a wrong password | exit 1, `Error: Server at <endpoint> did not provide a compatible V2 health response` |
| `GET /api/info` with no password | exit 1, same error |

That matches the packages: `@opencode/protocol@2.0.3/dist/groups/health.js:13` publishes `health.get` at `/api/health` returning `{healthy: true, version, pid}`; at 2.0.11 the health group is gone from `dist/groups/` entirely and `@opencode/protocol@2.0.11/dist/groups/server.js:13` publishes `server.info` at `/api/info` returning `{version, pid, urls, paths:{tmp}}`. The client mirrors it: `@opencode/client@2.0.3/dist/promise/client.d.ts:13-14` has `health.get`, `@opencode/client@2.0.11/dist/promise/client.d.ts:13-14` has `server.info`.

`contract/host-cli-contract.json:101` sets `shared_server_smoke.path` to `/api/health` with `expected_status: 0`, and `harness/host.ts:310-319` fails the run with `host_failed: "shared-server smoke correct attach failed"` when that call's exit code is not 0. So the moment the version stamp is fixed, every shared-server scenario — `bash/T4/abort`, `bash/T5/completion_wake`, `bash/T5/watch_pattern_once`, `inspect/T4/abort` — dies on the smoke instead, and the smoke would stop proving attachment.

The serve handoff itself is unchanged and needs no re-capture: `server listening on http://127.0.0.1:4096` and `server password <redacted>` both still match `endpoint_handoff.stdout_pattern` and `password_handoff.stdout_pattern`, and both negative controls still exit non-zero with the same stable error strings the contract records.

Next action for whoever repairs this: re-capture the three contracts against 2.0.11, moving `shared_server_smoke` to `GET /api/info` and replacing the `{healthy: true, version}` positive-control assertion with the observed `ServerInfo` body. The provider-config and schema-rejection contracts need real captures rather than a version bump, because neither was re-observed here.

## Mechanism B — hoisted mutators cannot request permission. Ours.

Every AFT tool that asks for permission refuses on the V2 host, and has since GA. This is the mechanism the retired `expected_fail:37164` label was covering.

AFT's `requestPermission` needs `host.client.permission.create` and `host.client.event.subscribe` (`src/permissions/v2.ts:36-45`). The server entry hands it the plugin Effect context (`src/entry/server-runtime.mjs:31`), and that context has no `client` — audit point 4, confirmed on both the declared surface (`@opencode/plugin@2.0.11/dist/effect/plugin.d.ts:25-53`) and the runtime object that populates it (`@opencode/core@2.0.11/dist/chunks/snapshot-qgzk9bq2.js:197-503`). `hoistedV2ToolConsumers`' guard `if (!("client" in host)) return {}` (`src/tools/hoisted/v2.ts:20`) therefore always returns no consumer, and `runtimeFor`'s `ask` rejects with `The "<op>" operation was refused because the OpenCode V2 host did not provide a permission request endpoint.` (`src/tools/definitions/v2.ts:191-198`). The harness captured that exact refusal at 2.0.3 in `contract/probe/bash-t1-permission-refusal.txt`, including the plugin's own log of the context keys with no `client` among them.

Rows this reaches, from the closed and tested ask inventory (`src/tools/hoisted/v2.ts:7-16`, pinned by `test/permissions/ask-site-inventory.test.ts`) — read, edit, write, apply_patch, aft_delete, aft_move, bash:

- T1: `read`, `edit`, `write`, `apply_patch`, `delete`, `move`, `bash`
- T3: `read`, `edit`, `write`, `apply_patch`, `delete`, `move`, `bash`
- T4, T5, T6: `bash`, whose scenarios all run a command and so all reach the same ask
- T7: the V2 leg of each of the above, since T7 is materialized from T1

The T1 rows are in that list on code evidence, not on the strength of the label that was just retired. `read` calls `context.ask({permission: "read", ...})` on every invocation before it reads anything (`src/tools/hoisted.ts:440-448`), and the filesystem mutators go through `askEditPermission`, which asks unconditionally too (`src/tools/permissions.ts:222-240`). There is no happy path through those tools that skips the ask. `bash` asks only when the binary answers `permission_required`, which it does under the harness's `bash_permissions: true` configuration — the 2.0.3 probe recorded the refusal for a `printf` fixture command. The four non-asking bash-family tools are not in this list: `V2_BASH_TOOLS` is `{bash, aft_bash}` (`src/tools/definitions/v2.ts:7`), so `bash_write`, `bash_status`, `bash_kill`, and `bash_watch` never enter the permission loop.

Attribution: ours. The capability exists on the API we call, unchanged across both pins: `permission.create` on the client (`@opencode/client@2.0.11/dist/promise/client.d.ts:157-160`), with `PermissionCreateInput` (`.../generated/types.d.ts:7506-7615`) accepting field for field exactly what `src/permissions/v2.ts:148-155` sends, and `POST /api/session/:sessionID/permission` unchanged in the protocol. Nothing upstream is missing. We hand a context where a client is required.

**A fix that only supplies a client will not work.** Audit point 5: `client.event.subscribe(options?)` returns `AsyncIterable<V2Event>` directly in both 2.0.3 and 2.0.11 (`@opencode/client@2.0.11/dist/promise/client.d.ts:10-12`), while `src/permissions/v2.ts:42-45,142-143` declares it as `Promise<{stream}>` and reads `.stream` off the awaited value. Against a real client that read is `undefined`, which is the same shape of dereference failure the 2.0.3 probe already recorded once. Both defects sit on the same path and want fixing together.

## Mechanism C — thirteen rows whose real cause is unknown and was never measured. Unknown.

Thirteen rows across ten tools carried the permission excuse for tools that never request permission: the ask inventory does not contain `search`, `glob`, `grep`, `import`, `safety`, `ast_replace`, `bash_kill`, `bash_status`, `bash_watch`, or `bash_write`, so mechanism B cannot describe them.

- T1: `search`, `glob`, `grep`, `import`, `safety`, `ast_replace`, `bash_kill`, `bash_status`, `bash_watch`, `bash_write`
- T6: `search`, `glob`, `grep`

These are now `applicable`. Whether they pass, and if not why, is unmeasured — mechanism A stopped the run before any of them executed. Four of them are the interesting case the label may have been hiding a working path on: `import/T1`, `safety/T1`, `ast_replace/T1`, and `bash_write/T1` are plain single-call scenarios with no ask site and no list surface, so they have no obvious reason to fail at all. (`import`, `safety` and `ast_replace` do carry `options.permission: "edit"` and `bash_write` carries `"bash"` from `hostPermission` in `src/tools/definitions/v2.ts:128-147`, but audit point 10 shows the host reads that label only to decide whether config rules disable a tool outright — it never prompts.) If they pass on the first run after mechanism A is cleared, they were green all along and the label was reporting them as expected failures. That is exactly the case worth calling out by name, and this report cannot yet call it either way.

The three T6 rows (`search`, `glob`, `grep`) are truncation-trailer comparisons against the Rust list-surface registry and are the more likely of this group to fail for a real reason.

## Mechanism D — T7's V1 leg. Host's, on the V1 line.

`https://github.com/anomalyco/opencode/issues/48340` is readable and is *not* what the label's placement suggests. It is titled "Plugin dispose is not called after one-shot run final stop", it is open, and it is reported against **`opencode-ai@1.18.29`** — the V1 host, not any `@opencode/*` package. Its claim: after `opencode run ... --auto` emits the final `step_finish` with `reason: "stop"`, the process stays alive and never invokes the plugin's `dispose`, so plugin-owned handles are never released and the CLI does not exit.

T7 is the only trajectory that runs the V1 host. It is materialized from each tool's T1 scenario (`harness/scenario-loader.ts:299-308`), run once on each host, and its projected text compared (`harness/driver.ts:1050-1096`); the V1 leg uses `OPENCODE1_BIN`, which the image installs at `opencode-ai@1.18.30`. A V1 host that does not exit after a one-shot run cannot yield a projected text to compare, so the label describes a real defect on a path T7 genuinely uses.

Two honest qualifications:

1. **No V2 contract point proves it, and none can.** 48340 is a V1 defect. The 2.0.11 pin move neither fixes nor worsens it, because the V1 host is pinned separately in `.github/opencode-version.txt` and did not move. Asking whether 48340 "still holds at the new pin" has the answer: the new pin is not the pin that governs it.
2. **It was not re-verified at 1.18.30.** The issue names 1.18.29; the harness pins 1.18.30; 1.18.31 exists upstream. An open issue is not proof that a particular later build still hangs, and mechanism A prevented running a T7 scenario to check. The label is therefore retained on the strength of the issue and the path, not on an observation at the pinned version. Re-verifying it is the first thing worth doing once mechanism A is cleared — and if the V1 leg does terminate at 1.18.30, all 23 T7 rows are mislabelled and should follow the 37164 rows to `applicable`.

Note also that T7 inherits T1: for the seven tools in mechanism B, the T7 V2 leg fails on the permission refusal before parity is ever compared, so 48340 is not the only thing failing those rows and never was.

## Summary

| Mechanism | Attribution | Rows reached | Evidence |
| --- | --- | --- | --- |
| A. Host contracts pinned at 2.0.3; `/api/health` removed at 2.0.11 | Harness | All, plus the four shared-server scenarios a second time | Observed `contract_uncaptured` failure; observed 404 on `/api/health`; `@opencode/protocol@2.0.11/dist/groups/server.js:13` |
| B. No client on the server plugin context; `event.subscribe` shape mismatch | Ours | T1/T3 for 7 mutators, T4/T5/T6 for bash, their T7 legs | Audit points 2, 4, 5; `src/tools/hoisted/v2.ts:20`; `contract/probe/bash-t1-permission-refusal.txt` |
| C. Unknown cause, never measured | Unknown | 10 tools at T1, 3 at T6 | Ask inventory excludes all of them; no execution at this pin |
| D. V1 host does not dispose after a one-shot run | Host (V1 line) | All 23 T7 rows | Upstream 48340 against `opencode-ai@1.18.29`; V1 pinned at 1.18.30; not re-verified |
