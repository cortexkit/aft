# Callgraph refresh write amplification investigation (September 2026)

## Summary

The 50–150 MB bursts are two effects that occur next to each other in the tier-2 log but have different causes:

1. A graph-neutral one-file refresh was still deleting and reinserting that file's rows when it contained import/value-reference provenance or qualified method calls. The equality check compared those rows against the wrong provenance and dispatch-hint IDs depended on `HashMap` iteration order.
2. The following dead-code snapshot was read-only at the database API, but SQLite materialized its 330k-row `ORDER BY` in a disk-backed temporary B-tree. On the 484 MB production-store copy, that read produced 104.65 MiB of physical writes with no WAL growth.

The fix makes graph-neutral refreshes update only the three freshness/revision rows and performs the snapshot's legacy ordering in Rust. The measured one-file WAL append fell from **2.904 MiB to 0.016 MiB**; checkpointed main-database payload fell from **2.887 MiB to 0.016 MiB**. The snapshot's physical-write delta fell from **104.65 MiB to 0.004 MiB** while preserving its order.

## Reproduction

The measurement used a consistent SQLite online backup of:

- source directory: `callgraph/90ff783f3f4c5cf2`
- source database size: 484 MiB
- project root: this checkout, with only the copied `backend_file_state.workspace_root` rerooted
- measured file: `crates/aft/tests/engine_comparator_test.rs`
- store shape at capture: 2,292 files, 34,640 nodes, 356,790 refs, 86,062 stored edges, 8,055 file dependencies, and 209,005 dispatch hints
- dead-code projection shape: 9,629 exports and 330,410 outbound rows

`crates/aft/tests/callgraph_refresh_bench.rs` runs as a standalone test process rather than through the daemon. It:

1. copies the live generation with SQLite's backup API;
2. normalizes the selected file once with the measured binary;
3. changes only the copied freshness marker, leaving source bytes and graph facts unchanged;
4. truncates the WAL and syncs the copied file set before sampling;
5. records WAL frame bytes, Darwin `proc_pid_rusage(RUSAGE_INFO_V4)` physical/logical deltas, `PRAGMA wal_checkpoint(PASSIVE)` frame counts, and the subsequent `TRUNCATE` result;
6. maps WAL page numbers to tables and indexes with `dbstat`; and
7. measures the dead-code snapshot separately after another truncate and sync.

Run the optimized probe with:

```sh
AFT_CALLGRAPH_REFRESH_STORE="$HOME/.local/share/cortexkit/aft/callgraph/90ff783f3f4c5cf2" \
AFT_CALLGRAPH_REFRESH_ROOT="$PWD" \
AFT_CALLGRAPH_REFRESH_FILE="crates/aft/tests/engine_comparator_test.rs" \
cargo test -p agent-file-tools --test callgraph_refresh_bench \
  bench_refresh_files_on_store_copy -- --ignored --nocapture
```

The baseline numbers below were taken with the pre-fix behaviour (unconditional same-file row replacement, `synchronous=FULL`, the 1,000-page autocheckpoint). That behaviour is not a switch in the product: to re-measure it, apply it as a scratch edit in `crates/aft/src/callgraph_store/mod.rs` (the three pragma sites in `configure_connection`/`configure_build_connection`/the reader opener and the `stored_extract_matches` guard in `refresh_files_profiled`), run the probe, and revert.

## What `refresh_files` writes

The selected file owned 1 `files` row, 12 nodes, 249 refs, 75 edges, and 140 dispatch hints. It had no file-dependency rows and selected no importing refs because its public surface was unchanged.

The indexed plans were bounded:

- dependent selection uses `idx_file_dependencies_dep_file`, `idx_refs_caller_file`, and `idx_refs_target_file`;
- edge deletion uses `idx_edges_ref_id`;
- method-reference selection uses `idx_refs_kind_caller_file` and primary-key lookups for files/nodes.

There is no `VACUUM`, `ANALYZE`, or `REINDEX` in refresh. There is also no persisted dead-code liveness/reachability table to rewrite: liveness is projected from files, nodes, refs, and edges after refresh.

### Baseline row replacement

The baseline appended 739 4 KiB frames (2.904 MiB). The dominant page owners were:

| Object | WAL pages | Payload MiB |
| --- | ---: | ---: |
| `sqlite_autoindex_refs_1` | 243 | 0.949 |
| `sqlite_autoindex_dispatch_hints_1` | 139 | 0.543 |
| `sqlite_autoindex_edges_1` | 73 | 0.285 |
| `idx_edges_ref_id` | 73 | 0.285 |
| `idx_refs_short_name` | 29 | 0.113 |
| `idx_refs_caller_node_kind` | 24 | 0.094 |
| `refs` table | 23 | 0.090 |
| `idx_dispatch_hints_method` | 20 | 0.078 |
| remaining tables/indexes | 115 | 0.449 |

