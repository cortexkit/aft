# Codebase Structure

## Directory Layout

```text
opencode-aft/
├── crates/                    # Rust workspace packages
│   └── aft/                   # Core AFT library, CLI binary, command handlers, and integration tests
├── packages/                  # JavaScript workspace packages
│   ├── opencode-plugin/       # OpenCode plugin that exposes and hoists AFT tools
│   └── npm/                   # Platform-specific npm binary packages
├── benchmarks/                # Bun-based benchmark runner and reporting code
├── scripts/                   # Release and version-management scripts
├── assets/                    # Repository assets such as the banner image
├── .github/workflows/         # Release automation workflows
├── Cargo.toml                 # Rust workspace manifest
├── package.json               # JavaScript workspace manifest
└── README.md                  # User-facing product and tool reference
```

## Directory Purposes

**`crates/aft/`:**
- Purpose: Keep the Rust execution engine, stdin/stdout protocol binary, and shared analysis logic together.
- Contains: `src/` Rust modules, `tests/` integration suites, crate manifest
- Key files: `crates/aft/src/main.rs`, `crates/aft/src/lib.rs`, `crates/aft/src/commands/`, `crates/aft/tests/integration/`

**`crates/aft/src/commands/`:**
- Purpose: Add one handler file per protocol command.
- Contains: Command-specific request parsing and response generation
- Key files: `crates/aft/src/commands/read.rs`, `crates/aft/src/commands/write.rs`, `crates/aft/src/commands/outline.rs`, `crates/aft/src/commands/conflicts.rs`

**`crates/aft/src/lsp/`:**
- Purpose: Keep LSP client, transport, registry, and diagnostics state separate from command handlers.
- Contains: LSP lifecycle modules and supporting types
- Key files: `crates/aft/src/lsp/manager.rs`, `crates/aft/src/lsp/client.rs`, `crates/aft/src/lsp/diagnostics.rs`

**`packages/opencode-plugin/`:**
- Purpose: Ship the OpenCode-facing package that resolves the binary and registers tools.
- Contains: `src/` TypeScript sources, `dist/` build output, tests, package manifest
- Key files: `packages/opencode-plugin/src/index.ts`, `packages/opencode-plugin/src/bridge.ts`, `packages/opencode-plugin/package.json`

**`packages/opencode-plugin/src/tools/`:**
- Purpose: Group OpenCode tool definitions by capability area.
- Contains: Thin adapters for hoisted, reading, import, structure, navigation, refactor, safety, AST, LSP, and conflict tools
- Key files: `packages/opencode-plugin/src/tools/hoisted.ts`, `packages/opencode-plugin/src/tools/reading.ts`, `packages/opencode-plugin/src/tools/refactoring.ts`

**`packages/opencode-plugin/src/__tests__/`:**
- Purpose: Verify plugin behavior, resolver logic, tool registration, and end-to-end bridge flows.
- Contains: Unit tests and `e2e/` test fixtures
- Key files: `packages/opencode-plugin/src/__tests__/tools.test.ts`, `packages/opencode-plugin/src/__tests__/structure.test.ts`, `packages/opencode-plugin/src/__tests__/e2e/`

**`packages/npm/`:**
- Purpose: Publish one npm package per target platform so the plugin can resolve a bundled binary.
- Contains: Per-platform package manifests and `bin/` payload directories
- Key files: `packages/npm/darwin-arm64/package.json`, `packages/npm/linux-x64/package.json`, `packages/npm/win32-x64/package.json`

**`benchmarks/`:**
- Purpose: Run benchmark scenarios and post-process benchmark output with Bun.
- Contains: Benchmark source files, configs, cached results, package manifest
- Key files: `benchmarks/src/runner.ts`, `benchmarks/src/analyze.ts`, `benchmarks/package.json`

**`scripts/`:**
- Purpose: Automate release, validation, and version synchronization tasks.
- Contains: Shell and Node scripts
- Key files: `scripts/release.sh`, `scripts/version-sync.mjs`, `scripts/validate-packages.mjs`

## Key File Locations

**Entry Points:** `packages/opencode-plugin/src/index.ts`: Register plugin tools and bridge configuration; `crates/aft/src/main.rs`: Start the Rust request loop; `.github/workflows/release.yml`: Drive tagged release publishing.

**Configuration:** `package.json`: Define Bun workspace scripts; `Cargo.toml`: Define the Rust workspace; `packages/opencode-plugin/src/config.ts`: Parse user and project AFT config.

**Core Logic:** `crates/aft/src/parser.rs`: Extract symbols and languages; `crates/aft/src/callgraph.rs`: Build navigation indexes; `crates/aft/src/edit.rs`: Run shared edit and diff logic; `packages/opencode-plugin/src/bridge.ts`: Manage subprocess transport.

**Tests:** `packages/opencode-plugin/src/__tests__/`: Plugin unit and e2e tests; `crates/aft/tests/integration/`: Rust integration tests.

## Naming Conventions

**Files:** Use capability-oriented filenames. Put Rust command handlers in snake_case files such as `crates/aft/src/commands/move_symbol.rs`. Put TypeScript tool groups in concise nouns such as `packages/opencode-plugin/src/tools/navigation.ts`. Use `.test.ts` for plugin tests and `_test.rs` for Rust tests.

**Directories:** Use lower-case descriptive directories. Group related runtime code under `packages/opencode-plugin/src/tools/`, `crates/aft/src/commands/`, and `crates/aft/src/lsp/`.

## Where to Add New Code

**New hoisted OpenCode file tool:** `packages/opencode-plugin/src/tools/hoisted.ts` — register the tool and map it onto a Rust command.

**New plugin tool group:** `packages/opencode-plugin/src/tools/[capability].ts` — export a `Record<string, ToolDefinition>` and wire it into `packages/opencode-plugin/src/index.ts`.

**New Rust command handler:** `crates/aft/src/commands/[command_name].rs` — expose the handler from `crates/aft/src/commands/mod.rs` and dispatch it from `crates/aft/src/main.rs`.

**New shared Rust engine code:** `crates/aft/src/[domain].rs` — keep reusable parser, formatter, import, or analysis logic outside command handlers.

**New LSP behavior:** `crates/aft/src/lsp/[module].rs` — keep transport and server-management code inside the LSP subsystem.

**New platform binary package:** `packages/npm/[platform-key]/` — add `package.json` and ship the platform binary in `bin/`.

**New plugin tests:** `packages/opencode-plugin/src/__tests__/` or `packages/opencode-plugin/src/__tests__/e2e/` — follow the existing `*.test.ts` naming.

**New Rust integration tests:** `crates/aft/tests/integration/` — follow the existing `*_test.rs` naming.
