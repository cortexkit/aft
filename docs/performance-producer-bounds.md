# Producer-bound performance audit

Measured on isolated temporary fixtures on a shared, heavily loaded macOS host. Times are single observations, not statistical throughput claims; concurrent builds caused large timing variance. Counts and admission outcomes are the stronger evidence. All fixture files are temporary and deleted by their owning tests.

## Findings and measurements

| Finding | Reproduced before | Change / after | Regression test |
|---|---|---|---|
| PERF-09 | 2,170,000-byte anchored candidate admitted; 23.260 ms | Exact corpus admission rejects before content read; 1.118 ms in focused run. Exact helper additionally caps the read itself against file growth. | `oversized_anchored_candidate_obeys_exact_byte_admission` |
| PERF-11 | 2,000 binary files all enumerated/read, no bound reason; 135.833 ms | Shared bounded walker examines 1,027 entries and reports enumeration limit. A later loaded-host run took 651.188 ms: bounded work, **not** a demonstrated latency improvement. | `fallback_bounds_enumeration_of_ineligible_files` |
| PERF-12 | One file plus alias/cycle: 100 repeated verifications, 3.811 s | Symlink-aware shared walker: 1 verification, 9.730 ms focused observation. | `fallback_does_not_follow_directory_aliases` |
| PERF-18 | 2,097,152-byte single line consumed; incorrectly `complete=true`; 8.523 ms | At most 2,004 text bytes scanned for that line, bounded buffer, `complete=false`, unknown total; 13.594 ms focused observation. Wide ranges stop near 50 KiB retained response data. Prefix lines and unrendered suffixes are skipped with a fixed-size buffer up to the existing 50 MiB whole-file ceiling; the original 1 MiB prefix cap was removed in review. | `ranged_huge_line_stops_at_response_budget`, `wide_range_stops_scanning_when_output_is_full` |
| PERF-19 | 11,000 directory entries enumerated; 18.435 ms | 10,001 examined including lookahead, 10,000 admitted, 1,000 rendered; 15.816 ms focused observation. Partial total explicitly marked inexact. | `directory_listing_stops_enumeration` |
| PERF-20 | `limit:1` counted 350,000 lines / 2,100,000 source bytes; 349.869 ms | Streaming prefix reads one line and byte lookahead; total omitted unless EOF observed; 2.715 ms focused observation. | `limit_only_read_does_not_count_unread_tail` |
| PERF-21 | 4,000,000-byte text fully counted as 2,000,000 lines; 130.003 ms | Binary sample only for oversized text; line count unknown, named line-count gap, incomplete response; 15.728 ms focused observation. Rollups no longer imply missing counts are zero. | `expensive_outline_line_count_is_unknown` |
| PERF-22 | 11,000 empty directories exhausted despite file cap; 3.117 s, `complete=true` | At most 10,001 examined entries including lookahead, bounded directory queue, explicit gap; 1.304 s focused observation. | `empty_directory_tree_has_entry_budget` |
| PERF-31 | 14-level doubled shared-callee graph: 32,767 nodes, 75.354 s | Store tree limited to 1,000 nodes; 2.795 s. SQL result batches limited before collecting; the store mutex remains held across the bounded traversal for graph coherence. Both store and adapter traversal share the 1,000-node limit and work-gap text. | `layered_diamond_call_tree_has_total_node_budget` |
| PERF-39 | 2 MiB newline-free stderr retained as one unfinished record | At most 65,536 UTF-16 code units per fragment; zero unfinished units at end of this fixture. Fragments explicitly disclose splitting and bytes shown; existing logical-line behavior preserved below cap. | `newline-free stderr retains at most 64KiB and discloses truncation` |
| PERF-41 | Delayed sink retained/wrote 2,097,152 burst bytes | 1,048,576 payload bytes admitted plus 123-byte overflow report; exactly 1,048,576 dropped bytes disclosed. Additional 4,096-record cap bounds tiny-record closure overhead. | `slow durable sink bounds queued bytes and records dropped bytes` |
| PERF-48 | Eight simultaneous 2048×2048 image reads: no capacity refusal, 24.105 s | Two worker-owned permits, six explicit capacity omissions, 18.984 s. Permit survives caller timeout until worker exits; overload does not queue image buffers. | `concurrent_image_reads_report_decoder_capacity` |

### Reachability and ordering

Exact fallback is reachable in current normal ranking code: `run_engine_ranking` substitutes `SearchIndex::new()` when the context index is unavailable, then passes that index to `ExactLane::search`; its `is_ready()` check chooses fallback. This is not merely the optional `None` test entry point. Fallback disclosures previously discarded by ranking are now carried to reply extras and text.

Small exhausted enumerations retain their existing ordering. Cut filesystem enumerations sort the admitted bounded subset, not a globally sorted prefix of an unexamined directory. Exact fallback voids page stability when enumeration is cut. Directory/files-mode outline replies disclose the gap instead of promising global completeness. A strict globally lexicographic prefix cannot be selected from an unordered filesystem iterator without exhausting it; this implementation prioritizes the requested producer work bound.

Line counts are exact only when EOF was reached. `limit`-only reads use the existing ranged-read contract permitting absent `total_lines`. Binary outline rows retain their previous `-` rendering; expensive nonbinary rows and affected rollups say `unknown`.

PERF-31 now bounds both `CallGraphStore::call_tree` and the distinct command-adapter traversal. The store retains its connection lock across the bounded traversal; the adapter retains its adjacency memoization and applies a shared node budget. Both serialize the same work-gap text, which is also rendered in the operator-facing call-tree reply.