The `DELETE ... WHERE caller_file/ref_id/file_path` statements are indexed, but replacing hundreds of rows updates every affected secondary index. That is why a few hundred logical rows produced hundreds of WAL frames even though no whole table was rewritten.

### Fixed graph-neutral refresh

The fixed refresh recognized that the extracted graph was identical and changed only:

- one `files` freshness row;
- one `backend_file_state` freshness row; and
- one durable projection-revision row in `meta`.

It appended four frames (16,512 bytes): one page each for `files`, `backend_file_state`, `meta`, and `sqlite_autoindex_backend_file_state_1`. No node, ref, edge, dependency, dispatch-hint, or importer row was replaced.

| Measurement | Baseline | Fixed |
| --- | ---: | ---: |
| WAL append | 2.904 MiB (739 frames) | 0.016 MiB (4 frames) |
| main-DB payload reported by passive checkpoint | 2.887 MiB (739 pages) | 0.016 MiB (4 pages) |
| refresh physical-write delta | 2.910 MiB | 0.004 MiB |
| refresh logical-write delta | 3.000 MiB | 0.059 MiB |
| checkpoint physical-write delta | 11.094 MiB | 0.082 MiB |
| checkpoint logical-write delta | 2.891 MiB | 0.023 MiB |
| `wal_checkpoint(TRUNCATE)` after passive checkpoint | `(0, 0, 0)` | `(0, 0, 0)` |

Physical checkpoint bytes exceed page payload because the process metric includes filesystem metadata, fsync/writeback behavior, and SQLite sidecar work; the passive checkpoint's page count is the direct main-database payload measurement.

## Why the following snapshot wrote about 105 MB

The old outbound projection ordered joined rows in SQLite by:

```sql
ORDER BY r.caller_file, n.name, r.line, r.byte_start, r.byte_end, r.ref_id
```

No index can directly provide this order because `n.name` comes from a left join. `EXPLAIN QUERY PLAN` ended with:

```text
USE TEMP B-TREE FOR ORDER BY
```

On the copied production store, the isolated read produced:

| Snapshot measurement | SQL `ORDER BY` | Rust ordering |
| --- | ---: | ---: |
| WAL delta | 0 | 0 |
| physical-write delta | 104.65 MiB | 0.004 MiB |
| logical-write delta | 229.00 MiB | 0.043 MiB |

This accounts for the observed physical burst adjacent to `tier2_callgraph_snapshot`: it was not a durable liveness projection or a whole-store refresh transaction. It was SQLite's temporary sorter. Five-second daemon buckets can show physical writeback after the logical temporary-file writes were issued, which explains a physical-only bucket next to the snapshot log.

The query now returns the same columns plus its six legacy sort keys without SQL ordering. Rust applies the same SQLite BINARY, NULL-first tuple order before materializing `CallgraphOutboundCall`. The plan no longer contains a temporary B-tree, and tests pin top-level/caller/byte order.

## Connection and checkpoint settings

Writer refresh connections use:

| Setting | Value | Reason |
| --- | --- | --- |
| `journal_mode` | `WAL` | readers do not block the incremental writer |
| `synchronous` | `NORMAL` | avoids a full sync for each tiny refresh commit |
| `wal_autocheckpoint` | 4,000 pages (16 MiB at 4 KiB/page) | avoids checkpointing each small burst |
| `cache_size` | -8,192 (8 MiB) | bounds the per-connection page cache |
| `mmap_size` | default 0 | no callgraph mmap write path is configured |
| busy timeout | 5 seconds | bounded writer contention |

The refresh worker checkpoints only when its queue becomes idle, and at most once per root per 60 seconds. A passive checkpoint measurement is followed by `TRUNCATE` in the harness; production uses a truncate checkpoint on the bounded idle cadence. WAL mode therefore still provides crash-safe atomic commits, while checkpoint work is separated from every individual refresh.

## Correctness and guards

The write-elision comparison now uses `ref_provenance(raw)` for both refs and their edges. Import and value-reference rows are no longer falsely compared against tree-sitter call provenance. Dispatch hints are built by sorted caller symbol so their ordinal-bearing IDs are stable across parses.

`one_file_no_graph_delta_refresh_appends_at_most_four_wal_pages_per_changed_row` builds a file with 256 functions, imports/value refs, and qualified dispatch calls. After a graph-neutral edit it requires exactly three logical row changes and caps the WAL at four pages per changed row. Reintroducing either provenance mismatch or nondeterministic dispatch-hint generation makes the test report the actual oversized WAL byte count.

`outbound_projection_sorts_in_rust_without_a_sqlite_temp_btree` asserts both sides of the snapshot change: the SQLite plan has no temporary B-tree, and output remains in the legacy NULL-first caller/file/line/byte order. `tool_call_parity_test` remains the end-to-end byte-output guard.
