# Nine-file magic-context refresh attribution (2026-09-19)

Evidence-only investigation; no resolver, row identity, index, or projection change.
The available replay does **not** establish a dependent wholesale-replacement bug.

## Specimen and replay

A consistent SQLite backup of `e274ab0872bb490b.g1789777429249238000.60983.sqlite`
was supplied privately (1,130,258,432 bytes); `PRAGMA integrity_check` returned `ok`
before replay. All database writes and source edits were on private copies.
Source was archived from clean magic-context HEAD
`d1d6fccfe1b7a7c9ba71857c89b9644a14cfc466`. The measured AFT base was
`f3d190cd36d69c7dd9959b5cd8ae085c6b458c52`, not the historical daemon card.

The nine most recently modified indexed `packages/**/*.ts` files still present in
that checkout were selected. Each received the prefix
`\nimport './nine-file-refresh-probe';\n`, shifting positions and adding an unresolved
side-effect import. No warmup consumed the transition. Files:

- `packages/e2e-tests/src/pi-harness.test.ts`
- `packages/e2e-tests/src/pi-harness.ts`
- `packages/pi-plugin/src/context-handler.ts`
- `packages/pi-plugin/src/tail-hygiene-walk-pi.test.ts`
- `packages/pi-plugin/src/tail-hygiene-walk-pi.ts`
- `packages/plugin/src/hooks/magic-context/tail-hygiene-defer-window.test.ts`
- `packages/plugin/src/hooks/magic-context/tail-hygiene-walk.test.ts`
- `packages/plugin/src/hooks/magic-context/tail-hygiene-walk.ts`
- `packages/plugin/src/hooks/magic-context/transform-postprocess-phase.ts`

Replay command (the private store directory has a `.current` pointer):

```sh
RUSTFLAGS=-Dwarnings \
AFT_CALLGRAPH_REFRESH_STORE="$PWD/.specimen/store" \
AFT_CALLGRAPH_REFRESH_ROOT="$PWD/.specimen/root" \
AFT_CALLGRAPH_REFRESH_PATHS="$PWD/.specimen/paths.txt" \
cargo test -p agent-file-tools --test callgraph_refresh_bench \
  bench_refresh_files_on_store_copy -- --ignored --nocapture
```

A separate run with `AFT_CALLGRAPH_REFRESH_COUNT_ROWS=1` measured row operations;
its trigger overhead is excluded from the byte measurements below. Private raw
logs: `.specimen/replay-before.log`, `replay-audit.log`, `replay-enhanced.log`.

## Byte and row attribution

| Measurement | Original harness | Enhanced index-key reporting |
| --- | ---: | ---: |
| WAL bytes (including frame headers) | 80,228,792 | 80,224,672 |
| 4 KiB WAL frames | 19,473 | 19,472 |
| Refresh process physical bytes | 342,609,920 | 342,605,824 |
| Refresh process logical bytes | 189,604,494 | 189,842,038 |
| Subsequent passive checkpoint physical/logical bytes | 0 / 0 | 0 / 0 |
| Passive checkpoint log/checkpointed pages | 19,473 / 19,473 | 19,472 / 19,472 |

These are repeated measurements of **unchanged product behavior**, not a fix's
before/after. The single-frame variation is in the refs primary-key index. The
refresh can autocheckpoint before measurement ends; the subsequent passive
checkpoint page count does not mean those pages were newly physically written.
Process physical writes on this filesystem exceed WAL payload plus one main-file
copy; do not equate process counters with an exact count of SQLite page writes.

| Table | DELETE | INSERT | UPDATE | Table WAL frames |
| --- | ---: | ---: | ---: | ---: |
| backend_file_state | 0 | 0 | 9 | 5 |
| dispatch_hints | 1,652 | 1,652 | 0 | 113 |
| edges | 1,758 | 1,758 | 565 | 468 |
| file_dependencies | 0 | 144 | 0 | 6 |
| files | 0 | 0 | 9 | 6 |
| meta | 0 | 1 | 1 | 4 |
| nodes | 375 | 375 | 1 | 38 |
| refs | 4,073 | 4,082 | 565 | 834 |

