# Incremental derived-view materialization

## Interface and durability

`apply_manifest_diff(path, base, next, callgraph_blob_database)` updates a **private copy** of the base generation's derived database. The publication owner supplies that copy and owns generation paths, copying/reflinks, fsync, pointer CAS and reader handles. This function does not replace a published file. One immediate SQLite transaction contains graph rows, dependency state and the manifest fingerprint; a missing blob or another error rolls it back. The previous generation's file remains unchanged.

The cold writer remains available through its old callgraph-store re-export. The materializer shares the existing graph schema, rather than duplicating it. Edges are owned by their `ref_id` through `refs.caller_file`; node IDs encode path, scoped name and AST ordinal, not SQLite rowids. Changed target ordinals therefore require incoming reference/edge relinking.

Derived metadata version **4** includes `view_manifest_fingerprint` and `view_materialization_version`. A mismatched fingerprint is refused. An older materialization version, or a legacy clone with neither diff metadata key, takes the cold path, preventing use of a database without binding dependencies. `view_bindings` is file-owned, and the existing `file_dependencies(file_path, dep_file)` table stores reverse-queryable dependency rows. Other legacy side tables are not populated by either view materialization path.

## Selection and remaining corpus work

Cold joins retain each bound reference's dependency candidates, resolved targets and source-file probes, including missing canonical paths. Binding caches use vector positions, not AST ordinals: real structural references can share an ordinal. Incremental joins start with changed paths and take the transitive reverse-dependency closure. Thus an unchanged barrel referring to a not-yet-existing module invalidates its unchanged importers when that module appears.

Binding work and reference work are separate. Unchanged callers reuse bindings unless membership probes changed. Candidate dependents replay the resolver-index queries consumed by their previous resolution (exports, aliases, nodes, modules and reexports); only changed answers require re-resolution. An unrelated appended export therefore does not invalidate a caller of an unchanged symbol. Queried files become dependencies even when they were not final targets. Rust crate-wide inline-module/parent lookups also depend on a module-index domain rechecked on manifest changes. Stable reference/edge tuples are not rewritten.

The resolver reads `package.json`, `tsconfig.json`, `pnpm-workspace.yaml` and `Cargo.toml` through its manifest facts. Changes to those names, explicitly marked resolution inputs, synthetic entries or symlink/gitlink identity force full resolution. Existing directory probes are excluded from persisted caller dependencies: workspace-discovery memo hits omit those incidental probes, and configuration changes invalidate discovery globally. File probes and misses are retained. Rust's absent declared-module candidates need recording **before** `FactPaths`' canonicalization can reject them.

The join restores compact persisted per-file symbol surfaces and rebuilds only changed or membership-invalidated entries. Manifest facts and lazily loaded immutable payloads are memoized for the duration of that join, with dependency probes recorded on hits as well as misses. Unchanged callers decode only after their consumed surface queries change. Row emission loads only changed owners and the callers/targets needed by actual emitted references. Restoring compact lookup maps and loading binding caches still scales with the manifest; expensive source/AST decode, binding and resolution do not. There is no separate semantic-plane materialization in this function. Historical version 3 measurements below predate persisted file surfaces.

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

## Release branch drill after integration

Two both-arm runs used this worktree's optimized `aft` binary, copied warm non-view caches, and a fresh view directory so the warm-up built the matching materialization schema. The opencode checkout and baseline were restored by the drill; the live daemon and live view storage were not subjects. Generated reports in the investigation directory were copied to `target/` and restored rather than committed over the existing investigation.

### First run: dependency closure without consumer-surface pruning

| switch | publication ms | views CPU s | legacy CPU s | views correct ms | legacy correct ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| HEAD → a085bf62a459 | 67,657 | 72.18 | 30.23 | 67,930 | 27,354 |
| a085bf62a459 → HEAD | 63,562 | 68.32 | 28.01 | 63,562 | 23,646 |
| HEAD → upstream/v2-timeouts | 61,571 | 66.47 | 28.85 | 61,571 | 24,329 |
| upstream/v2-timeouts → HEAD | 63,312 | 68.82 | 30.98 | 63,640 | 25,963 |

All correctness probes converged. The drill incorrectly reported 15 puts from the absolute manifest membership delta because it did not parse the new root-owned publication phase profile. Actual profile counters were zero. The attribution fix now prefers those profiles; its unit test goes red if they are ignored.

