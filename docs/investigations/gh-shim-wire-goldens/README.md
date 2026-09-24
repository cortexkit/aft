# gh shim `gh.route` wire goldens

What AFT's `gh` shim (`crates/aft/src/gh_shim.rs`) writes to the `gh.route`
operation (wire schema `gh_route_schema: 1`) and what it accepts back, for
the four governed families:

| Family | Verbs |
| --- | --- |
| v1 | `issue comment`, `pr comment`, `pr review`, `issue reaction` |
| v10 | `--edit-last` on `issue comment` / `pr comment` |
| v12 | `issue close` (with reason), `issue reopen`, `pr close`, `pr reopen` |
| v14 | `issue create`, `api -X PATCH /repos/{o}/{r}/issues/comments/{id}` (body only) |

## Real captures and derived files

| Path | Kind | How it was made |
| --- | --- | --- |
| `requests/<family>-<verb>.request.json` | **real capture** | the exact bytes the real `aft gh-shim` binary wrote as the `gh.route` request frame body, read by a fake route holder. Not typed by hand. |
| `requests/session-v1-issue-comment.frames.json` | real capture, normalized | every request frame body of one v1 exchange in arrival order (`catalog.list`, `route.open`, the `gh.route` request). The temporary project path and the pid are replaced with `<project_root>` / `<pid>`. |
| `exchanges/*.exchange.json` | **real capture** of the shim's side | argv, the request and response files used, and the exit code, stdout and stderr the real shim binary produced. |
| `responses/*.response.json` | **derived from code** | constructed from the shim's response parser and its existing test fixtures; see `responses/README.md` for the source line of each. No real prefrontal response was available (below). |
| `exits.md` | derived from code | response → exit code and stderr, with line references, cross-checked by the captured exchanges. |

### How the captures were made

`crates/aft/tests/gh_shim_wire_goldens_test.rs` runs the real
`target/debug/aft gh-shim <argv>` once per case against a loopback subc
daemon (the same fake-daemon pattern as the slow-daemon tests in
`crates/aft/tests/gh_shim_runtime_context_test.rs`). The fake holder
advertises `prefrontal-core` as the `gh.route` management surface, opens
route channel 42, records every request frame body, and answers the request
on channel 42 with the bytes of one `responses/` file, or stays silent for
the outcome-unknown case.

Each run gets its own temporary HOME, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`,
`AFT_GH_SHIM_STATE_DIR` and `AFT_STORAGE_DIR`; `GH_TOKEN`, `GITHUB_TOKEN`,
`GH_ENTERPRISE_TOKEN` and `GH_SHIM_BYPASS` are removed; PATH starts with a
recording stand-in for upstream `gh`, and every run asserts it was never
called. The manifest is the dev-signed `v12-manifest.json` fixture plus the
two v14 speech rows, published as `manifest_version: 14`, bound
`cortexkit/aft` → agent `alfonso-aft`. A fresh R3 rung record skips discovery.
Nothing contacted GitHub, prefrontal, the live daemon, or the real
`~/.local/state/cortexkit/aft/gh-shim/`.

Regenerate the captures (the hand-written `responses/` files are never
rewritten):

```sh
AFT_GH_SHIM_WIRE_GOLDENS_REGEN=1 CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= \
  cargo test -p agent-file-tools --test gh_shim_wire_goldens_test -- --test-threads=1
