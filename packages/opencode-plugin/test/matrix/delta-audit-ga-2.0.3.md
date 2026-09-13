# OpenCode V2 Delta Audit Evidence: 2.0.3 GA

- Harness owner: OpenCode (OC)
- Record ID: `oc2-ga-2.0.3`
- Date: 2026-09-13
- Evaluated range: `@opencode-ai/*@0.0.0-beta-19234` -> `@opencode/*@2.0.3`
- Coherent package set: `@opencode/cli`, `@opencode/core`, `@opencode/plugin`, and `@opencode/schema` at `2.0.3`
- Registry publication: `@opencode/cli@2.0.3` was published 2026-09-12T23:46:23.757Z
- Unpacked tarballs: `@opencode/core@2.0.3` (`sha1 540c5438150f8497c24d391da2f7a5a023b45c1d`), `@opencode/plugin@2.0.3` (`sha1 a106315476a4266d75c8e0ce6716ae053b56e237`)

The prior `delta-audit-beta-19234.md` record is retained. This record supersedes it for the pinned V2 host.

## Verdict

The scope, host API, and discovery metadata changed at GA. Loader shape, Effect-first selection, typed RPC registration, tool replacement, and interruption remain usable after updating specifiers. A plugin-initiated permission-request API is still absent (`expected_fail:upstream#37164`).

## Relied-on contract diff

1. **Permission endpoint — changed.** The GA Effect `Context` exposes a `permission` domain, but that domain only has `list`, `get`, `reply`, `rules`, and `hook`; it does not expose `ask`, `assert`, `create`, or a general client. The server-side plugin context implements exactly those five members. A plugin cannot initiate the permission prompt used by AFT's hoisted mutators. Citations: `@opencode/plugin@2.0.3/dist/effect/plugin.d.ts:24-50`, `@opencode/plugin@2.0.3/dist/effect/permission.d.ts:16-21`, `@opencode/core@2.0.3/dist/chunks/mime-pe4b2cf4.js:403-409`. Classification: `expected_fail:upstream#37164`.
2. **Effect entry precedence — preserved after scope rename.** The module decoder accepts default `{id,effect}` or `{id,setup}`, resolves `server` first, and chooses Effect when the value has an `effect` key. `tui` and `rpc` features come from resolvable subpaths. Citations: `@opencode/core@2.0.3/dist/chunks/mime-771dt0vh.js:54-65,72-101`; resolver order: `@opencode/plugin@2.0.3/dist/host.js:4-32`.
3. **Typed RPC — preserved through the Effect context, discovery split clarified.** Method dispatch is installed by `context.rpc.register`; the server context combines the RPC client and registration method. A resolvable `./rpc` only marks `features.rpc` during module loading. AFT therefore keeps `getStatus` registration in the server Effect and does not add a redundant `./rpc` export. Citations: `@opencode/plugin@2.0.3/dist/effect/rpc.d.ts:6-18`, `@opencode/core@2.0.3/dist/chunks/mime-pe4b2cf4.js:203-208`, `@opencode/core@2.0.3/dist/chunks/mime-771dt0vh.js:88-101`.
4. **Interruption ordering — preserved; HTTP surface renamed.** On provider interruption the runner interrupts tool fibers before awaiting all tool fibers, retries interruption if the join fails, then records unsettled-tool and assistant failures. GA renamed the HTTP control from `/api/session/:sessionID/abort` to `POST /api/session/:sessionID/interrupt`; its response is `{ interrupted: boolean }`, where `false` is the idle no-op. Citations: `@opencode/core@2.0.3/dist/chunks/mime-hf6kd63r.js:69-70,98-109,137-145`, `@opencode/protocol@2.0.3/dist/groups/session.js:511-526`.
5. **Built-in replacement — preserved through `ToolEditor.remove`.** The GA editor supports list/get/namespace/add/update/remove, and the host exposes the transform plus execute hooks. AFT can continue removing built-ins before adding projected replacements. Citations: `@opencode/plugin@2.0.3/dist/effect/tool.d.ts:7-19,20-51`, `@opencode/core@2.0.3/dist/chunks/mime-pe4b2cf4.js:442-446`.
6. **`path` header keys — rendering contract changed.** The beta renderer source paths are not published in the GA core/plugin tarballs. GA's read tool puts the requested path into model content while metadata only carries truncation; edit carries file diffs in `metadata.files`. AFT's own registered schemas and transcripts, rather than removed beta renderer line numbers, govern its path display. Citations: `@opencode/core@2.0.3/dist/chunks/mime-3zpd0x8a.js:78-116`, `@opencode/core@2.0.3/dist/chunks/mime-2b4vkxet.js:138-190`.
7. **Inert Effect dependency — changed package owner, V1 isolation retained.** The GA plugin declares exact `effect@4.0.0-rc.112`; AFT keeps Effect as a normal dependency and keeps the V1 `@opencode-ai/plugin` line separate. OpenTUI and Solid remain normal AFT dependencies, not peer externals. Citations: `@opencode/plugin@2.0.3/package.json:44-59`, `packages/opencode-plugin/package.json:37-60,76-80`.
8. **SessionID-scoped permission creation — request half removed from plugin API.** GA still checks `sessionID` ownership for `get` and `reply`, but the plugin context has no creation method. AFT cannot reproduce beta's `permission.create` request flow on the real GA context. Citation: `@opencode/core@2.0.3/dist/chunks/mime-pe4b2cf4.js:403-409`. Classification: `expected_fail:upstream#37164`.
9. **Dev/beta cadence — superseded by stable GA.** The coherent V2 packages are now under `@opencode/*`; both unpacked package manifests identify version `2.0.3`, while `@opencode-ai/plugin` latest remains on the V1 line. Citations: `@opencode/plugin@2.0.3/package.json:3-4,44-53`, `@opencode/core@2.0.3/package.json:3-4,104-127`.

## Discovery audit

`oc-plugin` has zero occurrences in the unpacked `@opencode/core@2.0.3/dist` and `@opencode/plugin@2.0.3/dist` trees. GA discovery is exclusively package-export resolution: server tries `<pkg>/server` then `<pkg>`, TUI tries `<pkg>/tui`, and RPC tries `<pkg>/rpc` (`@opencode/plugin@2.0.3/dist/host.js:4-32`).