### Final run: consumer surfaces, unique binding positions, lazy row emission

Measured code: `d81a33a4` (optimized build), observed **2026-09-12T20:04:02Z**. Warm-up: views 76,294 ms; legacy 5,506 ms. This is the actual captured table; the script's static narrative and historical SHA label are not reused as attribution.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | 70103 | 0 | 0 | 77.64 | 1935.953 | 70646 | — | — | 3 | 78.1 | 2689.516 | 65825 |
| a085bf62a459→HEAD | 70415 | 0 | 0 | 75.7 | 25.078 | 71031 | — | — | 4 | 68.71 | 101.141 | 56743 |
| HEAD→refs/remotes/upstream/v2-timeouts | 66092 | 0 | 0 | 71.48 | -45.141 | 66092 | — | — | 3 | 68.03 | -331.625 | 55434 |
| refs/remotes/upstream/v2-timeouts→HEAD | 64636 | 0 | 0 | 70.63 | -87.438 | 65024 | — | — | 4 | 60.58 | -447.469 | 52275 |

Every correctness probe passed, every views publication put/embedded zero blobs/batches, and the script reported no defects. **The stronger shipping criterion is still unmet: views does not beat legacy on every case.** It is slower to correctness on all four transitions, and uses more CPU on three. Do not treat a zero-defect script exit as a performance pass. The substantially different legacy CPU times between runs also rule out presenting these wall/CPU observations as a controlled cross-run speedup.

Profile attribution for the final run:

| switch | unchanged consumers re-resolved | current files resolved | references resolved | derived phase ms | CAS ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| HEAD → A | 296 | 556 | 93,230 | 37,632 | 13 |
| A → HEAD | 296 | 569 | 95,222 | 35,499 | 17 |
| HEAD → B | 323 | 583 | 96,254 | 34,951 | 13 |
| B → HEAD | 323 | 596 | 98,078 | 33,174 | 13 |

Compared with the first run's 1,335 / 1,146 unchanged dependents and 188,185 / 174,750 forward references, selection is materially smaller. However, complete symbol-index reconstruction, emitting tens of thousands of changed-owner rows, and persisted surface-query cache size remain material costs. The final derived database is approximately 424–428 MB versus 335–339 MB before surface recording. This delivery establishes parity, bounded writes and narrower resolution; it does not establish that enabling views is ready to ship.

Retained local evidence: `target/branch-drill-surface.json`, `target/branch-drill-surface.stderr.log`, and `target/view-diff-input/.tmpuNg1g2/{base,cold,incremental}.sqlite`. These are offline artifacts, not live stores.

### Offline measurement of the real 300-Git-path transition

The final drill produced a better input pair than the older retained artifact: its generations 1 → 2 correspond to HEAD → A, **300 changed Git paths and 276 changed manifest entries**. Those immutable manifests and the closed blob database were copied inside this worktree and measured separately with the same benchmark. This is additional offline measurement, not another drill run. The initial read-only Python backup opener returned `SQLITE_CANTOPEN`; a main-file clone of the closed, WAL-free callgraph blob database succeeded instead.

| operation | physical bytes | logical bytes | WAL bytes | wall seconds | CPU seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Full rewrite | 846,159,872 | 1,340,615,544 | 422,897,432 | 263.089 | 208.216 |
| Incremental | 379,342,848 | 487,851,246 | 143,586,152 | 291.479 | 167.437 |

Every-table parity passes on this real transition. Incremental work is 110,596 graph-row operations plus 36,923 dependency/surface-cache operations, versus 821,633 plus 344,787 cold; 296 unchanged consumers / 556 total files / 93,230 references are resolved. **The requested tens-of-MiB write target is not met on this larger real transition:** incremental physical writes are 361.77 MiB, though below 806.96 MiB cold. Wall time also did not improve in this offline sample. The successful 17-entry measurement must not be substituted for this larger case. Combined with the final drill, this is an explicit remaining acceptance gap, not a shipping recommendation.

Retained databases: `target/view-diff-real-300-input/.tmpoSckwe/{base,cold,incremental}.sqlite`.

## Release profiling prerequisite run (2026-09-12, base e2589c10)

