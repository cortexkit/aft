# Tool Reference

> **All line numbers are 1-based** (matching editor, git, and compiler conventions).
> Line 1 is the first line of the file.

## Response convention

Tool responses follow a tri-state contract so agents can tell "didn't run" from "ran clean"
from "ran but partial":

- **`success: false`** — the work could not be performed. Always carries a `code` (e.g. `path_not_found`,
  `no_lsp_server`, `project_too_large`, `invalid_request`, `ambiguous_match`) and a `message`.
- **`success: true` with `complete: true`** — the result is trustworthy. Absence of items in the
  result means the tool genuinely found nothing.
- **`success: true` with `complete: false`** — the tool ran but the result is partial. The
  response will name the gap with one or more of:
  - `pending_files`, `unchecked_files`, `walk_truncated` — files the tool didn't get to
  - `skipped_files: [{file, reason}]` — files intentionally skipped (parse error, unsupported language)
  - `scope_warnings`, `no_files_matched_scope` — paths/globs that resolved to zero files
- **Side-effect skips** — when the main work succeeded but a non-essential post-step was
  skipped, the response carries a `<step>_skipped_reason`. Approved values:
  - `format_skipped_reason`: `unsupported_language` | `no_formatter_configured` | `formatter_not_installed` | `formatter_excluded_path` | `timeout` | `error`
  - `validate_skipped_reason`: `unsupported_language` | `no_checker_configured` | `checker_not_installed` | `timeout` | `error`

### When an index is off or still building

Tools that read a background index (`indexes.trigram`, `indexes.semantic`, `indexes.callgraph`)
say so instead of returning an empty result:

- `grep` and `glob` scan the filesystem while the trigram index is not ready and mark the
  answer `fallback: "filesystem"`.
- `aft_search` uses whichever lanes are ready and names the ones it left out. With no lane
  ready it refuses with `no_search_lanes_enabled` (both indexes configured off) or
  `search_lanes_unavailable`, with each lane's status and cause. Use `grep` meanwhile.
- `aft_callgraph` refuses with `callgraph_off`, `callgraph_building` or `callgraph_unavailable`.
- `aft_inspect` reports dead code as unavailable rather than zero findings.
- `glob` and `aft_search` leave out indexed paths that no longer exist on disk and count them
  in `missing_on_disk_dropped`.

## Hoisted tools

These replace the host harness's built-ins under the same names. A tool is registered unless
its name is in `disabled_tools`; disabling a host name (for example `"grep"`) leaves the
host's own tool in place. There are no `aft_`-prefixed alternatives. The `bash_status`,
`bash_watch`, `bash_write`, and `bash_kill` companions register independently of `bash`,
but only while `bash.background` is on: they act only on background tasks. Apart from that,
index state and runtime settings never remove a registration: a tool whose index is off or
building reports that when called. The `bash` arguments follow their features the same way
(see [shell-tool-surface.md](shell-tool-surface.md)).

The Pi/OMP adapter has no `apply_patch` or `glob` implementation, so those two tools are not
registered there.

| Tool | Description | Key Params |
|------|-------------|------------|
| `read` | File read, directory listing, image/PDF detection | `path`, `startLine`, `endLine`, `offset`, `limit` |
| `write` | Write file with auto-dirs, backup, format, inline diagnostics | `path`, `content` |
| `edit` | Find/replace, symbol replace, batch, glob | `path`, `oldString`, `newString`, `symbol`, `content`, `edits[]` |
| `apply_patch` | `*** Begin Patch` multi-file patch format | `patchText` |
| `ast_grep_search` | AST pattern search with meta-variables | `pattern`, `lang`, `paths[]`, `globs[]` |
| `ast_grep_replace` | AST pattern replace (applies by default) | `pattern`, `rewrite`, `lang`, `dryRun` |
| `lsp_diagnostics` | Errors/warnings from language server | `path`, `directory`, `severity`, `waitMs` |
| `grep` | Trigram-indexed regex search with compressed output | `pattern`, `path`, `include`, `exclude` |
| `glob` | Indexed file discovery with compressed output | `pattern`, `path` |

## AFT-only tools

Registered unless listed in `disabled_tools`.

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_outline` | Structural outline of a file, directory, files, or URL; or indexed file tree | `target` (string or array), `files` |
| `aft_zoom` | Inspect symbols (same-file or cross-file); opt-in call-graph annotations | `path`, `symbols` (string or array), `targets`, `url`, `callgraph` |
| `aft_import` | Language-aware import add/remove/organize | `op`, `path`, `module`, `names[]` |
| `aft_conflicts` | Show all git merge conflicts with line-numbered regions | `path` (optional) |
| `aft_search` | Hybrid semantic + lexical code search by meaning | `query`, `topK`, `path` |
| `aft_inspect` | Codebase-health snapshot (TODOs, metrics, dead code, unused exports, duplicates) | `sections`, `scope`, `topK` |
| `aft_safety` | Undo, history, checkpoints, restore | `op`, `path`, `name` |

Default-off tools (in the default `disabled_tools`; set an explicit list such as `[]` to
enable them) plus the call graph:

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_delete` | Delete one or more files (or directories) with backup | `files`, `recursive` |
| `aft_move` | Move or rename a file with backup | `path`, `destination` |
| `aft_callgraph` | Call graph and data-flow navigation | `op`, `path`, `symbol`, `depth` |

---

### read

Plain file reading and directory listing. Pass `path` to read a file, or a directory path to
list its entries. Paginate large files with `startLine`/`endLine` or `offset`/`limit`.

With `github.read` enabled, use `pr://N/diff` for a live unified diff or
`pr://N/diff/<path>` for one exact changed path (renames use the new path).
Both accept `pr://OWNER/REPO/N/diff` forms. Each page names the PR head SHA;
line ranges select the diff body, not the repeated header. Binary changes get
one line, and paging or the 4 MiB fetch ceiling is disclosed. Diffs are read-only
views: use `read`, not `aft_outline` or `aft_zoom`. Diff failures never use cached data.

```json
// Read full file
{ "path": "src/app.ts" }

// Read lines 50-100
{ "path": "src/app.ts", "startLine": 50, "endLine": 100 }

// Read 30 lines from line 200
{ "path": "src/app.ts", "offset": 200, "limit": 30 }

// List directory
{ "path": "src/" }
```

Returns line-numbered content (e.g. `1: const x = 1`). Directories return sorted entries with
trailing `/` for subdirectories. Binary files return a size-only message. Image and PDF files
return metadata suitable for UI preview. Output is capped at 50KB.

For symbol inspection with call-graph annotations, use `aft_zoom`.

---

### write

Write the full content of a file. Creates the file (and any missing parent directories) if it
doesn't exist. Backs up any existing content before overwriting.

```json
{ "path": "src/config.ts", "content": "export const TIMEOUT = 10000;\n" }
```

Auto-formats using the project's configured formatter (biome, oxfmt, prettier, etc.).

The write returns as soon as the file is written. Diagnostics surface through the AFT status
bar and `aft_inspect`; set `lsp.diagnostics_on_edit: true` in `aft.jsonc` to additionally wait
for and inline fresh LSP diagnostics on every edit.

For partial edits (find/replace), use `edit` instead.

---

### edit

The main editing tool. Mode is determined by which parameters you pass:

**Find and replace** — pass `path` + `oldString` + `newString`:

```json
{ "path": "src/config.ts", "oldString": "const TIMEOUT = 5000", "newString": "const TIMEOUT = 10000" }
```

