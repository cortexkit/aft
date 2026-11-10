# gh routing shim: declared rows

The shim classifies each `gh` invocation against the signed
`gh-routing-manifest` and routes it by authority class. A row is only live when
BOTH the signed manifest declares it AND this build's classifier allowlist
admits it at that manifest version, so a signed artifact alone cannot widen what
the shim will speak.

## Shim status

`gh --status` prints one JSON document from local shim state, including
`cached_manifest` and `last_rung`. The audit fields are `bypass_audit_count`
(the total number of operator-bypass entries) and `bypass_audit_recent`
(the newest 10 entries, or all entries when fewer exist, with the newest last).
The summary streams the JSONL file and retains only those 10 entries in memory.

Use `gh --status --audit` to print the full list in the original `bypass_audit`
field instead of the count and recent entries. Both forms preserve each audit
entry's fields. A missing audit file means no bypasses have been recorded yet,
just like a readable empty file: the default reports count 0 and recent `[]`,
and `--audit` reports `bypass_audit: []`, with no error. If the audit file is
unreadable or corrupt, `bypass_audit_error` describes the failure and the
corresponding audit fields are `null`, not a zero count or an empty list.

## Authority classes

| Class | Tier | Identity the call runs under | Behavior |
| --- | --- | --- | --- |
| MECHANICAL | `mechanical` | none (upstream `gh`) | replaced by upstream `gh`; the shim holds no token |
| SPEECH-AS-BOT | `governed` | the seat's bot App | canonicalized into a structured request and routed to the holder |
| ADMINISTRATION | `admin` | the operator | refused unless `GH_SHIM_BYPASS=operator` and no bot credentials are detected; accepted bypasses record an operator-attributed audit line |

Anything not named below is `gh_shim_unclassified` (exit 86, nothing sent). The
refusal names the verb the classifier decided on, because the decision is made
on the verb alone.

## Operator bypass

`GH_SHIM_BYPASS=operator` is asserted by the caller, not an identity check.
Before any audited bypass, the shim checks the same ambient bot-credential
inventory used to admit governed routing: the signed manifest's credential
variables (including `GH_TOKEN`, `GITHUB_TOKEN` and `GH_ENTERPRISE_TOKEN`), wrapper configuration
directories, and a `GH_CONFIG_DIR` matching a declared bot directory pattern.
An ordinary operator `GH_CONFIG_DIR` outside those patterns is unchanged.

If bot credentials are detected, the shim refuses with
`gh_shim_bypass_bot_credentials` (exit 86): nothing is sent, upstream `gh` is
not run, and no operator-bypass audit row is written. For example:

```text
gh-shim: gh_shim_bypass_bot_credentials: GH_TOKEN is present; operator commands must run from a shell without bot credentials
gh-shim: gh_shim_bypass_bot_credentials: GH_CONFIG_DIR points to bot credentials; operator commands must run from a shell without bot credentials
```

This applies to every administration row and the audited unbound-write,
credential and label-only bypasses below. Workers still cannot write to GitHub,
even with the bypass set.

## Commands on repositories no bot is bound to

A command aimed at a repository the signed manifest binds no bot to cannot be
bot speech, and upstream `gh` would run it under the operator's own login.
Unless it is known to be safe (below), such a command is refused with
`gh_shim_unbound_target` (exit 86, nothing sent) unless
`GH_SHIM_BYPASS=operator` is set. For example:

```text
gh-shim: gh_shim_unbound_target: `issue comment` targets earendil-works/pi, which is not a bot-bound repository (the signed gh routing manifest binds no bot to it); bot speech is not possible there, and upstream gh would run it under the operator's own login. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line.
```

With the bypass the command runs under the operator's own `gh`. Before starting
it, the shim appends and syncs an audit line in the same format the bypass
writes for `pr merge` and `release create`: `{as_of_unix_secs, tuple,
repository}`, where `tuple` is the verb (`repo create`) or
`api:<METHOD>:<endpoint>` and `repository` is `null` when none is known.

