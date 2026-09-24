# Responses the shim accepts from `gh.route`

Each `*.response.json` file is the exact frame body the fake route holder in
`crates/aft/tests/gh_shim_wire_goldens_test.rs` sends back; the test reads the
file and writes its bytes to the route unchanged.

**Every file here is constructed from the shim's parser and its existing test
fixtures. None is a real response from prefrontal.** No real response body is
stored anywhere on this machine: the shim persists only the refusal *code* of
the last seam refusal (`seam-state.json`, written at
`crates/aft/src/gh_shim.rs:3884-3901`), and it runs before logging is set up
(`gh_shim.rs:313-315`), so the daemon logs under
`~/.local/share/cortexkit/aft/logs/` hold no `gh.route` traffic. See the
README one directory up for what the read-only look at the live state found.

Line numbers refer to `crates/aft/src/gh_shim.rs` at the commit that added
this directory.

| File | Outcome | Shape comes from | What the shim does with it |
| --- | --- | --- | --- |
| `success-result.response.json` | success (speech verbs: v1, v10, v14) | parser `gh_shim.rs:4127-4154` (`outcome:"result"`, `gh_route_schema` required and must be at most 1, `result` object, `field_order` array covering every `result` key exactly once, `render_governed_response` at `4267-4295`); fixture `crates/aft/tests/fixtures/gh_shim/gh-route-result-v1-golden.json:7-16` | prints each `field_order` field as `name: <json scalar>`; exit 0 |
| `success-applied-closed.response.json` | success (v12 close) | parser `gh_shim.rs:4167` + `render_applied_state` `4217-4226` (`state` string required, `state_reason` optional); fixture `gh_shim.rs:5786-5790` | prints `state`, then `state_reason` if present, one per line; exit 0 |
| `success-applied-open.response.json` | success (v12 reopen) | same parser; fixture `gh_shim.rs:5846-5849` | prints `open`; exit 0 |
| `state-applied-comment-failed.response.json` | partial: state changed, comment failed | parser `gh_shim.rs:4168-4170` + `render_state_applied_comment_failed` `4228-4257` (`comment_error.code` string or number, `comment_error.detail` string); fixture `gh_shim.rs:5809-5817` | prints `APPLIED <state>`, the reason, and `comment_error: <code>: <detail>` on stdout; exit 1 |
| `upstream-error.response.json` | upstream GitHub error relayed inside a result | `upstream_error_body` `gh_shim.rs:4177-4198` (a `status`/`status_code` outside 200-299 on the response or inside `result`; body taken from `error`, then `body`, else the whole `result`); fixture `gh_shim.rs:6830-6837` | prints the error body on stderr; exit 1 |
| `refusal-identity_mismatch.response.json` | holder refusal | parser `gh_shim.rs:4155-4164` (`refusal_code` must be a string; any string is accepted); fixture `crates/aft/tests/fixtures/gh_shim/holder-responses-v1.json:13-19` | `gh_shim_seam_refusal`; exit 86 |
| `refusal-custody_unreachable.response.json` | holder refusal | same parser. The code `custody_unreachable` is the one real value observed: it is the `last_seam_refusal.code` recorded in this machine's live `seam-state.json` (at unix time 1790181348). The surrounding bytes are constructed. | `gh_shim_seam_refusal`; exit 86 |
| `refusal-issue_edit_not_own.response.json` | holder refusal: the issue named by an own-issue `issue edit` was not opened by the calling seat's bot | same parser. The code and bytes follow prefrontal's `gh_route.rs` (`REFUSAL_ISSUE_EDIT_NOT_OWN = "issue_edit_not_own"` at line 52, returned through `refusal()` at line 1251, which builds `{"outcome","refusal_code"}`; prefrontal commit `1d8066e9e`). Read from source, not captured live. | `gh_shim_seam_refusal`; exit 86 |
| `unbound-identity.response.json` | holder says the route's identity is unbound | parser `gh_shim.rs:4166` (no other field read) | `gh_shim_unbound_identity`; exit 86 |
| *(no file)* | outcome unknown | the holder reads the request and never replies; `gh_shim.rs:3872-3881` (request error after the write) and `3936-3951` (5 s overall timeout while the stage is `request`), call timeout set at `3810` | `gh_shim_outcome_unknown`; exit 87 |

## Refusal codes

The shim does not keep a list of holder refusal codes: any string in
`refusal_code` is accepted and echoed (`gh_shim.rs:4155-4164`, `588-590`).
The codes named in the existing fixture
`crates/aft/tests/fixtures/gh_shim/holder-responses-v1.json:4-10` are
`identity_mismatch`, `unmapped_operation`, `custody_unavailable`,
`schema_unsupported`, `rate_limited`; `custody_unreachable` is the one seen
live. All of them take the same path, so one file per code would be the same
bytes with a different string. Only `identity_mismatch`, `custody_unreachable`
and `issue_edit_not_own` are exercised.

## Shapes the shim rejects

Rejected responses become `gh_shim_seam_schema_mismatch` (exit 86); they are
listed so a facade knows what not to send:

- not JSON, or not a JSON object (`gh_shim.rs:4118-4125`);
- `outcome` missing or not one of `result`, `refusal`, `unbound_identity`,
  `applied`, `state_applied_comment_failed` (`4171-4173`);
- `result` without `gh_route_schema`, with `gh_route_schema` above 1, without
  `result`, or (for a 2xx result) without `field_order` (`4128-4152`);
- a `field_order` that names an absent field, repeats one, or does not cover
  every field of `result`, or a `result` that is not an object (`4267-4295`);
- `refusal` without a string `refusal_code` (`4156-4163`);
- `applied` / `state_applied_comment_failed` without a string `state`
  (`4203-4209`), or a partial without `comment_error.code` / `.detail`
  (`4232-4245`).