```

Without the variable the same test compares instead: each captured request
must equal its golden byte for byte except the two values that change per run
(below), and each exchange's exit code, stdout and stderr must equal the
recorded ones.

### Read-only look for real responses on this machine

- `~/.local/state/cortexkit/aft/gh-shim/seam-state.json`: `bound_holder`
  `prefrontal-core`, binding `cortexkit/insula` → `agent_2fd02cfeb9c0484f`,
  `last_seam_refusal` `{"code":"custody_unreachable","at_unix_secs":1790181348}`.
  That code is the only real holder output found. The shim stores the code
  alone, not the response bytes, so `refusal-custody_unreachable.response.json`
  uses the real code inside constructed bytes.
- `last-probe.json` (`{"stage":"open_route","elapsed_ms":4,"outcome":"ready"}`)
  and `rung-cache.json` (R3, `manifest_version` 13, recorded by `ck-aft`
  0.57.2) hold no response bodies. The live manifest is v13, so the v14 rows
  have never run against the live holder from this machine.
- `~/.local/share/cortexkit/aft/logs/`: no `gh.route` traffic; the shim runs
  before logging starts (`gh_shim.rs:313-315`).

Nothing read contained a token or credential, so nothing needed redacting.

## The request

Built by `governed_wire_request` (`crates/aft/src/gh_shim.rs:4060-4115`),
serialized by `serde_json::to_vec` (`:3865`), sent unchanged as the request
frame body on the route opened for module `prefrontal-core` (`:3838-3851`).

Two shapes:

- **Speech** (v1, v10, v14): `operation` (`"gh.route"`), `gh_route_schema`
  (`1`), `action` (the verb tuple, or `api:PATCH:/repos/*/*/issues/comments/*`
  for the PATCH), `target`, `body`, `repository`, `manifest_version`,
  `rung_as_of_unix_secs`, `metadata`; plus `edit_last: true` only for
  `--edit-last`, and `author_scope: "own"` only for the PATCH (and
  `issue edit`, which is not part of this set).
- **Thread state** (v12): `operation`, `gh_route_schema`, `verb`,
  `repository`, `number`, `manifest_version`, `rung_as_of_unix_secs`,
  `metadata`; plus `reason` (`completed` / `not_planned`, `issue close` only)
  and `comment` only when given. There is no `action`, `target` or `body`, so
  a `--delete-branch` flag has nowhere to go (`:4069-4091`).

`metadata` is `{"agent_id": <bound agent>, "pid": <shim process id>}`
(`:4065-4068`). The op name is the `operation` field; the subc route itself is
opened with `route.open` to `management_surface` `prefrontal-core` under the
identity `{project_root, harness: "aft-gh-shim", session: "gh-shim:<agent_id>"}`
(see the session record).

Target values are strings even when numeric (`"number":"42"`,
`"comment_id":"123"`). `issue create` sends `"target":{}` and collects repeated
`--label` flags into a `labels` array. `pr review` carries the event in
`body.event` (`COMMENT`, `APPROVE`, `REQUEST_CHANGES`). `issue close --reason
"not planned"` goes out as `not_planned`.

### Values that change per run

`metadata.pid` is the shim's own pid, and `rung_as_of_unix_secs` is the
timestamp of the R3 rung record that allowed the route. The test checks that
the pid is the spawned shim's pid and the timestamp is at least the one it
wrote, then masks both before the byte comparison. Everything else is
compared byte for byte.

### Key order

The captured requests list keys in the order the code builds them (for
example `operation`, `gh_route_schema`, `action`, ...), because this build of
AFT gets `serde_json`'s `preserve_order` feature through a dependency
(`oxc_resolver`). That order is a side effect of Cargo feature unification,
not a declared contract: a facade should parse the JSON and not rely on key
order. The older fixture
`crates/aft/tests/fixtures/gh_shim/gh-route-request-v1-golden.json` shows
keys sorted alphabetically in `serialized_json` and `manifest_version: 1`;
that file is not what this build sends, and no test reads it.

## Outcomes and exits

See `exits.md`. In short: success exits 0 (or 1 when the holder relays an
upstream error or a partial state change); any holder refusal is exit 86
`gh_shim_seam_refusal` with the holder's code echoed; `unbound_identity` is
exit 86 `gh_shim_unbound_identity`; no reply within 5000 ms after the request
was written is exit 87 `gh_shim_outcome_unknown`.

Coverage: every verb has a success exchange; each family has one refusal
exchange and one outcome-unknown exchange (v1 `issue comment`, v10 `pr comment
--edit-last`, v12 `pr close`, v14 PATCH for outcome unknown; v1 `issue
comment`, v10 `issue comment --edit-last`, v12 `issue close`, v14 `issue
create` and PATCH for refusal); upstream error, unbound identity and the
partial state change are exercised once each.
