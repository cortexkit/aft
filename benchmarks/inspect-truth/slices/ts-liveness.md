# ts-liveness: inspect-truth cell gate

Baseline: the AFT 0.58.2 binary built from `13dd9cb71`, compared with the final TS-liveness binary built from `9f382ddf1`. Both tables use the same `benchmarks/inspect-truth/run.py score` implementation.

**Reading the tables:** `aft` names this repository's own checkout; it is a new corpus entry with no historical baseline, so only its after results are shown. `baseline_aft_truncated_scopes` identifies baseline file scopes whose finding lists exceeded the old 100-row response limit; those incomplete scopes are omitted from both comparison sides. The recorded before/after `git status --porcelain` output proves that collection left the local AFT checkout unchanged. An `unknown` dead-code bucket means the background code-health analysis did not publish a complete result, not that no dead code exists; a queued cold build is waiting for an initial-build slot.

## Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 274 | 221 (+0) | 0.99 | 0.81 | 15 | 8 | 215 |
| outline | typescript | dead_code | scored | 386 | 89 | 66 (+0) | 0.17 | 0.74 | 15 | 22 | 364 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 4 | 3 (+4) | 0.33 | 0.75 | 57 | 21 | 0 |
| typeorm | typescript | dead_code | scored | 75 | 1 | 0 (+4) | 0.05 | 0.00 | 8 | 75 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_3d31a16af35b713d5e481bcb/.inspect-truth-ts-liveness/corpus/_results/outline/13dd9cb71` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"harness.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_3d31a16af35b713d5e481bcb/.inspect-truth-ts-liveness/corpus/_results/typeorm/13dd9cb71` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"harness.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0}

## After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 240 | 274 | 238 (+0) | 0.99 | 0.87 | 15 | 8 | 232 |
| outline | typescript | dead_code | scored | 73 | 89 | 66 (+0) | 0.90 | 0.74 | 15 | 7 | 66 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 3 | 4 | 3 (+0) | 1.00 | 0.75 | 57 | 3 | 0 |
| typeorm | typescript | dead_code | scored | 0 | 1 | 0 (+0) | n/a | 0.00 | 8 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| aft | rust | dead_code | unknown | n/a | 0 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | rust | todos | scored | 0 | 7 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | scored | 113 | 166 | 113 (+0) | 1.00 | 0.68 | 0 | 0 | 113 |
| aft | typescript | dead_code | unknown | n/a | 38 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_3d31a16af35b713d5e481bcb/.inspect-truth-ts-liveness/corpus/_results/outline/9f382ddf1` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"harness.json": 0, "ts-liveness.json": 15, "dead-code-scanner.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_3d31a16af35b713d5e481bcb/.inspect-truth-ts-liveness/corpus/_results/typeorm/9f382ddf1` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"harness.json": 0, "ts-liveness.json": 193, "dead-code-scanner.json": 0}

