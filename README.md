<p align="center">
  <img src="assets/banner.jpeg" alt="AFT — Agent File Toolkit" width="50%" />
</p>

<h1 align="center">AFT — Agent File Toolkit</h1>

<p align="center">
  <strong>Tree-sitter powered code manipulation and analysis tools for AI coding agents.</strong><br>
  Semantic editing, call-graph navigation, and structural search — all in one toolkit.
</p>

<p align="center">
  <a href="https://crates.io/crates/agent-file-tools"><img src="https://img.shields.io/crates/v/agent-file-tools?label=crate&color=blue&style=flat-square" alt="crates.io"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft"><img src="https://img.shields.io/npm/v/@cortexkit/aft?label=cli&color=blue&style=flat-square" alt="npm @cortexkit/aft"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft-opencode"><img src="https://img.shields.io/npm/v/@cortexkit/aft-opencode?label=opencode&color=blue&style=flat-square" alt="npm @cortexkit/aft-opencode"></a>
  <a href="https://www.npmjs.com/package/@cortexkit/aft-pi"><img src="https://img.shields.io/npm/v/@cortexkit/aft-pi?label=pi&color=blue&style=flat-square" alt="npm @cortexkit/aft-pi"></a>
  <a href="https://discord.gg/DSa65w8wuf"><img src="https://img.shields.io/discord/1488852091056295957?style=flat-square&logo=discord&logoColor=white&label=Discord&color=5865F2" alt="Discord"></a>
  <a href="https://github.com/cortexkit/aft/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-green?style=flat-square" alt="MIT License"></a>
</p>

<p align="center">
  <a href="#get-started">Get Started</a> ·
  <a href="#what-is-aft">What is AFT?</a> ·
  <a href="#search-benchmarks">Benchmarks</a> ·
  <a href="#features">Features</a> ·
  <a href="#tool-reference">Tool Reference</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="https://discord.gg/DSa65w8wuf">💬 Discord</a>
</p>

---

## Get Started

Run the unified AFT setup wizard — it auto-detects which harnesses (OpenCode, Pi) you have installed and configures each one:

```bash
npx @cortexkit/aft setup
```

That's it. On the next session start the `aft` binary downloads if needed and all tools become available. Use `--harness opencode` or `--harness pi` to target a specific harness.

### What AFT does to each harness

- **OpenCode** — replaces the built-in `read`, `write`, `edit`, `apply_patch`, `ast_grep_search`, `ast_grep_replace`, and `lsp_diagnostics` with AFT-powered versions and adds the `aft_` family on top.
- **Pi** — replaces the built-in `read`, `write`, `edit`, and `grep` and adds the `aft_` family on top.

<details>
<summary>Manual install — OpenCode</summary>

```bash
opencode plugin --global @cortexkit/aft-opencode@latest
```

or

```json
// ~/.config/opencode/config.json
{
  "plugin": ["@cortexkit/aft-opencode@latest"]
}
```
</details>

<details>
<summary>Manual install — Pi</summary>

```bash
pi install npm:@cortexkit/aft-pi
```

See the [pi-plugin README](packages/pi-plugin/README.md) for configuration details.
</details>

### CLI Commands

The unified `@cortexkit/aft` CLI works across every supported harness:

| Command | What it does |
|---|---|
| `npx @cortexkit/aft setup` | Interactive first-time setup — auto-detects installed harnesses and registers AFT with each |
| `npx @cortexkit/aft doctor` | Read-only health check across all detected harnesses (host install, plugin registration, binary cache, ONNX, config) |
| `npx @cortexkit/aft doctor --fix` | Auto-fix what doctor can: register missing plugin entries, download a missing `aft` binary, repair ONNX Runtime |
| `npx @cortexkit/aft doctor lsp <file>` | Show exactly which LSP servers AFT would spawn for a file, where each binary resolves, and why a server failed to start |
| `npx @cortexkit/aft doctor --clear` | Interactive cache cleanup — pick which caches to clear (plugin packages, binary, LSP, semantic) |
| `npx @cortexkit/aft doctor --issue` | Collect diagnostics and open a GitHub issue with sanitized logs |

Add `--harness opencode` or `--harness pi` to any command to target one harness explicitly.

**`setup`** — Registers AFT with each installed harness (edits the harness config to enable
the AFT plugin). When multiple harnesses are detected, prompts you to pick which ones to
configure.

**`doctor`** — Read-only health check. Reports host install state, plugin registration,
plugin cache version, binary cache, config parse errors, ONNX Runtime availability (for
semantic search), storage directory sizes, and log file status. Exits non-zero when
something needs attention so it can be wired into CI scripts. Pure inspection — nothing
is modified.

**`doctor --fix`** — Applies the fixes doctor would otherwise just report. Registers
missing plugin entries in your harness config, downloads the matching `aft` binary if
`~/.cache/aft/bin` is empty (run this after `--clear` or after wiping the cache to recover
without opening a session), and repairs ONNX Runtime version mismatches by clearing AFT's
managed ONNX cache so the next bridge launch redownloads. Each step asks confirmation
before mutating state.

**`doctor lsp <file>`** — Per-file LSP triage. Shows which servers AFT registered for the
file's extension, where each binary resolves (project `node_modules/.bin` → `lsp_paths_extra`
→ `PATH` → not found), whether the workspace root marker resolves walking up from the file,
the spawn outcome, and the diagnostics returned (if any). Use this when `lsp_diagnostics`
returns `total: 0` and you can't tell whether the file is genuinely clean or no server ever
spawned. Pass `--harness opencode` or `--harness pi` if you have both plugins installed and
need to disambiguate. Example output:

```
$ npx @cortexkit/aft doctor lsp ./python/main.py

Server attempts:
  ✗ ty
    Binary: ty (NOT FOUND on PATH or in lsp_paths_extra)
    Workspace root: /repo/python (markers: requirements.txt)
    Status: binary not installed
    Action: Install with `uv tool install ty` or `pip install ty`.
```

**`doctor --clear`** — Walks you through interactive cache cleanup. Useful when you're on
an old version and `@latest` doesn't seem to update (some harness installers cache npm
packages aggressively), or when you want to reset the LSP server cache to force a fresh
download. Targets harness plugin cache, binary cache, downloaded LSP servers, and semantic
index storage.

**`doctor --issue`** — Collects a full diagnostic report, sanitizes your username and home
path out of the logs, and files a GitHub issue. If you have `gh` installed, it submits
directly; otherwise it writes the report to `./aft-issue-<timestamp>.md` and opens the
new-issue page in your browser.

---

## What is AFT?

AI coding agents are fast, but their interaction with code is often blunt. The typical pattern:
read an entire file to find one function, construct a diff from memory, apply it by line number,
and hope nothing shifted. Tokens burned on context noise. Edits that break when the file changes.
Navigation that requires reading three files to answer "what calls this?"

