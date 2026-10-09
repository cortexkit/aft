# Inspect-tool slice

## Delivery and corpus gate

The inspect-tool implementation and its behavioral fixtures are verified. The **before/after corpus cell gate is blocked**, not passed. The parent confirmed that no preserved baseline or judgment-origin raw outputs exist and directed recording the blocker rather than reconstructing scores from earlier reports.

`judgments/inspect-tool.json` enumerates the planned precision/recall cells by repository, language and category. It contains no invented row judgments or after-output directory. AFT's own cells remain explicitly new, with no baseline.

| Corpus | Language | Before/after cells blocked |
|---|---|---|
| outline | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall |
| typeorm | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall |
| ripgrep | Rust | dead_code, todos, diagnostics: precision and recall |
| axum | Rust | dead_code, todos, diagnostics: precision and recall |
| soft-serve | Go | dead_code, dead_code (functions), todos, diagnostics: precision and recall |
| fastapi | Python | dead_code, todos, diagnostics: precision and recall |
| aft | Rust | dead_code, todos, diagnostics: precision and recall; new, no baseline |
| aft | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall; new, no baseline |

### Structural finding: scoring provenance is deleted between slices

The harness stores its baseline and after outputs below `target/inspect-truth-corpus/_results/`. Worker delivery cleanup deletes `target/`. The committed judgment files still refer to those directories:

- baseline: `13dd9cb71`;
- harness origins: `52f09206136a11e1cd18ebf8348d58200bae6e55`;
- scanner TypeScript/Go origins: `715f01b05062908be3f9cd147f0e4399c40274ec`;
- scanner Rust/AFT origins: `7306b6953ace4ad20e8b3108486e6b0d831d8af5`.

`load_judgments` validates each judgment against the baseline and the after output recorded by that judgment's own file, not against a new candidate collection. Consequently, a later candidate collection alone cannot validate the earlier judgments. The origin raw rows and oracle snapshots should be committed or preserved outside regenerable build directories. No harness change is included in this tool slice.

Neither a no-lowered-cell result nor new precision/recall numbers are claimed here. The all-repository diagnostics pass and corpus footer checks remain unrun. Historical Markdown scores are not a replacement for the deleted raw rows.

## Behavioral acceptance

### Scoped diagnostics and project status

`scoped_inspect_without_diagnostics_does_no_producer_work` uses a running fake Rust producer, one pre-opened file, cold Rust files and a cold TypeScript producer. It calls the blocking inspect entry both with `sections: ["dead_code"]` and with sections omitted. It checks no document reports/open-close notifications, no cold producer start or quiescence phase, exactly `{status: "not_requested"}`, no diagnostics detail/text, and unchanged project D/U/C/T counts.

The fixture was run against an archive of the exact reviewed red revision, `52f09206136a11e1cd18ebf8348d58200bae6e55`. Only the new test was appended to that archive. The result was **0 passed, 1 failed**, with `cold file was analyzed: .../src/b.rs`. The candidate passes. A separate candidate mutant performs the sweep but discards its response; the same test fails on the cold diagnostic report, not on a superficial payload difference.

`scoped_diagnostics_discovery_preserves_project_bar_counts` discovers a new error in a cold file while retaining the prior project E total and D/U/C/T. `scoped_typescript_inspect_preserves_missing_gopls_mask` proves a scoped TS walk cannot erase the project-wide missing-Go mask. Unscoped summary counts are compared with the bar in the 150-export fixture.

Existing startup, producer-failure and provisional-diagnostics tests now explicitly select diagnostics when scoped. Their original diagnostic assertions remain. The prior numeric-E assumption before an expected Rust producer had reported is intentionally changed to unknown project E/W.

### Offset and full stored lists

`inspect_offset_pages_all_150_unused_exports` uses a non-library file with 150 exports. It checks 100 and 50 rows, stable repeat order, disjoint union of 150 `(file, symbol, line)` rows, invariant total, exhausted offsets and a scoped second page read from the complete stored aggregate.