- aft raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/2be5fae243ecd1a69e8a0e26ef5da711b9e2a4b8e31fc3ea5735634ccc2a602f/bg_dispatch_3d31a16af35b713d5e481bcb/.inspect-truth-ts-liveness/corpus/_results/aft/9f382ddf1` (corpus `9f382ddf14a4a9c8f1a8a094caf01c9ad78884f6`).
- aft baseline_aft_truncated_scopes (excluded from comparison): []
- aft judgments_unapplied: {"harness.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0}
- aft/rust/dead_code unknown: {"unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "last attempt failed: tier2 reuse worker exited without publishing a result (attempt 8, first at 1791569040); progress: checking cache for 3506 source files; estimate unavailable (no completed build in this session)"}], "building": {"state": "queued_behind_cold_builds", "started_at": 1791569052, "elapsed_ms": 0, "progress": "checking cache for 3506 source files", "estimated_remaining_ms": null, "estimate_basis": "last completed build in this session; unavailable on cold start"}}
- aft/typescript/dead_code unknown: {"unavailable": true, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "last attempt failed: tier2 reuse worker exited without publishing a result (attempt 8, first at 1791569040); progress: checking cache for 3506 source files; estimate unavailable (no completed build in this session)"}], "building": {"state": "queued_behind_cold_builds", "started_at": 1791569052, "elapsed_ms": 0, "progress": "checking cache for 3506 source files", "estimated_remaining_ms": null, "estimate_basis": "last completed build in this session; unavailable on cold start"}}
- aft git status --porcelain before:
```text
A  benchmarks/inspect-truth/judgments/ts-liveness.json
A  benchmarks/inspect-truth/slices/ts-liveness.md
?? .aftignore
```
- aft git status --porcelain after:
```text
A  benchmarks/inspect-truth/judgments/ts-liveness.json
A  benchmarks/inspect-truth/slices/ts-liveness.md
?? .aftignore
```

## Cell rule

- aft: this repository's checkout is a new corpus entry with no before-side baseline; its cells are not compared with historical values.
- PASS: no cell lowered.

## Interpretation and provenance

- The final binary is the local release build of `9f382ddf14a4a9c8f1a8a094caf01c9ad78884f6` (`aft 0.58.2`, Cargo 1.99.0). Raw captures, oracle snapshots, score JSON, mutation logs and base reproductions are retained under `.inspect-truth-ts-liveness/` **outside `target/`** in this worktree. The relative paths in `judgments/ts-liveness.json` identify the final captures; they are not placeholders for a later run.
- Earlier slices kept their raw snapshots under `target/`; delivery cleanup deleted them. Fresh baseline/oracle collection at the exact Outline and TypeORM pins was necessary to replace those deleted snapshots without changing the compared source revisions. Baseline binary: archive of `13dd9cb71a7bbf9076377ac1ba1717dabf15e586`, locally built with `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=`. The local-checkout AFT corpus uses this slice's code checkpoint, not another checkout. Tools: Fallow 2.88.3, Knip 5.85.0, TypeScript 5.9.3, Python 3.9.6. No tracked package manifest or lockfile changed.
- TypeORM's `./*` package export exposes the source root (`packages/typeorm/package.json:19-45,213-215`; its main tsconfig has no rootDir, so `src/` applies). All before-side TypeORM public-source AFT rows are judged; all Fallow public-source rows get category-specific oracle-side `public_api` judgments. Dead-code precision can therefore become undefined with zero findings without losing an unexplained before row. The three remaining codemod unused exports are same-file-only constants, not dead functions; all three are judged. The formerly AFT-only TypeORM rows are resolved by their manifest public path, not by a special exemption for `export declare` in `.ts` files.
- Outline's 15 incorrect oracle dead-code rows are six actually-rendered `Components` namespace members, four converters selected from awaited literal imports, and five service defaults dispatched through dynamic-import loaders. Their concrete use sites are recorded per row, for **both** categories. The seven remaining AFT-only dead-code rows and two AFT-only unused exports were reviewed and judged. The 66 other after dead-code rows agree with Fallow; they are not claimed as individually judged. Outline reaches 0.904 dead-code precision, above 0.7; its judged recall stays 0.742. TypeORM unused-export precision is 1.0, above 0.8; dead-code count is zero with all 75 compared before-side AFT rows judged.
- Outline's Fallow entry-point output (`entry_point_count: 330`, `fallow-entry-exports` exit 1) is retained as a **diagnostic**, not the gate deciding which liveness fix applies. The source use sites, regression fixtures and cell rule are the evidence for this slice.

### Measurement-only exclusions and incompleteness

For this comparison, identical temporary `.aftignore` exclusions were applied to exactly five out-of-domain, syntax-invalid fixtures. The four codemod fixture paths and `benchmarks/src/exercise.ts` are enumerated in the judgments provenance. They were applied on both comparison sides; no in-domain source was removed or made parser-tolerant. Original default captures remain as `aft.original-default.json`. AFT's local checkout had the two staged report files plus temporary `?? .aftignore` on both recorded hygiene sides, exactly as shown above. Neither report changed during collection. The temporary ignore file was removed afterward; final delivery commits the reports and leaves the checkout clean.

This is deliberately separate from the benchmark's `include_paths`: `run.py:203-207` applies those prefixes only to scoring. The scanner still analyzes out-of-domain files, including test-support files, because they can import product exports (`inspect/job.rs:762-773`). Oxc parse errors produce empty facts (`oxc_engine/facts.rs:74-84`); the engine preserves the error while analyzing the available facts (`oxc_engine/mod.rs:280-289`). `unused_exports.rs:493-517` marks the aggregate incomplete when any parse error exists, and the summary carries `analysis_incomplete`; `run.py:448-453` refuses to score that gap. An unparsed file can contain imports whose references are unavailable. Global incompleteness is therefore an intentional, conservative honesty boundary, **not a TS liveness defect fixed here**. Restricting incompleteness to a sound reference-closed analysis domain would be separate work; simply ignoring errors outside scorer prefixes would be unsound.

### Cold-builder outcomes, including base reproduction

These are not reclassified as empty findings. The first default after captures and every warm attempt are retained. Three additional inspect requests without a scope filter let the already-started project-wide analysis finish before scoped list collection began; final tables score those complete results.

- **Outline:** first after request reported `analysis_incomplete`: `building since ... (age_s=57); progress: waiting for or projecting the checkout call graph; estimate unavailable (no completed build in this session)`. A fresh exact-base binary (`6fe1a5aaca736634553cb61014405eddfbd51f45`) did **not** reproduce this transient in its first request: it returned count 387. Final after runs returned count 74 after warming. The initial transient is preserved, not asserted to be a base-reproduced bug.
- **TypeORM:** first after request reported `building since ... (age_s=57); progress: assembling and storing analysis results; estimate unavailable (no completed build in this session)`. The base binary reproduced that shape, then `tier2 dead_code aggregate did not complete; builder_state=building since ... (age_s=62)`, and later returned count 215. The original 0.58.2 baseline also hit `inspect_phase_timeout: tier2 dead_code aggregate did not complete within its phase wait budget`. Final warmed after count is 140 project-wide, zero in the scored public-library domain.
- **Local AFT:** after requests repeatedly reported `tier2 reuse worker exited without publishing a result`. The exact-base binary reproduced it across four requests, including the bare error with no count. This remains a real unresolved producer failure, named in both language buckets above; no accuracy claim is made for those cells. `inspect/manager.rs:669-686` supplies this RAII flight-exit error. The local AFT end-of-campaign target requires a Rust dead-code count no more than rustc's count plus ten, row judgments for every AFT dead-code and unused-export finding, and an explicit unsupported-Python coverage gap. The background aggregation task must first stop exiting without a result; the 113 local unused-export rows also still need individual judgments. This slice does not claim that target is met.

The exact request/response JSON is under `.inspect-truth-ts-liveness/base-repros/{outline,typeorm,aft}/`; readable summaries are in `logs/base-cold-repros.log`. The progress-producing locations remain unchanged: `manager.rs:3381-3403` (checkout projection) and `:3461-3474` (result assembly/storage).

### Output and upgrade compatibility

`used_by` is agent-visible in inspect detail rows. Oxc now emits **project-relative paths**, replacing basenames, and classifies test-support paths with the scanner's `is_test_code_file` predicate. The schema change is confined to `types.rs`; the two existing dead-code test expectations were updated to require the new relative-path output contract. Typed `ExportUsage` facts are stored on namespace imports and dynamic imports, shared by dead_code and unused_exports. `FACTS_FORMAT_VERSION` advances 4 → 5, invalidating prior fact-cache entries once on upgrade. The campaign upgrade slice still owns stored aggregate invalidation via `TIER2_CONTRIBUTION_CACHE_VERSION`; every measurement here used fresh storage.

### Verification and mutation coverage

- Focused restored gates: 20 entry-point unit tests, 53 dead-code scanner unit tests, 50 Oxc integration tests (one manual benchmark ignored), and 11 unused-export integration tests passed. Cargo 1.99.0; rustfmt 1.10.0-stable; `cargo fmt --all -- --check` exit 0.
- Before implementation, the six new manifest tests and ten new namespace/dynamic-import tests failed at the base. JSX precision and a published executable using a non-src rootDir were additionally shown failing before their extraction/resolution fixes were added. Published public and binary targets now use one source-root resolution path; binary files remain liveness roots, not public API.
- Seventeen isolated mutation controls cover source-root mapping for public and executable targets, wildcard coverage, prefix-less remapping, go.mod collection, namespace type/member and bare-value use, dynamic statement/discarded-await/lazy/static-member/Promise use, dynamic target enqueueing, test-tree classification, project-relative paths, star/default forwarding, and JSX member extraction. Each invocation failed **only** its named test (others filtered), with non-empty applied `git diff --stat` and empty unstaged diff after restoring from the index. Logs live in `.inspect-truth-ts-liveness/logs/`; the delivery's structured mutation evidence names each exact test. The restored suites above passed afterward.
- The mandatory `bash scripts/rust-test-gate.sh` ran on Linux. Biome checked 708 files; TLS fixture passed 1; tokenizer tests passed 3; main library ran 5606 tests (5551 passed, 5 failed, 50 ignored, one filtered). The intended basename-to-relative-path assertion mismatch was corrected, and the impacted scanner suite passed. Binary/nextest integration/watcher/release-storm phases were not reached after the library failure.
- All four other library failures were individually reproduced against exact base source on **Linux** (one test per command, each exit 101): `blackholed_backend_never_blocks_status_or_search` (network unreachable, os error 101, unexpected error wording); `producer_edit_last_vectors_pin_consumer_wire_request_and_refusals` (`wire request must carry a repository string`); `storage_debug_archive_ignores_public_member_modes` (ENOENT); `deferred_navigation_producer_notifies_completion` (`release producer: SendError`). They are base-reproduced failures, not inferred unrelatedness. Details are transcribed in `logs/base-linux-failures-and-jsx.txt`; no out-of-scope product fixes were included.
- The remote runner temporarily refused focused gates with `runner_draining`, meaning the Linux build service was winding down and did not execute those commands; focused verification and the initial mutation controls used the existing local build instead. Later gates, the full gate, four base reproductions and final focused suites ran on Linux. Release builds remained local because the corpus harness runs those binaries locally. Cargo invocations were serialized. Two daemon restarts interrupted tooling; staged implementation was restored and confirmed exact before work resumed. No mutant or base-restored source remains.
- `aft_inspect` was partial while rust-analyzer indexed/checked and while the checkout graph was unavailable; direct compilation and focused tests are the authoritative source gates. No TypeScript source, package manifest, tool argument, list-cutting site or search-ranking/routing file changed, so their specialized gates were not applicable.