AFT is a toolkit built on top of tree-sitter's concrete syntax trees. Every operation addresses
code by what it *is* — a function, a class, a call site, a symbol — not by where it happens to
sit in a file right now. Agents can outline a file's structure in one call, zoom into a single
function, edit it by name, then follow its callers across the workspace. All without reading a
single line they don't need.

AFT **hoists** itself into the host harness's built-in tool slots. Whichever tools your
harness exposes natively (`read`, `write`, `edit`, `bash`, `apply_patch`, `grep`,
`ast_grep_search`, `lsp_diagnostics`, etc.) are replaced by AFT-enhanced versions — same
names the agent already knows, but now backed by the Rust binary for backups, formatting,
inline diagnostics, indexed search, output compression, and symbol-aware operations. Bash
hoisting is a peer feature alongside file tools: agents can keep calling `bash`, while AFT
adds optional command rewriting, compression, background task control, and OpenCode's
tree-sitter permission model.

The toolkit is a two-component system: a Rust binary that does the heavy lifting (parsing,
analysis, edits, formatting) and a thin TypeScript plugin per harness that adapts the binary
to the harness's plugin API. The binary ships pre-built for all major platforms and downloads
automatically on first use — no install ceremony required.

**Currently supported harnesses:** [OpenCode](https://opencode.ai) and [Pi](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent).
MCP support for Claude Code / Cursor is on the roadmap.

**Supported languages (17):** TypeScript / TSX, JavaScript / JSX, Python, Rust, Go, C, C++,
C#, Zig, Bash / Shell, HTML, Markdown, Solidity, and Vue. All 17 work with `aft_outline`,
`aft_zoom`, `read`/`edit`/`write`, and the rest of the structural tool surface. AST pattern
search and replace (`ast_grep_search` / `ast_grep_replace`) covers TS/JS, Python, Rust, Go,
C, C++, C#, Zig, Solidity, and Vue. Import management (`aft_import`) covers TS/JS/TSX, Python,
Rust, and Go.

---

## How it Helps Agents

**The token problem.** A 500-line file costs ~375 tokens to read. Most of the time, the agent
needs one function. `aft_zoom` with a `symbol` param returns that function plus a few lines of
context: ~40 tokens. Over a multi-step task, the savings compound fast.

**The fragile-edit problem.** Line-number edits break the moment anything above the target moves.
`edit` in symbol mode addresses the function by name. The agent writes the new body; AFT finds
the symbol, replaces it, validates syntax, and runs the formatter. Nothing to count.

**The navigation problem.** "Where is this function called?" means grep or reading every importer.
`aft_navigate` with `callers` mode returns every call site across the workspace in one round trip.
`impact` mode goes further: it tells the agent what else breaks if that function's signature changes.

Here's a typical agent workflow:

**1. Get the file structure:**

```json
// aft_outline
{ "target": "src/auth/session.ts" }
```

```
src/auth/session.ts
  E fn    createSession(userId: string, opts?: SessionOpts): Promise<Session> 12:38
  E fn    validateToken(token: string): boolean 40:52
  E fn    refreshSession(sessionId: string): Promise<Session> 54:71
  - fn    signPayload(data: Record<string, unknown>): string 73:80
  E type  SessionOpts 5:10
  E var   SESSION_TTL 3:3
```

**2. Zoom into the specific function:**

```json
// aft_zoom
{ "filePath": "src/auth/session.ts", "symbol": "validateToken" }
```

```
src/auth/session.ts:40-52
  calls_out: verifyJwt (src/auth/jwt.ts:8), isExpired (src/auth/utils.ts:15)
  called_by: authMiddleware (src/middleware/auth.ts:22), handleLogin (src/routes/login.ts:45)

  37: // --- context_before ---
  38:
  39: /** Validate a JWT token and check expiration. */
  40: export function validateToken(token: string): boolean {
  41:   if (!token) return false;
  42:   const decoded = verifyJwt(token);
  43:   if (!decoded) return false;
  44:   return !isExpired(decoded.exp);
  45: }
  46:
  47: // --- context_after ---
  48: export function refreshSession(sessionId: string): Promise<Session> {
```

**3. Edit it by name:**

```json
// edit
{
  "filePath": "src/auth/session.ts",
  "symbol": "validateToken",
  "content": "export function validateToken(token: string): boolean {\n  if (!token) return false;\n  return verifyJwt(token);\n}"
}
```

**4. Check who calls it before changing its signature:**

```json
// aft_navigate
{ "op": "callers", "filePath": "src/auth/session.ts", "symbol": "validateToken", "depth": 2 }
```

---

## Search Benchmarks

With `search_index: true`, AFT builds a trigram index in the background and serves
grep queries from memory. Here's how it compares to ripgrep on real codebases:

### opencode-aft (253 files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `validate_path` | 31.4ms | 1.48ms | **21x** |
| `BinaryBridge` | 31.0ms | 1.3ms | **24x** |
| `fn handle_grep` | 31.3ms | 0.2ms | **136x** |
| `search_index` | 31.5ms | 0.4ms | **71x** |

### reth (1,878 Rust files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `impl Display for` | 98.9ms | 1.10ms | **90x** |
| `BlockNumber` | 61.6ms | 2.19ms | **28x** |
| `EthApiError` | 32.7ms | 1.31ms | **25x** |
| `fn execute` | 36.6ms | 2.19ms | **17x** |

### Chromium/base (3,953 C++ files)

| Query | ripgrep | AFT | Speedup |
|-------|---------|-----|---------|
| `WebContents` | 69.5ms | 0.29ms | **236x** |
| `StringPiece` | 51.8ms | 0.78ms | **66x** |
| `NOTREACHED` | 51.6ms | 2.16ms | **24x** |
| `base::Value` | 54.4ms | 1.13ms | **48x** |

Rare queries see the biggest gains — the trigram index narrows candidates to a few files instantly.
High-match queries still benefit from `memchr` SIMD scanning and early termination.

Index builds in ~2s for most projects (under 2K files). Larger codebases like Chromium/base
(~4K files) take ~2 minutes for the initial build. Once built, the index persists to disk for
instant cold starts and stays fresh via file watcher and mtime verification.

---

## Features

- **File read** — line-numbered file content, directory listing, and image/PDF detection
- **Semantic outline** — list all symbols in a file (or several files, or a directory) with kind, name, line range, visibility
- **Symbol editing** — replace a named symbol by name with auto-format and syntax validation
- **Match editing** — find-and-replace by content with fuzzy fallback (4-pass: exact → trim trailing → trim both → normalize Unicode)
- **Batch & transaction edits** — atomic multi-edit within a file, or atomic multi-file edits with rollback
- **Glob replace** — pattern replace across all matching files in one call
- **Patch apply** — multi-file `*** Begin Patch` format for creates, updates, deletes, and moves
- **Call tree & callers** — forward call graph and reverse lookup across the workspace
- **Trace-to & impact analysis** — how does execution reach this function? what breaks if it changes?
- **Data flow tracing** — follow a value through assignments and parameters across files
- **Auto-format & auto-backup** — every edit formats the file and saves a snapshot for undo
- **Import management** — add, remove, organize imports language-aware (TS/JS/TSX/Python/Rust/Go)
- **Structural transforms** — add class members, Rust derive macros, Python decorators, Go struct tags, wrap try/catch
- **Workspace-wide refactoring** — move symbols between files (updates all imports), extract functions, inline functions
- **Safety & recovery** — undo last edit, named checkpoints, restore to any checkpoint
- **AST pattern search & replace** — structural code search using meta-variables (`$VAR`, `$$$`), powered by ast-grep
- **Git conflict viewer** — show all merge conflicts across the repository in a single call with line-numbered regions
- **Indexed search** — trigram-indexed `grep` and `glob` that hoist the host harness's built-ins, with background index building, disk persistence, and compressed output mode
- **Semantic search** — search code by meaning using local embeddings (fastembed + all-MiniLM-L6-v2), with cAST-style symbol chunking, cosine similarity ranking, and disk persistence
- **Bash hoisting** — replaces the host's built-in `bash` with an AFT-backed shell that supports rewriter rules (`cat` → `read`, `grep` → `grep` tool, `cat >>` → edit append), per-command output compression (`git`/`cargo`/`npm`/`bun`/`pnpm`/`pytest`/`tsc`), background tasks via `background: true` with `bash_status`/`bash_kill` for control, and tree-sitter-based permission scanning (OpenCode)
- **Inline diagnostics** — write and edit return LSP errors detected after the change
- **UI metadata** — diff previews (`+N/-N`) and file paths surface in the harness UI (OpenCode desktop, Pi terminal renderer)
- **Local tool discovery** — finds biome, oxfmt, prettier, tsc, pyright in `node_modules/.bin` automatically

---

## Tool Reference

> **All line numbers are 1-based** (matching editor, git, and compiler conventions).
> Line 1 is the first line of the file.

### Response convention

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

### Hoisted tools

These replace the host harness's built-ins. Registered under the same names by default. When
`hoist_builtin_tools: false`, they get the `aft_` prefix instead (e.g. `aft_read`).

Tools that don't exist natively in a given harness are simply registered as new tools — no
hoisting needed. (Pi, for example, doesn't ship `apply_patch` or `lsp_diagnostics`; AFT adds
them either way when the surface tier includes them.)

| Tool | Description | Key Params |
|------|-------------|------------|
| `read` | File read, directory listing, image/PDF detection | `filePath`, `startLine`, `endLine`, `offset`, `limit` |
| `write` | Write file with auto-dirs, backup, format, inline diagnostics | `filePath`, `content` |
| `edit` | Find/replace, symbol replace, batch, transaction, glob | `filePath`, `oldString`, `newString`, `symbol`, `content`, `edits[]` |
| `apply_patch` | `*** Begin Patch` multi-file patch format | `patchText` |
| `ast_grep_search` | AST pattern search with meta-variables | `pattern`, `lang`, `paths[]`, `globs[]` |
| `ast_grep_replace` | AST pattern replace (applies by default) | `pattern`, `rewrite`, `lang`, `dryRun` |
| `lsp_diagnostics` | Errors/warnings from language server | `filePath`, `directory`, `severity`, `waitMs` |
| `grep` | Trigram-indexed regex search with compressed output | `pattern`, `path`, `include`, `exclude` |
| `glob` | Indexed file discovery with compressed output | `pattern`, `path` |

### AFT-only tools

Always registered with `aft_` prefix regardless of hoisting setting.

**Recommended tier** (default):

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_outline` | Structural outline of a file, directory, files, or URL | `target` (string or array) |
| `aft_zoom` | Inspect symbols with call-graph annotations | `filePath`, `symbol`, `symbols[]` |
| `aft_import` | Language-aware import add/remove/organize | `op`, `filePath`, `module`, `names[]` |
| `aft_conflicts` | Show all git merge conflicts with line-numbered regions | *(none)* |
| `aft_search` | Hybrid semantic + lexical code search by meaning | `query`, `topK` |
| `aft_safety` | Undo, history, checkpoints, restore | `op`, `filePath`, `name` |

**All tier** (set `tool_surface: "all"`):

| Tool | Description | Key Params |
|------|-------------|------------|
| `aft_delete` | Delete one or more files with backup | `files` |
| `aft_move` | Move or rename a file with backup | `filePath`, `destination` |
| `aft_navigate` | Call graph and data-flow navigation | `op`, `filePath`, `symbol`, `depth` |
| `aft_transform` | Structural code transforms (members, derives, decorators) | `op`, `filePath`, `container`, `target` |
| `aft_refactor` | Workspace-wide move, extract, inline | `op`, `filePath`, `symbol`, `destination` |

---

### read

Plain file reading and directory listing. Pass `filePath` to read a file, or a directory path to
list its entries. Paginate large files with `startLine`/`endLine` or `offset`/`limit`.

```json
// Read full file
{ "filePath": "src/app.ts" }

// Read lines 50-100
{ "filePath": "src/app.ts", "startLine": 50, "endLine": 100 }

// Read 30 lines from line 200
{ "filePath": "src/app.ts", "offset": 200, "limit": 30 }

// List directory
{ "filePath": "src/" }
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
{ "filePath": "src/config.ts", "content": "export const TIMEOUT = 10000;\n" }
```

Returns inline LSP diagnostics if type errors are introduced. Auto-formats using the project's
configured formatter (biome, oxfmt, prettier, etc.).

For partial edits (find/replace), use `edit` instead.

---

### edit

The main editing tool. Mode is determined by which parameters you pass:

**Find and replace** — pass `filePath` + `oldString` + `newString`:

```json
{ "filePath": "src/config.ts", "oldString": "const TIMEOUT = 5000", "newString": "const TIMEOUT = 10000" }
```

Matching uses a 4-pass fuzzy fallback: exact match first, then trailing-whitespace trim, then
both-ends trim, then Unicode normalization. Returns an error if multiple matches exist — use
`occurrence: N` (0-indexed) to pick one, or `replaceAll: true` to replace all.

**Symbol replace** — pass `filePath` + `symbol` + `content`:

```json
{
  "filePath": "src/utils.ts",
  "symbol": "formatDate",
  "content": "export function formatDate(d: Date): string {\n  return d.toISOString().split('T')[0];\n}"
}
```

Includes decorators, doc comments, and attributes in the replacement range.

**Batch edits** — pass `filePath` + `edits` array. Atomic: all edits apply or none do.

```json
{
  "filePath": "src/constants.ts",
  "edits": [
    { "oldString": "VERSION = '1.0'", "newString": "VERSION = '2.0'" },
    { "startLine": 5, "endLine": 7, "content": "// updated header\n" }
  ]
}
```

Set `content` to `""` to delete lines. Per-edit `occurrence` is supported.

**Multi-file transaction** — pass `operations` array. Rolls back all files if any operation fails.

```json
{
  "operations": [
    { "file": "a.ts", "command": "write", "content": "..." },
    { "file": "b.ts", "command": "edit_match", "match": "x", "replacement": "y" }
  ]
}
```

**Glob replace** — use a glob as `filePath` with `replaceAll: true`:

```json
{ "filePath": "src/**/*.ts", "oldString": "oldName", "newString": "newName", "replaceAll": true }
```

**Append to file** — pass `filePath` + `appendContent`:

```json
{ "filePath": "notes.md", "appendContent": "\n## New section\n..." }
```

Creates the file (and parent directories) if missing. Faster than read+write for adding to logs,
notepad files, or large appendable structures.

LSP diagnostics are returned automatically after every edit — if type errors are introduced,
they appear inline in the response. Use `aft_safety checkpoint` / `undo` for recovery before risky edits.

---

### apply_patch

Apply a multi-file patch using the `*** Begin Patch` format. Creates, updates, deletes, and
renames files atomically — if any operation fails, all revert.

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
Returns LSP diagnostics inline for any updated files that introduce type errors.

---

### bash

Execute shell commands through AFT's unified bash handler. AFT registers `bash` in the
recommended tool surface; experimental flags gate advanced behavior, not the tool itself.

**Schema:**

| Param | Type | Description |
|---|---|---|
| `command` | string | Shell command to execute |
| `timeout` | number | Foreground timeout in milliseconds (default: 120000) |
| `workdir` | string | Working directory for command execution |
| `description` | string | Short human-readable summary for harness UI metadata |
| `background` | boolean | Spawn detached and return a task id when enabled |
| `compressed` | boolean | Opt in/out of output compression for this call (default true; requires compression flag) |

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

**Compression** — when `experimental.bash.compress: true` (default-on once enabled), bash output
flows through three tiers in order:

1. **Built-in Rust compressors** — stateful parsers for high-traffic tools where heuristics like
   JSON parsing or section detection are required. Always wins when matched. Currently:
   `git` (status / diff / show / log / branch / blame / add / commit / push / pull / fetch /
   stash), `cargo`, `tsc`, `npm`, `bun`, `pnpm`, `pytest`, `eslint`, `vitest` / `jest`, `biome`.
2. **Built-in TOML filters** — declarative strip + truncate + cap + shortcircuit rules covering
   the long tail of CLI tools. v0.21 ships 15 filters: `make`, `ls`, `tree`, `df`, `du`, `find`,
   `wc`, `gradle`, `xcodebuild`, `terraform`, `helm`, `docker`, `kubectl`, `gh`,
   `ansible-playbook`. User-supplied filters at `<storage_dir>/filters/*.toml` override built-ins;
   project-supplied filters at `<project>/.aft/filters/*.toml` override both but require explicit
   trust via `npx @cortexkit/aft doctor filters trust`.
3. **Generic fallback** — ANSI stripping plus consecutive-line deduplication and middle-truncate.

Use `npx @cortexkit/aft doctor filters` to inspect what's loaded for the current project. Pass
`compressed: false` on a bash call to opt out for that invocation.

#### Writing a custom TOML filter

```toml
# ~/.local/share/opencode/storage/plugin/aft/filters/my-tool.toml

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

Project filters under `.aft/filters/` are an attack vector — a malicious repo could ship a filter
that strips real failures and replaces them with `tests: ok`. AFT therefore **only loads project
filters from explicitly trusted projects**. Run `npx @cortexkit/aft doctor filters trust` to
review and approve them. Inspect the active set with `npx @cortexkit/aft doctor filters` and dump
a single filter's resolved content with `--show <name>`.

**Background** — when `experimental.bash.background: true`, pass `background: true` to spawn
detached. The call returns `task_id`; inspect with `bash_status({ "task_id": "..." })`; kill
with `bash_kill({ "task_id": "..." })`. Completed-but-unread tasks surface in
`bg_completions: [...]` on the next foreground tool call. Output is buffered in memory up to 1MB
and spills beyond that to AFT's bash-output cache (default `~/.cache/aft/bash-output/<task_id>.log`,
or the harness storage directory when configured).

Foreground bash now also starts through the same task flow. Short commands are polled by the
plugin and return inline output as before; commands that exceed the foreground wait budget are
automatically promoted to background and return a `task_id` to inspect with `bash_status` or stop
with `bash_kill`.

**Permissions (OpenCode only)** — bash uses tree-sitter to parse the command into sub-commands
and asks for permission per sub-command via `ctx.ask({ permission: "bash", patterns, always })`.
File-touching commands (`rm`, `cp`, `mv`, etc.) also fire
`ctx.ask({ permission: "external_directory" })` for paths outside the project root. Pi has no
permission system; bash runs without prompts.

---

### ast_grep_search

Search for structural code patterns using meta-variables. Patterns must be complete AST nodes.

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
{ "filePath": "src/api.ts", "severity": "error" }

// Check files under a directory (workspace pull from active servers + 200-file walk for unchecked listing)
{ "directory": "src/", "severity": "all" }

// Wait up to 2s for push diagnostics on push-only servers (bash, yaml, typescript)
{ "filePath": "deploy.sh", "waitMs": 2000 }
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
`npx @cortexkit/aft doctor lsp <file>` for a per-file triage that names the binary
resolution path, workspace root markers, and spawn outcome for every server registered for
that extension. See [CLI Commands](#cli-commands).

---

### aft_outline

Returns all top-level symbols in a file with their kind, name, line range, visibility, and nested
`members` (methods in classes, sub-headings in Markdown). Takes a single `target` parameter that
auto-detects what to outline:

- **File path** → outline that file with signatures
- **Directory path** → recursively outline all source files (capped at 200)
- **Array of paths** → batch-outline multiple specific files
- **URL** (OpenCode only, `http://`/`https://`) → fetch and outline a remote HTML/Markdown document

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

Inspect code symbols with call-graph annotations. Returns the full source of named symbols with
`calls_out` (what it calls) and `called_by` (what calls it) annotations.

Use this when you need to understand a specific function, class, or type in detail — not for
reading entire files (use `read` for that).

```json
// Inspect a single symbol
{ "filePath": "src/app.ts", "symbol": "handleRequest" }

// Inspect multiple symbols in one call
{ "filePath": "src/app.ts", "symbols": ["Config", "createApp"] }
```

For Markdown files, use the heading text as the symbol name (e.g. `"symbol": "Architecture"`).

---

### aft_conflicts

Show all git merge conflicts across the repository in a single call. Auto-discovers conflicted
files via `git ls-files --unmerged`, parses conflict markers, and returns line-numbered regions
with 3 lines of surrounding context — the same format as `read` output.

```json
{}
```

No parameters required. Returns output like:

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

Find symbols by **concept** when grep keywords fall short. Returns ranked code matches with
similarity scores plus provenance (semantic, lexical, or hybrid). Requires
`semantic_search: true` and [ONNX Runtime](https://onnxruntime.ai/) installed on the system
when using the default `fastembed` backend.

**When to use it:**
- Exploring an unfamiliar area: *"where is rate limiting handled"*
- Concept doesn't appear as a literal string: *"retry logic"*, *"cache invalidation"*
- After grep attempts came back empty or noisy
- You know roughly what the function does but not its name

**When NOT to use it:**
- Error message or stack trace → use grep
- File/module structure → use `aft_outline`
- Following a call chain → use `aft_navigate`

**How it works — hybrid retrieval:** AFT classifies each query by shape (identifier, path,
error-code, mixed, natural-language) and routes through two lanes:

- **Semantic lane** — local embedding model (all-MiniLM-L6-v2, ~22MB, downloaded on first
  use) embeds code symbols (functions, classes, methods, structs, file-level summaries for
  thin files) and matches by cosine similarity. Always runs.
- **Lexical lane** — trigram-index scoring over the same code files, runs for identifier,
  path, error-code, and mixed shapes. Disabled for pure natural-language queries to avoid
  noise.

Results are fused: files that surface in both lanes get a boost and tag `source: hybrid`.
Semantic-only matches tag `source: semantic`. Lexical-only matches (files the embedding
lane missed but the trigram lane found by exact identifier hit) tag `source: lexical` and
render with `[lexical match — score: <X>]` instead of a symbol range. Indexes code
extensions only; markdown, HTML, and config files are intentionally excluded — they
crowd out real code matches. Use grep for prose.

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
handle_configure [function] lines 17-253 score 0.648 source hybrid
    pub fn handle_configure(req: &RawRequest, ctx: &AppContext) -> Response {
      let root = match req.params.get("project_root")...
      ...

packages/opencode-plugin/src/bridge.ts
checkVersion [method] lines 150-175 score 0.482 source semantic
    private async checkVersion(): Promise<void> {
      ...

packages/pi-plugin/src/commands/aft-status.ts
aft-status [file-summary] [file summary] score 0.504 source hybrid
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

Parameters: `query` (required — natural language description), `topK` (optional — default 10).

#### Embedding backends

`aft_search` supports three embedding backends. Set them under the `semantic` block in your
**user-level** AFT config (`~/.config/opencode/aft.jsonc` or `~/.pi/agent/aft.jsonc`).

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

### aft_delete

Delete one or more files with per-file backups. Each backup survives for the session and can
be restored via `aft_safety`. Single-file callers pass a single-element array.

```json
{ "files": ["src/deprecated/old-utils.ts"] }
```

```json
{ "files": ["dist/foo.js", "dist/bar.js", "dist/baz.js"] }
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
{ "filePath": "src/helpers.ts", "destination": "src/utils/helpers.ts" }
```

Returns `{ file, destination, moved, backup_id }` on success.

---

### aft_navigate

Call graph and data-flow analysis across the workspace.

| Mode | What it does |
|------|-------------|
| `call_tree` | What does this function call? (forward, default depth 5) |
| `callers` | Where is this function called from? (reverse, default depth 1) |
| `trace_to` | How does execution reach this function from entry points? |
| `impact` | What callers are affected if this function changes? |
| `trace_data` | Follow a value through assignments and parameters. Needs `expression`. |

```json
// Find everything that would break if processPayment changes
{
  "op": "impact",
  "filePath": "src/payments/processor.ts",
  "symbol": "processPayment",
  "depth": 3
}
```

---

### aft_import

Language-aware import management for TS, JS, TSX, Python, Rust, and Go.

```json
// Add named imports with auto-grouping and deduplication
{
  "op": "add",
  "filePath": "src/api.ts",
  "module": "react",
  "names": ["useState", "useEffect"]
}

// Remove a single named import
{ "op": "remove", "filePath": "src/api.ts", "module": "react", "removeName": "useEffect" }

// Re-sort and deduplicate all imports by language convention
{ "op": "organize", "filePath": "src/api.ts" }
```

`op: "remove"` reports `removed: false` with a `reason` of `module_not_found` (the module
was never imported) or `name_not_found` (the module is imported but the named symbol isn't
in it) instead of pretending the removal succeeded.

---

### aft_transform

Scope-aware structural transformations that handle indentation correctly.

| Op | Description |
|----|-------------|
| `add_member` | Insert a method or field into a class, struct, or impl block |
| `add_derive` | Add Rust derive macros (deduplicates) |
| `wrap_try_catch` | Wrap a TS/JS function body in try/catch |
| `add_decorator` | Add a Python decorator to a function or class |
| `add_struct_tags` | Add or update Go struct field tags |

```json
// Add a method to a TypeScript class
{
  "op": "add_member",
  "filePath": "src/user.ts",
  "container": "UserService",
  "code": "async deleteUser(id: string): Promise<void> {\n  await this.db.users.delete(id);\n}",
  "position": "last"
}
```

All ops support `validate` (`"syntax"` or `"full"`). Use `aft_safety checkpoint` / `undo` before risky transforms.

---

### aft_refactor

Workspace-wide refactoring that updates imports and references across all files.

| Op | Description |
|----|-------------|
| `move` | Move a symbol to another file, updating all imports workspace-wide |
| `extract` | Extract a line range (1-based) into a new function (auto-detects parameters) |
| `inline` | Replace a call site (1-based `callSiteLine`) with the function's body |

```json
// Move a utility function to a shared module
{
  "op": "move",
  "filePath": "src/pages/home.ts",
  "symbol": "formatCurrency",
  "destination": "src/utils/format.ts"
}
```

`move` saves a checkpoint before mutating anything. Use `aft_safety undo` to revert if needed.

---

### aft_safety

Backup and recovery for risky edits.

| Op | Description |
|----|-------------|
| `undo` | Undo the last edit to a file |
| `history` | List all edit snapshots for a file |
| `checkpoint` | Save a named snapshot of tracked files |
| `restore` | Restore files to a named checkpoint |
| `list` | List all available checkpoints |

```json
// Checkpoint before a multi-file refactor
{ "op": "checkpoint", "name": "before-auth-refactor" }

// Restore if something goes wrong
{ "op": "restore", "name": "before-auth-refactor" }
```

> **Note:** Backups are held in-memory for the session lifetime (lost on restart). Per-file undo
> stack is capped at 20 entries — oldest snapshots are evicted when exceeded.

---

## Configuration

AFT uses a two-level config system: user-level defaults plus project-level overrides.
Both files are JSONC (comments allowed). Config paths are harness-specific:

**OpenCode**

| Scope | Path |
|---|---|
| User | `~/.config/opencode/aft.jsonc` |
| Project | `<project>/.opencode/aft.jsonc` |

**Pi**

| Scope | Path |
|---|---|
| User | `~/.pi/agent/aft.jsonc` |
| Project | `<project>/.pi/aft.jsonc` |

The schema is identical across harnesses. Only file location differs.

### Config Options

```jsonc
{
  // Replace the host harness's built-in tools (read/write/edit/apply_patch/grep/etc.)
  // with AFT-enhanced versions. Default: true. Set to false to use aft_ prefix on all
  // tools instead — useful when you want to keep the harness defaults and access AFT
  // tools alongside them under explicit names.
  "hoist_builtin_tools": true,

  // Auto-format files after every edit. Default: true
  "format_on_edit": true,

  // Auto-validate after edits: "syntax" (tree-sitter, fast) or "full" (runs type checker)
  "validate_on_edit": "syntax",

  // Per-language formatter overrides (auto-detected from project config files if omitted)
  // Keys: "typescript", "python", "rust", "go"
  // Values: "biome" | "oxfmt" | "prettier" | "deno" | "ruff" | "black" | "rustfmt" | "goimports" | "gofmt" | "none"
  "formatter": {
    "typescript": "biome",
    "rust": "rustfmt"
  },

  // Per-language type checker overrides (auto-detected if omitted)
  // Keys: "typescript", "python", "rust", "go"
  // Values: "tsc" | "tsgo" | "biome" | "pyright" | "ruff" | "cargo" | "go" | "staticcheck" | "none"
  "checker": {
    "typescript": "biome"
  },

  // Tool surface level: "minimal" | "recommended" (default) | "all"
  // minimal:     aft_outline, aft_zoom, aft_safety only (no hoisting)
  // recommended: minimal + hoisted tools (read/write/edit/apply_patch/bash)
  //              + lsp_diagnostics + ast_grep + aft_import + aft_conflicts
  //              + grep/glob (when search_index is enabled)
  //              + aft_search (when semantic_search is enabled)
  //              (advanced bash behavior is gated by experimental.bash.* flags)
  // all:         recommended + aft_navigate, aft_delete, aft_move, aft_transform, aft_refactor
  "tool_surface": "recommended",

  // List of tool names to disable after surface filtering
  "disabled_tools": [],

  // Trigram-indexed grep/glob (graduated from experimental in v0.18).
  // Builds a background index on session start, persists to disk, updates via file watcher.
  // Falls back to direct scanning when the index isn't ready or for out-of-project paths.
  // Default: false
  "search_index": false,

  // Semantic code search (graduated from experimental in v0.18; aft_search tool).
  // Default backend is fastembed (local ONNX, no network) and requires ONNX Runtime
  // installed (brew install onnxruntime on macOS). The model is downloaded on first
  // use. Index persists to disk for fast cold start. To use a remote provider
  // (OpenAI-compatible) or self-hosted Ollama instead, see the "semantic" block
  // below and the aft_search "Embedding backends" section above.
  // Default: false
  "semantic_search": false,

  // Optional embedding-backend configuration for aft_search. Omit this block to use
  // the local fastembed default. Three backends are supported: "fastembed" (default,
  // local ONNX), "openai_compatible" (any /v1/embeddings endpoint — OpenAI, Together,
  // Voyage, vLLM, LM Studio, etc.), and "ollama" (self-hosted at /api/embeddings).
  //
  // USER-only fields: "backend", "base_url", "api_key_env" (project config cannot
  // inject these — strict-allowlist trust boundary). Project config can still tune
  // "model", "timeout_ms", "max_batch_size".
  //
  // Switching "backend", "model", or "base_url" deletes the persisted index and
  // rebuilds from scratch on next session start (necessary because dimensions and
  // semantic spaces differ across models). Rotating an API key without changing
  // "api_key_env" does NOT trigger a rebuild.
  "semantic": {
    "backend": "fastembed",            // "fastembed" | "openai_compatible" | "ollama"
    "model": "all-MiniLM-L6-v2",       // model id understood by the backend
    // "base_url": "https://api.openai.com/v1",   // required for openai_compatible / ollama
    // "api_key_env": "OPENAI_API_KEY",            // env var name (not the key itself)
    "timeout_ms": 25000,                // per-request timeout, kept under bridge limit
    "max_batch_size": 64                // embeddings batched in groups of this size
  },

  // Restrict all file operations to the project root directory.
  // Default: false. Matches OpenCode's and Pi's native behavior — neither host
  // hard-rejects out-of-root paths from their built-in tools (OpenCode prompts
  // the user; Pi just allows). Set to true to enforce a strict project-root
  // boundary on every AFT tool call. USER-only — strict-allowlist trust
  // boundary refuses to honor this field from project-level config so a
  // hostile repository cannot weaken your file boundary.
  "restrict_to_project_root": false,

  // OpenCode plugin only. When true, the auto-update hook installs newer
  // @cortexkit/aft-opencode versions automatically when you have @latest in your
  // OpenCode config.plugin entry. When false, the hook still notifies you that an
  // update is available but does not install it. Local-dev (file://) and pinned
  // (@x.y.z) installs always notify-only regardless of this setting.
  // Default: true. USER-only — strict-allowlist trust boundary refuses to honor
  // this field from project-level config to prevent hostile repos from silently
  // suppressing security updates.
  "auto_update": true,

  // Maximum source files allowed for call-graph operations (callers, trace_to,
  // trace_data, impact). Projects above this size return "project_too_large"
  // with guidance to open a specific subdirectory. Does not affect grep,
  // glob, read, edit, or any other tool.
  // Default: 5000. Measured cost: ~1ms per source file for the reverse-index
  // build, so 5000 ≈ 5–10s on cold start. The previous 20000 default exceeded
  // the bridge timeout on real ~7K-file projects, surfacing as bridge restart
  // instead of `project_too_large`. Raise this if you have patience and want
  // call-graph navigation on bigger projects.
  "max_callgraph_files": 5000,

  // Language servers used for post-edit diagnostics.
  //
  // Built-in servers (auto-registered when their binary is on PATH):
  //   typescript-language-server, pyright-langserver, rust-analyzer, gopls,
  //   bash-language-server, yaml-language-server
  //
  // Add your own with `lsp.servers`. Disable any with `lsp.disabled`.
  "lsp": {
    "servers": {
      "tinymist": {
        "extensions": [".typ"],
        "binary": "tinymist",
        "args": [],
        "root_markers": [".git", "typst.toml"],
        "env": {                  // optional — extra env vars passed to the spawned server
          "TYPST_FONT_PATHS": "/usr/share/fonts"
        },
        "initialization_options": {  // optional — server-specific LSP `initializationOptions`
          "formatterMode": "typstyle"
        }
      }
    },
    // Disable any registered server by id. IDs are case-insensitive. Built-in
    // ids: typescript, python, rust, go, bash, yaml, ty. Custom servers use
    // the key under `lsp.servers` (e.g. `tinymist`).
    "disabled": ["python"],
    "python": "ty",  // "auto" (default) | "pyright" | "ty"

    // LRU cap for the in-memory diagnostic cache.
    // Bigger = more files retained across the session.
    // Default: 5000. Set to 0 to disable cap (live dangerously on huge monorepos).
    "diagnostic_cache_size": 5000
  },

  "experimental": {
    // Use the experimental Astral `ty` Python type checker.
    // Implied when `lsp.python === "ty"`.
    "lsp_ty": false,

    // Bash-related experimental features (all default false).
    "bash": {
      // Rewrite common shell commands (cat / grep / find / sed / ls / rg / cat >>)
      // to AFT tools. Adds a footer hint nudging the agent to call the AFT tool
      // directly next time.
      "rewrite": false,

      // Compress bash output via per-command compressors (git, cargo, npm, bun,
      // pnpm, pytest, tsc) plus generic ANSI-strip + dedup. Pass `compressed: false`
      // on a single bash call to opt out for that call.
      "compress": false,

      // Enable background bash via `bash({ background: true })`. Adds the
      // `bash_status` and `bash_kill` tools. Completed-but-unread tasks surface
      // on the next foreground tool call as `bg_completions`.
      "background": false
    }
  }
}
```

AFT auto-detects the formatter and checker from project config files (`biome.json` → biome,
`.oxfmtrc.json` / `.oxfmtrc.jsonc` / `oxfmt.config.ts` → oxfmt, `.prettierrc` → prettier,
`Cargo.toml` → rustfmt, `pyproject.toml` → ruff/black, `go.mod` → goimports). Local tool binaries
(biome, oxfmt, prettier, tsc, pyright) are discovered in
`node_modules/.bin` before falling back to the system PATH. You only need per-language overrides
if auto-detection picks the wrong tool or you want to pin a specific formatter.

### Config schema migration

v0.18 reorganized experimental flags. Old config files using the flat shape:

```jsonc
{
  "experimental_search_index": true,
  "experimental_semantic_search": true,
  "experimental_lsp_ty": true,
  "experimental_bash_rewrite": true,
  "experimental_bash_compress": true,
  "experimental_bash_background": true
}
```

are migrated automatically on first load to the v0.18 shape:

```jsonc
{
  "search_index": true,        // graduated
  "semantic_search": true,     // graduated
  "experimental": {
    "lsp_ty": true,
    "bash": { "rewrite": true, "compress": true, "background": true }
  }
}
```

The original file is rewritten in place (both `.jsonc` and `.json` candidates are migrated).
JSONC comments are preserved. Both user-level and project-level configs are migrated
independently. The migration is idempotent — running again is a no-op.

### Language servers (LSP)

AFT runs language servers in-process for post-edit diagnostics and on-demand `lsp_diagnostics`
calls. Servers are spawned lazily — only when a file matching their extensions is touched, and
only if their binary is on `PATH`.

**Built-in servers** (auto-registered, no config needed):

| Server | Languages | Binary |
|---|---|---|
| TypeScript Language Server | `.ts .tsx .js .jsx .mjs .cjs` | `typescript-language-server` |
| Pyright | `.py .pyi` | `pyright-langserver` |
| rust-analyzer | `.rs` | `rust-analyzer` |
| gopls | `.go` | `gopls` |
| bash-language-server | `.sh .bash .zsh` | `bash-language-server` |
| yaml-language-server | `.yaml .yml` | `yaml-language-server` |

**Experimental:** `ty` (Astral's Python type checker) — gated behind
`experimental.lsp_ty: true` or `lsp.python: "ty"`. When enabled, ty runs alongside Pyright
unless you also disable Pyright via `lsp.disabled: ["python"]` (or use `lsp.python: "ty"`
which does both automatically).

**Registering a custom server:** add it under `lsp.servers` in your config. The example
configuration above shows registering `tinymist` for Typst files. Required fields per server:
`extensions` (array, leading `.` is stripped), `binary` (PATH lookup name). Optional:
`args`, `root_markers` (defaults to `[".git"]`), `disabled`.

**Disabling a built-in:** add the server's id to `lsp.disabled`. Built-in ids are
`typescript`, `python` (Pyright), `rust` (rust-analyzer), `go` (gopls), `bash`,
`yaml`, and `ty`. Custom servers use the key you registered them under in
`lsp.servers`. IDs are case-insensitive.

**Custom server fields:**

| Field | Required | Description |
|---|---|---|
| `extensions` | yes | Array of file extensions (leading `.` is stripped) |
| `binary` | yes | Binary name resolved against `PATH` |
| `args` | no | Args passed to the server (default: `[]`) |
| `root_markers` | no | Filenames whose presence anchors the workspace root (default: `[".git"]`) |
| `env` | no | Extra environment variables for the spawned process |
| `initialization_options` | no | Passed to the server's LSP `initialize` request |
| `disabled` | no | Skip this server even though it's registered |

**Missing-tool warnings:** on startup, AFT detects configured-but-missing formatters, type
checkers, and LSP binaries (for languages your project actually uses) and surfaces a one-time
notification per warning through whatever notification channel the harness exposes (OpenCode's
ignored-message channel, Pi's status messages). Dismissed warnings do not re-fire on plugin
updates — dedupe is per-warning-content, persisted in `<storage_dir>/warned_tools.json`.

### LSP auto-install

AFT auto-installs language servers your project actually needs. npm-distributed servers go
through `bun add` into AFT's cache; standalone binaries (clangd, lua-ls, zls, tinymist, texlab)
download from GitHub releases. The cache lives at `~/.cache/aft/lsp-packages/` and
`~/.cache/aft/lsp-binaries/` (Windows: `%LOCALAPPDATA%/aft/...`).

Configure via `lsp.*`:

```jsonc
"lsp": {
  // Auto-install relevant language servers on plugin startup. Default: true.
  // Set false to require manual install (servers still work if on PATH).
  "auto_install": true,

  // Supply-chain grace window in days. AFT only installs versions that have
  // been on the registry / GitHub releases for at least this many days,
  // defending against newly-published malicious versions that get yanked
  // within hours of detection. Default: 7. User pins via `lsp.versions`
  // bypass this.
  "grace_days": 7,

  // Per-package version pin map. Pins bypass the grace filter.
  // Keys: npm package name OR `owner/repo` for GitHub-hosted servers.
  "versions": {
    "typescript-language-server": "5.0.0",
    "clangd/clangd": "21.1.0"
  }
}
```

**Trust boundary:** `lsp.auto_install`, `lsp.grace_days`, `lsp.versions`, `lsp.servers`, and
`lsp.disabled` are **user-only** — values from project config (`.opencode/aft.jsonc` or
`.pi/aft.jsonc`) are stripped on load. A hostile repository cannot weaken your supply-chain
defenses, redirect AFT to download a different binary, or silently disable LSPs you rely on.
The plugin logs a warning when it strips a project-level setting.

**Trust-On-First-Use (TOFU) verification:** AFT records the SHA-256 of every downloaded
GitHub release archive in `.aft-installed`. If the same tag is ever re-installed with a
different hash, AFT refuses the install and points to `aft doctor --clear` for manual
recovery. The hash is also logged to the plugin log on every install for forensic comparison
against published checksums.

**What we do not do (yet):** AFT does **not** ship a vetted checksum allowlist. The TOFU
defense above only protects against post-cache-warmup tampering; the very first install of
any tag is accepted as-is once it passes the grace window and TLS verification. Supply-chain
attacks faster than the grace window are a residual risk. A fully-vetted allowlist is on the
v0.18 roadmap.

### Working with large repositories

If you point AFT at a very large directory (monorepo root, `~/Work`, `/home`, etc.), certain
features guard against unbounded work to keep the bridge responsive:

- **Call-graph ops** (`callers`, `trace_to`, `trace_data`, `impact`) return `project_too_large`
  above `max_callgraph_files` (default 5,000 — the empirical limit before the reverse-index build
  exceeds the bridge timeout on real workloads). Raise it in your config if you have patience.
- **Semantic indexing** is skipped above 10,000 source files.
- **`grep`, `glob`, `read`, `edit`, and other tools** work at any size.

Commands with heavier workloads get longer per-call timeouts: 60s for `callers`, `trace_to`,
`trace_data`, `impact`, `grep`, `glob`; 45s for `semantic_search`; 30s for everything else.
For best results in very large trees, point AFT at a specific project subdirectory.

---

## Architecture

AFT is a Rust binary that any AI coding harness can drive over JSON-over-stdio, plus a thin
adapter package per harness:

```
   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
   │  OpenCode   │    │     Pi      │    │  Future…    │
   │   agent     │    │   agent     │    │  (MCP, …)   │
   └──────┬──────┘    └──────┬──────┘    └──────┬──────┘
          │ tool calls       │ tool calls       │
          ▼                  ▼                  ▼
   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
   │ aft-opencode │   │   aft-pi     │   │     …        │  ← thin adapters per harness
   │  (TS plugin) │   │  (TS plugin) │   │              │    Each: hoist tools, manage
   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘    BridgePool, resolve binary,
          │                  │                  │            translate harness API
          └──────────────────┴──────────────────┘
                             │
                             │ JSON-over-stdio
                             ▼
                  ┌────────────────────────┐
                  │     aft binary         │  ← shared core
                  │       (Rust)           │
                  ├────────────────────────┤
                  │ • tree-sitter (17 lang)│
                  │ • symbols & call graph │
                  │ • diff/format/backup   │
                  │ • LSP client           │
                  │ • trigram index (exp)  │
                  │ • semantic index (exp) │
                  └────────────────────────┘
```

Per-harness adapter responsibilities:
- **Hoist** the harness's built-in tool slots (`read`, `write`, `edit`, etc.) and register
  AFT-only tools (`aft_outline`, `aft_navigate`, etc.) under whatever names the harness
  expects.
- **Manage a BridgePool** — one persistent `aft` process per session — for warm parse trees,
  isolated undo history, and zero per-call respawn cost.
- **Resolve the binary path** through the standard chain (`~/.cache/aft/bin/<v>/` → npm
  platform package → `PATH` → cargo install → GitHub release download).
- **Translate** between the harness's plugin API (permission prompts, metadata renderers,
  notification channels) and AFT's request/response protocol.

The binary speaks a simple request/response protocol: the adapter writes a JSON object to
stdin, the binary writes a JSON object to stdout. One process per session stays alive for the
session lifetime.

**Persistent storage:** AFT data lives under a per-harness storage directory (OpenCode:
`~/.local/share/opencode/storage/plugin/aft/`; Pi: `~/.pi/agent/aft/`). Backups, search
indexes, and downloaded LSP servers persist there across sessions.

---

## Supported Languages

| Language | Outline | Edit | Imports | Refactor |
|----------|---------|------|---------|---------|
| TypeScript | ✓ | ✓ | ✓ | ✓ |
| JavaScript | ✓ | ✓ | ✓ | ✓ |
| TSX | ✓ | ✓ | ✓ | ✓ |
| Python | ✓ | ✓ | ✓ | ✓ |
| Rust | ✓ | ✓ | ✓ | partial |
| Go | ✓ | ✓ | ✓ | partial |
| C | ✓ | ✓ | — | — |
| C++ | ✓ | ✓ | — | — |
| C# | ✓ | ✓ | — | — |
| Zig | ✓ | ✓ | — | — |
| Bash | ✓ | ✓ | — | — |
| HTML | ✓ | ✓ | — | — |
| Markdown | ✓ | ✓ | — | — |
| Solidity | ✓ | ✓ | — | — |
| Vue | ✓ | ✓ | — | — |

---

## Development

AFT is a monorepo: bun workspaces for TypeScript, cargo workspace for Rust.

**Requirements:** Bun ≥ 1.0, Rust stable toolchain (1.80+).

```sh
# Install JS dependencies
bun install

# Build the Rust binary
cargo build --release

# Build the TypeScript plugin
bun run build

# Run all tests
bun run test        # TypeScript tests
cargo test          # Rust tests

# Lint and format
bun run lint        # biome check
bun run lint:fix    # biome check --write
bun run format      # biome format + cargo fmt
```

**Project layout:**

```
opencode-aft/
├── crates/
│   └── aft/              # Rust binary — shared core (tree-sitter, search, LSP, etc.)
│       └── src/
├── packages/
│   ├── aft-cli/          # Unified CLI (@cortexkit/aft) — setup/doctor across all harnesses
│   ├── opencode-plugin/  # OpenCode adapter (@cortexkit/aft-opencode)
│   │   └── src/
│   │       ├── tools/    # One file per tool group
│   │       ├── config.ts # Config loading and schema
│   │       └── downloader.ts
│   ├── pi-plugin/        # Pi adapter (@cortexkit/aft-pi)
│   │   └── src/
│   └── npm/              # Platform-specific binary packages
└── scripts/
    └── version-sync.mjs  # Keeps npm and cargo versions in sync
```

---

## Roadmap

- **Additional harnesses** — MCP server for Claude Code, Cursor, and any other
  MCP-compatible host. Same Rust binary, new thin adapter.
- **Hardened ONNX Runtime download** — share the same size-cap / extraction-validation /
  TOFU pipeline used for LSP installs.
- **Streaming responses** — for large call trees and very large outline directories.
- **Watch mode** — live outline updates as files change.
- **Vetted checksum allowlist** — full supply-chain trust beyond the current TOFU + grace
  window defenses.

---

## Contributing

Bug reports and pull requests are welcome. For larger changes, open an issue first to discuss
the approach.

The binary protocol is documented in `crates/aft/src/main.rs`. Adding a new command means
implementing it in Rust and adding a corresponding tool definition (or extending an existing
one) in each harness adapter that should expose it (`packages/opencode-plugin/src/tools/` and
`packages/pi-plugin/src/tools/`).

**Adding a new harness adapter:** the existing OpenCode and Pi adapters are the reference
templates. Each one is roughly 1500 lines of TypeScript and reuses shared primitives
(downloader, resolver, BridgePool, config loader, LSP cache) that are currently duplicated
between the two — extracting these into a shared package is on the roadmap.

Run `bun run format` and `cargo fmt` before submitting. The CI will reject unformatted code.

---

## License

[MIT](LICENSE)
