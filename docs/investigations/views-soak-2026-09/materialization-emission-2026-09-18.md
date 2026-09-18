# Views materialization emission and cache-row reconciliation — 2026-09-18

## Scope and input

This run used the retained opencode HEAD → A pair copied out of isolated view storage. The manifests contain 7,060 → 7,045 entries and 285 changed manifest entries for the 300-Git-path checkout transition. The harness refuses other inputs by its canonical decoded-manifest fingerprints (`ffc2ece6…` → `e0b49e85…`). It creates a cold target database once, then clones the same cold base for each incremental repetition and checks every derived table against the cold target.

Command:

```sh
AFT_VIEW_PROFILE=1 AFT_VIEW_BENCH_REPETITIONS=3 \
  AFT_VIEW_DIFF_INPUT="$PWD/target/view-diff-real-300-input" \
  cargo test --release -p agent-file-tools --lib \
  views::materialization::tests::bench_real_manifest_diff -- \
  --ignored --exact --nocapture
```

SQLite page writes are deltas of `SQLITE_DBSTATUS_CACHE_WRITE`; checkpoint pages are WAL frame counts. The before harness originally truncated directly, so its checkpoint counts below are reconstructed exactly from the 149,222,312-byte WAL (`(bytes - 32) / (4,096 + 24)`); the after harness reads the same count from a passive checkpoint before truncation. Index maintenance remains incremental, so its cost and page writes are included in each affected delete/emission row. The explicit zero row proves the phase was observed and not omitted. Clone uses copy-on-write and therefore reports zero SQLite page writes.

## Before

Host load was 12.25–13.17 during the measured calls, so these wall observations are loaded-machine results rather than isolated release certification.

| run | phase | wall ms | SQLite page writes |
|---:|---|---:|---:|
| 1 | clone | 9.526 | 0 |
| 1 | selected join | 1,843.642 | 0 |
| 1 | delete rows | 894.559 | 23,423 |
| 1 | emit files | 6.064 | 58 |
| 1 | emit nodes | 40.476 | 1,312 |
| 1 | emit view file surfaces | 3.038 | 307 |
| 1 | emit file dependencies | 54.438 | 2,787 |
| 1 | emit view bindings | 67.684 | 12,279 |
| 1 | emit refs | 366.709 | 18,682 |
| 1 | emit edges | 149.499 | 8,985 |
| 1 | index maintenance | 0.000 | 0 |
| 1 | checkpoint | 760.776 | 36,219 |
| 2 | clone | 4.761 | 0 |
| 2 | selected join | 1,691.820 | 0 |
| 2 | delete rows | 840.098 | 23,423 |
| 2 | emit files | 5.359 | 58 |
| 2 | emit nodes | 38.351 | 1,312 |
| 2 | emit view file surfaces | 3.348 | 307 |
| 2 | emit file dependencies | 60.312 | 2,787 |
| 2 | emit view bindings | 91.589 | 12,279 |
| 2 | emit refs | 442.940 | 18,682 |
| 2 | emit edges | 198.987 | 8,985 |
| 2 | index maintenance | 0.000 | 0 |
| 2 | checkpoint | 493.774 | 36,219 |
| 3 | clone | 8.361 | 0 |
| 3 | selected join | 1,946.710 | 0 |
| 3 | delete rows | 1,776.215 | 23,423 |
| 3 | emit files | 7.120 | 58 |
| 3 | emit nodes | 42.763 | 1,312 |
| 3 | emit view file surfaces | 3.267 | 307 |
| 3 | emit file dependencies | 60.342 | 2,787 |
| 3 | emit view bindings | 72.192 | 12,279 |
| 3 | emit refs | 594.086 | 18,682 |
| 3 | emit edges | 231.693 | 8,985 |
| 3 | index maintenance | 0.000 | 0 |
| 3 | checkpoint | 762.629 | 36,219 |

Materialization wall / CPU / physical bytes were 5.454 s / 4.395 s / 151,359,488; 5.353 s / 4.147 s / 151,326,720; and 7.346 s / 4.999 s / 151,359,488. WAL size was 149,222,312 bytes in every run.

## Change

Changed cache owners are no longer deleted before replacement. `view_bindings` and `view_file_surfaces` use in-place UPSERTs, while removed owners are deleted explicitly. Changed-owner dependency rows are reconciled through a temporary primary-key table with set-based delete/insert. This preserves the existing logical row counters and the one-transaction publication boundary, but avoids deleting and reinserting unchanged B-tree cells for the largest payload table.

Emission is staged in memory by table and executed through one prepared statement per table. This does not relax any parity comparison. A measured experiment that dropped and rebuilt secondary indexes reduced delete/emission time but made index recreation the largest phase (~2.0–2.2 s), increased WAL from 149,222,312 to 186,945,032 bytes, and raised physical writes from ~151 MB to ~194 MB, so it was rejected.

## After

Host load was 10.70–11.19 during these measured calls, still above the acceptance threshold. The work and byte deltas are the defensible comparison; wall time is reported as provisional.

