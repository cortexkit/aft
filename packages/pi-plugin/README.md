# @cortexkit/aft-pi

**AFT (Agent File Tools) extension for the [Pi coding agent](https://github.com/badlogic/pi-mono)**

AFT is a high-performance file-manipulation toolkit for AI coding agents. It replaces Pi's built-in `read`, `write`, `edit`, and `grep` tools with an indexed Rust backend that adds trigram search, semantic search, fuzzy edits, auto-format, LSP diagnostics, call-graph navigation, and more — all backed by one warm long-running `aft` process per session.

## Install

```bash
pi install npm:@cortexkit/aft-pi
```

That's it. The extension auto-downloads the right AFT binary for your platform on first run (cached at `~/.cache/aft/bin/v<version>/aft`).

Prefer to pin a specific version?

```bash
pi install npm:@cortexkit/aft-pi@0.13.1
```

## What you get

### Hoisted built-in overrides

Pi's default `read`, `write`, `edit`, and `grep` are replaced with AFT-backed versions.

| Tool    | Pi built-in              | AFT replacement                                                                              |
| ------- | ------------------------ | -------------------------------------------------------------------------------------------- |
| `read`  | Node `fs.readFile`       | Rust reader with line-numbered output, directory listing, binary/image detection              |
| `write` | Node `fs.writeFile`      | Atomic write with per-file backup, auto-format (biome/oxfmt/prettier/ruff/rustfmt), LSP diagnostics |
| `edit`  | Plain substring replace  | Progressive fuzzy match (handles whitespace/Unicode drift), backups, glob-wide edits          |
| `grep`  | ripgrep shell-out        | Trigram-indexed search in-project, ripgrep fallback outside project root                      |

All four keep the same agent-facing parameters as Pi's built-ins, so your prompts, skills, and muscle memory don't change.

### AFT-specific tools

| Tool                | What it does                                                                      |
| ------------------- | --------------------------------------------------------------------------------- |
| `aft_outline`       | Structural outline (functions, classes, headings) for files or directories        |
| `aft_zoom`          | Symbol-level inspection with call-graph annotations                               |
| `aft_search`        | Semantic code search (embeddings, local ONNX or OpenAI-compatible)                |
| `aft_navigate`      | Call-graph navigation: callers, call_tree, impact, trace_to, trace_data           |
| `aft_conflicts`     | One-call merge-conflict inspection across all conflicted files                    |
| `aft_import`        | Language-aware import add / remove / organize (TS, JS, Python, Rust, Go)          |
| `aft_safety`        | Per-file undo, named checkpoints, restore                                         |
| `ast_grep_search`   | AST-aware pattern search across the filesystem                                    |
| `ast_grep_replace`  | AST-aware pattern rewrite                                                         |
| `lsp_diagnostics`   | On-demand LSP diagnostics (edit/write already inline diagnostics automatically)   |
| `aft_delete`        | Delete a file with backup (surface: `all`)                                        |
| `aft_move`          | Move/rename a file (surface: `all`)                                               |
| `aft_transform`     | Scope-aware structural transformations (surface: `all`)                           |
| `aft_refactor`      | Workspace-wide refactor: move symbol, extract function, inline call (surface: `all`) |

### Slash command

- `/aft-status` — show AFT version, search/semantic index state, LSP servers, storage paths

## Configure

AFT reads config from two levels, project overrides user:

- **User:** `~/.pi/agent/aft.jsonc` (or `.json`)
- **Project:** `<project>/.pi/aft.jsonc` (or `.json`)

All keys are optional. Example:

```jsonc
{
  // "minimal" | "recommended" (default) | "all"
  "tool_surface": "recommended",

  // Auto-format on write/edit using project formatter config.
  "format_on_edit": true,

  // "syntax" (tree-sitter parse) | "full" (LSP typecheck)
  "validate_on_edit": "syntax",

  // When true, write-capable commands reject paths outside project_root.
  // Defaults to false to match Pi's built-in behavior.
  "restrict_to_project_root": false,

  // Enable the trigram-indexed grep/glob (hoists them when true).
  "experimental_search_index": true,

  // Enable semantic search (aft_search). Requires ONNX runtime for local
  // embeddings; downloaded automatically on supported platforms.
  "experimental_semantic_search": true,

  // Disable specific tool names (applied after tool_surface selection).
  "disabled_tools": ["aft_transform", "aft_refactor"],

  "formatter": {
    "typescript": "biome",
    "python": "ruff",
    "rust": "rustfmt"
  },
  "checker": {
    "typescript": "biome"
  },

  // Semantic backend (when experimental_semantic_search=true).
  // "fastembed" (default, local ONNX) | "openai_compatible" | "ollama"
  "semantic": {
    "backend": "fastembed",
    "model": "all-MiniLM-L6-v2",
    "timeout_ms": 25000,
    "max_batch_size": 64
  }
}
```

Sensitive semantic backend fields (`backend`, `base_url`, `api_key_env`) are only read from **user-level** config. Project configs that try to set them are ignored with a warning to prevent credential-exfiltration via malicious repos.

### Tool surface tiers

| Tier              | Tools                                                                                                                   |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `minimal`         | `aft_outline`, `aft_zoom`, `aft_safety`                                                                                 |
| `recommended` (default) | `minimal` + hoisted `read`/`write`/`edit` + `aft_import` + `ast_grep_*` + `lsp_diagnostics` + `aft_conflicts` + (optional) `grep` + (optional) `aft_search` |
| `all`             | `recommended` + `aft_navigate` + `aft_delete` + `aft_move` + `aft_transform` + `aft_refactor`                           |

## Architecture

- **One persistent Rust process per session.** Pi loads the extension once per session; AFT spawns one `aft` binary for the session's working directory and keeps it alive. Trigram index, semantic index, tree-sitter caches, and LSP servers all stay warm.
- **NDJSON bridge.** The TypeScript extension talks to the Rust binary over stdin/stdout using a versioned JSON-RPC-style protocol.
- **Session isolation.** Pi's `session_shutdown` event triggers clean bridge shutdown — undo history, checkpoints, and LSP state don't leak across sessions.
- **Auto-download + version check.** Each plugin version pins a compatible binary version and resolves it in order: versioned cache → platform npm package → `PATH` → `~/.cargo/bin/aft` → GitHub release download. Mismatched binaries hot-swap transparently.

## Logs

Plugin logs go to `$TMPDIR/aft-pi.log`. Rust backend logs are forwarded into the same file with an `[aft]` tag.

Set `AFT_LOG_STDERR=1` to route logs to stderr instead (useful for piping or subprocess tests).

## License

MIT

---

**Main project:** https://github.com/cortexkit/aft
**Issues / feature requests:** https://github.com/cortexkit/aft/issues
