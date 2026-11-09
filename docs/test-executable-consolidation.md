# Integration test executable consolidation

Cargo auto-discovery is disabled for `agent-file-tools`. Five entrypoints under
`crates/aft/tests/{engine,list_envelope,semantic,alert,rest}/main.rs` include the
existing sources as modules. The `engine_*` and `search_b2_*` tests share one executable;
the list-envelope core and conformance directories retain their own structure
inside the list-envelope executable.

Sources stay at their original paths because search-quality descriptors and
source-relative fixture includes refer to them. `RANKING_FENCE_PREFIXES`, the
search-quality descriptor validator, and the inline tool-call parity fixture
path remain unchanged. No descriptor changes class because of this fold.

## Isolation retained

- `integration`: existing general integration harness, including its existing
  process-state synchronization.
- `watcher_integration`: live watchers must not run alongside unrelated
  process-heavy tests; CI also runs this harness single-threaded to avoid
  overlapping live-watcher setup and teardown load.
- `sandbox_launch_probe`: CI runs these probes single-threaded with mandatory
  Landlock enforcement on Linux.
- `semantic_test`: one test changes process cwd before launching its child.
- `semantic_embed_working_set_test` and
  `semantic_embed_overflow_recovery_working_set_test`: each declares a counting
  global allocator and measures the entire process heap.
- `daemon_malloc_small_growth_repro`: ignored macOS measurements also use a
  counting global allocator. Keeping this separate preserves their manual use.

Other ignored benchmarks/probes remain modules in the aggregate harnesses, with
their ignore attributes intact. `compress_spike` and `synapse_live_test` are
ordinary runnable tests, not omitted probes. The navigation profile remains
behind its explicit `aft_views_lazy_benchmark` cfg; the semantic chunk census
remains behind its existing feature. New test sources must be wired into an
entrypoint or explicitly declared in Cargo.toml because auto-discovery is off.

The disk persistence tests launch ignored tests in the current executable.
Their exact child filter now derives from `module_path!()` with the crate name
removed, so it works in both the old integration harness and the new nested
semantic harness. The helper modules are reused rather than compiled twice,
which preserves their test counts too.

## Measured inventory and cold builds

Measurements used the fresh isolated task worktree on macOS, baseline revision
`5db920506eecde299a87b78450130e447e4805a6`, and empty Cargo target directories
before each cold build. Dependencies were not copied from another build. The
Cargo registry/toolchain and machine were shared with other workers.

| Measurement | Before | After |
| --- | ---: | ---: |
| `cargo test --features test-hooks -p agent-file-tools --no-run` test executables | 81 | 16 |
| Integration test executables within that count | 77 | 12 |
| Cold `--no-run` wall time | 440.18 s | 1076.11 s |
| Cold build user CPU time | 381.10 s | 317.64 s |
| Cold build system CPU time | 80.24 s | 107.21 s |
| `cargo nextest list --features test-hooks -p agent-file-tools` test count | 7305 | 7305 |
| Ignored tests in that inventory | 84 | 84 |

The executable count falls by 65 (80.2%). These wall-time samples are not a
controlled performance comparison: the final cold build ran on a heavily
contended machine. No cold-build wall-time speedup is claimed.

The comparison checks the complete sorted multiset of test names, kinds and
ignore flags, not just counts. Former standalone targets gain their source
module prefix; existing aggregate targets and isolation exceptions keep their
names. The normalized inventories have the same SHA-256:
`382c8c0ea65fdbda87ebb4f0e0abc3d7a45386dcf47dbbb959c41a22ca5eb796`.
Temporarily omitting `compress_spike` made the comparison's
`test_fold_preserves_all_test_names_and_ignore_flags` fail with 7305 versus
7304; the source was restored and the comparison passed again.

The CI inventory proof independently covers all workspace tests exactly once:
4519 lib/bin tests, 2762 nextest integration tests and 27 watcher tests, with no
doctest cases. The LSP nextest group now selects module prefixes rather than the
removed standalone LSP binary names. The compression benchmark command selects
`rest` with the `compress_spike::` filter. Existing scripts and workflows naming
`integration`, `watcher_integration` and `sandbox_launch_probe` remain valid.

### Re-applied on the train 274 tree