| run | phase | wall ms | SQLite page writes |
|---:|---|---:|---:|
| 1 | clone | 16.677 | 0 |
| 1 | selected join | 1,759.359 | 0 |
| 1 | delete rows | 991.185 | 19,999 |
| 1 | emit files | 0.276 | 6 |
| 1 | emit nodes | 17.923 | 1,096 |
| 1 | emit view file surfaces | 2.104 | 521 |
| 1 | emit file dependencies | 29.146 | 798 |
| 1 | emit view bindings | 34.838 | 4,572 |
| 1 | emit refs | 392.611 | 19,255 |
| 1 | emit edges | 137.420 | 7,384 |
| 1 | index maintenance | 0.000 | 0 |
| 1 | checkpoint | 318.987 | 26,011 |
| 2 | clone | 4.148 | 0 |
| 2 | selected join | 1,898.596 | 0 |
| 2 | delete rows | 1,007.527 | 19,999 |
| 2 | emit files | 0.313 | 6 |
| 2 | emit nodes | 19.808 | 1,096 |
| 2 | emit view file surfaces | 2.099 | 521 |
| 2 | emit file dependencies | 28.732 | 798 |
| 2 | emit view bindings | 35.659 | 4,572 |
| 2 | emit refs | 303.427 | 19,255 |
| 2 | emit edges | 122.583 | 7,384 |
| 2 | index maintenance | 0.000 | 0 |
| 2 | checkpoint | 380.784 | 26,011 |
| 3 | clone | 8.233 | 0 |
| 3 | selected join | 1,865.709 | 0 |
| 3 | delete rows | 1,057.575 | 19,999 |
| 3 | emit files | 2.496 | 6 |
| 3 | emit nodes | 26.515 | 1,096 |
| 3 | emit view file surfaces | 2.107 | 521 |
| 3 | emit file dependencies | 27.412 | 798 |
| 3 | emit view bindings | 34.270 | 4,572 |
| 3 | emit refs | 332.338 | 19,255 |
| 3 | emit edges | 165.346 | 7,384 |
| 3 | index maintenance | 0.000 | 0 |
| 3 | checkpoint | 323.334 | 26,011 |

Materialization wall / CPU / physical bytes were 5.368 s / 4.403 s / 109,301,760; 5.368 s / 4.498 s / 109,268,992; and 5.479 s / 4.427 s / 109,301,760. WAL size was 107,165,352 bytes in every run. Physical writes fell 27.8–27.9%, WAL fell 28.2%, and cache-write pages in the individually attributed rows fell from 67,833 to 53,629. Every run passed all-table cold parity. Binding and surface payloads are decoded in parallel; this reduces allocation latency without changing stored bytes or row sets.

The requested ≤2.5 s offline wall target is not demonstrated. All measurements are provisional because host load exceeded 3. The phase table still identifies selected join, delete rows, and ref/edge emission as the remaining buckets; no unmeasured or byte-regressing index strategy is included.

## Fresh-storage in-daemon drill

The complete both-arm drill used fresh storage and the release binary after cache-row reconciliation, before the final parallel-deserialization-only commit. Every probe was correct and the report contained no defects. Host load was 17.01–31.61, so every timing is provisional. Views used less CPU than legacy in all four rows, satisfying the relative CPU condition four of four despite missing the absolute materialization target.

| switch | derived materialization ms | views CPU s | legacy CPU s | views correct ms | legacy correct ms | materialize physical bytes |
|---|---:|---:|---:|---:|---:|---:|
| HEAD → A | 12,577 | 28.85 | 197.32 | 23,757 | 99,858 | 122,359,808 |
| A → HEAD | 7,002 | 12.79 | 125.84 | 11,954 | 73,021 | 92,389,376 |
| HEAD → B | 6,783 | 13.66 | 65.88 | 11,332 | 66,159 | 94,777,344 |
| B → HEAD | 6,713 | 13.87 | 39.63 | 10,435 | 36,235 | 93,024,256 |

Physical bytes did not rise: all four in-daemon materializer intervals are below the retained pair's 151,326,720–151,359,488-byte baseline. The derived phase remains above 5 seconds in every row. A final fresh-storage rerun with the parallel-deserialization commit could not pass cold readiness because the configured embedding backend at `localhost:1234` refused connections; it produced no switch rows, restored the opencode checkout to `5716f8ba60e7`, and is not substituted for the complete table above.

## Scratch cleanup

After measurements, all retained manifest/blob pairs, generated databases, mutation logs, the complete 5 GB drill storage, and the failed 1.1 GB final-drill storage were removed from `target/`. The input is reproducible from the documented fingerprints; no measurement scratch is committed.

## Mutation controls

Both controls staged the live implementation first, observed an empty working diff, applied a `NON-VACUITY BREAK`, observed a non-empty diff, ran one named test, then restored with `git checkout -- <path> && touch <path>` and observed an empty diff.

- Replacing the `files` delete with a count made only `views::materialization::tests::incremental_rows_match_cold_with_cross_file_relink` fail. This proves cold parity sees a skipped table delete.
- Omitting the observed `index_maintenance` row made only `views::materialization::tests::incremental_phase_table_requires_every_phase_timed` fail with `index_maintenance`. This proves a phase cannot disappear from the table while the benchmark remains green.
