# dead-code-scanner: inspect-truth cell gate

Baseline: 0.58.2 (`13dd9cb71`). Both tables use this revision of `run.py score`.

## Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | unknown | n/a | 9 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 31 | 21 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 6 | 49 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 2 | 99 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 2 | 96 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/outline/13dd9cb71` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/typeorm/13dd9cb71` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}
- typeorm/typescript/dead_code unknown: {"unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "producer": "dead_code analysis (Tier-2)", "reason": "inspect_phase_timeout: tier2 dead_code aggregate did not complete within its phase wait budget; builder_state=last attempt failed: tier2 dead_code aggregate did not complete; builder_state=buildi (attempt 1, first at 1791273537); retry aft_inspect"}]}

- ripgrep raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/ripgrep/13dd9cb71` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

- axum raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/axum/13dd9cb71` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 4}

- soft-serve raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/soft-serve/13dd9cb71` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

## After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | scored | 75 | 9 | 0 (+4) | 0.05 | 0.00 | 0 | 0 | 75 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 21 | 0 | 0 (+0) | 0.00 | n/a | 0 | 21 | 0 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 10 | 0 | 0 (+0) | 0.00 | n/a | 0 | 10 | 0 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 121 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 121 |
| soft-serve | go | dead_code (functions) | scored | 118 | 20 | 9 (+0) | 0.08 | 0.45 | 0 | 0 | 118 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| aft | rust | dead_code | unknown | n/a | 0 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | rust | todos | scored | 0 | 6 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | scored | 110 | 161 | 110 (+0) | 1.00 | 0.68 | 0 | 0 | 110 |
| aft | typescript | dead_code | unknown | n/a | 37 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/outline/715f01b05062908be3f9cd147f0e4399c40274ec` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/typeorm/715f01b05062908be3f9cd147f0e4399c40274ec` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

- ripgrep raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/ripgrep/7306b6953ace4ad20e8b3108486e6b0d831d8af5` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 10}

- axum raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/axum/7306b6953ace4ad20e8b3108486e6b0d831d8af5` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}

- soft-serve raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/soft-serve/715f01b05062908be3f9cd147f0e4399c40274ec` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 2}

