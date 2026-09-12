# Incremental derived-view materialization

## Interface and durability

`apply_manifest_diff(path, base, next, callgraph_blob_database)` updates a **private copy** of the base generation's derived database. The publication owner supplies that copy and owns generation paths, copying/reflinks, fsync, pointer CAS and reader handles. This function does not replace a published file. One immediate SQLite transaction contains graph rows, dependency state and the manifest fingerprint; a missing blob or another error rolls it back. The previous generation's file remains unchanged.

The cold writer remains available through its old callgraph-store re-export. The materializer shares the existing graph schema, rather than duplicating it. Edges are owned by their `ref_id` through `refs.caller_file`; node IDs encode path, scoped name and AST ordinal, not SQLite rowids. Changed target ordinals therefore require incoming reference/edge relinking.

Derived metadata version **3** includes `view_manifest_fingerprint` and `view_materialization_version`. A mismatched fingerprint is refused. An older materialization version, or a legacy clone with neither diff metadata key, takes the cold path, preventing use of a database without binding dependencies. `view_bindings` is file-owned, and the existing `file_dependencies(file_path, dep_file)` table stores reverse-queryable dependency rows. Other legacy side tables are not populated by either view materialization path.

## Selection and remaining corpus work

Cold joins retain each bound reference's dependency candidates, resolved targets and source-file probes, including missing canonical paths. Binding caches use vector positions, not AST ordinals: real structural references can share an ordinal. Incremental joins start with changed paths and take the transitive reverse-dependency closure. Thus an unchanged barrel referring to a not-yet-existing module invalidates its unchanged importers when that module appears.

Binding work and reference work are separate. Unchanged callers reuse bindings unless membership probes changed. Candidate dependents replay the resolver-index queries consumed by their previous resolution (exports, aliases, nodes, modules and reexports); only changed answers require re-resolution. An unrelated appended export therefore does not invalidate a caller of an unchanged symbol. Queried files become dependencies even when they were not final targets. Rust crate-wide inline-module/parent lookups also depend on a module-index domain rechecked on manifest changes. Stable reference/edge tuples are not rewritten.

The resolver reads `package.json`, `tsconfig.json`, `pnpm-workspace.yaml` and `Cargo.toml` through its manifest facts. Changes to those names, explicitly marked resolution inputs, synthetic entries or symlink/gitlink identity force full resolution. Existing directory probes are excluded from persisted caller dependencies: workspace-discovery memo hits omit those incidental probes, and configuration changes invalidate discovery globally. File probes and misses are retained. Rust's absent declared-module candidates need recording **before** `FactPaths`' canonicalization can reject them.

The join still reconstructs the complete manifest's symbol index. Manifest facts are memoized for the duration of that immutable join, with dependency probes recorded on hits as well as misses. Row emission no longer decodes every unchanged blob a second time: it loads only changed owners and the callers/targets needed by actual emitted references. The optimization bounds expensive binding/resolution work, not every CPU instruction, to changed files and affected consumers. There is no separate semantic-plane materialization in this function.

## Measurements (Darwin, debug builds)

Input was copied from opencode view `0f3900af641f5248` and immutable callgraph blob database `aa69d52ef2dcad4d`; no probe writes to live storage. Consecutive retained manifests **14 → 15** contain **17 changed entries**, not 300: 16 removals and one addition; total membership 7,060 → 7,045. This is the real pair available in the supplied artifact. A separate controlled fixture covers 300 changes.

All bytes below are literal byte counts. Physical/logical counters use Darwin `RUSAGE_INFO_V4`; CPU uses `getrusage`; WAL is measured while a keeper connection remains open. Both branches start from copies of the same freshly materialized base, use WAL with the existing writer defaults, and include writer connection close/checkpoint effects. Copying and base preparation are outside the measured interval. Row parity includes **every derived table**, including metadata and dependency caches.

### Real 17-path pair

