# Language servers in dependency-free worktrees

## Investigation (2026-09-28)

`registry.rs` starts `typescript-language-server --stdio` and `biome lsp-proxy`.
The TypeScript server previously received neither `--tsserver-path` nor an
`initializationOptions.tsserver.path` unless users supplied an override. Its
binary being available did not prove that a TypeScript SDK was available.
Both plugins installed `typescript-language-server` into the LSP cache without
installing its separate `typescript` runtime dependency.

A fixture repository used TypeScript 5.9.3, typescript-language-server 5.0.0,
Biome 2.2.4, `tsconfig.json`, `biome.json`, and an intentional TS2322 assignment.
Dependencies were installed only in the fixture's main checkout; a linked
`git worktree` had no `node_modules`.

Before the change, NDJSON `configure` followed by scoped `inspect` returned
`fresh` but `complete: false` in **both** checkouts, with
`no LSP producer has a current diagnostic report for this file`.
Both servers started on this machine: globally available tools masked the
reported missing-installation failure. Thus the exact original initialize
failure was **not reproduced** here. Inspect deliberately does not open every
scoped file, so its uncovered-file result is not evidence of server failure.

After the change, explicit cached bin paths plus `lsp_inspect`/`inspect` started
both servers in both fixtures. Inspect reported `TypeScript 5.9.3: project
installation` for main and `TypeScript 5.9.3: AFT cache fallback; not the project's
pinned TypeScript` for the fresh worktree. Biome initialized successfully here;
its original failure was not reproducible. A separate real-server manager test
opens the document and waits for TS2322 rather than treating startup or Biome's
faster diagnostic report as proof of TypeScript analysis.

## Chosen behavior

- Prefer the nearest project TypeScript SDK, bounded by the configured project.
- Otherwise explicitly pass a TypeScript SDK from `lsp_paths_extra` to the
  language server. Both plugins now independently install/cache `typescript`
  using the existing registry grace period, version pins, locks and integrity
  checks. The distinct `typescript-sdk` entry also honors the `typescript`
  disabled-server setting. No install is
  performed by the Rust resolver.
- Cache fallback versions are not project-pinned. Inspect reports the selected
  version and source in both text and `lsp_runtime_notes`, captured at startup
  rather than recomputed from a possibly changed filesystem.
- If neither SDK is available to AFT, preserve the language server's own SDK
  discovery (including global and bundled installs). Report `TypeScript SDK
  unavailable` only when the server's actual initialize error says it could not
  find a valid TypeScript installation; preserve other failures unchanged.
- Explicit binary and tsserver path overrides retain responsibility for their
  own SDK selection. Their versions cannot be inferred reliably.
- Do not implicitly borrow the main checkout's dependencies. Matching lockfiles
  alone does not establish that its installed dependencies match the lockfile;
  the independent cache avoids relying on another checkout's mutable state.
- Biome retains its existing project/cache/PATH resolution. Initialize failures
  preserve their original cause and add an actionable worktree installation and
  configuration explanation; no project files are installed or changed.

## Verification recipe

```
bun install --frozen-lockfile
bun run --cwd packages/aft-bridge build
bun test packages/opencode-plugin/src/__tests__/lsp-auto-install.test.ts packages/pi-plugin/src/__tests__/lsp-auto-install.test.ts
bun run --cwd packages/opencode-plugin typecheck
bun run --cwd packages/pi-plugin typecheck
cargo test -p agent-file-tools --lib typescript_worktree_tests
AFT_TEST_LSP_BIN_DIR=/absolute/cache/node_modules/.bin cargo test --features test-hooks -p agent-file-tools --test lsp_fresh_worktree_test -- --nocapture
```

The opt-in real-server test requires Node and a bin directory with
`typescript-language-server` and adjacent `typescript`. Without the environment
variable or Node it prints a named skip reason. It creates a real linked
worktree, proves the TS2322 diagnostic and fallback disclosure, and checks both
directory entries and Git status for writes. Unit tests cover missing SDKs,
local precedence, explicit initialization overrides, version reporting and
read-only resolution without requiring Node or servers. Missing local/cache SDK
coverage asserts that configured initialization options pass through unchanged,
not that startup is refused. Actual server missing-installation errors are
separately tested for actionable classification.