**What passes through.** The check is a safe list, not a list of writes: a
verb or subcommand the shim does not recognise (`codespace create`, or one a
future `gh` adds, even under a listed verb such as `config`) is refused like
any other write. Every entry is an exact subcommand. Only these pass through
unapproved:

- reads: `issue`/`pr`/`release`/`repo`/`run`/`workflow`/`label`/`cache`
  views, lists and downloads, `search` `issues`/`prs`/`repos`/`code`/`commits`,
  `status`, `org list`, `gist` `list`/`view`, `secret list`, `variable`
  `list`/`get`, `ruleset` `list`/`view`/`check`, `project`
  `list`/`view`/`field-list`/`item-list`, `ssh-key list`, `gpg-key list`,
  `codespace list`, `release verify`/`verify-asset`, `attestation verify`,
  `extension list`/`search`, and the `list`/`view` actions of
  `repo deploy-key` and `repo autolink`;
- commands that act only on the local machine: `auth status` (without a token
  flag), `config` `get`/`set`/`list`/`clear-cache`, `alias`
  `list`/`set`/`delete`/`import` (managing aliases), `completion` for bash,
  zsh, fish or powershell, `help` alone or for a gh command or help topic,
  `version`, `repo clone`, `gist clone`, `pr checkout`, and
  `browse --no-browser` with at most one location argument;
- anything with `--help`, and `gh` with no verb;
- `gh api` reads: GET or HEAD (a method named with `--method`/`-X`, else POST
  once a field or `--input` is given, as upstream `gh` decides), and a
  `gh api graphql` call whose inline `query` holds no `mutation`.

A flag before the verb (`gh --something search issues`) is one the shim does
not model, so it cannot tell which verb upstream `gh` would run; the command
is refused. A value flag the shim reads as a subcommand (`gh status -o org`)
is refused the same way.

Running an extension (`gh extension exec`, or `gh <extension>`) or an alias
(`gh <alias>`) is not on the list: an extension is a program and an alias can
expand to `api --method POST` or a shell command, so either can write with the
operator's token where the shim cannot see it.

## The operator's gh credentials

`gh auth token` and `gh auth status --show-token` (or `-t`) print the
operator's GitHub token into the agent's session, and with the token an agent
could call the GitHub API directly, around the shim. `gh auth login`,
`logout`, `refresh`, `switch` and `setup-git`, and any other `auth`
subcommand, change the operator's gh login or git's credential configuration.
With a verified manifest all of these are refused with
`gh_shim_operator_credentials` (exit 86, nothing sent) wherever they run, in a
bound checkout or not, unless `GH_SHIM_BYPASS=operator` is set:

```text
gh-shim: gh_shim_operator_credentials: `auth token` prints the operator's GitHub token into this agent's session, and with it an agent could call the GitHub API directly, around the shim. The operator can approve it: re-run with GH_SHIM_BYPASS=operator, and the shim records an operator-attributed audit line.
```

With the bypass the command runs after an audit line
`{as_of_unix_secs, tuple: "auth token", repository: null}`.

**`gh auth status`.** Without a token flag, and while a signed routing
manifest is installed, the shim answers `gh auth status` itself, from local
state only: it runs neither the real `gh` nor a governance probe. The answer
names the repository resolved from the working directory (or `GH_REPO`), the
bot identity governed writes use there (or that the repository is unbound, so
writes are refused unless the operator bypass applies), the manifest version
and whether it verified, and whether governed routing is ready, from the last
recorded rung. It exits 0 when governed writes work in that repository and 1
otherwise, with one `Governed writes unavailable: <reason>` line per reason,
mirroring upstream `gh`'s not-logged-in status. Only `--hostname github.com`
(`-h`) is accepted; other flags are refused by name. With no manifest
installed, or with `github.shim` turned off, `gh auth status` passes through to
the real `gh` unchanged and reports the operator's own login.

