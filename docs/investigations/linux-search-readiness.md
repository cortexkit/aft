# Linux cold-search readiness regression

## Reproduction

Reproduced in an `ubuntu:24.04` Docker container (Linux x86_64 under OrbStack on an arm64 host), with this worktree mounted at `/work`, a container-only `/work/target` volume, Rust stable and Bun 1.4.2. Commands inside the container:

```sh
export PATH=/root/.cargo/bin:/root/.bun/bin:$PATH
export CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=
cargo build -p agent-file-tools
bun install
bun run --filter @cortexkit/aft-bridge build
CI=true bun test packages/opencode-plugin/src/__tests__/e2e/semantic-search.test.ts
```

The initial install repaired a pre-existing CLI workspace version mismatch in `bun.lock`; that incidental lockfile edit was reverted. No package manifest or test expectation was changed.

- `origin/main` (`383a119573719094a11fa75472d82a79d1e51a39`, train 233): 4/4 passed.
- Train 234 tip (`a44e1f5e753a3297fab8e4e611151a0925d19c3f`): 3/4 passed; the semantic-disabled test returned exactly `Found 0 results.\nSearch status: partial/incomplete.`

A first-parent `git bisect run` rebuilt the Linux binary at every step and ran the unchanged test file filtered by `-t 'aft_search degrades'`:

| Revision | Result |
| --- | --- |
| `9386601f81c20e0d1de70be2eff983a8efc2e8a2` | fail |
| `3b8421958e402fc9f9477823816907b19309832c` | pass |
| `a3e00ad1dd0155fe40759fe629f5d6bf82386e3b` | pass |
| `8f9ad01ea7edf2336b730cc1e4445fe70cdbd842` | pass |
| `3c9f37c46c6a9b25fa37075ced969ce27d4e6fc6` | fail |
| `b1e87c9d97e5a7d7c7554f9c9daa63f4f202a866` | pass |

The observed first bad revision is `3c9f37c46`, the selective database-readiness change, not the bounded exact walk or anchored admission changes.

## Failure mechanism

A temporary response-logging wrapper around the unchanged e2e test captured:

```text
plan.readiness: symbol_index=true, lexical_index=false, semantic_index=false
reasons: trigram:building:trigram_index, semantic:disabled
executed_callbacks: symbol, exact, readiness_disclosure
exact_mode: fallback
results: []
lanes.trigram.status: ready
```

The symbol cache can finish warming before trigram publication is drained. `search_b2::readiness::sample` incorrectly treated that as sufficient readiness and skipped the bounded first-search wait. Its plan consequently omitted the lexical and variants lanes. Later, `handle_semantic_search_inner` drained the now-completed trigram load and observed a ready retrieval index, so it allowed execution. It did **not** rebuild the earlier plan. Lexical candidates could be available in the live index but were not an executed lane.

The fixture contains `handle_request`, not the complete phrase `request authentication handler`. Symbol and exact matching alone legitimately find nothing. The failing response is an engine-only lexical fallback with the lexical lane omitted, **not** a truncated filesystem walk, a grep fallback, a missing inotify event, or a directory-symlink/mount/timestamp exclusion. The mismatch between plan readiness and final lane status is the evidence distinguishing these cases.

This is a scheduling-sensitive, platform-independent defect exposed on Linux. It is not a Linux-only branch: a ready symbol cache and queued trigram publication reproduce it deterministically in a Rust unit test on any platform. The macOS pass history does not make symbol readiness a sufficient admission condition.

## Fix and contract

The first-search wait now skips only when lexical or semantic retrieval is ready. If only symbols are ready, admission takes the existing bounded wait and samples again before building the plan. It retains the snapshot from that second sample. Existing cancellation, contention bounds, exact-walk bounds and completeness disclosure remain unchanged.

If neither retrieval index becomes ready, the existing `search_lanes_unavailable` refusal remains. No new on-disk lexical fallback is introduced. This preserves the established Rust refusal contract rather than changing it to match the initial diagnosis in the report.

Two regressions failed on the unmodified implementation:

- `ready_symbols_wait_for_trigram_before_selecting_lexical_plan`: a sequenced readiness source proves that the post-wait plan selects lexical retrieval.
- `commands::semantic_search::tests::ready_symbols_with_pending_trigram_retain_lexical_fallback_results`: a real source file, warm symbol cache and queued index publication produce `Found 0 results.` before the fix. No thread sleeps are needed. After the fix the result contains `pub fn handle_request` and `[lexical match]`, while completeness remains false and semantic remains disabled.

## Database change and the separate safety/undo stall

`3c9f37c46` replaces the broad database refusal with `persistence_gate::requires_database`: `search`, `semantic_search` and the `tool_call` envelope no longer require aft.db readiness, while safety/history/undo still do. Its predecessor moved database initialization after configure acknowledgement. In subc this explicitly permits a read-only search to reach index admission during database opening instead of being refused; persistence-dependent calls park outside the executor for a bounded notification wait. Neither change modifies the search planner or lexical matcher. They expose a pre-existing assumption about startup ordering, not a new matching algorithm.

There is an important limit to attributing the **NDJSON** timing change: `main.rs` still calls `StandaloneConfigureMaintenance::drain_prefix` before request dispatch, and that prefix includes the database stage. Thus the NDJSON reproduction does **not** prove that search actually ran before database initialization. The observed bisect boundary is empirical; the raw response and deterministic regression prove the symbol/trigram publication race, but do not isolate which instruction or scheduling change in that revision moved Linux into its window. It would be incorrect to describe the NDJSON failure as a proven database-not-ready execution or an inotify defect.

The same search-admission mechanism cannot account for a module that stops answering after an undo-history call: it returns a successful empty search response and does not execute safety/history/undo. Those commands are still classified as database-dependent, and subc's database wait is a different path (`subc/persistence.rs`). That path has a 10-second admission budget and is intended not to occupy the frame loop or executor. A permanent loss of responses would need separate evidence of database open/backup locking, executor work, or transport progress after admission. This investigation does not reproduce or diagnose that hang and makes no changes to persistence or safety.

## Verification results

- Fixed Linux debug binary: unchanged semantic-search e2e file passed all 4 tests (11 assertions).
- `cargo test --features test-hooks -p agent-file-tools --test search_b2_readiness`: all 13 passed, including after mutation restoration. Isolated post-restore runs of the real-handler regression and `building_indexes_refuse_with_each_lanes_status_instead_of_walking` also passed.
- `cargo test -p agent-file-tools --lib commands::semantic_search::tests`: 87 passed, 1 failed the unrelated 200 ms wall-clock assertion in `blackholed_backend_never_blocks_status_or_search` under x86_64 emulation. It took 209.6 ms for status in the suite and 203.3 ms for search on an isolated retry. That fixture already has a ready lexical index, so the changed admission condition takes the same branch as before. The test was not relaxed.
- Both new regressions failed against the old implementation; the real-handler test reproduced the same empty result and contradictory readiness metadata as the Linux e2e failure.
- Controlled mutation restored the symbol-only shortcut in `readiness.rs`: `ready_symbols_wait_for_trigram_before_selecting_lexical_plan` alone failed, and the other 12 readiness tests passed. The staged fix was restored afterward with an empty unstaged diff.
- `cargo fmt --all --check` and `git diff --check` passed. Scoped AFT inspection had no authoritative LSP diagnostics available in this worktree; the Linux Rust builds and compiled tests are the verification authority.
