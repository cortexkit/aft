# CLI Commands

The unified `@cortexkit/aft` CLI works across every supported harness:

| Command | What it does |
|---|---|
| `npx @cortexkit/aft@latest setup` | Interactive first-time setup — auto-detects installed harnesses and registers AFT with each |
| `npx @cortexkit/aft@latest doctor` | Read-only health check across all detected harnesses (host install, plugin registration, binary cache, ONNX, config) |
| `npx @cortexkit/aft@latest doctor --fix` | Auto-fix what doctor can: register missing plugin entries, download a missing `aft` binary, repair ONNX Runtime |
| `npx @cortexkit/aft@latest doctor lsp <file>` | Show exactly which LSP servers AFT would spawn for a file, where each binary resolves, and why a server failed to start |
| `npx @cortexkit/aft@latest doctor --clear` | Interactive cache cleanup — pick which caches to clear (plugin packages, binary, LSP, semantic) |
| `npx @cortexkit/aft@latest doctor --issue` | Collect diagnostics and open a GitHub issue with sanitized logs |

Add `--harness opencode` or `--harness pi` to any command to target one harness explicitly.

---

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
file's extension, where each binary resolves (Python workspace virtualenv, project
`node_modules/.bin`, `lsp_paths_extra`, `PATH`, or not found), whether the workspace root marker
resolves walking up from the file, the spawn outcome, and the diagnostics returned (if any). Use
this when `lsp_diagnostics` returns `total: 0` and you can't tell whether the file is genuinely
clean or no server ever spawned. Pass `--harness opencode` or `--harness pi` if you have both
plugins installed and need to disambiguate. Example output:

```
$ npx @cortexkit/aft@latest doctor lsp ./python/main.py

Server attempts:
  ✗ ty
    Binary: ty (NOT FOUND in workspace virtualenv, node_modules/.bin, lsp_paths_extra, or PATH)
    Workspace root: /repo/python (markers: requirements.txt)
    Status: binary not installed
    Action: Install with `uv tool install ty` or `pip install ty`.
```

**`doctor --clear`** — Walks you through interactive cache cleanup. Useful when you're on
an old version and `@latest` doesn't seem to update (some harness installers cache npm
packages aggressively), or when you want to reset the LSP server cache to force a fresh
download. Targets harness plugin cache, binary cache, downloaded LSP servers, and semantic
index storage.

**`doctor --issue`** — Collects a full diagnostic report and sanitizes secrets, your username,
and home path out of the logs. It always writes the report to `./aft-issue-<timestamp>.md`
first and prompts you to review it. Only after you confirm does it submit via `gh` (when
installed); otherwise it opens the prefilled new-issue page in your browser. Nothing is sent
without your explicit confirmation.