- aft raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_842058ef8adb86cc630e77ba/target/inspect-truth-corpus/_results/aft/7306b6953ace4ad20e8b3108486e6b0d831d8af5` (corpus `7306b6953ace4ad20e8b3108486e6b0d831d8af5`).
- aft baseline_aft_truncated_scopes (excluded from comparison): []
- aft judgments_unapplied: {"harness.json": 0, "dead-code-scanner.json": 0}
- aft/rust/dead_code unknown: {"unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "producer": "dead_code analysis (Tier-2)", "reason": "tier2 reuse worker exited without publishing a result; retry aft_inspect"}]}
- aft/typescript/dead_code unknown: {"unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "producer": "dead_code analysis (Tier-2)", "reason": "tier2 reuse worker exited without publishing a result; retry aft_inspect"}]}
- aft git status --porcelain before:
```text
?? benchmarks/inspect-truth/judgments/dead-code-scanner.json
?? benchmarks/inspect-truth/slices/dead-code-scanner.md
```
- aft git status --porcelain after:
```text
?? benchmarks/inspect-truth/judgments/dead-code-scanner.json
?? benchmarks/inspect-truth/slices/dead-code-scanner.md
```

## Cell rule

- aft: new, no baseline
- PASS: no cell lowered.

## Collection, provenance, and verification

- All clones, raw outputs, oracle tools, build archives, and logs are inside this worktree under `target/`. The baseline binary was built from an archive of `13dd9cb71`, not substituted with a same-version binary. Release builds used `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=`; all binaries report `aft 0.58.2`. Cargo is `1.99.0`, Python is `3.9.6`.
- Outline, TypeORM, and soft-serve were collected with the completed scanner/Go dispatch implementation at `715f01b05062908be3f9cd147f0e4399c40274ec`. The subsequent change at `7306b6953ace4ad20e8b3108486e6b0d831d8af5` only refines Rust manifest-named library module roots, so ripgrep, axum, and the local mixed-language AFT checkout were re-collected at that revision. Their raw directories are named individually above and in `judgments/dead-code-scanner.json`; older attempts remain preserved. No baseline or successful oracle snapshot was overwritten.
- Collection command: `python3 benchmarks/inspect-truth/run.py collect --corpus-root target/inspect-truth-corpus --aft-bin <release-binary> --baseline-bin target/inspect-truth-baseline-aft --aft-commit <revision> --diagnostics-sample 0 --repo <name>`. The final Rust pass used `target/inspect-truth-root-after-aft`; the TS/Go pass used `target/inspect-truth-after-aft`.
- Both sides were scored with this slice's unchanged harness revision. `run.py score --aft-commit 715f01b05062908be3f9cd147f0e4399c40274ec --output-dir target/dead-code-score --slice dead-code-scanner` scored all six selected repos. `run.py score --aft-commit 7306b6953ace4ad20e8b3108486e6b0d831d8af5 --output-dir target/dead-code-score-roots` re-scored ripgrep, axum, and AFT. Both commands used `--corpus-root target/inspect-truth-corpus` and explicit `--repo` arguments. `target/merge-dead-code-scores.py` replaces only those three repo records, verifies their raw directories against the judgments file, and calls the same harness `render_slice`/cell rule: **5 before repos, 6 after repos, 43 judgments, 0 lowered cells**. Its merged records remain at `target/dead-code-score/final-results.json`.
- The oracle snapshots use fallow `2.88.3`, knip `5.85.0`, TypeScript `5.9.3` where applicable, rustc/Cargo `1.99.0`, staticcheck `2026.2.1 (0.8.1)`, and x/tools deadcode `v0.51.0` on Go `1.26.4`. Node oracle tooling was installed only under `target/inspect-truth-corpus/.tools/node` (Bun `1.4.2`, 36 packages); Go executables are under `.tools/go-bin`. No tracked package manifest or lockfile changed.
- Diagnostics sampling was disabled. The harness collection disabled-server list is `typescript`, `typescript-native`, `python`, `rust`, `go`, `bash`, `yaml`, `ty`, `oxlint`, `biome`, `vue`, `svelte`, `astro`, `prisma`, `dockerfile`, and `terraform`. This is not an E/W masking or diagnostics-timing measurement.
- An initial collection stopped because the worktree-local knip executable had not yet been installed. A subsequent combined collection exceeded its 30-minute command cap during TypeORM's after-side list collection. Only the affected project was retried with a longer budget, preserving completed baseline outputs. Final language collections completed successfully.

## Findings and remaining campaign work

- Rust findings fell from **52 to 21** on ripgrep and **55 to 10** on axum. Every remaining Rust AFT row is judged in this slice's JSON (`aft_rows_unjudged: 0` on each after side). Ten removed ripgrep public-API rows are additionally judged on the before side. These are AFT-side judgments: they explain rows without dropping them or artificially raising precision against the zero-row rustc oracle.
- Ripgrep's count still exceeds the campaign's final target of 10. The 21 remaining judgments name alias-root chains, invoked macro templates, generic trait/sink caller identity, and concrete production call sites. The callgraph slice still owns the alias and real macro-body extraction fixes; this scanner traverses those edges without promoting unused macro definitions to roots. The scanner's macro fixture verifies the consumed `m!` node/edge contract, not compiler macro expansion.
- Axum's residual rows include generic/value/trait and item-macro reachability, as well as definitions located inside same-file `cfg(test)` ranges. Public visibility is not a file-wide exemption for any of them. The judgments document the remaining false positives rather than deleting them from the result.
- Go function precision rises from **0.051 to 0.076**, and recall from **0.25 to 0.45**. The absolute Go count rises from 101 to 121; this slice does not claim the final Go precision target of 0.6. The module-declared `WebhookStore` contract and used receiver make `DeleteWebhookByID` and `GetWebhookEventByID` live, and both removed baseline rows are judged. Existing explicit receiver-call dispatch evidence remains intact; an intermediate interface-only implementation was rejected after corpus verification exposed a large regression.
- Outline's TS cells remain unchanged. TypeORM's baseline dead-code response was genuinely `analysis_incomplete`; its before cells remain **unknown**, not zero. The after in-domain count is 75, unchanged from the earlier scanner implementation. The baseline's truncated `typings.ts` unused-export scope remains explicitly excluded on both sides.
- AFT's own dead-code aggregate remains unknown because its Tier-2 reuse worker exits without publishing a result. Both Rust and TS cells report that gap above and have no baseline. The local checkout's before/after git status is identical, including only the two intentional, not-yet-committed report files at collection time. This is not an accuracy claim for AFT's unavailable dead-code bucket.
- **Oxc handoff:** in `crates/aft/src/inspect/oxc_engine/graph.rs`, `reference_origin_for_path` must use the shared `dead_code::is_test_code_file` predicate and retain project-relative paths, not basenames. This unblocks the acceptance check that every test-only `used_by` entry is a project-relative test-code `file` or `file:line` (acceptance sketch “used_by”, constraints d). The parent assigned that graph edit to the Oxc slice; this slice exposes the predicate and fixes scanner-derived origins and transitive root labels only.
- `DEAD_CODE_FACTS_FORMAT_VERSION` is now 6. The campaign's upgrade slice still owns the once-per-campaign `TIER2_CONTRIBUTION_CACHE_VERSION` bump needed to invalidate already-stored aggregates. All harness runs here use fresh storage.

## Test evidence and scope decisions

- Focused tests: **52** scanner unit tests and **31** dead-code integration tests pass. The list-envelope registry target passes **175** tests. Rustfmt `1.10.0-stable` passes `cargo fmt --all` and `cargo fmt --all -- --check`; the exact Windows `-D warnings -A deprecated` compile gate finishes successfully. Biome `2.4.7` checks **695** files with `bun run lint`, no fixes. No search-ranking/routing fence file is touched.
- The final mandatory `bash scripts/rust-test-gate.sh` passes **4,952** library tests (57 ignored) and **121** binary tests, then nextest runs **415** integration/other tests: 410 pass and five fail with the known worktree `read_only_store_not_built` error. The failing names are `callgraph_callers_cross_file`, `callgraph_aliased_import_resolution`, `callgraph_callers_recursive`, `callgraph_callers_empty_result`, and `callgraph_cross_file_tree`. The dependency delivery already documented this baseline failure family. Watcher/release-storm phases were not reached. Full output is `target/dead-code-final-full-gate.log`.
- Before implementation, the new scanner fixtures failed on public-module/type paths, per-item inline visibility, Go interface receiver liveness, cfg(test) call locations, transitive test roots, full lists, and `go.mod` semantics. Targeted mutation runs then reddened each named property test; each mutant was staged safely, clearly marked, and restored with an empty unstaged `git diff --stat`. Detailed per-control evidence is in the delivery declaration; logs remain under `target/mutation-*.log`.
- The parent explicitly authorized `crates/aft/tests/integration/inspect_dead_code_test.rs` beyond the original three-file fence. Four assertions intentionally move with the specified contract: exact version-6 contribution snapshots (including `rust_api` facts), complete lists past 100 rather than scanner truncation, and cfg(test) `used_by` with the call line. Their equality assertions were retained, not relaxed. The renamed full-list fixture checks every symbol in order.
- `inspect_dead_code_keeps_public_rust_pub_use_leaf_live` remains unchanged: with a public `Foo` binding re-exported from a private module, both the binding and the leaf must be live while `Dead` remains the one finding. Removing Rust file-level suppression initially exposed the projection's synthetic binding as a false positive. The scanner now classifies that binding through its actual public re-export path; the unchanged test passes, and disabling that handling reddens precisely that fixture.
