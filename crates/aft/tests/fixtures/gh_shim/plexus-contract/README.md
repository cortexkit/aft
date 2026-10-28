# gh shim `gh.route` wire goldens

What AFT's `gh` shim (`crates/aft/src/gh_shim.rs`) writes to the `gh.route`
operation (wire schema `gh_route_schema: 1`) and what it accepts back, for
the four governed families:

| Family | Verbs |
| --- | --- |
| v1 | `issue comment`, `pr comment`, `pr review`, `issue reaction` |
| v10 | `--edit-last` on `issue comment` / `pr comment` |
| v12 | `issue close` (with reason), `issue reopen`, `pr close`, `pr reopen` |
| v14 | `issue create`, own-issue `issue edit` (title/body, labels), `api -X PATCH /repos/{o}/{r}/issues/comments/{id}` (body only) |

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
three v14 speech rows (`issue create`, own-issue `issue edit`, the own-comment
PATCH), published as `manifest_version: 14`, bound
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

## The request

Built by `governed_wire_request` (`crates/aft/src/gh_shim.rs:4060-4115`),
serialized by `serde_json::to_vec` (`:3865`), sent unchanged as the request
frame body on the route opened for module `prefrontal-core` (`:3838-3851`).

Two shapes:

- **Speech** (v1, v10, v14): `operation` (`"gh.route"`), `gh_route_schema`
  (`1`), `action` (the verb tuple, or `api:PATCH:/repos/*/*/issues/comments/*`
  for the PATCH), `target`, `body`, `repository`, `manifest_version`,
  `rung_as_of_unix_secs`, `metadata`; plus `edit_last: true` only for
  `--edit-last`, and `author_scope: "own"` only for the PATCH and
  `issue edit`.
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
"not planned"` goes out as `not_planned`. `issue edit` sends the issue number
as `target.number`, only the fields given (`title`, `body`), and collects
`--add-label` / `--remove-label` into `add_labels` / `remove_labels` arrays
(`--add-assignee` / `--remove-assignee` likewise, not captured here).

### Own-issue `issue edit`

The shim does not check authorship; it marks the request `author_scope:
"own"` and the holder reads the issue and refuses when its author is not the
calling seat's bot. That refusal code is `issue_edit_not_own`, taken from
prefrontal's `crates/prefrontal-core-module/src/gh_route.rs`
(`REFUSAL_ISSUE_EDIT_NOT_OWN`, line 52; returned by `route_issue_edit` at line
1251, prefrontal commit `1d8066e9e`). The holder builds it with its `refusal`
helper (line 1810), whose JSON is exactly
`{"outcome":"refusal","refusal_code":"issue_edit_not_own"}`; that is the byte
content of `responses/refusal-issue_edit_not_own.response.json`. The code is
read from prefrontal's source, not captured from a live holder. Two requests
are captured: `v14-issue-edit-title-body` and `v14-issue-edit-labels`, each
with success and not-own refusal exchanges, plus one generic
(`identity_mismatch`) refusal for the title/body edit. The success exchanges
reuse the generic `success-result.response.json`; the holder's real success
body for an issue edit was not available.

Under `GH_SHIM_BYPASS=operator` a label-only `issue edit` does not use this
route at all: it runs upstream `gh` as the operator after an audit line (see
`exits.md`). That path writes nothing to `gh.route`, so it has no golden here.

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
order. An older request fixture with alphabetically sorted keys and
`manifest_version: 1` did not match what the shim sends and no test read it;
it was deleted in favour of the captures in `requests/`.

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
create`, `issue edit` and PATCH for refusal, plus the holder's
`issue_edit_not_own` refusal for both `issue edit` requests); upstream error, unbound identity and the
partial state change are exercised once each.

## Request and reply contract vectors (plexus's own)

`requests-contract/` and `replies-contract/` are written and proven by
plexus, not captured from AFT. They pin what plexus's `gh.route` facade
accepts and how it answers, so AFT's gh shim tests can fail on a new verb's
shape before a live write does. Each file is proven on every plexus test run
against the real code; nothing in either directory is typed as an
expectation without a test reading it.

### `requests-contract/<name>.json`

```json
{"name": "<file name without .json>",
 "request": { <the full gh.route request> },
 "expect": {"accepted": true} | {"refused": "<refusal code>"}}
```

