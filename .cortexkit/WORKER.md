# Worker guide: AFT

The commands to build, check and test this repository, the commands not to use, and how to tell that a check really ran. Use the exact commands here. Never substitute a bare package-runner name (`npx biome`, `npx tsc`): from a package folder it can fetch an unrelated npm package and "pass" without checking anything.

A check that prints nothing and exits 0 is not a pass unless this guide says that check is silent on success. Report the version line and the number of checks run.

## Rust (`crates/aft`, package `agent-file-tools`)

| Gate | Command | Proof it ran |
|---|---|---|
| Format | `cargo fmt --all -- --check` | Silent on success. Show `cargo fmt --version` and exit 0. |
| Windows compile | `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu` | `Finished` line. Compile-only: it cannot catch Windows runtime failures. |
| Unit (lib) | `cargo test -p agent-file-tools --lib -- <filter>` | `test result: ok. N passed` with N > 0. A filter that matches nothing reports `0 passed`, which is not a pass. |
| Binary tests | `cargo test -p agent-file-tools --bin aft -- <filter>` | Same as above. Shared infrastructure (locks, leases, capability gates, path identity) breaks fixtures here too. |
| Integration | `cargo test -p agent-file-tools --test integration -- <filter>` | Same as above. |
| Windows native tests | `bash scripts/windows-gate.sh --filter <filter>` or `cargo test -p agent-file-tools --lib -- <filter>` | Run with `runon: "windows"`. The Windows job snapshots tracked and untracked files, but not ignored files. A gate filter that runs zero tests fails. |
| Other test targets | `--test engine`, `--test list_envelope`, `--test rest`, `--test semantic`, `--test alert`, `--test watcher_integration` | Same as above. |
| Real rust-analyzer | prefix with `AFT_TEST_REQUIRE_RUST_ANALYZER=1` | Without it, real-server tests skip silently. |
| Full suite | `cargo test -p agent-file-tools`, or `scripts/rust-test-gate.sh` | The gate prints `GATE PASSED` / `GATE FAILED` as its last line. |

### Build server jobs (`runon`)

- **Size the job to the work.** Each vCPU you request holds one of the server's shared slots for the whole job. A filtered test run builds and runs one test binary, which uses about one core, so it uses `linux,2c`. Workers do not run the full suite; your head runs it once when it integrates several deliveries. If a job is refused with `queue_wait_exceeded` while waiting on permits (`waiting_on=permits`, or an older job ahead needs more free permits), resubmit it with fewer vCPUs (down to `linux,1c`) rather than waiting or asking: a smaller job compiles slower but starts. If the refusal says an earlier job still holds your workspace, fewer vCPUs won't help: wait for that job to finish, or stop it with `bash_kill` if it is yours and no longer needed.
- **Commit first, then submit, in separate steps.** The job snapshots the checkout when it is accepted. A commit sent in the same parallel step can land after the snapshot. Print `git log -1` as the job's first line so the output shows what ran.
- **Keep gate scripts out of ignored paths.** The job copies tracked and untracked files, but not ignored ones, so a script under `.cortexkit/` or `target/` is missing on the server.
- **Write long output to a file inside the job and print a summary.** A job that streams tens of megabytes back can lose its connection and come back with an unknown result.

Mandatory when you touch them:
- A list-cutting site (`.take(`, `truncate(`, a cap): register it, then run `cargo test -p agent-file-tools --test list_envelope`.
- A new file under the search engine: register its owner, then run `cargo test -p agent-file-tools --test engine`.
- An agent tool argument: regenerate with `bun run --cwd packages/opencode-plugin build:tool-schemas`. The committed `crates/aft/src/subc_tool_schemas.json` must match byte for byte.
- Config keys or the resolver: run `bun scripts/capture-config-parity.ts` (zero diff), then `cargo test -p agent-file-tools --test integration config_parity`.
- Search ranking or routing files (the `RANKING_FENCE_PREFIXES` in `benchmarks/aft-search/search_quality_lib.py`): say so in your report. The train needs a search-quality descriptor, and ranking changes need benchmark evidence.

## TypeScript (`packages/*`)

Build the bridge first whenever you change it: `bun run --cwd packages/aft-bridge build`. The plugins import its `dist/`. A stale `dist/` gives "export not found" or "property does not exist" errors that look like your bug.

| Gate | Command | Proof it ran |
|---|---|---|
| Lint (Biome 2.x) | `bun run lint` at the repo root | `Checked N files`. Show `./node_modules/.bin/biome --version`. |
| Typecheck | `bun run --cwd packages/<pkg> typecheck` | Exit 0. Name the package. |
| Unit tests | `bun run --cwd packages/<pkg> test:unit` | `N pass` / `0 fail` with N > 0. |
| End-to-end tests | `bun run --cwd packages/opencode-plugin test:e2e` (and pi-plugin) | Same as above. They spawn `target/debug/aft`. |

New test files go under `src/__tests__/`. A guard fails the suite on any test file elsewhere.

## Do not
- **Pipe a gated command** (`cmd | tail`, `cmd | grep`). The pipe's exit code hides the failure. Run it bare, or save output to a file and read the file.
- **Run TypeScript end-to-end suites while cargo is building** the same checkout. Cargo rewrites `target/debug/aft` mid-run and every bridge test fails with "Bridge shutting down".
- **Run the plugin end-to-end or permission suites from a worktree under `/tmp`.** The temp-directory permission exemption makes their assertions wrong.
- **Generate synthetic CPU load** (busy loops, stress tools). To test timing under load, use the injected-delay test hooks (`test-timing-hooks`) or a narrower deadline.
- **Run `aft_inspect` on Rust files while your own build or tests are compiling.** Its rust-analyzer check can hold the target lock.
- **Send a `read` in the same step as the bash command whose effect it checks** (for example a `git checkout --` revert). Parallel calls race, and the read can finish first and show the old file. Read in the next step.
- **Write closing keywords** (`Fixes #N`, `Closes #N`, `Resolves #N`) in commit messages. Write `(#N)`.
- **Write scratch files to `/tmp` or `$TMPDIR`** (diffs, logs, extracted outputs). Reading them back raises a permission ask your head has to answer. Keep scratch under `target/` in your worktree, which is git-ignored and readable without asking.
- **Edit `ARCHITECTURE.md` or `STRUCTURE.md`** to log a change.

## Before you report
- Publication source changes do not need generated docs or alignment commits.
  Run `node scripts/release-gate-v049.mjs --check-inputs` after committing the
  source (requires the `v0.48.1` tag and the built bridge). Run it in your
  worktree, not with `runon`: build-server snapshots carry no git tags, so it
  always fails there. It prints six staged
  transitions and `release gates passed`; it never runs a native build.
  The reviewed legacy-vocabulary allowlist remains an input: update it explicitly
  only when intentionally changing a reviewed occurrence, never in preflight.
  Release snapshots are automatically prepared and validated by `release.sh`
  and the publish workflow, outside Git. To validate a saved snapshot without
  regenerating it: `node scripts/release-gate-v049.mjs --snapshot-dir <dir> --candidate`.
- Test what you changed, and only that. For a bug fix, add a test that fails without the fix. Run the filtered tests for the code you touched in one job, plus the Windows compile when you changed platform-specific code. Do not run the full suite, mutation or break checks (reverting a fix to show a test fails), or repeated runs to prove stability: each one recompiles the whole crate and holds a shared build-server slot. Your head integrates several deliveries and runs the full suite once.
- If a gate could not run, say which one and why. Do not report it as passed.
