# gh routing shim: declared rows

The shim classifies each `gh` invocation against the signed
`gh-routing-manifest` and routes it by authority class. A row is only live when
BOTH the signed manifest declares it AND this build's classifier allowlist
admits it at that manifest version, so a signed artifact alone cannot widen what
the shim will speak.

## Authority classes

| Class | Tier | Identity the call runs under | Behavior |
| --- | --- | --- | --- |
| MECHANICAL | `mechanical` | none (upstream `gh`) | replaced by upstream `gh`; the shim holds no token |
| SPEECH-AS-BOT | `governed` | the seat's bot App | canonicalized into a structured request and routed to the holder |
| ADMINISTRATION | `admin` | the operator | refused unless `GH_SHIM_BYPASS=operator`, which records an operator-attributed audit line |

Anything not named below is `gh_shim_unclassified` (exit 86, nothing sent). The
refusal names the verb the classifier decided on, because the decision is made
on the verb alone.

## Rows

| Row | Class | Since |
| --- | --- | --- |
| `issue view`, `issue list`, `pr view`, `pr list`, `pr diff`, `pr checks`, `run view`, `run list`, `repo clone`, `repo view` | MECHANICAL | v1 |
| `run watch`, `workflow view`, `workflow list` | MECHANICAL | v1 (classifier read-only set) |
| `api` GET `**` (field-free) | MECHANICAL | v1 |
| `issue comment`, `pr comment`, `pr review`, `issue reaction` | SPEECH-AS-BOT | v1 |
| `issue close`, `issue reopen`, `pr close`, `pr reopen` | SPEECH-AS-BOT | v12 |
| **`issue create`** | **SPEECH-AS-BOT** | **v14** |
| **`api` PATCH `/repos/*/*/issues/comments/*`** | **SPEECH-AS-BOT** | **v14** |
| `pr merge`, `release create` | ADMINISTRATION | v1 |
| `repo edit`, `run delete` | ADMINISTRATION | v9 |
| `workflow run`, `run rerun` | ADMINISTRATION | v10 |
| `release edit`, `release upload` | ADMINISTRATION | v13 |
| `api` PUT and DELETE `/repos/*/*/branches/*/protection` | ADMINISTRATION | v13 |
| **`issue edit`, label flags only, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| **`pr edit`, label flags only, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| **`label create`, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| `release delete`, `release delete-asset`, and any `release` verb with a `--delete-*` flag | refused as destructive | — |

Every row is declared for `macos` and `linux`: the schema requires a non-empty
`platform` list on tuples and API rules alike, so no row is OS-neutral.

## The v14 rows

Both are the same authority class as `issue comment` and `issue close`: public
speech under the bot identity that creates no authority and lands no code.

### `issue create`

Declared with the `fields-only` argv form, because a create names no target that
exists yet — the issue has no number until it exists, so the declaration carries
body fields and an empty target.

Admitted: `--title`, `--body`, `--body-file` (including the stdin spelling `-`),
`--label` once per label, and `--repo`. Repeated labels are collected into
GitHub's plural `labels` field.

Refused with `gh_shim_unsupported_flag` (exit 86, nothing sent, refused while
argv is read): `--assignee`, `--milestone`, `--project`, `--web`, `--template`,
`--recover`. Assignment, milestones and projects hand out work rather than
speak; the rest need an interactive terminal the governed seam cannot reproduce.
Short spellings of the refused flags are not admitted either — they refuse as
`gh_shim_unclassified`, also without sending anything.

A missing `--title` is upstream's error to report, not the shim's: `gh issue
create` already fails with its own text, and relaying that is more useful than a
second refusal invented here.

### `api` PATCH `/repos/*/*/issues/comments/*`

The id-addressed comment edit behind `edit(issue://N/comments/K)`, which refused
before v14 because no PATCH rule existed.

The endpoint carries the target (repository and comment id) and the row is
body-only: the shim parses the payload itself — a JSON object behind `--input`,
including the stdin spelling `-`, or a single `body` field — and forwards only
the field it recognized, rather than handing the holder bytes it never read. A
payload carrying anything besides `body` is not the declared request. Any other
flag is refused, because a governed route never runs upstream `gh` and silently
dropping a flag would change what the caller asked for.

Ownership is the route holder's check: the comment's author must be the calling
seat's bot. PATCH on any other path — including `/repos/*/*/issues/*`, the issue
itself — is not admitted by this row, and every other PATCH stays as v13 has it.

## Operator label rows (v14)

Maintainers running the shared design gate put `design-approved` on issues and
`trivial` on pull requests, and create those labels where a repository lacks
them. Labels are repository administration, not bot speech, so these rows run
under the operator's own `gh` with `GH_SHIM_BYPASS=operator`, like `pr merge`.
Each row is live only when the signed manifest declares its tuple at v14 or
later: `issue edit` in the governed tier, `pr edit` and `label create` in the
admin tier. Under the deployed v13 manifest none of them exists and the argv
stays `gh_shim_unclassified`.

Upstream `gh` runs the whole argv, so every argument outside a row refuses by
name (exit 86, nothing sent, no audit line), even beside an admitted flag: a
title, body or reviewer change riding along with a label would otherwise run
under the operator's identity without being recorded. The refusal names the
flag without echoing its value. The audit line is appended and synced before
upstream `gh` is spawned, so an attempt that dies mid-call is still on record.

### `issue edit` and `pr edit`, labels only

Admitted, and nothing else: `--add-label` and `--remove-label` (`--flag value`
or `--flag=value`, comma-separated labels, repeatable), `--repo`/`-R`, and
exactly one positional — an issue number or `https://github.com/<o>/<r>/issues/<n>`
URL for `issue edit`, a pull request number or
`https://github.com/<o>/<r>/pull/<n>` URL for `pr edit`. At least one label flag
is required. A branch name is not admitted for `pr edit`: the audit line records
the number that changed. `pr edit` works on any pull request, not only the
bot's own.

Audit line: `{as_of_unix_secs, tuple, repository, issue_number, labels_added,
labels_removed}` for `issue edit`, and the same with `pr_number` in place of
`issue_number` for `pr edit`.

Without the bypass `issue edit` stays on the governed own-issue route, and
`pr edit` has no bot-speech route: it refuses as `gh_shim_unclassified`, as it
did before v14.

### `label create`

Admitted, and nothing else (the flags `gh label create --help` lists): one
positional, the label name; `--color`/`-c` and `--description`/`-d` with a value;
`--force`/`-f`; `--repo`/`-R`. A second positional or any other flag refuses.

Audit line: `{as_of_unix_secs, tuple: "label create", repository, label, color}`;
`color` is `null` when none was given and upstream picks one.

Without the bypass it refuses as `gh_shim_unclassified`. `label delete` and
`label edit` stay undeclared.