The fold was re-applied unchanged to a tree that had gained one top-level
test source since it was written, `tool_provider_conformance.rs`, now a module
of `rest`. It resolves the `aft` binary through `tests/helpers/aft_binary.rs`,
which prefers `AFT_TEST_AFT_BINARY`, then nextest's `NEXTEST_BIN_EXE_aft`, then
Cargo's build-time path, so it keeps working from a relocated nextest archive.
Pointing `AFT_TEST_AFT_BINARY` at a missing file makes its tests fail at
launch, which shows the folded module goes through that helper.
`cargo nextest list --features test-hooks -p agent-file-tools --run-ignored all`: 82 to 16 test
executables, 7541 tests and 89 ignored before and after, and the sorted
multiset of (source, test name, ignore flag) is identical once the folded
module prefix is accounted for.

## Verification limitations

All five folded suites pass: alert 15, engine 204, list-envelope 165, rest 218
(with 7 ignored), and semantic 49 (with 3 ignored). Disk persistence tests also
pass in the retained integration harness: 22 passed, 3 ignored. Formatting,
Windows warnings-denied test checking, TypeScript typechecking, search-quality
self-tests, and the independent CI inventory proof pass.

The full Cargo test command and Rust gate are **not green**. The former stopped
on an unchanged library test about duplicate semantic reload workers; the gate
failed on four other unchanged library timing/state tests before reaching its
inventory phase. A bounded all-integration run reported six failures in the
unchanged integration harness (five real-rust-analyzer tests and a shared-DB
latency test), a transient rest migration failure that passed in the separate
folded-suite run, and semantic child tests that were not selected because their
exact filters lacked the new outer module prefix. Deriving those child filters
from the enclosing module path corrected the semantic failures. The run
hit its 20-minute limit when starting the watcher harness. A separate serial
watcher run passed 26 tests and failed the unchanged linked-worktree semantic
quiet-window test. Isolated baseline executables passed the semantic-reload and
one real-rust-analyzer test, consistent with load-sensitive failures, but a full
baseline suite was not rerun. Full gate success therefore remains unverified.

## Fake LSP server

No shared cache was added. After consolidation, caching the fake server can
avoid at most one ordinary executable's per-worktree macOS first-run security
scan (its empty test
harness is another target). That is a much smaller opportunity than the 65
removed harnesses. Consider it only if post-fold measurements still identify
fake-server first execution as a material bottleneck; a shared cache would need
its own invalidation, compatibility and concurrency design.

## Native Windows Rust test gate

Run `scripts/windows-gate.sh` before `scripts/train-push.sh` when changes need a
native Windows x64 check. This gate is an operator smoke, not a replacement for
three-platform CI; train-push behavior is unchanged. Run the command with the
bash runner's `runon: "windows"` setting. The runner executes it in Git Bash on
Windows. The command runs in that job without separate machine setup.

```sh
bash scripts/windows-gate.sh --plan                     # inspect the selected slices
bash scripts/windows-gate.sh --base origin/main         # run changed Rust slices
bash scripts/windows-gate.sh --full                     # lib + integration
bash scripts/windows-gate.sh --filter search_index::    # one library slice
bash scripts/windows-gate.sh --timeout-minutes 30 --full
bash scripts/windows-gate.sh --status                   # local runner and target info
```

The Windows job receives a snapshot of the checkout: tracked and untracked files
are included, while ignored files are not. Keep any fixture or generated input
needed by a test out of ignored paths, or the remote job will not have it. The
gate plans changed slices from the base diff plus tracked working-tree changes
and non-ignored untracked files. Cargo manifests, build scripts, shared library
infrastructure and configuration changes select the whole library suite. Other
touched Rust modules and test harnesses select their corresponding slices.
`--filter` overrides that selection for the library; with `--full` it filters
both library and integration. `--plan` prints commands without running them, and
`--status` reports local runner information. `--timeout-minutes` caps the test
run. A diff with no Rust changes selects nothing; use `--full` or `--filter` to
request an explicit run.

Every non-empty gate first runs the production storage/config isolation test.
A selected Cargo command that executes zero tests fails the gate rather than
silently reporting green. Direct Cargo invocations are also supported for
narrow checks, but verify that their `test result` reports at least one executed
test; Cargo itself returns success when a filter matches nothing.

Local control checks: `bash scripts/lib/test_windows_gate.sh`,
`bash -n scripts/windows-gate.sh`, `shellcheck -S warning scripts/windows-gate.sh`.