Matching uses a 4-pass fuzzy fallback: exact match first, then trailing-whitespace trim, then
both-ends trim, then Unicode normalization. Returns an error if multiple matches exist — use
`occurrence: N` (0-indexed) to pick one, or `replaceAll: true` to replace all.

**Symbol replace** — pass `path` + `symbol` + `content`:

```json
{
  "path": "src/utils.ts",
  "symbol": "formatDate",
  "content": "export function formatDate(d: Date): string {\n  return d.toISOString().split('T')[0];\n}"
}
```

Includes decorators, doc comments, and attributes in the replacement range.

**Batch edits** — pass `path` + `edits` array. Atomic: all edits apply or none do.

```json
{
  "path": "src/constants.ts",
  "edits": [
    { "oldString": "VERSION = '1.0'", "newString": "VERSION = '2.0'" },
    { "startLine": 5, "endLine": 7, "content": "// updated header\n" }
  ]
}
```

Set `content` to `""` to delete lines. Per-edit `occurrence` is supported.

To edit multiple files, make parallel `edit` calls in one response.

**Glob replace** — use a glob as `path` with `replaceAll: true`:

```json
{ "path": "src/**/*.ts", "oldString": "oldName", "newString": "newName", "replaceAll": true }
```

**Append to file** — pass `path` + `appendContent`:

```json
{ "path": "notes.md", "appendContent": "\n## New section\n..." }
```

Creates the file (and parent directories) if missing. Faster than read+write for adding to logs,
notepad files, or large appendable structures.

The edit returns as soon as the write completes. Diagnostics surface through the AFT status bar
and `aft_inspect`; set `lsp.diagnostics_on_edit: true` in `aft.jsonc` to additionally wait for
and inline fresh LSP diagnostics on every edit. Use `aft_safety checkpoint` / `undo` for
recovery before risky edits.

---

### apply_patch

Apply a multi-file patch using the `*** Begin Patch` format. Creates, updates, deletes, and
renames files. Hunks commit per file: successful hunks are kept and failures are reported, so a
partial patch leaves the applied changes in place (use `aft_safety` to revert if you want to
abort). A move hunk never deletes the source unless the destination write succeeds.

```
*** Begin Patch
*** Add File: path/to/new-file.ts
+line 1
+line 2
*** Update File: path/to/existing-file.ts
@@ context anchor line
-old line
+new line
*** Delete File: path/to/obsolete-file.ts
*** End Patch
```

Context anchors (`@@`) use fuzzy matching to handle whitespace and Unicode differences.
Diagnostics surface through the AFT status bar and `aft_inspect` (or inline on every edit with `lsp.diagnostics_on_edit: true`).