The previous worker's offline artifacts were unavailable in the new worktree. A fresh release binary (`cargo build --release -p agent-file-tools --bin aft`, passed) regenerated the input with `scripts/views-branch-drill.sh --mode both --binary "$PWD/target/release/aft" --storage "$PWD/target/branch-drill-baseline"`. No optimization was applied. The script restored opencode to `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252` with a clean checkout. Generated reports were retained under `target/branch-drill-baseline.{json,md}` rather than changing the historical investigation reports.

Generations 1 → 2 are **7,060 → 7,045 entries and 276 changed entries** across 300 Git paths. The membership-count difference of 15 is not the changed-entry count. Fingerprints:

- Base: `c3d6ca11eca18e25d2e58e52629921215cdea1d4f23ce9278941c1a13faa95d1`
- Next: `a632b1603f62b77a16c041435b34764810ec2919461ae14fba9c2a70b49fdace`

The pair is copied to `target/view-diff-real-300-input/{base,next}.json`, beside the supplied offline `callgraph.sqlite`. Equality comparison of entries keyed by `rel_path`, including plane keys and metadata, confirms 276 changes. These fingerprints must not be classified as a 17-entry pair merely from membership counts.

### Unmodified release drill baseline

Observed `2026-09-12T22:16:10Z`. Views warm-up: 561,819 ms; legacy warm-up: 2,511 ms. Fresh storage did not reproduce the earlier zero-embedding forward legs. The generated report's static narrative claims zero embeddings, but the actual table below does not; only the table is evidence for this run.

| switch | views publication ms | views puts | views embeds | views CPU s | views correct ms | legacy embeds | legacy CPU s | legacy correct ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HEAD → A | 39093 | 0 | 3 | 104.41 | 39093 | 3 | 30.89 | 27679 |
| A → HEAD | 35377 | 0 | 0 | 56.23 | 35376 | 4 | 27.93 | 23747 |
| HEAD → B | 38490 | 0 | 1 | 61.68 | 38794 | 3 | 29.73 | 25009 |
| B → HEAD | 35554 | 0 | 0 | 56.77 | 35857 | 4 | 30.25 | 25513 |

Views remains slower to correctness by 11,414 / 11,629 / 13,785 / 10,344 ms. This is an unmodified baseline, not a before/after optimization comparison or a shipping pass.

### Blocked release benchmark

`cargo test --release -p agent-file-tools --lib views::materialization::tests::bench_real_manifest_diff --no-run` fails before producing a benchmark executable: 17 E0425 errors in `gh_shim.rs` tests reference `DEV_MANIFEST_KEY_ID` / `DEV_MANIFEST_PUBLIC_KEY`, whose definitions are gated by `#[cfg(debug_assertions)]` at lines 2180–2183. The production release binary builds, but the release library test target does not. That file is outside the materialization/selected-join fence, and changing security-key compilation or enabling debug assertions was not used as a measurement workaround. Fix the release-test cfg mismatch separately, then run the release benchmark on the retained pair with sampling before selecting an optimization. No bucket attribution, optimized measurements, work-count mutation proofs, or parity/warnings acceptance is claimed by this prerequisite run.

## Release profile and persistent file surfaces (resumed)

The release-test prerequisite was supplied as `57be7181d` and cherry-picked without modifying its test-cfg changes. The original drill table above is retained verbatim. After the task's original `target/` artifacts were reclaimed, a second unmodified both-arm drill regenerated the same two fingerprints and **276** changed entries; its reports are `target/branch-drill-recovered.{json,md}`. The offline input now uses a SQLite backup of that isolated drill's blob database, not live storage.

### Release attribution before the optimization

`cargo test --release -p agent-file-tools --lib views::materialization::tests::bench_real_manifest_diff --no-run` passed. The resulting test executable was run with the real input while `/usr/bin/sample <pid> 180 10 -file target/real-release-before.sample.txt` sampled its lifetime (the benchmark exited after 90.25 s). Every-table parity passed. This sample includes base preparation, cold replacement, incremental replacement and snapshot comparison: its aggregated stack counts must not be labeled incremental-only CPU. The stacks show manifest blob reads through SQLite `pread`, decoding/binding in the selected join, resolver work, SQLite B-tree writes, and checkpoint I/O. A subsequent same-input run with `AFT_VIEW_PROFILE=1` supplies **wall-time phase boundaries**, not inferred CPU times, below. SQLite index maintenance is included with the corresponding writes; commit includes synchronization and checkpoint work and is not a pure fsync counter.

