# Train 345 Linux plugin E2E repair

Base: `73f908fd8e2d5cf255502e3785f02dfdb8241cdf`.
Implementation: `6f6420a50`.

## Confirmed causes and fixes

### Accepted import text

The current Rust producer correctly returns:

```text
removed the lodash import (its only name)
file …/imports.ts
```

The OpenCode E2E test still expected the older `removed lodash` / `name debounce` rendering. Updated only those response assertions to the accepted rich summary; the assertion that lodash is actually removed from the file remains.

Searched E2E and parity expectations for the old remove wording. Pi E2E checks actual file contents rather than this wording. The remaining `subc_parity/format/import_remove_removed_name` golden deliberately supplies no rich remaining-name metadata, so its legacy `removed pkg` / `name alpha` compatibility output stays unchanged. No product formatter or golden fixture was rewritten.

### Rustup proxy under isolated HOME

Confirmed the hypothesis using the real native debug engine. Removing toolchain pins from isolated child environments makes `rustfmt formats deformatted Rust` fail with the captured native response:

```json
{"formatted":false,"format_skipped_reason":"error","syntax_valid":true}
```

The file remains deformatted. A direct rustfmt probe with HOME isolated and RUSTUP_HOME/CARGO_HOME unset reports:

```text
rustup could not choose a version of rustfmt to run, because one wasn't specified explicitly, and no default is configured.
```

`isolatedAftEnvironment` now captures the original RUSTUP_HOME/CARGO_HOME, or their original-home defaults, before replacing HOME. Source and dist imports share that snapshot through the same global symbol. Explicit selections still pass through, and every AFT HOME/XDG/storage directory remains isolated. HOME is never reset to the account home.

Added fresh-process coverage proving default toolchain paths survive repeated HOME changes, and coverage for explicit selections. The Rustfmt E2E assertion now includes output/data in its failure diagnostic without weakening its exact formatted-file assertion.

### Why private storage targeted `/`

This was not an empty-HOME resolver bug. Under `BUN_TEST`, `resolvePluginLogPath` returned the top-level temp file `/tmp/aft-plugin-test.log`. `RotatingLogSink.ensureDirectory` treated the log directory `/tmp` as a storage child and passed its parent `/` to `openPrivateStorageDir`, which attempted fchmod on that root before walking to the log directory.

Test logs now live at `<temp>/aft-plugin-tests-<pid>/logs/aft-plugin-test.log`, giving the sink an owned, process-private namespace. `openPrivateStorageDir` also rejects filesystem roots before mkdir/open/chmod and checks containment before directory creation. Root rejection applies on Windows drive roots too.

New tests verify rejection before any filesystem I/O and that writing a test log does not change shared-parent permissions. The root test mocks syscalls and first proves interception on a disposable directory, so neither its baseline red nor mutation can modify a real system root. The shared-parent test nests its fixture under an owned temporary container for the same reason.

## Verification

Tools: Bun 1.4.2, TypeScript 5.9.3, Biome 2.4.7; native rustc/cargo 1.99.0.

The user-authorized local debug build used `CARGO_BUILD_JOBS=4`, this worktree's target directory and external HOME/XDG directories, while retaining toolchain caches. The first attempt timed out at 30 minutes in the shared six-slot compile queue. Output explicitly identified the queue, not a compiler failure. A longer cached retry completed successfully in 32m 55s; its background promotion was watched to completion before running E2E. No E2E ran while Cargo was rebuilding the executable.

Every local test command supplied a fresh HOME and XDG_DATA/CONFIG/STATE/CACHE_HOME created with `mkdir -p` outside all Git checkouts, with AFT_STORAGE_DIR and production-migration opt-in unset. Final permission-bearing runs used a non-system-temp namespace under the account's separate alfonso/test-homes directory: putting the outer HOME under `/private/tmp` made an external-directory fixture legitimately receive the temp-path exemption. No permission assertion was changed to accommodate that initial environmental mismatch.

| Gate | Result |
| --- | --- |
| Bridge build before plugin checks | Passed (`tsc`), also rebuilt after restoring the toolchain mutation |
| Root `bun run typecheck` | Passed for all four package scripts |
| Root `bun run lint` | Passed; 706 files checked |
| New safety/isolation unit filter, before fixes | 1 passed, 2 failed: missing default toolchain homes and filesystem-root admission |
| Relevant bridge unit filter after fixes | 26 passed, 0 failed |
| Full `bun run --cwd packages/aft-bridge test:unit` | 812 passed, 3 platform skips, 0 failed; 815 tests across 68 files |
| OpenCode import + format-on-edit-write E2E files, final restored run | 20 passed, 1 skip, 0 failed; 21 tests, 87 assertions |
| Bridge E2E directory | 7 passed, 15 skipped, 0 failed; 22 tests across 3 files |
| Full Pi `test:e2e` | 116 passed, 3 skipped, 1 failed; 120 tests across 19 files |
| Pi conflicts file, narrow default-timeout rerun | Reproduced the 5000ms setup-hook timeout |
| Pi conflicts file, diagnostic `--timeout 30000` rerun | 1 passed, 0 failed; 6 assertions, 11.58s total |

OpenCode's skip is the real Ruff formatter test because Ruff is unavailable/too old; Rustfmt and gofmt execute and pass. Bridge's skipped cases require an unavailable subc-core executable; no all-skipped lane is claimed as a pass. Its seven orphan-process tests do execute. Pi's three skips require absent OMP/Pi host installations; its remaining failure is the conflicts fixture's default five-second setup hook. The diagnostic longer-cap run verifies the actual conflict-region behavior; no committed timeout or assertion was weakened.

Scoped inspect reported authoritative zero diagnostics for the three changed bridge implementation files, with unrelated Tier-2 callgraph availability gaps. Typechecks and lint are the authoritative static gates.

## Mutation evidence

All mutations were staged from the live working state, marked `NON-VACUITY BREAK`, run narrowly, then restored with `git checkout -- <path> && touch <path>`. Each had a non-empty diff during the break and an empty working diff afterward. No mutant was committed.

1. Disable filesystem-root rejection: only `private storage rejects filesystem roots before any I/O` fails, because the function no longer throws before attempting mocked opens.
2. Collapse the private test-log namespace: only `test logs repair their private namespace, not the shared temp directory` fails; the disposable shared directory changes from mode 0755 to 0700.
3. Remove toolchain pins from isolated children: only `rustfmt formats deformatted Rust` fails with `formatted:false` and `format_skipped_reason:error`. Restored bridge build plus the final whole-file E2E run pass.
4. Restore retired import wording: only `aft_import uses tool_call formatting for add, remove, and organize` fails, receiving the accepted rich only-name summary instead.

## Scope

No Rust behavior, public tool argument, config schema, package manifest, lockfile or search-ranking file changed. Native binaries, compile caches and test logs remain untracked/ignored. Governed-doc alignment was **not run**, per the parent's instruction.