Other application tables recorded zero audited operations. Ref inserts involved
exactly nine caller files, **zero unchanged importers**. There were 19,269 selected
dependent refs but only 565 ref updates and 565 edge updates. This rules out
wholesale importer replacement in this replay, not all re-resolution CPU work.

All original-run WAL objects are listed below. Payload bytes = frames × 4,096;
these are frame occurrences, not necessarily distinct pages. Attribution uses
post-refresh `dbstat`, so freed pages are explicitly unmapped.

| Object | Frames | Payload bytes |
| --- | ---: | ---: |
| backend_file_state | 5 | 20,480 |
| dispatch_hints | 113 | 462,848 |
| edges | 468 | 1,916,928 |
| file_dependencies | 6 | 24,576 |
| files | 6 | 24,576 |
| meta | 4 | 16,384 |
| nodes | 38 | 155,648 |
| refs | 834 | 3,416,064 |
| sqlite_schema | 1 | 4,096 |
| idx_dispatch_hints_file | 44 | 180,224 |
| idx_dispatch_hints_method | 190 | 778,240 |
| idx_edges_ref_id | 2,488 | 10,190,848 |
| idx_edges_source_kind | 553 | 2,265,088 |
| idx_edges_target_file_symbol | 268 | 1,097,728 |
| idx_edges_target_kind | 820 | 3,358,720 |
| idx_file_dependencies_dep_file | 17 | 69,632 |
| idx_nodes_file | 19 | 77,824 |
| idx_nodes_name | 168 | 688,128 |
| idx_nodes_scoped | 146 | 598,016 |
| idx_refs_caller_file | 246 | 1,007,616 |
| idx_refs_caller_node_kind | 981 | 4,018,176 |
| idx_refs_kind_caller_file | 281 | 1,150,976 |
| idx_refs_short_name | 519 | 2,125,824 |
| idx_refs_target_file | 163 | 667,648 |
| sqlite_autoindex_backend_file_state_1 | 7 | 28,672 |
| sqlite_autoindex_dispatch_hints_1 | 2,443 | 10,006,528 |
| sqlite_autoindex_edges_1 | 2,198 | 9,003,008 |
| sqlite_autoindex_file_dependencies_1 | 43 | 176,128 |
| sqlite_autoindex_meta_1 | 1 | 4,096 |
| sqlite_autoindex_nodes_1 | 539 | 2,207,744 |
| sqlite_autoindex_nodes_2 | 23 | 94,208 |
| sqlite_autoindex_refs_1 | 5,795 | 23,736,320 |
| freelist-or-unmapped | 46 | 188,416 |

Only `sqlite_autoindex_nodes_2` explicitly includes position columns in its SQL
key: `(file_path,start_line,start_col,end_line,end_col,range_ordinal)`; it took
23 frames. **No SQL index key includes byte_start or byte_end.** However,
`node_id` hashes positions; `build_callable_refs`, `build_import_refs`, and
`build_dispatch_hints` hash line/byte positions into IDs. Ref/hint primary-key
indexes therefore churn when positions shift (5,795 and 2,443 frames), as do
node ID keys (539 frames) and edges that refer to changed IDs. The large edge
indexes are `(edge_id)` (2,198 frames) and `(ref_id,kind)` (2,488 frames).
Even secondary keys that contain only file/name columns undergo deletion and
insertion when row identities change. This is changed-file identity/index churn,
not evidence that their SQL keys contain explicit offsets.

Counting delete/reinsert pairs once, refs + hints + nodes account for 6,100
replacement/addition rows; including edges, dependent updates, and other rows
raises this to 9,162 insertion/update operations (plus 7,858 deletes). Thus the
19,473 frames are 3.19 per ref/hint/node replacement/addition, or 2.13 per total
insertion/update operation. Neither denominator is a count of unique changed
bindings. The row audit and object table above are the unambiguous measurements.

## Historical event and scratch-source evidence