**What the target is.** The same resolver the governed rows use: `--repo`/`-R`,
a thread URL, a `gh repo` subcommand's repository positional (`gh repo delete
owner/name`), a `/repos/<owner>/<name>` endpoint, `GH_REPO`, then the working
directory's `origin`. When that target is bound, the governed path applies
unchanged, and the bypass there still reaches only manifest-declared
administration. A command that is not bot speech, run from inside a bound
checkout, also stays on the governed path whatever it names, as before.

**Account writes** never have a binding, so they need the bypass even from a
bound checkout: `repo create` (a new repository cannot be bound yet),
`repo fork`, `gist` writes, `ssh-key`/`gpg-key` add and delete, `project`
writes, and `gh api` writes to an endpoint outside `/repos/<owner>/<name>`
(such as `POST /user/repos` or a GraphQL mutation).

**No determinable target.** A command not on the safe list whose target cannot
be resolved (nothing named and no github.com `origin`, or a `--repo` that is
not a github.com `owner/name`) is refused the same way, and the refusal says
why.

The check needs a verified signed manifest to compare against. Without one (no
manifest installed, or one that never verified) nothing is bound and the shim
passes commands through as before. An installed manifest that fails validation
after an earlier one verified (a regressed manifest) already refuses every
command that is not a read. Destructive forms keep their own refusal, and
`github.shim: false` turns the whole shim off.

The operator's own terminal is not affected. The shim runs only where AFT puts
its `shims` directory first on `PATH`, which it does for the bash and PTY
children it spawns for agents; it never edits shell startup files or the
terminal's `PATH`, so a `gh` the operator types reaches upstream `gh` directly.

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
| **`pr create`** (same-repository head, `--base` required) | **SPEECH-AS-BOT** | **v16** |
| `pr ready` (including `--undo`) | SPEECH-AS-BOT | v19 |
| `pr merge`, `release create` | ADMINISTRATION | v1 |
| `repo edit`, `run delete` | ADMINISTRATION | v9 |
| `workflow run`, `run rerun` | ADMINISTRATION | v10 |
| `run cancel` (including `--force`) | ADMINISTRATION-AS-OPERATOR | v15 |
| `repo rename`, `repo create` | ADMINISTRATION-AS-OPERATOR | v19 |
| `workflow disable`, `workflow enable` | ADMINISTRATION-AS-OPERATOR | v19 |
| `api` PUT and DELETE `/repos/*/*/private-vulnerability-reporting` | ADMINISTRATION-AS-OPERATOR | v19 |
| `release edit`, `release upload` | ADMINISTRATION | v13 |
| `api` PUT and DELETE `/repos/*/*/branches/*/protection` | ADMINISTRATION | v13 |
| **`issue edit`, label flags only, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| **`pr edit`, label flags only, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| **`label create`, under `GH_SHIM_BYPASS=operator`** | **ADMINISTRATION (operator label row)** | **v14** |
| `release delete`, `release delete-asset`, and any `release` verb with a `--delete-*` flag | refused as destructive | — |

`run cancel` uses the same operator gate as `run rerun`: without
`GH_SHIM_BYPASS=operator` it returns `gh_shim_admin_tier`; with the bypass
it appends and syncs the audit before upstream execution. Hung CI runs can
otherwise hold a seat until GitHub's timeout. `--force` is allowed because it
only changes the single selected run's cancellation endpoint to `force-cancel`,
not the scope of the operation (checked against gh 2.93.0 help and source).
Neither rerun nor cancel belongs in the unbound safe-read list: the existing
unbound-write refusal and audited bypass apply unchanged. The signed manifest
must declare cancellation at v15 or later on the bound-repository path; older
manifests retain their existing behavior. Code availability is not activation:
the credential-custody owner (CKCRED) signs the exact unsigned payload bytes for
an envelope-version-2 manifest, and the Subconscious daemon owner (SUBC)
coordinates placement. Production payloads and signing handoffs stay
outside git in `.alfonso/ceremonies/gh-routing-manifest-v15/`, never in public
docs or test fixtures. Prepare the payload from the trusted production v13
envelope with `python3 scripts/prepare-gh-shim-v15.py /path/to/v13-envelope.json`;
the script prints its SHA-256 and defaults to that private ceremony directory.
An optional second argument selects another output path. Verify the printed hash
before signing, and do not re-serialize the payload after signing.

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

## The v16 row: `pr create`

Bots may open pull requests. Opening one proposes a change under the bot
identity and lands nothing, so it is speech, the same class as `issue create`;
merging stays operator-only on the admin `pr merge` row. The manifest must
declare `pr create` at v16 or later: under v15 and earlier it refuses as
`gh_shim_unclassified` (undeclared), even if an older manifest carries the row.

Declared `fields-only` (a pull request has no number until it exists) with the
body fields `title`, `body`, `base`, `head`, `draft`, in that order. The shim
reads only that exact declaration; a signed row naming other fields refuses.
The request travels in the same envelope as `issue create`:
`{"action":"pr create","target":{},"body":{"title","body","base","head","draft"}}`
plus the repository, which Plexus checks against the bot's binding.

Admitted: `--title`/`-t`, `--body`/`-b`, `--body-file`/`-F` (read by the shim,
including the stdin spelling `-`, and sent as text), `--base`/`-B`,
`--head`/`-H`, `--draft`/`-d` (sent as a boolean, `false` when absent), and
`--repo`/`-R`. Each value flag may appear once.

`--base`, `--head` and `--title` are required on the governed path and refuse
as `gh_shim_unclassified` when missing or empty. Upstream `gh` would default the
base to the repository's default branch and the head to the local branch, or
prompt; the shim can look up neither and does not guess.

The head must be a branch of the target repository. A cross-repository head
(`owner:branch`) refuses as `gh_shim_unsupported_flag` before anything is sent.
Whether the head branch exists is GitHub's check: its refusal (for example a
422 for an invalid `head`) comes back from Plexus as a `gh_shim_seam_refusal`
carrying Plexus's code verbatim, and is not retried or handed to upstream `gh`.

Refused with `gh_shim_unsupported_flag` (exit 86, nothing sent): `--assignee`,
`--reviewer`, `--label`, `--milestone`, `--project` (and their short forms `-a`,
`-r`, `-l`, `-m`, `-p`), `--fill`/`-f`, `--fill-first`, `--fill-verbose`,
`--web`/`-w`, `--editor`/`-e`, `--template`/`-T`, `--recover`, `--dry-run`, and
`--no-maintainer-edit`. Any other flag or a positional argument refuses as
`gh_shim_unclassified`. On success the shim prints the new pull request's URL,
as `gh pr create` does.

Prepare the unsigned payload from the unsigned v15 bytes with
`python3 scripts/prepare-gh-shim-v16.py /path/to/unsigned-v15.json`; it writes
to `.alfonso/ceremonies/gh-routing-manifest-v16/` by default and prints the
SHA-256. It adds the one row and its canonicalization and keeps every other
byte of v15.

## The v19 rows

`pr ready` changes a pull request's public state as the calling session's bot,
like `pr close` and `pr reopen`; it never uses the operator's credentials on the
bound-repository path, even with the bypass set. It requires a bot-bound target
and a session that delegates its bot identity. `--undo` marks the pull request as
a draft again. The governed form takes one pull request positional and optional
`--repo`/`-R`, plus the boolean `--undo`; other flags are refused. As with the
other thread-state verbs, a missing positional is refused rather than guessing
from the current branch.

The canonicalization is `argv_forms: ["target-and-state"]`,
`target_fields: ["number"]`, `body_fields: ["undo"]`. The daemon receives the
thread-state wire shape with `verb: "pr ready"`, `repository`, `number`, and
`undo: false` by default or `undo: true` for `--undo`, alongside the usual
schema, manifest-version, rung and metadata fields. The relay preserves the
request and the caller's scoped identity; the GitHub route holder must support
this verb and boolean before activation.

`repo rename`, `repo create`, and the PUT/DELETE private vulnerability reporting
setting run only as administration under `GH_SHIM_BYPASS=operator`, with the audit
synced before upstream execution. A new repository cannot already be bot-bound:
`repo create` therefore takes the existing account-write refusal and audited
operator bypass, including from a bound checkout. There is no unapproved create
exception. These commands do not enter the bot-speech relay.

Before an operator-bypassed `repo rename`, the shim checks the existing target
repository using the real `gh` and the operator's login: `-R`/`--repo` wins,
otherwise `gh` selects the current repository (including `GH_REPO`). It reads
one recursive tree of the default branch and only fetches workflow blobs from
that tree. Any `action.yml` or `action.yaml` anywhere, or a direct
`.github/workflows/*.yml`/`*.yaml` file whose parsed YAML `on` includes
`workflow_call`, refuses the rename with exit 86 and `gh_shim_rename_published`.
The refusal names up to five paths and the total count. GitHub does not redirect
other repositories' `uses: owner/repo[/path]@ref` or reusable-workflow references
after a rename, so those workflows would silently break.

The check fails closed with exit 86 and `gh_shim_rename_unverified` on API errors,
an incomplete/truncated tree, a missing default branch, invalid or uninspectable
workflow YAML (including trigger aliases, anchors or merged mappings), or a
limit: 15 seconds for the whole check, 128 workflow files, 16 MiB for tree output
and 1 MiB per workflow. The operator can explicitly accept the risk with
`GH_SHIM_RENAME_PUBLISHED=allow` alongside `GH_SHIM_BYPASS=operator`; only the exact
value `allow` enables the override, which still runs this bounded inspection but
never refuses the rename based on its result. The synced `operator-bypass.jsonl`
line records `rename_published_override: "allow"`
and the full sorted `published_paths` list, including an empty array when the
inspection succeeds without finding any. If inspection fails or times out, it
records `published_paths: null` and the reason in `published_paths_unverified`.
This guard does not change other administration verbs or the v19 manifest.

`workflow disable` and `workflow enable` take the same operator gate, for the
same reason `run cancel` does: a workflow that keeps refusing every attempt
otherwise piles up failed runs with no way to stop it short of a token only the
operator holds. Switching one off is repository administration, not speech, so
neither verb enters the bot-speech relay and both record the usual
operator-attributed audit line before upstream `gh` runs. The real flags are the
workflow name or id positional and `--repo`/`-R`; the shim models no flags for
these rows, exactly as for `run cancel`, so a flag these verbs do not have is
forwarded unchanged and refused by upstream `gh`'s own argument parser before it
makes any request. `workflow run` keeps its v10 administration row and
`workflow view`/`workflow list` stay mechanical reads.

Older manifests refuse both verbs as `gh_shim_unclassified`, whatever the
bypass says: the declaration and the code-side review are each version-gated at
v19.

The API setting matches exactly `/repos/<owner>/<repo>/private-vulnerability-reporting`
(with or without the leading slash in argv). Neither nested or neighbouring
paths nor POST/PATCH inherit its allowance. Field-free GET remains a mechanical
read. Both the tuple declarations and these API write rules require manifest
version 19 or later; adding code does not activate them.

The exact additive JSON fields are recorded in the synthetic
`crates/aft/tests/fixtures/gh_shim/v19-additions.json` fixture: append the rows to
the corresponding arrays of the trusted unsigned v18 payload, add the
`canonicalization["pr ready"]` entry, and set `manifest_version` to `19`.
Preserve all existing declarations and bindings. The fixture is an additions
fragment, not a complete production manifest and not signing material. CKCRED
signs the exact UTF-8 bytes of the complete reviewed unsigned payload; envelope
version 2 carries the complete payload verbatim as a JSON string in
`manifest_bytes` and the base64-encoded Ed25519 signature in `signature`. Do not
re-serialize after signing. No signed envelope, production identity, or activation
artifact is part of this change.

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

## Worker broker security boundary and GET query contract

Worker reads use a command-scoped bearer at a dedicated Unix socket. This
boundary separates **independently jailed commands**; it does not isolate an
unsandboxed process owned by the same user. Such a process can read a mode-0600
capability file and use its matching endpoint. The host and other unsandboxed
same-user processes are trusted. Processes within an authorized command may
intentionally delegate its read capability until command revocation. Neither a
particular `gh` executable nor process ancestry is authenticated by the bearer.

The daemon derives the command identity, live scope and repository binding; it
admits only session-context lookup and the typed governed read operation. The
worker's signed manifest snapshot is not daemon authority, and the daemon checks
its own active v20-or-later manifest before serving reads.

Only the worker broker's `github.repo.get` query map has the following closed
key set. Existing operator/head execution paths retain their current behavior.

- Pagination: `page`, `per_page`.
- Native content/run reads: `ref`, `branch`, `head_sha`, `event`, `actor`, `status`,
  `check_suite_id`.
- Repository REST filters: `state`, `labels`, `milestone`, `assignee`, `creator`,
  `mentioned`, `since`, `sort`, `direction`, `base`, `head`, `path`, `sha`.

At most 16 pairs are permitted; each decoded value is at most 4096 UTF-8 bytes
and must contain no control characters. Unknown keys, including `q`, `query`,
`url`, `method`, and header-like keys, are refused before upstream work with a
typed refusal naming the key and listing the allowed keys. Dedicated search
requests still accept an unqualified search term; repository/organization/user
qualifiers, boolean operators and parentheses remain forbidden there. A GET
request cannot substitute a GraphQL endpoint.

Revocation invalidates the registry grant and closes the endpoint and its
connections. Production relay operations own cancellation-safe, bounded route
cleanup rather than relying on a canceled client's future to release a lease.
Endpoint ownership records and generation locks live outside worker-writable
scratch; startup reclaims endpoints of dead generations without PID-based
ownership guesses or deleting a live replacement.

On Linux, pathname AF_UNIX connection itself is not restricted by the deployed
Landlock ABI, so capability-file isolation is essential. A separately jailed
command can also consume another command's **pre-authentication** socket slots
without learning its capability. Per-endpoint/global caps and short frame
read deadlines bound the resources, but repeated connections can still starve
reads. This is a known availability limit, not cross-command read authority.

Worker GET paths are decoded exactly once before policy checks. Encoded GraphQL,
malformed/double encoding, dot segments and control characters are refused. A
slash-containing branch, commit or release-tag reference remains encoded as
`%2F` in its designated single endpoint segment; other encoded slashes cannot
change endpoint structure. AFT forwards the canonical checked path, not the
caller's alternate spelling. This worker-only rule does not alter existing
head/operator path behavior.

Query pairs are sent to the upstream provider as typed JSON strings; AFT does not
construct the final GitHub URL and does not attest the downstream URL encoder.
Worker GET values therefore refuse `&`, `=`, `#` and percent escapes as well as
control characters. This prevents a value such as `state=open&x=y` from reaching
that boundary as an extra or truncated parameter. Existing head/operator query
values retain their previous behavior.

### Regression labels and their actual scope

Two test names changed between the earlier broker implementation and its
lifecycle fixes; a third payload name was left stale and is corrected now:

- `gh_shim_broker_revocation_discards_in_flight_reply` became
  `gh_shim_broker_revocation_during_lookup_refuses_before_upstream`: the original
  case revokes during lookup and requires **zero** upstream calls. Its old name
  was reused for a separately added test with one held upstream call and a
  post-revocation discarded result; that is not a third historical rename.
- `worker_gh_read_t10_cancel_midflight_discards_the_upstream_reply` became
  `worker_gh_read_t10_cancel_midflight_terminates_reader_without_output`: it kills
  the reader and observes no output. Independent broker suppression is proved
  by `worker_gh_read_revocation_suppresses_reply_to_surviving_observer` instead.
- `task_store_paths_are_denied_while_held_payload_fd_remains_readable` now becomes
  `task_store_paths_are_denied_while_explicit_stdin_remains_readable`: approved
  payload bytes arrive deliberately through stdin while sibling-store reads
  are denied. It does not claim arbitrary high-descriptor inheritance.
