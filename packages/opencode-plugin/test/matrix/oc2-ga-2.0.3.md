# OpenCode 2 GA 2.0.3 facts

Observed 2026-09-13 from unpacked npm tarballs and isolated real-host runs. The beta-19234 audit and probe files remain in place; this record supersedes them for V2.

## Package and loader facts

- The V2 package scope is `@opencode/*`. `@opencode/plugin@2.0.3/package.json:3-4` identifies the stable plugin; its exported GA specifiers are root, `/effect`, `/host`, and `/tui` at `package.json:12-32`. AFT pins exact `@opencode/plugin@2.0.3` while retaining the separate V1 `@opencode-ai/plugin` dependency.
- The GA decoder requires default `{ id: string, effect: function }` or `{ id: string, setup: function }`: `@opencode/core@2.0.3/dist/chunks/mime-771dt0vh.js:54-65`. It selects Effect first with `"effect" in value ? value : fromPromise(value)` at lines 83-89 and derives `features.tui`/`features.rpc` from resolved entrypoints at lines 89-94.
- `Host.resolve` tries `server` then root, and resolves TUI and RPC only from `tui` and `rpc`: `@opencode/plugin@2.0.3/dist/host.js:4-30`. `oc-plugin` has zero occurrences in unpacked core/plugin `dist`; AFT therefore removed that package field.
- The GA context includes `location`, `permission`, `rpc`, `session`, `storage`, `tool`, and other domains: `@opencode/plugin@2.0.3/dist/effect/plugin.d.ts:24-50`. Tool registration uses `ToolEditor.add/remove` and execute hooks (`dist/effect/tool.d.ts:7-51`).

## RPC and permission

Typed RPC dispatch is `context.rpc.register`: `@opencode/plugin@2.0.3/dist/effect/rpc.d.ts:6-18`. The live `getStatus` round trip produced:

```text
rpc-call:0:lifecycle-0
rpc-route:context.rpc.register;features.rpc=false;export.rpc=absent
rpc-call:1:lifecycle-1
rpc-reload-event:indexProgress
```

A `./rpc` export is unnecessary: it would only advertise `features.rpc`; AFT's server Effect already registers the typed handlers that work.

The GA permission domain has `hook`, `list`, `get`, `reply`, and `rules`, but no plugin request/create method (`@opencode/plugin@2.0.3/dist/effect/permission.d.ts:16-21`). Consequently permission-gated tool execution, foreground abort, and idle wake remain `expected_fail:https://github.com/anomalyco/opencode/issues/37164`:

```text
permission-api:expected_fail:upstream#37164:domain=hook,list,get,reply,rules;create=absent
abort-path:expected_fail:upstream#37164:permission_refused_before_process
idle-wake:expected_fail:upstream#37164:permission_refused_before_background_start
```

## Loader and tool transcript

```text
selected=effect features={"tui":true}
tools-listed:0:aft_callgraph,aft_conflicts,aft_delete,aft_import,aft_inspect,aft_move,aft_outline,aft_safety,aft_zoom,apply_patch,ast_grep_replace,ast_grep_search,bash,bash_kill,bash_status,bash_watch,bash_write,edit,read,write
```

The package intentionally exports `./server` and `./tui`, not `./rpc`. The pin is `@opencode/cli@2.0.3` plus `@opencode/cli-linux-x64@2.0.3` in Docker and load-matrix harnesses.

## Docker contract and classification delta

GA contract captures are `tests/docker/opencode2/contract/probe/*-ga-2.0.3.txt`. Serve/password handoff and negative-control classes are unchanged; provider package scope changed to `@opencode/ai`; schema rejection gained Zod 4 wording and provider execution metadata.

Compared with beta-19234, the permission surface did not gain a request endpoint. The previously applicable T6 rows for `bash`, `glob`, `grep`, and `search` are now recorded as #37164 expected failures because their incomplete fixture cannot execute far enough to emit its trailer.

GA renamed the active-execution control from `POST /api/session/:sessionID/abort` to `POST /api/session/:sessionID/interrupt`. The protocol returns `{ interrupted: boolean }`, with `true` proving an active execution was interrupted and `false` identifying an idle no-op (`@opencode/protocol@2.0.3/dist/groups/session.js:511-526`); the operation list has no `session.abort`. Both Docker T4 controls use the GA route and require an `interrupted:true` response. `inspect/T4` remains applicable, while `bash/T4` remains #37164 because permission refusal occurs before its process can start.