| incremental phase | before ms | persistent surfaces ms |
| --- | ---: | ---: |
| Binding-cache load and dependency selection | 811.948 | 616.234 |
| SQLite owned deletions, including indexes | 1688.395 | 1043.251 |
| Changed-owner blob decode and node/file inserts | 798.042 | 565.182 |
| Eager whole-manifest blob fetch | 3339.224 | 0.000 |
| Blob decode, binding and file symbol-index construction/restoration | 4756.353 | 895.065 |
| Global index setup and consumer-surface replay | 60.083 | 52.130 |
| Deferred decode/bind of consumers that failed surface replay | included above | 443.226 |
| Reference resolution and surface/dependency recording | 2016.768 | 2111.027 |
| Dependency union | 60.367 | 74.248 |
| Entire selected join, including allocations/drop overhead | 10289.146 | 3600.256 |
| SQLite binding-cache writes | 192.922 | 232.496 |
| Ref/edge emission, relink comparisons, lazy blob decode and indexes | 2163.370 | 2079.154 |
| Metadata and transaction commit | 630.292 | 565.245 |
| Entire materialization, including connection-close overhead | **16646** | **8766** |

Nested join rows are subdivisions, not additional time to add to the entire selected join. Decode and symbol reconstruction are grouped where they share the same loop; these numbers do not pretend to distinguish their individual CPU costs. The largest actionable combined bucket was reading/decoding/binding all manifest payloads to reconstruct file indexes (~8.1 s), not the global `ProjectIndex::from_parts` setup (~60 ms including surface replay).

### Mechanism and work proof

Materialization version **4** persists a deterministic compact per-file resolver surface in `view_bindings`, alongside the existing generation-owned binding dependencies. It stores symbol/export/module/reexport lookup data but no source, AST or call sites. Unchanged entries restore their surface without opening their blob; changed entries or membership-invalidated bindings rebuild it. Existing configuration invalidation still forces a cold join. Consumer surface replay runs before decoding unchanged callers: only consumers actually selected for re-resolution decode/bind their call sites. No process-global cache, checkout read, or publication/orchestration change is involved. The existing `JoinResult` implementation is unchanged.

On the real pair only **260 current changed parse entries rebuild surfaces**, versus 4,921 cold. Only **556 caller blobs decode**, corresponding to the 260 changed files and 296 consumers whose results may change. Reference work remains 93,230; this optimization does not claim per-binding resolver deduplication. Removed files and non-parse entries explain the difference between 260 rebuilt surfaces and 276 changed manifest entries.

`persistent_surfaces_rebuild_only_changed_entries_without_reading_pruned_callers` reopens persisted bindings, counts actual immutable-blob reads, and checks cold parity. Disabling surface reuse with a restored `NON-VACUITY BREAK` makes that test alone fail (`rebuilt_surface_entries`: 2 rather than 1); the existing unrelated-export parity test remains green under the same mutation. The established graph/dependency row counts remain unchanged; its expected stats only gained the two new work counters. Debug/release materialization suites, callgraph-store suites, selected-join parity, and host plus Windows GNU library checks with `RUSTFLAGS='-D warnings'` pass.

### Same-input release offline measurements

| implementation | physical bytes | logical bytes | WAL bytes | wall seconds | CPU seconds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Baseline cold, sampled run | 849338368 | 1333890296 | 422963352 | 31.613 | 28.604 |
| Baseline incremental, sampled run | 378933248 | 479782414 | 143635592 | 21.626 | 18.913 |
| Baseline cold, phase-timed run | 849338368 | 1335094520 | 422963352 | 30.184 | 27.142 |
| Baseline incremental, phase-timed run | 378933248 | 475145742 | 143635592 | 16.646 | 14.466 |
| Persistent surfaces cold | 880517120 | 1363766296 | 436979592 | 23.916 | 21.375 |
| Persistent surfaces incremental | 383717376 | 484611958 | 145806832 | **8.766** | **6.951** |

All three runs pass every-table parity against their own cold materialization. Shared-machine and page-cache variation affects timings (including cold, whose work count is unchanged), so the work counters are the mechanism evidence. The optimized offline derived phase is 1.234 s below the 10 s target; the physical-write target remains unmet and slightly regresses from 361.38 to 365.94 MiB.

### WAL attribution, not a speculative index removal