`offset_applies_independently_to_all_detail_lists` covers the main, generated, test-only and diagnostic list keys, including final envelopes, and rejects negative, fractional and string offsets. `topK` still caps at 100. Complete envelopes are structured data only: the final page has no cap reason and **no rendered trailer**, preserving the agent-visible complete-list contract.

The aggregate cache-version bump remains assigned to the campaign's upgrade slice; this slice does not change that version. Fresh-storage acceptance demonstrates the new full-list behavior, not migration of older capped aggregates.

### Outline corpus paging: passed

Pinned Outline `d410db1b24594d71b766212282804d79ca18e5c1` was fetched and installed inside this worktree. An authorized native **debug** candidate ran unscoped `unused_exports` requests with topK 100. Other scanners and diagnostic servers were disabled to isolate the paging acceptance.

| offset | shown | total | next_offset | cap reasons |
|---:|---:|---:|---:|---|
| 0 | 100 | 228 | 100 | cap |
| 100 | 100 | 228 | 200 | cap |
| 200 | 28 | 228 | null | none |
| 238 | 0 | 228 | null | none |

Pages were pairwise disjoint, their union contained **228 distinct rows**, every page reported the same total, and repeated requests returned the same order. This is a whole-project envelope total, not an in-domain accuracy score. No release timing was inferred from the debug run.

The paging run used the harness's disabled-server list: `typescript`, `typescript-native`, `python`, `rust`, `go`, `bash`, `yaml`, `ty`, `oxlint`, `biome`, `vue`, `svelte`, `astro`, `prisma`, `dockerfile`, `terraform`.

### Pending and unsupported languages

`inspect_cold_build_limiter_timeout_is_pending_then_ready` holds the sole test cold-build permit, requests a fresh blocking dead-code build, observes the pending payload and scanner-state category, preserves an old D count with the stale marker, releases the permit and obtains a count on the later call. It does not use the unrelated refresh-worker barrier. The payload-contract test now allows pending fields only for an actual pending dead-code outcome. Borrow-only and terminal-unavailable fixtures remain passing.

`inspect_python_gap_is_unknown_without_supported_files_and_counted_when_mixed` proves a Python-only scan has a null count and clears the old bar D value, while a mixed supported-language/Python scan has a numeric count and still names the unsupported-language gap. Non-code files do not supply artificial language support.

### Diagnostic producer authority and footer

Four applicability fixtures cover disabled/undefined producers, a TS SDK failure, a missing YAML binary and a TS-scoped walk in a project missing gopls. A separate count-cache fixture proves E/W remain unknown until the project-wide snapshot exists. Snapshot expectations are recorded on configure and unscoped walks, never overwritten by scoped walks.

The OpenCode parser retains producer/root gaps, and the real footer formatter renders `E? W?` for `producer_missing`, even when numeric values are supplied. Its three direct checks verify gap preservation, masking and unchanged project counts. A footer mutant removing the mask fails only `inspect_footer_producer_missing_mask`; the other two checks remain green.

## Mutation evidence

All Rust controls were independently activated in one temporary mutation batch, staged safely before mutation, and restored to an empty unstaged `git diff --stat`. Each selected test ran alone and was the sole failing test. The footer control likewise restored an empty diff.

| Neutralized behavior | Exact failing test |
|---|---|
| Skip diagnostics work, but run a sweep and discard its response | inspect_command_test::scoped_inspect_without_diagnostics_does_no_producer_work |
| Preserve project bar on scoped reads | inspect_command_test::scoped_inspect_without_diagnostics_does_no_producer_work |
| Preserve project producer snapshot on scoped walks | inspect_command_test::scoped_typescript_inspect_preserves_missing_gopls_mask |
| Store uncapped unused-export aggregates | inspect_command_test::inspect_offset_pages_all_150_unused_exports |
| Report unsupported programming-language coverage | inspect_command_test::inspect_python_gap_is_unknown_without_supported_files_and_counted_when_mixed |
| Apply offsets independently | commands::inspect::fresh_payload_tests::offset_applies_independently_to_all_detail_lists |
| Return pending during a held cold build | commands::inspect::deferred_terminal_tests::inspect_cold_build_limiter_timeout_is_pending_then_ready |
| Wait for applicability before numeric diagnostics | context::status_bar_tests::diagnostics_counts_are_unknown_until_project_applicability_exists |
| Mask active SDK failures | lsp::manager::project_applicability_tests::project_authority_masks_typescript_sdk_unavailable |
| Include absent binaries in missing-producer gaps | lsp::manager::project_applicability_tests::project_authority_masks_missing_yaml_binary |
| Keep supported-file parse/read failures unknown despite a language gap | commands::inspect::status_bar_refresh_tests::unsupported_language_gap_does_not_hide_failed_supported_files |
| Mask numeric footer values for missing producers | inspect_footer_producer_missing_mask |

