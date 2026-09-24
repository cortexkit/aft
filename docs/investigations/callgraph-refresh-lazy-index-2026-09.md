# Incremental refresh: on-demand file index loading (September 2026)

Every number below was measured with the harness in
`docs/investigations/scripts/callgraph-large-refresh-bench.rs` (see
[Reproduction](#reproduction)). Wall times on this shared machine are noisy;
row and file counts are exact.

## What changed

An incremental refresh resolves the refs of the files it re-parses against a
`ProjectIndex`. Until now `ProjectIndex::from_db_and_callers` built that index
by reading every `files` row, every `nodes` row and every `module`, `reexport`
and `export_alias` ref of the store, on every batch, however small.

It now loads a stored file's index (its `files` row, its `nodes` rows by
`file_path`, its module refs by `caller_file`, in the same order as before) the
first time resolution reaches that file, and keeps it for the rest of the
batch. Whether a path is an indexed file is a memoized point lookup over
`files` and `nodes`. The two index-wide scans (Rust module parents and Rust
inline modules) list candidate paths with one query per batch and load only
the files whose paths can match. The existing `idx_nodes_file` and
`idx_refs_kind_caller_file` indexes serve the per-file queries at both schema
sites, so no schema change was needed.

`RefreshFilesProfile` gained `index_files_loaded` and `index_rows_read`. Every
refresh batch now logs one `callgraph refresh index load:` line: at info level
when the index load takes at least 250 ms or reads at least 50,000 rows, at
debug level otherwise.

## Numbers

Store: cold build of oh-my-pi `e1a86ce4f9` (6,761 files, 91,785 nodes, 879,573
refs). The former load read all 6,761 files and 109,514 rows every batch
(6,761 `files` + 91,785 `nodes` + 10,968 module/re-export/alias refs).

| case | files loaded | rows read | index_load ms | total ms | heap peak MiB | max RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| comment-only save, `packages/coding-agent/src/cursor.ts` (before) | 6,761 | 109,514 | 1,117 | 1,399 | 62.1 | 116.4 |
| same (after) | 41 | 1,431 | 48 | 357 | 14.3 | 42.8 |
| hub-file edit, `session/agent-session.ts`, 89,463 dependent refs (before) | 6,761 | 109,514 | 1,498 | 21,226 | 4,572.6 | 5,250.9 |
| same (after) | 860 | 24,335 | 888 | 23,881 | 4,537.0 | 5,173.2 |
| created file importing through the `index.ts` barrel (before) | 6,761 | 109,514 | 1,118 | 1,751 | 50.3 | 103.9 |
| same (after) | 11 | 750 | 156 | 750 | 0.7 | 27.5 |
| 51-path batch `e1a86ce4f9` → `757b49a4bc`, 3 runs (before) | 6,761 | 109,514 | 1,781 / 2,193 / 6,127 | 43,209 / 54,601 / 116,502 | 4,919.3 | 5,636.5 / 5,641.1 / 5,620.5 |
| same, 3 runs (after) | 1,168 | 31,453 | 2,276 / 1,609 / 1,440 | 69,054 / 55,677 / 87,523 | 4,888.6 | 5,451.3 / 5,598.7 / 5,596.9 |

Reading the table:

- After the change `index_load` also counts loads made during resolution, so
  that time is included in `ref_resolution` as well. Before, the load was one
  separate phase.
- Small refreshes are where the index load dominated: the comment-only save
  spent 1.1 s of 1.4 s loading the index and now takes 0.36 s. The created
  file drops from 1.75 s to 0.75 s.
- Batches that pull in hundreds of dependent files read 70-78% fewer rows, but
  their index time does not fall much: they reach 860-1,168 files and pay a
  few queries per file. Their wall time and memory are set by parsing and
  dependency selection (`callgraph-large-refresh-2026-09.md`); the index was
  never more than a few tens of MiB of their ~4.9 GiB heap peak.
- The 51-path totals moved with machine load (the same binary varied 43-117 s).
  `ref_resolution` there is 1-3 s higher after the change (5.0 / 6.3 / 8.4 s
  before, 7.9 / 8.2 / 9.2 s after) because it now contains the lazy loads.

## Reproduction

```sh
git clone --no-hardlinks ~/Work/OSS/oh-my-pi .tmp/omp && git -C .tmp/omp checkout e1a86ce4f9
cp docs/investigations/scripts/callgraph-large-refresh-bench.rs \
   crates/aft/tests/callgraph_large_refresh_bench.rs
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo test --profile stage -p agent-file-tools \
   --test callgraph_large_refresh_bench --no-run
```

One cold build (`AFT_LRB_MODE=cold`) made the base store. Each case then ran
`AFT_LRB_MODE=refresh` in its own process on an APFS clone (`cp -cR`) of that
store, with private `XDG_DATA_HOME`, `XDG_CACHE_HOME` and `AFT_STORAGE_DIR`,
after applying the change to the checkout and listing the changed path in
`AFT_LRB_PATHS`:

- comment-only save: append `// aft bench edit` to
  `packages/coding-agent/src/cursor.ts`;
- hub-file edit: append a non-exported `function aftBenchLocal()` to
  `packages/coding-agent/src/session/agent-session.ts`;
- created file: `packages/coding-agent/src/aft-bench-new.ts` importing
  `getAgentDir` and `Settings` from `./index`;
- 51-path batch: check out `757b49a4bc` and list
  `git diff --name-only --no-renames e1a86ce4f9 757b49a4bc`.

"Before" is the binary built from the commit that added the two counters to the
former whole-store load; "after" is the on-demand load. The clone, stores and
harness copy were deleted afterwards.