With `validate_on_edit: "full"` (or a binary request's `validate: "full"` override),
formatting and syntax validation still happen per write, but type checking waits until
**all** hunks have succeeded. Each distinct checker runs once for the configured project
root against the completed patch. File-scoped checkers receive all touched paths when
their CLI supports it; Go named-file checks across directories fall back to one run per
file. Deleted files are not checker inputs, and repeated writes to a path are checked once.

Checker diagnostics appear on the existing `metadata.files` entries as `validation_errors`
(including an empty array for a clean check) and, when necessary, `validate_skipped_reason`.
Each diagnostic's `file` retains the checker-reported path, resolved against the checker
root for attribution to the correct touched file.
For a move hunk, diagnostics belong to the destination. The rendered `output` also includes
a single summary, for example `type check: 2 errors in 1 of 3 files (cargo check, tsc)`;
unchecked files are explicitly named by count and skip reason in that line. There is no
top-level aggregate diagnostic list. Failed or partially applied patches do **not** run
type checkers, even though their successful writes are kept.

---

### bash

Execute shell commands through AFT's unified bash handler. AFT registers `bash` in the
recommended tool surface; experimental flags gate advanced behavior, not the tool itself.

**Schema:**

| Param | Type | Description |
|---|---|---|
| `command` | string | Shell command to execute |
| `timeout` | number | Hard-kill cap in milliseconds (positive integer). Default 30 minutes when unset; while a delegated (subagent) session keeps waiting on the command, each wait pushes that default to at least `bash.worker_wait_max_ms` after the wait. An explicit `timeout` is never extended. NOT a polling window — see below. |
| `workdir` | string | Working directory for command execution |
| `description` | string | Short human-readable summary for harness UI metadata |
| `background` | boolean | Spawn detached and return a `taskId` (requires the background flag) |
| `compressed` | boolean | Opt in/out of output compression for this call (default true; requires compression flag) |
| `pty` | boolean | Run in a real PTY for interactive programs. Implies `background: true`. |
| `ptyRows` / `ptyCols` | number | PTY dimensions (max 60 rows / 140 cols). Soft-ignored on non-PTY calls. |
| `runon` | string | Run the whole line on the remote Linux build server (`"linux"`). Offered only where remote runs are configured; see below. |

Calls admitted under the `worker` catalog preset set `NEXTEST_TEST_THREADS` and `RUST_TEST_THREADS` for local bash children from `../.cargo/alfonso-test-threads` (one decimal integer plus newline), defaulting to 4 when the file is missing, invalid, or above 256; valid values through 256 are preserved. Existing inherited or per-call values take precedence, and shell command prefixes still override them; head sessions and unscoped plugin worker flags receive no defaults, and exec-remote requests never carry these variables.

**Timeout model:** `timeout` is a hard-kill cap, never a polling parameter, and starts after
process spawn (setup time does not count). Expiry sends SIGTERM to the Unix process group, then
SIGKILL after up to 2 seconds (Windows uses `taskkill /T /F`) and reports exit 124. Unix processes
that leave the group (`setsid`, `setpgid`, Python `start_new_session=True`) survive; macOS has no
tree kill. A bare foreground
`bash({ command })` is polled for a short internal wait window (~5s); if the command hasn't
finished it auto-promotes to a background task and returns a `taskId` while the command keeps
running under the 30-minute (or explicit `timeout`) kill cap. `bash({ timeout: 2000 })` polls
briefly then hard-kills at 2s. `background: true` skips polling entirely.

**Foreground example:**

```json
{ "command": "git status" }
```

Returns combined stdout/stderr plus `exit_code`, `duration_ms`, truncation status, and an
`output_path` when large output spills to disk.

**Running on the remote build server (`runon`)** — `runon: "linux"` sends the whole command
line, exactly as written (pipes, lists, environment prefixes and all), to the remote Linux
build server (ck-motor, reached through the Subconscious daemon's `exec-remote/v1`). It runs
there under `bash -c` in the same working directory, with the same timeout and the same
environment, minus the secret-shaped and AFT/CortexKit control variables AFT strips before any
off-host request (the reply names them). New plans require `runon` to send a line away. Deployed
worker plans with an enabled `commands` prefix list retain their existing automatic routing:
every literal command must match an allowed prefix, and unsupported syntax stays local.
Explicit whole-line `runon` also requires the user-only live safety switch
`bash.runon_enabled: true` (default false); projects cannot enable it. Turning that switch off
hides and refuses `runon` without disabling legacy prefix routing.

When remote runs are available, put `runon: "linux"` on build and test lines (cargo, bun test), including chains and pipes. Keep git, gh, interactive and file-editing commands local, and keep a line local if it needs macOS (Seatbelt, codesign, launchd, TCC, AppKit) or runs binaries built on this machine: a remote build leaves no binaries or target/ output here.

Who is offered `runon`:

- OpenCode and Pi sessions on macOS or Linux in subc mode, when the user config sets
  `remote_exec.enabled: true`, the user safety switch `bash.runon_enabled: true`, and the project
  has not turned remote runs off (see
  [Configuration](config.md#remote-runs)). The decision is made once when the tool is built;
  provider discovery happens at call time, without a startup round trip.
- Head catalogs follow the same user setting. Broca worker catalogs instead use that session's
  persisted plan, and offer `runon` only when its `remote_exec.enabled` and the user safety switch
  are true, including after
  a paramless refetch or restart.

Default schemas and unavailable sessions contain neither the parameter nor remote-build
instructions. Standalone NDJSON sessions and Windows never advertise it, whatever the user
setting says.

A call that sets `runon` is refused by name, and runs nowhere, when it cannot run remotely:
the project turned remote runs off (`remote runs are off for this project`), the session has no
remote runner (`this session has no remote runner`), the demand is not one AFT knows
(`unknown runner demand`), or the call also sets `pty: true`, a PowerShell shell, or
`sandbox: "host"`. On Windows `runon` is refused by name: remote dispatch needs AFT on macOS or
Linux. If no `exec-remote/v1` provider answers, or daemon discovery fails before dispatch,
the task is refused by name and the command is not run locally. `background: true` works as
for local commands, and a background remote task re-attaches after a restart.

When the runner refuses an explicit `runon` job before starting it, the call fails with code
`remote_unavailable`: `runon refused: remote refused: <reason>; command was not run; retry, or
omit runon to run locally`. The refusal reason is preserved, including reasons from newer
runners. This also applies to pending tasks recovered after a restart. No local process is
spawned. A deliberately backgrounded call still returns its task ID; a later refusal marks
that task failed and its status includes `remote_refusal` with the same error text.

Successful remote runs begin with `ran remotely on ck-motor`. Only automatic prefix routing
(without an explicit `runon`) retains local fallback with the advisory
`ran locally on macOS: remote refused: <reason>` (or the local OS name). A job that started
remotely is never automatically resubmitted: if AFT loses track of it, the reply says the
outcome is unknown instead of re-running it. Remote jobs run on a
server-side copy of the workspace with no network access, and no files are synced back. The
reply therefore says `remote outcome unknown (job <id>); the remote job could not affect this
machine or the network, so rerunning is safe; check exec.status <id> first if you need its
result`. If AFT has no job ID, it omits `(job <id>)` and the `check exec.status` advice. This
applies only to remote jobs; local commands, including a remote refusal's local fallback,
keep their existing unknown-outcome warnings because their side effects may have happened.
After the output, the reply prints what the runner reported about the server's workspace, none of which
is copied back: the files the run changed (`These files changed on the server and were NOT
copied back:`), a changed Git state (HEAD before -> after, the symbolic ref or `detached`,
whether the index tree changed, and the stash count change), new untracked files (marked when
the runner listed only some of them), and the number of writes under ignored paths with sample
paths. A kind of change the runner reported as empty is not mentioned. A kind it did not report
(an older runner) gets one line, such as `git state: not reported by the runner`, and is never
shown as "nothing changed".

**Rewriter** — when `experimental.bash.rewrite: true`, common shell command shapes route to AFT
tools instead of spawning bash:

| Pattern | Routes to | Example |
|---|---|---|
| `cat <file>` | `read` | `cat README.md` → `read` |
| `grep [-r] PATTERN <path>` | `grep` | `grep -r TODO src/` → `grep` |
| `find <path> -name '<glob>'` | `glob` | `find src -name '*.ts'` → `glob` |
| `sed -n 'N,Mp' <file>` | `read startLine/endLine` | `sed -n '10,20p' src/x.ts` → `read` |
| `ls [-l] [-R] [<path>]` | `read` directory mode / `glob` | `ls src/` → `read` |
| `rg PATTERN [<path>]` | `grep` | `rg foo` → `grep` |
| `cat >> <file>` / `echo "X" >> <file>` | `edit` append op | `cat >> notes.md <<< 'note'` → `edit appendContent` |

Each rewrite returns the AFT tool's result with a footer hint reminding the agent to call the
direct tool next time.

**Compression** — when the compression flag is enabled (default-on once enabled), bash output
flows through five tiers in order:

1. **Specific Rust compressors** — stateful parsers keyed by a specific tool token anywhere in
   the command (`npx vitest`, `pnpm exec eslint`, etc.). Win first. Currently:
   `git` (status / diff / show / log / branch / blame / add / commit / push / pull / fetch /
   stash), `cargo`, `tsc`, `pytest`, `eslint`, `vitest` / `jest`, `biome`, `prettier`, `ruff`,
   `mypy`, `go`, `golangci-lint`, `playwright`, `next`.
2. **Output-shape sniffers** — the same inner-tool parsers recognizing their own summaries even
   when invoked through wrappers (`npm test`, `make test`, `bun run vitest`, `./scripts/check.sh`).
3. **Package-manager compressors** — broad head-token matchers (`npm`, `pnpm`, `bun`) that
   compress unclaimed package-manager output.
4. **Built-in TOML filters** — declarative strip + truncate + cap + shortcircuit rules covering
   the long tail of CLI tools. Ships 22 filters: `make`, `ls`, `tree`, `df`, `du`, `find`, `wc`,
   `gradle`, `xcodebuild`, `terraform`, `helm`, `docker`, `kubectl`, `gh`, `ansible-playbook`,
   `aws`, `curl`, `wget`, `deno`, `pip`, `uv`, `psql`. User-supplied filters at
   `<storage_dir>/filters/*.toml` override built-ins; project-supplied filters at
   `<project>/.cortexkit/aft/filters/*.toml` override both but require explicit trust via
   `npx @cortexkit/aft@latest doctor filters trust`.
5. **Generic fallback** — ANSI stripping plus consecutive-line deduplication and middle-truncate.

Use `npx @cortexkit/aft@latest doctor filters` to inspect what's loaded for the current project. Pass
`compressed: false` on a bash call to opt out for that invocation.

#### Writing a custom TOML filter

```toml
# ~/.local/share/cortexkit/aft/filters/my-tool.toml

[filter]
matches = ["my-tool"]                # program name (after stripping env vars + path)
description = "Compact my-tool output"

[strip]
patterns = [                          # regex per line; matching lines are dropped
  '^Loading config from',
  '^Resolving \d+ dependencies',
]

[truncate]
line_max = 500                        # middle-truncate per-line over N chars

[cap]
max_lines = 80                        # head|tail|middle
keep = "tail"

[shortcircuit]                        # if remainder matches `when`, replace whole output
when = '^\s*$'
replacement = "my-tool: ok"

[ansi]
strip = true                          # default true
```

Project filters under `.cortexkit/aft/filters/` are an attack vector — a malicious repo could ship a filter
that strips real failures and replaces them with `tests: ok`. AFT therefore **only loads project
filters from explicitly trusted projects**. Run `npx @cortexkit/aft@latest doctor filters trust` to
review and approve them. Inspect the active set with `npx @cortexkit/aft@latest doctor filters` and dump
a single filter's resolved content with `--show <name>`.

**Background** — when the background flag is enabled, pass `background: true` to spawn detached.
The call returns `taskId`; inspect a snapshot with `bash_status({ "taskId": "..." })`; kill with
`bash_kill({ "taskId": "..." })`. Completed-but-unread tasks surface in `bg_completions: [...]` on
the next foreground tool call, and a completion reminder is delivered automatically (no polling
needed). Output is buffered in memory up to 1MB and spills beyond that to AFT's bash-output cache
(default `~/.cache/aft/bash-output/<taskId>.log`, or the harness storage directory when configured).
Background tasks and undelivered completions are persisted to disk and survive AFT restarts.
Once a task has finished for 24 hours and its completion has been delivered, its output files are
deleted; copy evidence you need into your report or a shared file. With project-root restrictions,
only the starting session gets an ownership exception for output outside the project; another
session (such as a reviewer reading a worker's path) may be refused even while the files exist.

Foreground bash also starts through the same task flow. Short commands are polled and return inline
output; commands that exceed the foreground wait window are automatically promoted to background
and return a `taskId`.

**Background task limit** — at most 8 background tasks (`max_background_bash_tasks`) run at once
per project root, shared by all sessions in it. Local and remote (`runon`) tasks share the same
slots. Only background tasks count: `background: true` and
`pty: true` launches, and foreground commands once they are promoted. A background launch at the
limit is refused with `background_task_limit_exceeded`; the message lists your own session's tasks
holding slots (task id, age, the first 60 characters of the command, up to 8 rows), counts the slots
held by other sessions in one line ("N more held by other sessions in this project", with no task ids
or commands), and says to free a slot by stopping one of your own tasks with `bash_kill` or to wait
for a task to finish. A foreground command always starts, even at the limit. If it then outlives
its wait window it is still promoted (it is
already running, so refusing it would lose its work) and counts from then on, so the count can
briefly exceed the limit.

**`bash_status`** — read-only snapshot of a background or PTY task's current state and output.
Never waits. For PTY tasks, `outputMode` selects `screen` (vt100-rendered), `raw` (byte stream),
or `both`.

**`bash_watch`** — block on or register for a background task's output. In a main session sync
waits are for a short remaining wait on a task (default 30s, max `bash.watch_sync_max_ms`, 120s
by default); for anything longer end the turn on `bash({background:true})` and let the completion
reminder wake you, or use `bash({wait:true})` when the result is needed before anything else. In a
delegated (subagent) session, which cannot be woken once its turn ends, a sync wait without
`timeoutMs` waits up to the worker wait limit (`bash.worker_wait_max_ms`, 30 minutes by default),
then reports the command is still running with how long it has run and its latest output; the
worker watches again to keep waiting or kills it. An explicit `timeoutMs` can shorten that wait,
but never extends it beyond the configured worker limit. Every session's blocking `bash`
call (`wait: true`, or any foreground call when
`bash.subagent_background` is false) is bounded the same way: at the limit the command moves to the
background, not killed, and the reply gives its task id, including on the tool-provider/v1 route.
The head preset declares a reply deadline for `bash`; the worker preset declares it for `bash`
and `bash_watch`. Both use the resolved `bash.worker_wait_max_ms` plus 30 seconds for spawn,
terminal handoff and reply delivery. A watch does not spawn a command, but retains the same
margin for admission, observing terminal state, and delivering its reply under load. Head
catalogs do not serve `bash_watch` (heads receive completion wakes); reader catalogs serve neither
shell tool and declare no deadline. If the sum exceeds the role contract's 24-hour maximum, the catalog
still serves the tools but omits their reply metadata, leaving callers' fallback in force.
The wait limit is not the task's kill
deadline: every reply that hands a task back (launch, promotion, detach) and every `bash_watch`
result also names the task's own deadline (for example, "AFT kills this task at 2026-09-10 10:30:00Z, when it has run 30 minutes (its default background limit); about 12 minutes remain", or the `timeout` you passed), and a task killed by it is reported by
name ("killed by AFT's default background limit of 30 minutes (exit 124)") rather than as a bare
timeout. Sync mode
waits until a `pattern` matches, the task exits, `timeoutMs` elapses, a new message arrives, or the
call is aborted. Async mode (`background: true`)
registers a pattern watcher that fires a notification when matched and suppresses the default
completion reminder. (Wait/watch semantics moved here from `bash_status` — `bash_status` is
snapshot-only.)

**`bash_write`** — send input to a running PTY task. `input` is either a literal string or an
array mixing literal strings and `{ key: "..." }` objects for control keys, e.g.
`[ "iHello", { key: "esc" }, ":wq", { key: "enter" } ]`. Named keys cover enter/tab/esc/arrows/
function keys/ctrl chords. `{ key: "enter" }` emits CR. Expanded input is capped at 1 MiB.

**PTY** — pass `pty: true` (implies `background: true`) to run interactive programs (python,
node, vim, even a nested agent) in a real PTY. Drive it with `bash_write` and inspect with
`bash_status({ outputMode: "screen" })`. PTY sessions are session-scoped and do not survive a
bridge restart. Subagents cannot spawn PTY tasks.

**Permissions (OpenCode only)** — bash uses tree-sitter to parse the command into sub-commands
and asks for permission per sub-command via `ctx.ask({ permission: "bash", patterns, always })`.
File-touching commands (`rm`, `cp`, `mv`, etc.) also fire
`ctx.ask({ permission: "external_directory" })` for paths outside the project root. Pi has no
permission system; bash runs without prompts.

---

### ast_grep_search

Search for structural code patterns using meta-variables. Patterns must be complete AST nodes.
Groovy path filtering covers `.groovy`, `.gvy`, `.gy`, `.gsh`, `*.gradle`, and `Jenkinsfile` when `lang` is `groovy`.

```json
{ "pattern": "console.log($MSG)", "lang": "typescript" }
```

- `$VAR` matches a single AST node
- `$$$` matches multiple nodes (variadic)

Returns matches with file, line (1-based), column, matched text, and captured variable values.
Add `contextLines: 3` to include surrounding lines.

```json
// Find all async functions in JS/TS
{ "pattern": "async function $NAME($$$) { $$$ }", "lang": "typescript" }
```

When the supplied `paths` or `globs` resolve to zero files (rather than matching files with no
hits), the response carries `no_files_matched_scope: true` and `scope_warnings: [...]` listing
each path/glob that contributed zero files. This is distinct from a successful search that
returned no matches.

---

### ast_grep_replace

Replace structural code patterns across files. Applies changes by default — set `dryRun: true` to preview.
Groovy replacements use the same `lang: "groovy"` path rules as `ast_grep_search`, including `*.gradle` and `Jenkinsfile`.

```json
{ "pattern": "console.log($MSG)", "rewrite": "logger.info($MSG)", "lang": "typescript" }
```

Meta-variables captured in `pattern` are available in `rewrite`. Returns unified diffs per file
in dry-run mode, or writes changes with backups when applied.

---

### lsp_diagnostics

On-demand LSP file/scope check. Lazily spawns the relevant language server, opens the document, prefers
LSP 3.17 pull diagnostics where supported (rust-analyzer, gopls, ty), and falls back to push + waitMs
for servers that don't support pull (bash-language-server, yaml-language-server, typescript-language-server).

**Not** a project-wide type checker — for full coverage run `tsc --noEmit`, `cargo check`,
`pyright src/`, etc. AFT's LSP is for fast feedback during edits.

**Built-in servers (6 + 1 experimental):** TypeScript (`.ts`/`.tsx`/`.js`/`.jsx`), Pyright (Python),
rust-analyzer (Rust), gopls (Go), bash-language-server (`.sh`/`.bash`/`.zsh`),
yaml-language-server (`.yaml`/`.yml`), and ty (Python, gated by `experimental.lsp_ty`).

User-defined servers go in `lsp.servers` (see Configuration). Disable any built-in via `lsp.disabled`.

```json
// Check a single file (pull where supported, push fallback otherwise)
{ "path": "src/api.ts", "severity": "error" }

// Check files under a directory (workspace pull from active servers + 200-file walk for unchecked listing)
{ "directory": "src/", "severity": "all" }

// Wait up to 2s for push diagnostics on push-only servers (bash, yaml, typescript)
{ "path": "deploy.sh", "waitMs": 2000 }
```

Response shape:

```jsonc
{
  "diagnostics": [{ "file", "line", "column", "end_line", "end_column", "severity", "message", "code" }],
  "total": 2,
  "files_with_errors": 1,
  "complete": true,                 // true = trustable absence of diagnostics; false = partial result
  "lsp_servers_used": [             // per-server status; empty array means nothing was checked
    { "id": "rust-analyzer", "status": "pull_ok" },
    { "id": "bash-language-server", "status": "binary_not_installed" }
  ],
  "unchecked_files": []              // directory mode only — files we couldn't get info for
}
```

**Reading honestly:** `total: 0` with empty `lsp_servers_used` means **nothing was checked** —
install the relevant LSP server (see warnings on plugin startup). `total: 0` with `pull_ok` /
`push_only` means the file is genuinely clean.

When the response looks unhelpful and you can't tell which case applies, run
`npx @cortexkit/aft@latest doctor lsp <file>` for a per-file triage that names the binary
resolution path, workspace root markers, and spawn outcome for every server registered for
that extension.

---

### aft_outline

Returns all top-level symbols in a file with their kind, name, line range, visibility, and nested
`members` (methods in classes, sub-headings in Markdown). Takes a single `target` parameter that
auto-detects what to outline:

- **File path** → outline that file with signatures
- **Directory path** → recursively outline all source files (capped at 200)
- **Array of paths** → batch-outline multiple specific files
- **URL** (`http://`/`https://`) → fetch and outline a remote HTML/Markdown/JSON document

Pass `files: true` with a directory `target` to get a flat indexed file tree instead of a symbol
outline — each entry carries language, top-level symbol count, and byte size, reusing the symbol
cache so it's cheap on large trees.

For **Markdown** files (`.md`, `.mdx`): returns heading hierarchy with section ranges — each
heading becomes a symbol you can read by name.

```json
// Outline a single file
{ "target": "src/server.ts" }

// Outline two files at once
{ "target": ["src/server.ts", "src/router.ts"] }

// Outline all source files in a directory
{ "target": "src/auth" }

// Outline a remote document (OpenCode)
{ "target": "https://docs.example.com/api.md" }
```

In multi-file and directory modes, files that fail to parse or whose language is unsupported
are listed under `skipped_files` with a per-file `reason` (e.g. `parse_error`,
`unsupported_language`) instead of being silently dropped from the result.

---

### aft_zoom

Inspect code symbols. Returns the full source of named symbols. Pass `callgraph: true` to also
annotate each symbol with `calls_out` (what it calls) and `called_by` (what calls it); it
defaults to `false` to keep output minimal. Use exactly one input mode.

Use this when you need to understand a specific function, class, or type in detail — not for
reading entire files (use `read` for that).

```json
// Single symbol in a file
{ "path": "src/app.ts", "symbols": "handleRequest" }

// Add call-graph annotations (calls_out / called_by) for the symbol
{ "path": "src/app.ts", "symbols": "handleRequest", "callgraph": true }

// Multiple symbols in the SAME file (polymorphic: string or array)
{ "path": "src/app.ts", "symbols": ["Config", "createApp"] }

// Cross-file batch — each target names its own file
{ "targets": [
  { "path": "src/app.ts", "symbol": "createApp" },
  { "path": "src/db.ts", "symbol": "connect" }
] }

// Section of a remote/cached document by heading (OpenCode)
{ "url": "https://docs.example.com/api.md", "symbols": "Authentication" }
```

`symbols` (string or array, same file), `targets` (cross-file array), and `path`/`url`
(single-file or URL) are mutually exclusive — pass exactly one mode. For Markdown/HTML, use the
heading text as the symbol name. Cross-file batches return partial results with per-symbol
`symbol_not_found` rather than failing the whole call.

---

### aft_conflicts

Show all git merge conflicts across a repository in a single call. Resolves the git top level,
unions index-unmerged files (`git ls-files --unmerged`) with a tracked working-tree marker scan
(so staged-but-still-marked files are caught), parses conflict markers, and returns line-numbered
regions with 3 lines of surrounding context — the same format as `read` output. The output names
the `Checked repo root` so a false "none" is obvious.

```json
// Inspect the session project repository
{}

// Inspect a different repository or git worktree (e.g. where a rebase is running)
{ "path": "/Users/me/work/some-other-worktree" }
```

`path` is optional (defaults to the session project root); conflicts are discovered from that
path's git top level, so it works even when the conflict is in a sibling worktree or a subdirectory.
Returns output like:

```
9 files, 13 conflicts

── src/manager.ts [3 conflicts] ──

  15:   resolveInheritedPromptTools,
  16:   createInternalAgentTextPart,
  17: } from "../../shared"
  18: <<<<<<< HEAD
  19: import { normalizeAgentForPrompt } from "../../shared/agent-display-names"
  20: =======
  21: import { applySessionPromptParams } from "../../shared/session-prompt-params-helpers"
  22: >>>>>>> upstream/dev
  23: import { setSessionTools } from "../../shared/session-tools-store"
```

Use `edit` with the full conflict block (including markers) as `oldString` to resolve each conflict.

When a `git merge` or `git rebase` produces conflicts, the plugin automatically appends a hint
suggesting `aft_conflicts` to the bash output.

---

### grep

Trigram-indexed regex search that hoists the host harness's built-in `grep`. Requires
`indexes.trigram` (on by default). The trigram index is built in a background thread
at session start, persisted to disk for fast cold starts, and kept fresh via file watcher.
Falls back to direct file scanning when the index isn't ready.

For out-of-project paths, shells out to ripgrep with the same flag set the harness's native
grep would have used.

```json
{ "pattern": "handleRequest", "include": "*.ts" }
```

Returns matches grouped by file with relative paths, sorted by modification time (newest first),
100 matching lines per call:

```
src/server.ts
42: export async function handleRequest(req: Request) {
89:     return handleRequest(retryReq)

src/test/server.test.ts
15: import { handleRequest } from "../server"

Found 3 match(es) across 2 file(s). [index: ready]
```

Every matching line of the page is printed. Lines longer than 500 characters are cut and marked
`… [line truncated]`, and a page stops early once its rows reach 50 KB. When more matches remain,
the reply names the next `offset` and ends with the standard
`shown N of M rows (cap) · narrow: offset, path, include, exclude` trailer.

Paging with `offset` is exact for a single file and for any search that saw every match. When a
search over several files stops at the match limit, the engine keeps the first matches it reaches
before sorting them, so later pages may overlap or skip rows; the reply says so, and narrowing with
`path` or `include` restores exact paging.

Parameters: `pattern` (required), `path` (optional — scope to subdirectory or absolute path),
`include` (glob filter, e.g. `"*.ts"`), `offset` (zero-based count of matching lines to skip,
default 0). The bridge command also accepts `exclude` (negate glob), `case_sensitive`
(default true) and `max_results` (page size, default 100).

---

### glob

Indexed file discovery that hoists the host harness's built-in `glob`. Uses
`indexes.trigram` (on by default) and scans directly while the index is off or building. Returns absolute paths sorted by modification time,
capped at 100 files.

```json
{ "pattern": "**/*.test.ts" }
```

Returns relative paths. For small result sets, a flat list:

```
3 files matching **/*.test.ts

src/server.test.ts
src/utils.test.ts
src/auth/login.test.ts
```

For larger result sets (>20 files), groups by directory:

```
20 files matching **/*.test.ts

src/ (8 files)
  server.test.ts, utils.test.ts, config.test.ts, ...

src/auth/ (4 files)
  login.test.ts, session.test.ts, token.test.ts, permissions.test.ts

... and 8 more files in 3 directories
```

Parameters: `pattern` (required), `path` (optional — scope to subdirectory or absolute path).

---

### aft_search

The primary code-search tool: concepts, identifiers, error strings, regex, literals, and
filenames are auto-routed to the right engine and returned ranked. Works even when you only
know what the code does, not what it's named (*"where is rate limiting handled"*, *"retry
logic"*, `^export`, `Cargo.lock`). The semantic lane uses `indexes.semantic` (on by default) and
[ONNX Runtime](https://onnxruntime.ai/) installed on the system when using the default
`fastembed` backend.

**How it works — hybrid retrieval:** AFT classifies each query by shape (identifier, path,
error-code, mixed, natural-language) and routes through two lanes:

- **Semantic lane** — local embedding model (all-MiniLM-L6-v2, ~22MB, downloaded on first
  use) embeds code symbols (functions, classes, methods, structs, file-level summaries for
  thin files) and matches by cosine similarity. Always runs.
- **Lexical lane** — trigram-index scoring over the same code files, runs for identifier,
  path, error-code, and mixed shapes. Disabled for pure natural-language queries to avoid
  noise.

Each result carries a `source` tag drawn from a closed set: `semantic`, `lexical`, `regex`, or
`literal`. A semantic result that the lexical lane also surfaced is not retagged — instead it
carries `hybrid_boosted: true` plus a `lexical_score` alongside its `semantic_score`, so the
provenance of each result stays unambiguous. Lexical-only matches (files the embedding lane
missed but the trigram lane found by exact identifier hit) tag `source: lexical` and render with
`[lexical match — score: <X>]` instead of a symbol range. Indexes code extensions only; markdown,
HTML, and config files are intentionally excluded — they crowd out real code matches. Use grep
for prose.

**Install ONNX Runtime:**
- **macOS:** `brew install onnxruntime`
- **Linux (Debian/Ubuntu):** `apt install libonnxruntime`
- **Linux (other):** Download from [ONNX Runtime releases](https://github.com/microsoft/onnxruntime/releases)
- **Windows:** `winget install Microsoft.ONNXRuntime`

Without ONNX Runtime, all other AFT tools work normally — only `aft_search` is unavailable.

```json
{ "query": "authentication middleware that validates JWT tokens" }
```

Returns ranked results with relevance scores, provenance tags, and code snippets:

```
crates/aft/src/commands/configure.rs
handle_configure [function] lines 17-253 score 0.648 source semantic (hybrid_boosted)
    pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
      let root = match req.params.get("project_root")...
      ...

packages/opencode-plugin/src/bridge.ts
checkVersion [method] lines 150-175 score 0.482 source semantic
    private async checkVersion(): Promise<void> {
      ...

packages/pi-plugin/src/commands/aft-status.ts
aft-status [file-summary] [file summary] score 0.504 source semantic
    /**
     * /aft-status — show AFT status (version, indexes, LSP, storage).

Found 10 semantic result(s). [index: ready]
```

The index is built in a background thread at session start, persisted to disk for fast cold
start, and uses cAST-style enrichment (file path + kind + name + signature + body snippet)
for better embedding quality. Files with ≤2 top-level exports additionally produce a
synthetic "file-summary" chunk that captures filename, parent directory, leading doc
comment, and export list — this lifts recall for filename-shaped concept queries like
*"the bridge spawn helper"*.

Parameters: `query` (natural language description, or a single auto-routed string), `pattern`
(a regex for names or text that must appear), `topK` (optional — default 10), `path` (optional —
search a different project root, see below). At least one of `query` and `pattern` is required;
an empty or whitespace `pattern` counts as absent.

**Query and pattern together.** When you know a name the answer must contain but also want the
file that explains a concept, give both:

```json
{ "query": "how is the config file loaded", "pattern": "load_config|ConfigLoader" }
```

`pattern` uses grep's regex syntax and is case-sensitive, like grep. The ranking starts from
exactly the ranking `query` alone would return, and the pattern only adds bounded evidence to
it. Each top-level alternative of the pattern (`a|b`, and the branches of a single group such as
`^pub struct (A|B)`) is judged on its own; one that matches more than 50 files, or that nothing
declares, adds nothing. A leading result moves up a little for its own lines that match a
selective alternative, a little more if it declares the name. An alternative's definition is
placed right after the best-placed leading result that mentions the name (a caller always
does), never above it, and marked with `supports`. With no such result, the definition is added
only when the query has no semantic lane (identifier-shaped prose, or the semantic index is
still building) or when it is relevant to the query on its own (semantic or lexical score);
otherwise the summary line still names it. The reply
opens with a pattern summary line: how many files matched, how many
the query also found, up to three definition sites (or "no definition found"), and "examined N
of M candidate files" when the pattern's examination hit its bound. Each result carries
`matched_by` (`query`, `pattern` or `both`). A reply is marked incomplete when either input
was bounded. An invalid regex is refused with grep's `invalid_pattern` error and position.
`pattern` alone ranks the files it matches, definitions first, one result per file. `pattern`
cannot yet be combined with `path`.

#### Cross-project search

Pass `path` to search another project on the same machine — a sibling repository, a
worktree, or any directory AFT has indexed before:

```json
{ "query": "how does the sidebar discover the RPC endpoint", "path": "~/Work/other-project" }
```

How it behaves:

- **Read-only borrow.** The session never mutates the other project's caches: indexes are
  opened with strict read-only loaders, no rebuild or re-embed is triggered, and the other
  project's watcher/ownership state is untouched. The owning session keeps exclusive write
  access.
- **Served with drift disclosure.** The borrowed index reflects the owner's last persisted
  state. If files changed since (or the checkouts differ in untracked ignore files), results
  are still served and the response carries a note — `borrowed`, drift count, and
  `ignore_rules_differ` — with a hint to grep the target root when exact line numbers matter.
  Owners flush their index on clean shutdown, so drift is normally the owner's uncommitted
  working set since it last ran.
- **Semantic lane requires a matching embedding backend.** A borrowed semantic index is only
  used when its embedding fingerprint matches your session's backend; otherwise the search
  degrades to the lexical lane for that query.
- **Nonexistent paths fail loud** with `path_not_found` instead of walking up to an
  unrelated parent.
- **Never indexed?** If the target project has never been opened with AFT, there is nothing
  to borrow and the search reports the index as unavailable — open the project once (any
  session) to build its index.

Under `restrict_to_project_root: true` (and always for untrusted MCP callers), cross-project
search outside the session's root is denied.

#### Embedding backends

`aft_search` supports three embedding backends. Set them under the `semantic` block in your
**user-level** AFT config (`~/.config/cortexkit/aft.jsonc`).

> **Trust boundary:** `backend`, `base_url`, and `api_key_env` are user-only. Project-level
> `aft.jsonc` files cannot inject these — a hostile repository cannot point your embeddings
> at an attacker-controlled endpoint or steal your API keys. Project config can still tune
> `model`, `timeout_ms`, and `max_batch_size`.

**1. `fastembed` (default)** — local ONNX Runtime, no network, no API key. Uses
`all-MiniLM-L6-v2` (384 dims, ~22MB downloaded on first use). Works fully offline.

```jsonc
{
  "indexes": { "semantic": true }
  // No "semantic" block needed — fastembed is the default.
}
```

**2. `openai_compatible`** — any OpenAI-compatible `/v1/embeddings` endpoint. Works with
OpenAI, Together, Voyage, Anyscale, Fireworks, vLLM, LM Studio, etc.

```jsonc
{
  "indexes": { "semantic": true },
  "semantic": {
    "backend": "openai_compatible",
    "model": "text-embedding-3-small",
    "base_url": "https://api.openai.com/v1",
    "api_key_env": "OPENAI_API_KEY",   // env var name, not the key itself
    "timeout_ms": 25000,                // optional, default 25000
    "max_batch_size": 64                // optional, default 64
  }
}
```

The plugin reads the API key from the environment variable named in `api_key_env` at request
time. The key itself is never stored in config or logs.

**3. `ollama`** — self-hosted Ollama at its `/api/embeddings` endpoint. No API key required.

```jsonc
{
  "indexes": { "semantic": true },
  "semantic": {
    "backend": "ollama",
    "model": "nomic-embed-text",
    "base_url": "http://127.0.0.1:11434"
  }
}
```

**Choosing a backend:**

| backend | when |
|---|---|
| `fastembed` | Default. Offline, free, zero setup beyond ONNX Runtime. Lower recall than larger models but good enough for most code search. |
| `openai_compatible` | You want higher recall (1536/3072-dim models), already pay for an embeddings API, or your repo is large enough that local CPU embedding is too slow. |
| `ollama` | You want a local self-hosted model larger than `all-MiniLM-L6-v2` without paying per-token. |

**Switching backends rebuilds the index.** AFT stores a fingerprint
(`backend`, `model`, `base_url`, `dimension`, plus an internal `chunking_version` for the
synthetic file-summary chunk format) with every persisted index. Changing any fingerprint
field deletes the cached index on the next session start and rebuilds from scratch in the
background — necessary because different models produce different vector dimensions and
incompatible semantic spaces. For OpenAI-compatible backends on a large repo this can
mean hundreds of API calls and a few minutes of wall-clock time. `aft_search` returns
`[index: building]` while the rebuild runs; status is also visible via `/aft-status` and
the OpenCode TUI sidebar. **First launch on AFT v0.23+** triggers a one-time rebuild
because `chunking_version` bumped to add file-summary chunks.

Switching API keys (rotating `OPENAI_API_KEY` without changing `api_key_env`) does **not**
trigger a rebuild — the key isn't part of the fingerprint.

**Constraints:**
- `base_url` must be `http://` or `https://`.
- **Loopback is allowed.** `127.0.0.1`, `localhost`, and `*.localhost` are accepted so
  self-hosted backends like Ollama work at their default config (`http://127.0.0.1:11434`).
  Loopback is by definition same-machine and not an SSRF target.
- **Non-loopback private/reserved IPs are rejected** at configure time as an SSRF guard
  against a malicious config redirecting embeddings to internal services. This includes
  10/8, 172.16/12, 192.168/16, 169.254/16 (link-local), and 100.64/10 (CGNAT). mDNS
  hostnames (`*.local`) are also rejected. Users running self-hosted services on a LAN IP
  can either bind the service to loopback and use SSH/port-forward, or expose it on a
  public-routable interface.
- The plugin retries failed HTTP requests with exponential backoff before giving up.
- Vector dimension is detected from the first response and validated on every subsequent
  insert; mismatches abort the build instead of silently corrupting the index.

---

### aft_inspect

Codebase-health snapshot in a single call. Returns summary stats for TODOs, file/symbol metrics,
dead code, unused exports, and code duplicates. Use it when starting work in unfamiliar code,
before a refactor or review, or to verify cleanup completeness.

```json
// Summary across all active categories
{}

// Drill into specific categories with per-category detail
{ "sections": ["todos", "dead_code"], "topK": 20 }

// Restrict to a subtree
{ "sections": "duplicates", "scope": "crates/aft/src/inspect" }
```

Categories run in two tiers:

- **Tier 1** (`todos`, `metrics`) — computed synchronously with a ~1s soft deadline. Always
  present in the response.
- **Tier 2** (`dead_code`, `unused_exports`, `duplicates`) — heavier cross-file analyses backed
  by a callgraph snapshot. They run as background scans triggered on session idle. An
  `aft_inspect` call reads cached aggregates and returns immediately: a category that hasn't been
  scanned yet appears in `pending_categories`, and a category whose inputs changed since the last
  scan appears in `stale_categories`. Tier 2 never blocks the call on a full scan.

Response shape:

```jsonc
{
  "success": true,
  "scanner_state": {
    "disabled_categories": ["complexity", "circular_deps", "..."], // deferred to a later release
    "pending_categories": ["dead_code", "unused_exports", "duplicates"],
    "stale_categories": [],
    "failed_categories": [],
    "tier2_last_run": null
  },
  "summary": {
    "metrics": { "files": 845, "loc": 318236, "symbols": 8490 },
    "todos": { "count": 8, "by_kind": { "TODO": 3, "FIXME": 1, "BUG": 2, "HACK": 1, "XXX": 1 } },
    "dead_code": { "count": 0, "by_language": {} },
    "unused_exports": { "count": 0 },
    "duplicates": { "count": 0, "total_groups": 0 }
  },
  "details": { /* present only for categories named in `sections` */ }
}
```

Parameters: `sections` (string or array of category names, or `"all"`; omit for summary-only),
`scope` (file or directory to restrict results to — applied as a result filter), `topK` (max
drill-down items per category, default 20).

#### Terminal results

Every call ends in exactly one terminal result, named by `inspect_terminal` and rendered as the
first line of the tool output:

| `inspect_terminal` | Header | Meaning |
|---|---|---|
| `fresh` | `FRESH` | Completed, and every diagnostics producer gave an authoritative answer. Carries `wait_stamp` (`text` and `phases`). |
| `partial` | `PARTIAL — diagnostics unknown: rust-analyzer @ .: cargo check still running (1 file); retry aft_inspect.` | Completed with the same payload and `wait_stamp` as `fresh`, but some diagnostics are unknown. The header explains each analyzer's reason and affected file count once. `partial_reason` holds the header text without the `PARTIAL — ` prefix. Full reasons and per-file gaps remain structured. |
| `interrupted` | `INTERRUPTED — ...` | Cancelled before completion; retry the request. `completed_phases` remains in structured data. |
| `phase_failed` | `PHASE-FAILED — ...` | Inspection could not finish; address the reported reason and retry or narrow the scope. `completed_phases`, `failed_phase`, `failure_reason` and `failure_detail` remain structured. |

`FRESH` never heads a result whose diagnostics summary reads `diagnostics: unknown`. A missing
language server binary (for example `docker-langserver`) leaves its files' diagnostics unknown and
the result `partial`; install the server or disable it with `lsp.disabled` to get `fresh`.

Wait stamps and phase lists are retained only in structured data, not agent-visible text.
For example, the structured wait stamp counts phases instead of listing each one:
`waited: yes; completed: lsp_start ×9 (typescript 3, python 2, bash 1, ...), lsp_quiescence ×9 (...), tier2_rescan ×5 (...)`.
Repeated TypeScript runtime notes collapse the same way
(`TypeScript 5.9.3: project installation ×3 (first: ...)`).

With `scope`, all findings, counts, worst offenders, and examples are narrowed to those paths.
Duplicate groups can cross the boundary: their summary says how many groups touch the scope,
and their examples show only scoped occurrences. Project-wide duplicate percentages and
suppression totals are omitted because they cannot be narrowed from the cached aggregate.
Cross-boundary import cycles likewise show only scoped members and edges, labeled as cycles
touching the scope rather than claiming a closed cycle entirely within it.
Without `scope`, the repository-wide summaries are unchanged.

#### Which language servers start

A blocking inspect walks the project (or the scope) and starts one server per (server, workspace
root) its files need, all at once, under one shared startup deadline. The walk honors `.gitignore`
and `.aftignore`, and skips dependency and build output directories, test-fixture directories
(`fixtures`, `__fixtures__`, `testdata`, `test-data`, `__mocks__`, `__snapshots__`, `corpora`) and
`spikes/`, unless the scope itself names one of them. Roots are shared where one server can serve
several packages: TypeScript packages that see the same installed TypeScript version share one
server, and Bash and YAML use the outermost root marker in the project. Rust uses the owning Cargo
workspace.

Registered on the `recommended` and `all` tiers; disable via `inspect.enabled: false` in config.

---

### aft_delete

Delete one or more files (or directories) with per-file backups. Each file is backed up before
deletion and can be restored via `aft_safety undo` — one delete call is one undo operation, even
when it removes many files. Single-file callers pass a single-element array.

```json
{ "files": ["src/deprecated/old-utils.ts"] }
```

```json
{ "files": ["dist/foo.js", "dist/bar.js", "dist/baz.js"] }
```

Deleting a directory requires `recursive: true`. The tree is backed up before it is removed,
and one `aft_safety undo` restores it exactly:

- directories, including empty ones, with their permissions;
- file contents;
- hard links inside the tree, relinked so they share one file again;
- symlinks: the link itself is deleted and restored with its exact target text (also when
  dangling or pointing outside the tree); the target is never followed or touched.

Reported as warnings: sockets are deleted but not restored (they hold no data, and a recreated
one would have no process listening), and a file hard-linked to paths outside the tree comes
back as an independent copy. Refused before anything is deleted: a mount point of another
filesystem (removing it would delete that filesystem's contents), named pipes, device nodes,
and symlinks undo cannot recreate exactly (a non-UTF-8 target, or any symlink on Windows).
The delete removes only the entries it backed up, deepest first; if something new appears in
the tree meanwhile, it stops with `partial: true`, leaves the new entry in place, and undo
restores what was removed.

Paths under the system temp directory are never backed up, and neither is anything when
backups are disabled; such deletes skip these checks and the backup budget, and only a mount
point is refused. A recursive delete whose backup would record more than 2,000 entries (files,
directories and links) or copy 100 MiB in one call is refused; delete it in smaller pieces or
use bash `rm -rf` when no undo is needed.

```json
{ "files": ["build/cache"], "recursive": true }
```

Returns `{ success, complete, deleted: [paths], skipped_files: [{file, reason}] }`. Partial
success is allowed: files that can be deleted are deleted; files that fail (missing,
permission denied, etc.) are reported in `skipped_files` and `complete: false`. If every
file fails the call throws an error.

---

### aft_move

Move or rename a file. Creates parent directories for the destination automatically. Falls back
to copy+delete for cross-filesystem moves. Backs up the original before moving.

```json
{ "path": "src/helpers.ts", "destination": "src/utils/helpers.ts" }
```

Returns `{ file, destination, moved, backup_id }` on success.

### move_symbol (binary command)

The symbol-relocation command moves a TypeScript/JavaScript symbol and rewrites its
consumers. It is separate from `aft_move`, which moves a whole file. Source, destination
and consumer writes are checkpointed; a write failure or syntax rollback restores the
operation and does not run the type checker.

With `validate: "full"` or `validate_on_edit: "full"`, type checking runs only after every
file has been written and formatted, once per distinct checker and configured project
root. File arguments are batched where supported (Go named-file checks across directories
fall back to one run per file). Each existing per-file `results` entry carries its own
`validation_errors` array and optional `validate_skipped_reason`. The rendered `output`
contains one `type check: …` summary line, not a separate top-level diagnostic list.
Type errors report the completed project's state; they do not roll back an otherwise
successful move. Intermediate errors from a consumer's not-yet-rewritten import are not
reported.

---

### aft_callgraph

Call graph and data-flow analysis across the workspace.

| Mode | What it does |
|------|-------------|
| `call_tree` | What does this function call? (forward, default depth 5) |
| `callers` | Where is this function called from? (reverse, default depth 1) |
| `trace_to` | How does execution reach this function from entry points? |
| `impact` | What callers are affected if this function changes? |
| `trace_to_symbol` | Shortest call path from one symbol to another. Needs `toSymbol` (and `toPath` to disambiguate). |
| `trace_data` | Follow a value through assignments and parameters. Needs `expression`. |

```json
// Find everything that would break if processPayment changes
{
  "op": "impact",
  "path": "src/payments/processor.ts",
  "symbol": "processPayment",
  "depth": 3
}
```

---

### aft_import

Language-aware import management for TS, JS, TSX, Python, Rust, Go, Solidity, Java, C#, PHP,
Kotlin, Scala, Swift, Ruby, Lua, C, C++, Perl, and Vue.

```json
// Add named imports with auto-grouping and deduplication
{
  "op": "add",
  "path": "src/api.ts",
  "module": "react",
  "names": ["useState", "useEffect"]
}

// Remove a single named import
{ "op": "remove", "path": "src/api.ts", "module": "react", "removeName": "useEffect" }

// Re-sort and deduplicate all imports by language convention
{ "op": "organize", "path": "src/api.ts" }
```

Beyond `module`/`names`, `add` accepts language-appropriate fields: `defaultImport` and
`namespace` (ES `import X, * as NS`), `alias` (whole-module alias, e.g. Solidity `import "./X.sol" as X`),
`typeOnly` (TS), `modifiers` (statement modifiers such as Java/C# `static`, Swift `@testable`),
and `importKind` (kind-specific imports such as PHP `function`/`const`, Swift `struct`/`func`).

`op: "remove"` reports `removed: false` with a `reason` of `module_not_found` (the module
was never imported) or `name_not_found` (the module is imported but the named symbol isn't
in it) instead of pretending the removal succeeded. For languages whose grammar can't be
safely regenerated (wildcard/group/rename forms), `organize` sorts verbatim and refuses
rather than corrupting syntax; a generated line that fails to parse rolls back and reports
`generated_invalid_syntax` instead of a false success.

---

### aft_safety

Backup and recovery for risky edits.

| Op | Description |
|----|-------------|
| `undo` | Undo the entire last tool call (omit `path`), or the last edit to one file (pass `path`) |
| `history` | List all edit snapshots for a file |
| `checkpoint` | Save a named snapshot; explicit files may be untracked or gitignored |
| `restore` | Restore files to a named checkpoint |
| `list` | List all available checkpoints |

```json
// Checkpoint before a multi-file refactor
{ "op": "checkpoint", "name": "before-auth-refactor" }

// Restore if something goes wrong
{ "op": "restore", "name": "before-auth-refactor" }
```

> **Note:** Backups are persisted to disk (SQLite-backed) and survive bridge and host restarts.
> Named checkpoints are memory-only and session-scoped: they are lost when the bridge or daemon
> restarts. A checkpoint response reports this durability limit; use persisted undo history when
> recovery must survive a restart.
> Undo is operation-scoped: a single multi-file delete, directory delete, file move, symbol move,
> or AST replace is reverted atomically by one `undo` with no `path`. Per-file undo stack is
> capped at 20 entries — oldest snapshots are evicted when exceeded. History, undo, and
> checkpoints are session-private even when multiple sessions share one project bridge.