The benchmark maps **every WAL frame** through the final database's `dbstat` page ownership, preserving repeated page writes. This is final-owner attribution: pages reused mid-transaction can have had another owner. Missing `dbstat` fails the benchmark rather than silently printing an empty map. The largest baseline incremental object is **view_bindings: 46,090,440 bytes**, rising to 48,261,680 with persisted surfaces; `refs` is 18,366,960. The five refs secondary indexes together are 30,731,080 bytes: caller-file 6,044,040; caller-node/kind 8,610,800; kind/caller-file 6,888,640; short-name 5,479,600; target-file 3,708,000. Ref primary-key index: 6,600,240. Edge secondary indexes total 11,568,960. No individual secondary index dominates: removing a shared graph index is not justified by this profile. Binding payload churn and collective ref/edge row/index churn remain the physical-write problem; a separate normalized binding/surface persistence layout is a better next write-amplification experiment than dropping one shared query index.

Raw logs: `target/real-release-before.log`, `target/real-release-before-profile.log`, `target/real-release-surface.log`, and `target/surface-mutation.log`. The measured databases remain under `target/view-diff-real-300-input/`.

### Final release drill after persistent surfaces

Measured implementation: `f143185c`; observed `2026-09-13T01:12:24Z`. Command: `scripts/views-branch-drill.sh --mode both --binary "$PWD/target/release/aft" --storage "$PWD/target/branch-drill-surface"`, after a fresh release binary build. Views warm-up: 956,554 ms; legacy warm-up: 5,418 ms. The designated checkout was restored clean to `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`. Reports were copied to `target/branch-drill-surface.{json,md}` and historical investigation files restored.

| switch | on publication_ms | on puts | on embeds | on cpu_s | on rss_delta_mb | on correct_ms | off publication_ms | off puts | off embeds | off cpu_s | off rss_delta_mb | off correct_ms |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| HEAD→a085bf62a459 | 53980 | 0 | 3 | 137.86 | 754.453 | 53980 | — | — | 3 | 65.72 | 2686.703 | 59809 |
| a085bf62a459→HEAD | 48569 | 0 | 0 | 69.9 | -130.719 | 48875 | — | — | 4 | 46.63 | -543.312 | 44762 |
| HEAD→refs/remotes/upstream/v2-timeouts | 43846 | 0 | 1 | 70.0 | -443.75 | 44216 | — | — | 3 | 48.14 | 2.391 | 44020 |
| refs/remotes/upstream/v2-timeouts→HEAD | 46048 | 0 | 0 | 72.51 | -416.438 | 46047 | — | — | 4 | 44.67 | 41.078 | 38002 |

| switch | derived ms | over 10,000 ms target | final publication event total ms | views correct_ms minus legacy correct_ms |
| --- | ---: | ---: | ---: | ---: |
| HEAD → A | 23539 | 13539 | 26703 | -5829 |
| A → HEAD | 20245 | 10245 | 23696 | +4113 |
| HEAD → B | 18042 | 8042 | 21061 | +196 |
| B → HEAD | 19521 | 9521 | 22779 | +8045 |

**Shipping criterion remains unmet.** Only HEAD → A beats legacy to correctness; views CPU is higher on every row. The offline 8.766 s result must not be substituted for the drill's 18–24 s derived phase. Legacy times also increased substantially versus the retained before table, so cross-run wall-time changes are not a controlled speedup claim. Within the final run, there is another ~23–27 s between switch initiation and the final publication event's own measured duration, including the earlier pending publication and semantic readiness work; that is outside this materialization-only change. Final published-event puts are zero, but earlier pending forward events put 269 / 13 blobs, and embeddings remain 3 / 1 on forward legs. The generated script's static zero-embedding narrative is not evidence.

The next measured offline CPU/wall buckets are resolver/surface recording (~2.11 s) and ref/edge emission (~2.08 s). Resolution still runs once per reference. A cache keyed only by `(dependent, import binding)` would be unsound: namespace member accesses differ by `full_ref`/`short_name`, value refs apply callable-target checks, and Rust resolution consumes additional raw-reference context. A narrower JS/TS target lookup memo could preserve those inputs and replay both surface queries and dependency probes, but has not been implemented or claimed here. Independently, the dependency-basis set is reconstructed per reference in the existing selected loop; hoisting that immutable per-caller set is a lower-risk next experiment. These follow-ups and normalized binding storage need their own work guards and same-input measurements. No unmeasured second optimization was included to make this run appear to pass.

## In-situ publication attribution and release drill (2026-09-13)