### Ranking fence

`anchored_search_benchmark_rankings_remain_byte_identical` loads all nine checked-in anchored search benchmark fixtures into one indexed corpus and compares serialized ordered `CandidateResult` vectors against the original whole-file verifier for every fixture query. All nine are byte-identical. The existing 15 exact-engine, 8 fallback, and 7 trailer tests also pass. This is a focused anchored benchmark parity proof, **not** a replay of the external/full semantic quality benchmark corpus; that larger benchmark was not run.

## Captured pre-fix red results

- `oversized_anchored_candidate_obeys_exact_byte_admission`: `admitted=true`; `oversized source must not enter anchored verification` (1 failed, 13 filtered).
- `fallback_bounds_enumeration_of_ineligible_files`: `bound=None`; `enumeration must stop even when no file is eligible`.
- `fallback_does_not_follow_directory_aliases`: assertion `left: 100, right: 1` (both fallback additions failed; 3 existing filtered-suite tests passed).
- `ranged_huge_line_stops_at_response_budget`: assertion `left: Bool(true), right: false`.
- `limit_only_read_does_not_count_unread_tail`: `total_lines=350000`; `unread total must remain unknown` (both initial read tests failed).
- `directory_listing_stops_enumeration`: `reported=11000`; `must not exhaust the directory for a capped listing` (2 text-bound tests passed).
- `expensive_outline_line_count_is_unknown`: `lines=2000000`; `data["files"][0]["lines"].is_null()` failed.
- `empty_directory_tree_has_entry_budget`: assertion `left: Bool(true), right: false` (both outline tests failed).
- `layered_diamond_call_tree_has_total_node_budget`: `nodes=32767`; `total expansion must be bounded independently of depth` (1 failed, 38 filtered).
- `newline-free stderr retains at most 64KiB and discloses truncation`: expected `<=65536`, received `2097152`.
- `slow durable sink bounds queued bytes and records dropped bytes`: expected `<1100000`, received `2097152` (both bridge additions failed).
- `concurrent_image_reads_report_decoder_capacity`: `busy=0`; `concurrent decoders must have a finite admission capacity` (1 failed, 5 filtered).

## Verification

- `cargo check -p agent-file-tools --locked`: passed after each Rust area.
- `cargo check -p agent-file-tools --tests --locked`: passed, including library test compilation. This caught and prompted restoration of a test-only forward-call helper.
- Final combined `cargo test --features test-hooks -p agent-file-tools --test engine_anchored_test --test engine_fallback_test --test engine_exact_test --test engine_trailer_contract_test --test read_producer_bounds_test --test outline_producer_bounds_test -- --nocapture`: 53 passed.
- `cargo test --features test-hooks -p agent-file-tools --test callgraph_store_test -- --nocapture`: 38 passed, 1 ignored. Existing live poisoned-DB-copy test self-skipped because that optional fixture was absent.
- Bridge producer, durable-log, and transport tests: 36 passed; package `typecheck` and `build` passed. No manifests or lockfiles changed; no install required.
- Separate safe mutations disabled the stderr fragment cap and durable queue cap. Each named corresponding producer regression alone reddened while its sibling remained green. Both mutations were staged-safe and restored, with empty unstaged diffs afterward.
- AFT inspect timed out twice at LSP quiescence, including a single-file diagnostic scope. Cargo checks are the authoritative diagnostic evidence.
- Library-test execution attempts timed out during compilation under shared-host contention; public integration targets supplied the behavioral gates instead. Later `cargo check --tests` verified library-test types.

## Review revisions

- Removed the 1 MiB skipped-prefix cap. Prefix lines and unrendered long-line suffixes are discarded through `BufRead::fill_buf`/`consume`, without retaining their contents, subject only to the 50 MiB whole-file scan ceiling. Output/retained-line budgets still apply. Deep ranges keep the established whole-file `complete=false` slice convention while returning every requested line without a scan gap.
- Added `deep_range_skips_large_prefix_without_retaining_it`, `range_after_huge_line_returns_requested_lines`, and `requested_huge_line_is_truncated_without_hiding_later_lines`. The first two were run before the revision: both failed, while the existing wide-range and small Unicode-range controls passed. The deep range returned 0 instead of 51 lines; the post-huge-line range returned a scan-gap notice instead of `3: third\n4: fourth\n`.
- Restored a single connection lock across store tree expansion. Added the same shared 1,000-node budget to adapter expansion and a common work-gap message rendered by the call-tree formatter. `adapter_call_tree_shares_store_node_budget_and_gap` checks the adapter bound, matching structured disclosures, and operator-facing text.
- Ran workspace `cargo fmt`; formatting-only changes outside the revised functions normalize code introduced by the preceding area commits. Builds and tests for this revision run in the background with both `CARGO_BUILD_RUSTC_WRAPPER=` and `RUSTC_WRAPPER=`.
- Revision gates passed: 9 read producer tests, 16 callgraph envelope tests, and the adapter budget/formatter regression. The full store suite passed its 38 existing active tests (1 ignored); the initial adapter fixture was corrected to place repeated calls on distinct source lines because the adapter intentionally deduplicates same-line call sites, then its regression passed. `cargo check -p agent-file-tools --tests --locked` and `cargo fmt --check` both passed with wrappers disabled. Store diamond remained bounded at 1,000 nodes (4.491 s on this loaded run).