## Gates and unrun measurements

Tool versions: Cargo `1.99.0 (5f94df478 2026-08-27)`, rustc `1.99.0 (b940084d7 2026-09-28)`, rustfmt `1.10.0-stable`, Bun `1.4.2`, TypeScript `5.9.3`, Biome `2.4.7`, Python `3.9.6`.

Passed checks:

- `cargo test -p agent-file-tools --test integration inspect_command_test`: **112 passed, 4 pre-existing ignores, 0 failed** on Linux; the subsequently added scoped-page assertion also passes its focused test.
- `cargo test -p agent-file-tools --test list_envelope`: **175 passed** on Linux.
- `cargo test -p agent-file-tools --lib commands::inspect`: **79 passed** on Linux after the final payload-contract and incomplete-coverage refinements.
- Authorized local four-job/four-thread fallback: `--lib inspect::` **332 passed, 4 ignored**; `--lib context::status_bar_tests` **10 passed**; `--lib project_applicability_tests` **4 passed**. The later refinements are covered by the 79-test Linux run.
- `cargo test -p agent-file-tools --test rest status_counts_inspect_seams`: **4 passed** on Linux.
- `RUSTFLAGS="-D warnings -A deprecated" cargo check -p agent-file-tools --tests --target x86_64-pc-windows-gnu`: **Finished**, exit 0. Compile-only, not Windows runtime verification.
- `bun run --cwd packages/opencode-plugin typecheck` and the corresponding pi-plugin command: exit 0.
- Full plugin unit scripts against the candidate debug binary: OpenCode **1640 passed, 3 skipped**; Pi **842 passed, 1 skipped**; no failures.
- `bun run lint`: **708 files checked**, no fixes at the source gate. `cargo fmt --all` and `cargo fmt --all -- --check`: exit 0.
- Scoped AFT diagnostics for the four changed TS files: authoritative **4/4**, **0 errors, 0 warnings**; unrelated Tier-2 view coverage was partial.
- Schema and catalog regeneration used the named generators. Both catalog files were transferred byte-for-byte as the exact Linux generator delta, not recalculated by hand. Governed documentation is aligned after the source commit.
- Comment review examined the 15 source/test files and flagged no remaining comments after the initial clarifications.

The mandatory `bash scripts/rust-test-gate.sh` was attempted on Linux. It checked 708 files, then its prerequisite `semantic_index::tests::platform_verifier_tls_client_subprocess` could not exercise certificate trust: `https://localhost:<port>/v1/embeddings` failed DNS lookup with **Temporary failure in name resolution**. The remaining full-gate phases are **not run**, with this runner environment error; no gate pass is claimed and the TLS assertion was not changed.

The runner also repeatedly refused jobs with `runner_draining`. The parent authorized bounded local debug/correctness work but explicitly prohibited a Mac release build and Mac timing. **TypeORM release timing is not run**: the machine and both medians are unmeasured, not substituted with debug or loaded-Mac numbers. The real cold release TypeORM run and the TypeORM/soft-serve/Outline corpus footer measurements are also unrun. Their fixture counterparts pass, but that is not a corpus measurement.

No search-ranking or routing fence file is changed. No root package manifest or lockfile is changed; dependency installation was confined to the fetched corpus under this worktree.