| implementation | physical bytes | logical bytes | WAL bytes | wall seconds | CPU seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Full rewrite before dependency caching | 541,216,768 | 1,027,746,902 | 271,133,112 | 289.908 | 278.424 |
| Write-bounded, still full resolution | 19,386,368 | 11,748,000 | 4,379,592 | 297.580 | 279.096 |
| Version 2 full rewrite, including dependency cache | 674,893,824 | 1,160,887,532 | 336,517,512 | 220.138 | 214.764 |
| Version 2 incremental binding/resolution | **28,332,032** | **16,664,736** | **6,752,712** | **90.804** | **89.920** |
| Version 3 full rewrite, including surface cache | 848,138,240 | 1,350,133,088 | 424,739,072 | 199.779 | 188.604 |
| Version 3 surface-pruned incremental | **34,279,424** | **25,155,920** | **9,595,512** | **49.467** | **48.519** |

Version 2 incremental selection: **529 unchanged dependents**, 530 current files resolved, 78,989 references resolved, versus cold 4,921 files / 299,487 references. Graph writes: 949 owned deletions, 43 owned insertions, 28 relink deletions, 15 relink insertions. Dependency-cache writes: 784 deletions and 415 insertions. Physical writes are 27.02 MiB, rather than hundreds of MiB. The cold CPU numbers vary across debug runs on a shared machine; the final paired measurement is the like-for-like comparison.

Version 3 reduces that to **29 unchanged dependents**, 30 current files and **7,546 references** resolved. Its physical write delta is 32.69 MiB and CPU is 48.519 seconds, versus 188.604 seconds for the paired full rewrite. Surface-cache rows add storage and modest write overhead compared with version 2 while eliminating most dependent reference work.

The real parity gate initially caught 20 excess dependency rows caused by incidental workspace-directory probes. Excluding existing directories while retaining file probes/misses fixed that discrepancy. The surface-cache gate subsequently exposed a real `tool.ts` reexport/export-alias ordinal collision: keying cached bindings by ordinal merged distinct dependencies. Unique vector-position cache keys fix it, while emitted reference rows preserve the original cold writer's first-reference lookup. The final version 3 run passes every-table parity.

### Controlled 300 changed paths out of 5,056 TypeScript files

Each file has a local caller and callee; the first 300 files receive a leading newline. This is not a claim about 300 real drill changes.

| operation | physical bytes | logical bytes | WAL bytes | wall seconds | CPU seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Full rewrite | 30,253,056 | 21,394,768 | 10,089,912 | 4.308 | 3.392 |
| Incremental | 18,010,112 | 21,485,816 | 8,132,912 | 1.919 | 1.675 |

Incremental resolves exactly 300 files / 300 references, with zero unchanged dependents. It writes 3,000 graph rows plus 1,200 dependency rows, versus 50,560 graph rows plus 20,224 dependency rows cold. SQLite page/index locality and checkpoint effects mean this small fixture's logical byte counter does not improve despite exact row bounds. All-table parity passes.

## Reproduction and guards

Place copied `base.json`, `next.json` and `callgraph.sqlite` in an offline input directory, then run:

```sh
AFT_VIEW_DIFF_INPUT="$PWD/target/view-diff-input" cargo test -p agent-file-tools --lib views::materialization::tests::bench_real_manifest_diff -- --ignored --exact --nocapture
cargo test -p agent-file-tools --lib views::materialization::tests::bench_controlled_300_path_diff -- --ignored --exact --nocapture
cargo test -p agent-file-tools --lib views::materialization::tests
```

The real probe prints and retains its measured database directory under the offline input so parity failures can be inspected without rerunning the expensive cold build. The comparison reports only the first mismatching table and a bounded row sample.

The small edit/add/remove fixture writes exactly **13 graph rows + 6 dependency/surface-cache rows**. The original full rewrite performs **23 graph writes**. Mutation controls demonstrate that skipping incoming relinks fails edge parity, forcing a full rewrite fails exact work counts, skipping transitive dependents fails the new-reexport edge, and omitting missing canonical probes fails the Rust module-addition fixture. Each mutation is restored before any commit.