The publication profile now reports the materializer's existing phase timers on the root-attributed `index_event kind=view_publication` line. It also distinguishes the `apply_manifest_diff` call from generation clone and publication-closure durability. The old `materialize_ms` field incorrectly repeated the whole `derived_ms` phase; it now measures only the materialization call. `derived_ms` remains the cancellation/health phase boundary and therefore includes clone, materialization, trigram creation and closure durability.

A fresh-storage, pre-attribution-fix both-arm drill (`target/branch-drill-prefixed.{json,md}` and `.stderr.log`) had one pending and one published event per switch. Every pending event had `derived_ms=0`; only the published event materialized. Thus there are two publication attempts, but **not two materializations** for one switch. The pending attempt assembled HEAD and filled missing immutable blobs while semantic data was unavailable; the semantic-ready attempt adopted those blobs and performed the sole derived build.

The final release drill reused that isolated storage so its warm-up measured the same already-built caches rather than repeating the 15-minute cold semantic build. Command: `scripts/views-branch-drill.sh --mode both --binary "$PWD/target/release/aft" --storage "$PWD/target/branch-drill-prefixed"`. Reports are retained as `target/branch-drill-final.{json,md}` and `.stderr.log`; the historical investigation reports were restored. The designated opencode checkout was restored clean at `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`.

### Offline and daemon materialization phases

All daemon rows below are the one published event for that switch. Times are milliseconds. Nested join rows are still subdivisions of `selected join`, not additional time. Small differences between the displayed phase sum and materialization call are connection setup/close, fingerprinting and integer truncation.

| operation | load/select | delete rows | owned decode/insert | selected join | binding writes | ref/edge emission | commit | materialization call | closure durability | derived phase |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Offline persistent surfaces | 616 | 1,043 | 565 | 3,600 | 232 | 2,079 | 565 | **8,766** | not measured | not measured |
| Daemon HEAD → A | 1,054 | 820 | 496 | 3,463 | 224 | 1,820 | 442 | **8,482** | 2,502 | 10,986 |
| Daemon A → HEAD | 1,220 | 754 | 628 | 3,858 | 193 | 2,229 | 498 | **9,453** | 3,791 | 13,245 |
| Daemon HEAD → B | 1,245 | 805 | 518 | 3,654 | 221 | 2,218 | 552 | **9,296** | 3,377 | 12,673 |
| Daemon B → HEAD | 1,111 | 715 | 576 | 3,987 | 201 | 2,349 | 560 | **9,577** | 3,622 | 13,200 |

| operation | join decode/index | join surface replay | join deferred caller decode | join resolve/record | join dependency union |
| --- | ---: | ---: | ---: | ---: | ---: |
| Offline persistent surfaces | 895 | 52 | 443 | 2,111 | 74 |
| Daemon HEAD → A | 994 | 58 | 415 | 1,911 | 59 |
| Daemon A → HEAD | 1,125 | 55 | 687 | 1,913 | 56 |
| Daemon HEAD → B | 1,053 | 54 | 499 | 1,965 | 60 |
| Daemon B → HEAD | 1,098 | 51 | 721 | 2,034 | 61 |

The materialization itself is 8.482–9.577 s in situ, bracketing the 8.766 s offline result. No phase exhibits the former 2–3× inflation, and clonefile is 0 ms in every row. The apparent 18–24 s discrepancy came from treating the publication's broader derived phase as the materialization timer, plus run-to-run contention: the fresh-storage profiling run measured 12.819–14.539 s derived, while the final run measured 10.986–13.245 s. In the final run, 2.502–3.791 s is required publication-closure checkpoint/fsync and is absent from the offline benchmark. These numbers do not support clone warming, lower-priority deferral, or publication cancellation as a materialization optimization.

### Publication count and final acceptance

| switch | publication events | pending derived ms | published materializations | views CPU s | legacy CPU s | views correct ms | legacy correct ms | views minus legacy correct ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HEAD → A | 2 | 0 | **1** | 33.03 | 40.17 | 32,792 | 34,211 | -1,419 |
| A → HEAD | 2 | 0 | **1** | 58.26 | 32.62 | 38,008 | 27,466 | +10,542 |
| HEAD → B | 2 | 0 | **1** | 54.40 | 35.85 | 35,291 | 29,947 | +5,344 |
| B → HEAD | 2 | 0 | **1** | 58.63 | 32.58 | 38,225 | 27,160 | +11,065 |

