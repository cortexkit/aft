# Train 345 Windows storage-fence repair

Base: `b7d5ad95300933a040a08d9adbee9943a976cc13`, the rebased `origin/train/345` candidate on main `7c3e39b8c`.

Implementation commit: `8483c57841923cfe0dc0cc3c2e12003f01b892d3`.

## Confirmed failure mechanism

CI run 37743824987's Windows Bash permission job failed during configure because `SHGetFolderPathW(CSIDL_LOCAL_APPDATA)` was asserted successful. The test harness substitutes child HOME/USERPROFILE/LOCALAPPDATA, and the original lookup requires the expanded folder to exist.

The real OVH Windows Server 2022 VM reproduced the mechanism. Under a nonexistent profile environment, the native regression printed:

```text
legacy SHGetFolderPathW without DONT_VERIFY: HRESULT=0x80070003
```

`0x80070003` is the path-not-found HRESULT. The new resolver simultaneously returned the real token-profile root `C:\Users\builder\AppData\Local\cortexkit\aft` and the shell's environment-expanded root below the deliberately nonexistent profile. The test proves the real root remains protected, the nonexistent environment directories are not created, and separate disposable storage is writable.

The observed shell path under `nonexistent-profile\AppData\Local` confirms that the known-folder result is environment-sensitive. It must supplement the token-derived account identity, not replace it.

## Implementation

- Open the current process token with `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY)` and use `GetUserProfileDirectoryW` to resolve the real profile. Protect its `AppData\Local\cortexkit\aft` subtree.
- Add the root returned by `SHGetKnownFolderPath(FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY)` whenever that lookup succeeds. Protect the union, including redirected shell locations.
- Close token handles through RAII and release shell paths with `CoTaskMemFree` on all exits.
- Account lookup failures return errors, not assertions. If no OS lookup resolves a protected root, debug write refusal retains `dev_build_refused_production_migration` and `PermissionDenied`, with a reason naming the unresolved lookup. The bool predicate remains conservative for existing database read-only admission paths.
- Unix continues to use the effective account's passwd home, independently of HOME/XDG. Lookup failures and buffer-limit exhaustion now return errors instead of asserting. Path normalization no longer panics when the current directory is unavailable, and ancestor-prefix handling no longer unwraps.
- Update the test-only live-store guard to use the same native root union and token profile. Existing intentional should-panic test fences remain intact; spawned debug binaries do not compile those test-only assertions.
- Correct the module comment and development-storage documentation to distinguish the environment-independent identity lookup from the environment-sensitive additional shell root.
- Add only `Win32_System_Threading` and `Win32_System_Com` to windows-sys. The existing `Win32_Security` and `Win32_UI_Shell` features already supply the other APIs.

## Native Windows verification

The required gate was absent from the candidate. The parent supplied accepted ref `1fb6a2d4a`. Its three gate files were extracted with `git show` into an external scratch directory, keeping the scripts/lib layout; **none was committed to Train 345**.

Ran the extracted `scripts/windows-gate.sh --filter production_storage --timeout-minutes 60` with this worktree as cwd. The gate bundles committed HEAD and owns `C:\build\aft\gate.lock`. Both the initial and post-mutation-restoration runs passed:

```text
storage preflight: 1 passed; 0 failed
production_storage: 11 passed; 0 failed
Windows gate passed.
```

Wall times: 206.37 seconds initially, 140.94 seconds after restoration. Neither run encountered the maintenance exit 75.

Native toolchain: cargo 1.99.0 (`5f94df478`, 2026-08-27), rustc 1.99.0 (`b940084d7`, 2026-09-28), host `x86_64-pc-windows-msvc`, Visual Studio 2022 Developer PowerShell 17.14.41.

A separate SSH follow-up acquired the same lock atomically, refused maintenance/busy ownership, verified exact guest HEAD and a clean tracked checkout, and created external HOME/XDG directories before tests. It kept the parent process's native USERPROFILE/LOCALAPPDATA for the separate read-only Bun canary; the Rust regression and Bun preload independently substitute their child profiles.

Additional native results:

| Command | Result |
| --- | --- |
| Exact Windows missing-home regression with `--nocapture` | 1 passed; printed the HRESULT and both roots above |
| `cargo test --locked -j 4 -p agent-file-tools --lib test_storage -- --test-threads 1` | 9 passed, including account-default rejection controls |
| `cargo build --locked -j 4 -p agent-file-tools` | Finished dev build, 1m 13s |
| `bun install --frozen-lockfile` | Bun 1.4.2, 428 packages installed, 16.46s |
| `bun run --cwd packages/aft-bridge build` | Passed (`tsc`) |
| `bun run --cwd packages/pi-plugin build` | Passed, 189 modules bundled plus declarations |
| `bun test src/__tests__/e2e/bash.test.ts` in OpenCode plugin | **18 pass, 5 skip, 4 fail, 1 error**, 27 tests, 185 assertions, 57.22s |

The gate's shared Cargo target is outside its checkout (`C:\build\aft\target`). After building, only the verified `aft.exe` was copied to the checkout-relative `target\debug` location used by the E2E fixtures. No global software was installed. The lock covered build, install and test commands and was released in a finally block.

### Remaining Bash E2E failures

The original configure panic is gone. The file now reaches its assertions; its remaining failures were recorded rather than changing unrelated Bash behavior or weakening fixtures:

1. `non-zero exit appends [exit code: N] to agent-visible output`: the fixture sends POSIX `false` to PowerShell. Expected `\n[exit code: 1]`; received `false : The term 'false' is not recognized ...`.
2. `bash_status reports running then completed output`: the test times out at 5000 ms. Teardown subsequently produces the unhandled `Bridge is shutting down, cannot send "bash_status"` error.
3. `background completions are no longer appended by the bash adapter`: `bg-marker.txt` never contains `bg-done` within the fixture's wait; failure after 30383 ms.
4. `permission ask round-trip invokes OpenCode ctx.ask`: expected git status exit 128 for a non-repository fixture, received 0. The accepted preload places its HOME and fixture cache beneath the gate checkout's target directory, where git can discover the ancestor repository.

The test file itself documents PowerShell as the Windows contract. No Git-Bash override was used to hide these failures. These are observed residual failures, not a claimed green file or a proven clean-baseline result; the old candidate could not reach these assertions because configure panicked.

## Mutation proof

The local live file was staged and its working diff confirmed empty. Replaced the Windows root union with the old SHGetFolderPath-only lookup, marked `NON-VACUITY BREAK`, and captured:

```text
crates/aft/src/production_storage.rs | 11 ++++++++++-
1 file changed, 10 insertions(+), 1 deletion(-)
```

No mutation was committed. Under the VM's gate lock, the guest live file was staged, its diff checked empty, the same mutant copied in, and the exact new Windows test run:

```text
token root must survive failed or redirected shell lookup:
Custom { kind: Other, error: "legacy SHGetFolderPathW failed: HRESULT=0x80070003" }
test production_storage::tests::windows_token_profile_is_protected_with_missing_home_environment ... FAILED
test result: FAILED. 0 passed; 1 failed
```

Only that exact test was selected, and only it failed. The guest restored from its staged live state and updated the file timestamp; its diff was empty afterward. Locally, `git checkout -- crates/aft/src/production_storage.rs && touch crates/aft/src/production_storage.rs` likewise left an empty working diff. The final native gate then rebuilt the restored implementation and passed all 11 production-storage tests plus its preflight.

## Linux and workspace checks

All Rust commands ran with `runon: linux` and started with `[ "$(uname)" = Linux ] || exit 99`. Rust tests used created external HOME/XDG directories with AFT_STORAGE_DIR and production-migration opt-in unset.

- `cargo test --locked -p agent-file-tools --lib production_storage`: **12 passed**, including the Unix missing-HOME test, root-union fallbacks and typed no-root refusal.
- `cargo test --locked -p agent-file-tools --lib test_storage`: **9 passed**.
- `cargo test --locked -p agent-file-tools --bin aft`: **122 passed**.
- `cargo fmt --all -- --check`: exit 0; rustfmt 1.10.0-stable.
- Linux toolchain: rustc/cargo 1.99.0, matching the native VM versions above.
- Local `bun install --frozen-lockfile`: Bun 1.4.2, 437 installs checked across 463 packages, no lockfile changes. Run because the Cargo manifest changed.
- Root `bun run lint`: **706 files checked**, Biome 2.4.7, no fixes needed.

No TypeScript source, tool arguments, config schema or search-ranking files changed in this repair. The required native Windows compilation supersedes the previously unavailable cross-compile target for the touched Windows code. The parent's existing search-quality replay requirement is unchanged.
