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

## Hoisted tools

These replace the host harness's built-ins. Registered under the same names by default. When
`hoist_builtin_tools: false`, AFT file, search, and primary bash replacements get the `aft_`
prefix instead (for example, `aft_read` and `aft_bash`). The `bash_status`, `bash_watch`,
`bash_write`, and `bash_kill` companions remain unprefixed because they control AFT-owned task IDs.

Tools that don't exist natively in a given harness are simply registered as new tools — no
hoisting needed. (Pi, for example, doesn't ship `apply_patch` or `lsp_diagnostics`; AFT adds
them either way when the surface tier includes them.)

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

Always registered with `aft_` prefix regardless of hoisting setting.

**Recommended tier** (default):

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_outline` | Structural outline of a file, directory, files, or URL; or indexed file tree | `target` (string or array), `files` |
| `aft_zoom` | Inspect symbols (same-file or cross-file); opt-in call-graph annotations | `path`, `symbols` (string or array), `targets`, `url`, `callgraph` |
| `aft_import` | Language-aware import add/remove/organize | `op`, `path`, `module`, `names[]` |
| `aft_conflicts` | Show all git merge conflicts with line-numbered regions | `path` (optional) |
| `aft_search` | Hybrid semantic + lexical code search by meaning | `query`, `topK`, `path` |
| `aft_inspect` | Codebase-health snapshot (TODOs, metrics, dead code, unused exports, duplicates) | `sections`, `scope`, `topK` |
| `aft_safety` | Undo, history, checkpoints, restore | `op`, `path`, `name` |

**All tier** (set `tool_surface: "all"`):

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_delete` | Delete one or more files (or directories) with backup | `files`, `recursive` |
| `aft_move` | Move or rename a file with backup | `path`, `destination` |
| `aft_callgraph` | Call graph and data-flow navigation | `op`, `path`, `symbol`, `depth` |

---

### read

Plain file reading and directory listing. Pass `path` to read a file, or a directory path to
list its entries. Paginate large files with `startLine`/`endLine` or `offset`/`limit`.

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

---

### bash

Execute shell commands through AFT's unified bash handler. AFT registers `bash` in the
recommended tool surface; experimental flags gate advanced behavior, not the tool itself.

**Schema:**

| Param | Type | Description |
|---|---|---|
| `command` | string | Shell command to execute |
| `timeout` | number | Hard-kill cap in milliseconds (positive integer). Default 30 minutes when unset. NOT a polling window — see below. |
| `workdir` | string | Working directory for command execution |
| `description` | string | Short human-readable summary for harness UI metadata |
| `background` | boolean | Spawn detached and return a `taskId` (requires the background flag) |
| `compressed` | boolean | Opt in/out of output compression for this call (default true; requires compression flag) |
| `pty` | boolean | Run in a real PTY for interactive programs. Implies `background: true`. |
| `ptyRows` / `ptyCols` | number | PTY dimensions (max 60 rows / 140 cols). Soft-ignored on non-PTY calls. |

**Timeout model:** `timeout` is a hard-kill cap, never a polling parameter. A bare foreground
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

Foreground bash also starts through the same task flow. Short commands are polled and return inline
output; commands that exceed the foreground wait window are automatically promoted to background
and return a `taskId`.

**`bash_status`** — read-only snapshot of a background or PTY task's current state and output.
Never waits. For PTY tasks, `outputMode` selects `screen` (vt100-rendered), `raw` (byte stream),
or `both`.

**`bash_watch`** — block on or register for a background task's output. Sync waits are for a
short remaining wait on a task (default 30s, max `bash.watch_sync_max_ms`, 120s by default);
for anything longer end the turn on `bash({background:true})` and let the completion reminder
wake you, or use `bash({wait:true})` when the result is needed before anything else. Sync mode
waits until a `pattern` matches, the task exits, or `timeoutMs` elapses. Async mode (`background: true`)
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
`search_index: true` in config. The trigram index is built in a background thread
at session start, persisted to disk for fast cold starts, and kept fresh via file watcher.
Falls back to direct file scanning when the index isn't ready.

For out-of-project paths, shells out to ripgrep with the same flag set the harness's native
grep would have used.

```json
{ "pattern": "handleRequest", "include": "*.ts" }
```

Returns matches grouped by file with relative paths, sorted by modification time (newest first),
capped at 100 matches:

```
src/server.ts
42: export async function handleRequest(req: Request) {
89:     return handleRequest(retryReq)

src/test/server.test.ts
15: import { handleRequest } from "../server"

Found 3 match(es) across 2 file(s). [index: ready]
```

Files with more than 5 matches show the first 5 and `... and N more matches`. Lines are truncated
at 200 characters.

Parameters: `pattern` (required), `path` (optional — scope to subdirectory or absolute path),
`include` (glob filter, e.g. `"*.ts"`), `exclude` (negate glob), `case_sensitive` (default true).

---

### glob

Indexed file discovery that hoists the host harness's built-in `glob`. Requires
`search_index: true`. Returns absolute paths sorted by modification time,
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
logic"*, `^export`, `Cargo.lock`). Requires `semantic_search: true` and
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

Parameters: `query` (required — natural language description), `topK` (optional — default 10),
`path` (optional — search a different project root, see below).

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
  "semantic_search": true
  // No "semantic" block needed — fastembed is the default.
}
```

**2. `openai_compatible`** — any OpenAI-compatible `/v1/embeddings` endpoint. Works with
OpenAI, Together, Voyage, Anyscale, Fireworks, vLLM, LM Studio, etc.

```jsonc
{
  "semantic_search": true,
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
  "semantic_search": true,
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

Deleting a directory requires `recursive: true`. Every file inside is individually backed up
before the tree is removed; symlinks and empty directories are rejected before any mutation.

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