Every correctness probe passed and the drill reported no defects. **The shipping criterion remains unmet:** views must beat legacy to correctness on all four rows with CPU not above legacy. Views wins correctness and CPU only on HEAD → A. It is 5.344–11.065 s slower on the other three rows and uses 18.55–26.05 more CPU-seconds there.

The next wall bucket is outside the final publication event: correctness minus that event's own total is 19.551 / 22.117 / 20.414 / 22.418 s. That interval includes watcher application, semantic readiness, the pending assembly and other concurrent maintenance. Within the sole materialization, selected join (3.463–3.987 s) and ref/edge emission (1.820–2.349 s) remain the largest buckets. Optimizing either requires a separate guarded change; the present measurements do not justify changing publication orchestration or durability semantics.

## Plane-ready publication investigation (2026-09-13)

The retained baseline log is `~/.cache/aft-views-soak/0f3900af641f5248/branch-drill-views-on.stderr.log`; a copy was regenerated while testing the release binary. For the baseline forward HEAD → A switch its timestamped timeline was:

| UTC | elapsed from watcher | event / attribution |
| --- | ---: | --- |
| 03:22:05 | 0 s | watcher applied the 300-path batch and invalidated the resident indexes |
| 03:22:05–03:22:08 | 0–3 s | concurrent tier-2 refreshes ran; no legacy callgraph refresh completion was logged |
| 03:22:05–03:22:20 | 0–15 s | semantic collection waited/scheduled and then collected 2,690 chunks from 260 files (`sched=6302ms`, collection 73 ms) |
| 03:22:20–03:22:23 | 15–18 s | the semantic embedder completed 3 batches for 260 files |
| 03:22:22–03:22:25 | 17–20 s | the first view attempt assembled HEAD, found 260 semantic keys pending, and ended `outcome=pending`, `derived_ms=0`, `total_ms=3053` |
| 03:22:25–03:22:52 | 20–47 s | semantic readiness retriggered the view; the final publication spent 26,556 ms total, including 20,494 ms materialization and 3,834 ms closure durability |
| 03:22:53 | 48 s | the first `callers(activeInfo)` probe succeeded |

This confirms Fact A: the manifest was withheld solely because semantic keys were absent, even though its callgraph blobs were available. Publication now treats semantic misses as plane-local pending state: it publishes the callgraph-bearing manifest immediately, retains those paths for the semantic fill, and the fill advances the manifest fingerprint without rewriting callgraph rows. The guarded acceptance test holds an embedding request at the fake server and resolves the checkout's new callgraph symbol through the view while that request remains held.

Fact B was **false in the same baseline log**. The views-on switch contained two `index_event kind=view_publication` lines (one pending and one published), zero `refreshed callgraph store ... for N watcher path(s)` lines, and its perf tick reported `callgraph_invalidations=0`. `CallGraphRead` selected `reader_kind=view` for the current pinned generation, but the hypothesized duplicate legacy incremental refresh did not occur. No legacy-refresh suppression is shipped from this investigation.

### Release drill after the plane split

The required command was attempted with both throwaway seeded storage and the known warm storage:

`scripts/views-branch-drill.sh --mode both --binary "$PWD/target/release/aft" --storage <isolated-storage>`

Both attempts exercised all four views-on transitions, restored the designated checkout to `5716f8ba60e79ec60ec485b6e5291c0b0bc1f252`, and then hit the command timeout before producing the combined JSON/Markdown table. Consequently process CPU deltas and a valid same-run legacy comparison are unavailable; the partial wall observations below are diagnostic only, not an acceptance claim. The warm store had inherited mismatched historical manifests, forcing full-resolution materialization (17.0–23.8 s) rather than the 8.5–9.6 s incremental path measured above.

| switch | views correct ms (log timestamps) | views CPU s | prior legacy correct ms | prior legacy CPU s |
| --- | ---: | ---: | ---: | ---: |
| HEAD → A | ~49,000 | unavailable | 34,211 | 40.17 |
| A → HEAD | ~53,000 | unavailable | 27,466 | 32.62 |
| HEAD → B | ~43,000 | unavailable | 29,947 | 35.85 |
| B → HEAD | ~42,000 | unavailable | 27,160 | 32.58 |