`request` is the whole request the shim writes, including its envelope
(`operation`, `gh_route_schema`, `manifest_version`, `rung_as_of_unix_secs`,
`metadata`) and `repository`. Every vector carries the full envelope, so a
refused vector differs from an accepted one only in the field its name
describes. `tests/it/gh_route_request_contract.rs` runs each request through
`plexus_core::github::facade::request_map::map` (the facade step that decides
whether a request's shape is accepted, with no GitHub contact) and asserts:

- `{"accepted": true}`: `map` accepts the request;
- `{"refused": code}`: `map` refuses it with exactly `code`, compared as a
  whole string. `request_field_forbidden{field: base}` is a different code
  from `request_field_forbidden{field: title}`; the field name is part of it.
  When the facade refuses a request here, the reply's `refusal_code` is this
  string.

Coverage: one accepted vector for every request kind in
`effects::REQUEST_KINDS` (the facade's list of every kind it routes), with both the
`issue` and `pr` spellings where `map` takes both; for the closed-shape kinds
(`pr ready` / its `undo` draft form, `pr edit`, the review-comment reply,
thread resolve/unresolve, stack create/add/unstack) one refused vector per
rule: an unknown top-level, target and body field, each missing required
field, each wrong type, and the kind's own rules (`pr edit` refuses `body.base`
and needs `author_scope: "own"`; the stack `pull_requests` list must be 1-50
distinct positive integers); the `pr create` head and base rules; the reaction
vocabulary; `pr review` events; `issue edit`'s `state`, `milestone` and list
fields; close/reopen `comment`, `body` and `title`; the six repository
selectors refused on every kind (`owner`, `repo`, `full_name`,
`repositories`, `target_repository`, `path_repository`); and unmapped kinds.

Two things a reader might not expect. There are no unknown-field vectors for
the speech kinds (comments, review, reaction, issue create and edit,
close/reopen) or for `pr create`'s body: `map` has no closed field list for
them and copies only each kind's declared fields towards GitHub
(`resolver::RequestKind::body_fields`), so an unknown field there is dropped,
not refused. And a target number that is not a number refuses as
`request_kind_unmapped` on the speech and `pr ready` kinds, but as
`request_field_forbidden{field: number}` on the closed nested kinds.

What `map` does not decide is not here: the facade looks up the binding for
`repository` before calling `map`, so an unbound repository refuses earlier
(`repository_unbound`), and the request nonce is checked outside the request
body.

**A new request kind must add vectors.** The test derives the kind list from
`effects::REQUEST_KINDS` and fails while any kind lacks an accepted vector; a
closed-shape kind should also get its refused vectors. An empty or missing
directory fails the test too.

### `replies-contract/<name>.json`

```json
{"name": "<file name without .json>",
 "request": { <the full gh.route request> },
 "github_reads": [{"request_line": "GET /repos/org/repo/pulls/7 HTTP/1.1",
                   "request_body": null | { <GraphQL body> },
                   "status": 200, "response": { <GitHub's answer> }}, ...],
 "reply": { <the facade's full reply> }}
```

These are replies after a GitHub pre-read, which `map` alone cannot reach.
`tests/it/github_facade_vendor_band/v22.rs`
(`v22_reply_contract_files_match_the_facade`) sends each request through the
real facade (`bot_request_scoped`) with a fake GitHub server answering the
reads in `github_reads` order, followed by any writes in the optional
`github_writes` array, then compares the rendered file to the stored one byte
for byte. `request_line` and `request_body` are what the facade sent; the
refusal cases in this directory send no writes. `resolve-thread-own.json`
records an accepted resolve and its mutation in `github_writes`. The bound
repository in this harness is
`org/repo` and the bot is `app-fixture[bot]`.

`reply` is exactly what the facade returns:
`{"repo_binding_generation": <n>, "result": {...}}`, where `result` has
`status: "refused"`, `refusal_code`, `retry_same_nonce`, and, for the typed
refusals, `code` and `detail`:

- `pr_edit_not_own`: `detail.message`;
- `stack_pr_not_own`: `detail.number`, `detail.author_login`,
  `detail.message`.
- `review_thread_not_own`: `detail.number`, `detail.author_login`,
  `detail.message`. Resolve refuses PRs the bot did not open; unresolve still
  goes to GitHub without a PR ownership check.

`pr ready` (and its draft form) refuses a pull request the bot did not open
with a bare `refusal_code: "pr_edit_not_own"` and no `code` or `detail`; see
`pr-ready-not-own.json`.

Keys in these files are sorted; parse them rather than relying on key order.
Every refusal code `src/github/facade/vendor_band.rs` writes as a string
literal must have a reply file, or be listed with its reason in the test's
`NOT_V22_REPLIES`; the test reads the source and fails otherwise.

Regenerate after an intended reply change, then review the diff:

```sh
PLEXUS_REGENERATE_GH_ROUTE_REPLIES=1 cargo test -p plexus-core --test it v22_reply_contract
```
