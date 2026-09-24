# @cortexkit/aft-pi

**AFT (Agent File Tools) extension for the [Pi coding agent](https://github.com/badlogic/pi-mono) and [OMP (oh-my-pi)](https://omp.sh)**

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

Pi's default `read`, `write`, `edit`, `grep`, and `bash` are replaced with AFT-backed versions. List a name in `disabled_tools` (for example `["grep"]`) to keep Pi's native tool for that slot. The AFT background-bash companions `bash_status`, `bash_watch`, `bash_write`, and `bash_kill` register independently of `bash`.

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
| `aft_outline`       | Structural outline for files or directories; with `github.read`, indexes GitHub issue and PR discussions |
| `aft_zoom`          | Symbol-level inspection with call-graph annotations; with `github.read`, drills into GitHub discussion ordinals |
| `aft_search`        | Semantic code search (embeddings, local ONNX or OpenAI-compatible)                |
| `aft_callgraph`      | Call-graph navigation: callers, call_tree, impact, trace_to, trace_data           |
| `aft_conflicts`     | One-call merge-conflict inspection across all conflicted files                    |
| `aft_import`        | Language-aware import add / remove / organize (TS, JS, Python, Rust, Go)          |
| `aft_safety`        | Per-file undo, named checkpoints, restore                                         |
| `ast_grep_search`   | AST-aware pattern search across the filesystem                                    |
| `ast_grep_replace`  | AST-aware pattern rewrite                                                         |
| `lsp_diagnostics`   | On-demand LSP diagnostics (edit/write already inline diagnostics automatically)   |
| `aft_delete`        | Delete a file with backup (surface: `all`)                                        |
| `aft_move`          | Move/rename a file (surface: `all`)                                               |

### Slash command

- `/aft-status` — show AFT version, search/semantic index state, LSP servers, storage paths

## Configure

AFT reads config from two levels, project overrides user:

- **User:** `~/.config/cortexkit/aft.jsonc`
- **Project:** `<project>/.cortexkit/aft.jsonc`

All keys are optional. Example:

```jsonc
{
  // Auto-format on write/edit using project formatter config.
  "format_on_edit": true,

  // "syntax" (tree-sitter parse) | "full" (LSP typecheck)
  "validate_on_edit": "syntax",

  // When true, write-capable commands reject paths outside project_root.
  // Defaults to false to match Pi's built-in behavior.
  "restrict_to_project_root": false,

  // Background indexes, all on by default. The local semantic backend may
  // download an ONNX runtime and model and use CPU.
  "indexes": { "trigram": true, "semantic": true, "callgraph": true },

  // Tools that are not registered. Absent => ["aft_move", "aft_delete"];
  // an explicit list replaces that default ([] enables every tool).
  "disabled_tools": ["aft_move"],

  // Pi / OMP harness options:
  "pi": {
    // "top_level" (default) | "host_default"
    // On OMP, "top_level" registers tools with loadMode: "essential" so they appear
    // directly in the model tools array. "host_default" mounts tools under xd://.
    "tool_presentation": "top_level"
  },

  "formatter": {
    "typescript": "biome",
    "python": "ruff",
    "rust": "rustfmt"
  },
  "checker": {
    "typescript": "biome"
  },

  // Missing formatter/checker/LSP warnings after configure: "toast" (default), "log", or "chat".
  "configure_warnings_delivery": "toast",

  // Semantic backend for the semantic index.
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

### Registered tools

Every AFT tool registers unless listed in `disabled_tools`: `read`, `write`, `edit`, `grep`,
`bash` and its companions, `aft_outline`, `aft_zoom`, `aft_search`, `aft_callgraph`,
`aft_inspect`, `aft_import`, `aft_safety`, `aft_conflicts`, `ast_grep_search`,
`ast_grep_replace`, `aft_delete` and `aft_move` (the last two are in the default disabled
list). Pi has no AFT `apply_patch` or `glob` tool. Index state and runtime settings never
remove a registration. `tool_surface`, `hoist_builtin_tools` and the `search_index` /
`semantic_search` keys are translated during v0.58 and rejected from v0.59.

## Architecture

- **One persistent Rust process per session.** Pi loads the extension once per session; AFT spawns one `aft` binary for the session's working directory and keeps it alive. Trigram index, semantic index, tree-sitter caches, and LSP servers all stay warm.
- **NDJSON bridge.** The TypeScript extension talks to the Rust binary over stdin/stdout using a versioned JSON-RPC-style protocol.
- **Session isolation.** Pi's `session_shutdown` event triggers clean bridge shutdown — undo history, checkpoints, and LSP state don't leak across sessions.
- **Auto-download + version check.** Each plugin version pins a compatible binary version and resolves it in order: versioned cache → platform npm package → `PATH` → `~/.cargo/bin/aft` → GitHub release download. Mismatched binaries hot-swap transparently.

## Logs

Plugin logs go to `<storage_root>/logs/aft-plugin.log` with an `[aft-pi]` tag. The file rotates at 20 MB through five retained generations; Rust module processes use adjacent `aft-<pid>.log` files.

Set `AFT_LOG_STDERR=1` to route logs to stderr instead (useful for piping or subprocess tests).

## License

MIT

---

**Main project:** https://github.com/cortexkit/aft
**Issues / feature requests:** https://github.com/cortexkit/aft/issues