The owner's rule remains: **views must beat legacy to correctness on every row, with views CPU not above legacy**. This incomplete drill does not establish that rule. Its wall observations miss legacy by approximately 14.8 / 25.5 / 13.1 / 14.8 s, and CPU cannot be adjudicated. The next benchmark bucket is obtaining a clean, compatible warm manifest set (or allowing its one-time rebuild to finish outside the timed run), then rerunning the unchanged both-arm drill so the intended incremental callgraph-plane publication—not full-resolution recovery—is measured.

## Policy costs landed: own 1 s publication window, deferred checkpoint (2026-09-13, f0750e15)

Both-arm drill on fresh storage per arm, release binary at `f0750e15`, `scripts/views-branch-drill.sh --mode both --binary target/release/aft --storage /tmp/aft-views-drill-77/on --baseline-storage /tmp/aft-views-drill-77/off --output-dir /tmp/aft-views-drill-77/out` (detached, ~1 h; views-on warm-up 533 s, views-off 492 s, both cold semantic builds). No defects, every correctness probe passed, zero blob puts and zero embeds on both return legs. Raw record: `/tmp/aft-views-drill-77/out/branch-drill.{json,md}` and the two per-arm stderr logs.

| switch | views publication ms | views CPU s | views correct ms | legacy CPU s | legacy correct ms | views − legacy correct |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| HEAD → A | 27,876 | 95.3 | 32,436 | 80.4 | 34,254 | −1,818 |
| A → HEAD | 27,228 | 72.6 | 31,434 | 28.9 | 23,997 | +7,437 |
| HEAD → B | 25,222 | 54.6 | 29,252 | 31.0 | 25,932 | +3,320 |
| B → HEAD | 25,225 | 54.7 | 29,327 | 30.5 | 25,708 | +3,619 |

**Shipping criterion still unmet** (views beats legacy to correctness on all four rows with CPU not above legacy): views wins one row and loses three by 3.3–7.4 s; views CPU is above legacy on every row.

The published events' own attribution, callgraph-plane publication per switch (the semantic fill that follows each is 2.8–3.2 s total with a 26–31 ms materialization and a 2.5–3.0 s closure):

| switch | manifest | derived | materialization call | load/select | delete rows | selected join | ref/edge emission | commit | closure |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HEAD → A | 1,971 | 22,350 | 19,213 | 615 | 1,121 | 10,781 | 5,187 | 97 | 3,126 |
| A → HEAD | 2,163 | 22,136 | 19,577 | 753 | 917 | 10,249 | 5,850 | 121 | 2,548 |
| HEAD → B | 1,616 | 20,951 | 18,330 | 800 | 823 | 10,200 | 5,202 | 77 | 2,611 |
| B → HEAD | 1,612 | 20,953 | 18,475 | 877 | 1,335 | 9,834 | 5,089 | 85 | 2,469 |

Two findings this run adds, both unexplained and both the next work:

1. **The incremental materialization measures 18.3–19.6 s here against 8.5–9.6 s in the previous in-situ run and 8.8 s offline, on the same transition and the same binary lineage.** Selected join 10 s vs 3.5 s, ref/edge emission 5.2 s vs 2 s — a uniform ~2.5–3× on the two SQLite-heavy phases, and the cold seed materialization (18.5 s) costs the same as the incremental. The only product changes between the two runs are this delivery's (`journal_mode=WAL` + `synchronous=FULL` + `wal_autocheckpoint=0` on the materialization connection, the keeper connection held open through the build, the deferred checkpoint) and fresh storage per arm. Bisect on the same input before touching the join.
2. **The closure costs 2.5–3.1 s per publication with zero blob puts** (the semantic-fill publications, materialization 26 ms, closure 2.5–3.0 s), so it is a fixed cost: PASSIVE checkpoints and fsyncs of the two shared blob stores (the callgraph store is ~1.7 GB) plus alias/trigram fsyncs, paid twice per switch. It is no longer the derived checkpoint (that is deferred; `derived_clone_ms` is 4–11 ms). Proportional-to-change durability is the fix shape: a store with no puts since its last durable point needs neither checkpoint nor fsync.

Critical path after this run: ~1 s window + ~2 s assembly + 18–19 s materialization + ~2.6 s closure ≈ 25 s publication, correctness at ~29–32 s; legacy at 24–26 s. With the materialization back at ~9 s and the fixed closure removed, the views path is ~13 s against legacy's ~24 s on this switch.
