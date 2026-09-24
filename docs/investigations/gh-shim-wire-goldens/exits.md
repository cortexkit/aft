# How a `gh.route` response becomes an exit code and stderr

Line numbers refer to `crates/aft/src/gh_shim.rs` at the commit that added
this directory. Each row's "captured" column names an `exchanges/` file where
the real shim binary produced exactly that exit code, stdout and stderr
against the named response.

Exit constants: `REFUSAL_EXIT_STATUS = 86` (`gh_shim.rs:38`),
`OUTCOME_UNKNOWN_EXIT_STATUS = 87` (`:39`), `UPSTREAM_FAILURE_EXIT_STATUS = 1`
(`:40`).

Every shim refusal goes through `refuse` (`gh_shim.rs:4840-4847`): it writes
one line `gh-shim: <code>: <text>\n` to stderr (newlines inside the text are
replaced with spaces, `:4841`) and returns 87 for `gh_shim_outcome_unknown`,
86 for every other code (`:4843-4846`). Nothing is written to stdout.

The response is parsed by `parse_governed_response` (`gh_shim.rs:4117-4175`)
into a `RouteOutcome`, and `governed_outcome_status` (`gh_shim.rs:545-586`)
turns the outcome into the exit code.

| Holder behaviour | Outcome | Exit | stdout | stderr | Code | Captured |
| --- | --- | --- | --- | --- | --- | --- |
| `outcome:"result"`, 2xx or no status | `Result` | 0 | `field: value` lines in `field_order` | empty | `:552-555` | `v1-issue-comment-success`, every `*-success` except v12 |
| `outcome:"applied"` | `Result` | 0 | `state`, then `state_reason` if present | empty | `:4167`, `:552-555` | `v12-*-success` |
| `outcome:"state_applied_comment_failed"` | `StateAppliedCommentFailed` | 1 | `APPLIED <state>`, reason, `comment_error: <code>: <detail>` | empty | `:556-559` | `v12-issue-close-state-applied-comment-failed` |
| `outcome:"result"` with non-2xx `status` | `UpstreamError` | 1 | empty | the error body, then `\n` | `:560-563` | `v1-issue-comment-upstream-error` |
| `outcome:"refusal"`, string `refusal_code` | `Refusal` | 86 | empty | `gh-shim: gh_shim_seam_refusal: governance seam refused the action: <refusal_code>` | `:564`, text at `:588-590` | `v1-issue-comment-refusal`, `v10-issue-comment-edit-last-refusal`, `v12-issue-close-refusal`, `v14-issue-create-refusal`, `v14-api-patch-issue-comment-refusal-custody_unreachable` |
| `outcome:"unbound_identity"` | `UnboundIdentity` | 86 | empty | `gh-shim: gh_shim_unbound_identity: the project binding was unavailable at route time` | `:565-568` | `v1-issue-comment-unbound-identity` |
| request written, no reply within 5000 ms | `OutcomeUnknown` | 87 | empty | `gh-shim: gh_shim_outcome_unknown: the governed request was sent but no reply arrived within 5000 ms — it may have executed; check before retrying (for comments: gh api repos/<owner>/<repo>/issues/<n>/comments --jq '.[-1]')` | `:581-583`, `:3996-4020`, text `:3990-3994` | `*-outcome-unknown` (one per family) |
| any malformed or unknown response (see `responses/README.md`) | `SchemaMismatch` | 86 | empty | `gh-shim: gh_shim_seam_schema_mismatch: <reason>` | `:569` | not captured |

A refusal also records `last_seam_refusal` in the shim's `seam-state.json`:
the holder's own code for `outcome:"refusal"` (`gh_shim.rs:3884-3901`), and
`gh_shim_outcome_unknown` for the silent holder (`:4002-4009`).

## When the request never reaches the holder

These are exit 86 without any `gh.route` bytes on the wire, so they are not
exchanges; they are listed so the table above is not mistaken for the whole
set of exits.

- Route cannot be reached (connect, `catalog.list`, no `gh.route` advertiser)
  before the request is written: `gh_shim_governance_unavailable`
  (`:570-580`). If the 5 s budget ran out at a stage other than `connect`,
  the text names the stage and says the command was not run (`:154-158`).
- `route.open` fails: `gh_shim_unbound_identity` (`:3851`, `:565-568`).
- Argv refused while being read, before routing: `gh_shim_missing_reason`
  (`issue close` without `--reason`), `gh_shim_destructive_flag`
  (`--delete-branch`), `gh_shim_unsupported_flag` (`issue create` with
  `--assignee` and similar), `gh_shim_unclassified` (`:3072-3329`,
  `:3356-3469`, surfaced by `:541-543`).
- Local self-report write failure: `gh_shim_seam_unavailable` (`:584`).

## When the outcome is unknown versus not run

The distinction the 87 exit carries: the shim moves to stage `request`
(`gh_shim.rs:3867`) only after it has serialized the body and immediately
before writing it. A failure or the 5 s timeout after that point is
`gh_shim_outcome_unknown` (`:3872-3881`, `:3944-3945`); before it, the same
timeout is `gh_shim_governance_unavailable`, exit 86, "the command was not run"
(`:3946-3950`).