The supplied 11:40–11:52Z log has **two separate** `legacy_callgraph_refresh`
events: 435,470,336 physical bytes at 11:48:17 and 821,813,248 at 11:48:28.
The latter is repeated verbatim by the nine-path tier-2 log. These are process-wide
windows, not a demonstrated single refresh followed by its checkpoint. They
coincide with watcher drains mentioning `.pure-replay-differential-zZstpA` (+687
paths, 83,698 ms) and `Y2hV5H` (+6,522 paths, 5,506 ms), and search corpus counts
moving 4,308 → 6,463 → 2,154. `concurrent_publications=0` does not exclude this
other process activity. No claim that the two windows are additive is justified.

The relevant dead-code line at 11:49:01 says `projection=spliced`,
`journal_bytes=8164`, `changed_files=156`, `scan=24547ms(9 files)`,
`snapshot=94793ms`, `projection_ms=1452`, `rollup=incremental`. The earlier
`projection=full reason=cold` belongs to a different linked worktree. There is
no evidence here of a whole dead-code projection rebuild causing the event.
The standalone replay read one copied generation; the historical sibling's
existence alone does not prove two writers or generation copying.

The specimen has 2,555 indexed files; 598 are absent from the source archive.
**85 of those 598** are under `.pure-replay-differential-*`, all under the stored
`4MVTDh/left` tree. That scratch namespace holds 18,879 refs (by caller_file),
1,828 nodes, 11,043 dispatch hints, and 2,473 edges (joining refs by ref_id, also
2,473 joining source nodes). There are 2,462 edges targeting scratch files;
source and target counts overlap and must not be added.

`git check-ignore -v --no-index` at the captured source HEAD returned exit 1,
no matching rule, for each of:

- `.pure-replay-differential-4MVTDh/left/ARCHITECTURE.md`
- `.pure-replay-differential-4MVTDh/left/AUDITOR.md`
- `.pure-replay-differential-4MVTDh/left/Cargo.toml`
- `.pure-replay-differential-Y2hV5H/left/.cortexkit/alfonso/release-notes/dashboard-v0.11.1.md`
- `.pure-replay-differential-Y2hV5H/right/packages/plugin/src/index.ts`
- `.pure-replay-differential-zZstpA/left/.cortexkit/alfonso/release-notes/dashboard-v0.11.1.md`

Only the drain's first path is logged, not all +6,522 names; the right-side
source path above is a representative probe, not a recovered event path.
The store and ignore probes demonstrate scratch source ingestion, warranting a
separate exclusion/lifecycle investigation. They do not recover the historical
transition or attribute the full 821 MB to those scratch paths.

## Limits and disposition

The supplied store is a later mutable state of the named generation; current
source is not its complete historical corpus. No every-table cold parity or
historical 821 MB reproduction is claimed. The parent explicitly chose an
evidence-only delivery rather than an unsupported resolver fix, position-ID
redesign, byte-bound assertion, or mutation proof. Private artifacts stay out of
git. The harness enhancement prints index owner/key columns, including SQL-less
autoindexes and expression placeholders, to make future page tables interpretable.

## Verification

- `RUSTFLAGS=-Dwarnings cargo test -p agent-file-tools --test callgraph_refresh_bench`:
  5 passed, 2 ignored (the offline probes); the specimen probe was run separately
  and passed before and after adding index-key reporting.
- `RUSTFLAGS=-Dwarnings cargo check -p agent-file-tools --test callgraph_refresh_bench --target x86_64-pc-windows-gnu`:
  passed. Windows execution was not attempted on the macOS host.
- `RUSTFLAGS=-Dwarnings cargo test -p agent-file-tools --lib callgraph_store`:
  incomplete; the command sequence timed out at 1,200 seconds during this suite,
  with several existing tests still running, no terminal suite result.
- `RUSTFLAGS=-Dwarnings cargo test -p agent-file-tools --test callgraph_store_test`:
  incomplete; timed out at 240 seconds after starting 38 tests. Neither timeout
  is represented as a passing suite or evidence of a product regression.
- Scoped `aft_inspect` exceeded its 120-second request budget. The harness host
  compilation/tests and Windows check above are the authoritative narrow gates.
- `git diff --check` and sidekick comment review passed.
