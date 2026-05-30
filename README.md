<h1 align="center">AFT</h1>

<p align="center">
  <strong>Give your agent a proper IDE and OS.</strong><br>
  The sensorimotor cortex for coding agents. <br>
</p>

<!-- BANNER: replace with the new cortex/family banner (see banner prompts). Path is repo-relative for the final location. -->
<p align="center">
  <img src="assets/aft_banner.jpg" alt="AFT, the sensorimotor cortex for coding agents" width="80%">
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
  <a href="#what-is-aft">What is AFT?</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#part-of-cortexkit">CortexKit</a> ·
  <a href="#-sensory-cortex-perceive">Sensory</a> ·
  <a href="#-motor-cortex-act">Motor</a> ·
  <a href="#-brainstem-keep-it-alive">Brainstem</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="https://discord.gg/DSa65w8wuf">💬 Discord</a>
</p>

---

## What is AFT?

You give yourself the best tools for the job: an IDE that shows you the whole codebase at a glance, the fastest terminal you can find, an operating system that runs a dozen things at once so you never wait on a single task to finish.

Then you hand your agent `read`, `edit`, and raw `bash`, and wonder why it burns tokens on whole-file reads and breaks edits the moment a line moves.

AFT gives it the real thing. It sits between an agent's reasoning and your codebase as a **sensorimotor cortex**, the part of the brain wired to perception and action:

- **Sensory cortex: perceive.** Outline a file, zoom into one symbol, search by meaning, follow a call graph. The agent sees *structure* instead of scrolling text.
- **Motor cortex: act.** Edit a function by name, refactor across the workspace, organize imports. Every change is parsed, validated, formatted, and backed up by the binary.
- **Brainstem: stay alive.** Background bash tasks, PTY sessions, and compressed output keep the agent's environment running without it having to think about it. On-demand health checks and an undo stack keep the codebase healthy and recoverable when something does go wrong.

Sensory and motor make the **IDE**; the brainstem is the **OS**. Your agent gets both.

**Increase productivity. Decrease token usage.**

