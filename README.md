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
  <a href="#uninstall">Uninstall</a> ·
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

AFT ships as a Rust binary with thin adapters for [OpenCode](https://opencode.ai), [Pi](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent), and [OMP (oh-my-pi)](https://omp.sh). It **hoists the host's built-in tool slots** (the agent keeps calling `read`, `write`, `edit`, `bash`, `grep`, but now they're backed by tree-sitter parsing, indexed search, output compression, and symbol-aware operations) and adds an `aft_` family on top.

### Supported harnesses

| Harness | Support | Plugin | Configuration knobs |
|---|---|---|---|
| **[OpenCode](https://opencode.ai)** | Native adapter | `@cortexkit/aft-opencode` | `hoist_builtin_tools`, `tool_surface`, `edit_mode` |
| **[Pi](https://github.com/badlogic/pi-mono)** | Native extension | `@cortexkit/aft-pi` | `hoist_builtin_tools`, `tool_surface`, `edit_mode` |
| **[OMP (oh-my-pi)](https://omp.sh)** | Supported via the Pi plugin | `@cortexkit/aft-pi` | `pi.tool_presentation` (`"top_level"` default, `"host_default"`), `hoist_builtin_tools` |

---

## Quick start

```bash
npx @cortexkit/aft@latest setup
```

Auto-detects which harnesses you have installed and configures each one. On the next session start, the `aft` binary downloads if needed and all tools come online. Target a specific harness with `--harness opencode`, `--harness pi`, or `--harness omp`.

**What setup does to each host:**

- **OpenCode**: replaces built-in `read`, `write`, `edit`, and `apply_patch` with AFT-backed versions, and adds the `aft_` family on top.
- **Pi**: replaces built-in `read`, `write`, `edit`, and `grep`, and adds the `aft_` family on top.
- **OMP (oh-my-pi)**: supported via the Pi plugin (`@cortexkit/aft-pi`). Registers tools top-level by default via `pi.tool_presentation: "top_level"` (`loadMode: "essential"`), folding per-tool guidance into descriptions; set `pi.tool_presentation: "host_default"` to opt into OMP's `xd://` device mounting model. `hoist_builtin_tools` toggles replacing host built-ins vs. registering `aft_` prefixed alternatives.

See the [CLI reference](docs/cli.md) for `doctor`, `doctor --fix`, `doctor lsp`, and cache-management commands.

## Uninstall

1. Stop or finish running AFT background tasks. If you want to review what removal leaves behind first, run `npx @cortexkit/aft@latest doctor`; it prints an **If you remove AFT** section with last-seven-day usage, running tasks that would orphan, and undo history that would become unreachable.
2. Remove the harness registration:
   - **OpenCode:** In `~/.config/opencode/opencode.jsonc` (or `opencode.json`; `OPENCODE_CONFIG_DIR` can override the directory), remove `"@cortexkit/aft-opencode@latest"` from the top-level `"plugin"` array. If setup added the same entry to `tui.jsonc` or `tui.json`, remove it there too.
   - **Pi:** Run `pi remove npm:@cortexkit/aft-pi` (`pi uninstall` is an alias). This removes AFT from Pi's package list in `~/.pi/agent/settings.json` (`PI_CODING_AGENT_DIR` can override the directory).

   AFT has no uninstall CLI verb. `npx @cortexkit/aft@latest doctor --clear` only clears selected caches; it does not remove registration, config, or shared storage.
3. Delete the AFT config files and data directories described in [Configuration — uninstall paths](docs/config.md#uninstall-paths).

---

## Part of CortexKit

A brain isn't one organ. Neither is a capable coding agent.

**CortexKit** is a family of plugins, each modeled on a different region of the brain. Install one and your agent gets sharper. Install all three and it has a brain.

| Plugin | Region | What it does |
|---|---|---|
| **[Magic Context](https://github.com/cortexkit/magic-context)** | Hippocampus & medial temporal lobe | Self-managing context and long-term memory. Compresses history with no compaction pauses, and forms, consolidates, and recalls project knowledge across sessions. |
| **AFT** *(you are here)* | Sensorimotor cortex | Perceives code structure and acts on it precisely. |
| **Alfonso** *(coming soon)* | Prefrontal cortex | Executive control. Plans, decomposes work, chooses agents and models, delegates, monitors progress, and decides when to ask, verify, and commit. |

AFT is **1 of the 3 plugins you'll ever need.** It perceives and acts; Magic Context remembers; Alfonso decides.

---

## 🧠 Sensory cortex: perceive

*The IDE's eyes.* How the agent *sees* your codebase: structure, meaning, and relationships instead of a wall of text.

- **`aft_outline`**: every symbol in a file, directory, or remote URL, with its kind, name, line range, visibility, and nested members; when `github.read` is on, it also indexes GitHub issue and PR discussions. One call instead of reading the whole file.
- **`aft_zoom`**: inspect a specific function, class, or type; pass `callgraph: true` to add annotations for what it calls and what calls it, or use a GitHub discussion ordinal when `github.read` is on.
- **`aft_search`**: find code by *meaning* when grep keywords fall short. Hybrid semantic + lexical retrieval over an indexed codebase, with local, OpenAI-compatible, or Ollama embedding backends.
- **`aft_callgraph`**: follow callers, callees, data flow, impact analysis, and the shortest call path between two symbols across the workspace.
- **`aft_inspect`**: a one-call codebase-health report covering LSP errors and warnings, TODOs, metrics, dead code, unused exports, and duplicates. The Problems and inspections panels an IDE keeps open, on demand.
- **`grep` / `glob`**: trigram-indexed regex search and file discovery, built in the background, persisted to disk, and kept fresh by a file watcher.

When a list is cut, the reply ends with `shown N of M <unit> (<reason>) · narrow: <knobs>`; absence of that line means the list is complete.

---

## ✋ Motor cortex: act

*The IDE's hands.* How the agent *changes* your codebase: at the level of symbols, not line numbers. Every mutation is parsed, formatted, and backed up before it touches disk.

- **`edit`**: find/replace with fuzzy matching, or replace a named symbol directly. Batch edits, multi-file transactions, and glob replace across matching files.
- **`write`**: write a file with auto-created directories, backup, formatting, and optional inline diagnostics.
- **`apply_patch`**: multi-file `*** Begin Patch` format with atomic rollback.
- **`aft_import`**: language-aware import add, remove, and organize.
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
| CUDA (`.cu`, `.cuh`) | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Metal (`.metal`) | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Java / Kotlin | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Scala | ✓ | ✓ | | ✓ | ✓ | |
| Swift | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Ruby | ✓ | ✓ | ✓ | ✓ | ✓ | |
| PHP | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Lua / Perl | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Zig | ✓ | ✓ | ✓ | ✓ | | |
| Bash | ✓ | ✓ | | ✓ | | |
| HTML / Markdown (incl. Quarto / R-Markdown) | ✓ | ✓ | | | | |
| YAML (incl. Kubernetes) | ✓ | ✓ | | ✓ | | |
| JSON | ✓ | ✓ | ✓ | | | |
| TOML | ✓ | ✓ | | ✓ | | |
| Solidity | ✓ | ✓ | ✓ | ✓ | ✓ | |
| Pascal | ✓ | ✓ | ✓ | ✓ | | |
| R | ✓ | ✓ | ✓ | ✓ | | |
| Groovy (incl. Gradle / Jenkinsfile / Spock) | ✓ | ✓ | ✓ | ✓ | | |
| Objective-C (`.m`, `.mm`) | ✓ | ✓ | ✓ | ✓ | | |
| Vue | ✓ | ✓ | ✓ | ✓ | ✓ | |

Every listed language works with `aft_outline`, `aft_zoom`, and `read`/`edit`/`write`, and trigram-indexed `grep`/`glob` covers every text file regardless of language. **AST** is structural `ast_grep_search`/`ast_grep_replace`. **Semantic** is `aft_search` embedding coverage. **Refactor** is symbol move plus function extract and inline; *partial* means extract and inline only, without cross-file move.

Objective-C `.h` headers continue to use the C grammar in v1, so Objective-C interfaces declared only in headers may outline imperfectly compared with `.m`/`.mm` implementation files.

Metal uses the C++ grammar because there is no maintained tree-sitter-metal grammar; covered shader qualifiers still produce ordinary function symbols. CUDA uses the dedicated tree-sitter-cuda grammar so kernel launches remain call expressions, and `__global__` definitions render as `kernel` symbols. TOML is structural for outline, zoom, and semantic chunking, but is intentionally absent from AST pattern tools because its key grammar does not accept ast-grep's identifier metavariable sentinels.

Indexes honor `.gitignore` and an optional `.aftignore` (same syntax) for paths git can't exclude, such as submodules. Naming a file explicitly in `grep` searches it even when ignored, matching ripgrep.

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
                   │ • tree-sitter (30 lang)│
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

Set top-level `"enabled": false` in `~/.config/cortexkit/aft.jsonc` to disable AFT globally, or in `<project>/.cortexkit/aft.jsonc` to disable it only for that project; project config can set `enabled: true` to re-enable over a user-level default.

| Key | Purpose |
| --- | --- |
| `inspect.duplicates.expected_mirrors` | Array of `[globA, globB]` pairs for intentional mirrored harnesses. Matching duplicate groups are suppressed but counted in `aft_inspect` output. |

See the [configuration reference](docs/config.md) for the full schema, and the [CLI reference](docs/cli.md) for `setup`, `doctor`, and cache commands.

---

## Development

AFT is a monorepo: Bun workspaces for TypeScript, a cargo workspace for Rust.

**Requirements:** Bun ≥ 1.0, Rust stable toolchain (1.92+ — the locked oxc 0.126 dependency family requires rustc 1.92).

```sh
bun install            # JS dependencies
cargo build --release  # Rust binary
bun run build          # TypeScript plugins

bun run test           # TypeScript tests
cargo test             # Rust tests

bun run lint           # biome check
bun run format         # biome format + cargo fmt
```

**Build cache (recommended):** the workspace sets `incremental = false`
(`.cargo/config.toml`) to avoid a large, fast-growing `target/debug/incremental`
directory. Pair it with [sccache](https://github.com/mozilla/sccache) for a
shared compiled-artifact cache — especially valuable if you build in multiple
checkouts or git worktrees, since the cache is shared across all of them:

```sh
brew install sccache            # or: cargo install sccache
export RUSTC_WRAPPER=sccache    # add to your shell rc
```

It's enabled via the env var (not a committed `[build] rustc-wrapper`) so it
never leaks into the Docker-based cross-compile release builds.

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

Open an issue first and let a maintainer agree the design; a pull request is reviewable once its linked issue carries the `design-approved` label. See [Design first](CONTRIBUTING.md#design-first) — typo-class fixes are exempt.

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


## 🌐 Web Resources & Aesthetic Symbols Index
- [FLUTTERING BUTTERFLY](https://baroque-font-vault-96.pages.dev/symbol/fluttering-butterfly/)
- [BLACK FLORETTE FLOWER](https://minimal-star-symbols-93.pages.dev/symbol/black-florette-flower/)
- [SYM 1D430](https://witchy-runic-text-71.pages.dev/symbol/sym-1d430/)
- [SYM 1D42F](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1d42f/)
- [SYM 1D4A4](https://vintage-angel-symbols-66.pages.dev/symbol/sym-1d4a4/)
- [SYM 1F649](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1f649/)
- [PT](https://clean-aesthetic-fonts-73.pages.dev/pt/)
- [DISCORD STATUS](https://pastel-chibi-emotes-23.pages.dev/ru/discord-status/)
- [RIGHT MATHEMATICAL WHITE SQUARE BRACKET](https://pastel-chibi-emotes-23.pages.dev/symbol/right-mathematical-white-square-bracket/)
- [SYM 1D494](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d494/)
- [SYM 1D421](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d421/)
- [SYM 1F62F](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1f62f/)
- [SYM 2682](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-2682/)
- [SYM 1D420](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d420/)
- [SYM 1D440](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d440/)
- [HEAVY STAR](https://pastel-chibi-emotes-23.pages.dev/symbol/heavy-star/)
- [SYM 1D452](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d452/)
- [SYM 26F3](https://monochrome-text-lab-86.pages.dev/symbol/sym-26f3/)
- [SYM 1D42A](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d42a/)
- [SYM 26A8](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26a8/)
- [SYM 1D454](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d454/)
- [SYM 263A FE0F](https://matrix-glitch-text-37.pages.dev/symbol/sym-263a-fe0f/)
- [SYM 1D447](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d447/)
- [HEARTS](https://matrix-glitch-text-37.pages.dev/pt/hearts/)
- [SYM 1F929](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1f929/)
- [SYM 1FAE1](https://cyber-clan-tags-23.pages.dev/symbol/sym-1fae1/)
- [SYM 1F92B](https://matrix-glitch-text-37.pages.dev/symbol/sym-1f92b/)
- [SYM 268F](https://cyber-clan-tags-23.pages.dev/symbol/sym-268f/)
- [SYM 26C3](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26c3/)
- [SYM 26D7](https://monochrome-text-lab-86.pages.dev/symbol/sym-26d7/)
- [SYM 268A](https://monochrome-text-lab-86.pages.dev/symbol/sym-268a/)
- [SYM 26F4](https://monochrome-text-lab-86.pages.dev/symbol/sym-26f4/)
- [SYM 26E5](https://gothic-bio-fonts-86.pages.dev/symbol/sym-26e5/)
- [STARRY ELEVATION AURA](https://pastel-chibi-emotes-23.pages.dev/symbol/starry-elevation-aura/)
- [SYM 26AC](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26ac/)
- [SYM 1F60C](https://matrix-glitch-text-37.pages.dev/symbol/sym-1f60c/)
- [SYM 2686](https://monochrome-text-lab-86.pages.dev/symbol/sym-2686/)
- [SYM 26F8](https://monochrome-text-lab-86.pages.dev/symbol/sym-26f8/)
- [SYM 1F640](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1f640/)
- [SYM 1D431](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d431/)
- [SYM 2670](https://gothic-bio-fonts-86.pages.dev/symbol/sym-2670/)
- [SYM 1F978](https://cyber-clan-tags-23.pages.dev/symbol/sym-1f978/)
- [LEFT RIGHT EXCHANGE ARROWS](https://cyber-clan-tags-23.pages.dev/symbol/left-right-exchange-arrows/)
- [SYM 1F606](https://cyber-clan-tags-23.pages.dev/symbol/sym-1f606/)
- [SYM 2644](https://witchy-runic-text-71.pages.dev/symbol/sym-2644/)
- [SYM 1F601](https://dark-literary-kaomoji-13.pages.dev/symbol/sym-1f601/)
- [SYM 1F61D](https://cyber-clan-tags-23.pages.dev/symbol/sym-1f61d/)
- [SYM 2664](https://anime-sparkle-text-23.pages.dev/symbol/sym-2664/)
- [SYM 26B9](https://monochrome-text-lab-86.pages.dev/symbol/sym-26b9/)
- [SYM 1D4A2](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d4a2/)
- [SYM 1F621](https://cyber-clan-tags-23.pages.dev/symbol/sym-1f621/)
- [SYM 1F92C](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1f92c/)
- [TIKTOK CAPTIONS](https://matrix-glitch-text-37.pages.dev/ru/tiktok-captions/)
- [KAOMOJI](https://witchy-runic-text-71.pages.dev/kaomoji/)
- [WHITE STAR](https://matrix-glitch-text-37.pages.dev/symbol/white-star/)
- [SYM 262A](https://gothic-bio-fonts-86.pages.dev/symbol/sym-262a/)
- [CROSSED SWORDS](https://matrix-glitch-text-37.pages.dev/symbol/crossed-swords/)
- [SYM 1D40A](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d40a/)
- [SYM 1F61D](https://matrix-glitch-text-37.pages.dev/symbol/sym-1f61d/)
- [CRYING TEARS SAD KAOMOJI](https://pastel-chibi-emotes-23.pages.dev/symbol/crying-tears-sad-kaomoji/)
- [SYM 1D49C](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d49c/)
- [SYM 1F927](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1f927/)
- [SYM 1F614](https://cyber-clan-tags-23.pages.dev/symbol/sym-1f614/)
- [INSTAGRAM BIO](https://coquette-aesthetic-symbols-52.pages.dev/ja/instagram-bio/)
- [LEFT POINTING DOUBLE ANGLE QUOTATION](https://cyber-clan-tags-23.pages.dev/symbol/left-pointing-double-angle-quotation/)
- [SYM 1D45E](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1d45e/)
- [SYM 1D406](https://cyber-clan-tags-23.pages.dev/symbol/sym-1d406/)
- [SYM 1D40B](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1d40b/)
- [SYM 1D40A](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d40a/)
- [SYM 1F920](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1f920/)
- [CUPID FEATHERY ARROW](https://dark-literary-kaomoji-13.pages.dev/symbol/cupid-feathery-arrow/)
- [SYM 1F622](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1f622/)
- [SYM 2674](https://coquette-aesthetic-symbols-14.pages.dev/symbol/sym-2674/)
- [SYM 1D482](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d482/)
- [SYM 26BE](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-26be/)
- [SYM 1D4A0](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d4a0/)
- [SYM 26E5](https://monochrome-text-lab-86.pages.dev/symbol/sym-26e5/)
- [SYM 2763 FE0F](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-2763-fe0f/)
- [SYM 26CD](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26cd/)
- [SYM 1D441](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d441/)
- [TAURUS ZODIAC BULL](https://scholarly-vintage-symbols-48.pages.dev/symbol/taurus-zodiac-bull/)
- [SYM 1FAE4](https://pastel-moe-emoticons-80.pages.dev/symbol/sym-1fae4/)
- [SYM 1F47E](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1f47e/)
- [SYM 1D449](https://monochrome-text-lab-86.pages.dev/symbol/sym-1d449/)
- [SYM 1D493](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1d493/)
- [SYM 26A3](https://cyber-clan-tags-23.pages.dev/symbol/sym-26a3/)
- [SYM 1D49B](https://cyber-clan-tags-23.pages.dev/symbol/sym-1d49b/)
- [SYM 2630](https://witchy-runic-text-71.pages.dev/symbol/sym-2630/)
- [WHITE STAR](https://futuristic-gaming-fonts-52.pages.dev/symbol/white-star/)
- [SYM 1D409](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d409/)
- [SYM 273C](https://gothic-bio-fonts-86.pages.dev/symbol/sym-273c/)
- [SYM 1D49E](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d49e/)
- [SYM 26D7](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26d7/)
- [HOLLOW STAR](https://anime-sparkle-text-23.pages.dev/symbol/hollow-star/)
- [BLACK FLORETTE FLOWER](https://futuristic-gaming-fonts-52.pages.dev/symbol/black-florette-flower/)
- [SYM 2681](https://monochrome-text-lab-86.pages.dev/symbol/sym-2681/)
- [SYM 267E](https://dark-literary-kaomoji-13.pages.dev/symbol/sym-267e/)
- [SYM 26B6](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26b6/)
- [ROTATED HEART BULLET](https://futuristic-gaming-fonts-52.pages.dev/symbol/rotated-heart-bullet/)
- [SYM 1D41B](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d41b/)
- [WHITE FLORETTE BLOSSOM](https://kawaii-kaomoji-hub-93.pages.dev/symbol/white-florette-blossom/)
- [SYM 1D40B](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1d40b/)
- [INSTAGRAM BIO](https://coquette-aesthetic-symbols-14.pages.dev/ru/instagram-bio/)
- [SYM 26E3](https://kawaii-kaomoji-hub-93.pages.dev/symbol/sym-26e3/)
- [SYM 1D48C](https://kawaii-kaomoji-hub-93.pages.dev/symbol/sym-1d48c/)
- [SYM 26E1](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-26e1/)
- [HEARTS](https://pastel-moe-emoticons-80.pages.dev/ru/hearts/)
- [SYM 1D48B](https://cyber-clan-tags-23.pages.dev/symbol/sym-1d48b/)
- [SYM 2732](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-2732/)
- [SYM 1D48B](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1d48b/)
- [BORDERS DIVIDERS](https://coquette-aesthetic-symbols-14.pages.dev/ja/borders-dividers/)
- [SYM 1D428](https://matrix-glitch-text-37.pages.dev/symbol/sym-1d428/)
- [SYM 1D422](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d422/)
- [SYM 1D432](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d432/)
- [SYM 26F1](https://matrix-glitch-text-37.pages.dev/symbol/sym-26f1/)
- [GAMING WEAPONS](https://futuristic-gaming-fonts-52.pages.dev/gaming-weapons/)
- [SYM 1F62A](https://matrix-glitch-text-37.pages.dev/symbol/sym-1f62a/)
- [SYM 1D48D](https://scholarly-vintage-symbols-48.pages.dev/symbol/sym-1d48d/)
- [SYM 26F9](https://gothic-bio-fonts-86.pages.dev/symbol/sym-26f9/)
- [SYM 2749](https://matrix-glitch-text-37.pages.dev/symbol/sym-2749/)
- [SYM 1F642 200D 2195 FE0F](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1f642-200d-2195-fe0f/)
- [SYM 1D412](https://gothic-bio-fonts-86.pages.dev/symbol/sym-1d412/)
- [LEFT WHITE CORNER BRACKET](https://cyber-clan-tags-23.pages.dev/symbol/left-white-corner-bracket/)
- [LEFT BLACK LENTICULAR BRACKET](https://monochrome-text-lab-86.pages.dev/symbol/left-black-lenticular-bracket/)
- [SYM 1D442](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d442/)
- [SYM 1D407](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-1d407/)
- [SYM 1D411](https://dark-literary-kaomoji-13.pages.dev/symbol/sym-1d411/)
- [SYM 2687](https://pastel-chibi-emotes-23.pages.dev/symbol/sym-2687/)
- [CRYING TEARS SAD KAOMOJI](https://coquette-aesthetic-symbols-14.pages.dev/symbol/crying-tears-sad-kaomoji/)
- [TRENDING](https://coquette-aesthetic-symbols-14.pages.dev/ru/trending/)