AFT ships as a Rust binary with thin adapters for [OpenCode](https://opencode.ai) and [Pi](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent). It **hoists the host's built-in tool slots** (the agent keeps calling `read`, `write`, `edit`, `bash`, `grep`, but now they're backed by tree-sitter parsing, indexed search, output compression, and symbol-aware operations) and adds an `aft_` family on top.

---

## Quick start

```bash
npx @cortexkit/aft setup
```

Auto-detects which harnesses you have installed and configures each one. On the next session start, the `aft` binary downloads if needed and all tools come online. Target a specific harness with `--harness opencode` or `--harness pi`.

**What setup does to each host:**

- **OpenCode**: replaces built-in `read`, `write`, `edit`, and `apply_patch` with AFT-backed versions, and adds the `aft_` family on top.
- **Pi**: replaces built-in `read`, `write`, `edit`, and `grep`, and adds the `aft_` family on top.

See the [CLI reference](docs/cli.md) for `doctor`, `doctor --fix`, `doctor lsp`, and cache-management commands.

---

## Before / after

**Without AFT**, to change one function:

```text
read src/server.ts          → the whole file lands in context
(reconstruct the edit from memory)
edit by line 247            → breaks the moment the file shifted
```

**With AFT:**

```text
aft_outline src/server.ts   → every symbol, about one screen
aft_zoom handleRequest      → just that function + who calls it
edit symbol: handleRequest  → replaced by name, auto-formatted, backed up
```

Read one function, not the whole file. Edit by name, not by line number. Tens of tokens where the old loop spent hundreds, and the edit doesn't rot when the file moves.

---

## Part of CortexKit

A brain isn't one organ. Neither is a capable coding agent.

**CortexKit** is a family of plugins, each modeled on a different region of the brain. Install one and your agent gets sharper. Install all three and it has a brain.

| Plugin | Region | What it does |
|---|---|---|
| **[Magic Context](https://github.com/cortexkit/magic-context)** | Hippocampus & medial temporal lobe | Long-term project memory. Compresses episodes into compartments, promotes stable facts, recalls prior context, tracks how knowledge changes over time. |
| **AFT** *(you are here)* | Sensorimotor cortex | Perceives code structure and acts on it precisely. |
| **Alfonso** *(coming soon)* | Prefrontal cortex | Executive control. Plans, decomposes work, chooses agents and models, delegates, monitors progress, and decides when to ask, verify, and commit. |

AFT is **1 of the 3 plugins you'll ever need.** It perceives and acts; Magic Context remembers; Alfonso decides.

---

## 🧠 Sensory cortex: perceive

*The IDE's eyes.* How the agent *sees* your codebase: structure, meaning, and relationships instead of a wall of text.

- **`aft_outline`**: every symbol in a file, directory, or remote URL, with its kind, name, line range, visibility, and nested members. One call instead of reading the whole file.
- **`aft_zoom`**: inspect a specific function, class, or type with call-graph annotations for what it calls and what calls it.
- **`aft_search`**: find code by *meaning* when grep keywords fall short. Hybrid semantic + lexical retrieval over an indexed codebase, with local, OpenAI-compatible, or Ollama embedding backends.
- **`aft_callgraph`**: follow callers, callees, data flow, impact analysis, and the shortest call path between two symbols across the workspace.
- **`aft_inspect`**: a one-call codebase-health report covering LSP errors and warnings, TODOs, metrics, dead code, unused exports, and duplicates. The Problems and inspections panels an IDE keeps open, on demand.
- **`grep` / `glob`**: trigram-indexed regex search and file discovery, built in the background, persisted to disk, and kept fresh by a file watcher.

---

## ✋ Motor cortex: act

*The IDE's hands.* How the agent *changes* your codebase: at the level of symbols, not line numbers. Every mutation is parsed, formatted, and backed up before it touches disk.

- **`edit`**: find/replace with fuzzy matching, or replace a named symbol directly. Batch edits, multi-file transactions, and glob replace across matching files.
- **`write`**: write a file with auto-created directories, backup, formatting, and optional inline diagnostics.
- **`apply_patch`**: multi-file `*** Begin Patch` format with atomic rollback.
- **`aft_refactor`**: workspace-wide symbol move (updates every import), function extraction, and inlining.
- **`aft_import`**: language-aware import add, remove, and organize.
- **`aft_transform`**: structural transforms such as adding class members, Rust derives, Python decorators, Go struct tags, or wrapping a body in try/catch.
- **`ast_grep_search` / `ast_grep_replace`**: structural search and replace using AST patterns with meta-variables.

---

## ⚙️ Brainstem: keep it alive

*The OS.* The autonomic layer. Long-running work, noisy output, and recovery, handled without the agent's attention.

- **`bash`**: unified shell execution with command rewriting (`cat`→`read`, `grep`→grep tool), per-command output compression, and tree-sitter permission scanning (OpenCode).
- **Background tasks**: spawn detached work with `background: true`, inspect with `bash_status`, kill with `bash_kill`, and block or watch for output with `bash_watch`. Tasks and their completions survive restarts.
- **Output compression**: multi-tier compression turns firehose CLI output (test runners, installers, `docker ps`, `kubectl`) into the few lines that actually matter, keeping errors and summaries while dropping the noise.
- **PTY**: real interactive terminal sessions for REPLs and terminal apps (python, node, vim, even a nested agent). Drive them with `bash_write`, inspect rendered screen state with `bash_status`.
- **`aft_safety`**: per-file undo stack, named checkpoints, and restore. Every edit is backed up to disk and survives bridge and host restarts.

---

## Benchmarks

A full, reproducible benchmark suite is in progress: search latency, retrieval quality, bash-output token reduction, and end-to-end agent task success against other code-context plugins. We'll publish numbers here once the methodology is locked and the harnesses are reproducible from a clean checkout.

_Coming soon._

---

## Supported languages

| Language | Outline | Edit | AST | Semantic | Imports | Refactor |
|----------|---------|------|-----|----------|---------|---------|
| TypeScript / TSX | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| JavaScript / JSX | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Python | ✓ | ✓ | ✓ | ✓ | ✓ | partial |
| Rust | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Go | ✓ | ✓ | ✓ | ✓ | ✓ | |
| C / C++ / C# | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Java / Kotlin | ✓ | ✓ | ✓ | | ✓ | |
| Scala | ✓ | ✓ | | | ✓ | |
| Swift | ✓ | ✓ | ✓ | | ✓ | |
| Ruby | ✓ | ✓ | ✓ | | ✓ | |
| PHP | ✓ | ✓ | ✓ | | ✓ | |
| Lua / Perl | ✓ | ✓ | ✓ | | ✓ | |
| Zig | ✓ | ✓ | ✓ | ✓ | | |
| Bash | ✓ | ✓ | | ✓ | | |
| HTML / Markdown | ✓ | ✓ | | | | |
| YAML (incl. Kubernetes) | ✓ | ✓ | — | — |
| JSON | ✓ | ✓ | ✓ | | | |
| Solidity | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Vue | ✓ | ✓ | ✓ | ✓ | ✓ | |

Every listed language works with `aft_outline`, `aft_zoom`, and `read`/`edit`/`write`, and trigram-indexed `grep`/`glob` covers every text file regardless of language. **AST** is structural `ast_grep_search`/`ast_grep_replace`. **Semantic** is `aft_search` embedding coverage. **Refactor** is symbol move plus function extract and inline; *partial* means extract and inline only, without cross-file move.

Indexes honor `.gitignore` and an optional `.aftignore` (same syntax) for paths git can't exclude, such as submodules. Naming a file explicitly in `grep` searches it even when ignored, matching ripgrep.

**YAML / Kubernetes / CRDs.** YAML files are semantically indexed. Each document carrying `apiVersion` + `kind` (the Kubernetes/CRD contract, so this generalizes to any custom resource) becomes one rich symbol named `<namespace>/<Kind>/<name>` (falling back to `metadata.generateName` when no name is set). The symbol's embed text is enriched with high-signal spec fields so intent queries match: container `image`/ports, resource `cpu`/`memory` limits and requests, `volumeMounts`, env var names, RBAC `verbs`/`resources`/`apiGroups`, and Argo Workflow `entrypoint`/`templates`/`command`/`schedule`. Multi-document (`---`) streams are handled per document; plain non-k8s YAML (docker-compose, CI configs, Helm `values.yaml`) falls back to top-level keys as symbols.
---

## Architecture

AFT is a Rust binary driven by thin adapter packages per harness. The binary speaks a simple JSON-over-stdio request/response protocol. One process per project root stays alive for the project's lifetime, shared across sessions on that root, keeping parse trees warm while each session keeps its own isolated undo history.

```
   ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
   │  OpenCode   │    │     Pi      │    │  Future…    │
   │   agent     │    │   agent     │    │  (MCP, …)   │
   └──────┬──────┘    └──────┬──────┘    └──────┬──────┘
           │ tool calls       │ tool calls       │
           ▼                  ▼                  ▼
   ┌──────────────┐   ┌──────────────┐   ┌──────────────┐
   │ aft-opencode │   │   aft-pi     │   │     …        │  ← thin adapters per harness
   │  (TS plugin) │   │  (TS plugin) │   │              │    Hoist tools, manage
   └──────┬───────┘   └──────┬───────┘   └──────┬───────┘    BridgePool, resolve binary
           │                  │                  │
           └──────────────────┴──────────────────┘
                              │
                              │ JSON-over-stdio
                              ▼
                   ┌────────────────────────┐
                   │     aft binary         │  ← shared core
                   │       (Rust)           │
                   ├────────────────────────┤
                   │ • tree-sitter (24 lang)│
                   │ • symbols & call graph │
                   │ • diff/format/backup   │
                   │ • LSP client           │
                   │ • trigram index        │
                   │ • semantic index       │
                   └────────────────────────┘
```

Per-harness adapters **hoist** the host's built-in tool slots and register AFT-only tools, **manage a BridgePool** (one persistent `aft` process per project root, shared across sessions), **resolve the binary** (cache → npm platform package → PATH → cargo install → GitHub release), and **translate** between the host's plugin API and AFT's protocol.

AFT data lives under a shared CortexKit storage root (`~/.local/share/cortexkit/aft/`). Backups, search indexes, and downloaded LSP servers persist there across sessions.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full layer map and the [tool reference](docs/tools.md) for every tool.

---

## Configuration

AFT works out of the box; everything below is optional. Configure it via `aft.jsonc` at the user or project level: tool surface, semantic-search backend, LSP servers, bash compression, and more.

See the [configuration reference](docs/config.md) for the full schema, and the [CLI reference](docs/cli.md) for `setup`, `doctor`, and cache commands.

---

## Development

AFT is a monorepo: Bun workspaces for TypeScript, a cargo workspace for Rust.

**Requirements:** Bun ≥ 1.0, Rust stable toolchain (1.82+).

```sh
bun install            # JS dependencies
cargo build --release  # Rust binary
bun run build          # TypeScript plugins

bun run test           # TypeScript tests
cargo test             # Rust tests

bun run lint           # biome check
bun run format         # biome format + cargo fmt
```

**Project layout:**

```
opencode-aft/
├── crates/
│   └── aft/              # Rust binary, shared core (tree-sitter, search, LSP, etc.)
├── packages/
│   ├── aft-cli/          # Unified CLI (@cortexkit/aft), setup/doctor across all harnesses
│   ├── opencode-plugin/  # OpenCode adapter (@cortexkit/aft-opencode)
│   ├── pi-plugin/        # Pi adapter (@cortexkit/aft-pi)
│   └── npm/              # Platform-specific binary packages
└── scripts/              # Release + version-sync tooling
```

---

## Contributing

Pull requests for bugs are welcome. For features or broader fixes that need architectural changes, please open an issue first to discuss the approach.

Adding a command means implementing it in Rust (`crates/aft/src/commands/`) and adding a tool definition in each harness adapter (`packages/opencode-plugin/src/tools/`, `packages/pi-plugin/src/tools/`). Run `bun run format` and `cargo fmt` before submitting; CI rejects unformatted code.

---

## License

[MIT](LICENSE)

---

## Documentation

- [Tool reference](docs/tools.md): complete documentation for every tool
- [Configuration](docs/config.md): config schema, LSP, auto-install
- [CLI commands](docs/cli.md): setup, doctor, and cache management
- [Benchmarks](docs/benchmarks.md): search-index methodology *(numbers being finalized)*
